//! Integración M10 — compresión de página (Slice B): un dataset comprimible
//! ocupa menos en disco con la compresión activa, **manteniendo** `verify()`, el
//! round-trip y el tiempo-viaje; cifrado+comprimido funciona igual; y una página
//! comprimida manipulada se detecta (jamás dato silenciosamente malo — la
//! estabilidad NO-NEGOCIABLE #1: el tag cubre los bytes finales, se valida antes
//! de descomprimir).

use arkeion::format::{FIRST_DATA_PAGE, LEN_PREFIX_LEN};
use arkeion::{Connection, Database, Error, FromValue, Key, Options, params};

/// Texto de baja entropía y repetitivo ⇒ muy comprimible. Idéntico en cada fila.
fn nota() -> String {
    "estado: pendiente de revisión por el equipo; prioridad normal; ".repeat(4)
}

fn build(path: &std::path::Path, opts: Options, rows: i64) {
    let db = Database::open(path, opts).unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, nota TEXT NOT NULL)",
        &[],
    )
    .unwrap();
    // Una sola transacción (muchas hojas, un commit): rápido y las hojas siguen
    // llenas de texto repetido → muy comprimible.
    let nota = nota();
    conn.execute("BEGIN", &[]).unwrap();
    for _ in 0..rows {
        conn.execute("INSERT INTO t (nota) VALUES (?1)", &params![nota.clone()])
            .unwrap();
    }
    conn.execute("COMMIT", &[]).unwrap();
    assert!(db.verify().unwrap().chain_ok);
}

/// Primer valor de la primera fila.
fn get1<T: FromValue>(conn: &Connection, sql: &str) -> T {
    conn.query(sql, &[])
        .unwrap()
        .next()
        .expect("una fila")
        .unwrap()
        .get(0)
        .unwrap()
}

#[test]
fn compressed_is_smaller_and_lossless() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain.arkeion");
    let comp = dir.path().join("comp.arkeion");
    build(&plain, Options::default(), 3000);
    build(&comp, Options::default().compress(true), 3000);

    let s_plain = std::fs::metadata(&plain).unwrap().len();
    let s_comp = std::fs::metadata(&comp).unwrap().len();
    assert!(
        s_comp * 2 < s_plain,
        "comprimido {s_comp} debería ser < mitad de {s_plain}"
    );

    // Reabrir el comprimido SIN pedir compresión: se lee del header. verify OK,
    // datos íntegros.
    let db = Database::open(&comp, Options::default().create_if_missing(false)).unwrap();
    assert!(db.verify().unwrap().chain_ok);
    let conn = db.connect().unwrap();
    assert_eq!(get1::<i64>(&conn, "SELECT COUNT(*) FROM t"), 3000);
    let sample: String = get1(&conn, "SELECT nota FROM t WHERE id = 1500");
    assert_eq!(sample, nota());
}

#[test]
fn encrypted_and_compressed_together() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ec.arkeion");
    let key = || Key::new([0x5A; 32]);

    build(&path, Options::default().key(key()).compress(true), 1500);

    // Sin clave ⇒ KeyRequired (no "base vacía"); con la clave, datos íntegros.
    assert!(matches!(
        Database::open(&path, Options::default().create_if_missing(false)),
        Err(Error::KeyRequired)
    ));
    let db = Database::open(
        &path,
        Options::default().create_if_missing(false).key(key()),
    )
    .unwrap();
    assert!(db.verify().unwrap().chain_ok);
    let conn = db.connect().unwrap();
    assert_eq!(get1::<i64>(&conn, "SELECT COUNT(*) FROM t"), 1500);
    let sample: String = get1(&conn, "SELECT nota FROM t WHERE id = 1");
    assert_eq!(sample, nota());
}

#[test]
fn tampering_a_compressed_page_is_detected_before_decompress() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tamper.arkeion");
    build(&path, Options::default().compress(true), 200);

    // Corrompe un byte del payload sellado del primer registro de datos (su
    // marco empieza al inicio de la zona append). El tag —sobre los bytes
    // finales, comprimidos+sellados— lo atrapa antes de descomprimir.
    let off = FIRST_DATA_PAGE.byte_offset() as usize + LEN_PREFIX_LEN + 8;
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[off] ^= 0x01;
    std::fs::write(&path, &bytes).unwrap();

    let result = Database::open(&path, Options::default().create_if_missing(false))
        .and_then(|db| db.verify());
    assert!(
        matches!(result, Err(Error::ChainBroken { .. })),
        "una página comprimida manipulada debe dar ChainBroken: {result:?}"
    );
}
