//! Búsqueda vectorial — KNN exacto.
//!
//! Un vector es un BLOB con un byte de **tag** de formato + payload:
//! - `0x00` **f32**: `dim` floats little-endian (4·dim bytes) — `vector()`.
//! - `0x01` **int8** (quantizado): `escala` f32 LE + `dim` bytes int8 — `vector_i8()`.
//!   ~4× menos storage; cada componente ≈ `escala · q`. Pérdida de precisión
//!   pequeña (cuantización simétrica por vector).
//!
//! Las distancias **desempaquetan ambos formatos transparentemente** (un f32 y un
//! int8 se comparan sin problema) y acumulan en `f64`. `cosine_distance` y
//! `l2_distance` ordenan de menor (más parecido) a mayor; `dot` al revés. El KNN
//! es `ORDER BY <distancia> LIMIT k` en SQL normal. Cero dependencias. Ver
//! `docs/13-vectores.md`.

use crate::error::{Error, Result};

const TAG_F32: u8 = 0x00;
const TAG_I8: u8 = 0x01;

/// Empaqueta floats como un BLOB f32 (tag `0x00`). El constructor `vector()`.
pub fn pack_f32(vals: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + vals.len() * 4);
    out.push(TAG_F32);
    for &v in vals {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Empaqueta floats como un BLOB int8 quantizado (tag `0x01`): escala simétrica
/// `max|v|/127` + cada componente redondeado a int8. El constructor `vector_i8()`.
pub fn pack_i8(vals: &[f32]) -> Vec<u8> {
    let max = vals.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
    let scale = max / 127.0;
    let mut out = Vec::with_capacity(1 + 4 + vals.len());
    out.push(TAG_I8);
    out.extend_from_slice(&scale.to_le_bytes());
    for &v in vals {
        let q = if scale > 0.0 {
            (v / scale).round().clamp(-127.0, 127.0) as i8
        } else {
            0
        };
        out.push(q as u8);
    }
    out
}

/// Normaliza a norma 1 (para coseno: el orden por L2 coincide con el de coseno).
/// Un vector nulo se deja igual.
pub fn normalize(v: &[f32]) -> Vec<f32> {
    let norm = (v.iter().map(|&x| x as f64 * x as f64).sum::<f64>()).sqrt() as f32;
    if norm == 0.0 {
        return v.to_vec();
    }
    v.iter().map(|&x| x / norm).collect()
}

/// Desempaqueta cualquier formato (f32 o int8) a `Vec<f32>`. Público para que el
/// índice IVF lea los vectores de las filas.
pub fn to_f32(b: &[u8]) -> Result<Vec<f32>> {
    unpack(b)
}

/// Desempaqueta cualquier formato (f32 o int8) a `Vec<f32>`.
fn unpack(b: &[u8]) -> Result<Vec<f32>> {
    match b.split_first() {
        Some((&TAG_F32, rest)) => {
            if !rest.len().is_multiple_of(4) {
                return Err(Error::InvalidInput("BLOB de vector f32 mal formado"));
            }
            Ok(rest
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect())
        }
        Some((&TAG_I8, rest)) => {
            let scale_bytes = rest
                .get(0..4)
                .ok_or(Error::InvalidInput("BLOB de vector int8 sin escala"))?;
            let scale = f32::from_le_bytes([
                scale_bytes[0],
                scale_bytes[1],
                scale_bytes[2],
                scale_bytes[3],
            ]);
            Ok(rest[4..].iter().map(|&q| q as i8 as f32 * scale).collect())
        }
        _ => Err(Error::InvalidInput("BLOB de vector con tag desconocido")),
    }
}

/// Desempaqueta ambos vectores y valida que tengan la misma dimensión.
fn pair(a: &[u8], b: &[u8]) -> Result<(Vec<f32>, Vec<f32>)> {
    let (va, vb) = (unpack(a)?, unpack(b)?);
    if va.len() != vb.len() {
        return Err(Error::InvalidInput("vectores de distinta dimensión"));
    }
    Ok((va, vb))
}

/// Producto interno `a·b` con **8 acumuladores f32 independientes**. El truco de
/// rendimiento: un `.sum()` mono-acumulador es una cadena de dependencia serial
/// (cada `add` espera al anterior) que el compilador NO puede vectorizar; con 8
/// acumuladores independientes sí emite SIMD (8 lanes a la vez). 2–4× sobre el
/// escalar en KNN/re-rank, que es donde el coseno/L2 se llaman millones de veces.
pub fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let mut acc = [0.0f32; 8];
    let mut ia = a[..n].chunks_exact(8);
    let mut ib = b[..n].chunks_exact(8);
    for (qa, qb) in ia.by_ref().zip(ib.by_ref()) {
        for j in 0..8 {
            acc[j] += qa[j] * qb[j];
        }
    }
    let mut s = acc.iter().sum::<f32>();
    for (x, y) in ia.remainder().iter().zip(ib.remainder()) {
        s += x * y;
    }
    s
}

