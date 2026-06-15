//! SQL de Arkeion: lexer y parser descendente recursivo escritos a mano,
//! cero dependencias (docs/04-sql.md).

pub mod ast;
pub(crate) mod datetime;
pub(crate) mod json;
pub mod lexer;
pub mod parser;

pub use parser::{parse, parse_full};
