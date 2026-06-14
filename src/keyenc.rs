//! Codificación **memcomparable** de valores para claves de índice secundario
//! (M10.5): el orden lexicográfico de los bytes codificados coincide con el
//! orden SQL del valor, y la codificación es **self-delimitada** (un texto de
//! longitud variable no se mezcla con el sufijo `rowid` de la clave de índice).
//!
//! Cada valor empieza por un byte de **presencia**: `0x00` para `NULL` (ordena
//! primero) y `0x01` para no-null, seguido de la codificación del tipo. Dentro de
//! un índice el tipo de columna es fijo (las filas se coercionan), así que el
//! byte de presencia basta —no hace falta un tag de tipo—.

use crate::record::{Value, ValueRef, rowid_be};

/// Añade a `out` la codificación memcomparable de `v` (clave de índice).
pub fn encode_index_value(v: &Value, out: &mut Vec<u8>) {
    encode_index_value_ref(ValueRef::of(v), out);
}

/// Como [`encode_index_value`] pero sobre la vista prestada [`ValueRef`]: el
/// camino sin clones (insert y bulk-load) codifica entradas de índice directo
/// de los valores resueltos. Único sitio con el formato memcomparable.
pub fn encode_index_value_ref(v: ValueRef<'_>, out: &mut Vec<u8>) {
    match v {
        ValueRef::Null => out.push(0x00), // NULL ordena antes que cualquier no-null
        ValueRef::Integer(n) => {
            out.push(0x01);
            out.extend_from_slice(&rowid_be(n)); // i64 BE con bit de signo invertido
        }
        ValueRef::Real(f) => {
            out.push(0x01);
            out.extend_from_slice(&real_be(f));
        }
        ValueRef::Bool(b) => {
            out.push(0x01);
            out.push(u8::from(b)); // false(0) < true(1)
        }
        ValueRef::Text(s) => {
            out.push(0x01);
            encode_bytes(s.as_bytes(), out);
        }
        ValueRef::Blob(b) => {
            out.push(0x01);
            encode_bytes(b, out);
        }
    }
}

/// f64 → 8 bytes BE order-preserving: si el bit de signo está puesto (negativo),
/// invertir **todos** los bits; si no, poner el bit de signo. Así los negativos
/// caen por debajo de los positivos y, entre negativos, el de mayor magnitud va
/// primero. Ordena bien ±0 y los finitos; `NaN` cae de forma consistente al final.
fn real_be(f: f64) -> [u8; 8] {
    let bits = f.to_bits();
    let transformed = if bits & 0x8000_0000_0000_0000 != 0 {
        !bits
    } else {
        bits | 0x8000_0000_0000_0000
    };
    transformed.to_be_bytes()
}

/// Bytes escapados + terminador, order-preserving para longitud variable: cada
/// `0x00` del dato se escapa a `0x00 0xFF`, y la cadena termina en `0x00 0x00`.
/// Como el terminador es menor que cualquier `0x00 0xFF` (continuación) y que
/// cualquier byte de dato, un prefijo ordena antes que su extensión
/// (`"ab" < "abc"`).
fn encode_bytes(b: &[u8], out: &mut Vec<u8>) {
    for &byte in b {
        out.push(byte);
        if byte == 0x00 {
            out.push(0xFF);
        }
    }
    out.push(0x00);
    out.push(0x00);
}

/// Entero `i64` **order-preserving**, **self-delimitado** y de **longitud
/// variable**: las magnitudes pequeñas ocupan menos bytes y el orden
/// lexicográfico de los bytes coincide con el orden numérico. Reemplaza los
/// `table_id`/`rowid` de ancho fijo de la clave de fila (`[0x01][tid][rowid]`,
/// 4+8 B) por ~2–5 B típicos sin perder el orden del b-tree.
///
/// Un byte de cabecera codifica signo y nº de bytes significativos: los
/// no-negativos usan cabeceras `0x80..=0x88` (por encima de cualquier negativa),
/// los negativos `0x77..=0x7F` (por debajo). Dentro de cada signo, más bytes ⇒
/// mayor magnitud; los negativos llevan los bytes **complementados** para que el
/// más negativo ordene primero. Como ningún código es prefijo de otro, concatenar
/// `enc(tid) ‖ enc(rowid)` ordena por `(tid, rowid)`.
pub fn encode_oint(v: i64, out: &mut Vec<u8>) {
    if v >= 0 {
        let u = v as u64;
        let nbytes = 8 - (u.leading_zeros() / 8) as usize; // 0 si v == 0
        out.push(0x80 | nbytes as u8);
        out.extend_from_slice(&u.to_be_bytes()[8 - nbytes..]);
    } else {
        let c = !(v as u64); // = -v - 1, en 0..=2^63-1; más negativo ⇒ mayor c
        let nbytes = 8 - (c.leading_zeros() / 8) as usize;
        out.push(0x7F - nbytes as u8);
        for &b in &c.to_be_bytes()[8 - nbytes..] {
            out.push(!b); // complementado: mayor c (más negativo) ⇒ bytes menores
        }
    }
}

