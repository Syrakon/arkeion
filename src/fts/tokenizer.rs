//! Tokenizers FTS: convierten texto en términos buscables.
//!
//! Son **deterministas y sin modelo** (nada que ver con un tokenizer de LLM): la
//! misma entrada produce siempre los mismos términos, por eso encajan con la
//! reproducibilidad/auditoría del motor. Regla del repo: solo `std`, sin
//! dependencias de runtime. Ver `docs/12-fts.md`.

/// Un término emitido por un [`Tokenizer`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// Término normalizado tal cual se indexa (minúsculas, sin diacríticos…).
    pub text: String,
    /// Posición ordinal 0-based dentro del campo (para frase y `NEAR`). Los
    /// términos descartados por un filtro **consumen** su posición (deja un
    /// hueco) para no inventar adyacencias en consultas de frase.
    pub position: u32,
    /// Offset de byte del término en el texto **original**, inicio inclusivo
    /// (lo necesitan `snippet`/`highlight` para subrayar el match).
    pub byte_start: usize,
    /// Offset de byte del término en el texto **original**, fin exclusivo.
    pub byte_end: usize,
}

/// Parte un texto en [`Token`]s.
///
/// El **mismo** tokenizer debe usarse al indexar y al consultar: es el contrato
/// que define qué dos cadenas distintas cuentan como «la misma palabra»
/// (`café` ≡ `CAFE`). Por eso se serializa su nombre en el catálogo del índice.
pub trait Tokenizer: Send + Sync {
    /// Tokeniza `text` **añadiendo** los términos a `out` (reusa el buffer del
    /// llamador; no lo vacía antes).
    fn tokenize(&self, text: &str, out: &mut Vec<Token>);

    /// Nombre estable con el que el tokenizer se guarda en el catálogo.
    fn name(&self) -> &str;
}

/// Tokenizer Unicode por defecto.
///
/// Token = run maximal de [`char::is_alphanumeric`] (usa las tablas Unicode de
/// `std`). Normaliza con [`str::to_lowercase`] y, si `fold_diacritics`, pliega
/// diacríticos latinos a ASCII (`café→cafe`, `ñ→n`, `ß→ss`, `œ→oe`…) para que la
/// búsqueda sea robusta entre lenguas europeas.
///
/// Limitación v1: asume entrada **NFC**; secuencias descompuestas (marca
/// combinante separada) tokenizan imperfecto (no hay normalizador Unicode sin
/// dependencia).
#[derive(Clone, Debug)]
pub struct UnicodeTokenizer {
    /// Plegar diacríticos a ASCII (por defecto `true`).
    pub fold_diacritics: bool,
    /// Descartar términos cuya forma normalizada supere este nº de caracteres
    /// (hashes, base64…). `None` = sin límite.
    pub max_token_len: Option<usize>,
}

impl Default for UnicodeTokenizer {
    fn default() -> Self {
        Self {
            fold_diacritics: true,
            max_token_len: None,
        }
    }
}

impl UnicodeTokenizer {
    /// Tokenizer Unicode con los ajustes por defecto.
    pub fn new() -> Self {
        Self::default()
    }

    /// Normaliza el run `text[start..end]` y, si no queda vacío ni excede el
    /// límite, lo añade a `out` con la `position` dada.
    fn emit(&self, text: &str, start: usize, end: usize, position: u32, out: &mut Vec<Token>) {
        let raw = &text[start..end];
        let mut norm = String::with_capacity(end - start);
        for c in raw.chars() {
            for lc in c.to_lowercase() {
                if self.fold_diacritics {
                    fold_lower_into(lc, &mut norm);
                } else {
                    norm.push(lc);
                }
            }
        }
        if norm.is_empty() {
            return;
        }
        if let Some(max) = self.max_token_len
            && norm.chars().count() > max
        {
            return;
        }
        out.push(Token {
            text: norm,
            position,
            byte_start: start,
            byte_end: end,
        });
    }
}

