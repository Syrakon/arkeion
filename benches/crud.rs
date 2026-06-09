//! Harness de benchmarks CRUD monohilo — arkeion vs SQLite.
//!
//! Mide con `std::time` (sin criterion, espíritu D8: lo demás va a mano) sobre
//! archivos reales. Ambos motores ejecutan la **misma** secuencia con
//! **sentencias preparadas** (`prepare` fuera del bucle, `execute` dentro): se
//! compara el motor de almacenamiento/ejecución, no el parser.
//!
//! ## Metodología (lee esto antes de citar un número)
//!
//! - **Mediana de N**, no una sola corrida. Lecturas y scan: mediana **in situ**
//!   (`READ_REPS` / `scan_reps`). Pasos **durables** (insert/update/delete con
//!   fsync por commit): **DB fresca por repetición** (`DURABLE_REPS`), sembrando
//!   **fuera del cronómetro** — re-insertar PKs viola UNIQUE y re-actualizar
//!   inflaría la historia CoW de este motor versionado y sesgaría el tiempo.
//! - **Calentamiento simétrico**: ambos motores reciben una pasada descartada.
//! - **Disco**: por defecto `tempfile::tempdir()` (TMPDIR/`/tmp`). **Si `/tmp` es
//!   `tmpfs` (RAM) los fsync NO tocan disco** y los números durables no demuestran
//!   durabilidad real. Exporta `ARKEION_BENCH_DIR=/ruta/en/disco/real` para medir
//!   en disco. El programa imprime dónde escribe.
//! - **Durabilidad**: arkeion hace **1** `fdatasync`/commit (append-only); SQLite
//!   se mide con `synchronous=FULL` + journal de rollback por defecto (**2**
//!   fsync/commit). `ARKEION_BENCH_SQLITE_WAL=1` ⇒ WAL (1 sync) — más rápido para
//!   SQLite, cambia mucho los ratios.
//! - **Sensible al tamaño**: con `bulk_n` pequeño el conjunto cabe en la caché de
//!   páginas de arkeion (16 MB) y las lecturas vuelan; al superarla, la ventaja se
//!   estrecha o se invierte. Barre tamaños: `-- 2000 50000 20`, `-- 2000 1000000 5`.
//!
//! ```text
//! cargo bench --bench crud --features bench-sqlite                 # vs SQLite
//! cargo bench --bench crud --features bench-sqlite -- 2000 50000 20  # durable_n bulk_n scan_reps
//! ARKEION_BENCH_DIR=/mnt/datos/tmp cargo bench --bench crud --features bench-sqlite  # fsync a disco real
//! ```

use std::time::{Duration, Instant};

use arkeion::{Database, Options, params};

const CREATE: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER NOT NULL)";

/// Repeticiones por operación para la mediana (lecturas baratas y puras vs pasos
/// durables que reconstruyen una DB por repetición — más caros, menos reps).
const READ_REPS: usize = 5;
const DURABLE_REPS: usize = 3;

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

/// Mediana de una muestra (resiste outliers de calentamiento mejor que la media
/// o el mejor caso, que es justo el número que ataca un escéptico).
fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).expect("sin NaN en tiempos"));
    let n = v.len();
    if n == 0 {
        0.0
    } else if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// Mediana de `reps` ejecuciones de `f` (cada una devuelve ops/s ya cronometrado).
fn median_of(reps: usize, mut f: impl FnMut() -> f64) -> f64 {
    median((0..reps.max(1)).map(|_| f()).collect())
}

/// Directorio temporal del bench: en **disco real** si `ARKEION_BENCH_DIR` está
/// puesto (recomendado para medir durabilidad de verdad), si no el tempdir por
/// defecto (que puede ser `tmpfs`/RAM y no tocar disco en los fsync).
fn bench_dir() -> tempfile::TempDir {
    match std::env::var("ARKEION_BENCH_DIR") {
        Ok(d) => tempfile::Builder::new()
            .prefix("ark-bench-")
            .tempdir_in(d)
            .unwrap(),
        Err(_) => tempfile::tempdir().unwrap(),
    }
}

// --- arkeion ---

