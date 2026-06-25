//! Lecturas concurrentes a través del **page cache shardeado** (`ShardedCache`,
//! `perf/shard-page-cache`). N hilos leen a la vez contra un cache **diminuto** que
//! fuerza eviction cruzando shards en casi cada lectura; cada resultado debe ser
//! correcto. El `Mutex<PageCache>` único anterior serializaba toda lectura (incluido
//! el hit); el cache shardeado reparte el lock por id de página. Estos tests no miden
//! velocidad (eso es `benches/vec_concurrent.rs`) — verifican **corrección** bajo
//! contención: ninguna carrera puede devolver un valor equivocado, panic ni colgar.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use arkeion::{Database, Error, Key, Options, params};

/// Abre una BD con cache **minúsculo** (64 KiB ≈ 16 páginas): con los shards por
/// defecto (~2× núcleos) `nshards` se recorta a la capacidad y la eviction CLOCK
/// cruza shards constantemente — el caso que estresa el sharding.
fn open_tiny_cache(path: &std::path::Path) -> Database {
    Database::open(path, Options::default().cache_bytes(64 * 1024)).unwrap()
}

/// Igual pero **cifrada** (AES-256-GCM): cada miss del cache re-descifra la página
/// del disco, así que la lectura concurrente con eviction ejercita el descifrado por
/// shard bajo contención.
fn open_tiny_encrypted(path: &std::path::Path) -> Database {
    Database::open(
        path,
        Options::default().cache_bytes(64 * 1024).key(Key::new([0x5A; 32])),
    )
    .unwrap()
}

/// RNG xorshift sembrado (determinista por hilo), como el resto de tests del repo.
fn rng(seed: u64) -> impl FnMut() -> u64 {
    let mut s = seed | 1;
    move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    }
}

/// Función conocida id → valor; cualquier corrupción del cache la delata.
fn val_of(id: i64) -> i64 {
    id.wrapping_mul(2_654_435_761).wrapping_add(7)
}

fn seed_rows(db: &Database, n: i64) {
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER NOT NULL)", &[])
        .unwrap();
    // Inserción masiva en una transacción para que quede en disco y exceda el cache.
    conn.execute("BEGIN", &[]).unwrap();
    for id in 1..=n {
        conn.execute("INSERT INTO t (id, v) VALUES (?1, ?2)", &params![id, val_of(id)])
            .unwrap();
    }
    conn.execute("COMMIT", &[]).unwrap();
}

/// N hilos hacen búsquedas puntuales aleatorias a la vez; con el cache diminuto cada
/// una evicta/recarga páginas de varios shards. Todo valor leído debe ser el correcto.
#[test]
fn concurrent_point_reads_are_correct_under_eviction() {
    const N: i64 = 4000;
    const THREADS: u64 = 16;
    const ITERS: u64 = 4000;

    let dir = tempfile::tempdir().unwrap();
    let db = open_tiny_cache(&dir.path().join("c.arkeion"));
    seed_rows(&db, N);

    let mut handles = Vec::new();
    for t in 0..THREADS {
        let db = db.clone();
        handles.push(thread::spawn(move || {
            let conn = db.connect().unwrap();
            let mut next = rng(0xA53F_0001 ^ (t.wrapping_mul(0x9E37_79B9)));
            for _ in 0..ITERS {
                let id = (next() % N as u64) as i64 + 1;
                let got: i64 = conn
                    .query("SELECT v FROM t WHERE id = ?1", &params![id])
                    .unwrap()
                    .next()
                    .unwrap_or_else(|| panic!("id {id} no encontrado"))
                    .unwrap()
                    .get(0)
                    .unwrap();
                assert_eq!(got, val_of(id), "valor corrupto para id {id} (hilo {t})");
            }
        }));
    }
    for h in handles {
        h.join().expect("un hilo lector entró en pánico");
    }
}

/// Como el anterior pero sobre una BD **cifrada**: valida que el cache shardeado +
/// el descifrado AES-GCM bajo contención sirven siempre la página correcta (un fallo
/// de integridad o un descifrado con nonce/clave equivocados rompería el valor).
#[test]
fn concurrent_point_reads_correct_on_encrypted_db() {
    const N: i64 = 3000;
    const THREADS: u64 = 12;
    const ITERS: u64 = 3000;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("enc.arkeion");
    let db = open_tiny_encrypted(&path);
    seed_rows(&db, N);

    let mut handles = Vec::new();
    for t in 0..THREADS {
        let db = db.clone();
        handles.push(thread::spawn(move || {
            let conn = db.connect().unwrap();
            let mut next = rng(0x5EC0_0001 ^ (t.wrapping_mul(0x9E37_79B9)));
            for _ in 0..ITERS {
                let id = (next() % N as u64) as i64 + 1;
                let got: i64 = conn
                    .query("SELECT v FROM t WHERE id = ?1", &params![id])
                    .unwrap()
                    .next()
                    .unwrap_or_else(|| panic!("id {id} no encontrado (cifrada)"))
                    .unwrap()
                    .get(0)
                    .unwrap();
                assert_eq!(got, val_of(id), "valor corrupto cifrado para id {id} (hilo {t})");
            }
        }));
    }
    for h in handles {
        h.join().expect("un hilo lector (cifrada) entró en pánico");
    }
}

