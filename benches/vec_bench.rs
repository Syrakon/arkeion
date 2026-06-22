//! Benchmark vectorial de Arkeion sobre un dataset real tipo SIFT (métrica L2),
//! con **recall@k contra ground-truth**. Mide: carga masiva, tamaño en disco,
//! KNN **exacto** (full scan) y el **IVF/ANN** barriendo `nprobe` (la curva
//! recall/latencia). Emite JSON para fusionarlo con pgvector/Qdrant (ver `bench/`).
//!
//! No es comparable con SQLite/Postgres/Qdrant *dentro* de este binario: cada
//! sistema se mide en su cliente idiomático y se cruza por recall + build + tamaño
//! (independientes del lenguaje) y por latencia con el caveat embebido-vs-servidor.
//!
//! Entrada por entorno:
//!   VEC_DIR    dir con base.f32 / query.f32 / gt.i32 / meta.json (de prep_sift.py)
//!   VEC_DBDIR  dir en disco real para el .arkeion (default: tempdir)
//!   VEC_LISTS  nº de clusters IVF (default: round(sqrt(n)))
//!   VEC_OUT    ruta del JSON de salida (default: stdout)
//!   VEC_EXACT_Q  nº de queries para el KNN exacto/full-scan (default: 100)
//!
//! ```text
//! VEC_DIR=bench/data/sift_1000000 VEC_DBDIR=/mnt/datos/tmp VEC_OUT=bench/results/arkeion_vec.json \
//!   cargo bench --bench vec_bench
//! ```

use std::fs;
use std::time::Instant;

use arkeion::{Database, Options, Value};

const STRIDE_TAG: usize = 1; // byte de tag del formato de vector (0x00 = f32)

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok()
}

/// Parser minúsculo para el meta.json plano `{"n":..,"dim":..,...}`.
fn json_int(s: &str, key: &str) -> usize {
    let pat = format!("\"{key}\"");
    let i = s
        .find(&pat)
        .unwrap_or_else(|| panic!("falta {key} en meta.json"));
    let rest = &s[i + pat.len()..];
    let j = rest.find(':').unwrap() + 1;
    let tail = &rest[j..];
    let num: String = tail
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    num.parse().unwrap()
}

