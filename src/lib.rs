//! # Arkeion
//!
//! Motor de base de datos embebido, auditable y versionado, escrito en Rust puro.
//! *Como si SQLite y Git tuvieran un hijo… y hubiera nacido en Europa.*
//!
//! Diseño completo en `docs/` (arquitectura, formato de archivo, API, hitos).
//!
//! ## Ejemplo
//!
//! ```
//! use arkeion::{Database, Options, params};
//!
//! let dir = tempfile::tempdir().unwrap();
//! let db = Database::open(dir.path().join("demo.arkeion"), Options::default()).unwrap();
//! let conn = db.connect().unwrap();
//!
//! conn.execute(
//!     "CREATE TABLE clientes (id INTEGER PRIMARY KEY, nombre TEXT NOT NULL, saldo REAL)",
//!     &[],
//! )
//! .unwrap();
//! conn.execute(
//!     "INSERT INTO clientes (nombre, saldo) VALUES (?1, ?2)",
//!     &params!["Acme GmbH", 1250.0],
//! )
//! .unwrap();
//!
//! let mut rows = conn
//!     .query("SELECT id, nombre FROM clientes WHERE saldo > ?1", &params![1000])
//!     .unwrap();
//! let row = rows.next().unwrap().unwrap();
//! assert_eq!(row.get::<i64>("id").unwrap(), 1);
//! assert_eq!(row.get::<String>("nombre").unwrap(), "Acme GmbH");
//!
//! // Transacción explícita: o todo, o nada (soltarla sin commit = rollback).
//! let tx = conn.begin().unwrap();
//! tx.execute("UPDATE clientes SET saldo = saldo - ?1 WHERE id = 1", &params![250.0])
//!     .unwrap();
//! tx.commit().unwrap();
//! ```
//!
//! **Estado**: M9 (núcleo) — `vacuum`/compactación sobre M8. [`Database::vacuum`]
//! reescribe la base según [`Retention`] (checkpoint de cadena + replay de
//! deltas + rename atómico) y [`Database::vacuum_rekey`] rota la clave; ambas
//! mantienen `verify()` OK y el `AS OF` de las versiones retenidas. Reposa sobre
//! M8 (ramas: [`Database::create_branch`]/[`Database::merge`]/[`Database::diff`]),
//! M7 (cifrado: [`Options::key`]), M6 (auditoría: [`Database::verify`]), M5
//! (time-travel: `AS OF`) y M4 (DML completo, JOINs, agregados, transacciones,
//! prepared statements). La API se estabiliza milestone a milestone; lo marcado
//! `#[doc(hidden)]` es interno.

#![forbid(unsafe_code)]

mod api;
mod branch;
mod error;

pub use api::{
    ColIndex, Connection, Database, FromValue, Options, Row, Rows, Statement, Transaction,
};
pub use branch::{ChangeKind, Diff, RowChange, SchemaChange};
pub use commit::{AuditAnchor, AuditReport};
pub use crypto::Key;
pub use error::{Error, Result};
pub use record::Value;
pub use tx::{
    AsOf, BranchInfo, MergeConflict, MergePolicy, MergeReport, Retention, Revision, VacuumReport,
};

// Módulos internos: públicos solo para que los hitos se construyan incrementalmente
// sin marcar código de fundación como muerto. NO son API estable.
#[doc(hidden)]
pub mod btree;
#[doc(hidden)]
pub mod catalog;
#[doc(hidden)]
pub mod commit;
#[doc(hidden)]
pub mod compress;
#[doc(hidden)]
pub mod crypto;
#[doc(hidden)]
pub mod exec;
#[doc(hidden)]
pub mod format;
#[doc(hidden)]
pub mod io;
#[doc(hidden)]
pub mod pager;
#[doc(hidden)]
pub mod record;
#[doc(hidden)]
pub mod sql;
#[doc(hidden)]
pub mod tx;

#[cfg(test)]
pub(crate) mod testutil;
