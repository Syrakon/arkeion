//! Throughput vectorial **concurrente**: aísla el lever de paralelismo del de
//! latencia por query. Construye un índice IVF UNA vez, fija `nprobe` (un punto de
//! recall) y barre el nº de hilos, midiendo **qps agregado** = total_queries /
//! wall_time. QPS es throughput: el modelo CoW/MVCC (páginas inmutables, `Database`
//! es `Clone`+`Send`+`Sync`) permite N lectores; el techo lo marca el `Mutex<PageCache>`
//! del pager. Esto mide ese techo real.
//!
//! Entrada por entorno:
//!   VEC_DIR        dir con base.f32 / query.f32 / gt.i32 / meta.json (de prep_sift.py)
//!   VEC_DBDIR      dir en disco real para el .arkeion (default: tempdir)
//!   VEC_LISTS      nº de clusters IVF (default: round(sqrt(n)))
//!   VEC_CONC_NPROBE  lista de nprobe a medir, p.ej. "10,20,50" (default "10,20,50")
//!   VEC_THREADS    lista de nº de hilos, p.ej. "1,2,4,8" (default "1,2,4,8")
//!   VEC_CONC_Q     total de queries por medición (default 8000; se cicla si > nq)
//!   VEC_RECALL_Q   queries para medir recall por nprobe (default 1000)

use std::fs;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use arkeion::{Database, Options, Value};

const STRIDE_TAG: usize = 1;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok()
}

fn json_int(s: &str, key: &str) -> usize {
    let pat = format!("\"{key}\"");
    let i = s.find(&pat).unwrap_or_else(|| panic!("falta {key} en meta.json"));
    let rest = &s[i + pat.len()..];
    let j = rest.find(':').unwrap() + 1;
    s[i + pat.len() + j..]
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap()
}

fn read_f32(path: &str) -> Vec<f32> {
    fs::read(path)
        .unwrap()
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn read_i32(path: &str) -> Vec<i32> {
    fs::read(path)
        .unwrap()
        .chunks_exact(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn recall_at_k(got: &[Vec<i64>], gt: &[i32], nq: usize, k: usize) -> f64 {
    let mut acc = 0.0;
    for (q, ids) in got.iter().enumerate().take(nq) {
        let truth: std::collections::HashSet<i64> = (0..k).map(|j| gt[q * k + j] as i64).collect();
        let hit = ids.iter().filter(|r| truth.contains(r)).count();
        acc += hit as f64 / k as f64;
    }
    acc / nq as f64
}

fn parse_list(s: Option<String>, default: &[usize]) -> Vec<usize> {
    match s {
        Some(s) => s.split(',').filter_map(|x| x.trim().parse().ok()).collect(),
        None => default.to_vec(),
    }
}

fn main() {
    let dir = env("VEC_DIR").expect("define VEC_DIR (salida de prep_sift.py)");
    let meta = fs::read_to_string(format!("{dir}/meta.json")).unwrap();
    let n = json_int(&meta, "n");
    let dim = json_int(&meta, "dim");
    let nq = json_int(&meta, "nq");
    let k = json_int(&meta, "k");
    let stride = dim * 4;
    eprintln!("== Arkeion vec CONCURRENT == n={n} dim={dim} nq={nq} k={k}");

    let base_bytes = fs::read(format!("{dir}/base.f32")).unwrap();
    assert_eq!(base_bytes.len(), n * stride, "tamaño de base.f32 inesperado");
    let queries = read_f32(&format!("{dir}/query.f32"));
    let gt = read_i32(&format!("{dir}/gt.i32"));

    let tmp = tempfile::tempdir().unwrap();
    let dbdir = env("VEC_DBDIR").unwrap_or_else(|| tmp.path().to_string_lossy().into_owned());
    let dbpath = format!("{dbdir}/vec_concurrent.arkeion");
    let _ = fs::remove_file(&dbpath);
    let db = Database::open(&dbpath, Options::default()).unwrap();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE docs (id INTEGER PRIMARY KEY, emb BLOB)", &[])
        .unwrap();

    let t = Instant::now();
    conn.bulk_insert(
        "docs",
        (0..n).map(|i| {
            let off = i * stride;
            let mut blob = Vec::with_capacity(STRIDE_TAG + stride);
            blob.push(0x00);
            blob.extend_from_slice(&base_bytes[off..off + stride]);
            [Value::Integer(i as i64), Value::Blob(blob)]
        }),
    )
    .unwrap();
    drop(base_bytes);
    eprintln!("carga: {n} vectores en {:.2}s", t.elapsed().as_secs_f64());

    let lists: usize = env("VEC_LISTS")
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| (n as f64).sqrt().round() as usize)
        .clamp(1, 65535);
    let t = Instant::now();
    conn.execute(
        &format!("CREATE VECTOR INDEX vi ON docs (emb) USING l2 LISTS {lists}"),
        &[],
    )
    .unwrap();
    eprintln!("IVF build: lists={lists} en {:.2}s", t.elapsed().as_secs_f64());

    let nprobes = parse_list(env("VEC_CONC_NPROBE"), &[10, 20, 50]);
    let thread_counts = parse_list(env("VEC_THREADS"), &[1, 2, 4, 8]);
    let total_q = env("VEC_CONC_Q").and_then(|s| s.parse().ok()).unwrap_or(8000);
    let recall_q = env("VEC_RECALL_Q")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000)
        .min(nq);

    let queries = Arc::new(queries);
    let db = Arc::new(db);

    for &np in &nprobes {
        // Recall del punto de operación (single-thread, una muestra).
        let conn = db.connect().unwrap();
        let reader = conn.table("docs").unwrap();
        let mut got = Vec::with_capacity(recall_q);
        for q in 0..recall_q {
            let qv = &queries[q * dim..(q + 1) * dim];
            got.push(reader.vector_search("vi", qv, k, np).unwrap());
        }
        let rec = recall_at_k(&got, &gt, recall_q, k);
        drop(reader);
        drop(conn);
        eprintln!("\n=== nprobe={np}  recall@{k}={rec:.4}  (total_q={total_q}) ===");

        let mut single_qps = 0.0f64;
        for &nt in &thread_counts {
            let t = Instant::now();
            let handles: Vec<_> = (0..nt)
                .map(|tid| {
                    let db = Arc::clone(&db);
                    let queries = Arc::clone(&queries);
                    thread::spawn(move || {
                        // Cada hilo: su propia Connection/Snapshot (lectura sin lock con
                        // el escritor); comparte el Arc<Store> y su caché de páginas.
                        let conn = db.connect().unwrap();
                        let reader = conn.table("docs").unwrap();
                        let mut done = 0usize;
                        let mut q = tid;
                        while q < total_q {
                            let idx = (q % nq) * dim;
                            let qv = &queries[idx..idx + dim];
                            let _ = reader.vector_search("vi", qv, k, np).unwrap();
                            done += 1;
                            q += nt;
                        }
                        done
                    })
                })
                .collect();
            let done: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
            let secs = t.elapsed().as_secs_f64();
            let qps = done as f64 / secs;
            if nt == 1 {
                single_qps = qps;
            }
            let speedup = if single_qps > 0.0 { qps / single_qps } else { 1.0 };
            eprintln!(
                "  threads={nt:>2}: {done} queries en {secs:6.3}s = {qps:7.0} qps  ({speedup:.2}× vs 1 hilo)"
            );
        }
    }
    eprintln!("\nlisto.");
}