fn run_arkeion(durable_n: i64, bulk_n: i64, scan_reps: i64) -> Vec<(&'static str, f64)> {
    let mut out = Vec::new();

    // 1) INSERT durable (1 commit/fila, fsync por operación): DB fresca por rep.
    out.push((
        "insert 1 fila/commit (durable)",
        median_of(DURABLE_REPS, || {
            let dir = bench_dir();
            let db = Database::open(dir.path().join("a.arkeion"), Options::default()).unwrap();
            let conn = db.connect().unwrap();
            conn.execute(CREATE, &[]).unwrap();
            let ins = conn.prepare("INSERT INTO t (id, n) VALUES (?1, ?2)").unwrap();
            let t = Instant::now();
            for i in 1..=durable_n {
                ins.execute(&params![i, i * 2]).unwrap();
            }
            ops(durable_n, t.elapsed())
        }),
    ));

    // 2) INSERT por lote (un único commit, fsync amortizado): DB fresca por rep.
    out.push((
        "insert lote (1 commit)",
        median_of(DURABLE_REPS, || {
            let dir = bench_dir();
            let db = Database::open(dir.path().join("b.arkeion"), Options::default()).unwrap();
            let conn = db.connect().unwrap();
            conn.execute(CREATE, &[]).unwrap();
            let ins = conn.prepare("INSERT INTO t (id, n) VALUES (?1, ?2)").unwrap();
            let t = Instant::now();
            conn.execute("BEGIN", &[]).unwrap();
            for i in 1..=bulk_n {
                ins.execute(&params![i, i * 2]).unwrap();
            }
            conn.execute("COMMIT", &[]).unwrap();
            ops(bulk_n, t.elapsed())
        }),
    ));

    // DB canónica poblada (`bulk_n` filas) para los pasos de lectura 3 y 5.
    let dir = bench_dir();
    let db = Database::open(dir.path().join("r.arkeion"), Options::default()).unwrap();
    let conn = db.connect().unwrap();
    conn.execute(CREATE, &[]).unwrap();
    {
        let ins = conn.prepare("INSERT INTO t (id, n) VALUES (?1, ?2)").unwrap();
        conn.execute("BEGIN", &[]).unwrap();
        for i in 1..=bulk_n {
            ins.execute(&params![i, i * 2]).unwrap();
        }
        conn.execute("COMMIT", &[]).unwrap();
    }

    // 3) SELECT por PK (point lookup): mediana in situ. Acceso por índice entero
    // (simétrico con el `r.get(0)` de SQLite, sin búsqueda de columna por nombre).
    let sel = conn.prepare("SELECT n FROM t WHERE id = ?1").unwrap();
    out.push((
        "select por PK",
        median_of(READ_REPS, || {
            let t = Instant::now();
            let mut checksum = 0i64;
            for k in 0..bulk_n {
                let id = scattered(k, bulk_n);
                let row = sel.query(&params![id]).unwrap().next().unwrap().unwrap();
                checksum ^= row.get::<i64>(0).unwrap();
            }
            std::hint::black_box(checksum);
            ops(bulk_n, t.elapsed())
        }),
    ));

    // 4) UPDATE durable por PK (1 commit/fila): DB fresca sembrada por rep (la
    // siembra de `durable_n` filas queda fuera del cronómetro).
    out.push((
        "update por PK (durable)",
        median_of(DURABLE_REPS, || {
            let dir = bench_dir();
            let db = Database::open(dir.path().join("u.arkeion"), Options::default()).unwrap();
            let conn = db.connect().unwrap();
            conn.execute(CREATE, &[]).unwrap();
            {
                let ins = conn.prepare("INSERT INTO t (id, n) VALUES (?1, ?2)").unwrap();
                conn.execute("BEGIN", &[]).unwrap();
                for i in 1..=durable_n {
                    ins.execute(&params![i, i * 2]).unwrap();
                }
                conn.execute("COMMIT", &[]).unwrap();
            }
            let upd = conn.prepare("UPDATE t SET n = ?2 WHERE id = ?1").unwrap();
            let t = Instant::now();
            for i in 1..=durable_n {
                upd.execute(&params![i, i * 3]).unwrap();
            }
            ops(durable_n, t.elapsed())
        }),
    ));

    // 5) Full scan: mediana de `scan_reps` pasadas (cada una una pasada completa).
    let scan = conn.prepare("SELECT n FROM t").unwrap();
    out.push((
        "full scan (filas/s)",
        median_of(scan_reps as usize, || {
            let t = Instant::now();
            let mut sum = 0i64;
            for row in scan.query(&[]).unwrap() {
                sum = sum.wrapping_add(row.unwrap().get::<i64>(0).unwrap());
            }
            std::hint::black_box(sum);
            ops(bulk_n, t.elapsed())
        }),
    ));

    // 6) DELETE durable por PK (1 commit/fila): DB fresca sembrada por rep.
    out.push((
        "delete por PK (durable)",
        median_of(DURABLE_REPS, || {
            let dir = bench_dir();
            let db = Database::open(dir.path().join("d.arkeion"), Options::default()).unwrap();
            let conn = db.connect().unwrap();
            conn.execute(CREATE, &[]).unwrap();
            {
                let ins = conn.prepare("INSERT INTO t (id, n) VALUES (?1, ?2)").unwrap();
                conn.execute("BEGIN", &[]).unwrap();
                for i in 1..=durable_n {
                    ins.execute(&params![i, i * 2]).unwrap();
                }
                conn.execute("COMMIT", &[]).unwrap();
            }
            let del = conn.prepare("DELETE FROM t WHERE id = ?1").unwrap();
            let t = Instant::now();
            for i in 1..=durable_n {
                del.execute(&params![i]).unwrap();
            }
            ops(durable_n, t.elapsed())
        }),
    ));

    // 7) SELECT por columna con índice secundario: DB propia con índice, mediana
    // in situ. `k` único (i*2); la consulta desciende el índice → rowid → fila.
    let idir = bench_dir();
    let idb = Database::open(idir.path().join("i.arkeion"), Options::default()).unwrap();
    let iconn = idb.connect().unwrap();
    iconn
        .execute("CREATE TABLE it (id INTEGER PRIMARY KEY, k INTEGER NOT NULL)", &[])
        .unwrap();
    {
        let ins = iconn.prepare("INSERT INTO it (id, k) VALUES (?1, ?2)").unwrap();
        iconn.execute("BEGIN", &[]).unwrap();
        for i in 1..=bulk_n {
            ins.execute(&params![i, i * 2]).unwrap();
        }
        iconn.execute("COMMIT", &[]).unwrap();
    }
    iconn.execute("CREATE INDEX ix_k ON it (k)", &[]).unwrap();
    let isel = iconn.prepare("SELECT id FROM it WHERE k = ?1").unwrap();
    out.push((
        "select por índice 2º",
        median_of(READ_REPS, || {
            let t = Instant::now();
            let mut checksum = 0i64;
            for j in 0..bulk_n {
                let k = scattered(j, bulk_n) * 2; // un valor de k existente
                let row = isel.query(&params![k]).unwrap().next().unwrap().unwrap();
                checksum ^= row.get::<i64>(0).unwrap();
            }
            std::hint::black_box(checksum);
            ops(bulk_n, t.elapsed())
        }),
    ));

    out
}

