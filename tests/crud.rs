//! Aceptación del hito M4 (docs/06-hitos.md): una app CRUD realista (fixture
//! de gestión: clientes/facturas/líneas) corre entera contra Arkeion; un
//! rollback restaura estado byte-idéntico; lectores concurrentes durante
//! escritura sostenida no observan estados intermedios.

use std::time::{Duration, Instant};
use std::{fs, thread};

use arkeion::{Connection, Database, FromValue, Options, params};

fn schema(conn: &Connection) {
    conn.execute(
        "CREATE TABLE clientes (id INTEGER PRIMARY KEY, nombre TEXT NOT NULL, vip BOOLEAN)",
        &[],
    )
    .unwrap();
    conn.execute(
        "CREATE TABLE facturas (id INTEGER PRIMARY KEY, cliente_id INTEGER NOT NULL, \
         estado TEXT NOT NULL DEFAULT 'borrador')",
        &[],
    )
    .unwrap();
    conn.execute(
        "CREATE TABLE lineas (id INTEGER PRIMARY KEY, factura_id INTEGER NOT NULL, \
         concepto TEXT NOT NULL, importe REAL NOT NULL)",
        &[],
    )
    .unwrap();
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
fn una_app_crud_realista_corre_entera() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crud.arkeion");
    let db = Database::open(&path, Options::default()).unwrap();
    let conn = db.connect().unwrap();
    schema(&conn);

    // Altas con una sentencia preparada: se parsea una vez, se ejecuta N.
    let alta = conn
        .prepare("INSERT INTO clientes (nombre, vip) VALUES (?1, ?2)")
        .unwrap();
    for (nombre, vip) in [("Acme GmbH", true), ("Beta SL", false), ("Gamma Oy", false)] {
        assert_eq!(alta.execute(&params![nombre, vip]).unwrap(), 1);
    }

    // Una factura nace con sus líneas, atómicamente.
    let tx = conn.begin().unwrap();
    tx.execute(
        "INSERT INTO facturas (cliente_id, estado) VALUES (?1, 'emitida')",
        &params![1],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO lineas (factura_id, concepto, importe) \
         VALUES (1, 'licencia', 1200.0), (1, 'soporte', 300.0)",
        &[],
    )
    .unwrap();
    // La transacción ve lo suyo; la conexión (snapshot), todavía no.
    let dentro: i64 = tx
        .query("SELECT COUNT(*) FROM lineas", &[])
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .get(0)
        .unwrap();
    assert_eq!(dentro, 2);
    assert_eq!(get1::<i64>(&conn, "SELECT COUNT(*) FROM lineas"), 0);
    tx.commit().unwrap();
    assert_eq!(get1::<i64>(&conn, "SELECT COUNT(*) FROM lineas"), 2);

    // Un borrador que se queda a medias se abandona (BEGIN/ROLLBACK SQL).
    conn.execute("BEGIN", &[]).unwrap();
    conn.execute("INSERT INTO facturas (cliente_id) VALUES (2)", &[])
        .unwrap();
    assert_eq!(get1::<i64>(&conn, "SELECT COUNT(*) FROM facturas"), 2);
    conn.execute("ROLLBACK", &[]).unwrap();
    assert_eq!(get1::<i64>(&conn, "SELECT COUNT(*) FROM facturas"), 1);

    // Consulta de negocio: JOIN triple + agregado.
    let facturado: f64 = get1(
        &conn,
        "SELECT SUM(l.importe) FROM clientes c \
         INNER JOIN facturas f ON f.cliente_id = c.id \
         INNER JOIN lineas l ON l.factura_id = f.id \
         WHERE f.estado = 'emitida' AND c.vip",
    );
    assert_eq!(facturado, 1500.0);

    // El día a día: cobrar, repercutir el IVA, depurar clientes.
    assert_eq!(
        conn.execute("UPDATE facturas SET estado = 'pagada' WHERE id = 1", &[])
            .unwrap(),
        1
    );
    assert_eq!(
        conn.execute("UPDATE lineas SET importe = importe * 1.21", &[])
            .unwrap(),
        2
    );
    assert_eq!(
        conn.execute("DELETE FROM clientes WHERE NOT vip AND id != 2", &[])
            .unwrap(),
        1
    );

    // Persistencia: reabrir y seguir donde estábamos.
    drop(conn);
    drop(db);
    let db = Database::open(&path, Options::default()).unwrap();
    let conn = db.connect().unwrap();
    assert_eq!(get1::<i64>(&conn, "SELECT COUNT(*) FROM clientes"), 2);
    let estado: String = get1(&conn, "SELECT estado FROM facturas WHERE id = 1");
    assert_eq!(estado, "pagada");
    let total: f64 = get1(&conn, "SELECT SUM(importe) FROM lineas");
    assert!((total - 1815.0).abs() < 1e-9, "total: {total}");
}

