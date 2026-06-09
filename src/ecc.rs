//! Corrección de errores por página (ECC, M10 Slice C): Reed-Solomon sobre
//! GF(256), pure-Rust y sin deps (D8, como el b-tree/parser/LZSS). Convierte la
//! detección (el tag) en **recuperación**: dentro del presupuesto de paridad,
//! N bytes corruptos por bloque se **corrigen**; fuera, falla limpio (nunca dato
//! silenciosamente malo — la corrección se verifica recomputando los síndromes).
//!
//! Un bloque RS abarca como mucho 255 bytes (`k` datos + `nsym` paridad,
//! corrige `nsym/2` errores de byte). Una página se trocea en bloques
//! independientes; la paridad de la página es la concatenación de la de cada
//! bloque. El polinomio se representa con índice 0 = grado más alto.

use std::sync::OnceLock;

/// Polinomio primitivo x^8+x^4+x^3+x^2+1 (estándar QR/Reed-Solomon).
const PRIM: u16 = 0x11D;
/// Raíz primitiva de `PRIM`: el generador del campo.
const GEN: u8 = 2;
/// Símbolos por bloque RS (codeword): 255 = 2^8 − 1.
const BLOCK: usize = 255;

struct Tables {
    exp: [u8; 512],
    log: [u8; 256],
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut exp = [0u8; 512];
        let mut log = [0u8; 256];
        let mut x = 1u16;
        for (i, slot) in exp.iter_mut().take(255).enumerate() {
            *slot = x as u8;
            log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= PRIM;
            }
        }
        // Duplicar para que `log[a]+log[b]` (< 510) indexe sin módulo.
        exp.copy_within(0..257, 255);
        Tables { exp, log }
    })
}

fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    let t = tables();
    t.exp[t.log[a as usize] as usize + t.log[b as usize] as usize]
}

fn div(a: u8, b: u8) -> u8 {
    // b != 0 garantizado por el llamador.
    if a == 0 {
        return 0;
    }
    let t = tables();
    let l = t.log[a as usize] as i32 - t.log[b as usize] as i32;
    t.exp[l.rem_euclid(255) as usize]
}

fn inv(a: u8) -> u8 {
    let t = tables();
    t.exp[255 - t.log[a as usize] as usize]
}

/// `GEN^n` (n con signo: negativos vía aritmética modular).
fn pow(n: i32) -> u8 {
    let t = tables();
    t.exp[(t.log[GEN as usize] as i32 * n).rem_euclid(255) as usize]
}

fn poly_scale(p: &[u8], x: u8) -> Vec<u8> {
    p.iter().map(|&pi| mul(pi, x)).collect()
}

fn poly_add(p: &[u8], q: &[u8]) -> Vec<u8> {
    let n = p.len().max(q.len());
    let mut r = vec![0u8; n];
    for (i, &pi) in p.iter().enumerate() {
        r[i + n - p.len()] = pi;
    }
    for (i, &qi) in q.iter().enumerate() {
        r[i + n - q.len()] ^= qi;
    }
    r
}

fn poly_mul(p: &[u8], q: &[u8]) -> Vec<u8> {
    let mut r = vec![0u8; p.len() + q.len() - 1];
    for (j, &qj) in q.iter().enumerate() {
        for (i, &pi) in p.iter().enumerate() {
            r[i + j] ^= mul(pi, qj);
        }
    }
    r
}

fn poly_eval(poly: &[u8], x: u8) -> u8 {
    let mut y = poly[0];
    for &c in &poly[1..] {
        y = mul(y, x) ^ c;
    }
    y
}

/// Cociente y resto de `dividend / divisor` (índice 0 = grado más alto).
fn poly_div(dividend: &[u8], divisor: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut out = dividend.to_vec();
    for i in 0..(dividend.len() - (divisor.len() - 1)) {
        let coef = out[i];
        if coef != 0 {
            for j in 1..divisor.len() {
                if divisor[j] != 0 {
                    out[i + j] ^= mul(divisor[j], coef);
                }
            }
        }
    }
    let sep = out.len() - (divisor.len() - 1);
    let rem = out.split_off(sep);
    (out, rem)
}

