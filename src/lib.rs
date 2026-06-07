//! # Arkeion
//!
//! Motor de base de datos embebido, auditable y versionado, escrito en Rust puro.
//! *Como si SQLite y Git tuvieran un hijo… y hubiera nacido en Europa.*
//!
//! Diseño completo en `docs/` (arquitectura, formato de archivo, API, hitos).
//!
//! **Estado**: M0 — fundación: formato de página, E/S, sellado criptográfico
//! y pager. La API pública llega en M3.

#![forbid(unsafe_code)]

mod error;

pub use error::{Error, Result};

// Módulos internos: públicos solo para que los hitos se construyan incrementalmente
// sin marcar código de fundación como muerto. NO son API estable.
#[doc(hidden)]
pub mod crypto;
#[doc(hidden)]
pub mod format;
#[doc(hidden)]
pub mod io;
#[doc(hidden)]
pub mod pager;
