//! `snippet()` / `highlight()`: extractos del texto con los términos de la
//! consulta `MATCH` resaltados.
//!
//! Es **puro** (texto + [`Query`] + tokenizer), sin tocar el índice: tokeniza el
//! texto con el mismo tokenizer del índice, marca los tokens que casan algún
//! término **positivo** de la consulta (se ignoran los operandos derechos de
//! `NOT`) y reconstruye el texto preservando lo de en medio (espacios,
//! puntuación). `snippet` además recorta una ventana alrededor de la primera
//! coincidencia. Ver `docs/12-fts.md`.

use super::query::Query;
use super::tokenizer::Tokenizer;

/// Términos crudos a resaltar: las hojas positivas de la consulta. `NOT a b`
/// aporta solo `a`. Cada uno: (texto crudo, ¿es prefijo `term*`?).
fn positive_terms(q: &Query, out: &mut Vec<(String, bool)>) {
    match q {
        Query::Term { text, prefix } => out.push((text.clone(), *prefix)),
        Query::Phrase(words) => out.extend(words.iter().map(|w| (w.clone(), false))),
        Query::Near { terms, .. } => out.extend(terms.iter().map(|t| (t.clone(), false))),
        Query::Column { query, .. } => positive_terms(query, out),
        Query::And(a, b) | Query::Or(a, b) => {
            positive_terms(a, out);
            positive_terms(b, out);
        }
        Query::Not(a, _) => positive_terms(a, out), // solo el lado positivo
    }
}

/// Normaliza los términos positivos con el tokenizer (igual que se indexó), para
/// poder compararlos con los tokens del texto. Un término crudo que tokeniza en
/// varios pierde su flag de prefijo (se trata cada token por separado).
fn highlight_targets(q: &Query, tk: &dyn Tokenizer) -> Vec<(String, bool)> {
    let mut raw = Vec::new();
    positive_terms(q, &mut raw);
    let mut out = Vec::new();
    let mut buf = Vec::new();
    for (text, prefix) in raw {
        buf.clear();
        tk.tokenize(&text, &mut buf);
        if buf.len() == 1 {
            out.push((buf[0].text.clone(), prefix));
        } else {
            out.extend(buf.iter().map(|t| (t.text.clone(), false)));
        }
    }
    out
}

fn token_matches(tok: &str, targets: &[(String, bool)]) -> bool {
    targets.iter().any(|(t, prefix)| {
        if *prefix {
            tok.starts_with(t.as_str())
        } else {
            tok == t
        }
    })
}

/// Envuelve en `open`/`close` cada token de `text` que case un término positivo
/// de `q`. Devuelve el texto completo con el formato original intacto.
pub fn highlight(text: &str, q: &Query, tk: &dyn Tokenizer, open: &str, close: &str) -> String {
    let targets = highlight_targets(q, tk);
    let mut toks = Vec::new();
    tk.tokenize(text, &mut toks);
    let mut out = String::with_capacity(text.len() + 8);
    let mut last = 0;
    for t in &toks {
        if token_matches(&t.text, &targets) {
            out.push_str(&text[last..t.byte_start]);
            out.push_str(open);
            out.push_str(&text[t.byte_start..t.byte_end]);
            out.push_str(close);
            last = t.byte_end;
        }
    }
    out.push_str(&text[last..]);
    out
}

/// Extracto de hasta `max_tokens` tokens centrado en la primera coincidencia, con
/// los términos resaltados y `ellipsis` al recortar por los bordes.
pub fn snippet(
    text: &str,
    q: &Query,
    tk: &dyn Tokenizer,
    open: &str,
    close: &str,
    ellipsis: &str,
    max_tokens: usize,
) -> String {
    let targets = highlight_targets(q, tk);
    let mut toks = Vec::new();
    tk.tokenize(text, &mut toks);
    if toks.is_empty() || max_tokens == 0 {
        return String::new();
    }
    // Ventana: centrada en la primera coincidencia (o el inicio si no hay).
    let center = toks
        .iter()
        .position(|t| token_matches(&t.text, &targets))
        .unwrap_or(0);
    let start = center.saturating_sub(max_tokens / 2);
    let end = (start + max_tokens).min(toks.len());
    let start = end.saturating_sub(max_tokens); // reajusta si tocamos el final
    let to = toks[end - 1].byte_end;

    let mut out = String::new();
    if start > 0 {
        out.push_str(ellipsis);
    }
    let mut last = toks[start].byte_start;
    for t in &toks[start..end] {
        if token_matches(&t.text, &targets) {
            out.push_str(&text[last..t.byte_start]);
            out.push_str(open);
            out.push_str(&text[t.byte_start..t.byte_end]);
            out.push_str(close);
            last = t.byte_end;
        }
    }
    out.push_str(&text[last..to]);
    if end < toks.len() {
        out.push_str(ellipsis);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fts::{UnicodeTokenizer, parse_query};

    fn hl(text: &str, query: &str) -> String {
        let tk = UnicodeTokenizer::new();
        highlight(text, &parse_query(query).unwrap(), &tk, "[", "]")
    }

    #[test]
    fn highlight_basic_and_multiple() {
        assert_eq!(hl("el mundo es grande", "mundo"), "el [mundo] es grande");
        assert_eq!(
            hl("el mundo es un mundo", "mundo"),
            "el [mundo] es un [mundo]"
        );
        assert_eq!(hl("mundo grande", "mundo AND grande"), "[mundo] [grande]");
    }

    #[test]
    fn highlight_prefix_and_diacritics() {
        // Prefijo: `mun*` resalta `mundo`.
        assert_eq!(hl("hola mundo", "mun*"), "hola [mundo]");
        // El término normaliza (cafe) y casa el original con acento (Café).
        assert_eq!(hl("un Café aquí", "cafe"), "un [Café] aquí");
    }

    #[test]
    fn highlight_ignores_not_branch() {
        // `mundo NOT grande`: solo se resalta el lado positivo.
        assert_eq!(
            hl("el mundo grande", "mundo NOT grande"),
            "el [mundo] grande"
        );
    }

    #[test]
    fn highlight_preserves_unmatched_text() {
        assert_eq!(hl("nada que ver", "ausente"), "nada que ver");
    }

    #[test]
    fn snippet_windows_around_first_match_with_ellipsis() {
        let tk = UnicodeTokenizer::new();
        let text = "uno dos tres cuatro objetivo seis siete ocho nueve diez";
        let q = parse_query("objetivo").unwrap();
        let s = snippet(text, &q, &tk, "[", "]", "…", 4);
        // Ventana de 4 tokens centrada en "objetivo", recortada por ambos lados.
        assert!(s.contains("[objetivo]"), "snippet: {s}");
        assert!(s.starts_with('…') && s.ends_with('…'), "snippet: {s}");
        assert!(!s.contains("uno") && !s.contains("diez"), "snippet: {s}");
    }

    #[test]
    fn snippet_short_text_no_ellipsis() {
        let tk = UnicodeTokenizer::new();
        let q = parse_query("mundo").unwrap();
        let s = snippet("hola mundo", &q, &tk, "[", "]", "…", 10);
        assert_eq!(s, "hola [mundo]");
    }
}