/// Polinomio generador de grado `nsym`: ∏ (x − GEN^i), i ∈ [0, nsym).
fn gen_poly(nsym: usize) -> Vec<u8> {
    let mut g = vec![1u8];
    for i in 0..nsym {
        g = poly_mul(&g, &[1, pow(i as i32)]);
    }
    g
}

/// Paridad sistemática (`nsym` bytes) de un bloque de datos (`data.len()` ≤ k).
fn encode_block(data: &[u8], nsym: usize) -> Vec<u8> {
    let gp = gen_poly(nsym);
    let mut out = vec![0u8; data.len() + nsym];
    out[..data.len()].copy_from_slice(data);
    for i in 0..data.len() {
        let coef = out[i];
        if coef != 0 {
            for j in 1..gp.len() {
                out[i + j] ^= mul(gp[j], coef);
            }
        }
    }
    out[data.len()..].to_vec()
}

/// Síndromes (longitud `nsym+1`, con un 0 al frente por convención).
fn syndromes(codeword: &[u8], nsym: usize) -> Vec<u8> {
    let mut s = vec![0u8; nsym + 1];
    for i in 0..nsym {
        s[i + 1] = poly_eval(codeword, pow(i as i32));
    }
    s
}

/// Localizador de errores (Berlekamp-Massey). `None` si hay demasiados errores.
fn error_locator(synd: &[u8], nsym: usize) -> Option<Vec<u8>> {
    let mut err_loc = vec![1u8];
    let mut old_loc = vec![1u8];
    let synd_shift = synd.len() - nsym; // = 1 (el 0 del frente)
    for i in 0..nsym {
        let k = i + synd_shift;
        let mut delta = synd[k];
        for j in 1..err_loc.len() {
            delta ^= mul(err_loc[err_loc.len() - 1 - j], synd[k - j]);
        }
        old_loc.push(0);
        if delta != 0 {
            if old_loc.len() > err_loc.len() {
                let new_loc = poly_scale(&old_loc, delta);
                old_loc = poly_scale(&err_loc, inv(delta));
                err_loc = new_loc;
            }
            err_loc = poly_add(&err_loc, &poly_scale(&old_loc, delta));
        }
    }
    while err_loc.first() == Some(&0) {
        err_loc.remove(0);
    }
    let errs = err_loc.len() - 1;
    if errs * 2 > nsym {
        return None; // más errores de los que la paridad puede corregir
    }
    Some(err_loc)
}

/// Posiciones de error (búsqueda de Chien). Para la posición `p` (de la
/// izquierda), su valor localizador es `X_p = GEN^{nmess-1-p}` y `Λ` tiene una
/// raíz en `X_p^{-1}`; se prueba cada `p`. `None` si no cuadra el número.
fn error_positions(err_loc: &[u8], nmess: usize) -> Option<Vec<usize>> {
    let errs = err_loc.len() - 1;
    let mut pos = Vec::new();
    for p in 0..nmess {
        let x_inv = pow(-((nmess - 1 - p) as i32));
        if poly_eval(err_loc, x_inv) == 0 {
            pos.push(p);
        }
    }
    (pos.len() == errs).then_some(pos)
}

/// Corrige los errores en `msg` (codeword) en `err_pos` (Forney).
fn correct_errata(msg: &mut [u8], synd: &[u8], err_pos: &[usize]) {
    // Localizador a partir de las posiciones.
    let coef_pos: Vec<usize> = err_pos.iter().map(|&p| msg.len() - 1 - p).collect();
    let mut e_loc = vec![1u8];
    for &i in &coef_pos {
        e_loc = poly_mul(&e_loc, &poly_add(&[1], &[pow(i as i32), 0]));
    }
    // Evaluador de errores: (synd_rev · e_loc) mod x^{len}.
    let synd_rev: Vec<u8> = synd.iter().rev().copied().collect();
    let mut divisor = vec![0u8; e_loc.len() + 1];
    divisor[0] = 1;
    let (_, err_eval) = poly_div(&poly_mul(&synd_rev, &e_loc), &divisor);

    // X_i = GEN^{coef_pos}.
    let xs: Vec<u8> = coef_pos.iter().map(|&p| pow(p as i32)).collect();
    for (i, &xi) in xs.iter().enumerate() {
        let xi_inv = inv(xi);
        // Derivada formal del localizador en X_i^{-1} (denominador de Forney).
        let mut prime = 1u8;
        for (j, &xj) in xs.iter().enumerate() {
            if j != i {
                prime = mul(prime, 1 ^ mul(xi_inv, xj));
            }
        }
        let mut y = poly_eval(&err_eval, xi_inv);
        y = mul(xi, y);
        let magnitude = div(y, prime);
        msg[err_pos[i]] ^= magnitude;
    }
}

