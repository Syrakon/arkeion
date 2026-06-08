//! Harness de benchmarks CRUD monohilo — arkeion vs SQLite (M9).
//!
//! Criterio honesto del hito: *mismo orden de magnitud que SQLite en CRUD
//! monohilo*. Mide con `std::time` (sin criterion, en el espíritu D8: lo demás
//! va a mano) sobre archivos **reales en disco** (el `fsync` del commit cuenta).
//! Ambos motores ejecutan la **misma** secuencia con **sentencias preparadas**,
//! de modo que se compara el motor de almacenamiento, no el parser.
//!
//! ```text
//! cargo bench --bench crud                       # solo arkeion
//! cargo bench --bench crud --features bench-sqlite   # + comparación con SQLite
//! cargo bench --bench crud -- 2000 50000 20      # durable_n bulk_n scan_reps
//! ```
//!
//! Durabilidad comparable: arkeion hace `fsync` por commit (M1); SQLite se
//! configura con `synchronous=FULL` + journal por defecto (también durable por
//! commit). WAL + `synchronous=NORMAL` sería más rápido en SQLite: no es el
//! punto de comparación elegido (queremos durabilidad equivalente).

use std::time::{Duration, Instant};

use arkeion::{Database, Options, params};

const CREATE: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER NOT NULL)";

/// Operaciones por segundo a partir de un recuento y un tiempo.
fn ops(n: i64, elapsed: Duration) -> f64 {
    n as f64 / elapsed.as_secs_f64()
}

/// Permutación barata id ∈ [1, n] para tocar filas en orden no secuencial (sin
/// dependencia de RNG): rompe la localidad de caché en los point lookups.
fn scattered(k: i64, n: i64) -> i64 {
    k.wrapping_mul(2_654_435_761).rem_euclid(n) + 1
}

fn human(v: f64) -> String {
    if v >= 1e6 {
        format!("{:.2}M", v / 1e6)
    } else if v >= 1e3 {
        format!("{:.1}k", v / 1e3)
    } else {
        format!("{v:.0}")
    }
}

// --- arkeion ---

fn run_arkeion(durable_n: i64, bulk_n: i64, scan_reps: i64) -> Vec<(&'static str, f64)> {
    let mut out = Vec::new();

    // 1) INSERT durable: una fila por commit (camino con fsync por operación).
    {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("a.arkeion"), Options::default()).unwrap();
        let conn = db.connect().unwrap();
        conn.execute(CREATE, &[]).unwrap();
        let ins = conn
            .prepare("INSERT INTO t (id, n) VALUES (?1, ?2)")
            .unwrap();
        let t = Instant::now();
        for i in 1..=durable_n {
            ins.execute(&params![i, i * 2]).unwrap();
        }
        out.push((
            "insert 1 fila/commit (durable)",
            ops(durable_n, t.elapsed()),
        ));
    }

    // 2..6) un solo archivo con `bulk_n` filas para lecturas/updates/deletes.
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("b.arkeion"), Options::default()).unwrap();
    let conn = db.connect().unwrap();
    conn.execute(CREATE, &[]).unwrap();

    // 2) INSERT por lote: todas las filas en un único commit (fsync amortizado).
    let ins = conn
        .prepare("INSERT INTO t (id, n) VALUES (?1, ?2)")
        .unwrap();
    let t = Instant::now();
    conn.execute("BEGIN", &[]).unwrap();
    for i in 1..=bulk_n {
        ins.execute(&params![i, i * 2]).unwrap();
    }
    conn.execute("COMMIT", &[]).unwrap();
    out.push(("insert lote (1 commit)", ops(bulk_n, t.elapsed())));

    // 3) SELECT por clave primaria (point lookup).
    let sel = conn.prepare("SELECT n FROM t WHERE id = ?1").unwrap();
    let t = Instant::now();
    let mut checksum = 0i64;
    for k in 0..bulk_n {
        let id = scattered(k, bulk_n);
        let row = sel.query(&params![id]).unwrap().next().unwrap().unwrap();
        checksum ^= row.get::<i64>("n").unwrap();
    }
    std::hint::black_box(checksum);
    out.push(("select por PK", ops(bulk_n, t.elapsed())));

    // 4) UPDATE durable por PK (un commit por fila).
    let upd = conn.prepare("UPDATE t SET n = ?2 WHERE id = ?1").unwrap();
    let t = Instant::now();
    for i in 1..=durable_n {
        upd.execute(&params![i, i * 3]).unwrap();
    }
    out.push(("update por PK (durable)", ops(durable_n, t.elapsed())));

    // 5) Full scan repetido (filas/s).
    let scan = conn.prepare("SELECT n FROM t").unwrap();
    let t = Instant::now();
    let mut sum = 0i64;
    for _ in 0..scan_reps {
        for row in scan.query(&[]).unwrap() {
            sum = sum.wrapping_add(row.unwrap().get::<i64>("n").unwrap());
        }
    }
    std::hint::black_box(sum);
    out.push(("full scan (filas/s)", ops(bulk_n * scan_reps, t.elapsed())));

    // 6) DELETE durable por PK (un commit por fila).
    let del = conn.prepare("DELETE FROM t WHERE id = ?1").unwrap();
    let t = Instant::now();
    for i in 1..=durable_n {
        del.execute(&params![i]).unwrap();
    }
    out.push(("delete por PK (durable)", ops(durable_n, t.elapsed())));

    out
}

