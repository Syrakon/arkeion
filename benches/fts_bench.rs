//! Benchmark FTS (full-text search) sobre passages reales de MS MARCO: Arkeion
//! (`MATCH` + `bm25`) vs **SQLite FTS5**, ambos embebidos y en el mismo proceso
//! (comparación limpia manzana-con-manzana). Mide carga, build del índice, tamaño
//! y latencia por tipo de query (término, AND, frase, prefijo). Postgres tsvector
//! se mide aparte (Python, ver `bench/`). Emite JSON.
//!
//! ```text
//! FTS_FILE=bench/data/msmarco_1M.tsv FTS_DBDIR=/mnt/datos/tmp FTS_OUT=bench/results/fts.json \
//!   cargo bench --bench fts_bench --features bench-sqlite
//! ```

use std::fs;
use std::io::{BufRead, BufReader};
use std::time::Instant;

use arkeion::{Database, Options, Value};

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok()
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

/// Queries comunes a ambos motores. (nombre, query_arkeion, query_fts5).
/// Sintaxis casi igual; difieren en detalles (frase/AND) que se mapean aquí.
fn queries() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("term_común", "water", "water"),
        ("term_raro", "photosynthesis", "photosynthesis"),
        ("AND", "blood AND pressure", "blood AND pressure"),
        ("frase", "\"new york\"", "\"new york\""),
        ("prefijo", "comput*", "comput*"),
    ]
}

const REPS: usize = 5;

fn load_passages(path: &str, limit: usize) -> Vec<(i64, String)> {
    let f = fs::File::open(path).expect("FTS_FILE no existe");
    let mut out = Vec::new();
    for line in BufReader::new(f).lines() {
        let line = line.unwrap();
        let Some((id, text)) = line.split_once('\t') else {
            continue;
        };
        let id: i64 = match id.parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        out.push((id, text.to_owned()));
        if out.len() >= limit {
            break;
        }
    }
    out
}

fn main() {
    let path = env("FTS_FILE").expect("define FTS_FILE (id\\ttext por línea)");
    let limit: usize = env("FTS_LIMIT")
        .and_then(|s| s.parse().ok())
        .unwrap_or(usize::MAX);
    eprintln!("== FTS bench == cargando passages de {path} ...");
    let docs = load_passages(&path, limit);
    let n = docs.len();
    eprintln!("{n} passages cargados");

    let tmp = tempfile::tempdir().unwrap();
    let dbdir = env("FTS_DBDIR").unwrap_or_else(|| tmp.path().to_string_lossy().into_owned());

    // SQLite primero: así sus números se capturan aunque Arkeion peté por el bug
    // de corrupción del índice FTS a escala (lo registramos sin abortar).
    let sqlite = run_sqlite(&docs, &dbdir);
    let ark =
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_arkeion(&docs, &dbdir)))
        {
            Ok(s) => s,
            Err(_) => {
                eprintln!("[arkeion] FTS build PANIC (bug de corrupción a escala) — registrado");
                String::from("{\"error\": \"FTS index build corrupted the b-tree (panic)\"}")
            }
        };

    // --- JSON ---
    let mut j = String::new();
    j.push_str("{\n");
    j.push_str(&format!("  \"dataset\": \"msmarco\", \"n\": {n},\n"));
    j.push_str(&format!("  \"arkeion\": {ark},\n"));
    j.push_str(&format!("  \"sqlite_fts5\": {sqlite}\n"));
    j.push_str("}\n");
    match env("FTS_OUT") {
        Some(p) => {
            fs::write(&p, &j).unwrap();
            eprintln!("escrito {p}");
        }
        None => println!("{j}"),
    }
}

