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
}
