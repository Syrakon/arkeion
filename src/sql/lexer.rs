//! Lexer SQL escrito a mano: cero dependencias y errores con posición exacta
//! en bytes (docs/04-sql.md).

use crate::error::{Error, Result};

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Ident(String),
    Kw(Kw),
    Int(i64),
    Float(f64),
    Str(String),
    Blob(Vec<u8>),
    /// Parámetro posicional `?N` (1-based).
    Param(usize),
    /// Parámetro nombrado `:nombre` (sin los dos puntos). El parser le asigna un
    /// índice posicional por orden de aparición (reusando los repetidos).
    NamedParam(String),
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    /// `||` — concatenación de texto.
    Concat,
    LParen,
    RParen,
    Comma,
    Semi,
    Dot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kw {
    Add,
    After,
    All,
    Alter,
    And,
    As,
    Asc,
    Before,
    Begin,
    Between,
    Blob,
    Boolean,
    By,
    Cascade,
    Case,
    Cast,
    Column,
    Columns,
    Commit,
    Conflict,
    Create,
    Default,
    Do,
    Delete,
    Desc,
    Distinct,
    Drop,
    Each,
    Else,
    End,
    Except,
    Exists,
    False,
    First,
    For,
    Foreign,
    From,
    Group,
    Having,
    If,
    In,
    Index,
    Inner,
    Insert,
    Instead,
    Integer,
    Intersect,
    Into,
    Is,
    Join,
    Key,
    Left,
    Like,
    Limit,
    Move,
    Not,
    Nothing,
    Null,
    Of,
    Offset,
    On,
    Or,
    Order,
    Over,
    Partition,
    Primary,
    Real,
    Recursive,
    References,
    Rename,
    Reorder,
    Restrict,
    Returning,
    Rollback,
    Row,
    Select,
    Set,
    Statement,
    Table,
    Text,
    Then,
    Timestamp,
    To,
    Trigger,
    True,
    Union,
    Unique,
    Update,
    Values,
    Version,
    View,
    When,
    Where,
    With,
}

fn keyword(word: &str) -> Option<Kw> {
    Some(match word.to_ascii_uppercase().as_str() {
        "ADD" => Kw::Add,
        "AFTER" => Kw::After,
        "ALL" => Kw::All,
        "ALTER" => Kw::Alter,
        "AND" => Kw::And,
        "AS" => Kw::As,
        "ASC" => Kw::Asc,
        "BEFORE" => Kw::Before,
        "BEGIN" => Kw::Begin,
        "BETWEEN" => Kw::Between,
        "BLOB" => Kw::Blob,
        "BOOLEAN" => Kw::Boolean,
        "BY" => Kw::By,
        "CASCADE" => Kw::Cascade,
        "CASE" => Kw::Case,
        "CAST" => Kw::Cast,
        "COLUMN" => Kw::Column,
        "COLUMNS" => Kw::Columns,
        "COMMIT" => Kw::Commit,
        "CONFLICT" => Kw::Conflict,
        "CREATE" => Kw::Create,
        "DEFAULT" => Kw::Default,
        "DO" => Kw::Do,
        "DELETE" => Kw::Delete,
        "DESC" => Kw::Desc,
        "DISTINCT" => Kw::Distinct,
        "DROP" => Kw::Drop,
        "EACH" => Kw::Each,
        "ELSE" => Kw::Else,
        "END" => Kw::End,
        "EXCEPT" => Kw::Except,
        "EXISTS" => Kw::Exists,
        "FALSE" => Kw::False,
        "FIRST" => Kw::First,
        "FOR" => Kw::For,
        "FOREIGN" => Kw::Foreign,
        "FROM" => Kw::From,
        "GROUP" => Kw::Group,
        "HAVING" => Kw::Having,
        "IF" => Kw::If,
        "IN" => Kw::In,
        "INDEX" => Kw::Index,
        "INNER" => Kw::Inner,
        "INSERT" => Kw::Insert,
        "INSTEAD" => Kw::Instead,
        "INTEGER" => Kw::Integer,
        "INTERSECT" => Kw::Intersect,
        "INTO" => Kw::Into,
        "IS" => Kw::Is,
        "JOIN" => Kw::Join,
        "KEY" => Kw::Key,
        "LEFT" => Kw::Left,
        "LIKE" => Kw::Like,
        "LIMIT" => Kw::Limit,
        "MOVE" => Kw::Move,
        "NOT" => Kw::Not,
        "NOTHING" => Kw::Nothing,
        "NULL" => Kw::Null,
        "OF" => Kw::Of,
        "OFFSET" => Kw::Offset,
        "ON" => Kw::On,
        "OR" => Kw::Or,
        "ORDER" => Kw::Order,
        "OVER" => Kw::Over,
        "PARTITION" => Kw::Partition,
        "PRIMARY" => Kw::Primary,
        "REAL" => Kw::Real,
        "RECURSIVE" => Kw::Recursive,
        "REFERENCES" => Kw::References,
        "RENAME" => Kw::Rename,
        "REORDER" => Kw::Reorder,
        "RESTRICT" => Kw::Restrict,
        "RETURNING" => Kw::Returning,
        "ROLLBACK" => Kw::Rollback,
        "ROW" => Kw::Row,
        "SELECT" => Kw::Select,
        "SET" => Kw::Set,
        "STATEMENT" => Kw::Statement,
        "TABLE" => Kw::Table,
        "TEXT" => Kw::Text,
        "THEN" => Kw::Then,
        "TIMESTAMP" => Kw::Timestamp,
        "TO" => Kw::To,
        "TRIGGER" => Kw::Trigger,
        "TRUE" => Kw::True,
        "UNION" => Kw::Union,
        "UNIQUE" => Kw::Unique,
        "UPDATE" => Kw::Update,
        "VALUES" => Kw::Values,
        "VERSION" => Kw::Version,
        "VIEW" => Kw::View,
        "WHEN" => Kw::When,
        "WHERE" => Kw::Where,
        "WITH" => Kw::With,
        _ => return None,
    })
}