/// Distancia euclídea al cuadrado `‖a − b‖₂²` con 8 acumuladores f32 (ver
/// [`dot_f32`]). Basta para ordenar (evita la raíz); la usan el IVF (centroides +
/// k-means) y el re-rank de candidatos.
pub fn l2_sq(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let mut acc = [0.0f32; 8];
    let mut ia = a[..n].chunks_exact(8);
    let mut ib = b[..n].chunks_exact(8);
    for (qa, qb) in ia.by_ref().zip(ib.by_ref()) {
        for j in 0..8 {
            let d = qa[j] - qb[j];
            acc[j] += d * d;
        }
    }
    let mut s = acc.iter().sum::<f32>();
    for (x, y) in ia.remainder().iter().zip(ib.remainder()) {
        let d = x - y;
        s += d * d;
    }
    s
}

/// L2² entre un vector **int8 empaquetado** (`pack_i8`: `[TAG_I8, escala(4 LE),
/// int8…]`) y una query f32, **sin desempaquetar a `Vec`** (dequantiza al vuelo)
/// y con 8 acumuladores. Es el camino caliente del re-rank ANN (#3): lee el int8
/// contiguo del posting y NO fetchea la fila. `None` si el blob no es int8 o la
/// dimensión no casa.
pub fn l2_sq_packed_i8(packed: &[u8], query: &[f32]) -> Option<f32> {
    if packed.first() != Some(&TAG_I8) || packed.len() < 5 {
        return None;
    }
    let scale = f32::from_le_bytes([packed[1], packed[2], packed[3], packed[4]]);
    let q = &packed[5..];
    if q.len() != query.len() {
        return None;
    }
    let mut acc = [0.0f32; 8];
    let mut iq = q.chunks_exact(8);
    let mut ie = query.chunks_exact(8);
    for (cq, ce) in iq.by_ref().zip(ie.by_ref()) {
        for j in 0..8 {
            let d = (cq[j] as i8 as f32) * scale - ce[j];
            acc[j] += d * d;
        }
    }
    let mut s = acc.iter().sum::<f32>();
    for (&qb, &e) in iq.remainder().iter().zip(ie.remainder()) {
        let d = (qb as i8 as f32) * scale - e;
        s += d * d;
    }
    Some(s)
}

/// Producto interno `a·b` (mayor = más parecido).
pub fn dot(a: &[u8], b: &[u8]) -> Result<f64> {
    let (va, vb) = pair(a, b)?;
    Ok(dot_f32(&va, &vb) as f64)
}

/// Distancia euclídea `‖a − b‖₂` (menor = más parecido).
pub fn l2_distance(a: &[u8], b: &[u8]) -> Result<f64> {
    let (va, vb) = pair(a, b)?;
    Ok((l2_sq(&va, &vb) as f64).sqrt())
}

