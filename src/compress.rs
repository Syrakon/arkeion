//! Compresión de página tras el `trait Compressor` (M10, D8): backend
//! sustituible, **pure-Rust/auditable** y **opcional** (off por defecto, no
//! engorda el core mínimo). El orden en el pager es comprimir → cifrar → sellar,
//! así que el tag cubre los bytes **finales** y cualquier corrupción se detecta
//! **antes** de descomprimir (estabilidad NO-NEGOCIABLE #1): el descompresor
//! nunca recibe basura. Cada página se comprime **independiente** (#2): un bit
//! malo pierde una página, no el resto.

use crate::format::{put_varint, take_varint};

/// Transforma el body de una página para ahorrar espacio. Como `CryptoProvider`
/// (D8), el motor habla solo con el trait: el algoritmo es sustituible (incluso
/// por uno certificado europeo) sin tocar formato ni motor.
pub trait Compressor: Send + Sync {
    /// Comprime `body`. `Some(c)` **solo** si `c` es estrictamente más pequeño
    /// (nunca inflar); `None` ⇒ el llamador guarda el body crudo (tag de método
    /// por página, M10). Lo deja el pager decidir entre crudo y comprimido.
    fn compress(&self, body: &[u8]) -> Option<Vec<u8>>;

    /// Inverso de `compress`. `None` si el flujo está mal formado —imposible
    /// sobre datos auténticos, porque el tag se valida antes (M10 #1)—.
    fn decompress(&self, data: &[u8]) -> Option<Vec<u8>>;
}

/// Sin compresión: el modo por defecto (D8, supply-chain mínima). `compress`
/// nunca ahorra, así que todas las páginas se guardan crudas.
pub struct NoCompression;

impl Compressor for NoCompression {
    fn compress(&self, _body: &[u8]) -> Option<Vec<u8>> {
        None
    }
    fn decompress(&self, data: &[u8]) -> Option<Vec<u8>> {
        Some(data.to_vec())
    }
}

/// LZSS hecho a mano (LZ77 con tokens literal/match), pure-Rust y sin deps —
/// fiel a D8, como el parser, el b-tree y el varint. Ventana de 4 KiB, longitud
/// de match 3..273; emparejado por cadenas de hash. Aprieta bien la repetición
/// típica de páginas (claves con prefijo común, valores repetidos, runs de
/// ceros); un backend de más ratio puede entrar luego tras este mismo trait.
pub struct Lz;

impl Compressor for Lz {
    fn compress(&self, body: &[u8]) -> Option<Vec<u8>> {
        let out = lz_compress(body);
        (out.len() < body.len()).then_some(out)
    }
    fn decompress(&self, data: &[u8]) -> Option<Vec<u8>> {
        lz_decompress(data)
    }
}

const MIN_MATCH: usize = 3;
// Longitud del match: el nibble alto del código de 2 bytes lleva 0..14 ⇒
// longitudes 3..17; el valor 15 marca la **forma extendida** (un 3.er byte
// `extra`) ⇒ longitud 18..273. Así un run largo (ceros, valor repetido) usa
// pocos tokens en vez de uno cada 18 bytes.
const MAX_MATCH: usize = 273;
const WINDOW: usize = 4096; // offset cabe en 12 bits
const HASH_BITS: u32 = 13;
const HASH_SIZE: usize = 1 << HASH_BITS;
const MAX_CHAIN: usize = 128; // tope de la cadena de hash (ratio vs velocidad)