/// Inverso de [`encode_oint`], avanzando `pos`. `None` si el flujo está mal
/// formado (cabecera fuera de rango o bytes insuficientes).
pub fn decode_oint(buf: &[u8], pos: &mut usize) -> Option<i64> {
    let header = *buf.get(*pos)?;
    *pos += 1;
    let mut arr = [0u8; 8];
    if header >= 0x80 {
        let nbytes = (header - 0x80) as usize;
        if nbytes > 8 {
            return None;
        }
        let bytes = buf.get(*pos..*pos + nbytes)?;
        arr[8 - nbytes..].copy_from_slice(bytes);
        *pos += nbytes;
        Some(u64::from_be_bytes(arr) as i64)
    } else {
        let nbytes = (0x7F - header) as usize;
        if nbytes > 8 {
            return None;
        }
        let bytes = buf.get(*pos..*pos + nbytes)?;
        for (i, &b) in bytes.iter().enumerate() {
            arr[8 - nbytes + i] = !b; // des-complementar los bytes de c
        }
        *pos += nbytes;
        Some(!u64::from_be_bytes(arr) as i64) // v = !c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(v: &Value) -> Vec<u8> {
        let mut out = Vec::new();
        encode_index_value(v, &mut out);
        out
    }

    /// PRNG determinista (xorshift).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    /// El orden de los bytes codificados == el orden SQL, comprobado sobre una
    /// lista ordenada de valores representativos de cada tipo.
    #[test]
    fn order_preserved_known_sequences() {
        // Integers, incluidos límites y signos.
        let ints = [i64::MIN, -1_000_000, -1, 0, 1, 42, 1_000_000, i64::MAX].map(Value::Integer);
        assert_sorted(&ints);

        // Reales finitos.
        let reals = [
            f64::NEG_INFINITY,
            -1e9,
            -1.5,
            -0.0,
            0.0,
            1.5,
            2.5,
            1e9,
            f64::INFINITY,
        ]
        .map(Value::Real);
        assert_sorted(&reals);

        // Bool.
        assert_sorted(&[Value::Bool(false), Value::Bool(true)]);

        // Texto: prefijos, bytes nulos internos, longitudes.
        let texts = ["", "a", "ab", "abc", "b", "z"].map(|s| Value::Text(s.to_owned()));
        assert_sorted(&texts);

        // NULL ordena antes que cualquier no-null.
        assert!(enc(&Value::Null) < enc(&Value::Integer(i64::MIN)));
        assert!(enc(&Value::Null) < enc(&Value::Text(String::new())));
    }

    /// `enc(a) < enc(b)  ⇔  a < b` para pares aleatorios del mismo tipo.
    #[test]
    fn order_preserved_random_pairs() {
        let mut rng = Rng(0x1234_5678_9ABC_DEF0);
        for _ in 0..5000 {
            // Integers
            let (a, b) = (rng.next() as i64, rng.next() as i64);
            assert_eq!(
                enc(&Value::Integer(a)).cmp(&enc(&Value::Integer(b))),
                a.cmp(&b),
                "int {a} vs {b}"
            );

            // Reales finitos (descartar NaN/inf para comparar con <)
            let fa = f64::from_bits(rng.next());
            let fb = f64::from_bits(rng.next());
            if fa.is_finite() && fb.is_finite() {
                assert_eq!(
                    enc(&Value::Real(fa)).cmp(&enc(&Value::Real(fb))),
                    fa.partial_cmp(&fb).unwrap(),
                    "real {fa} vs {fb}"
                );
            }

            // Texto con bytes variados, incluidos 0x00.
            let sa = rand_bytes(&mut rng);
            let sb = rand_bytes(&mut rng);
            assert_eq!(
                encode_bytes_vec(&sa).cmp(&encode_bytes_vec(&sb)),
                sa.cmp(&sb),
                "bytes {sa:?} vs {sb:?}"
            );
        }
    }

    fn encode_bytes_vec(b: &[u8]) -> Vec<u8> {
        enc(&Value::Blob(b.to_vec()))
    }

    fn rand_bytes(rng: &mut Rng) -> Vec<u8> {
        let len = (rng.next() % 6) as usize;
        (0..len).map(|_| (rng.next() % 4) as u8).collect() // bytes en {0,1,2,3}: estresa el 0x00
    }

    /// Comprueba que la secuencia está estrictamente ordenada al codificar.
    fn assert_sorted(values: &[Value]) {
        for w in values.windows(2) {
            assert!(
                enc(&w[0]) < enc(&w[1]),
                "orden roto: {:?} debería ir antes que {:?}",
                w[0],
                w[1]
            );
        }
    }

    fn enc_oint(v: i64) -> Vec<u8> {
        let mut o = Vec::new();
        encode_oint(v, &mut o);
        o
    }

    /// `encode_oint` preserva el orden y es self-delimitado: para una lista de
    /// i64 estrictamente creciente (con todos los bordes), los bytes codificados
    /// también lo están, y `decode_oint` es su inverso exacto.
    #[test]
    fn oint_order_and_roundtrip() {
        let vals = [
            i64::MIN,
            i64::MIN + 1,
            -4_294_967_296,
            -65_537,
            -65_536,
            -257,
            -256,
            -255,
            -2,
            -1,
            0,
            1,
            2,
            127,
            128,
            255,
            256,
            65_535,
            65_536,
            16_777_215,
            16_777_216,
            4_294_967_295,
            4_294_967_296,
            i64::MAX - 1,
            i64::MAX,
        ];
        for w in vals.windows(2) {
            assert!(
                enc_oint(w[0]) < enc_oint(w[1]),
                "orden roto: {} debería ir antes que {}",
                w[0],
                w[1]
            );
        }
        for &v in &vals {
            let e = enc_oint(v);
            let mut pos = 0;
            assert_eq!(decode_oint(&e, &mut pos), Some(v), "round-trip {v}");
            assert_eq!(pos, e.len(), "self-delimitado: consume exacto, {v}");
            assert!(e.len() <= 9, "máximo 9 bytes (cabecera + 8), {v}");
        }
    }

    /// Orden preservado sobre pares aleatorios (incluye el cruce de signo).
    #[test]
    fn oint_order_random_pairs() {
        let mut rng = Rng(0x0BAD_F00D_DEAD_BEEF);
        for _ in 0..20_000 {
            let a = rng.next() as i64;
            let b = rng.next() as i64;
            assert_eq!(
                enc_oint(a).cmp(&enc_oint(b)),
                a.cmp(&b),
                "orden roto: {a} vs {b}"
            );
        }
    }

    /// La concatenación `enc(tid) ‖ enc(rowid)` ordena por `(tid, rowid)` — la
    /// propiedad que necesita la clave de fila del b-tree.
    #[test]
    fn oint_concatenation_orders_lexicographically() {
        let keys = [
            (1u32, 5i64),
            (1, 50_000),
            (1, i64::MAX),
            (2, -1),
            (2, 0),
            (300, 1),
        ];
        let enc_key = |tid: u32, rid: i64| {
            let mut k = Vec::new();
            encode_oint(tid as i64, &mut k);
            encode_oint(rid, &mut k);
            k
        };
        let encoded: Vec<Vec<u8>> = keys.iter().map(|&(t, r)| enc_key(t, r)).collect();
        let sorted = {
            let mut s = encoded.clone();
            s.sort();
            s
        };
        // `keys` ya está en orden (tid, rowid); su codificación debe coincidir
        // con el orden lexicográfico de los bytes.
        assert_eq!(
            encoded, sorted,
            "el orden de bytes debe seguir (tid, rowid)"
        );
    }

    /// `decode_oint` no entra en pánico ante cabeceras/colas mal formadas.
    #[test]
    fn oint_malformed_is_none_not_panic() {
        assert_eq!(decode_oint(&[], &mut 0), None);
        assert_eq!(decode_oint(&[0x88], &mut 0), None); // promete 8 bytes, no hay
        assert_eq!(decode_oint(&[0x83, 0x01], &mut 0), None); // promete 3, hay 1
    }
}