/// Token con su posición (offset en bytes dentro del SQL original).
#[derive(Clone, Debug)]
pub struct Spanned {
    pub tok: Tok,
    pub pos: usize,
}

fn err(pos: usize, msg: impl Into<String>) -> Error {
    Error::Sql {
        msg: msg.into(),
        pos: Some(pos),
    }
}

pub fn lex(sql: &str) -> Result<Vec<Spanned>> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;

    while i < bytes.len() {
        let start = i;
        let b = bytes[i];
        match b {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                // Comentario hasta fin de línea.
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'(' => push(&mut out, Tok::LParen, start, &mut i),
            b')' => push(&mut out, Tok::RParen, start, &mut i),
            b',' => push(&mut out, Tok::Comma, start, &mut i),
            b';' => push(&mut out, Tok::Semi, start, &mut i),
            b'.' => push(&mut out, Tok::Dot, start, &mut i),
            b'+' => push(&mut out, Tok::Plus, start, &mut i),
            b'-' => push(&mut out, Tok::Minus, start, &mut i),
            b'*' => push(&mut out, Tok::Star, start, &mut i),
            b'/' => push(&mut out, Tok::Slash, start, &mut i),
            b'%' => push(&mut out, Tok::Percent, start, &mut i),
            b'=' => push(&mut out, Tok::Eq, start, &mut i),
            b'|' if bytes.get(i + 1) == Some(&b'|') => {
                out.push(Spanned {
                    tok: Tok::Concat,
                    pos: start,
                });
                i += 2;
            }
            b'!' if bytes.get(i + 1) == Some(&b'=') => {
                out.push(Spanned {
                    tok: Tok::Ne,
                    pos: start,
                });
                i += 2;
            }
            b'<' => match bytes.get(i + 1) {
                Some(b'=') => {
                    out.push(Spanned {
                        tok: Tok::Le,
                        pos: start,
                    });
                    i += 2;
                }
                Some(b'>') => {
                    out.push(Spanned {
                        tok: Tok::Ne,
                        pos: start,
                    });
                    i += 2;
                }
                _ => push(&mut out, Tok::Lt, start, &mut i),
            },
            b'>' => match bytes.get(i + 1) {
                Some(b'=') => {
                    out.push(Spanned {
                        tok: Tok::Ge,
                        pos: start,
                    });
                    i += 2;
                }
                _ => push(&mut out, Tok::Gt, start, &mut i),
            },
            b'?' => {
                i += 1;
                let d0 = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if d0 == i {
                    return Err(err(start, "parámetro sin número: usa ?1, ?2, …"));
                }
                let n: usize = sql[d0..i]
                    .parse()
                    .map_err(|_| err(start, "número de parámetro demasiado grande"))?;
                if n == 0 {
                    return Err(err(start, "los parámetros empiezan en ?1"));
                }
                out.push(Spanned {
                    tok: Tok::Param(n),
                    pos: start,
                });
            }
            b':' => {
                i += 1;
                let s = i;
                if i < bytes.len() && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
                    i += 1;
                    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
                    {
                        i += 1;
                    }
                    out.push(Spanned {
                        tok: Tok::NamedParam(sql[s..i].to_owned()),
                        pos: start,
                    });
                } else {
                    return Err(err(start, "parámetro nombrado sin nombre: usa :nombre"));
                }
            }
            b'\'' => {
                let (s, next) = lex_string(sql, start)?;
                out.push(Spanned {
                    tok: Tok::Str(s),
                    pos: start,
                });
                i = next;
            }
            b'"' => {
                // Identificador entre comillas dobles (SQL estándar): permite
                // palabras reservadas, espacios y unicode como nombres. Nunca es
                // palabra clave ni cadena: sale como `Ident` literal.
                let (name, next) = lex_quoted_ident(sql, start)?;
                out.push(Spanned {
                    tok: Tok::Ident(name),
                    pos: start,
                });
                i = next;
            }
            b'x' | b'X' if bytes.get(i + 1) == Some(&b'\'') => {
                let (raw, next) = lex_string(sql, start + 1)?;
                if raw.len() % 2 != 0 || !raw.bytes().all(|c| c.is_ascii_hexdigit()) {
                    return Err(err(start, "literal de blob inválido: usa x'AABB…'"));
                }
                let blob = raw
                    .as_bytes()
                    .chunks(2)
                    .map(|p| {
                        u8::from_str_radix(std::str::from_utf8(p).expect("hex ascii"), 16)
                            .expect("hex validado")
                    })
                    .collect();
                out.push(Spanned {
                    tok: Tok::Blob(blob),
                    pos: start,
                });
                i = next;
            }
            b'0'..=b'9' => {
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                let mut is_float = false;
                // Parte fraccionaria: un punto seguido de dígitos (`1.5`) o un punto
                // final (`100.`). No se consume el punto si lo sigue una letra
                // (no existe `numero.identificador` en este SQL).
                if bytes.get(i) == Some(&b'.')
                    && !bytes.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic())
                {
                    is_float = true;
                    i += 1;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                // Exponente científico: `e`/`E`, signo opcional, uno o más dígitos
                // (`1e308`, `1.5e3`, `1e-308`). Solo se consume si hay dígitos.
                if matches!(bytes.get(i), Some(b'e' | b'E')) {
                    let mut j = i + 1;
                    if matches!(bytes.get(j), Some(b'+' | b'-')) {
                        j += 1;
                    }
                    if bytes.get(j).is_some_and(|c| c.is_ascii_digit()) {
                        is_float = true;
                        i = j + 1;
                        while i < bytes.len() && bytes[i].is_ascii_digit() {
                            i += 1;
                        }
                    }
                }
                if is_float {
                    let f: f64 = sql[start..i]
                        .parse()
                        .map_err(|_| err(start, "literal real inválido"))?;
                    out.push(Spanned {
                        tok: Tok::Float(f),
                        pos: start,
                    });
                } else {
                    let n: i64 = sql[start..i]
                        .parse()
                        .map_err(|_| err(start, "entero fuera de rango (i64)"))?;
                    out.push(Spanned {
                        tok: Tok::Int(n),
                        pos: start,
                    });
                }
            }
            c if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 => {
                // Identificador sin comillas. Los bytes ≥ 0x80 (cuerpo de un
                // carácter UTF-8 multibyte) cuentan como letra, así que se admiten
                // identificadores unicode (`café`, `名前`) como en SQLite. El corte
                // siempre cae en un byte ASCII (frontera de carácter): el slice es
                // UTF-8 válido.
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] >= 0x80)
                {
                    i += 1;
                }
                let word = &sql[start..i];
                match keyword(word) {
                    Some(kw) => out.push(Spanned {
                        tok: Tok::Kw(kw),
                        pos: start,
                    }),
                    None => out.push(Spanned {
                        tok: Tok::Ident(word.to_owned()),
                        pos: start,
                    }),
                }
            }
            _ => {
                // Posible UTF-8 multibyte u otro símbolo: inválido en SQL v1.
                return Err(err(start, "carácter inesperado"));
            }
        }
    }
    Ok(out)
}