/// Hash multiplicativo de 3 bytes a `HASH_BITS` bits.
fn hash3(b: &[u8]) -> usize {
    let v = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
    (v.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize
}

fn lz_compress(input: &[u8]) -> Vec<u8> {
    let n = input.len();
    let mut out = Vec::with_capacity(n / 2 + 16);
    put_varint(&mut out, n as u64); // el descompresor conoce el tamaño destino

    // Cadenas de hash: `head[h]` = última posición con ese hash; `prev[p]` = la
    // anterior con el mismo. `-1` = vacío.
    let mut head = vec![-1i32; HASH_SIZE];
    let mut prev = vec![-1i32; n];

    let mut flags_pos = 0usize; // dónde va el byte de control del grupo actual
    let mut flags = 0u8; // bit i = 1 ⇒ token i es literal
    let mut nflags = 0u8; // tokens emitidos en el grupo actual (0..8)
    let mut pos = 0usize;

    let insert = |pos: usize, head: &mut [i32], prev: &mut [i32]| {
        if pos + MIN_MATCH <= n {
            let h = hash3(&input[pos..]);
            prev[pos] = head[h];
            head[h] = pos as i32;
        }
    };

    while pos < n {
        if nflags == 0 {
            flags_pos = out.len();
            out.push(0); // reservado: se rellena al cerrar el grupo
            flags = 0;
        }

        // Mejor match en la ventana, recorriendo la cadena de hash.
        let (mut best_len, mut best_off) = (0usize, 0usize);
        if pos + MIN_MATCH <= n {
            let max_len = (n - pos).min(MAX_MATCH);
            let mut cand = head[hash3(&input[pos..])];
            let mut chain = 0;
            while cand >= 0 && chain < MAX_CHAIN {
                let c = cand as usize;
                if pos - c > WINDOW {
                    break;
                }
                let mut l = 0;
                while l < max_len && input[c + l] == input[pos + l] {
                    l += 1;
                }
                if l > best_len {
                    best_len = l;
                    best_off = pos - c;
                    if l == max_len {
                        break;
                    }
                }
                cand = prev[c];
                chain += 1;
            }
        }

        if best_len >= MIN_MATCH {
            let lc: u16 = if best_len <= 17 {
                (best_len - MIN_MATCH) as u16
            } else {
                15 // forma extendida: longitud en el byte `extra`
            };
            let code = ((best_off - 1) as u16) | (lc << 12);
            out.extend_from_slice(&code.to_le_bytes());
            if best_len > 17 {
                out.push((best_len - 18) as u8);
            }
            for i in pos..pos + best_len {
                insert(i, &mut head, &mut prev);
            }
            pos += best_len;
        } else {
            flags |= 1 << nflags; // literal
            out.push(input[pos]);
            insert(pos, &mut head, &mut prev);
            pos += 1;
        }

        nflags += 1;
        if nflags == 8 {
            out[flags_pos] = flags;
            nflags = 0;
        }
    }
    if nflags > 0 {
        out[flags_pos] = flags; // cierra el último grupo parcial
    }
    out
}

fn lz_decompress(data: &[u8]) -> Option<Vec<u8>> {
    let mut pos = 0usize;
    let n = take_varint(data, &mut pos)? as usize;
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let ctrl = *data.get(pos)?;
        pos += 1;
        let mut bit = 0u8;
        while bit < 8 && out.len() < n {
            if ctrl & (1 << bit) != 0 {
                out.push(*data.get(pos)?);
                pos += 1;
            } else {
                let code = u16::from_le_bytes([*data.get(pos)?, *data.get(pos + 1)?]);
                pos += 2;
                let off = (code & 0x0FFF) as usize + 1;
                let lc = (code >> 12) as usize;
                let len = if lc < 15 {
                    lc + MIN_MATCH
                } else {
                    let extra = *data.get(pos)?;
                    pos += 1;
                    18 + extra as usize
                };
                if off > out.len() {
                    return None; // referencia fuera de lo ya descomprimido
                }
                let start = out.len() - off;
                for i in 0..len {
                    out.push(out[start + i]); // copia byte a byte: tolera solape (runs)
                }
            }
            bit += 1;
        }
    }
    (out.len() == n).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Entradas representativas: vacío, cortas, muy repetitivas, runs de ceros,
    /// texto realista y aleatorio incompresible.
    fn samples() -> Vec<Vec<u8>> {
        let mut v: Vec<Vec<u8>> = vec![
            vec![],
            vec![0],
            vec![7, 7],
            vec![0u8; 4068],                                  // todo ceros
            b"abcabcabcabcabcabcabcabcabcabcabcabc".to_vec(), // periódico
            std::iter::repeat_n(b"fila de ejemplo, ", 200)
                .flatten()
                .copied()
                .collect(),
        ];
        // Página realista: claves con prefijo + un valor repetido + cola de ceros.
        let mut page = Vec::new();
        for i in 0..120u32 {
            page.extend_from_slice(format!("usuario:{i:06}=").as_bytes());
            page.extend_from_slice(b"valor por defecto compartido;");
        }
        page.resize(4068, 0);
        v.push(page);
        // Aleatorio determinista (incompresible).
        let mut rnd = vec![0u8; 4068];
        let mut s = 0x1234_5678u32;
        for b in rnd.iter_mut() {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            *b = s as u8;
        }
        v.push(rnd);
        v
    }

    #[test]
    fn lz_roundtrip_stress() {
        // Entradas estructuradas (runs de ceros, runs de un byte, regiones
        // aleatorias) de longitudes variadas: caza bugs de round-trip que las
        // muestras fijas no tocan (p. ej. páginas de commit con relleno a ceros).
        let mut s = 0x9E37_79B1u32;
        let mut rng = || {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            s
        };
        for _ in 0..3000 {
            let len = (rng() % 4069) as usize;
            let mut buf = vec![0u8; len];
            let mut i = 0;
            while i < len {
                let run = 1 + (rng() % 40) as usize;
                let end = (i + run).min(len);
                match rng() % 3 {
                    0 => {} // run de ceros (ya está)
                    1 => {
                        let b = rng() as u8;
                        buf[i..end].fill(b);
                    }
                    _ => {
                        for x in &mut buf[i..end] {
                            *x = rng() as u8;
                        }
                    }
                }
                i = end;
            }
            let c = lz_compress(&buf);
            assert_eq!(lz_decompress(&c).as_deref(), Some(&buf[..]), "len={len}");
        }
    }

    #[test]
    fn lz_roundtrip_all_samples() {
        for (i, s) in samples().iter().enumerate() {
            let c = lz_compress(s);
            let back = lz_decompress(&c).unwrap_or_else(|| panic!("muestra {i}: descomprime"));
            assert_eq!(&back, s, "muestra {i}: round-trip");
        }
    }

    #[test]
    fn compressible_shrinks_incompressible_is_none() {
        let lz = Lz;
        // Muy repetitivo ⇒ comprime y mucho.
        let repetitive: Vec<u8> = std::iter::repeat_n(b"fila de ejemplo, ", 200)
            .flatten()
            .copied()
            .collect();
        let c = lz
            .compress(&repetitive)
            .expect("debe comprimir lo repetitivo");
        assert!(
            c.len() < repetitive.len() / 4,
            "ratio pobre: {} de {}",
            c.len(),
            repetitive.len()
        );
        assert_eq!(lz.decompress(&c).unwrap(), repetitive);

        // Aleatorio ⇒ None (nunca inflar): el pager guardaría crudo.
        let mut rnd = vec![0u8; 4068];
        let mut s = 0xC0FF_EE11u32;
        for b in rnd.iter_mut() {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            *b = s as u8;
        }
        assert!(
            lz.compress(&rnd).is_none(),
            "no debe inflar datos aleatorios"
        );
    }

    #[test]
    fn zeros_crush() {
        let zeros = vec![0u8; 4068];
        let c = Lz.compress(&zeros).expect("ceros comprimen");
        assert!(
            c.len() < 100,
            "4 KiB de ceros deberían quedar en ~50 B: {}",
            c.len()
        );
        assert_eq!(Lz.decompress(&c).unwrap(), zeros);
    }

    #[test]
    fn no_compression_is_identity_and_never_compresses() {
        let nc = NoCompression;
        assert!(
            nc.compress(b"cualquier cosa repetida repetida repetida")
                .is_none()
        );
        assert_eq!(nc.decompress(b"crudo").unwrap(), b"crudo");
    }

    #[test]
    fn malformed_stream_is_rejected_not_panic() {
        // Cabecera que promete 100 bytes pero el flujo se corta: None, sin pánico.
        let mut bad = Vec::new();
        put_varint(&mut bad, 100);
        bad.push(0xFF); // control: 8 literales, pero no hay bytes que seguir
        assert_eq!(lz_decompress(&bad), None);
        // Varint de longitud truncado.
        assert_eq!(lz_decompress(&[]), None);
    }
}