/// Escaneos completos concurrentes: cada hilo recorre toda la tabla (agregados), lo
/// que machaca el cache de extremo a extremo en todos los shards. COUNT y SUM deben
/// ser siempre exactos — una página servida del shard equivocado rompería el total.
#[test]
fn concurrent_full_scans_agree_under_eviction() {
    const N: i64 = 3000;
    const THREADS: u64 = 12;
    const ITERS: u64 = 40;

    let dir = tempfile::tempdir().unwrap();
    let db = open_tiny_cache(&dir.path().join("scan.arkeion"));
    seed_rows(&db, N);
    let expected_sum: i64 = (1..=N).map(val_of).fold(0i64, |a, b| a.wrapping_add(b));

    let mut handles = Vec::new();
    for _ in 0..THREADS {
        let db = db.clone();
        handles.push(thread::spawn(move || {
            let conn = db.connect().unwrap();
            for _ in 0..ITERS {
                let count: i64 = conn
                    .query("SELECT COUNT(*) FROM t", &[])
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .get(0)
                    .unwrap();
                assert_eq!(count, N, "COUNT inconsistente bajo lectura concurrente");
                let sum: i64 = conn
                    .query("SELECT SUM(v) FROM t", &[])
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .get(0)
                    .unwrap();
                assert_eq!(sum, expected_sum, "SUM corrupto bajo lectura concurrente");
            }
        }));
    }
    for h in handles {
        h.join().expect("un hilo de escaneo entró en pánico");
    }
}

/// Snapshot isolation bajo el cache shardeado: un escritor transfiere saldo entre
/// cuentas en transacciones (el total es invariante); muchos lectores concurrentes
/// deben ver SIEMPRE el total intacto — nunca una transferencia a medias. Valida que
/// shardear el cache no rompe el aislamiento por snapshot.
#[test]
fn readers_never_see_torn_transfer_while_writer_runs() {
    const ACCOUNTS: i64 = 200;
    const START: i64 = 1000;
    const TRANSFERS: u64 = 1500;
    const READERS: u64 = 8;
    let total = ACCOUNTS * START;

    let dir = tempfile::tempdir().unwrap();
    let db = open_tiny_cache(&dir.path().join("bank.arkeion"));
    {
        let conn = db.connect().unwrap();
        conn.execute("CREATE TABLE acct (id INTEGER PRIMARY KEY, bal INTEGER NOT NULL)", &[])
            .unwrap();
        conn.execute("BEGIN", &[]).unwrap();
        for id in 1..=ACCOUNTS {
            conn.execute("INSERT INTO acct (id, bal) VALUES (?1, ?2)", &params![id, START])
                .unwrap();
        }
        conn.execute("COMMIT", &[]).unwrap();
    }

    let stop = Arc::new(AtomicU64::new(0));

    // Lectores: el total de saldos debe ser SIEMPRE `total`, nunca intermedio.
    let mut handles = Vec::new();
    for r in 0..READERS {
        let db = db.clone();
        let stop = Arc::clone(&stop);
        handles.push(thread::spawn(move || {
            let conn = db.connect().unwrap();
            let mut reads = 0u64;
            while stop.load(Ordering::Relaxed) == 0 {
                let sum: i64 = conn
                    .query("SELECT SUM(bal) FROM acct", &[])
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .get(0)
                    .unwrap();
                assert_eq!(sum, total, "lector {r} vio una transferencia a medias");
                reads += 1;
            }
            reads
        }));
    }

    // Escritor único: mueve 1 unidad entre dos cuentas por transacción.
    let writer = {
        let db = db.clone();
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let conn = db.connect().unwrap();
            let mut next = rng(0xBEEF_1234);
            let mut done = 0u64;
            while done < TRANSFERS {
                let a = (next() % ACCOUNTS as u64) as i64 + 1;
                let b = (next() % ACCOUNTS as u64) as i64 + 1;
                if a == b {
                    continue;
                }
                // Transacción atómica: el snapshot de un lector la ve entera o nada.
                let txn = || -> Result<(), Error> {
                    conn.execute("BEGIN", &[])?;
                    conn.execute("UPDATE acct SET bal = bal - 1 WHERE id = ?1", &params![a])?;
                    conn.execute("UPDATE acct SET bal = bal + 1 WHERE id = ?1", &params![b])?;
                    conn.execute("COMMIT", &[])?;
                    Ok(())
                };
                loop {
                    match txn() {
                        Ok(()) => break,
                        Err(Error::Busy) => {
                            let _ = conn.execute("ROLLBACK", &[]);
                            std::hint::spin_loop();
                        }
                        Err(e) => panic!("transferencia falló: {e}"),
                    }
                }
                done += 1;
            }
            stop.store(1, Ordering::Relaxed);
        })
    };

    writer.join().expect("el escritor entró en pánico");
    for h in handles {
        h.join().expect("un lector entró en pánico");
    }

    // Comprobación final: el total se conserva tras todas las transferencias.
    let conn = db.connect().unwrap();
    let sum: i64 = conn
        .query("SELECT SUM(bal) FROM acct", &[])
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .get(0)
        .unwrap();
    assert_eq!(sum, total, "el total no se conservó al final");
}
