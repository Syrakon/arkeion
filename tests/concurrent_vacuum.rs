//! Vacuum concurrente con lectores. La compactación reescribe la base y **reemplaza
//! el inode atómicamente** (retención CoW); este test la corre mientras varios
//! lectores consultan a la vez y verifica que (a) ningún lector ve datos corruptos,
//! (b) la BD queda íntegra (cadena auditable) tras varias compactaciones. Cruza dos
//! ejes que no se probaban juntos: **concurrencia × retención/vacuum**.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;

use arkeion::{Database, Error, Options, Retention, params};

#[test]
fn readers_stay_correct_while_vacuum_compacts() {
    const ROWS: i64 = 500;
    const COMMITS: i64 = 80;
    const READERS: u64 = 6;

    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("v.arkeion"), Options::default()).unwrap();
    {
        let conn = db.connect().unwrap();
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER NOT NULL)", &[])
            .unwrap();
        conn.execute("BEGIN", &[]).unwrap();
        for id in 1..=ROWS {
            conn.execute("INSERT INTO t (id, v) VALUES (?1, ?2)", &params![id, id])
                .unwrap();
        }
        conn.execute("COMMIT", &[]).unwrap();
        // Historia: muchas commits que mueven 1 unidad entre dos filas ⇒ SUM(v) es
        // INVARIANTE en todo momento, así que cualquier lectura debe ver el mismo total.
        for c in 0..COMMITS {
            let a = (c % ROWS) + 1;
            let b = ((c * 7 + 3) % ROWS) + 1;
            if a == b {
                continue;
            }
            conn.execute("BEGIN", &[]).unwrap();
            conn.execute("UPDATE t SET v = v + 1 WHERE id = ?1", &params![a])
                .unwrap();
            conn.execute("UPDATE t SET v = v - 1 WHERE id = ?1", &params![b])
                .unwrap();
            conn.execute("COMMIT", &[]).unwrap();
        }
    }
    let total: i64 = (1..=ROWS).sum(); // SUM(v) invariante

    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicU64::new(0));
    let mut readers = Vec::new();
    for r in 0..READERS {
        let db = db.clone();
        let stop = Arc::clone(&stop);
        let reads = Arc::clone(&reads);
        readers.push(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                // Conexión fresca cada vuelta: ve el head, que la retención conserva.
                let conn = db.connect().unwrap();
                let sum: i64 = conn
                    .query("SELECT SUM(v) FROM t", &[])
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .get(0)
                    .unwrap();
                assert_eq!(sum, total, "lector {r} vio SUM corrupto durante vacuum");
                let cnt: i64 = conn
                    .query("SELECT COUNT(*) FROM t", &[])
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .get(0)
                    .unwrap();
                assert_eq!(cnt, ROWS, "lector {r} vio COUNT corrupto durante vacuum");
                reads.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    // Varias compactaciones mientras los lectores corren (Busy → reintenta).
    let mut vacuums = 0;
    for _ in 0..6 {
        loop {
            match db.vacuum(Retention::KeepLast(3)) {
                Ok(_) => {
                    vacuums += 1;
                    break;
                }
                Err(Error::Busy) => std::hint::spin_loop(),
                Err(e) => panic!("vacuum falló: {e}"),
            }
        }
    }
    stop.store(true, Ordering::Relaxed);
    for h in readers {
        h.join().expect("un lector entró en pánico");
    }

    assert!(vacuums >= 6, "no se completaron las compactaciones");
    assert!(
        reads.load(Ordering::Relaxed) > 0,
        "los lectores no llegaron a leer"
    );

    // Integridad final: total intacto y cadena auditable. En bloque para que la
    // conexión (su snapshot mantiene vivo el `Store` y su lock de archivo) se suelte
    // antes de reabrir.
    {
        let conn = db.connect().unwrap();
        let sum: i64 = conn
            .query("SELECT SUM(v) FROM t", &[])
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .get(0)
            .unwrap();
        assert_eq!(sum, total, "el total no se conservó tras vacuum concurrente");
    }
    assert!(
        db.verify().unwrap().chain_ok,
        "la cadena quedó rota tras vacuum concurrente"
    );
    drop(db);

    let db = Database::open(dir.path().join("v.arkeion"), Options::default()).unwrap();
    let conn = db.connect().unwrap();
    let sum: i64 = conn
        .query("SELECT SUM(v) FROM t", &[])
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .get(0)
        .unwrap();
    assert_eq!(sum, total, "el total no sobrevivió al reabrir");
}