/// Lee un archivo de f32 little-endian a Vec<f32>.
fn read_f32(path: &str) -> Vec<f32> {
    let bytes = fs::read(path).unwrap();
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Lee un archivo de i32 little-endian a Vec<i32>.
fn read_i32(path: &str) -> Vec<i32> {
    let bytes = fs::read(path).unwrap();
    bytes
        .chunks_exact(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// recall@k medio: |devueltos ∩ ground-truth| / k, promediado por query.
fn recall_at_k(got: &[Vec<i64>], gt: &[i32], nq: usize, k: usize) -> f64 {
    let mut acc = 0.0;
    for (q, ids) in got.iter().enumerate().take(nq) {
        let truth: std::collections::HashSet<i64> = (0..k).map(|j| gt[q * k + j] as i64).collect();
        let hit = ids.iter().filter(|r| truth.contains(r)).count();
        acc += hit as f64 / k as f64;
    }
    acc / nq as f64
}

fn main() {
    let dir = env("VEC_DIR").expect("define VEC_DIR (salida de prep_sift.py)");
    let meta = fs::read_to_string(format!("{dir}/meta.json")).unwrap();
    let n = json_int(&meta, "n");
    let dim = json_int(&meta, "dim");
    let nq = json_int(&meta, "nq");
    let k = json_int(&meta, "k");
    let stride = dim * 4;

    eprintln!("== Arkeion vec bench == n={n} dim={dim} nq={nq} k={k}");

    // Vectores base como bytes crudos (ya son f32 LE → el BLOB es tag ++ bytes).
    let base_bytes = fs::read(format!("{dir}/base.f32")).unwrap();
    assert_eq!(
        base_bytes.len(),
        n * stride,
        "tamaño de base.f32 inesperado"
    );
    let queries = read_f32(&format!("{dir}/query.f32"));
    let gt = read_i32(&format!("{dir}/gt.i32"));

    // DB en disco real si se pide (los fsync de la carga tocan disco de verdad).
    let tmp = tempfile::tempdir().unwrap();
    let dbdir = env("VEC_DBDIR").unwrap_or_else(|| tmp.path().to_string_lossy().into_owned());
    let dbpath = format!("{dbdir}/vec_bench.arkeion");
    let _ = fs::remove_file(&dbpath);
    let db = Database::open(&dbpath, Options::default()).unwrap();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE docs (id INTEGER PRIMARY KEY, emb BLOB)", &[])
        .unwrap();

    // --- Carga masiva: id = i (0-based, casa con los ids del ground-truth) ---
    let t = Instant::now();
    conn.bulk_insert(
        "docs",
        (0..n).map(|i| {
            let off = i * stride;
            let mut blob = Vec::with_capacity(STRIDE_TAG + stride);
            blob.push(0x00); // TAG_F32
            blob.extend_from_slice(&base_bytes[off..off + stride]);
            [Value::Integer(i as i64), Value::Blob(blob)]
        }),
    )
    .unwrap();
    let load_s = t.elapsed().as_secs_f64();
    let size_loaded = fs::metadata(&dbpath).unwrap().len();
    drop(base_bytes); // libera ~n*dim*4 bytes de RAM antes del resto
    eprintln!(
        "carga: {n} vectores en {load_s:.2}s = {:.0}/s · tabla {:.1} MB",
        n as f64 / load_s,
        size_loaded as f64 / 1e6
    );

    // --- KNN EXACTO (full scan, sin índice): mide latencia y recall (~1.0) ---
    let exact_q: usize = env("VEC_EXACT_Q")
        .and_then(|s| s.parse().ok())
        .unwrap_or(100)
        .min(nq);
    let sel = conn
        .prepare("SELECT id FROM docs ORDER BY l2_distance(emb, ?1) LIMIT ?2")
        .unwrap();
    let mut exact_got = Vec::with_capacity(exact_q);
    let t = Instant::now();
    for q in 0..exact_q {
        let qv = &queries[q * dim..(q + 1) * dim];
        let mut blob = Vec::with_capacity(STRIDE_TAG + stride);
        blob.push(0x00);
        for &v in qv {
            blob.extend_from_slice(&v.to_le_bytes());
        }
        let rows = sel
            .query(&[Value::Blob(blob), Value::Integer(k as i64)])
            .unwrap();
        let ids: Vec<i64> = rows.map(|r| r.unwrap().get::<i64>(0).unwrap()).collect();
        exact_got.push(ids);
    }
    let exact_ms = t.elapsed().as_secs_f64() * 1000.0 / exact_q as f64;
    let exact_recall = recall_at_k(&exact_got, &gt, exact_q, k);
    eprintln!("KNN exacto: {exact_ms:.2} ms/query · recall@{k}={exact_recall:.4} (sanity ~1.0)");

    // --- Construye el índice IVF: mide build time y tamaño del índice ---
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
    let build_s = t.elapsed().as_secs_f64();
    let size_indexed = fs::metadata(&dbpath).unwrap().len();
    let index_mb = (size_indexed.saturating_sub(size_loaded)) as f64 / 1e6;
    eprintln!("IVF build: lists={lists} en {build_s:.2}s · índice +{index_mb:.1} MB");

    // --- Barrido ANN: nprobe en consulta (curva recall/latencia) ---
    // A 1M, re-rankear nprobe·(n/lists) candidatos por query se dispara: se acota
    // el nº de queries del barrido (VEC_ANN_Q) y el nprobe máximo (VEC_MAXPROBE).
    let reader = conn.table("docs").unwrap();
    let ann_q = env("VEC_ANN_Q")
        .and_then(|s| s.parse().ok())
        .unwrap_or(nq)
        .min(nq);
    let maxprobe = env("VEC_MAXPROBE")
        .and_then(|s| s.parse().ok())
        .unwrap_or(usize::MAX);
    let probes: Vec<usize> = [1usize, 2, 5, 10, 20, 50, 100, 200, 400]
        .into_iter()
        .filter(|&p| p <= lists && p <= maxprobe)
        .collect();
    let mut sweep = Vec::new();
    for &np in &probes {
        let mut got = Vec::with_capacity(ann_q);
        let t = Instant::now();
        for q in 0..ann_q {
            let qv = &queries[q * dim..(q + 1) * dim];
            got.push(reader.vector_search("vi", qv, k, np).unwrap());
        }
        let ms = t.elapsed().as_secs_f64() * 1000.0 / ann_q as f64;
        let rec = recall_at_k(&got, &gt, ann_q, k);
        eprintln!(
            "  nprobe={np:>3}: {ms:6.3} ms/query · {:7.0} qps · recall@{k}={rec:.4}",
            1000.0 / ms
        );
        sweep.push((np, ms, rec));
    }

    // --- JSON de salida ---
    let mut j = String::new();
    j.push_str("{\n");
    j.push_str("  \"system\": \"arkeion\", \"metric\": \"l2\",\n");
    j.push_str(&format!(
        "  \"n\": {n}, \"dim\": {dim}, \"nq\": {nq}, \"k\": {k}, \"lists\": {lists},\n"
    ));
    j.push_str(&format!(
        "  \"load_sec\": {load_s:.4}, \"load_per_sec\": {:.1},\n",
        n as f64 / load_s
    ));
    j.push_str(&format!(
        "  \"table_mb\": {:.2}, \"index_mb\": {index_mb:.2},\n",
        size_loaded as f64 / 1e6
    ));
    j.push_str(&format!("  \"build_sec\": {build_s:.4},\n"));
    j.push_str(&format!("  \"exact_ms\": {exact_ms:.4}, \"exact_recall\": {exact_recall:.4}, \"exact_queries\": {exact_q},\n"));
    j.push_str(&format!("  \"ann_queries\": {ann_q},\n"));
    j.push_str("  \"ann\": [\n");
    for (i, (np, ms, rec)) in sweep.iter().enumerate() {
        let comma = if i + 1 < sweep.len() { "," } else { "" };
        j.push_str(&format!(
            "    {{\"nprobe\": {np}, \"ms\": {ms:.4}, \"qps\": {:.1}, \"recall\": {rec:.4}}}{comma}\n",
            1000.0 / ms
        ));
    }
    j.push_str("  ]\n}\n");

    match env("VEC_OUT") {
        Some(p) => {
            fs::write(&p, &j).unwrap();
            eprintln!("escrito {p}");
        }
        None => println!("{j}"),
    }
}
