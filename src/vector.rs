//! Búsqueda vectorial — KNN exacto.
//!
//! Un vector = **BLOB de `f32` little-endian**. Aquí viven solo las operaciones
//! puras (empaquetado y distancias); el KNN es `ORDER BY <distancia> LIMIT k` en
//! SQL normal (full scan exacto). Cero dependencias. Ver `docs/13-vectores.md`.
//!
//! Las distancias acumulan en `f64` por precisión. `cosine_distance` y
//! `l2_distance` ordenan de menor (más parecido) a mayor; `dot` al revés (mayor =
//! más parecido).

use crate::error::{Error, Result};

/// Empaqueta floats como un BLOB de `f32` little-endian (el constructor `vector()`).
pub fn pack_f32(vals: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vals.len() * 4);
    for &v in vals {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Desempaqueta un BLOB de `f32` little-endian. Error si la longitud no es
/// múltiplo de 4.
fn unpack(b: &[u8]) -> Result<Vec<f32>> {
    if !b.len().is_multiple_of(4) {
        return Err(Error::InvalidInput(
            "BLOB de vector con longitud no múltiplo de 4",
        ));
    }
    Ok(b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

/// Desempaqueta ambos vectores y valida que tengan la misma dimensión.
fn pair(a: &[u8], b: &[u8]) -> Result<(Vec<f32>, Vec<f32>)> {
    let (va, vb) = (unpack(a)?, unpack(b)?);
    if va.len() != vb.len() {
        return Err(Error::InvalidInput("vectores de distinta dimensión"));
    }
    Ok((va, vb))
}

fn dot_f(a: &[f32], b: &[f32]) -> f64 {
    a.iter().zip(b).map(|(&x, &y)| x as f64 * y as f64).sum()
}

/// Producto interno `a·b` (mayor = más parecido).
pub fn dot(a: &[u8], b: &[u8]) -> Result<f64> {
    let (va, vb) = pair(a, b)?;
    Ok(dot_f(&va, &vb))
}

/// Distancia euclídea `‖a − b‖₂` (menor = más parecido).
pub fn l2_distance(a: &[u8], b: &[u8]) -> Result<f64> {
    let (va, vb) = pair(a, b)?;
    let s: f64 = va
        .iter()
        .zip(&vb)
        .map(|(&x, &y)| {
            let d = x as f64 - y as f64;
            d * d
        })
        .sum();
    Ok(s.sqrt())
}

/// Distancia coseno `1 − cos(a, b)` ∈ [0, 2] (menor = más parecido). Si alguno
/// tiene norma 0 (indefinido) devuelve `1.0`.
pub fn cosine_distance(a: &[u8], b: &[u8]) -> Result<f64> {
    let (va, vb) = pair(a, b)?;
    let na = dot_f(&va, &va).sqrt();
    let nb = dot_f(&vb, &vb).sqrt();
    if na == 0.0 || nb == 0.0 {
        return Ok(1.0);
    }
    Ok(1.0 - dot_f(&va, &vb) / (na * nb))
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
        // 5 bytes no es múltiplo de 4.
        assert!(matches!(
            l2_distance(&[0u8; 5], &[0u8; 5]),
            Err(Error::InvalidInput(_))
        ));
    }
}
