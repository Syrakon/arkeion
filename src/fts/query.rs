//! Parser del mini-lenguaje de consulta de `MATCH` (estilo FTS5).
//!
//! Convierte el texto de la derecha de `col MATCH '<texto>'` en un árbol
//! [`Query`]. Es **puramente estructural**: los términos se guardan crudos y se
//! tokenizan/normalizan en la evaluación (fase 4) con el tokenizer del índice —
//! así un término como `Café` casa con lo indexado igual que en el resto. Ver
//! `docs/12-fts.md`.
//!
//! Gramática (precedencia de menor a mayor: `OR` < `AND` < `NOT` < primario; la
//! yuxtaposición `a b` es un `AND` implícito):
//!
//! ```text
//! query   := or
//! or      := and ( 'OR' and )*
//! and     := not ( ('AND')? not )*
//! not     := primary ( 'NOT' primary )*
//! primary := '(' or ')'
//!          | 'NEAR' '(' word+ (',' number)? ')'
//!          | word ':' primary        (filtro por columna)
//!          | '"' word* '"'           (frase)
//!          | word '*'?               (término / prefijo)
//! ```
//! `AND`/`OR`/`NOT`/`NEAR` son operadores solo en MAYÚSCULAS (en minúsculas son
//! términos), como en FTS5.

use crate::error::{Error, Result};

/// Árbol de una consulta `MATCH`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Query {
    /// Palabra cruda; `prefix` = llevaba `*` (coincidencia por prefijo). Se
    /// tokeniza en evaluación: si produce varios tokens se trata como frase.
    Term {
        text: String,
        prefix: bool,
    },
    /// Frase `"a b c"`: las palabras deben aparecer adyacentes y en orden.
    Phrase(Vec<String>),
    /// `NEAR(t1 t2 …, k)`: los términos a distancia ≤ `k` entre sí.
    Near {
        terms: Vec<String>,
        distance: u32,
    },
    /// `col: <sub>`: restringe la subconsulta a una columna del índice.
    Column {
        column: String,
        query: Box<Query>,
    },
    And(Box<Query>, Box<Query>),
    Or(Box<Query>, Box<Query>),
    /// `a NOT b` = `a` y no `b`.
    Not(Box<Query>, Box<Query>),
}

/// Distancia por defecto de `NEAR(...)` sin `, k` (igual que FTS5).
const DEFAULT_NEAR: u32 = 10;

/// Parsea el texto de consulta de `MATCH`. Error `Sql` si está mal formado.
pub fn parse_query(input: &str) -> Result<Query> {
    let toks = lex(input)?;
    let mut p = Parser { toks, i: 0 };
    let q = p.or_expr()?;
    if p.i != p.toks.len() {
        return Err(err("token inesperado al final de la consulta MATCH"));
    }
    Ok(q)
}

fn err(msg: &str) -> Error {
    Error::Sql {
        msg: format!("consulta MATCH inválida: {msg}"),
        pos: None,
    }
}

#[derive(Clone, Debug, PartialEq)]
enum QTok {
    Word(String),
    Phrase(String),
    LParen,
    RParen,
    Comma,
    Colon,
    Star,
    And,
    Or,
    Not,
    Near,
}

/// Caracteres que cortan una palabra (además del espacio en blanco).
fn is_special(c: char) -> bool {
    matches!(c, '(' | ')' | ',' | ':' | '*' | '"')
}