impl Tokenizer for UnicodeTokenizer {
    fn tokenize(&self, text: &str, out: &mut Vec<Token>) {
        let mut position = 0u32;
        let mut run_start: Option<usize> = None;
        let mut run_end = 0usize;
        for (i, c) in text.char_indices() {
            if c.is_alphanumeric() {
                if run_start.is_none() {
                    run_start = Some(i);
                }
                run_end = i + c.len_utf8();
            } else if let Some(start) = run_start.take() {
                self.emit(text, start, run_end, position, out);
                position += 1;
            }
        }
        if let Some(start) = run_start.take() {
            self.emit(text, start, run_end, position, out);
        }
    }

    fn name(&self) -> &str {
        "unicode"
    }
}

/// Tokenizer ASCII: solo `[A-Za-z0-9]`, minúsculas ASCII, sin plegado. Ignora
/// todo lo no-ASCII (lo trata como separador). Más rápido que `unicode` cuando
/// el corpus es ASCII.
#[derive(Clone, Debug, Default)]
pub struct AsciiTokenizer;

impl Tokenizer for AsciiTokenizer {
    fn tokenize(&self, text: &str, out: &mut Vec<Token>) {
        let bytes = text.as_bytes();
        let mut position = 0u32;
        let mut i = 0;
        while i < bytes.len() {
            if !bytes[i].is_ascii_alphanumeric() {
                i += 1;
                continue;
            }
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_alphanumeric() {
                i += 1;
            }
            let norm: String = bytes[start..i]
                .iter()
                .map(|&b| char::from(b.to_ascii_lowercase()))
                .collect();
            out.push(Token {
                text: norm,
                position,
                byte_start: start,
                byte_end: i,
            });
            position += 1;
        }
    }

    fn name(&self) -> &str {
        "ascii"
    }
}