// --- SQLite (rusqlite, bundled), tras la feature `bench-sqlite` ---

#[cfg(feature = "bench-sqlite")]
fn run_sqlite(durable_n: i64, bulk_n: i64, scan_reps: i64) -> Vec<(&'static str, f64)> {
    use rusqlite::Connection;

    // Durabilidad equivalente a arkeion: synchronous=FULL. Journal de rollback
    // por defecto; con ARKEION_BENCH_SQLITE_WAL=1, modo WAL (1 fsync/commit).
    fn open(path: std::path::PathBuf) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.pragma_update(None, "synchronous", "FULL").unwrap();
        if std::env::var("ARKEION_BENCH_SQLITE_WAL").is_ok() {
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        }
        conn.execute_batch(CREATE).unwrap();
        conn
    }

    let mut out = Vec::new();

    // 1) INSERT durable.
    {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(dir.path().join("a.sqlite"));
        let mut ins = conn
            .prepare("INSERT INTO t (id, n) VALUES (?1, ?2)")
            .unwrap();
        let t = Instant::now();
        for i in 1..=durable_n {
            ins.execute(rusqlite::params![i, i * 2]).unwrap();
        }
        out.push((
            "insert 1 fila/commit (durable)",
            ops(durable_n, t.elapsed()),
        ));
    }

    let dir = tempfile::tempdir().unwrap();
    let mut conn = open(dir.path().join("b.sqlite"));

    // 2) INSERT por lote en una transacción.
    let t = Instant::now();
    {
        let tx = conn.transaction().unwrap();
        {
            let mut ins = tx.prepare("INSERT INTO t (id, n) VALUES (?1, ?2)").unwrap();
            for i in 1..=bulk_n {
                ins.execute(rusqlite::params![i, i * 2]).unwrap();
            }
        }
        tx.commit().unwrap();
    }
    out.push(("insert lote (1 commit)", ops(bulk_n, t.elapsed())));

    // 3) SELECT por PK.
    let mut sel = conn.prepare("SELECT n FROM t WHERE id = ?1").unwrap();
    let t = Instant::now();
    let mut checksum = 0i64;
    for k in 0..bulk_n {
        let id = scattered(k, bulk_n);
        let n: i64 = sel.query_row(rusqlite::params![id], |r| r.get(0)).unwrap();
        checksum ^= n;
    }
    std::hint::black_box(checksum);
    out.push(("select por PK", ops(bulk_n, t.elapsed())));
    drop(sel);

    // 4) UPDATE durable.
    let mut upd = conn.prepare("UPDATE t SET n = ?2 WHERE id = ?1").unwrap();
    let t = Instant::now();
    for i in 1..=durable_n {
        upd.execute(rusqlite::params![i, i * 3]).unwrap();
    }
    out.push(("update por PK (durable)", ops(durable_n, t.elapsed())));
    drop(upd);

    // 5) Full scan.
    let mut scan = conn.prepare("SELECT n FROM t").unwrap();
    let t = Instant::now();
    let mut sum = 0i64;
    for _ in 0..scan_reps {
        let mut rows = scan.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            sum = sum.wrapping_add(row.get::<_, i64>(0).unwrap());
        }
    }
    std::hint::black_box(sum);
    out.push(("full scan (filas/s)", ops(bulk_n * scan_reps, t.elapsed())));
    drop(scan);

    // 6) DELETE durable.
    let mut del = conn.prepare("DELETE FROM t WHERE id = ?1").unwrap();
    let t = Instant::now();
    for i in 1..=durable_n {
        del.execute(rusqlite::params![i]).unwrap();
    }
    out.push(("delete por PK (durable)", ops(durable_n, t.elapsed())));

    out
}

#[cfg(not(feature = "bench-sqlite"))]
fn run_sqlite(_: i64, _: i64, _: i64) -> Vec<(&'static str, f64)> {
    Vec::new()
}

