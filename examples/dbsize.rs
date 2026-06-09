//! Mide el tamaño en disco de un dataset en arkeion (con y sin historia, y tras
//! vacuum) frente a SQLite. `cargo run --release --example dbsize --features bench-sqlite`.

use arkeion::{Database, Options, Retention, params};

fn mb(path: &std::path::Path) -> f64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) as f64 / 1_000_000.0
}

fn main() {
    let n: i64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(100_000);
    println!("Dataset: {n} filas de (id INTEGER PK, a INTEGER, b TEXT='fila de ejemplo')\n");

    // --- arkeion ---
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.arkeion");
    let db = Database::open(&path, Options::default()).unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)",
        &[],
    )
    .unwrap();

    let ins = conn
        .prepare("INSERT INTO t (a, b) VALUES (?1, ?2)")
        .unwrap();
    conn.execute("BEGIN", &[]).unwrap();
    for i in 0..n {
        ins.execute(&params![i, "fila de ejemplo"]).unwrap();
    }
    conn.execute("COMMIT", &[]).unwrap();
    println!(
        "arkeion  insert (1 commit)            : {:.1} MB",
        mb(&path)
    );

    // Mismo dataset con compresión de página (M10, Slice B): mismo motor, mismas
    // propiedades (verify/AS OF), backend LZSS pure-Rust off por defecto.
    {
        let dirc = tempfile::tempdir().unwrap();
        let pathc = dirc.path().join("c.arkeion");
        let dbc = Database::open(&pathc, Options::default().compress(true)).unwrap();
        let connc = dbc.connect().unwrap();
        connc
            .execute(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)",
                &[],
            )
            .unwrap();
        let insc = connc
            .prepare("INSERT INTO t (a, b) VALUES (?1, ?2)")
            .unwrap();
        connc.execute("BEGIN", &[]).unwrap();
        for i in 0..n {
            insc.execute(&params![i, "fila de ejemplo"]).unwrap();
        }
        connc.execute("COMMIT", &[]).unwrap();
        println!(
            "arkeion  insert comprimido (1 commit) : {:.1} MB",
            mb(&pathc)
        );
    }

    // Actualizar TODAS las filas una vez: arkeion conserva la versión vieja (CoW).
    let upd = conn
        .prepare("UPDATE t SET a = a + 1 WHERE id = ?1")
        .unwrap();
    conn.execute("BEGIN", &[]).unwrap();
    for i in 1..=n {
        upd.execute(&params![i]).unwrap();
    }
    conn.execute("COMMIT", &[]).unwrap();
    println!(
        "arkeion  +1 update de todo (historia) : {:.1} MB",
        mb(&path)
    );

    db.vacuum(Retention::KeepLast(1)).unwrap();
    println!(
        "arkeion  tras vacuum KeepLast(1)      : {:.1} MB",
        mb(&path)
    );

    // Insert fila-a-fila (un commit por fila): el peor caso de amplificación.
    let dir2 = tempfile::tempdir().unwrap();
    let path2 = dir2.path().join("b.arkeion");
    let db2 = Database::open(&path2, Options::default()).unwrap();
    let conn2 = db2.connect().unwrap();
    conn2
        .execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)",
            &[],
        )
        .unwrap();
    let small = (n / 10).min(20_000);
    let ins2 = conn2
        .prepare("INSERT INTO t (a, b) VALUES (?1, ?2)")
        .unwrap();
    for i in 0..small {
        ins2.execute(&params![i, "fila de ejemplo"]).unwrap();
    }
    println!(
        "arkeion  {small} filas, 1 commit POR fila    : {:.1} MB  ({:.0} bytes/fila)",
        mb(&path2),
        mb(&path2) * 1_000_000.0 / small as f64
    );

    #[cfg(feature = "bench-sqlite")]
    {
        use rusqlite::Connection;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.sqlite");
        let mut conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)")
            .unwrap();
        {
            let tx = conn.transaction().unwrap();
            {
                let mut ins = tx.prepare("INSERT INTO t (a, b) VALUES (?1, ?2)").unwrap();
                for i in 0..n {
                    ins.execute(rusqlite::params![i, "fila de ejemplo"])
                        .unwrap();
                }
            }
            tx.commit().unwrap();
        }
        println!(
            "\nsqlite   insert (1 commit)            : {:.1} MB",
            mb(&path)
        );
        {
            let tx = conn.transaction().unwrap();
            {
                let mut upd = tx.prepare("UPDATE t SET a = a + 1 WHERE id = ?1").unwrap();
                for i in 1..=n {
                    upd.execute(rusqlite::params![i]).unwrap();
                }
            }
            tx.commit().unwrap();
        }
        println!(
            "sqlite   +1 update de todo (in-place) : {:.1} MB",
            mb(&path)
        );
    }
}
