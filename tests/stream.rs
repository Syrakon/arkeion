//! Full scan en streaming: equivalencia exacta con el camino materializado.
//!
//! Oráculo: la misma consulta DENTRO de `BEGIN` va por el executor clásico
//! (la fuente es la tx prestada: sin streaming), así que comparar dentro vs
//! fuera detecta cualquier divergencia del camino nuevo.

use arkeion::{Database, Options, Value};
use tempfile::TempDir;

fn db() -> (TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("s.arkeion"), Options::default()).unwrap();
    (dir, db)
}

/// Todas las filas (nombres de columna + valores) de una consulta.
fn rows(conn: &arkeion::Connection, sql: &str) -> (Vec<String>, Vec<Vec<Value>>) {
    let rows = conn.query(sql, &[]).unwrap();
    let cols = rows.columns().to_vec();
    let vals = rows
        .map(|r| r.unwrap().values().to_vec())
        .collect::<Vec<_>>();
    (cols, vals)
}

/// La consulta da lo MISMO en streaming (autocommit) que materializada (en tx).
fn assert_equivalent(conn: &arkeion::Connection, sql: &str) {
    let streamed = rows(conn, sql);
    conn.execute("BEGIN", &[]).unwrap();
    let buffered = rows(conn, sql);
    conn.execute("ROLLBACK", &[]).unwrap();
    assert_eq!(streamed, buffered, "divergencia en: {sql}");
}

#[test]
fn streaming_matches_materialized_paths() {
    let (_dir, db) = db();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER, s TEXT, r REAL, b BOOLEAN)",
        &[],
    )
    .unwrap();
    conn.execute("BEGIN", &[]).unwrap();
    for i in 1..=50i64 {
        conn.execute(
            "INSERT INTO t VALUES (?1, ?2, ?3, ?4, ?5)",
            &[
                Value::Integer(i),
                if i % 7 == 0 {
                    Value::Null
                } else {
                    Value::Integer(i * 3)
                },
                if i % 5 == 0 {
                    Value::Null
                } else {
                    Value::Text(format!("fila {i} ñ"))
                },
                Value::Real(i as f64 / 2.0),
                Value::Bool(i % 2 == 0),
            ],
        )
        .unwrap();
    }
    // Rowid negativo: el orden del scan debe respetarlo igual en ambos caminos.
    conn.execute("INSERT INTO t VALUES (-3, 1, 'neg', 0.5, FALSE)", &[])
        .unwrap();
    conn.execute("COMMIT", &[]).unwrap();

    for sql in [
        "SELECT * FROM t",
        "SELECT n FROM t",
        "SELECT s, n FROM t",
        "SELECT id FROM t",         // alias del rowid reconstruido
        "SELECT n, n FROM t",       // columna repetida
        "SELECT t.n FROM t",        // calificada
        "SELECT n AS x, id FROM t", // alias de salida
        "SELECT * FROM t LIMIT 10",
        "SELECT * FROM t LIMIT 10 OFFSET 45", // cola parcial
        "SELECT n FROM t LIMIT 0",
        "SELECT * FROM t OFFSET 100", // más allá del final
    ] {
        assert_equivalent(&conn, sql);
    }
}

#[test]
fn streaming_after_alter_add_column_applies_defaults() {
    let (_dir, db) = db();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)", &[])
        .unwrap();
    conn.execute("INSERT INTO t VALUES (1, 10)", &[]).unwrap();
    conn.execute("ALTER TABLE t ADD COLUMN d INTEGER DEFAULT 42", &[])
        .unwrap();
    conn.execute("ALTER TABLE t ADD COLUMN e TEXT", &[])
        .unwrap();
    conn.execute("INSERT INTO t VALUES (2, 20, 7, 'x')", &[])
        .unwrap();
    // La fila vieja no se reescribió: d/e salen del DEFAULT (42 / NULL),
    // idéntico por ambos caminos.
    assert_equivalent(&conn, "SELECT * FROM t");
    assert_equivalent(&conn, "SELECT e, d FROM t");
    let (_, vals) = rows(&conn, "SELECT d, e FROM t");
    assert_eq!(vals[0], vec![Value::Integer(42), Value::Null]);
    assert_eq!(vals[1], vec![Value::Integer(7), Value::Text("x".into())]);
}

#[test]
fn streaming_respects_as_of_and_pinned_snapshots() {
    let (_dir, db) = db();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)", &[])
        .unwrap();
    conn.execute("INSERT INTO t VALUES (1, 1)", &[]).unwrap();
    let v1 = conn.version();
    conn.execute("INSERT INTO t VALUES (2, 2)", &[]).unwrap();

    // AS OF VERSION en streaming: ve solo la primera fila.
    let sql = format!("SELECT n FROM t AS OF VERSION {v1}");
    let (_, vals) = rows(&conn, &sql);
    assert_eq!(vals, vec![vec![Value::Integer(1)]]);

    // Conexión fijada (snapshot): mismo resultado.
    let antes = conn.snapshot(arkeion::AsOf::Version(v1)).unwrap();
    let (_, vals) = rows(&antes, "SELECT n FROM t");
    assert_eq!(vals, vec![vec![Value::Integer(1)]]);
}

#[test]
fn streaming_sees_a_stable_snapshot_while_writes_land() {
    let (_dir, db) = db();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)", &[])
        .unwrap();
    for i in 1..=100i64 {
        conn.execute(
            "INSERT INTO t VALUES (?1, ?2)",
            &[Value::Integer(i), Value::Integer(i)],
        )
        .unwrap();
    }
    // Rows en vuelo: escrituras posteriores no aparecen (snapshot inmutable).
    let mut it = conn.query("SELECT id FROM t", &[]).unwrap();
    let first = it.next().unwrap().unwrap().get::<i64>(0).unwrap();
    assert_eq!(first, 1);
    let conn2 = db.connect().unwrap();
    conn2
        .execute("INSERT INTO t VALUES (101, 101)", &[])
        .unwrap();
    let mut rest = 0;
    let mut last = first;
    for r in it {
        last = r.unwrap().get::<i64>(0).unwrap();
        rest += 1;
    }
    assert_eq!((rest, last), (99, 100), "el stream no debe ver la fila 101");
}
