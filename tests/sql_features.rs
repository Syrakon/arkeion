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

#[test]
fn scalar_functions() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT, n INTEGER)",
        &[],
    )
    .unwrap();
    conn.execute("INSERT INTO t (s, n) VALUES ('  Hi  ', -5)", &[])
        .unwrap();

    assert_eq!(
        one(&conn, "SELECT UPPER(s) FROM t"),
        Value::Text("  HI  ".into())
    );
    assert_eq!(
        one(&conn, "SELECT LENGTH(TRIM(s)) FROM t"),
        Value::Integer(2)
    ); // anidada
    assert_eq!(one(&conn, "SELECT ABS(n) FROM t"), Value::Integer(5));
    assert_eq!(
        one(&conn, "SELECT COALESCE(NULL, n) FROM t"),
        Value::Integer(-5)
    );
    assert_eq!(
        one(&conn, "SELECT ROUND(1.23456, 2) FROM t"),
        Value::Real(1.23)
    );
    assert_eq!(
        one(&conn, "SELECT SUBSTR(s, 3, 2) FROM t"),
        Value::Text("Hi".into())
    );
    assert_eq!(
        one(&conn, "SELECT TYPEOF(n) FROM t"),
        Value::Text("INTEGER".into())
    );
    // NULL se propaga; el tipo equivocado y la función desconocida son errores.
    assert_eq!(one(&conn, "SELECT UPPER(NULL) FROM t"), Value::Null);
    assert!(conn.query("SELECT UPPER(n) FROM t", &[]).is_err());
    assert!(conn.query("SELECT NOPE(1) FROM t", &[]).is_err());
}
