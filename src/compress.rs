//! Compresión de página tras el `trait Compressor` (M10, D8): backend
//! sustituible, **pure-Rust/auditable** y **opcional** (off por defecto, no
//! engorda el core mínimo). El orden en el pager es comprimir → cifrar → sellar,
//! así que el tag cubre los bytes **finales** y cualquier corrupción se detecta
//! **antes** de descomprimir (estabilidad NO-NEGOCIABLE #1): el descompresor
//! nunca recibe basura. Cada página se comprime **independiente** (#2): un bit
//! malo pierde una página, no el resto.

use crate::format::{put_varint, take_varint};

/// Tag de método que el pager antepone al payload sellado de cada página de datos
/// (M10). Como va **por página**, un backend nuevo es retrocompatible: cada
/// página se decodifica por su propio tag, sin migrar las demás.
pub const METHOD_RAW: u8 = 0;
/// LZSS pelado ([`Lz`]).
pub const METHOD_LZ: u8 = 1;
/// LZSS + etapa de entropía (coder de rango adaptativo) ([`Densa`]).
pub const METHOD_DENSA: u8 = 2;

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

    /// Tag de método ([`METHOD_LZ`], …) que el pager antepone a las páginas de
    /// este backend para luego despacharlas a su `decompress`.
    fn method(&self) -> u8;
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
    fn method(&self) -> u8 {
        METHOD_RAW
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
    fn method(&self) -> u8 {
        METHOD_LZ
    }
}

/// **Densa** — LZSS **+ etapa de entropía sin cabecera** (pure-Rust, sin deps,
/// fiel a D8): el LZSS deja los literales a 1 byte crudo y los códigos de match
/// con layout fijo; un codificador de rango **adaptativo** sobre esa salida
/// exprime sus distribuciones sesgadas. Clave a 4 KiB: el modelo arranca uniforme
/// y se adapta símbolo a símbolo, así que **no paga tabla por página** (la cabecera
/// es lo que hace perder a un Huffman estático en bloques pequeños). Cada página
/// elige entre **crudo**, **LZSS** y **LZSS+rango** el más pequeño (nunca inflar);
/// la salida lleva un sub-tag (0 = LZSS, 1 = LZSS+rango) que el `decompress` deshace.
pub struct Densa;

impl Compressor for Densa {
    fn compress(&self, body: &[u8]) -> Option<Vec<u8>> {
        let lz = lz_compress(body);
        // Sub-tag 0 = solo LZSS. Probamos la etapa de entropía sobre la salida
        // LZSS y la adoptamos solo si (a) es más pequeña y (b) **reconstruye
        // bit a bit**: la auto-verificación hace que un bug del coder jamás pueda
        // corromper una página —en el peor caso se descarta la etapa—.
        let (sub, payload) = match rc_compress(&lz) {
            Some(c) if c.len() < lz.len() && rc_decompress(&c).as_deref() == Some(&lz[..]) => {
                (1u8, c)
            }
            _ => (0u8, lz),
        };
        let mut out = Vec::with_capacity(1 + payload.len());
        out.push(sub);
        out.extend_from_slice(&payload);
        (out.len() < body.len()).then_some(out)
    }

    fn decompress(&self, data: &[u8]) -> Option<Vec<u8>> {
        let (&sub, rest) = data.split_first()?;
        match sub {
            0 => lz_decompress(rest),
            1 => lz_decompress(&rc_decompress(rest)?),
            _ => None,
        }
    }