/// Pliega un carácter **ya en minúsculas** a su forma ASCII de búsqueda
/// (best-effort para escrituras latinas europeas). Los caracteres sin mapeo se
/// copian tal cual; algunos se expanden (`ß→ss`, `œ→oe`, `þ→th`).
fn fold_lower_into(c: char, out: &mut String) {
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => out.push('a'),
        'æ' => out.push_str("ae"),
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => out.push('c'),
        'ð' | 'ď' | 'đ' => out.push('d'),
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => out.push('e'),
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => out.push('g'),
        'ĥ' | 'ħ' => out.push('h'),
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => out.push('i'),
        'ĵ' => out.push('j'),
        'ķ' => out.push('k'),
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => out.push('l'),
        'ñ' | 'ń' | 'ņ' | 'ň' | 'ŋ' => out.push('n'),
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => out.push('o'),
        'œ' => out.push_str("oe"),
        'ŕ' | 'ŗ' | 'ř' => out.push('r'),
        'ś' | 'ŝ' | 'ş' | 'š' => out.push('s'),
        'ß' => out.push_str("ss"),
        'ţ' | 'ť' | 'ŧ' => out.push('t'),
        'þ' => out.push_str("th"),
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => out.push('u'),
        'ŵ' => out.push('w'),
        'ý' | 'ÿ' | 'ŷ' => out.push('y'),
        'ź' | 'ż' | 'ž' => out.push('z'),
        other => out.push(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Atajo: tokeniza y devuelve solo los textos de los términos.
    fn terms(tk: &dyn Tokenizer, text: &str) -> Vec<String> {
        let mut out = Vec::new();
        tk.tokenize(text, &mut out);
        out.into_iter().map(|t| t.text).collect()
    }

    #[test]
    fn unicode_basico_y_posiciones() {
        let tk = UnicodeTokenizer::new();
        let mut out = Vec::new();
        tk.tokenize("The quick brown fox", &mut out);
        let textos: Vec<_> = out.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(textos, ["the", "quick", "brown", "fox"]);
        let pos: Vec<_> = out.iter().map(|t| t.position).collect();
        assert_eq!(pos, [0, 1, 2, 3]);
    }

    #[test]
    fn case_folding_diacriticos() {
        let tk = UnicodeTokenizer::new();
        // Misma palabra en tres formas → mismo término.
        assert_eq!(terms(&tk, "Café CAFÉ café"), ["cafe", "cafe", "cafe"]);
        // Lenguas europeas variadas.
        assert_eq!(
            terms(&tk, "Niño Köln Łódź Straße œuvre"),
            ["nino", "koln", "lodz", "strasse", "oeuvre"]
        );
    }

    #[test]
    fn separadores_y_numeros() {
        let tk = UnicodeTokenizer::new();
        assert_eq!(terms(&tk, "quick-brown, fox!"), ["quick", "brown", "fox"]);
        assert_eq!(terms(&tk, "abc123  42"), ["abc123", "42"]);
        assert!(terms(&tk, "   \t\n  ").is_empty());
    }

    #[test]
    fn fold_desactivable() {
        let tk = UnicodeTokenizer {
            fold_diacritics: false,
            max_token_len: None,
        };
        assert_eq!(terms(&tk, "Café"), ["café"]);
    }

    #[test]
    fn max_token_len_descarta_y_deja_hueco() {
        let tk = UnicodeTokenizer {
            fold_diacritics: true,
            max_token_len: Some(3),
        };
        let mut out = Vec::new();
        tk.tokenize("ok toolong end", &mut out);
        let textos: Vec<_> = out.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(textos, ["ok", "end"]);
        // El término largo descartado consume su posición (hueco 0,_,2).
        let pos: Vec<_> = out.iter().map(|t| t.position).collect();
        assert_eq!(pos, [0, 2]);
    }

    #[test]
    fn byte_offsets_recortan_el_original() {
        let tk = UnicodeTokenizer::new();
        let text = "Hola, Café del Mañana";
        let mut out = Vec::new();
        tk.tokenize(text, &mut out);
        for t in &out {
            // El slice debe ser válido, no vacío…
            let slice = &text[t.byte_start..t.byte_end];
            assert!(!slice.is_empty());
            // …y re-tokenizarlo debe dar exactamente este término.
            let got = terms(&tk, slice);
            assert_eq!(got.len(), 1);
            assert_eq!(got[0], t.text);
        }
        // Comprobación concreta: el segundo término es "café" y recorta "Café".
        assert_eq!(out[1].text, "cafe");
        assert_eq!(&text[out[1].byte_start..out[1].byte_end], "Café");
    }

    #[test]
    fn normalizacion_idempotente() {
        let tk = UnicodeTokenizer::new();
        let once = terms(&tk, "Köln Straße Niño");
        let twice = terms(&tk, &once.join(" "));
        assert_eq!(once, twice);
    }

    #[test]
    fn ascii_ignora_no_ascii() {
        let tk = AsciiTokenizer;
        // "Café" → run ASCII "Caf" (é y siguientes bytes no-ASCII = separador).
        assert_eq!(terms(&tk, "Café au lait"), ["caf", "au", "lait"]);
        assert_eq!(terms(&tk, "Hello-World 99"), ["hello", "world", "99"]);
    }

    #[test]
    fn ascii_byte_offsets_validos() {
        let tk = AsciiTokenizer;
        let text = "foo, BAR123";
        let mut out = Vec::new();
        tk.tokenize(text, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(&text[out[0].byte_start..out[0].byte_end], "foo");
        assert_eq!(&text[out[1].byte_start..out[1].byte_end], "BAR123");
    }

    #[test]
    fn tokenize_anade_no_vacia_el_buffer() {
        let tk = UnicodeTokenizer::new();
        let mut out = Vec::new();
        tk.tokenize("uno dos", &mut out);
        tk.tokenize("tres", &mut out);
        let textos: Vec<_> = out.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(textos, ["uno", "dos", "tres"]);
    }
}