fn push(out: &mut Vec<Spanned>, tok: Tok, pos: usize, i: &mut usize) {
    out.push(Spanned { tok, pos });
    *i += 1;
}

/// Identificador entre comillas dobles `"…"` con escape `""` (SQL estándar).
/// Devuelve (nombre, byte siguiente). Rechaza `""` (un nombre vacío no es válido).
fn lex_quoted_ident(sql: &str, quote: usize) -> Result<(String, usize)> {
    let bytes = sql.as_bytes();
    debug_assert_eq!(bytes[quote], b'"');
    let mut s = String::new();
    let mut i = quote + 1;
    loop {
        match bytes.get(i) {
            None => return Err(err(quote, "identificador entre comillas sin cerrar")),
            Some(b'"') if bytes.get(i + 1) == Some(&b'"') => {
                s.push('"');
                i += 2;
            }
            Some(b'"') => {
                if s.is_empty() {
                    return Err(err(quote, "identificador entre comillas vacío"));
                }
                return Ok((s, i + 1));
            }
            Some(_) => {
                let ch = sql[i..].chars().next().expect("índice en frontera de char");
                s.push(ch);
                i += ch.len_utf8();
            }
        }
    }
}

/// Cadena `'…'` con escape `''`. Devuelve (contenido, byte siguiente).
fn lex_string(sql: &str, quote: usize) -> Result<(String, usize)> {
    let bytes = sql.as_bytes();
    debug_assert_eq!(bytes[quote], b'\'');
    let mut s = String::new();
    let mut i = quote + 1;
    loop {
        match bytes.get(i) {
            None => return Err(err(quote, "cadena sin cerrar")),
            Some(b'\'') if bytes.get(i + 1) == Some(&b'\'') => {
                s.push('\'');
                i += 2;
            }
            Some(b'\'') => return Ok((s, i + 1)),
            Some(_) => {
                // Avanzar un carácter UTF-8 completo.
                let ch = sql[i..].chars().next().expect("índice en frontera de char");
                s.push(ch);
                i += ch.len_utf8();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(sql: &str) -> Vec<Tok> {
        lex(sql).unwrap().into_iter().map(|s| s.tok).collect()
    }

    #[test]
    fn keywords_are_case_insensitive_idents_keep_case() {
        assert_eq!(
            toks("select Facturas"),
            vec![Tok::Kw(Kw::Select), Tok::Ident("Facturas".into())]
        );
    }

    #[test]
    fn literals() {
        assert_eq!(
            toks("42 3.5 'ho''la' x'CAfe' TRUE NULL ?2"),
            vec![
                Tok::Int(42),
                Tok::Float(3.5),
                Tok::Str("ho'la".into()),
                Tok::Blob(vec![0xCA, 0xFE]),
                Tok::Kw(Kw::True),
                Tok::Kw(Kw::Null),
                Tok::Param(2),
            ]
        );
    }

    #[test]
    fn quoted_and_unicode_identifiers() {
        // Comillas dobles: palabra reservada y espacios como nombre; escape `""`.
        assert_eq!(
            toks(r#"SELECT "select", "mi col", "a""b" FROM t"#),
            vec![
                Tok::Kw(Kw::Select),
                Tok::Ident("select".into()),
                Tok::Comma,
                Tok::Ident("mi col".into()),
                Tok::Comma,
                Tok::Ident("a\"b".into()),
                Tok::Kw(Kw::From),
                Tok::Ident("t".into()),
            ]
        );
        // Identificador unicode sin comillas (como SQLite: bytes ≥ 0x80 son letra).
        assert_eq!(
            toks("SELECT café, 名前"),
            vec![
                Tok::Kw(Kw::Select),
                Tok::Ident("café".into()),
                Tok::Comma,
                Tok::Ident("名前".into()),
            ]
        );
        // `""` y comilla sin cerrar son errores con posición.
        assert!(matches!(lex(r#"SELECT """#), Err(Error::Sql { .. })));
        assert!(matches!(lex(r#"SELECT "abc"#), Err(Error::Sql { .. })));
    }

    #[test]
    fn operators_and_comments() {
        assert_eq!(
            toks("a <= b -- esto se ignora\n <> c"),
            vec![
                Tok::Ident("a".into()),
                Tok::Le,
                Tok::Ident("b".into()),
                Tok::Ne,
                Tok::Ident("c".into()),
            ]
        );
    }

    #[test]
    fn errors_carry_byte_position() {
        match lex("SELECT 'sin cerrar") {
            Err(Error::Sql { pos: Some(7), .. }) => {}
            other => panic!("se esperaba error en byte 7, llegó {other:?}"),
        }
        match lex("SELECT ?") {
            Err(Error::Sql { pos: Some(7), .. }) => {}
            other => panic!("se esperaba error en byte 7, llegó {other:?}"),
        }
        assert!(lex("SELECT 99999999999999999999").is_err());
    }
}