/// Comparación **justa**: SQLite obligado a dar las mismas garantías que arkeion
/// —historia completa (tabla `t_log`) + cadena de hash tamper-evident por
/// escritura—. Cada insert lógico = (fila en `t`) + (entrada encadenada en
/// `t_log`), en **una** transacción (1 fsync, como el commit de arkeion).
/// Devuelve (durable ops/s, lote ops/s).
#[cfg(feature = "bench-sqlite")]
fn run_sqlite_audited(durable_n: i64, bulk_n: i64) -> (f64, f64) {
    use rusqlite::Connection;
    use sha2::{Digest, Sha256};

    let wal = std::env::var("ARKEION_BENCH_SQLITE_WAL").is_ok();
    let setup = |path: std::path::PathBuf| -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.pragma_update(None, "synchronous", "FULL").unwrap();
        if wal {
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        }
        conn.execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER NOT NULL);\
             CREATE TABLE t_log (seq INTEGER PRIMARY KEY, rid INTEGER, n INTEGER, hash BLOB NOT NULL);",
        )
        .unwrap();
        conn
    };
    let chain = |prev: &[u8; 32], rid: i64, n: i64| -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(prev);
        h.update(rid.to_le_bytes());
        h.update(n.to_le_bytes());
        h.finalize().into()
    };

    // Durable: cada insert lógico en su propia transacción ⇒ 1 fsync, como arkeion.
    let durable = {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = setup(dir.path().join("ad.sqlite"));
        let mut prev = [0u8; 32];
        let t = Instant::now();
        for i in 1..=durable_n {
            let n = i * 2;
            prev = chain(&prev, i, n);
            let tx = conn.transaction().unwrap();
            tx.execute(
                "INSERT INTO t (id, n) VALUES (?1, ?2)",
                rusqlite::params![i, n],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO t_log (rid, n, hash) VALUES (?1, ?2, ?3)",
                rusqlite::params![i, n, &prev[..]],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        ops(durable_n, t.elapsed())
    };

    // Lote: todo en una transacción.
    let batch = {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = setup(dir.path().join("ab.sqlite"));
        let mut prev = [0u8; 32];
        let t = Instant::now();
        let tx = conn.transaction().unwrap();
        {
            let mut ins_t = tx.prepare("INSERT INTO t (id, n) VALUES (?1, ?2)").unwrap();
            let mut ins_l = tx
                .prepare("INSERT INTO t_log (rid, n, hash) VALUES (?1, ?2, ?3)")
                .unwrap();
            for i in 1..=bulk_n {
                let n = i * 2;
                prev = chain(&prev, i, n);
                ins_t.execute(rusqlite::params![i, n]).unwrap();
                ins_l.execute(rusqlite::params![i, n, &prev[..]]).unwrap();
            }
        }
        tx.commit().unwrap();
        ops(bulk_n, t.elapsed())
    };
    (durable, batch)
}

fn main() {
    // Args posicionales numéricos: durable_n bulk_n scan_reps.
    let nums: Vec<i64> = std::env::args()
        .skip(1)
        .filter_map(|a| a.parse().ok())
        .collect();
    let durable_n = nums.first().copied().unwrap_or(2_000);
    let bulk_n = nums.get(1).copied().unwrap_or(50_000);
    let scan_reps = nums.get(2).copied().unwrap_or(20);

    let with_sqlite = cfg!(feature = "bench-sqlite");
    println!("Arkeion — benchmark CRUD monohilo (M9)");
    println!(
        "  durable_n={durable_n}  bulk_n={bulk_n}  scan_reps={scan_reps}  tmp={}",
        std::env::temp_dir().display()
    );
    println!(
        "  durabilidad: fsync por commit en ambos (SQLite synchronous=FULL); SQLite={}",
        if with_sqlite {
            "ON"
        } else {
            "OFF (--features bench-sqlite)"
        }
    );
    println!("  calentando…");
    // Calentamiento: una pasada corta descartada (estabiliza caché de páginas/FS).
    let _ = run_arkeion(200, 2_000, 2);

    let ark = run_arkeion(durable_n, bulk_n, scan_reps);
    let sql = if with_sqlite {
        Some(run_sqlite(durable_n, bulk_n, scan_reps))
    } else {
        None
    };

    println!();
    println!(
        "{:<32} {:>12} {:>12} {:>9}",
        "operación", "arkeion", "sqlite", "ratio"
    );
    println!("{}", "-".repeat(68));
    for (i, (op, a)) in ark.iter().enumerate() {
        match &sql {
            Some(s) => {
                let b = s[i].1;
                println!(
                    "{:<32} {:>12} {:>12} {:>8.2}x",
                    op,
                    human(*a),
                    human(b),
                    a / b
                );
            }
            None => println!("{:<32} {:>12} {:>12} {:>9}", op, human(*a), "-", "-"),
        }
    }
    println!();
    println!("ratio = arkeion / sqlite  (>1 ⇒ arkeion más rápido en esa operación)");

    // Comparación justa: SQLite con las garantías de arkeion (historia + hash chain).
    #[cfg(feature = "bench-sqlite")]
    {
        let (ad, ab) = run_sqlite_audited(durable_n, bulk_n);
        let ark_durable = ark[0].1; // "insert 1 fila/commit (durable)"
        let ark_batch = ark[1].1; // "insert lote (1 commit)"
        println!();
        println!(
            "— comparación JUSTA: SQLite con historia + cadena de hash (= garantías de arkeion) —"
        );
        println!(
            "{:<24} {:>12} {:>14} {:>9}",
            "insert", "arkeion", "sqlite+audit", "ratio"
        );
        println!("{}", "-".repeat(62));
        println!(
            "{:<24} {:>12} {:>14} {:>8.2}x",
            "durable (1/commit)",
            human(ark_durable),
            human(ad),
            ark_durable / ad
        );
        println!(
            "{:<24} {:>12} {:>14} {:>8.2}x",
            "lote (1 commit)",
            human(ark_batch),
            human(ab),
            ark_batch / ab
        );
    }
}
