//! `INSERT`/`UPDATE`/`DELETE … RETURNING <select-list>`: la escritura devuelve
//! las filas afectadas (vía `query`). El runner declarativo de `tests/sql/`
//! enruta lo no-SELECT a `execute` (recuento), así que esto se prueba aquí.

use arkeion::{Connection, Database, Options, Value};

fn fresh() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("r.arkeion"), Options::default()).unwrap();
    (dir, db)
}

/// Filas de una consulta, como `Vec<Vec<Value>>`.
fn rows_of(conn: &Connection, sql: &str) -> Vec<Vec<Value>> {
    let rows = conn.query(sql, &[]).unwrap();
    let n = rows.columns().len();
    rows.map(|r| {
        let r = r.unwrap();
        (0..n).map(|i| r.get::<Value>(i).unwrap()).collect()
    })
    .collect()
}

fn int(n: i64) -> Value {
    Value::Integer(n)
}
fn txt(s: &str) -> Value {
    Value::Text(s.into())
}

#[test]
fn insert_returning_projects_inserted_rows() {
    let (_d, db) = fresh();
    let c = db.connect().unwrap();
    c.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, n INTEGER)",
        &[],
    )
    .unwrap();

    // RETURNING ve el rowid autogenerado.
    assert_eq!(
        rows_of(
            &c,
            "INSERT INTO t (name, n) VALUES ('a', 1), ('b', 2) RETURNING id, name"
        ),
        vec![vec![int(1), txt("a")], vec![int(2), txt("b")]],
    );

    // RETURNING * = columnas visibles (orden lógico).
    assert_eq!(
        rows_of(&c, "INSERT INTO t (name, n) VALUES ('c', 3) RETURNING *"),
        vec![vec![int(3), txt("c"), int(3)]],
    );

    // RETURNING admite expresiones y alias.
    assert_eq!(
        rows_of(
            &c,
            "INSERT INTO t (name, n) VALUES ('dd', 5) RETURNING id, upper(name) AS up, n * 2"
        ),
        vec![vec![int(4), txt("DD"), int(10)]],
    );
}

#[test]
fn update_returning_sees_new_values() {
    let (_d, db) = fresh();
    let c = db.connect().unwrap();
    c.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)", &[])
        .unwrap();
    c.execute("INSERT INTO t (n) VALUES (1), (2), (3)", &[])
        .unwrap();

    // RETURNING proyecta la fila NEW (tras el SET).
    assert_eq!(
        rows_of(&c, "UPDATE t SET n = n + 100 WHERE id <= 2 RETURNING id, n"),
        vec![vec![int(1), int(101)], vec![int(2), int(102)]],
    );
}

#[test]
fn delete_returning_projects_deleted_rows() {
    let (_d, db) = fresh();
    let c = db.connect().unwrap();
    c.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)", &[])
        .unwrap();
    c.execute("INSERT INTO t (name) VALUES ('x'), ('y')", &[])
        .unwrap();

    assert_eq!(
        rows_of(&c, "DELETE FROM t WHERE id = 1 RETURNING id, name"),
        vec![vec![int(1), txt("x")]],
    );
    // La fila ya no está.
    assert!(rows_of(&c, "SELECT id FROM t WHERE id = 1").is_empty());
}

#[test]
fn execute_of_returning_statement_still_returns_count() {
    let (_d, db) = fresh();
    let c = db.connect().unwrap();
    c.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)", &[])
        .unwrap();
    // Vía execute(): la escritura se hace y se devuelve el recuento (filas descartadas).
    let n = c
        .execute("INSERT INTO t (n) VALUES (9) RETURNING id", &[])
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(rows_of(&c, "SELECT n FROM t"), vec![vec![int(9)]]);
}

#[test]
fn returning_inside_a_transaction() {
    let (_d, db) = fresh();
    let c = db.connect().unwrap();
    c.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)", &[])
        .unwrap();

    c.execute("BEGIN", &[]).unwrap();
    assert_eq!(
        rows_of(&c, "INSERT INTO t (name) VALUES ('tx') RETURNING name"),
        vec![vec![txt("tx")]],
    );
    c.execute("COMMIT", &[]).unwrap();

    assert_eq!(rows_of(&c, "SELECT name FROM t"), vec![vec![txt("tx")]]);
}
