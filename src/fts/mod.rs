//! Full-text search (FTS): `MATCH` + índice invertido.
//!
//! Diseño y plan en `docs/12-fts.md`. FTS es **nativo** (no virtual table) y
//! vive en el mismo árbol versionado/copy-on-write que los datos, así que hereda
//! time-travel, cifrado y auditoría: `WHERE col MATCH 'x' AS OF VERSION n` busca
//! en el pasado sin un segundo store que mantener sincronizado.
//!
//! Estado: Fase 1 (tokenización). El índice invertido, el operador `MATCH`, el
//! planner y el ranking BM25 llegan en fases siguientes.

mod excerpt;
mod query;
mod tokenizer;

pub use excerpt::{highlight, snippet};
pub use query::{Query, parse_query};
pub use tokenizer::{AsciiTokenizer, Token, Tokenizer, UnicodeTokenizer};

use crate::error::{Error, Result};

/// Instancia el [`Tokenizer`] registrado con `name` (el que se guarda en el
/// catálogo del índice FTS). Tokenizers válidos: `unicode` (por defecto),
/// `ascii`. Un nombre desconocido es un error de SQL (se selecciona con
/// `CREATE FULLTEXT INDEX … USING <name>`).
pub fn tokenizer_for(name: &str) -> Result<Box<dyn Tokenizer>> {
    match name {
        "unicode" => Ok(Box::new(UnicodeTokenizer::default())),
        "ascii" => Ok(Box::new(AsciiTokenizer)),
        other => Err(Error::Sql {
            msg: format!("tokenizer FTS desconocido: «{other}» (válidos: unicode, ascii)"),
            pos: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registro_resuelve_y_rechaza() {
        assert_eq!(tokenizer_for("unicode").unwrap().name(), "unicode");
        assert_eq!(tokenizer_for("ascii").unwrap().name(), "ascii");
        assert!(matches!(tokenizer_for("porter"), Err(Error::Sql { .. })));
    }
}
