//! Integración M5 — time-travel por SQL: `SELECT … AS OF VERSION/TIMESTAMP`
//! sobre la API pública. El camino de resolución (índice histórico) tiene su
//! prueba exhaustiva a nivel KV en `src/tx.rs`; aquí se valida de extremo a
//! extremo el SQL y el cableado de la conexión.

use arkeion::{AsOf, Connection, Database, Error, Options, Value};

fn fresh() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("tt.arkeion"), Options::default()).unwrap();
    (dir, db)
}

/// Valores de la única fila de `sql`, en el orden de columnas que devuelve.
fn star_row(conn: &Connection, sql: &str) -> Vec<Value> {
    let mut rows = conn.query(sql, &[]).unwrap();
    let ncols = rows.columns().len();
    let row = rows.next().expect("la consulta no devolvió filas").unwrap();
    (0..ncols).map(|i| row.get::<Value>(i).unwrap()).collect()
}

/// Valores de la columna `n` de una consulta, en orden de fila.
fn nums(conn: &Connection, sql: &str) -> Vec<i64> {
    conn.query(sql, &[])
        .unwrap()
        .map(|row| row.unwrap().get::<i64>("n").unwrap())
        .collect()
}

#[test]
fn as_of_version_sees_each_past_state() {
    let (_dir, db) = fresh();
    let conn = db.connect().unwrap();

    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER NOT NULL)",
        &[],
    )
    .unwrap();
    let v_create = conn.version(); // tabla creada, aún vacía

    conn.execute("INSERT INTO t (n) VALUES (10)", &[]).unwrap();
    let v_one = conn.version(); // una fila

    conn.execute("INSERT INTO t (n) VALUES (20)", &[]).unwrap();
    let v_two = conn.version(); // dos filas

    // Presente.
    assert_eq!(nums(&conn, "SELECT n FROM t ORDER BY n"), vec![10, 20]);

    // Cada versión histórica reproduce su estado exacto.
    assert_eq!(
        nums(
            &conn,
            &format!("SELECT n FROM t ORDER BY n AS OF VERSION {v_one}")
        ),
        vec![10],
    );
    assert!(nums(&conn, &format!("SELECT n FROM t AS OF VERSION {v_create}")).is_empty());

    // La consulta histórica no mueve el head.
    assert_eq!(conn.version(), v_two);
}

#[test]
fn as_of_version_in_the_future_is_not_found() {
    let (_dir, db) = fresh();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)", &[])
        .unwrap();

    let future = conn.version() + 100;
    // `Rows` no es Debug ⇒ no se puede `unwrap_err`; `.err()` descarta el Ok.
    let err = conn
        .query(&format!("SELECT n FROM t AS OF VERSION {future}"), &[])
        .err()
        .unwrap();
    assert!(matches!(err, Error::VersionNotFound(_)), "fue {err:?}");
}

#[test]
fn as_of_timestamp_resolves_boundaries() {
    let (_dir, db) = fresh();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER NOT NULL)",
        &[],
    )
    .unwrap();
    conn.execute("INSERT INTO t (n) VALUES (42)", &[]).unwrap();

    // Antes de cualquier commit: estado génesis (la tabla aún no existía).
    let err = conn
        .query(
            "SELECT n FROM t AS OF TIMESTAMP '2000-01-01T00:00:00Z'",
            &[],
        )
        .err()
        .unwrap();
    assert!(matches!(err, Error::Sql { .. }), "fue {err:?}");

    // Muy en el futuro: ve el estado vivo.
    assert_eq!(
        nums(
            &conn,
            "SELECT n FROM t AS OF TIMESTAMP '2100-01-01T00:00:00Z'"
        ),
        vec![42],
    );
}

#[test]
fn as_of_rejected_inside_write_transaction() {
    let (_dir, db) = fresh();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)", &[])
        .unwrap();

    let tx = conn.begin().unwrap();
    let err = tx
        .query("SELECT n FROM t AS OF VERSION 1", &[])
        .err()
        .unwrap();
    assert!(matches!(err, Error::Sql { .. }), "fue {err:?}");
}