fn lex(input: &str) -> Result<Vec<QTok>> {
    let mut toks = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '(' {
            chars.next();
            toks.push(QTok::LParen);
        } else if c == ')' {
            chars.next();
            toks.push(QTok::RParen);
        } else if c == ',' {
            chars.next();
            toks.push(QTok::Comma);
        } else if c == ':' {
            chars.next();
            toks.push(QTok::Colon);
        } else if c == '*' {
            chars.next();
            toks.push(QTok::Star);
        } else if c == '"' {
            chars.next(); // comilla de apertura
            let mut content = String::new();
            let mut closed = false;
            for c in chars.by_ref() {
                if c == '"' {
                    closed = true;
                    break;
                }
                content.push(c);
            }
            if !closed {
                return Err(err("comilla de frase sin cerrar"));
            }
            toks.push(QTok::Phrase(content));
        } else {
            let mut word = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() || is_special(c) {
                    break;
                }
                word.push(c);
                chars.next();
            }
            toks.push(match word.as_str() {
                "AND" => QTok::And,
                "OR" => QTok::Or,
                "NOT" => QTok::Not,
                "NEAR" => QTok::Near,
                _ => QTok::Word(word),
            });
        }
    }
    Ok(toks)
}

struct Parser {
    toks: Vec<QTok>,
    i: usize,
}

impl Parser {
    fn peek(&self) -> Option<QTok> {
        self.toks.get(self.i).cloned()
    }