#[test]
fn un_rollback_restaura_estado_byte_identico() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crud.arkeion");
    let db = Database::open(&path, Options::default()).unwrap();
    let conn = db.connect().unwrap();
    schema(&conn);
    conn.execute("INSERT INTO clientes (nombre) VALUES ('ana')", &[])
        .unwrap();

    let antes = fs::read(&path).unwrap();
    let version = conn.version();

    // Una transacción con trabajo de verdad, abandonada de las dos formas.
    let tx = conn.begin().unwrap();
    tx.execute(
        "INSERT INTO clientes (nombre, vip) VALUES ('bo', TRUE)",
        &[],
    )
    .unwrap();
    tx.execute("UPDATE clientes SET nombre = 'ana maria' WHERE id = 1", &[])
        .unwrap();
    tx.execute("CREATE TABLE temporal (x INTEGER)", &[])
        .unwrap();
    tx.execute("DELETE FROM clientes WHERE id = 1", &[])
        .unwrap();
    tx.rollback().unwrap();

    assert_eq!(
        fs::read(&path).unwrap(),
        antes,
        "el archivo no debe cambiar"
    );
    assert_eq!(conn.version(), version);

    conn.execute("BEGIN", &[]).unwrap();
    conn.execute("INSERT INTO clientes (nombre) VALUES ('eva')", &[])
        .unwrap();
    conn.execute("ROLLBACK", &[]).unwrap();

    assert_eq!(fs::read(&path).unwrap(), antes, "ROLLBACK SQL, mismo trato");
    assert_eq!(conn.version(), version);
    assert_eq!(get1::<i64>(&conn, "SELECT COUNT(*) FROM clientes"), 1);
}

#[test]
fn lectores_concurrentes_no_ven_estados_intermedios() {
    const COMMITS: i64 = 40;
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("crud.arkeion"), Options::default()).unwrap();
    schema(&db.connect().unwrap());

    // Invariante publicada por commit: cada factura nace con su cliente y
    // exactamente 3 líneas, en una única transacción.
    let escritor = {
        let db = db.clone();
        thread::spawn(move || {
            let conn = db.connect().unwrap();
            for i in 0..COMMITS {
                let tx = conn.begin().unwrap();
                tx.execute(
                    "INSERT INTO clientes (nombre) VALUES (?1)",
                    &params![format!("c{i}")],
                )
                .unwrap();
                tx.execute(
                    "INSERT INTO facturas (cliente_id, estado) VALUES (?1, 'emitida')",
                    &params![i + 1],
                )
                .unwrap();
                for j in 0..3 {
                    tx.execute(
                        "INSERT INTO lineas (factura_id, concepto, importe) \
                         VALUES (?1, ?2, 10.0)",
                        &params![i + 1, format!("l{j}")],
                    )
                    .unwrap();
                }
                tx.commit().unwrap();
            }
        })
    };

    let lectores: Vec<_> = (0..4)
        .map(|_| {
            let db = db.clone();
            thread::spawn(move || {
                let conn = db.connect().unwrap();
                let limite = Instant::now() + Duration::from_secs(60);
                loop {
                    // Un único SELECT = un único snapshot: si el commit no
                    // fuera atómico, aquí asomaría una factura a medias.
                    let row = conn
                        .query(
                            "SELECT COUNT(*), COUNT(l.id) FROM facturas f \
                             LEFT JOIN lineas l ON l.factura_id = f.id",
                            &[],
                        )
                        .unwrap()
                        .next()
                        .unwrap()
                        .unwrap();
                    let total: i64 = row.get(0).unwrap();
                    let con_linea: i64 = row.get(1).unwrap();
                    assert_eq!(total, con_linea, "factura visible sin sus líneas");
                    assert_eq!(total % 3, 0, "líneas a medio insertar visibles");
                    if total == COMMITS * 3 {
                        return;
                    }
                    assert!(Instant::now() < limite, "el escritor no avanza");
                }
            })
        })
        .collect();

    escritor.join().unwrap();
    for lector in lectores {
        lector.join().unwrap();
    }
}