#[test]
fn pinned_snapshot_connection_is_read_only_at_its_version() {
    let (_dir, db) = fresh();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER NOT NULL)",
        &[],
    )
    .unwrap();
    conn.execute("INSERT INTO t (n) VALUES (1)", &[]).unwrap();
    let v_one = conn.version();
    conn.execute("INSERT INTO t (n) VALUES (2)", &[]).unwrap();

    // Conexión fijada a la versión con una sola fila.
    let pinned = conn.snapshot(AsOf::Version(v_one)).unwrap();
    assert_eq!(pinned.version(), v_one);
    assert_eq!(nums(&pinned, "SELECT n FROM t ORDER BY n"), vec![1]);

    // Solo lectura: ni escrituras ni transacciones.
    assert!(pinned.execute("INSERT INTO t (n) VALUES (9)", &[]).is_err());
    assert!(pinned.begin().is_err());

    // La conexión viva sigue al día y la fijada no la perturbó.
    assert_eq!(nums(&conn, "SELECT n FROM t ORDER BY n"), vec![1, 2]);

    // Fijar a una versión inexistente falla en el acto.
    assert!(conn.snapshot(AsOf::Version(conn.version() + 50)).is_err());
}

/// Reordenar columnas es **lógico** (de presentación): un `AS OF` anterior al
/// reorden ve el orden de su época. La fila histórica nunca se reescribe (el
/// registro es posicional-físico); solo cambia la permutación de presentación,
/// que se versiona en el mismo b-tree que los datos. El acceso por nombre es
/// independiente del orden, en head y en el pasado.
#[test]
fn column_reorder_is_logical_and_time_travel_safe() {
    let (_dir, db) = fresh();
    let conn = db.connect().unwrap();

    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER, c INTEGER)",
        &[],
    )
    .unwrap();
    conn.execute("INSERT INTO t (a, b, c) VALUES (10, 20, 30)", &[])
        .unwrap();
    let v_before = conn.version(); // orden de presentación de entonces: id, a, b, c

    conn.execute("ALTER TABLE t REORDER COLUMNS (c, b, a, id)", &[])
        .unwrap();

    // Head: `*` sale en el orden nuevo c, b, a, id.
    assert_eq!(
        star_row(&conn, "SELECT * FROM t"),
        vec![
            Value::Integer(30),
            Value::Integer(20),
            Value::Integer(10),
            Value::Integer(1),
        ],
    );

    // AS OF antes del reorden: el orden de entonces, id, a, b, c. La fila en disco
    // es idéntica; solo se versionó la permutación de presentación.
    assert_eq!(
        star_row(&conn, &format!("SELECT * FROM t AS OF VERSION {v_before}")),
        vec![
            Value::Integer(1),
            Value::Integer(10),
            Value::Integer(20),
            Value::Integer(30),
        ],
    );

    // El acceso por nombre no depende del orden, ni en head ni en el pasado.
    assert_eq!(
        star_row(&conn, "SELECT a, b, c FROM t"),
        vec![Value::Integer(10), Value::Integer(20), Value::Integer(30)],
    );
    assert_eq!(
        star_row(
            &conn,
            &format!("SELECT a, b, c FROM t AS OF VERSION {v_before}")
        ),
        vec![Value::Integer(10), Value::Integer(20), Value::Integer(30)],
    );
}

/// `DROP COLUMN` es **lógico** (tombstone): la fila no se reescribe, así que un
/// `AS OF` anterior al DROP sigue viendo la columna con sus valores.
#[test]
fn drop_column_is_time_travel_safe() {
    let (_dir, db) = fresh();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER)",
        &[],
    )
    .unwrap();
    conn.execute("INSERT INTO t (a, b) VALUES (10, 20)", &[])
        .unwrap();
    let before = conn.version();

    conn.execute("ALTER TABLE t DROP COLUMN b", &[]).unwrap();

    // Head: `b` ya no aparece — `*` = id, a.
    assert_eq!(
        star_row(&conn, "SELECT * FROM t"),
        vec![Value::Integer(1), Value::Integer(10)],
    );
    // AS OF antes del DROP: `b` sigue ahí (catálogo versionado, fila no reescrita).
    assert_eq!(
        star_row(&conn, &format!("SELECT * FROM t AS OF VERSION {before}")),
        vec![Value::Integer(1), Value::Integer(10), Value::Integer(20)],
    );
}