fn run_arkeion(docs: &[(i64, String)], dbdir: &str) -> String {
    let n = docs.len();
    let dbpath = format!("{dbdir}/fts_bench.arkeion");
    let _ = fs::remove_file(&dbpath);
    let db = Database::open(&dbpath, Options::default()).unwrap();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .unwrap();

    let t = Instant::now();
    conn.bulk_insert(
        "docs",
        docs.iter()
            .map(|(id, body)| [Value::Integer(*id), Value::Text(body.clone())]),
    )
    .unwrap();
    let load_s = t.elapsed().as_secs_f64();
    let size_loaded = fs::metadata(&dbpath).unwrap().len();

    // CREATE FULLTEXT INDEX backfillea las filas existentes ⇒ build del índice.
    let t = Instant::now();
    conn.execute("CREATE FULLTEXT INDEX f ON docs (body)", &[])
        .unwrap();
    let build_s = t.elapsed().as_secs_f64();
    let index_mb = (fs::metadata(&dbpath)
        .unwrap()
        .len()
        .saturating_sub(size_loaded)) as f64
        / 1e6;
    eprintln!(
        "[arkeion] carga {load_s:.2}s ({:.0}/s) · índice FTS build {build_s:.2}s +{index_mb:.1} MB",
        n as f64 / load_s
    );

    let mut per_query = String::from("[\n");
    for (i, (name, q, _)) in queries().iter().enumerate() {
        let sql = format!(
            "SELECT id FROM docs WHERE body MATCH '{q}' ORDER BY bm25(body, '{q}') DESC LIMIT 10"
        );
        let mut hits = 0usize;
        let ms = median(
            (0..REPS)
                .map(|_| {
                    let t = Instant::now();
                    hits = conn.query(&sql, &[]).unwrap().count();
                    t.elapsed().as_secs_f64() * 1000.0
                })
                .collect(),
        );
        eprintln!("  [ark] {name:<10} {ms:7.3} ms · {hits} hits");
        let comma = if i + 1 < queries().len() { "," } else { "" };
        per_query.push_str(&format!(
            "      {{\"q\": \"{name}\", \"ms\": {ms:.4}, \"hits\": {hits}}}{comma}\n"
        ));
    }
    per_query.push_str("    ]");
    let _ = fs::remove_file(&dbpath);

    format!(
        "{{\n    \"load_sec\": {load_s:.4}, \"load_per_sec\": {:.1}, \"table_mb\": {:.2},\n    \"build_sec\": {build_s:.4}, \"index_mb\": {index_mb:.2},\n    \"queries\": {per_query}\n  }}",
        n as f64 / load_s,
        size_loaded as f64 / 1e6
    )
}

#[cfg(feature = "bench-sqlite")]
fn run_sqlite(docs: &[(i64, String)], dbdir: &str) -> String {
    use rusqlite::Connection;
    let n = docs.len();
    let dbpath = format!("{dbdir}/fts_bench.sqlite");
    let _ = fs::remove_file(&dbpath);
    let conn = Connection::open(&dbpath).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
        .unwrap();
    // FTS5 construye el índice DURANTE el insert (no hay build aparte).
    conn.execute_batch("CREATE VIRTUAL TABLE docs USING fts5(body)")
        .unwrap();

    let t = Instant::now();
    {
        let tx = conn.unchecked_transaction().unwrap();
        let mut ins = tx
            .prepare("INSERT INTO docs(rowid, body) VALUES (?1, ?2)")
            .unwrap();
        for (id, body) in docs {
            ins.execute(rusqlite::params![id, body]).unwrap();
        }
        drop(ins);
        tx.commit().unwrap();
    }
    let load_s = t.elapsed().as_secs_f64();
    let size_mb = fs::metadata(&dbpath).unwrap().len() as f64 / 1e6;
    eprintln!(
        "[sqlite fts5] carga+índice {load_s:.2}s ({:.0}/s) · total {size_mb:.1} MB",
        n as f64 / load_s
    );

    let mut per_query = String::from("[\n");
    for (i, (name, _, q)) in queries().iter().enumerate() {
        let sql = "SELECT rowid FROM docs WHERE docs MATCH ?1 ORDER BY bm25(docs) LIMIT 10";
        let mut hits = 0usize;
        let ms = median(
            (0..REPS)
                .map(|_| {
                    let t = Instant::now();
                    let mut stmt = conn.prepare(sql).unwrap();
                    hits = stmt
                        .query_map(rusqlite::params![q], |row| row.get::<_, i64>(0))
                        .unwrap()
                        .count();
                    t.elapsed().as_secs_f64() * 1000.0
                })
                .collect(),
        );
        eprintln!("  [sqlite] {name:<10} {ms:7.3} ms · {hits} hits");
        let comma = if i + 1 < queries().len() { "," } else { "" };
        per_query.push_str(&format!(
            "      {{\"q\": \"{name}\", \"ms\": {ms:.4}, \"hits\": {hits}}}{comma}\n"
        ));
    }
    per_query.push_str("    ]");
    let _ = fs::remove_file(&dbpath);

    format!(
        "{{\n    \"load_build_sec\": {load_s:.4}, \"load_per_sec\": {:.1}, \"total_mb\": {size_mb:.2},\n    \"queries\": {per_query}\n  }}",
        n as f64 / load_s
    )
}

#[cfg(not(feature = "bench-sqlite"))]
fn run_sqlite(_docs: &[(i64, String)], _dbdir: &str) -> String {
    String::from("\"(activa --features bench-sqlite)\"")
}
