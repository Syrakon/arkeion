//! API de **motor** (sin SQL): `Connection::table` → `TableReader` (`get`/`scan`/
//! `scan_columns`/`count`). Se cruza contra la vía SQL sobre los MISMOS datos: el
//! motor salta el parser/planner/ejecutor, pero debe dar exactamente lo mismo.

use arkeion::{Database, Options, Value};

fn db() -> Database {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("engine.arkeion"), Options::default()).unwrap();
    std::mem::forget(dir); // vive lo que el test
    db
}

#[test]
fn engine_reads_match_sql() {
    let db = db();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER NOT NULL)", &[])
        .unwrap();
    conn.bulk_insert(
        "t",
        (1..=1000i64).map(|i| [Value::Integer(i), Value::Integer(i * 2)]),
    )
    .unwrap();

    let t = conn.table("t").unwrap();

    // count() == SELECT COUNT(*)
    assert_eq!(t.count().unwrap(), 1000);

    // get(rowid) == SELECT * WHERE id = ?  (fila completa [id, n])
    for id in [1i64, 7, 500, 999, 1000] {
        let row = t.get(id).unwrap();
        assert_eq!(row, Some(vec![Value::Integer(id), Value::Integer(id * 2)]));
    }
    // rowid inexistente → None
    assert_eq!(t.get(1001).unwrap(), None);
    assert_eq!(t.get(0).unwrap(), None);

    // scan() recorre todas las filas por rowid ascendente, igual que SELECT * ORDER BY id.
    let scanned: Vec<(i64, Vec<Value>)> = t.scan().unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(scanned.len(), 1000);
    assert_eq!(scanned[0], (1, vec![Value::Integer(1), Value::Integer(2)]));
    assert_eq!(
        scanned[999],
        (1000, vec![Value::Integer(1000), Value::Integer(2000)])
    );

    // scan_columns([n]) proyecta SOLO n; su suma == suma de SELECT n.
    let n_col = t.column_index("n").unwrap();
    let mut sum_engine = 0i64;
    let mut proj = t.scan_columns(&[n_col]).unwrap();
    while let Some(row) = proj.next().unwrap() {
        assert_eq!(row.len(), 1);
        if let Value::Integer(v) = row[0] {
            sum_engine += v;
        } else {
            panic!("n no es entero");
        }
    }
    let sum_sql: i64 = conn
        .query("SELECT SUM(n) FROM t", &[])
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .get(0)
        .unwrap();
    assert_eq!(sum_engine, sum_sql);
    assert_eq!(sum_engine, (1..=1000).map(|i| i * 2).sum());
}

/// El alias del rowid y las columnas añadidas por `ALTER TABLE` se reconstruyen
/// igual por la vía motor que por SQL (mismo `finish_row`/proyección).
#[test]
fn engine_reconstructs_rowid_alias_and_added_columns() {
    let db = db();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER NOT NULL)", &[])
        .unwrap();
    conn.execute("INSERT INTO t (id, n) VALUES (1, 10), (2, 20)", &[])
        .unwrap();
    // Columna nueva con DEFAULT: las filas viejas no se reescriben; debe verse el default.
    conn.execute("ALTER TABLE t ADD COLUMN tag TEXT DEFAULT 'x'", &[])
        .unwrap();

    let t = conn.table("t").unwrap();
    // get reconstruye el alias del rowid (id) y el default de la columna añadida.
    assert_eq!(
        t.get(1).unwrap(),
        Some(vec![
            Value::Integer(1),
            Value::Integer(10),
            Value::Text("x".into()),
        ])
    );
    // scan_columns puede pedir el alias del rowid y la columna nueva a la vez.
    let (id_c, tag_c) = (t.column_index("id").unwrap(), t.column_index("tag").unwrap());
    let mut proj = t.scan_columns(&[id_c, tag_c]).unwrap();
    let first = proj.next().unwrap().unwrap().to_vec();
    assert_eq!(first, vec![Value::Integer(1), Value::Text("x".into())]);
}

/// El lector toma su snapshot al crearse: ve un estado estable y NO las escrituras
/// posteriores (aislamiento por snapshot, como el time-travel del motor).
#[test]
fn engine_reader_is_a_stable_snapshot() {
    let db = db();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER NOT NULL)", &[])
        .unwrap();
    conn.execute("INSERT INTO t (id, n) VALUES (1, 10)", &[])
        .unwrap();

    let t = conn.table("t").unwrap(); // snapshot con 1 fila
    conn.execute("INSERT INTO t (id, n) VALUES (2, 20)", &[])
        .unwrap(); // escritura posterior

    assert_eq!(t.count().unwrap(), 1); // el lector sigue viendo 1
    assert_eq!(t.get(2).unwrap(), None); // no ve la fila nueva
    // Un lector nuevo sí la ve.
    assert_eq!(conn.table("t").unwrap().count().unwrap(), 2);
}