    fn method(&self) -> u8 {
        METHOD_DENSA
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

// ---------- Codificador de rango adaptativo orden-0 (etapa de entropía) ----------
//
// **Sin cabecera de modelo**: las frecuencias arrancan uniformes y se adaptan
// símbolo a símbolo, así que —a diferencia de un Huffman estático— no se paga una
// tabla por página (justo el coste que hace perder a Huffman en bloques de 4 KiB).
// Codificador de rango de Subbotin (carryless, 32 bits): `low`/`range` se
// renormalizan a bytes; el total del modelo se mantiene < `RC_BOT` para que
// `range / total >= 1` siempre.

const RC_TOP: u32 = 1 << 24;
const RC_BOT: u32 = 1 << 16;
/// Incremento de frecuencia por símbolo observado (adaptación).
const RC_INC: u32 = 24;
/// Reescala el modelo al alcanzar este total (< `RC_BOT`).
const RC_MAX_TOTAL: u32 = 1 << 15;
/// Por debajo de esto la cola de 4 bytes del coder no se amortiza: se omite.
const RC_MIN_INPUT: usize = 32;

/// Modelo de frecuencias adaptativo orden-0 sobre el alfabeto de bytes.
struct RangeModel {
    freq: [u16; 256],
    total: u32,
}

impl RangeModel {
    fn new() -> RangeModel {
        RangeModel {
            freq: [1; 256],
            total: 256,
        }
    }

    /// Frecuencia acumulada de los símbolos `< sym`.
    fn cum(&self, sym: usize) -> u32 {
        self.freq[..sym].iter().map(|&f| f as u32).sum()
    }

    /// Símbolo cuyo intervalo acumulado contiene `target`, con `(cum, freq)`.
    fn find(&self, target: u32) -> (usize, u32, u32) {
        let mut cum = 0u32;
        for (s, &f) in self.freq.iter().enumerate() {
            let f = f as u32;
            if cum + f > target {
                return (s, cum, f);
            }
            cum += f;
        }
        // Inalcanzable con `target < total`; defensa: último símbolo.
        let last = self.freq[255] as u32;
        (255, self.total - last, last)
    }

    fn update(&mut self, sym: usize) {
        self.freq[sym] += RC_INC as u16;
        self.total += RC_INC;
        if self.total >= RC_MAX_TOTAL {
            self.total = 0;
            for f in self.freq.iter_mut() {
                *f = (*f >> 1) | 1; // nunca a cero: todo símbolo sigue codificable
                self.total += *f as u32;
            }
        }
    }
}

/// Comprime `input` con el coder de rango adaptativo. `None` si es demasiado
/// corto para amortizar la cola (4 bytes) o no resulta más pequeño. Cabecera:
/// solo `varint(len)` —el modelo no se serializa, se reconstruye adaptándose—.
fn rc_compress(input: &[u8]) -> Option<Vec<u8>> {
    let n = input.len();
    if n < RC_MIN_INPUT {
        return None;
    }
    let mut out = Vec::with_capacity(n / 2 + 8);
    put_varint(&mut out, n as u64);

    let mut low: u32 = 0;
    let mut range: u32 = 0xFFFF_FFFF;
    let mut model = RangeModel::new();
    for &b in input {
        let sym = b as usize;
        let cum = model.cum(sym);
        let freq = model.freq[sym] as u32;
        range /= model.total;
        low = low.wrapping_add(cum * range);
        range *= freq;
        loop {
            if (low ^ low.wrapping_add(range)) < RC_TOP {
                // byte alto fijado
            } else if range < RC_BOT {
                range = low.wrapping_neg() & (RC_BOT - 1); // underflow: fuerza progreso
            } else {
                break;
            }
            out.push((low >> 24) as u8);
            low <<= 8;
            range <<= 8;
        }
        model.update(sym);
    }
    for _ in 0..4 {
        out.push((low >> 24) as u8);
        low <<= 8;
    }
    (out.len() < n).then_some(out)
}

/// Inverso de [`rc_compress`]. Sin pánico ante flujos mal formados (el byte que
/// falta se lee como 0; la salida se acota a `n`); el tag de integridad valida
/// los bytes antes, esto es defensa en profundidad.
fn rc_decompress(data: &[u8]) -> Option<Vec<u8>> {
    let mut pos = 0usize;
    let n = take_varint(data, &mut pos)? as usize;
    let mut out = Vec::with_capacity(n);

    let mut low: u32 = 0;
    let mut range: u32 = 0xFFFF_FFFF;
    let mut code: u32 = 0;
    let body = &data[pos..];
    let mut bp = 0usize;
    let next = |bp: &mut usize| -> u32 {
        let b = body.get(*bp).copied().unwrap_or(0);
        *bp += 1;
        b as u32
    };
    for _ in 0..4 {
        code = (code << 8) | next(&mut bp);
    }

    let mut model = RangeModel::new();
    while out.len() < n {
        range /= model.total;
        let target = (code.wrapping_sub(low) / range).min(model.total - 1);
        let (sym, cum, freq) = model.find(target);
        low = low.wrapping_add(cum * range);
        range *= freq;
        loop {
            if (low ^ low.wrapping_add(range)) < RC_TOP {
            } else if range < RC_BOT {
                range = low.wrapping_neg() & (RC_BOT - 1);
            } else {
                break;
            }
            code = (code << 8) | next(&mut bp);
            low <<= 8;
            range <<= 8;
        }
        out.push(sym as u8);
        model.update(sym);
    }
    Some(out)
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

    /// Huffman round-trip sobre entradas estructuradas aleatorias: caza bugs de
    /// longitudes/canónico/bitstream que las muestras fijas no tocan.
    #[test]
    fn rc_roundtrip_stress() {
        let mut s = 0x1357_9BDFu32;
        let mut rng = || {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            s
        };
        for _ in 0..4000 {
            let len = (rng() % 4069) as usize;
            // Alfabeto y sesgo variables: desde casi uniforme hasta un símbolo
            // dominante, ejercitando adaptación y reescalado del modelo.
            let alphabet = 1 + (rng() % 200) as usize;
            let buf: Vec<u8> = (0..len)
                .map(|_| (rng() as usize % alphabet) as u8)
                .collect();
            // `None` = no comprimió (entrada corta / no ahorra).
            if let Some(c) = rc_compress(&buf) {
                assert_eq!(
                    rc_decompress(&c).as_deref(),
                    Some(&buf[..]),
                    "len={len} alpha={alphabet}"
                );
            }
        }
    }

    /// Sesgo extremo (un símbolo casi siempre) y una cola larga de raros: fuerza
    /// el reescalado del modelo y reconstruye exacto.
    #[test]
    fn rc_extreme_skew_roundtrips() {
        let mut buf = vec![0u8; 4000];
        for (i, b) in buf.iter_mut().enumerate() {
            if i % 64 == 0 {
                *b = (i % 251) as u8; // raros dispersos entre el símbolo dominante
            }
        }
        let c = rc_compress(&buf).expect("comprime");
        assert_eq!(rc_decompress(&c).as_deref(), Some(&buf[..]));
    }

    /// `rc_decompress` no entra en pánico ante flujos mal formados (lee 0 al
    /// agotarse y acota la salida a `n`).
    #[test]
    fn rc_malformed_is_rejected_not_panic() {
        assert_eq!(rc_decompress(&[]), None); // varint de longitud truncado
        let mut bad = Vec::new();
        put_varint(&mut bad, 100); // promete 100 bytes, sin cuerpo del coder
        // No debe entrar en pánico; produce algún Vec de longitud 100 (la
        // integridad real la valida el tag antes de llegar aquí).
        assert_eq!(rc_decompress(&bad).map(|v| v.len()), Some(100));
    }

    /// El códec combinado `Densa` round-trip sobre las muestras, nunca infla, y
    /// gana o iguala a `Lz` pelado (la etapa de entropía solo se adopta si ayuda).
    #[test]
    fn densa_roundtrip_never_inflates_and_dominates_lz() {
        let lz = Lz;
        let densa = Densa;
        for (i, s) in samples().iter().enumerate() {
            match densa.compress(s) {
                Some(c) => {
                    assert_eq!(densa.decompress(&c).as_deref(), Some(&s[..]), "muestra {i}");
                    assert!(c.len() < s.len(), "muestra {i}: no debe inflar");
                    // Densa ≤ Lz (+1 del sub-tag): nunca peor que el LZSS pelado.
                    if let Some(only_lz) = lz.compress(s) {
                        assert!(
                            c.len() <= only_lz.len() + 1,
                            "muestra {i}: Densa {} debería ≤ Lz {}+1",
                            c.len(),
                            only_lz.len()
                        );
                    }
                }
                None => assert!(
                    lz.compress(s).is_none(),
                    "muestra {i}: si Lz gana, Densa también"
                ),
            }
        }
    }
}