/// Distancia coseno `1 − cos(a, b)` ∈ [0, 2] (menor = más parecido). Si alguno
/// tiene norma 0 (indefinido) devuelve `1.0`.
pub fn cosine_distance(a: &[u8], b: &[u8]) -> Result<f64> {
    let (va, vb) = pair(a, b)?;
    let na = dot_f32(&va, &va).sqrt();
    let nb = dot_f32(&vb, &vb).sqrt();
    if na == 0.0 || nb == 0.0 {
        return Ok(1.0);
    }
    Ok(1.0 - (dot_f32(&va, &vb) / (na * nb)) as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(vals: &[f32]) -> Vec<u8> {
        pack_f32(vals)
    }

    #[test]
    fn pack_unpack_roundtrip() {
        let vals = [1.0, -2.5, 0.0, 3.25];
        assert_eq!(unpack(&v(&vals)).unwrap(), vals);
    }

    #[test]
    fn dot_and_l2() {
        assert_eq!(
            dot(&v(&[1.0, 2.0, 3.0]), &v(&[4.0, 5.0, 6.0])).unwrap(),
            32.0
        );
        // ‖(0,0)-(3,4)‖ = 5
        assert_eq!(l2_distance(&v(&[0.0, 0.0]), &v(&[3.0, 4.0])).unwrap(), 5.0);
        assert_eq!(l2_distance(&v(&[1.0, 1.0]), &v(&[1.0, 1.0])).unwrap(), 0.0);
    }

    #[test]
    fn cosine_basics() {
        // Vectores iguales (en dirección) ⇒ distancia 0.
        assert!(
            cosine_distance(&v(&[1.0, 0.0]), &v(&[2.0, 0.0]))
                .unwrap()
                .abs()
                < 1e-6
        );
        // Ortogonales ⇒ 1.
        assert!((cosine_distance(&v(&[1.0, 0.0]), &v(&[0.0, 1.0])).unwrap() - 1.0).abs() < 1e-6);
        // Opuestos ⇒ 2.
        assert!((cosine_distance(&v(&[1.0, 0.0]), &v(&[-1.0, 0.0])).unwrap() - 2.0).abs() < 1e-6);
        // Norma 0 ⇒ 1.0.
        assert_eq!(
            cosine_distance(&v(&[0.0, 0.0]), &v(&[1.0, 1.0])).unwrap(),
            1.0
        );
    }

    #[test]
    fn cosine_orders_by_similarity() {
        // q más cerca de a que de b ⇒ menor distancia a a.
        let q = v(&[1.0, 0.1]);
        let a = v(&[1.0, 0.0]);
        let b = v(&[0.0, 1.0]);
        assert!(cosine_distance(&q, &a).unwrap() < cosine_distance(&q, &b).unwrap());
    }

    #[test]
    fn dimension_mismatch_and_bad_length_error() {
        assert!(matches!(
            dot(&v(&[1.0, 2.0]), &v(&[1.0])),
            Err(Error::InvalidInput(_))
        ));
        // tag f32 (0x00) + 2 bytes de payload: no es múltiplo de 4.
        assert!(matches!(
            l2_distance(&[0x00, 1, 2], &[0x00, 1, 2]),
            Err(Error::InvalidInput(_))
        ));
    }

    #[test]
    fn int8_quantization_roundtrips_and_compares_with_f32() {
        let vals = [0.9, -0.4, 0.1, 0.5];
        // El blob int8 es mucho más corto que el f32 (~4× para dim grande).
        let q = pack_i8(&vals);
        let f = pack_f32(&vals);
        assert!(q.len() < f.len());
        // Desempaqueta aproximando los valores originales.
        let back = unpack(&q).unwrap();
        for (a, b) in back.iter().zip(&vals) {
            assert!((a - b).abs() < 0.02, "{a} vs {b}");
        }
        // Distancias cruzadas (un f32 vs un int8) funcionan y son ≈ las exactas.
        let a = pack_f32(&[1.0, 0.0, 0.0]);
        let b_f = pack_f32(&[0.9, 0.1, 0.0]);
        let b_q = pack_i8(&[0.9, 0.1, 0.0]);
        let exact = cosine_distance(&a, &b_f).unwrap();
        let approx = cosine_distance(&a, &b_q).unwrap();
        assert!((exact - approx).abs() < 0.01, "{exact} vs {approx}");
    }

    /// Micro-bench manual del kernel: escalar (mono-acumulador, lo que el
    /// compilador NO vectoriza por la no-asociatividad del f32) vs `l2_sq` (8
    /// acumuladores → SIMD). Correr: `cargo test --release -- --ignored
    /// bench_l2_kernel --nocapture`.
    #[test]
    #[ignore = "micro-bench manual de la distancia f32 multi-acumulador"]
    fn bench_l2_kernel() {
        use std::hint::black_box;
        use std::time::Instant;
        let dim = 128;
        let mk = |seed: u64| -> Vec<f32> {
            let mut s = seed;
            (0..dim)
                .map(|_| {
                    s ^= s << 13;
                    s ^= s >> 7;
                    s ^= s << 17;
                    (s % 512) as f32 - 256.0
                })
                .collect()
        };
        let (a, b) = (mk(1), mk(2));
        fn scalar(a: &[f32], b: &[f32]) -> f32 {
            a.iter().zip(b).map(|(&x, &y)| (x - y) * (x - y)).sum()
        }
        let iters = 20_000_000u64;
        let t = Instant::now();
        let mut s1 = 0.0f32;
        for _ in 0..iters {
            s1 += scalar(black_box(a.as_slice()), black_box(b.as_slice()));
        }
        let t_scalar = t.elapsed().as_secs_f64();
        black_box(s1);
        let t = Instant::now();
        let mut s2 = 0.0f32;
        for _ in 0..iters {
            s2 += l2_sq(black_box(a.as_slice()), black_box(b.as_slice()));
        }
        let t_multi = t.elapsed().as_secs_f64();
        black_box(s2);
        // Correctitud (una llamada): mismo valor que el escalar salvo redondeo f32.
        let (one_s, one_m) = (scalar(&a, &b), l2_sq(&a, &b));
        assert!((one_s - one_m).abs() / one_s.abs() < 1e-4, "{one_s} vs {one_m}");
        eprintln!(
            "l2 dim={dim} ×{iters}: escalar {t_scalar:.3}s · multi-acum {t_multi:.3}s · speedup {:.2}×",
            t_scalar / t_multi
        );
    }
}