// --- SQLite (rusqlite, bundled), tras la feature `bench-sqlite` ---

#[cfg(feature = "bench-sqlite")]
fn run_sqlite(durable_n: i64, bulk_n: i64, scan_reps: i64) -> Vec<(&'static str, f64)> {
    use rusqlite::Connection;

    // Durabilidad equivalente a arkeion en cómputo de commits: synchronous=FULL.
    // Journal de rollback por defecto (2 fsync/commit); con ARKEION_BENCH_SQLITE_WAL=1,
    // modo WAL (1 fsync/commit).
    fn open(path: std::path::PathBuf) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.pragma_update(None, "synchronous", "FULL").unwrap();
        if std::env::var("ARKEION_BENCH_SQLITE_WAL").is_ok() {
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        }
        conn.execute_batch(CREATE).unwrap();
        conn
    }
    fn seed(conn: &Connection, n: i64) {
        let tx = conn.unchecked_transaction().unwrap();
        {
            let mut ins = tx.prepare("INSERT INTO t (id, n) VALUES (?1, ?2)").unwrap();
            for i in 1..=n {
                ins.execute(rusqlite::params![i, i * 2]).unwrap();
            }
        }
        tx.commit().unwrap();
    }

    let mut out = Vec::new();

    // 1) INSERT durable: DB fresca por rep.
    out.push((
        "insert 1 fila/commit (durable)",
        median_of(DURABLE_REPS, || {
            let dir = bench_dir();
            let conn = open(dir.path().join("a.sqlite"));
            let mut ins = conn.prepare("INSERT INTO t (id, n) VALUES (?1, ?2)").unwrap();
            let t = Instant::now();
            for i in 1..=durable_n {
                ins.execute(rusqlite::params![i, i * 2]).unwrap();
            }
            ops(durable_n, t.elapsed())
        }),
    ));

    // 2) INSERT por lote en una transacción: DB fresca por rep.
    out.push((
        "insert lote (1 commit)",
        median_of(DURABLE_REPS, || {
            let dir = bench_dir();
            let conn = open(dir.path().join("b.sqlite"));
            let t = Instant::now();
            {
                let tx = conn.unchecked_transaction().unwrap();
                {
                    let mut ins = tx.prepare("INSERT INTO t (id, n) VALUES (?1, ?2)").unwrap();
                    for i in 1..=bulk_n {
                        ins.execute(rusqlite::params![i, i * 2]).unwrap();
                    }
                }
                tx.commit().unwrap();
            }
            ops(bulk_n, t.elapsed())
        }),
    ));

    // DB canónica para lecturas.
    let dir = bench_dir();
    let conn = open(dir.path().join("r.sqlite"));
    seed(&conn, bulk_n);

    // 3) SELECT por PK: mediana in situ.
    let mut sel = conn.prepare("SELECT n FROM t WHERE id = ?1").unwrap();
    out.push((
        "select por PK",
        median_of(READ_REPS, || {
            let t = Instant::now();
            let mut checksum = 0i64;
            for k in 0..bulk_n {
                let id = scattered(k, bulk_n);
                let n: i64 = sel.query_row(rusqlite::params![id], |r| r.get(0)).unwrap();
                checksum ^= n;
            }
            std::hint::black_box(checksum);
            ops(bulk_n, t.elapsed())
        }),
    ));
    drop(sel);

    // 4) UPDATE durable: DB fresca sembrada por rep.
    out.push((
        "update por PK (durable)",
        median_of(DURABLE_REPS, || {
            let dir = bench_dir();
            let conn = open(dir.path().join("u.sqlite"));
            seed(&conn, durable_n);
            let mut upd = conn.prepare("UPDATE t SET n = ?2 WHERE id = ?1").unwrap();
            let t = Instant::now();
            for i in 1..=durable_n {
                upd.execute(rusqlite::params![i, i * 3]).unwrap();
            }
            ops(durable_n, t.elapsed())
        }),
    ));

    // 5) Full scan: mediana de `scan_reps` pasadas.
    let mut scan = conn.prepare("SELECT n FROM t").unwrap();
    out.push((
        "full scan (filas/s)",
        median_of(scan_reps as usize, || {
            let t = Instant::now();
            let mut sum = 0i64;
            let mut rows = scan.query([]).unwrap();
            while let Some(row) = rows.next().unwrap() {
                sum = sum.wrapping_add(row.get::<_, i64>(0).unwrap());
            }
            std::hint::black_box(sum);
            ops(bulk_n, t.elapsed())
        }),
    ));
    drop(scan);

    // 6) DELETE durable: DB fresca sembrada por rep.
    out.push((
        "delete por PK (durable)",
        median_of(DURABLE_REPS, || {
            let dir = bench_dir();
            let conn = open(dir.path().join("d.sqlite"));
            seed(&conn, durable_n);
            let mut del = conn.prepare("DELETE FROM t WHERE id = ?1").unwrap();
            let t = Instant::now();
            for i in 1..=durable_n {
                del.execute(rusqlite::params![i]).unwrap();
            }
            ops(durable_n, t.elapsed())
        }),
    ));

    // 7) SELECT por columna con índice secundario.
    let idir = bench_dir();
    let iconn = Connection::open(idir.path().join("i.sqlite")).unwrap();
    iconn.pragma_update(None, "synchronous", "FULL").unwrap();
    iconn
        .execute_batch("CREATE TABLE it (id INTEGER PRIMARY KEY, k INTEGER NOT NULL)")
        .unwrap();
    {
        let tx = iconn.unchecked_transaction().unwrap();
        {
            let mut ins = tx.prepare("INSERT INTO it (id, k) VALUES (?1, ?2)").unwrap();
            for i in 1..=bulk_n {
                ins.execute(rusqlite::params![i, i * 2]).unwrap();
            }
        }
        tx.commit().unwrap();
    }
    iconn.execute_batch("CREATE INDEX ix_k ON it (k)").unwrap();
    let mut isel = iconn.prepare("SELECT id FROM it WHERE k = ?1").unwrap();
    out.push((
        "select por índice 2º",
        median_of(READ_REPS, || {
            let t = Instant::now();
            let mut checksum = 0i64;
            for j in 0..bulk_n {
                let k = scattered(j, bulk_n) * 2;
                let id: i64 = isel.query_row(rusqlite::params![k], |r| r.get(0)).unwrap();
                checksum ^= id;
            }
            std::hint::black_box(checksum);
            ops(bulk_n, t.elapsed())
        }),
    ));

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
/// Mediana de `DURABLE_REPS` con DB fresca por rep. Devuelve (durable, lote) ops/s.
///
/// Aviso de alcance: `t_log` + cadena de hash aproxima la historia tamper-evident,
/// pero **no** reproduce el `content_hash` por commit de arkeion (que autentica el
/// cuerpo físico completo) ni un snapshot versionado **consultable** (`AS OF`). Así
/// que sub-estima el coste real de igualar a arkeion: trata estos ratios como
/// **cota inferior** del sobrecoste de SQLite.
#[cfg(feature = "bench-sqlite")]
fn run_sqlite_audited(durable_n: i64, bulk_n: i64) -> (f64, f64) {
    use rusqlite::Connection;
    use sha2::{Digest, Sha256};

    let wal = std::env::var("ARKEION_BENCH_SQLITE_WAL").is_ok();
    fn chain(prev: &[u8; 32], rid: i64, n: i64) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(prev);
        h.update(rid.to_le_bytes());
        h.update(n.to_le_bytes());
        h.finalize().into()
    }
    let setup = move |path: std::path::PathBuf| -> Connection {
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

    // Durable: cada insert lógico en su propia transacción ⇒ 1 fsync, como arkeion.
    let durable = median_of(DURABLE_REPS, || {
        let dir = bench_dir();
        let mut conn = setup(dir.path().join("ad.sqlite"));
        let mut prev = [0u8; 32];
        let t = Instant::now();
        for i in 1..=durable_n {
            let n = i * 2;
            prev = chain(&prev, i, n);
            let tx = conn.transaction().unwrap();
            tx.execute("INSERT INTO t (id, n) VALUES (?1, ?2)", rusqlite::params![i, n])
                .unwrap();
            tx.execute(
                "INSERT INTO t_log (rid, n, hash) VALUES (?1, ?2, ?3)",
                rusqlite::params![i, n, &prev[..]],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        ops(durable_n, t.elapsed())
    });

    // Lote: todo en una transacción.
    let batch = median_of(DURABLE_REPS, || {
        let dir = bench_dir();
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
    });
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
    let on_real_disk = std::env::var("ARKEION_BENCH_DIR").is_ok();
    let loc = std::env::var("ARKEION_BENCH_DIR")
        .unwrap_or_else(|_| std::env::temp_dir().display().to_string());
    println!("Arkeion — benchmark CRUD monohilo (+ índice secundario)");
    println!(
        "  durable_n={durable_n}  bulk_n={bulk_n}  scan_reps={scan_reps}  reps(read/durable)={READ_REPS}/{DURABLE_REPS} (mediana)"
    );
    println!(
        "  escritura en: {loc}  {}",
        if on_real_disk {
            "(ARKEION_BENCH_DIR → disco real)"
        } else {
            "(por defecto; si es tmpfs los fsync NO tocan disco — usa ARKEION_BENCH_DIR)"
        }
    );
    println!(
        "  durabilidad: arkeion 1 fdatasync/commit; SQLite synchronous=FULL+journal (2 fsync/commit); SQLite={}",
        if with_sqlite {
            "ON"
        } else {
            "OFF (--features bench-sqlite)"
        }
    );
    println!("  calentando ambos motores…");
    // Calentamiento simétrico: una pasada corta descartada por motor (estabiliza
    // caché de páginas/FS, allocator y predicción de saltos en los DOS lados).
    let _ = run_arkeion(200, 2_000, 2);
    let _ = run_sqlite(200, 2_000, 2);

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
    println!("ratio = arkeion / sqlite  (>1 ⇒ arkeion más rápido en esa operación). Mediana de N.");

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