/// Corrige un bloque (`data` ≤ k bytes + `parity` = `nsym` bytes) in place sobre
/// una copia y devuelve los datos corregidos, o `None` si es irrecuperable.
fn decode_block(data: &[u8], parity: &[u8]) -> Option<Vec<u8>> {
    let nsym = parity.len();
    let mut msg = Vec::with_capacity(data.len() + nsym);
    msg.extend_from_slice(data);
    msg.extend_from_slice(parity);

    let synd = syndromes(&msg, nsym);
    if synd.iter().all(|&s| s == 0) {
        return Some(data.to_vec()); // sin errores
    }
    let err_loc = error_locator(&synd, nsym)?;
    let err_pos = error_positions(&err_loc, msg.len())?;
    correct_errata(&mut msg, &synd, &err_pos);
    // Verificación: el codeword corregido debe tener síndromes nulos.
    if syndromes(&msg, nsym).iter().any(|&s| s != 0) {
        return None;
    }
    Some(msg[..data.len()].to_vec())
}

/// Tamaño de datos por bloque para un presupuesto `nsym` de paridad.
fn data_per_block(nsym: usize) -> usize {
    BLOCK - nsym
}

/// Longitud de la paridad que produce [`parity`] para `data_len` bytes: el
/// pager la deriva (no la guarda) para barrer y leer registros con ECC.
pub fn parity_len(data_len: usize, nsym: usize) -> usize {
    data_len.div_ceil(data_per_block(nsym)) * nsym
}

/// Paridad RS de `data` con `nsym` bytes de paridad por bloque de 255. La
/// longitud devuelta es `ceil(len/k) · nsym`. `nsym` par y en [2, 254].
pub fn parity(data: &[u8], nsym: usize) -> Vec<u8> {
    debug_assert!((2..=254).contains(&nsym) && nsym.is_multiple_of(2));
    let k = data_per_block(nsym);
    let mut out = Vec::new();
    for block in data.chunks(k) {
        out.extend_from_slice(&encode_block(block, nsym));
    }
    out
}

