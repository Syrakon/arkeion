//! IVF (inverted file) — núcleo del ANN vectorial.
//!
//! k-means agrupa los vectores en `k` clusters; la búsqueda solo escanea los
//! `nprobe` clusters cuyos centroides están más cerca de la query (aproximado:
//! puede saltarse vecinos en clusters no visitados). Encaja con Arkeion porque
//! **entrenar es un evento discreto** (como construir un índice), no una mutación
//! por-insert, y es **determinista** dado el orden de entrada (init equiespaciado
//! + Lloyd) → reproducible. Aquí vive solo el núcleo **en memoria** y puro; la
//! persistencia (centroides como datos versionados + postings por cluster, mismo
//! patrón que el FTS) y la integración con el planner van aparte. Cero deps.
//!
//! Métrica: distancia euclídea. Para coseno, **normalizar** los vectores a norma
//! 1 antes (entonces el orden por L2 coincide con el de coseno). Ver
//! `docs/13-vectores.md`.

/// Distancia euclídea al cuadrado (basta para ordenar; evita la raíz). Kernel f32
/// multi-acumulador compartido con el re-rank de candidatos (ver `vector::l2_sq`).
fn dist2(a: &[f32], b: &[f32]) -> f32 {
    crate::vector::l2_sq(a, b)
}

/// Índice del centroide más cercano a `v`.
fn nearest(centroids: &[Vec<f32>], v: &[f32]) -> usize {
    let mut best = 0;
    let mut best_d = f32::INFINITY;
    for (i, c) in centroids.iter().enumerate() {
        let d = dist2(c, v);
        if d < best_d {
            best_d = d;
            best = i;
        }
    }
    best
}

/// Entrena `k` centroides por k-means (Lloyd) sobre `vectors`. Init **determinista**
/// (muestreo equiespaciado), `max_iters` iteraciones o hasta convergencia. Los
/// clusters que quedan vacíos conservan su centroide. Devuelve menos de `k`
/// centroides solo si hay menos vectores que `k`.
pub fn train(vectors: &[Vec<f32>], k: usize, max_iters: usize) -> Vec<Vec<f32>> {
    let n = vectors.len();
    if n == 0 {
        return Vec::new();
    }
    let k = k.clamp(1, n);
    let dim = vectors[0].len();

    // RNG xorshift **sembrado de forma determinista** (depende solo de la forma de
    // los datos: n/k/dim). Lo usan el submuestreo y k-means++ ⇒ REBUILD reproduce
    // exactamente los mismos centroides (el tenet de determinismo se conserva; lo
    // que cambia respecto al init equiespaciado es que ahora arranca mejor).
    let mut st = 0x9E37_79B9_7F4A_7C15u64
        ^ (n as u64).wrapping_mul(0x0100_0000_01b3)
        ^ ((k as u64) << 40)
        ^ (dim as u64);
    let mut rng = || {
        st ^= st << 13;
        st ^= st >> 7;
        st ^= st << 17;
        st
    };

    // Submuestra para ENTRENAR: entrenar sobre los n vectores es O(iters·n·k) y
    // domina el build a escala. Una muestra representativa de ~256·k da centroides
    // equivalentes a una fracción del coste (igual que FAISS). `assign` sí recorre
    // los n vectores después (postings exactos por cluster).
    let sample_cap = k.saturating_mul(256).max(4096);
    let sample: Vec<&Vec<f32>> = if n <= sample_cap {
        vectors.iter().collect()
    } else {
        let step = n / sample_cap;
        (0..sample_cap)
            .map(|i| &vectors[(i * step + rng() as usize % step) % n])
            .collect()
    };
    let m = sample.len();

    // Init **k-means++** (sembrado): cada centroide se elige con prob ∝ distancia²
    // al más cercano ya elegido ⇒ centroides bien separados, menos iteraciones para
    // converger. Coste ≈ una iteración de Lloyd (k·m·dim).
    let mut centroids: Vec<Vec<f32>> = Vec::with_capacity(k);
    centroids.push(sample[rng() as usize % m].clone());
    let mut d2: Vec<f32> = sample.iter().map(|v| dist2(v, &centroids[0])).collect();
    while centroids.len() < k {
        let sum: f64 = d2.iter().map(|&x| x as f64).sum();
        let pick = if sum <= 0.0 {
            rng() as usize % m
        } else {
            let mut t = (rng() as f64 / u64::MAX as f64) * sum;
            let mut p = m - 1;
            for (i, &x) in d2.iter().enumerate() {
                t -= x as f64;
                if t <= 0.0 {
                    p = i;
                    break;
                }
            }
            p
        };
        let c = sample[pick].clone();
        for (i, v) in sample.iter().enumerate() {
            let dd = dist2(v, &c);
            if dd < d2[i] {
                d2[i] = dd;
            }
        }
        centroids.push(c);
    }

    // Refinamiento Lloyd sobre la submuestra (el kernel #1 ya lo acelera 4.78×);
    // corta en cuanto converge.
    for _ in 0..max_iters {
        let mut sums = vec![vec![0.0f64; dim]; k];
        let mut counts = vec![0usize; k];
        for v in &sample {
            let c = nearest(&centroids, v);
            counts[c] += 1;
            for (s, &x) in sums[c].iter_mut().zip(v.iter()) {
                *s += x as f64;
            }
        }
        let mut changed = false;
        for c in 0..k {
            if counts[c] == 0 {
                continue; // cluster vacío: mantener centroide
            }
            let new_c: Vec<f32> = sums[c]
                .iter()
                .map(|&s| (s / counts[c] as f64) as f32)
                .collect();
            if new_c != centroids[c] {
                centroids[c] = new_c;
                changed = true;
            }
        }
        if !changed {
            break; // convergió
        }
    }
    centroids
}

