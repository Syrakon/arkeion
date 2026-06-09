//! Bulk-load API (`Connection::bulk_insert`): atomicidad o-todo-o-nada,
//! equivalencia con el INSERT fila a fila (defaults, rowids explícitos,
//! índices con UNIQUE diferido) y sus restricciones (autocommit).

use arkeion::{Database, Options, Value, params};
use tempfile::TempDir;

fn db() -> (TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("bulk.arkeion"), Options::default()).unwrap();
    (dir, db)
}

fn count(conn: &arkeion::Connection, sql: &str) -> i64 {
    conn.query(sql, &[])
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .get::<i64>(0)
        .unwrap()
}

#[test]
fn roundtrip_and_reopen() {
    let (dir, db) = db();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER NOT NULL)",
        &[],
    )
    .unwrap();
    let n = conn
        .bulk_insert(
            "t",
            (1..=1000i64).map(|i| [Value::Integer(i), Value::Integer(i * 2)]),
        )
        .unwrap();
    assert_eq!(n, 1000);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM t"), 1000);
    assert_eq!(count(&conn, "SELECT n FROM t WHERE id = 500"), 1000);

    // Persistencia tras reabrir.
    drop(conn);
    drop(db);
    let db = Database::open(dir.path().join("bulk.arkeion"), Options::default()).unwrap();
    let conn = db.connect().unwrap();
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM t"), 1000);
    assert_eq!(count(&conn, "SELECT n FROM t WHERE id = 1000"), 2000);
}

#[test]
fn defaults_auto_rowid_and_mixed_explicit() {
    let (_dir, db) = db();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER DEFAULT 7, b TEXT)",
        &[],
    )
    .unwrap();
    // Columnas ausentes al final → DEFAULT (o NULL); alias NULL → rowid auto.
    conn.bulk_insert(
        "t",
        vec![
            vec![Value::Null, Value::Integer(1), Value::Text("x".into())],
            vec![Value::Null],                    // a=7 (default), b=NULL
            vec![Value::Integer(10)],             // rowid explícito 10
            vec![Value::Null, Value::Integer(3)], // tras el 10 → rowid 11
        ],
    )
    .unwrap();
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM t"), 4);
    assert_eq!(count(&conn, "SELECT a FROM t WHERE id = 2"), 7);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM t WHERE id = 10"), 1);
    assert_eq!(count(&conn, "SELECT a FROM t WHERE id = 11"), 3);
    // El contador siguió: el próximo INSERT normal no choca.
    conn.execute("INSERT INTO t (a) VALUES (99)", &[]).unwrap();
    assert_eq!(count(&conn, "SELECT id FROM t WHERE a = 99"), 12);
}

#[test]
fn batch_is_atomic_on_constraint_error() {
    let (_dir, db) = db();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER NOT NULL)",
        &[],
    )
    .unwrap();
    // La fila 3 viola NOT NULL → no queda NADA del lote.
    let err = conn.bulk_insert(
        "t",
        vec![
            vec![Value::Integer(1), Value::Integer(1)],
            vec![Value::Integer(2), Value::Integer(2)],
            vec![Value::Integer(3), Value::Null],
        ],
    );
    assert!(err.is_err());
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM t"), 0);

    // Rowid explícito duplicado DENTRO del lote → error y nada persistido.
    let err = conn.bulk_insert(
        "t",
        vec![
            vec![Value::Integer(5), Value::Integer(1)],
            vec![Value::Integer(5), Value::Integer(2)],
        ],
    );
    assert!(err.is_err());
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM t"), 0);
}