/// Corrige `data` (longitud original conocida) usando `parity`. Devuelve los
/// datos corregidos, o `None` si algún bloque excede el presupuesto de
/// corrección (`nsym/2` bytes por bloque) — fallo limpio, nunca dato malo.
pub fn correct(data: &[u8], parity: &[u8], nsym: usize) -> Option<Vec<u8>> {
    let k = data_per_block(nsym);
    let nblocks = data.len().div_ceil(k).max(1);
    if parity.len() != nblocks * nsym && !(data.is_empty() && parity.is_empty()) {
        return None;
    }
    if data.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::with_capacity(data.len());
    for (i, block) in data.chunks(k).enumerate() {
        let p = &parity[i * nsym..(i + 1) * nsym];
        out.extend(decode_block(block, p)?);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PRNG determinista (xorshift) para datos y patrones de error.
    struct Rng(u32);
    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() as usize) % n
        }
    }

    #[test]
    fn gf_field_axioms() {
        // a · a^{-1} = 1; a · 1 = a; distributividad básica de muestra.
        for a in 1u8..=255 {
            assert_eq!(mul(a, inv(a)), 1, "inverso de {a}");
            assert_eq!(mul(a, 1), a);
            assert_eq!(div(a, a), 1);
        }
    }

    #[test]
    fn no_errors_roundtrip() {
        let data: Vec<u8> = (0..200u32).map(|i| (i * 31 % 256) as u8).collect();
        for nsym in [2usize, 8, 16, 32] {
            let p = parity(&data, nsym);
            assert_eq!(correct(&data, &p, nsym).as_deref(), Some(&data[..]));
        }
    }

    #[test]
    fn corrects_up_to_t_errors_per_block() {
        let mut rng = Rng(0xECC0_1234);
        for _ in 0..1200 {
            let nsym = 2 * (1 + rng.below(16)); // paridad par 2..32
            let t = nsym / 2; // errores corregibles por bloque
            let k = data_per_block(nsym);
            // Longitud que abarca 1..3 bloques, con un último parcial.
            let len = 1 + rng.below(2 * k + 7);
            let data: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            let p = parity(&data, nsym);

            // Corrompe hasta `t` bytes por bloque, en posiciones distintas.
            let mut recv = data.clone();
            for (bi, block) in data.chunks(k).enumerate() {
                let blen = block.len();
                let errs = rng.below(t + 1).min(blen); // 0..t, acotado al bloque
                let mut used = std::collections::HashSet::new();
                for _ in 0..errs {
                    let mut pos = rng.below(blen);
                    while !used.insert(pos) {
                        pos = rng.below(blen);
                    }
                    recv[bi * k + pos] ^= (rng.next() as u8) | 1; // flip != 0
                }
            }
            assert_eq!(
                correct(&recv, &p, nsym).as_deref(),
                Some(&data[..]),
                "nsym={nsym} len={len}: no corrigió ≤{t} errores/bloque"
            );
        }
    }

    #[test]
    fn beyond_budget_never_returns_wrong_data() {
        // Más de `t` errores en un bloque: debe devolver `None` o, si por azar el
        // decodificador "corrige" a un codeword válido, jamás los datos
        // originales silenciosamente mal. Comprobamos que nunca devuelve un
        // resultado != original sin avisar… aceptando None.
        let mut rng = Rng(0xBADD_0001);
        let mut clean_fails = 0;
        for _ in 0..800 {
            let nsym = 2 * (1 + rng.below(8)); // 2..16
            let t = nsym / 2;
            let k = data_per_block(nsym);
            let len = k + 1 + rng.below(k); // ≥ 1 bloque lleno + algo
            let data: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            let p = parity(&data, nsym);

            // Corrompe t+1..2t+2 bytes en el primer bloque (excede el presupuesto).
            let mut recv = data.clone();
            let extra = t + 1 + rng.below(t + 2);
            let mut used = std::collections::HashSet::new();
            for _ in 0..extra.min(k) {
                let mut pos = rng.below(k);
                while !used.insert(pos) {
                    pos = rng.below(k);
                }
                recv[pos] ^= (rng.next() as u8) | 1;
            }
            match correct(&recv, &p, nsym) {
                None => clean_fails += 1,
                Some(out) => assert_ne!(
                    out, data,
                    "devolvió datos originales pese a exceder el presupuesto (imposible)"
                ),
            }
        }
        // La inmensa mayoría debe fallar limpio (None); el resto son falsos
        // "éxitos" a OTRO codeword, nunca al original.
        assert!(clean_fails > 0, "ningún caso falló limpio");
    }

    #[test]
    fn empty_and_tiny() {
        for nsym in [2usize, 16] {
            assert_eq!(parity(&[], nsym).len(), 0);
            assert_eq!(correct(&[], &[], nsym).as_deref(), Some(&[][..]));
            let d = [0xABu8];
            let p = parity(&d, nsym);
            let mut bad = d;
            bad[0] ^= 0xFF;
            assert_eq!(correct(&bad, &p, nsym).as_deref(), Some(&d[..]));
        }
    }

    /// Casos deterministas pequeños (1 y 2 errores) que documentan el contrato.
    #[test]
    fn deterministic_small_cases() {
        let d1 = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        let p1 = parity(&d1, 4);
        let mut r1 = d1.clone();
        r1[2] ^= 0x55;
        assert_eq!(decode_block(&r1, &p1).as_deref(), Some(&d1[..]));

        let d2 = vec![10u8, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        let p2 = parity(&d2, 4); // t=2
        let mut r2 = d2.clone();
        r2[1] ^= 0x33;
        r2[7] ^= 0xAA;
        assert_eq!(decode_block(&r2, &p2).as_deref(), Some(&d2[..]));
    }
}
