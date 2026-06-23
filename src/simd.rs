//! EXPERIMENTAL (rama `experiment/simd`): kernel de distancia SIMD escrito a MANO
//! con AVX2+FMA, para MEDIR cuánto gana frente a la autovectorización (#1). Es el
//! ÚNICO módulo con `unsafe` (aislado con `#[allow]`); el resto del crate sigue en
//! `deny(unsafe)`. Si se adoptara, sería este módulo auditado tras relajar `forbid`
//! a `deny`. El dispatcher detecta AVX2+FMA en runtime y cae a la autovec si no hay.
#![allow(unsafe_code)]

/// `‖a − b‖₂²`. Runtime-dispatch a AVX2+FMA si el CPU lo soporta; si no, autovec (#1).
pub fn l2_sq(a: &[f32], b: &[f32]) -> f32 {
    #[cfg(target_arch = "x86_64")]
    {
        if std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("fma") {
            // SAFETY: avx2+fma comprobados arriba en runtime; `l2_sq_avx2` solo lee
            // `a`/`b` dentro de rango (índices < min(len)).
            return unsafe { l2_sq_avx2(a, b) };
        }
    }
    crate::vector::l2_sq(a, b)
}

/// L2² con AVX2+FMA: 4 acumuladores de 256 bits (32 lanes) para saturar las
/// unidades FMA (la misma idea de multi-acumulador del #1, pero en hardware real).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn l2_sq_avx2(a: &[f32], b: &[f32]) -> f32 {
    use std::arch::x86_64::*;
    let n = a.len().min(b.len());
    let (pa, pb) = (a.as_ptr(), b.as_ptr());
    let mut acc0 = _mm256_setzero_ps();
    let mut acc1 = _mm256_setzero_ps();
    let mut acc2 = _mm256_setzero_ps();
    let mut acc3 = _mm256_setzero_ps();
    let mut i = 0;
    while i + 32 <= n {
        let d0 = _mm256_sub_ps(_mm256_loadu_ps(pa.add(i)), _mm256_loadu_ps(pb.add(i)));
        let d1 = _mm256_sub_ps(_mm256_loadu_ps(pa.add(i + 8)), _mm256_loadu_ps(pb.add(i + 8)));
        let d2 = _mm256_sub_ps(_mm256_loadu_ps(pa.add(i + 16)), _mm256_loadu_ps(pb.add(i + 16)));
        let d3 = _mm256_sub_ps(_mm256_loadu_ps(pa.add(i + 24)), _mm256_loadu_ps(pb.add(i + 24)));
        acc0 = _mm256_fmadd_ps(d0, d0, acc0);
        acc1 = _mm256_fmadd_ps(d1, d1, acc1);
        acc2 = _mm256_fmadd_ps(d2, d2, acc2);
        acc3 = _mm256_fmadd_ps(d3, d3, acc3);
        i += 32;
    }
    while i + 8 <= n {
        let d = _mm256_sub_ps(_mm256_loadu_ps(pa.add(i)), _mm256_loadu_ps(pb.add(i)));
        acc0 = _mm256_fmadd_ps(d, d, acc0);
        i += 8;
    }
    let acc = _mm256_add_ps(_mm256_add_ps(acc0, acc1), _mm256_add_ps(acc2, acc3));
    let mut tmp = [0.0f32; 8];
    _mm256_storeu_ps(tmp.as_mut_ptr(), acc);
    let mut s = tmp.iter().sum::<f32>();
    while i < n {
        let d = a[i] - b[i];
        s += d * d;
        i += 1;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_autovec() {
        // dim 130 ⇒ ejercita el bucle de 32, el de 8 y la cola.
        let a: Vec<f32> = (0..130).map(|i| (i as f32 * 0.7).sin() * 3.0).collect();
        let b: Vec<f32> = (0..130).map(|i| (i as f32 * 0.3).cos() * 2.0).collect();
        let s = l2_sq(&a, &b);
        let r = crate::vector::l2_sq(&a, &b);
        assert!((s - r).abs() / r.max(1.0) < 1e-4, "simd={s} autovec={r}");
    }

    #[test]
    #[ignore = "micro-bench manual: SIMD AVX2 a mano vs autovectorización (#1)"]
    fn bench() {
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
        let iters = 40_000_000u64;
        let t = Instant::now();
        let mut s1 = 0.0f32;
        for _ in 0..iters {
            s1 += crate::vector::l2_sq(black_box(a.as_slice()), black_box(b.as_slice()));
        }
        let t_auto = t.elapsed().as_secs_f64();
        black_box(s1);
        let t = Instant::now();
        let mut s2 = 0.0f32;
        for _ in 0..iters {
            s2 += l2_sq(black_box(a.as_slice()), black_box(b.as_slice()));
        }
        let t_simd = t.elapsed().as_secs_f64();
        black_box(s2);
        eprintln!(
            "l2 dim={dim} x{iters}: autovec(#1) {t_auto:.3}s · AVX2-a-mano {t_simd:.3}s · {:.2}x",
            t_auto / t_simd
        );
    }
}
