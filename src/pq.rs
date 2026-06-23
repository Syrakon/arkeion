//! Product Quantization (PQ) — comprime un vector D-dim en **M códigos de 1 byte**
//! y permite re-rankear por **tablas de lookup (ADC)** en vez de distancia completa.
//!
//! El vector se parte en `M` subvectores; cada subespacio tiene un codebook de
//! `K=256` centroides (entrenados por k-means). Codificar = el índice del centroide
//! más cercano por subespacio ⇒ `M` bytes (vs `D` floats). Para una query se
//! precalcula una tabla `LUT[m][c] = ‖query_sub_m − codebook[m][c]‖²` (256·M); luego
//! la distancia² aproximada de un código es `Σ_m LUT[m][code[m]]` — `M` sumas y
//! lookups, no `D` multiplicaciones. Es lo que hace FAISS IVFPQ: re-rank mucho más
//! rápido **y** índice ~8× más pequeño que guardar el vector int8 entero.
//!
//! El núcleo es puro y **determinista** (reusa `ivf::train`, sembrado). La
//! persistencia/integración con el índice IVF va aparte. Cero dependencias.

/// Centroides por subespacio: código de 1 byte ⇒ 256.
const K: usize = 256;

/// Codebooks PQ entrenados: `m` subespacios, cada uno con ≤`K` centroides de la
/// dimensión de su tramo.
pub struct Pq {
    /// Número de subespacios (= bytes por código).
    pub m: usize,
    /// Dimensión del vector completo.
    pub dim: usize,
    /// Fronteras de los subespacios: el subespacio `s` cubre `[bounds[s], bounds[s+1])`.
    /// Reparto lo más uniforme posible (admite `dim` no divisible por `m`).
    bounds: Vec<usize>,
    /// `codebooks[s]` = centroides del subespacio `s` (cada uno de longitud el tramo).
    codebooks: Vec<Vec<Vec<f32>>>,
}

impl Pq {
    /// Entrena `m` codebooks (un k-means de `K` centroides por subespacio) sobre
    /// `vectors`. Determinista (k-means sembrado). `iters` por subespacio.
    pub fn train(vectors: &[Vec<f32>], m: usize, iters: usize) -> Pq {
        let dim = vectors.first().map(|v| v.len()).unwrap_or(0);
        let m = m.clamp(1, dim.max(1));
        let bounds: Vec<usize> = (0..=m).map(|i| i * dim / m).collect();
        let codebooks: Vec<Vec<Vec<f32>>> = (0..m)
            .map(|s| {
                let (a, b) = (bounds[s], bounds[s + 1]);
                let subs: Vec<Vec<f32>> = vectors.iter().map(|v| v[a..b].to_vec()).collect();
                crate::ivf::train(&subs, K, iters)
            })
            .collect();
        Pq {
            m,
            dim,
            bounds,
            codebooks,
        }
    }

    /// Codifica `v` → `m` bytes: el índice del centroide más cercano en cada
    /// subespacio (distancia L2).
    pub fn encode(&self, v: &[f32]) -> Vec<u8> {
        (0..self.m)
            .map(|s| {
                let sub = &v[self.bounds[s]..self.bounds[s + 1]];
                let cb = &self.codebooks[s];
                let mut best = 0u8;
                let mut bd = f32::INFINITY;
                for (i, c) in cb.iter().enumerate() {
                    let d = crate::vector::l2_sq(sub, c);
                    if d < bd {
                        bd = d;
                        best = i as u8;
                    }
                }
                best
            })
            .collect()
    }

    /// Tabla ADC de una query: `table[s][c] = ‖query_sub_s − codebook[s][c]‖²`. Se
    /// calcula UNA vez por query; luego cada candidato cuesta `m` lookups + sumas.
    pub fn adc_table(&self, query: &[f32]) -> Vec<Vec<f32>> {
        (0..self.m)
            .map(|s| {
                let sub = &query[self.bounds[s]..self.bounds[s + 1]];
                self.codebooks[s]
                    .iter()
                    .map(|c| crate::vector::l2_sq(sub, c))
                    .collect()
            })
            .collect()
    }

    /// Distancia² aproximada (ADC) de un código a la query, vía su tabla.
    pub fn adc_distance(table: &[Vec<f32>], code: &[u8]) -> f32 {
        code.iter()
            .zip(table)
            .map(|(&c, row)| row[c as usize])
            .sum()
    }