#[test]
fn indexed_table_entries_and_unique_enforcement() {
    let (_dir, db) = db();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER, u TEXT)",
        &[],
    )
    .unwrap();
    conn.execute("CREATE INDEX ix_k ON t (k)", &[]).unwrap();
    conn.execute("CREATE UNIQUE INDEX ux_u ON t (u)", &[])
        .unwrap();

    conn.bulk_insert(
        "t",
        vec![
            vec![Value::Null, Value::Integer(2), Value::Text("a".into())],
            vec![Value::Null, Value::Integer(1), Value::Text("b".into())],
            vec![Value::Null, Value::Integer(2), Value::Null], // NULL: UNIQUE lo permite
            vec![Value::Null, Value::Integer(3), Value::Null], // segundo NULL también
        ],
    )
    .unwrap();
    // El índice no-único resuelve la consulta (dos filas con k=2).
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM t WHERE k = 2"), 2);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM t WHERE u = 'b'"), 1);

    // Duplicado UNIQUE intra-lote → error, nada del lote queda.
    let before = count(&conn, "SELECT COUNT(*) FROM t");
    let err = conn.bulk_insert(
        "t",
        vec![
            vec![Value::Null, Value::Integer(9), Value::Text("dup".into())],
            vec![Value::Null, Value::Integer(9), Value::Text("dup".into())],
        ],
    );
    assert!(err.is_err());
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM t"), before);

    // Duplicado UNIQUE contra lo ya existente → error, nada del lote queda.
    let err = conn.bulk_insert(
        "t",
        vec![vec![
            Value::Null,
            Value::Integer(9),
            Value::Text("a".into()),
        ]],
    );
    assert!(err.is_err());
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM t"), before);

    // Y un lote válido tras los fallidos sigue funcionando (índices íntegros).
    conn.bulk_insert(
        "t",
        vec![vec![
            Value::Null,
            Value::Integer(9),
            Value::Text("c".into()),
        ]],
    )
    .unwrap();
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM t WHERE k = 9"), 1);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM t WHERE u = 'c'"), 1);
}

#[test]
fn equivalent_to_row_by_row_inserts() {
    let (_dir, db) = db();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE a (id INTEGER PRIMARY KEY, k INTEGER, s TEXT)",
        &[],
    )
    .unwrap();
    conn.execute(
        "CREATE TABLE b (id INTEGER PRIMARY KEY, k INTEGER, s TEXT)",
        &[],
    )
    .unwrap();
    conn.execute("CREATE INDEX ia ON a (k)", &[]).unwrap();
    conn.execute("CREATE INDEX ib ON b (k)", &[]).unwrap();

    let filas: Vec<Vec<Value>> = (1..=200i64)
        .map(|i| {
            vec![
                Value::Null,
                Value::Integer(i % 17),
                Value::Text(format!("s{}", i % 5)),
            ]
        })
        .collect();
    conn.bulk_insert("a", filas.iter().map(|f| f.as_slice()))
        .unwrap();
    let ins = conn.prepare("INSERT INTO b VALUES (?1, ?2, ?3)").unwrap();
    conn.execute("BEGIN", &[]).unwrap();
    for f in &filas {
        ins.execute(&params![f[0].clone(), f[1].clone(), f[2].clone()])
            .unwrap();
    }
    conn.execute("COMMIT", &[]).unwrap();

    // Mismo contenido lógico por ambos caminos, también vía índice.
    for k in 0..17 {
        let qa = format!("SELECT COUNT(*) FROM a WHERE k = {k}");
        let qb = format!("SELECT COUNT(*) FROM b WHERE k = {k}");
        assert_eq!(count(&conn, &qa), count(&conn, &qb), "k={k}");
    }
}

#[test]
fn rejected_inside_begin_and_unknown_table() {
    let (_dir, db) = db();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)", &[])
        .unwrap();
    conn.execute("BEGIN", &[]).unwrap();
    assert!(
        conn.bulk_insert("t", vec![vec![Value::Null, Value::Integer(1)]])
            .is_err()
    );
    conn.execute("ROLLBACK", &[]).unwrap();
    assert!(
        conn.bulk_insert("nope", vec![vec![Value::Integer(1)]])
            .is_err()
    );
    // Lote vacío: 0 filas, sin error.
    assert_eq!(
        conn.bulk_insert("t", std::iter::empty::<Vec<Value>>())
            .unwrap(),
        0
    );
}
