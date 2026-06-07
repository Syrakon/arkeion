//! # Arkeion
//!
//! Motor de base de datos embebido, auditable y versionado, escrito en Rust puro.
//! *Como si SQLite y Git tuvieran un hijo… y hubiera nacido en Europa.*
//!
//! Diseño completo en `docs/` (arquitectura, formato de archivo, API, hitos).
//!
//! **Estado**: M2 — capas física, transaccional (KV ACID con snapshots sin
//! locks) y relacional (record/catalog). El SQL y la API pública llegan en M3.

#![forbid(unsafe_code)]

mod error;

pub use error::{Error, Result};
pub use record::Value;

// Módulos internos: públicos solo para que los hitos se construyan incrementalmente
// sin marcar código de fundación como muerto. NO son API estable.
#[doc(hidden)]
pub mod btree;
#[doc(hidden)]
pub mod catalog;
#[doc(hidden)]
pub mod commit;
#[doc(hidden)]
pub mod crypto;
#[doc(hidden)]
pub mod format;
#[doc(hidden)]
pub mod io;
#[doc(hidden)]
pub mod pager;
#[doc(hidden)]
pub mod record;
#[doc(hidden)]
pub mod tx;

#[cfg(test)]
pub(crate) mod testutil;
