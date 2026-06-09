//! Caché de páginas configurable vía `Options::cache_bytes` (como `PRAGMA
//! cache_size` de SQLite). La caché es solo rendimiento sobre páginas inmutables:
//! con un tope diminuto la corrección debe ser idéntica (la eviction CLOCK relee
//! del disco), y un valor minúsculo no debe romper la apertura.

use arkeion::{Database, Options, Value};

fn val(conn: &arkeion::Connection, sql: &str) -> Value {
    conn.query(sql, &[])
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .get::<Value>(0)
        .unwrap()
}

#[test]
fn cache_bytes_default_and_builder() {
    // Por defecto 64 MiB; el builder fija el campo.
    assert_eq!(Options::default().cache_bytes, 64 * 1024 * 1024);
    assert_eq!(
        Options::default().cache_bytes(8 * 1024 * 1024).cache_bytes,
        8 * 1024 * 1024
    );
}

#[test]
fn tiny_cache_opens_and_reads() {
    // 1 byte → se acota a ≥ 1 página; debe abrir y operar sin romperse.
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("tiny.arkeion"),
        Options::default().cache_bytes(1),
    )
    .unwrap();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)", &[])
        .unwrap();
    conn.execute("INSERT INTO t (v) VALUES (42)", &[]).unwrap();
    assert_eq!(
        val(&conn, "SELECT v FROM t WHERE id = 1"),
        Value::Integer(42)
    );
}

#[test]
fn small_cache_is_correct_under_pressure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("press.arkeion");
    // 64 KiB ≈ 16 páginas, muy por debajo del working set de abajo: fuerza
    // eviction continua. La corrección no debe depender del tamaño de caché.
    let small = || Options::default().cache_bytes(64 * 1024);

    let n: i64 = 5000;
    {
        let db = Database::open(&path, small()).unwrap();
        let conn = db.connect().unwrap();
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)", &[])
            .unwrap();
        // Un único commit: todas las hojas del árbol exceden de largo la caché.
        let tx = conn.begin().unwrap();
        for i in 0..n {
            tx.execute(
                &format!("INSERT INTO t (id, v) VALUES ({i}, {})", i * 2),
                &[],
            )
            .unwrap();
        }
        tx.commit().unwrap();

        // Lecturas dispersas: cada una probablemente toca una hoja ya evictada.
        for id in [0_i64, 1, 2500, n - 1] {
            assert_eq!(
                val(&conn, &format!("SELECT v FROM t WHERE id = {id}")),
                Value::Integer(id * 2)
            );
        }
        // Un full scan (recorre todas las hojas) cuadra la suma.
        let sum: i64 = (0..n).map(|i| i * 2).sum();
        assert_eq!(val(&conn, "SELECT SUM(v) FROM t"), Value::Integer(sum));
    }

    // Reabrir con la misma caché diminuta sigue siendo correcto y completo.
    let db = Database::open(&path, small()).unwrap();
    let conn = db.connect().unwrap();
    assert_eq!(val(&conn, "SELECT COUNT(*) FROM t"), Value::Integer(n));
    assert_eq!(
        val(&conn, "SELECT v FROM t WHERE id = 4321"),
        Value::Integer(8642)
    );
}
