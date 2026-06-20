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

/// Distancia euclídea al cuadrado (basta para ordenar; evita la raíz).
fn dist2(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(&x, &y)| (x - y) * (x - y)).sum()
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
    // Init: vectores equiespaciados en el orden de entrada (determinista).
    let mut centroids: Vec<Vec<f32>> = (0..k).map(|i| vectors[i * n / k].clone()).collect();

    for _ in 0..max_iters {
        let mut sums = vec![vec![0.0f64; dim]; k];
        let mut counts = vec![0usize; k];
        for v in vectors {
            let c = nearest(&centroids, v);
            counts[c] += 1;
            for (s, &x) in sums[c].iter_mut().zip(v) {
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
}