/// Asigna cada vector a su centroide más cercano → listas invertidas
/// (`lists[c]` = índices de los vectores del cluster `c`).
pub fn assign(vectors: &[Vec<f32>], centroids: &[Vec<f32>]) -> Vec<Vec<usize>> {
    let mut lists = vec![Vec::new(); centroids.len()];
    for (i, v) in vectors.iter().enumerate() {
        lists[nearest(centroids, v)].push(i);
    }
    lists
}

/// Búsqueda IVF: escanea los `nprobe` clusters más cercanos a `query` y devuelve
/// los `top_k` índices de vector por distancia exacta (euclídea), ascendente.
/// `nprobe == centroids.len()` ⇒ resultado **exacto** (escanea todo).
pub fn search(
    query: &[f32],
    centroids: &[Vec<f32>],
    lists: &[Vec<usize>],
    vectors: &[Vec<f32>],
    nprobe: usize,
    top_k: usize,
) -> Vec<usize> {
    // Centroides ordenados por cercanía a la query.
    let mut cents: Vec<(f32, usize)> = centroids
        .iter()
        .enumerate()
        .map(|(i, c)| (dist2(c, query), i))
        .collect();
    cents.sort_by(|a, b| a.0.total_cmp(&b.0));

    // Candidatos de los `nprobe` clusters más cercanos, rankeados por distancia exacta.
    let probe = nprobe.clamp(1, cents.len().max(1));
    let mut cands: Vec<(f32, usize)> = Vec::new();
    for &(_, ci) in cents.iter().take(probe) {
        for &idx in &lists[ci] {
            cands.push((dist2(&vectors[idx], query), idx));
        }
    }
    cands.sort_by(|a, b| a.0.total_cmp(&b.0));
    cands.truncate(top_k);
    cands.into_iter().map(|(_, idx)| idx).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dos blobs bien separados: cluster A en torno a (0,0), cluster B en (10,10).
    fn two_blobs() -> Vec<Vec<f32>> {
        vec![
            vec![0.0, 0.0],   // 0  A
            vec![0.1, -0.1],  // 1  A
            vec![-0.1, 0.2],  // 2  A
            vec![10.0, 10.0], // 3  B
            vec![9.9, 10.1],  // 4  B
            vec![10.2, 9.8],  // 5  B
        ]
    }

    #[test]
    fn train_finds_the_clusters() {
        let v = two_blobs();
        let cents = train(&v, 2, 25);
        assert_eq!(cents.len(), 2);
        // Cada blob cae bajo un único centroide.
        let lists = assign(&v, &cents);
        let mut sizes: Vec<usize> = lists.iter().map(|l| l.len()).collect();
        sizes.sort_unstable();
        assert_eq!(sizes, vec![3, 3]);
    }

    #[test]
    fn train_is_deterministic() {
        let v = two_blobs();
        assert_eq!(train(&v, 2, 25), train(&v, 2, 25));
    }

    #[test]
    fn search_finds_nearest_in_probed_cluster() {
        let v = two_blobs();
        let cents = train(&v, 2, 25);
        let lists = assign(&v, &cents);
        // Query cerca del blob A: con nprobe=1 solo se escanea su cluster.
        let near_a = search(&[0.05, 0.05], &cents, &lists, &v, 1, 2);
        assert!(
            near_a.iter().all(|&i| i < 3),
            "deberían ser del blob A: {near_a:?}"
        );
        assert_eq!(near_a.len(), 2);
    }

    #[test]
    fn full_probe_equals_brute_force() {
        let v = two_blobs();
        let cents = train(&v, 2, 25);
        let lists = assign(&v, &cents);
        let q = [9.95, 10.0];
        // nprobe = k ⇒ exacto. El vecino 1 es el índice 3 (10,10) o 4 (9.9,10.1).
        let ivf = search(&q, &cents, &lists, &v, cents.len(), 1);
        // Fuerza bruta: el más cercano a q.
        let brute = (0..v.len())
            .min_by(|&i, &j| dist2(&v[i], &q).total_cmp(&dist2(&v[j], &q)))
            .unwrap();
        assert_eq!(ivf, vec![brute]);
    }

    /// Red de seguridad de #2: con submuestreo + k-means++, el recall ANN sobre
    /// datos agrupados debe seguir siendo alto. 40 clusters naturales × 75 puntos;
    /// recall@10 con nprobe=4 vs fuerza bruta. Determinista (datos sembrados).
    #[test]
    fn recall_is_high_on_clustered_data() {
        let (nc, per, dim) = (40usize, 75usize, 16usize);
        let mut st = 0x1234_5678u64;
        let mut rnd = || {
            st ^= st << 13;
            st ^= st >> 7;
            st ^= st << 17;
            (st >> 40) as f32 / (1u64 << 24) as f32
        };
        let centers: Vec<Vec<f32>> = (0..nc)
            .map(|c| (0..dim).map(|d| ((c * 7 + d * 3) % 23) as f32 * 2.0).collect())
            .collect();
        let mut data: Vec<Vec<f32>> = Vec::new();
        for c in &centers {
            for _ in 0..per {
                data.push(c.iter().map(|&x| x + (rnd() - 0.5) * 0.6).collect());
            }
        }
        let cents = train(&data, nc, 25);
        let lists = assign(&data, &cents);
        let k = 10;
        let (mut hits, mut total) = (0usize, 0usize);
        for qi in (0..data.len()).step_by(data.len() / 50) {
            let q = &data[qi];
            let ivf = search(q, &cents, &lists, &data, 4, k);
            let mut bf: Vec<(f32, usize)> =
                (0..data.len()).map(|i| (dist2(&data[i], q), i)).collect();
            bf.sort_by(|a, b| a.0.total_cmp(&b.0));
            let truth: std::collections::HashSet<usize> =
                bf.iter().take(k).map(|&(_, i)| i).collect();
            hits += ivf.iter().filter(|i| truth.contains(i)).count();
            total += k;
        }
        let recall = hits as f64 / total as f64;
        assert!(recall >= 0.80, "recall@{k} = {recall:.3} (esperado ≥ 0.80)");
    }

    /// Micro-bench manual del build: full-batch Lloyd sobre TODOS los n (algoritmo
    /// viejo) vs submuestra + k-means++ (#2). Ambos usan el kernel #1, así que aísla
    /// la mejora algorítmica. Correr con `--ignored bench_train_build --nocapture`.
    #[test]
    #[ignore = "micro-bench manual del build de centroides IVF"]
    fn bench_train_build() {
        use std::time::Instant;
        // n ≫ 256·k para que entre el submuestreo (el caso de escala, como SIFT-1M).
        let (n, k, dim) = (200_000usize, 64usize, 64usize);
        let mut st = 0xABCDu64;
        let mut rnd = || {
            st ^= st << 13;
            st ^= st >> 7;
            st ^= st << 17;
            (st >> 40) as f32 / (1u64 << 24) as f32
        };
        let data: Vec<Vec<f32>> = (0..n)
            .map(|_| (0..dim).map(|_| rnd() * 10.0).collect())
            .collect();
        fn full_batch(vectors: &[Vec<f32>], k: usize, iters: usize) -> Vec<Vec<f32>> {
            let (n, dim) = (vectors.len(), vectors[0].len());
            let mut cents: Vec<Vec<f32>> = (0..k).map(|i| vectors[i * n / k].clone()).collect();
            for _ in 0..iters {
                let mut sums = vec![vec![0.0f64; dim]; k];
                let mut cnt = vec![0usize; k];
                for v in vectors {
                    let c = nearest(&cents, v);
                    cnt[c] += 1;
                    for (s, &x) in sums[c].iter_mut().zip(v) {
                        *s += x as f64;
                    }
                }
                for c in 0..k {
                    if cnt[c] == 0 {
                        continue;
                    }
                    cents[c] = sums[c].iter().map(|&s| (s / cnt[c] as f64) as f32).collect();
                }
            }
            cents
        }
        let t = Instant::now();
        let _ = full_batch(&data, k, 25);
        let t_old = t.elapsed().as_secs_f64();
        let t = Instant::now();
        let _ = train(&data, k, 25);
        let t_new = t.elapsed().as_secs_f64();
        eprintln!(
            "build n={n} k={k} dim={dim}: full-batch {t_old:.3}s · submuestra+k-means++ {t_new:.3}s · {:.1}× más rápido",
            t_old / t_new
        );
    }
}