    fn or_expr(&mut self) -> Result<Query> {
        let mut left = self.and_expr()?;
        while self.peek() == Some(QTok::Or) {
            self.i += 1;
            let right = self.and_expr()?;
            left = Query::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<Query> {
        let mut left = self.not_expr()?;
        loop {
            if self.peek() == Some(QTok::And) {
                self.i += 1;
            } else if !self.starts_primary() {
                break;
            }
            // `AND` explícito o yuxtaposición (otro primario) ⇒ AND.
            let right = self.not_expr()?;
            left = Query::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn not_expr(&mut self) -> Result<Query> {
        let mut left = self.primary()?;
        while self.peek() == Some(QTok::Not) {
            self.i += 1;
            let right = self.primary()?;
            left = Query::Not(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// `true` si lo siguiente puede iniciar un primario (para la yuxtaposición).
    fn starts_primary(&self) -> bool {
        matches!(
            self.peek(),
            Some(QTok::Word(_) | QTok::Phrase(_) | QTok::LParen | QTok::Near)
        )
    }

    fn primary(&mut self) -> Result<Query> {
        match self.peek() {
            Some(QTok::LParen) => {
                self.i += 1;
                let q = self.or_expr()?;
                if self.peek() != Some(QTok::RParen) {
                    return Err(err("falta ')'"));
                }
                self.i += 1;
                Ok(q)
            }
            Some(QTok::Near) => self.near(),
            Some(QTok::Phrase(s)) => {
                self.i += 1;
                Ok(Query::Phrase(
                    s.split_whitespace().map(String::from).collect(),
                ))
            }
            Some(QTok::Word(w)) => {
                self.i += 1;
                // Filtro por columna: `col: <sub>`.
                if self.peek() == Some(QTok::Colon) {
                    self.i += 1;
                    let sub = self.primary()?;
                    return Ok(Query::Column {
                        column: w,
                        query: Box::new(sub),
                    });
                }
                // Prefijo: `term*`.
                let prefix = self.peek() == Some(QTok::Star);
                if prefix {
                    self.i += 1;
                }
                Ok(Query::Term { text: w, prefix })
            }
            _ => Err(err("se esperaba un término")),
        }
    }

    fn near(&mut self) -> Result<Query> {
        self.i += 1; // NEAR
        if self.peek() != Some(QTok::LParen) {
            return Err(err("NEAR requiere '(' "));
        }
        self.i += 1;
        let mut terms = Vec::new();
        while let Some(QTok::Word(w)) = self.peek() {
            terms.push(w);
            self.i += 1;
        }
        if terms.len() < 2 {
            return Err(err("NEAR necesita al menos dos términos"));
        }
        let distance = if self.peek() == Some(QTok::Comma) {
            self.i += 1;
            match self.peek() {
                Some(QTok::Word(w)) => {
                    self.i += 1;
                    w.parse::<u32>()
                        .map_err(|_| err("la distancia de NEAR debe ser un entero"))?
                }
                _ => return Err(err("se esperaba la distancia tras ',' en NEAR")),
            }
        } else {
            DEFAULT_NEAR
        };
        if self.peek() != Some(QTok::RParen) {
            return Err(err("falta ')' en NEAR"));
        }
        self.i += 1;
        Ok(Query::Near { terms, distance })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(t: &str) -> Query {
        Query::Term {
            text: t.into(),
            prefix: false,
        }
    }

    #[test]
    fn single_term_and_prefix() {
        assert_eq!(parse_query("hola").unwrap(), term("hola"));
        assert_eq!(
            parse_query("hol*").unwrap(),
            Query::Term {
                text: "hol".into(),
                prefix: true
            }
        );
    }

    #[test]
    fn implicit_and_juxtaposition() {
        assert_eq!(
            parse_query("foo bar").unwrap(),
            Query::And(Box::new(term("foo")), Box::new(term("bar")))
        );
    }

    #[test]
    fn explicit_operators_and_precedence() {
        // `a OR b c` = a OR (b AND c): AND liga más fuerte que OR.
        assert_eq!(
            parse_query("a OR b c").unwrap(),
            Query::Or(
                Box::new(term("a")),
                Box::new(Query::And(Box::new(term("b")), Box::new(term("c"))))
            )
        );
        // `a AND b NOT c` = a AND (b NOT c): NOT liga más fuerte que AND.
        assert_eq!(
            parse_query("a AND b NOT c").unwrap(),
            Query::And(
                Box::new(term("a")),
                Box::new(Query::Not(Box::new(term("b")), Box::new(term("c"))))
            )
        );
    }

    #[test]
    fn parentheses_override_precedence() {
        // `(a OR b) c` = (a OR b) AND c.
        assert_eq!(
            parse_query("(a OR b) c").unwrap(),
            Query::And(
                Box::new(Query::Or(Box::new(term("a")), Box::new(term("b")))),
                Box::new(term("c"))
            )
        );
    }

    #[test]
    fn phrase() {
        assert_eq!(
            parse_query("\"foo bar baz\"").unwrap(),
            Query::Phrase(vec!["foo".into(), "bar".into(), "baz".into()])
        );
    }

    #[test]
    fn near_with_and_without_distance() {
        assert_eq!(
            parse_query("NEAR(foo bar, 5)").unwrap(),
            Query::Near {
                terms: vec!["foo".into(), "bar".into()],
                distance: 5
            }
        );
        assert_eq!(
            parse_query("NEAR(foo bar baz)").unwrap(),
            Query::Near {
                terms: vec!["foo".into(), "bar".into(), "baz".into()],
                distance: DEFAULT_NEAR
            }
        );
    }

    #[test]
    fn column_filter() {
        assert_eq!(
            parse_query("subject:urgente").unwrap(),
            Query::Column {
                column: "subject".into(),
                query: Box::new(term("urgente"))
            }
        );
        // El filtro envuelve un primario (aquí un grupo).
        assert_eq!(
            parse_query("body:(a OR b)").unwrap(),
            Query::Column {
                column: "body".into(),
                query: Box::new(Query::Or(Box::new(term("a")), Box::new(term("b"))))
            }
        );
    }

    #[test]
    fn lowercase_keywords_are_terms() {
        // En minúsculas, `and` es un término normal, no el operador.
        assert_eq!(
            parse_query("foo and").unwrap(),
            Query::And(Box::new(term("foo")), Box::new(term("and")))
        );
    }

    #[test]
    fn malformed_queries_error() {
        assert!(parse_query("").is_err()); // vacía
        assert!(parse_query("(a OR b").is_err()); // paréntesis sin cerrar
        assert!(parse_query("\"sin cerrar").is_err()); // comilla sin cerrar
        assert!(parse_query("a OR").is_err()); // operador sin operando
        assert!(parse_query("NEAR(solo)").is_err()); // NEAR con un término
        assert!(parse_query("NEAR(a b, x)").is_err()); // distancia no numérica
        assert!(parse_query(")").is_err()); // basura
    }
}
