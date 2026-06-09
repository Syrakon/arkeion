//! Integración M10 — corrección de errores por página (Slice C2): con ECC
//! activo, el bit-rot dentro del presupuesto se **corrige** al leer (verify
//! sigue verde, datos íntegros); fuera del presupuesto falla limpio
//! (`ChainBroken`, nunca dato silenciosamente malo). Sin ECC, esa misma
//! corrupción rompe la cadena: ECC es *más estable*, no menos.

use arkeion::format::{FIRST_DATA_PAGE, LEN_PREFIX_LEN};
use arkeion::{Connection, Database, Error, FromValue, Options, params};

const NSYM: u8 = 16; // corrige 8 bytes corruptos por bloque de 255

fn build(path: &std::path::Path, opts: Options, rows: i64) {
    let db = Database::open(path, opts).unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, nota TEXT NOT NULL)",
        &[],
    )
    .unwrap();
    conn.execute("BEGIN", &[]).unwrap();
    for i in 0..rows {
        conn.execute(
            "INSERT INTO t (nota) VALUES (?1)",
            &params![format!("registro numero {i} con texto de relleno")],
        )
        .unwrap();
    }
    conn.execute("COMMIT", &[]).unwrap();
    assert!(db.verify().unwrap().chain_ok);
}

fn get1<T: FromValue>(conn: &Connection, sql: &str) -> T {
    conn.query(sql, &[])
        .unwrap()
        .next()
        .expect("una fila")
        .unwrap()
        .get(0)
        .unwrap()
}

/// Offset del payload sellado del primer registro de datos (su marco empieza al
/// inicio de la zona append; tras el prefijo de longitud va el payload).
fn first_payload() -> usize {
    FIRST_DATA_PAGE.byte_offset() as usize + LEN_PREFIX_LEN
}

#[test]
fn ecc_corrects_on_disk_bit_rot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ecc.arkeion");
    build(&path, Options::default().ecc(NSYM), 400);

    // Corrompe 6 bytes (< 8 del presupuesto) en el primer bloque del payload del
    // primer registro de datos.
    let mut bytes = std::fs::read(&path).unwrap();
    let base = first_payload();
    for i in 0..6 {
        bytes[base + i * 3] ^= 0x5A;
    }
    std::fs::write(&path, &bytes).unwrap();

    // ECC corrige al leer: verify recorre y revalida cada página ⇒ verde.
    let db = Database::open(&path, Options::default().create_if_missing(false)).unwrap();
    assert!(
        db.verify().unwrap().chain_ok,
        "ECC debió corregir el bit-rot dentro del presupuesto"
    );
    let conn = db.connect().unwrap();
    assert_eq!(get1::<i64>(&conn, "SELECT COUNT(*) FROM t"), 400);
}

#[test]
fn beyond_budget_fails_clean() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ecc.arkeion");
    build(&path, Options::default().ecc(NSYM), 400);

    // Corrompe 12 bytes (> 8) en un solo bloque: excede lo corregible.
    let mut bytes = std::fs::read(&path).unwrap();
    let base = first_payload();
    for i in 0..12 {
        bytes[base + i] ^= 0xA5;
    }
    std::fs::write(&path, &bytes).unwrap();

    let result = Database::open(&path, Options::default().create_if_missing(false))
        .and_then(|db| db.verify());
    assert!(
        matches!(result, Err(Error::ChainBroken { .. })),
        "fuera del presupuesto debe fallar limpio: {result:?}"
    );
}

#[test]
fn ecc_is_more_stable_than_plain() {
    // La MISMA corrupción (6 bytes) rompe una DB sin ECC y la sobrevive una con
    // ECC: demuestra que comprimir/proteger no la hace menos estable.
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain.arkeion");
    let prot = dir.path().join("prot.arkeion");
    build(&plain, Options::default(), 400);
    build(&prot, Options::default().ecc(NSYM), 400);

    let corrupt = |path: &std::path::Path| {
        let mut bytes = std::fs::read(path).unwrap();
        let base = first_payload();
        for i in 0..6 {
            bytes[base + i * 3] ^= 0x33;
        }
        std::fs::write(path, &bytes).unwrap();
    };
    corrupt(&plain);
    corrupt(&prot);

    // Sin ECC: ChainBroken. Con ECC: verde.
    let plain_res = Database::open(&plain, Options::default().create_if_missing(false))
        .and_then(|db| db.verify());
    assert!(
        matches!(plain_res, Err(Error::ChainBroken { .. })),
        "sin ECC la corrupción debe romper: {plain_res:?}"
    );
    let prot_db = Database::open(&prot, Options::default().create_if_missing(false)).unwrap();
    assert!(
        prot_db.verify().unwrap().chain_ok,
        "con ECC debe sobrevivir"
    );
}
