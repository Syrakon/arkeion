//! Features de superficie SQL añadidas tras el dogfooding: alias de columna,
//! funciones escalares, `IN (...)` y `SELECT <expr>` sin `FROM`.

use arkeion::{Database, Options, Value};

fn db() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("t.arkeion"), Options::default()).unwrap();
    (dir, db)
}

fn one(conn: &arkeion::Connection, sql: &str) -> Value {
    conn.query(sql, &[])
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .get::<Value>(0)
        .unwrap()
}

#[test]
fn column_aliases_name_the_output() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)", &[])
        .unwrap();
    conn.execute("INSERT INTO t (n) VALUES (5)", &[]).unwrap();

    let rows = conn
        .query("SELECT n AS valor, n + 1 AS sig FROM t", &[])
        .unwrap();
    assert_eq!(rows.columns(), ["valor".to_string(), "sig".to_string()]);
    // Alias sobre un agregado.
    let rows = conn.query("SELECT count(*) AS total FROM t", &[]).unwrap();
    assert_eq!(rows.columns(), ["total".to_string()]);
}
