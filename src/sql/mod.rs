//! SQL de Arkeion: lexer y parser descendente recursivo escritos a mano,
//! cero dependencias (docs/04-sql.md).

pub mod ast;
pub mod lexer;
pub mod parser;

pub use parser::parse;