    /// Serializa los codebooks: `[m u16][dim u16]` y por subespacio
    /// `[ncentroides u16][centroides f32…]`. Las fronteras se recomputan de `m`/`dim`.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(self.m as u16).to_le_bytes());
        out.extend_from_slice(&(self.dim as u16).to_le_bytes());
        for cb in &self.codebooks {
            out.extend_from_slice(&(cb.len() as u16).to_le_bytes());
            for c in cb {
                for &x in c {
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
        }
        out
    }

    /// Reconstruye los codebooks de `to_bytes`. `None` si el blob está truncado.
    pub fn from_bytes(b: &[u8]) -> Option<Pq> {
        if b.len() < 4 {
            return None;
        }
        let m = u16::from_le_bytes([b[0], b[1]]) as usize;
        let dim = u16::from_le_bytes([b[2], b[3]]) as usize;
        let bounds: Vec<usize> = (0..=m).map(|i| i * dim / m.max(1)).collect();
        let mut pos = 4;
        let mut codebooks = Vec::with_capacity(m);
        for s in 0..m {
            let sub_dim = bounds[s + 1] - bounds[s];
            let nc = u16::from_le_bytes([*b.get(pos)?, *b.get(pos + 1)?]) as usize;
            pos += 2;
            let mut cb = Vec::with_capacity(nc);
            for _ in 0..nc {
                if pos + sub_dim * 4 > b.len() {
                    return None;
                }
                let c: Vec<f32> = (0..sub_dim)
                    .map(|j| {
                        let o = pos + j * 4;
                        f32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
                    })
                    .collect();
                pos += sub_dim * 4;
                cb.push(c);
            }
            codebooks.push(cb);
        }
        Some(Pq {
            m,
            dim,
            bounds,
            codebooks,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Datos agrupados deterministas (clusters separados, jitter sembrado).
    fn clustered(nc: usize, per: usize, dim: usize) -> Vec<Vec<f32>> {
        let mut st = 0x9E37_79B9_7F4A_7C15u64;
        let mut rnd = move || {
            st ^= st << 13;
            st ^= st >> 7;
            st ^= st << 17;
            (st >> 40) as f32 / (1u64 << 24) as f32
        };
        let mut data = Vec::new();
        for c in 0..nc {
            let center: Vec<f32> = (0..dim).map(|d| ((c * 5 + d * 3) % 17) as f32).collect();
            for _ in 0..per {
                data.push(center.iter().map(|&x| x + (rnd() - 0.5) * 0.4).collect());
            }
        }
        data
    }

    #[test]
    fn deterministic() {
        let data = clustered(8, 10, 16);
        let a = Pq::train(&data, 4, 15);
        let b = Pq::train(&data, 4, 15);
        // Mismo codebook ⇒ mismos códigos para todos los vectores.
        for v in &data {
            assert_eq!(a.encode(v), b.encode(v));
        }
    }

    #[test]
    fn adc_matches_exact_distance() {
        let data = clustered(8, 10, 16);
        let pq = Pq::train(&data, 4, 20);
        let q = &data[5];
        let table = pq.adc_table(q);
        // La distancia ADC de un código aproxima la L2 exacta al centroide cuantizado.
        for v in &data {
            let approx = Pq::adc_distance(&table, &pq.encode(v));
            let exact = crate::vector::l2_sq(v, q);
            // Cota generosa: el error de cuantización es pequeño con datos agrupados.
            assert!(
                (approx - exact).abs() <= exact.max(1.0) * 0.5 + 2.0,
                "approx={approx} exact={exact}"
            );
        }
    }

    #[test]
    fn adc_preserves_knn_order() {
        // El re-rank por ADC debe recuperar casi el mismo top-k que la L2 exacta.
        let data = clustered(12, 20, 24);
        let pq = Pq::train(&data, 6, 20);
        let codes: Vec<Vec<u8>> = data.iter().map(|v| pq.encode(v)).collect();
        let k = 10;
        let (mut hits, mut total) = (0usize, 0usize);
        for qi in (0..data.len()).step_by(data.len() / 20) {
            let q = &data[qi];
            // top-k exacto.
            let mut ex: Vec<(f32, usize)> = data
                .iter()
                .enumerate()
                .map(|(i, v)| (crate::vector::l2_sq(v, q), i))
                .collect();
            ex.sort_by(|a, b| a.0.total_cmp(&b.0));
            let truth: std::collections::HashSet<usize> =
                ex.iter().take(k).map(|&(_, i)| i).collect();
            // top-k por ADC.
            let table = pq.adc_table(q);
            let mut ad: Vec<(f32, usize)> = codes
                .iter()
                .enumerate()
                .map(|(i, c)| (Pq::adc_distance(&table, c), i))
                .collect();
            ad.sort_by(|a, b| a.0.total_cmp(&b.0));
            hits += ad.iter().take(k).filter(|(_, i)| truth.contains(i)).count();
            total += k;
        }
        let recall = hits as f64 / total as f64;
        assert!(recall >= 0.80, "recall ADC@{k} = {recall:.3} (≥ 0.80)");
    }

    #[test]
    fn serialize_roundtrip() {
        let data = clustered(8, 10, 16);
        let pq = Pq::train(&data, 4, 15);
        let pq2 = Pq::from_bytes(&pq.to_bytes()).expect("deserializa");
        assert_eq!(pq2.m, pq.m);
        assert_eq!(pq2.dim, pq.dim);
        for v in &data {
            assert_eq!(pq.encode(v), pq2.encode(v));
            assert_eq!(pq.adc_table(v), pq2.adc_table(v));
        }
    }
}
