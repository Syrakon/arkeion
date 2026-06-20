//! Catálogo relacional sobre el árbol de datos (docs/02-formato-archivo.md).
//!
//! El esquema vive en el árbol que **ramifica**: una migración en una rama
//! cambia el esquema solo en esa rama (M8). Espacios de claves:
//!
//! ```text
//! [0x00,0x00]                       → próximo table_id (u32 LE)
//! [0x00,0x01, nombre UTF-8]         → esquema serializado (incluye sus índices)
//! [0x00,0x02, table_id BE]          → próximo rowid (i64 LE)
//! [0x00,0x03]                       → próximo index_id (u32 LE)
//! [0x00,0x04, nombre índice UTF-8]  → nombre de tabla (ref global del índice)
//! [0x00,0x07]                       → próximo fts_id (u32 LE)
//! [0x00,0x08, nombre FTS UTF-8]     → nombre de tabla (ref global del índice FTS)
//! [enc_oint(table_id), enc_oint(rowid)] → registro (v5: enteros
//!     order-preserving de longitud variable; sin byte de namespace — la
//!     cabecera 0x80+ de table_id ya lo distingue de 0x00/0x02)
//! [0x02, index_id BE, valor*, rowid BE*] → entrada de índice (valor memcomparable)
//! [0x03, fts_id BE, 0x00, term, rowid BE, field, pos] → posting full-text (docs/12)
//! [0x03, fts_id BE, 0x01|0x02|0x03, …]    → stats BM25 (doclen / globales / df)
//! ```

use crate::btree::{self, Cursor, NodeSource, NodeStore};
use crate::error::{Error, Result};
use crate::format::{PageId, put_varint, take_varint};
use crate::keyenc;
use crate::record::{self, Value, ValueRef};

pub const MAX_COLUMNS: usize = 255;
pub const MAX_NAME_LEN: usize = 128;

const KS_CATALOG: u8 = 0x00;
// 0x01 era el byte de namespace de las filas (v4); en v5 la clave de fila empieza
// directo por enc_oint(table_id) (cabecera 0x80+), disjunta de 0x00 y 0x02.
const KS_INDEX: u8 = 0x02;
// Índices full-text: postings y stats, todo bajo `[0x03, fts_id BE]` (un solo
// prefijo por índice ⇒ DROP en una pasada). Disjunto de 0x00/0x02 y de las filas.
const KS_FTS: u8 = 0x03;
// Índices vectoriales (IVF): centroides y postings por cluster, bajo
// `[0x04, vidx_id BE]` (un prefijo por índice ⇒ DROP en una pasada).
const KS_VECTOR: u8 = 0x04;
const CAT_META: u8 = 0x00;
const CAT_TABLE: u8 = 0x01;
const CAT_COUNTER: u8 = 0x02;
const CAT_INDEX_COUNTER: u8 = 0x03;
const CAT_INDEX_REF: u8 = 0x04;
const CAT_VIEW: u8 = 0x05;
const CAT_TRIGGER: u8 = 0x06;
const CAT_FTS_COUNTER: u8 = 0x07;
const CAT_FTS_REF: u8 = 0x08;
const CAT_VECTOR_COUNTER: u8 = 0x09;
const CAT_VECTOR_REF: u8 = 0x0A;
// Sub-tipos dentro de un índice FTS, tras `[0x03, fts_id BE]`:
const FTS_POSTING: u8 = 0x00; // ‖ term ‖ rowid BE ‖ field ‖ pos varint → vacío
const FTS_DOCLEN: u8 = 0x01; // ‖ rowid BE → nº de tokens del doc (varint)
const FTS_GLOBAL: u8 = 0x02; // → {N docs, Σ tokens} (dos varints) para avgdl
const FTS_DF: u8 = 0x03; // ‖ term → nº de docs con el término (varint)
// Sub-tipos dentro de un índice vectorial, tras `[0x04, vidx_id BE]`:
const VEC_CENTROID: u8 = 0x00; // ‖ centroid_id BE(2) → vector f32 del centroide
const VEC_POSTING: u8 = 0x01; // ‖ centroid_id BE(2) ‖ rowid BE(8) → vacío

/// v2: el esquema serializado incluye la lista de índices de la tabla.
// v3 añade el orden lógico de columnas (reorden de presentación) al final del
// registro de esquema; v2 (sin él) se sigue leyendo como identidad.
// v4 añade las claves foráneas al final del registro de esquema; v2/v3 (sin
// ellas) se leen con `foreign_keys` vacío. v5 añade el flag `dropped` por columna
// (DROP COLUMN lógico); v<5 lo lee como `false`. v7 añade los predicados CHECK de
// tabla. v8 añade los índices FTS al final del registro; v<8 los lee vacíos.
const SCHEMA_VERSION: u8 = 9;
const NO_ALIAS: u8 = 0xFF;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColType {
    Integer = 1,
    Real = 2,
    Text = 3,
    Blob = 4,
    Boolean = 5,
}

impl ColType {
    fn from_u8(b: u8) -> Option<ColType> {
        Some(match b {
            1 => ColType::Integer,
            2 => ColType::Real,
            3 => ColType::Text,
            4 => ColType::Blob,
            5 => ColType::Boolean,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            ColType::Integer => "INTEGER",
            ColType::Real => "REAL",
            ColType::Text => "TEXT",
            ColType::Blob => "BLOB",
            ColType::Boolean => "BOOLEAN",
        }
    }
}

/// Acción al borrar la fila padre referenciada por una FK.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FkAction {
    /// Falla si quedan hijos referenciando la fila (por defecto).
    Restrict,
    /// Borra en cascada los hijos.
    Cascade,
    /// Pone a NULL la columna FK de los hijos.
    SetNull,
}

impl FkAction {
    fn as_u8(self) -> u8 {
        match self {
            FkAction::Restrict => 0,
            FkAction::Cascade => 1,
            FkAction::SetNull => 2,
        }
    }
    fn from_u8(b: u8) -> Option<FkAction> {
        match b {
            0 => Some(FkAction::Restrict),
            1 => Some(FkAction::Cascade),
            2 => Some(FkAction::SetNull),
            _ => None,
        }
    }
}

/// Clave foránea. Una o más columnas hijas → una o más columnas del padre.
/// `parent_columns` vacío significa «la PK (rowid) del padre» (referencia por
/// rowid, el caso por defecto y el único de v4/v5); si no está vacío, referencia
/// esas columnas físicas del padre, que deben estar cubiertas por un índice
/// `UNIQUE` (o ser la PK). La aridad de `columns` y `parent_columns` coincide
/// (salvo el caso PK-por-rowid, en que `parent_columns` va vacío y `columns`
/// tiene una sola entrada).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignKey {
    /// Posiciones físicas de las columnas hijas en `TableDef.columns`.
    pub columns: Vec<usize>,
    /// Tabla padre.
    pub parent: String,
    /// Posiciones físicas referenciadas en el padre; vacío = la PK (rowid).
    pub parent_columns: Vec<usize>,
    pub on_delete: FkAction,
    pub on_update: FkAction,
}

/// Momento de disparo de un trigger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerTiming {
    Before,
    After,
    /// `INSTEAD OF` — solo en vistas: el cuerpo **reemplaza** la escritura sobre la
    /// vista (que no tiene almacenamiento). Siempre row-level.
    InsteadOf,
}

/// Evento que dispara un trigger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerEvent {
    Insert,
    Update,
    Delete,
}

impl TriggerTiming {
    fn as_u8(self) -> u8 {
        match self {
            TriggerTiming::Before => 0,
            TriggerTiming::After => 1,
            TriggerTiming::InsteadOf => 2,
        }
    }
    fn from_u8(b: u8) -> Option<TriggerTiming> {
        match b {
            0 => Some(TriggerTiming::Before),
            1 => Some(TriggerTiming::After),
            2 => Some(TriggerTiming::InsteadOf),
            _ => None,
        }
    }
}

impl TriggerEvent {
    fn as_u8(self) -> u8 {
        match self {
            TriggerEvent::Insert => 0,
            TriggerEvent::Update => 1,
            TriggerEvent::Delete => 2,
        }
    }
    fn from_u8(b: u8) -> Option<TriggerEvent> {
        match b {
            0 => Some(TriggerEvent::Insert),
            1 => Some(TriggerEvent::Update),
            2 => Some(TriggerEvent::Delete),
            _ => None,
        }
    }
}

/// Granularidad de disparo: por fila afectada o una vez por sentencia.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerForEach {
    /// `FOR EACH ROW`: el cuerpo se dispara por cada fila afectada (con OLD/NEW).
    Row,
    /// `FOR EACH STATEMENT`: el cuerpo se dispara una sola vez por sentencia,
    /// aunque afecte a cero o varias filas. Sin acceso a OLD/NEW (no hay fila).
    Statement,
}

impl TriggerForEach {
    fn as_u8(self) -> u8 {
        match self {
            TriggerForEach::Row => 0,
            TriggerForEach::Statement => 1,
        }
    }
    fn from_u8(b: u8) -> Option<TriggerForEach> {
        match b {
            0 => Some(TriggerForEach::Row),
            1 => Some(TriggerForEach::Statement),
            _ => None,
        }
    }
}

/// Trigger: dispara su cuerpo (DML) en un evento INSERT/UPDATE/DELETE, por fila
/// (`Row`, con `OLD`/`NEW`) o una vez por sentencia (`Statement`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TriggerDef {
    pub name: String,
    pub timing: TriggerTiming,
    pub event: TriggerEvent,
    pub for_each: TriggerForEach,
    pub table: String,
    /// Cuerpo `BEGIN … END` como texto; se re-parsea al disparar (con `OLD`/`NEW`
    /// sustituidos por los valores de la fila en los triggers `Row`).
    pub body: String,
}

/// Definición de columna tal y como la pide el llamador.
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnSpec {
    pub name: String,
    pub col_type: ColType,
    pub not_null: bool,
    pub primary_key: bool,
    pub default: Option<Value>,
    /// `REFERENCES padre [(col)] [ON DELETE acción] [ON UPDATE acción]` declarado a
    /// nivel de columna. `parent_column` `None` = la PK del padre.
    pub references: Option<ColumnFk>,
    /// `UNIQUE` en línea: se crea un índice UNIQUE sobre esta columna.
    pub unique: bool,
    /// `CHECK (expr)` en línea (texto del predicado).
    pub check: Option<String>,
}

/// Clave foránea de una sola columna declarada en línea con la columna hija.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnFk {
    pub parent: String,
    /// Columna del padre; `None` = su PK (rowid).
    pub parent_column: Option<String>,
    pub on_delete: FkAction,
    pub on_update: FkAction,
}

/// Clave foránea declarada a nivel de tabla:
/// `FOREIGN KEY (c…) REFERENCES padre (p…) [ON DELETE …] [ON UPDATE …]`. Permite
/// claves **compuestas** (varias columnas).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignKeySpec {
    pub columns: Vec<String>,
    pub parent: String,
    /// Columnas del padre; vacío = su PK (rowid).
    pub parent_columns: Vec<String>,
    pub on_delete: FkAction,
    pub on_update: FkAction,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TableSpec {
    pub name: String,
    pub columns: Vec<ColumnSpec>,
    /// FKs a nivel de tabla (compuestas). Las de columna van en `ColumnSpec`.
    pub foreign_keys: Vec<ForeignKeySpec>,
    /// `UNIQUE (c…)` a nivel de tabla (por nombres de columna); cada uno → índice UNIQUE.
    pub uniques: Vec<Vec<String>>,
    /// `CHECK (expr)` a nivel de tabla (texto). Los de columna van en `ColumnSpec.check`.
    pub checks: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub col_type: ColType,
    pub not_null: bool,
    pub default: Option<Value>,
    /// `DROP COLUMN` lógico (tombstone): la columna deja de verse y de resolverse
    /// por nombre, pero su **posición física se congela** (las filas no se
    /// reescriben, time-travel intacto). Se excluye de `logical_order`; sus bytes
    /// muertos los reclama el vacuum. Estilo Postgres (`attisdropped`).
    pub dropped: bool,
}

/// Índice secundario sobre una o más columnas de una tabla (keyspace `0x02`).
#[derive(Clone, Debug, PartialEq)]
pub struct IndexDef {
    pub name: String,
    pub index_id: u32,
    /// Posiciones de las columnas indexadas (en `TableDef.columns`).
    pub columns: Vec<usize>,
    pub unique: bool,
}

/// Índice full-text sobre una o más columnas de texto (keyspace `0x03`).
///
/// A diferencia de `IndexDef`, indexa **por término** (tras tokenizar) en vez de
/// por valor completo, y guarda el nombre del tokenizer con el que se construyó:
/// el mismo debe usarse al consultar `MATCH`. Ver `docs/12-fts.md`.
#[derive(Clone, Debug, PartialEq)]
pub struct FtsIndexDef {
    pub name: String,
    pub fts_id: u32,
    /// Posiciones de las columnas indexadas (en `TableDef.columns`).
    pub columns: Vec<usize>,
    /// Nombre del tokenizer (`unicode`, `ascii`, …); ver `crate::fts::tokenizer_for`.
    pub tokenizer: String,
}

/// Métrica de un índice vectorial. Para `Cosine` los vectores se **normalizan** a
/// norma 1 al construir/buscar, así el orden por L2 coincide con el de coseno.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VectorMetric {
    Cosine,
    L2,
}

impl VectorMetric {
    fn as_u8(self) -> u8 {
        match self {
            VectorMetric::Cosine => 0,
            VectorMetric::L2 => 1,
        }
    }
    fn from_u8(b: u8) -> Option<VectorMetric> {
        match b {
            0 => Some(VectorMetric::Cosine),
            1 => Some(VectorMetric::L2),
            _ => None,
        }
    }
}

/// Índice vectorial IVF sobre una columna BLOB de vectores (keyspace `0x04`).
///
/// Agrupa los vectores en `lists` clusters (k-means); la búsqueda escanea solo los
/// `nprobe` clusters más cercanos (ANN aproximado). Centroides y postings por
/// cluster persisten bajo `[0x04, vidx_id]`. Ver `docs/13-vectores.md`.
#[derive(Clone, Debug, PartialEq)]
pub struct VectorIndexDef {
    pub name: String,
    pub vidx_id: u32,
    /// Posición de la columna BLOB con los vectores (en `TableDef.columns`).
    pub column: usize,
    /// Número de clusters (k de k-means).
    pub lists: u16,
    pub metric: VectorMetric,
    /// Dimensión de los vectores (fijada al construir).
    pub dim: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TableDef {
    pub name: String,
    pub table_id: u32,
    /// Columna `INTEGER PRIMARY KEY` = alias del rowid (estilo SQLite, D11).
    /// Su valor no se almacena en el registro: se reconstruye del rowid.
    pub rowid_alias: Option<usize>,
    pub columns: Vec<ColumnDef>,
    /// Índices secundarios de la tabla (se mantienen en cada insert/update/delete).
    pub indexes: Vec<IndexDef>,
    /// Índices full-text de la tabla (keyspace `0x03`); mismo ciclo de
    /// mantenimiento que `indexes`. Vacío salvo `CREATE FULLTEXT INDEX`.
    pub fts_indexes: Vec<FtsIndexDef>,
    /// Índices vectoriales IVF de la tabla (keyspace `0x04`). Vacío salvo
    /// `CREATE VECTOR INDEX`.
    pub vector_indexes: Vec<VectorIndexDef>,
    /// Orden **lógico** (de presentación) de las columnas: `logical_order[i]` es la
    /// posición **física** de la i-ésima columna lógica — una permutación de
    /// `0..columns.len()`. Físicamente las columnas y las filas **nunca** se mueven
    /// (el registro es posicional, ver `record.rs`), así que reordenar es solo
    /// metadato del catálogo: O(1), sin reescribir filas y con time-travel intacto
    /// (las filas históricas se decodifican igual; el catálogo se versiona en el
    /// mismo b-tree, así que un `AS OF` ve el orden de su época). Lo honran solo la
    /// expansión de `*` y el `INSERT` posicional; índices, `rowid_alias` y la
    /// codificación siguen siendo físicos.
    pub logical_order: Vec<usize>,
    /// Claves foráneas de esta tabla (v4 del esquema). Se comprueban en cada
    /// INSERT/UPDATE (el padre debe existir) y el DELETE del padre las respeta.
    pub foreign_keys: Vec<ForeignKey>,
    /// Predicados `CHECK (expr)` como texto (v7 del esquema), de columna y de tabla.
    /// Se re-parsean y evalúan por fila en cada INSERT/UPDATE; falla si alguno da
    /// FALSE (NULL/TRUE pasan, semántica SQL).
    pub checks: Vec<String>,
}

/// Posición destino de `ALTER TABLE … MOVE COLUMN` (reorden lógico).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnPos {
    First,
    Before(String),
    After(String),
}

// --- claves ---

fn meta_key() -> [u8; 2] {
    [KS_CATALOG, CAT_META]
}

fn table_key(name: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(2 + name.len());
    k.extend_from_slice(&[KS_CATALOG, CAT_TABLE]);
    k.extend_from_slice(name.as_bytes());
    k
}

fn counter_key(table_id: u32) -> [u8; 6] {
    let mut k = [KS_CATALOG, CAT_COUNTER, 0, 0, 0, 0];
    k[2..6].copy_from_slice(&table_id.to_be_bytes());
    k
}

/// Clave de fila `[enc_oint(table_id)][enc_oint(rowid)]` (v5): `table_id` y
/// `rowid` van en el entero order-preserving de longitud variable de `keyenc`, así
/// que una fila típica cuesta ~5 B (~6 con el byte de namespace de v4, 13 fijos en
/// v3) — conservando el orden `(table_id, rowid)` del b-tree (ambos códigos son
/// self-delimitados y ninguno es prefijo de otro). **No lleva byte de namespace**:
/// `table_id ≥ 1` ⇒ la cabecera de `enc_oint` es `0x81..=0x84`, disjunta de los
/// keyspaces de catálogo (`0x00`) e índice (`0x02`), así que la clave se
/// auto-identifica por su primer byte (≥ `0x80`).
pub fn row_key(table_id: u32, rowid: i64) -> Vec<u8> {
    let mut k = Vec::with_capacity(6);
    keyenc::encode_oint(table_id as i64, &mut k);
    keyenc::encode_oint(rowid, &mut k);
    k
}

/// Prefijo de **todas** las filas de una tabla: `[enc_oint(table_id)]`.
fn row_prefix(table_id: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity(2);
    keyenc::encode_oint(table_id as i64, &mut p);
    p
}

/// Decodifica una clave de fila en `(table_id, rowid)`, o `None` si no es una
/// (otro keyspace o mal formada). Inverso de [`row_key`]; lo usan el diff de ramas
/// y la extracción del alias de rowid en el scan. Una clave de fila empieza por
/// `enc_oint(table_id ≥ 1)` ⇒ primer byte `≥ 0x80`, lo que la distingue del
/// catálogo (`0x00`) y los índices (`0x02`).
pub(crate) fn decode_row_key(key: &[u8]) -> Option<(u32, i64)> {
    if *key.first()? < 0x80 {
        return None;
    }
    let mut pos = 0;
    let table_id = keyenc::decode_oint(key, &mut pos)?;
    let rowid = keyenc::decode_oint(key, &mut pos)?;
    Some((u32::try_from(table_id).ok()?, rowid))
}

// --- claves de índice secundario (keyspace 0x02) ---

fn index_counter_key() -> [u8; 2] {
    [KS_CATALOG, CAT_INDEX_COUNTER]
}

/// `[0x00,0x04, nombre]` → nombre de tabla: ref global del índice (su nombre es
/// único en toda la base, como en SQLite, para que `DROP INDEX nombre` lo ubique).
fn index_ref_key(name: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(2 + name.len());
    k.extend_from_slice(&[KS_CATALOG, CAT_INDEX_REF]);
    k.extend_from_slice(name.as_bytes());
    k
}

/// Prefijo de **todas** las entradas de un índice: `[0x02][index_id BE]`.
fn index_id_prefix(index_id: u32) -> [u8; 5] {
    let mut p = [0u8; 5];
    p[0] = KS_INDEX;
    p[1..5].copy_from_slice(&index_id.to_be_bytes());
    p
}

/// Prefijo de las entradas con un valor concreto: `[0x02][index_id][enc(values)]`.
/// `values` son los valores de las columnas indexadas (en orden).
fn index_value_prefix(idx: &IndexDef, values: &[Value]) -> Vec<u8> {
    let mut k = index_id_prefix(idx.index_id).to_vec();
    for v in values {
        keyenc::encode_index_value(v, &mut k);
    }
    k
}

/// Clave de la entrada de índice de una fila: prefijo de valor + rowid. El rowid
/// al final permite varias filas con el mismo valor (índice no único) y un orden
/// total estable; la codificación memcomparable es self-delimitada, así que el
/// rowid no se confunde con un valor de longitud variable.
fn index_entry_key(idx: &IndexDef, record: &[Value], rowid: i64) -> Vec<u8> {
    let mut k = index_id_prefix(idx.index_id).to_vec();
    for &col in &idx.columns {
        keyenc::encode_index_value(&record[col], &mut k);
    }
    k.extend_from_slice(&record::rowid_be(rowid));
    k
}

// --- esquema: serialización ---

fn encode_def(def: &TableDef) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.push(SCHEMA_VERSION);
    out.extend_from_slice(&def.table_id.to_le_bytes());
    out.push(def.rowid_alias.map_or(NO_ALIAS, |i| i as u8));
    put_varint(&mut out, def.columns.len() as u64);
    for col in &def.columns {
        put_varint(&mut out, col.name.len() as u64);
        out.extend_from_slice(col.name.as_bytes());
        out.push(col.col_type as u8);
        out.push(u8::from(col.not_null));
        match &col.default {
            None => out.push(0),
            Some(v) => {
                out.push(1);
                let encoded = record::encode_values(std::slice::from_ref(v));
                put_varint(&mut out, encoded.len() as u64);
                out.extend_from_slice(&encoded);
            }
        }
        out.push(u8::from(col.dropped)); // v5: tombstone de DROP COLUMN
    }
    // Índices (v2): id, unique, columnas y nombre por índice.
    put_varint(&mut out, def.indexes.len() as u64);
    for idx in &def.indexes {
        out.extend_from_slice(&idx.index_id.to_le_bytes());
        out.push(u8::from(idx.unique));
        put_varint(&mut out, idx.columns.len() as u64);
        for &c in &idx.columns {
            put_varint(&mut out, c as u64);
        }
        put_varint(&mut out, idx.name.len() as u64);
        out.extend_from_slice(idx.name.as_bytes());
    }
    // Orden lógico de columnas (v3): flag 0 = identidad (lo común, sin entradas);
    // 1 = permutación explícita (un varint por columna, posición física en orden
    // lógico). Nunca afecta a los bytes de las filas.
    if def.logical_order.iter().enumerate().all(|(i, &p)| i == p) {
        out.push(0);
    } else {
        out.push(1);
        for &p in &def.logical_order {
            put_varint(&mut out, p as u64);
        }
    }
    // Claves foráneas (v6): cuenta, y por cada una: columnas hijas, acciones
    // ON DELETE / ON UPDATE, nombre del padre y columnas del padre (vacío = PK).
    // El formato v4/v5 (una sola columna → PK) lo lee `decode_def` por su rama.
    put_varint(&mut out, def.foreign_keys.len() as u64);
    for fk in &def.foreign_keys {
        put_varint(&mut out, fk.columns.len() as u64);
        for &c in &fk.columns {
            put_varint(&mut out, c as u64);
        }
        out.push(fk.on_delete.as_u8());
        out.push(fk.on_update.as_u8());
        put_varint(&mut out, fk.parent.len() as u64);
        out.extend_from_slice(fk.parent.as_bytes());
        put_varint(&mut out, fk.parent_columns.len() as u64);
        for &p in &fk.parent_columns {
            put_varint(&mut out, p as u64);
        }
    }
    // Predicados CHECK (v7): cuenta y texto de cada uno. v2..v6 no los llevan.
    put_varint(&mut out, def.checks.len() as u64);
    for c in &def.checks {
        put_varint(&mut out, c.len() as u64);
        out.extend_from_slice(c.as_bytes());
    }
    // Índices FTS (v8): cuenta, y por cada uno: fts_id, columnas, nombre y
    // tokenizer. v2..v7 no los llevan ⇒ se leen vacíos.
    put_varint(&mut out, def.fts_indexes.len() as u64);
    for fts in &def.fts_indexes {
        out.extend_from_slice(&fts.fts_id.to_le_bytes());
        put_varint(&mut out, fts.columns.len() as u64);
        for &c in &fts.columns {
            put_varint(&mut out, c as u64);
        }
        put_varint(&mut out, fts.name.len() as u64);
        out.extend_from_slice(fts.name.as_bytes());
        put_varint(&mut out, fts.tokenizer.len() as u64);
        out.extend_from_slice(fts.tokenizer.as_bytes());
    }
    // Índices vectoriales (v9): cuenta, y por cada uno: vidx_id, columna, lists,
    // métrica, dim y nombre. v2..v8 no los llevan ⇒ se leen vacíos.
    put_varint(&mut out, def.vector_indexes.len() as u64);
    for vi in &def.vector_indexes {
        out.extend_from_slice(&vi.vidx_id.to_le_bytes());
        put_varint(&mut out, vi.column as u64);
        put_varint(&mut out, vi.lists as u64);
        out.push(vi.metric.as_u8());
        out.extend_from_slice(&vi.dim.to_le_bytes());
        put_varint(&mut out, vi.name.len() as u64);
        out.extend_from_slice(vi.name.as_bytes());
    }
    out
}

/// `true` si `order` es una permutación exacta de `0..n`.
fn is_permutation(order: &[usize], n: usize) -> bool {
    if order.len() != n {
        return false;
    }
    let mut seen = vec![false; n];
    for &p in order {
        match seen.get_mut(p) {
            Some(s) if !*s => *s = true,
            _ => return false,
        }
    }
    true
}

fn decode_def(name: &str, buf: &[u8]) -> Result<TableDef> {
    let bad = |reason: &'static str| Error::CorruptRecord(reason);
    let mut pos = 0usize;
    let take = |pos: &mut usize, n: usize| -> Result<&[u8]> {
        let s = buf.get(*pos..*pos + n).ok_or(bad("esquema truncado"))?;
        *pos += n;
        Ok(s)
    };

    let version = *take(&mut pos, 1)?.first().expect("len 1");
    if !(2..=9).contains(&version) {
        return Err(bad("versión de esquema desconocida"));
    }
    let table_id = u32::from_le_bytes(
        take(&mut pos, 4)?
            .try_into()
            .expect("rango fijo de 4 bytes"),
    );
    let alias = *take(&mut pos, 1)?.first().expect("len 1");
    let ncols = take_varint(buf, &mut pos).ok_or(bad("esquema truncado"))? as usize;
    if ncols > MAX_COLUMNS {
        return Err(bad("demasiadas columnas"));
    }

    let mut columns = Vec::with_capacity(ncols);
    for _ in 0..ncols {
        let nlen = take_varint(buf, &mut pos).ok_or(bad("esquema truncado"))? as usize;
        let cname = String::from_utf8(take(&mut pos, nlen)?.to_vec())
            .map_err(|_| bad("nombre de columna no UTF-8"))?;
        let col_type = ColType::from_u8(*take(&mut pos, 1)?.first().expect("len 1"))
            .ok_or(bad("tipo de columna desconocido"))?;
        let not_null = *take(&mut pos, 1)?.first().expect("len 1") != 0;
        let default = match *take(&mut pos, 1)?.first().expect("len 1") {
            0 => None,
            1 => {
                let dlen = take_varint(buf, &mut pos).ok_or(bad("esquema truncado"))? as usize;
                let values = record::decode_values(take(&mut pos, dlen)?)?;
                Some(values.into_iter().next().ok_or(bad("default vacío"))?)
            }
            _ => return Err(bad("marcador de default inválido")),
        };
        // v5: flag `dropped`. v<5 ⇒ false (ninguna columna borrada lógicamente).
        let dropped = version >= 5 && *take(&mut pos, 1)?.first().expect("len 1") != 0;
        columns.push(ColumnDef {
            name: cname,
            col_type,
            not_null,
            default,
            dropped,
        });
    }

    // Índices (v2).
    let nidx = take_varint(buf, &mut pos).ok_or(bad("esquema truncado"))? as usize;
    let mut indexes = Vec::with_capacity(nidx);
    for _ in 0..nidx {
        let index_id = u32::from_le_bytes(take(&mut pos, 4)?.try_into().expect("rango fijo"));
        let unique = *take(&mut pos, 1)?.first().expect("len 1") != 0;
        let icols = take_varint(buf, &mut pos).ok_or(bad("esquema truncado"))? as usize;
        if icols == 0 || icols > MAX_COLUMNS {
            return Err(bad("número de columnas de índice inválido"));
        }
        let mut idx_cols = Vec::with_capacity(icols);
        for _ in 0..icols {
            let c = take_varint(buf, &mut pos).ok_or(bad("esquema truncado"))? as usize;
            if c >= columns.len() {
                return Err(bad("columna de índice fuera de rango"));
            }
            idx_cols.push(c);
        }
        let nlen = take_varint(buf, &mut pos).ok_or(bad("esquema truncado"))? as usize;
        let iname = String::from_utf8(take(&mut pos, nlen)?.to_vec())
            .map_err(|_| bad("nombre de índice no UTF-8"))?;
        indexes.push(IndexDef {
            name: iname,
            index_id,
            columns: idx_cols,
            unique,
        });
    }

    // Orden lógico de columnas (v3). v2 no lo lleva ⇒ identidad.
    let logical_order = if version >= 3 {
        match *take(&mut pos, 1)?.first().expect("len 1") {
            0 => (0..columns.len()).collect(),
            1 => {
                let mut lo = Vec::with_capacity(columns.len());
                for _ in 0..columns.len() {
                    lo.push(take_varint(buf, &mut pos).ok_or(bad("esquema truncado"))? as usize);
                }
                if !is_permutation(&lo, columns.len()) {
                    return Err(bad("orden lógico de columnas inválido"));
                }
                lo
            }
            _ => return Err(bad("marcador de orden lógico inválido")),
        }
    } else {
        (0..columns.len()).collect()
    };

    // Claves foráneas. v2/v3 no las llevan ⇒ vacío. v4/v5: formato antiguo (una
    // columna → PK del padre, solo ON DELETE). v6+: compuestas, ON DELETE/UPDATE y
    // columnas explícitas del padre.
    let mut foreign_keys = Vec::new();
    if version >= 4 {
        let nfk = take_varint(buf, &mut pos).ok_or(bad("esquema truncado"))? as usize;
        for _ in 0..nfk {
            if version <= 5 {
                let column = take_varint(buf, &mut pos).ok_or(bad("esquema truncado"))? as usize;
                if column >= columns.len() {
                    return Err(bad("columna de FK fuera de rango"));
                }
                let on_delete = FkAction::from_u8(*take(&mut pos, 1)?.first().expect("len 1"))
                    .ok_or(bad("acción de FK desconocida"))?;
                let plen = take_varint(buf, &mut pos).ok_or(bad("esquema truncado"))? as usize;
                let parent = String::from_utf8(take(&mut pos, plen)?.to_vec())
                    .map_err(|_| bad("nombre de padre de FK no UTF-8"))?;
                foreign_keys.push(ForeignKey {
                    columns: vec![column],
                    parent,
                    parent_columns: Vec::new(), // = PK por rowid
                    on_delete,
                    on_update: FkAction::Restrict,
                });
            } else {
                let ncols = take_varint(buf, &mut pos).ok_or(bad("esquema truncado"))? as usize;
                let mut cols = Vec::with_capacity(ncols);
                for _ in 0..ncols {
                    let c = take_varint(buf, &mut pos).ok_or(bad("esquema truncado"))? as usize;
                    if c >= columns.len() {
                        return Err(bad("columna de FK fuera de rango"));
                    }
                    cols.push(c);
                }
                let on_delete = FkAction::from_u8(*take(&mut pos, 1)?.first().expect("len 1"))
                    .ok_or(bad("acción de FK desconocida"))?;
                let on_update = FkAction::from_u8(*take(&mut pos, 1)?.first().expect("len 1"))
                    .ok_or(bad("acción de FK desconocida"))?;
                let plen = take_varint(buf, &mut pos).ok_or(bad("esquema truncado"))? as usize;
                let parent = String::from_utf8(take(&mut pos, plen)?.to_vec())
                    .map_err(|_| bad("nombre de padre de FK no UTF-8"))?;
                let nparent = take_varint(buf, &mut pos).ok_or(bad("esquema truncado"))? as usize;
                let mut parent_columns = Vec::with_capacity(nparent);
                for _ in 0..nparent {
                    parent_columns
                        .push(take_varint(buf, &mut pos).ok_or(bad("esquema truncado"))? as usize);
                }
                foreign_keys.push(ForeignKey {
                    columns: cols,
                    parent,
                    parent_columns,
                    on_delete,
                    on_update,
                });
            }
        }
    }

    // Predicados CHECK (v7). v2..v6 no los llevan ⇒ vacío.
    let mut checks = Vec::new();
    if version >= 7 {
        let nck = take_varint(buf, &mut pos).ok_or(bad("esquema truncado"))? as usize;
        for _ in 0..nck {
            let clen = take_varint(buf, &mut pos).ok_or(bad("esquema truncado"))? as usize;
            checks.push(
                String::from_utf8(take(&mut pos, clen)?.to_vec())
                    .map_err(|_| bad("texto de CHECK no UTF-8"))?,
            );
        }
    }

    // Índices FTS (v8). v2..v7 no los llevan ⇒ vacío.
    let mut fts_indexes = Vec::new();
    if version >= 8 {
        let nfts = take_varint(buf, &mut pos).ok_or(bad("esquema truncado"))? as usize;
        for _ in 0..nfts {
            let fts_id = u32::from_le_bytes(take(&mut pos, 4)?.try_into().expect("rango fijo"));
            let icols = take_varint(buf, &mut pos).ok_or(bad("esquema truncado"))? as usize;
            if icols == 0 || icols > MAX_COLUMNS {
                return Err(bad("número de columnas de índice FTS inválido"));
            }
            let mut idx_cols = Vec::with_capacity(icols);
            for _ in 0..icols {
                let c = take_varint(buf, &mut pos).ok_or(bad("esquema truncado"))? as usize;
                if c >= columns.len() {
                    return Err(bad("columna de índice FTS fuera de rango"));
                }
                idx_cols.push(c);
            }
            let nlen = take_varint(buf, &mut pos).ok_or(bad("esquema truncado"))? as usize;
            let name = String::from_utf8(take(&mut pos, nlen)?.to_vec())
                .map_err(|_| bad("nombre de índice FTS no UTF-8"))?;
            let tlen = take_varint(buf, &mut pos).ok_or(bad("esquema truncado"))? as usize;
            let tokenizer = String::from_utf8(take(&mut pos, tlen)?.to_vec())
                .map_err(|_| bad("nombre de tokenizer no UTF-8"))?;
            fts_indexes.push(FtsIndexDef {
                name,
                fts_id,
                columns: idx_cols,
                tokenizer,
            });
        }
    }

    // Índices vectoriales (v9). v2..v8 no los llevan ⇒ vacío.
    let mut vector_indexes = Vec::new();
    if version >= 9 {
        let nv = take_varint(buf, &mut pos).ok_or(bad("esquema truncado"))? as usize;
        for _ in 0..nv {
            let vidx_id = u32::from_le_bytes(take(&mut pos, 4)?.try_into().expect("rango fijo"));
            let column = take_varint(buf, &mut pos).ok_or(bad("esquema truncado"))? as usize;
            if column >= columns.len() {
                return Err(bad("columna de índice vectorial fuera de rango"));
            }
            let lists = take_varint(buf, &mut pos).ok_or(bad("esquema truncado"))? as u16;
            let metric = VectorMetric::from_u8(*take(&mut pos, 1)?.first().expect("len 1"))
                .ok_or(bad("métrica vectorial desconocida"))?;
            let dim = u32::from_le_bytes(take(&mut pos, 4)?.try_into().expect("rango fijo"));
            let nlen = take_varint(buf, &mut pos).ok_or(bad("esquema truncado"))? as usize;
            let name = String::from_utf8(take(&mut pos, nlen)?.to_vec())
                .map_err(|_| bad("nombre de índice vectorial no UTF-8"))?;
            vector_indexes.push(VectorIndexDef {
                name,
                vidx_id,
                column,
                lists,
                metric,
                dim,
            });
        }
    }

    if pos != buf.len() {
        return Err(bad("bytes sobrantes tras el esquema"));
    }

    let rowid_alias = (alias != NO_ALIAS).then_some(alias as usize);
    if rowid_alias.is_some_and(|i| i >= columns.len()) {
        return Err(bad("alias de rowid fuera de rango"));
    }
    Ok(TableDef {
        name: name.to_owned(),
        table_id,
        rowid_alias,
        columns,
        indexes,
        fts_indexes,
        vector_indexes,
        logical_order,
        foreign_keys,
        checks,
    })
}

// --- operaciones de catálogo ---

fn validate_name(name: &str, what: &'static str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return Err(Error::InvalidInput(what));
    }
    Ok(())
}

/// Normaliza las FKs declaradas (en columna y a nivel de tabla) a `ForeignKey`
/// con posiciones físicas. Valida: las columnas hijas existen; el padre existe;
/// las columnas del padre son su PK (⇒ `parent_columns` vacío, referencia por
/// rowid) o están cubiertas por un índice `UNIQUE`; la aridad coincide.
fn resolve_foreign_keys<S: NodeSource>(
    s: &S,
    root: PageId,
    spec: &TableSpec,
    rowid_alias: Option<usize>,
) -> Result<Vec<ForeignKey>> {
    // FKs de columna + de tabla, todas en términos de NOMBRES.
    struct Pending {
        columns: Vec<String>,
        parent: String,
        parent_columns: Vec<String>,
        on_delete: FkAction,
        on_update: FkAction,
    }
    let mut pending: Vec<Pending> = Vec::new();
    for col in &spec.columns {
        if let Some(r) = &col.references {
            pending.push(Pending {
                columns: vec![col.name.clone()],
                parent: r.parent.clone(),
                parent_columns: r.parent_column.clone().into_iter().collect(),
                on_delete: r.on_delete,
                on_update: r.on_update,
            });
        }
    }
    for fk in &spec.foreign_keys {
        pending.push(Pending {
            columns: fk.columns.clone(),
            parent: fk.parent.clone(),
            parent_columns: fk.parent_columns.clone(),
            on_delete: fk.on_delete,
            on_update: fk.on_update,
        });
    }

    let child_pos = |name: &str| spec.columns.iter().position(|c| c.name == name);
    let mut out = Vec::with_capacity(pending.len());
    for pf in &pending {
        let mut columns = Vec::with_capacity(pf.columns.len());
        for cn in &pf.columns {
            columns.push(child_pos(cn).ok_or(Error::Constraint("columna hija de FK desconocida"))?);
        }
        // El padre (auto-referencia usa las columnas en construcción; sus índices
        // aún no existen ⇒ solo se puede auto-referenciar la PK).
        let (parent_names, parent_rowid, parent_indexes): (
            Vec<String>,
            Option<usize>,
            Vec<IndexDef>,
        ) = if pf.parent == spec.name {
            (
                spec.columns.iter().map(|c| c.name.clone()).collect(),
                rowid_alias,
                Vec::new(),
            )
        } else {
            match get_table(s, root, &pf.parent)? {
                Some(p) => (
                    p.columns.iter().map(|c| c.name.clone()).collect(),
                    p.rowid_alias,
                    p.indexes.clone(),
                ),
                None => return Err(Error::Constraint("tabla padre de FK desconocida")),
            }
        };
        let mut parent_positions = Vec::with_capacity(pf.parent_columns.len());
        for pn in &pf.parent_columns {
            parent_positions.push(
                parent_names
                    .iter()
                    .position(|n| n == pn)
                    .ok_or(Error::Constraint("columna del padre de FK desconocida"))?,
            );
        }
        // Referencia por rowid (PK): columnas del padre vacías, o exactamente la PK.
        let is_pk_ref = parent_positions.is_empty()
            || (parent_positions.len() == 1 && Some(parent_positions[0]) == parent_rowid);
        if is_pk_ref {
            if columns.len() != 1 {
                return Err(Error::Constraint(
                    "una FK a la PK referencia una sola columna",
                ));
            }
            if parent_rowid.is_none() {
                return Err(Error::Constraint(
                    "la tabla padre de una FK necesita PRIMARY KEY",
                ));
            }
            parent_positions.clear(); // = PK por rowid
        } else {
            if columns.len() != parent_positions.len() {
                return Err(Error::Constraint(
                    "la FK y las columnas del padre tienen distinta aridad",
                ));
            }
            let covered = parent_indexes.iter().any(|ix| {
                ix.unique
                    && ix.columns.len() == parent_positions.len()
                    && parent_positions.iter().all(|p| ix.columns.contains(p))
            });
            if !covered {
                return Err(Error::Constraint(
                    "las columnas referenciadas por una FK deben ser PRIMARY KEY o tener un índice UNIQUE",
                ));
            }
        }
        out.push(ForeignKey {
            columns,
            parent: pf.parent.clone(),
            parent_columns: parent_positions,
            on_delete: pf.on_delete,
            on_update: pf.on_update,
        });
    }
    Ok(out)
}

pub fn create_table<S: NodeStore>(
    s: &mut S,
    root: PageId,
    spec: &TableSpec,
) -> Result<(PageId, TableDef)> {
    validate_name(&spec.name, "nombre de tabla vacío o de más de 128 bytes")?;
    if spec.columns.is_empty() || spec.columns.len() > MAX_COLUMNS {
        return Err(Error::InvalidInput(
            "la tabla necesita entre 1 y 255 columnas",
        ));
    }
    let mut rowid_alias = None;
    for (i, col) in spec.columns.iter().enumerate() {
        validate_name(&col.name, "nombre de columna vacío o de más de 128 bytes")?;
        if spec.columns[..i].iter().any(|c| c.name == col.name) {
            return Err(Error::InvalidInput("nombre de columna duplicado"));
        }
        if col.primary_key {
            if rowid_alias.is_some() {
                return Err(Error::InvalidInput("solo se admite una PRIMARY KEY"));
            }
            if col.col_type != ColType::Integer {
                return Err(Error::InvalidInput("en v1 la PRIMARY KEY debe ser INTEGER"));
            }
            rowid_alias = Some(i);
        }
    }
    if get_table(s, root, &spec.name)?.is_some() {
        return Err(Error::Constraint("la tabla ya existe"));
    }
    if get_view(s, root, &spec.name)?.is_some() {
        return Err(Error::Constraint("ya existe una vista con ese nombre"));
    }

    // Asignar table_id desde el contador global del catálogo.
    let table_id = match btree::get(s, root, &meta_key())? {
        Some(v) => u32::from_le_bytes(
            v.as_slice()
                .try_into()
                .map_err(|_| Error::CorruptRecord("contador de tablas"))?,
        ),
        None => 1,
    };
    let mut root = btree::insert(s, root, &meta_key(), &(table_id + 1).to_le_bytes())?;

    // Claves foráneas (columna + tabla, posiblemente compuestas). El padre debe
    // existir; las columnas referenciadas deben ser su PK o tener un índice UNIQUE.
    // Se permite auto-referencia (padre = esta tabla) a su PK.
    let foreign_keys = resolve_foreign_keys(s, root, spec, rowid_alias)?;

    // Predicados CHECK: texto de columna + texto de tabla.
    let mut checks: Vec<String> = spec
        .columns
        .iter()
        .filter_map(|c| c.check.clone())
        .collect();
    checks.extend(spec.checks.iter().cloned());

    let def = TableDef {
        name: spec.name.clone(),
        table_id,
        rowid_alias,
        columns: spec
            .columns
            .iter()
            .map(|c| ColumnDef {
                name: c.name.clone(),
                col_type: c.col_type,
                // El alias del rowid nunca es NULL por construcción.
                not_null: c.not_null || c.primary_key,
                default: c.default.clone(),
                dropped: false,
            })
            .collect(),
        indexes: Vec::new(),
        fts_indexes: Vec::new(),
        vector_indexes: Vec::new(),
        logical_order: (0..spec.columns.len()).collect(),
        foreign_keys,
        checks,
    };
    root = btree::insert(s, root, &table_key(&spec.name), &encode_def(&def))?;

    // Restricciones UNIQUE (columna + tabla) → un índice UNIQUE por cada una. Se
    // crean tras escribir la tabla (`create_index` la relee). El nombre se autogenera.
    let mut unique_cols: Vec<Vec<String>> = spec
        .columns
        .iter()
        .filter(|c| c.unique)
        .map(|c| vec![c.name.clone()])
        .collect();
    unique_cols.extend(spec.uniques.iter().cloned());
    let mut def = def;
    for (i, cols) in unique_cols.iter().enumerate() {
        let positions: Vec<usize> = cols
            .iter()
            .map(|name| {
                def.columns
                    .iter()
                    .position(|c| &c.name == name)
                    .ok_or(Error::Constraint("columna UNIQUE desconocida"))
            })
            .collect::<Result<_>>()?;
        // `UNIQUE` sobre la PK es redundante (el rowid ya es único); se omite.
        if positions.len() == 1 && Some(positions[0]) == def.rowid_alias {
            continue;
        }
        let ix_name = format!("uq_{}_{}", spec.name, i);
        root = create_index(s, root, &spec.name, &ix_name, &positions, true)?;
        // Releer la def para que el llamador reciba los índices ya añadidos.
        def = get_table(s, root, &spec.name)?.ok_or(Error::CorruptRecord("tabla recién creada"))?;
    }
    Ok((root, def))
}

/// Añade una columna al final de una tabla (`ALTER TABLE ADD COLUMN`). **No
/// reescribe filas**: las existentes leerán la columna como su `DEFAULT` (o
/// NULL) gracias a `finish_row`. Falla si la tabla no existe, la columna ya
/// existe, se exceden las columnas, o es `NOT NULL` sin `DEFAULT` (las filas
/// viejas violarían la restricción).
pub fn add_column<S: NodeStore>(
    s: &mut S,
    root: PageId,
    table_name: &str,
    col: ColumnDef,
) -> Result<(PageId, TableDef)> {
    validate_name(&col.name, "nombre de columna vacío o de más de 128 bytes")?;
    let mut def = get_table(s, root, table_name)?.ok_or(Error::Constraint("tabla desconocida"))?;
    if def.columns.iter().any(|c| c.name == col.name) {
        return Err(Error::Constraint("la columna ya existe"));
    }
    if def.columns.len() >= MAX_COLUMNS {
        return Err(Error::InvalidInput("se excede el máximo de columnas"));
    }
    if col.not_null && col.default.is_none() {
        return Err(Error::Constraint(
            "ADD COLUMN NOT NULL requiere un DEFAULT (las filas existentes serían NULL)",
        ));
    }
    def.columns.push(col);
    // La columna nueva aparece **última** en el orden lógico (igual que su posición
    // física): conserva la semántica de `ADD COLUMN` (se añade al final).
    def.logical_order.push(def.columns.len() - 1);
    let root = btree::insert(s, root, &table_key(table_name), &encode_def(&def))?;
    Ok((root, def))
}

/// Reordena lógicamente una columna (`ALTER TABLE … MOVE COLUMN c {FIRST | BEFORE
/// x | AFTER x}`). **No reescribe filas** ni toca la posición física: solo cambia
/// el orden de presentación en el catálogo. Falla si la tabla o la columna no
/// existen, o si la referencia es la propia columna o no existe.
pub fn move_column<S: NodeStore>(
    s: &mut S,
    root: PageId,
    table_name: &str,
    col: &str,
    pos: &ColumnPos,
) -> Result<(PageId, TableDef)> {
    let mut def = get_table(s, root, table_name)?.ok_or(Error::Constraint("tabla desconocida"))?;
    let phys = def
        .columns
        .iter()
        .position(|c| c.name == col)
        .ok_or(Error::Constraint("columna desconocida"))?;
    // Secuencia lógica sin la columna que se mueve; luego se reinserta en destino.
    let mut seq: Vec<usize> = def
        .logical_order
        .iter()
        .copied()
        .filter(|&p| p != phys)
        .collect();
    let at = match pos {
        ColumnPos::First => 0,
        ColumnPos::Before(name) | ColumnPos::After(name) => {
            if name == col {
                return Err(Error::InvalidInput(
                    "MOVE COLUMN: la referencia no puede ser la propia columna",
                ));
            }
            let tphys = def
                .columns
                .iter()
                .position(|c| c.name == *name)
                .ok_or(Error::Constraint("columna de referencia desconocida"))?;
            let idx = seq
                .iter()
                .position(|&p| p == tphys)
                .expect("la referencia está en la permutación");
            if matches!(pos, ColumnPos::After(_)) {
                idx + 1
            } else {
                idx
            }
        }
    };
    seq.insert(at, phys);
    def.logical_order = seq;
    let root = btree::insert(s, root, &table_key(table_name), &encode_def(&def))?;
    Ok((root, def))
}

/// Fija el orden lógico **completo** (`ALTER TABLE … REORDER COLUMNS (…)`).
/// `order` debe listar todas las columnas exactamente una vez. No toca nada
/// físico ni reescribe filas.
pub fn reorder_columns<S: NodeStore>(
    s: &mut S,
    root: PageId,
    table_name: &str,
    order: &[String],
) -> Result<(PageId, TableDef)> {
    let mut def = get_table(s, root, table_name)?.ok_or(Error::Constraint("tabla desconocida"))?;
    if order.len() != def.columns.len() {
        return Err(Error::InvalidInput(
            "REORDER COLUMNS debe listar todas las columnas exactamente una vez",
        ));
    }
    let mut new_logical = Vec::with_capacity(order.len());
    let mut seen = vec![false; def.columns.len()];
    for name in order {
        let phys = def
            .columns
            .iter()
            .position(|c| c.name == *name)
            .ok_or(Error::Constraint("columna desconocida en REORDER COLUMNS"))?;
        if seen[phys] {
            return Err(Error::InvalidInput("columna repetida en REORDER COLUMNS"));
        }
        seen[phys] = true;
        new_logical.push(phys);
    }
    def.logical_order = new_logical;
    let root = btree::insert(s, root, &table_key(table_name), &encode_def(&def))?;
    Ok((root, def))
}

/// Renombra una columna (`ALTER TABLE … RENAME COLUMN old TO new`). Solo cambia el
/// nombre en el catálogo: posición física, índices y FKs (por posición) intactos.
/// OJO: vistas/triggers que la nombren por texto quedan obsoletos.
pub fn rename_column<S: NodeStore>(
    s: &mut S,
    root: PageId,
    table_name: &str,
    old: &str,
    new: &str,
) -> Result<(PageId, TableDef)> {
    validate_name(new, "nombre de columna vacío o de más de 128 bytes")?;
    let mut def = get_table(s, root, table_name)?.ok_or(Error::Constraint("tabla desconocida"))?;
    let phys = def
        .columns
        .iter()
        .position(|c| !c.dropped && c.name == old)
        .ok_or(Error::Constraint("columna desconocida"))?;
    if def.columns.iter().any(|c| !c.dropped && c.name == new) {
        return Err(Error::Constraint("ya existe una columna con ese nombre"));
    }
    def.columns[phys].name = new.to_owned();
    let root = btree::insert(s, root, &table_key(table_name), &encode_def(&def))?;
    Ok((root, def))
}

/// `ALTER TABLE … DROP COLUMN` **lógico** (tombstone): marca la columna como
/// borrada y la saca del orden lógico, sin tocar la posición física ni reescribir
/// filas (time-travel intacto; los bytes muertos los reclama el vacuum). Falla con
/// la PK, la última columna visible, o una columna en un índice o FK (bórralos
/// antes).
pub fn drop_column<S: NodeStore>(
    s: &mut S,
    root: PageId,
    table_name: &str,
    col: &str,
) -> Result<(PageId, TableDef)> {
    let mut def = get_table(s, root, table_name)?.ok_or(Error::Constraint("tabla desconocida"))?;
    let phys = def
        .columns
        .iter()
        .position(|c| !c.dropped && c.name == col)
        .ok_or(Error::Constraint("columna desconocida"))?;
    if def.rowid_alias == Some(phys) {
        return Err(Error::Constraint("no se puede borrar la PRIMARY KEY"));
    }
    if def.columns.iter().filter(|c| !c.dropped).count() <= 1 {
        return Err(Error::Constraint("una tabla necesita al menos una columna"));
    }
    if def.indexes.iter().any(|i| i.columns.contains(&phys)) {
        return Err(Error::Constraint(
            "la columna está en un índice (haz DROP INDEX primero)",
        ));
    }
    if def.foreign_keys.iter().any(|fk| fk.columns.contains(&phys)) {
        return Err(Error::Constraint("la columna tiene una clave foránea"));
    }
    // La columna se queda en `logical_order` (sigue siendo permutación de
    // `0..ncols` ⇒ serialización intacta); la presentación la filtra por `dropped`.
    def.columns[phys].dropped = true;
    let root = btree::insert(s, root, &table_key(table_name), &encode_def(&def))?;
    Ok((root, def))
}

pub fn get_table<S: NodeSource>(src: &S, root: PageId, name: &str) -> Result<Option<TableDef>> {
    match btree::get(src, root, &table_key(name))? {
        Some(bytes) => Ok(Some(decode_def(name, &bytes)?)),
        None => Ok(None),
    }
}

/// Todas las tablas del catálogo, en orden de nombre (escanea `[0x00,0x01,*]`).
/// Para herramientas que listan el esquema (CLI, introspección).
pub fn list_tables<S: NodeSource>(src: &S, root: PageId) -> Result<Vec<TableDef>> {
    let prefix = [KS_CATALOG, CAT_TABLE];
    let mut out = Vec::new();
    for item in btree::scan_from(src, root, &prefix)? {
        let (key, val) = item?;
        if !key.starts_with(&prefix) {
            break;
        }
        let name = std::str::from_utf8(&key[prefix.len()..])
            .map_err(|_| Error::CorruptRecord("nombre de tabla no UTF-8"))?;
        out.push(decode_def(name, &val)?);
    }
    Ok(out)
}

/// Elimina tabla, contador y todas sus filas. `false` si no existía.
pub fn drop_table<S: NodeStore>(s: &mut S, root: PageId, name: &str) -> Result<(PageId, bool)> {
    let Some(def) = get_table(s, root, name)? else {
        return Ok((root, false));
    };
    let rowids: Vec<i64> = scan_table(s, root, &def)?
        .map(|r| r.map(|(id, _)| id))
        .collect::<Result<_>>()?;
    let mut root = root;
    for rowid in rowids {
        (root, _) = btree::delete(s, root, &row_key(def.table_id, rowid))?;
    }
    // Índices de la tabla: sus entradas y su ref de nombre global.
    for idx in &def.indexes {
        root = delete_all_index_entries(s, root, idx.index_id)?;
        (root, _) = btree::delete(s, root, &index_ref_key(&idx.name))?;
    }
    (root, _) = btree::delete(s, root, &counter_key(def.table_id))?;
    (root, _) = btree::delete(s, root, &table_key(name))?;
    Ok((root, true))
}

// --- vistas: SELECT con nombre guardado como texto en el catálogo ---

/// `[0x00, 0x05, nombre]` → texto SQL del SELECT que define la vista.
fn view_key(name: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(2 + name.len());
    k.extend_from_slice(&[KS_CATALOG, CAT_VIEW]);
    k.extend_from_slice(name.as_bytes());
    k
}

/// Crea una vista (su SELECT se guarda como **texto**, se re-parsea al usarla, como
/// en SQLite). Falla si ya existe una tabla o una vista con ese nombre.
pub fn create_view<S: NodeStore>(
    s: &mut S,
    root: PageId,
    name: &str,
    select_sql: &str,
) -> Result<PageId> {
    validate_name(name, "nombre de vista vacío o de más de 128 bytes")?;
    if get_table(s, root, name)?.is_some() {
        return Err(Error::Constraint("ya existe una tabla con ese nombre"));
    }
    if get_view(s, root, name)?.is_some() {
        return Err(Error::Constraint("la vista ya existe"));
    }
    btree::insert(s, root, &view_key(name), select_sql.as_bytes())
}

/// Borra una vista. `false` si no existía.
pub fn drop_view<S: NodeStore>(s: &mut S, root: PageId, name: &str) -> Result<(PageId, bool)> {
    if get_view(s, root, name)?.is_none() {
        return Ok((root, false));
    }
    let (root, _) = btree::delete(s, root, &view_key(name))?;
    Ok((root, true))
}

/// El SELECT (texto) de una vista, o `None` si no existe.
pub fn get_view<S: NodeSource>(src: &S, root: PageId, name: &str) -> Result<Option<String>> {
    match btree::get(src, root, &view_key(name))? {
        Some(bytes) => Ok(Some(
            String::from_utf8(bytes).map_err(|_| Error::CorruptRecord("vista no UTF-8"))?,
        )),
        None => Ok(None),
    }
}

/// Todas las vistas `(nombre, SELECT)`, en orden de nombre (para `.schema`).
pub fn list_views<S: NodeSource>(src: &S, root: PageId) -> Result<Vec<(String, String)>> {
    let prefix = [KS_CATALOG, CAT_VIEW];
    let mut out = Vec::new();
    for item in btree::scan_from(src, root, &prefix)? {
        let (key, val) = item?;
        if !key.starts_with(&prefix) {
            break;
        }
        let name = std::str::from_utf8(&key[prefix.len()..])
            .map_err(|_| Error::CorruptRecord("nombre de vista no UTF-8"))?
            .to_owned();
        let sql = String::from_utf8(val).map_err(|_| Error::CorruptRecord("vista no UTF-8"))?;
        out.push((name, sql));
    }
    Ok(out)
}

// --- triggers: cuerpo (DML) disparado por fila en INSERT/UPDATE/DELETE ---

fn trigger_key(name: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(2 + name.len());
    k.extend_from_slice(&[KS_CATALOG, CAT_TRIGGER]);
    k.extend_from_slice(name.as_bytes());
    k
}

/// `[timing u8][event u8][tabla_len varint][tabla][cuerpo]` (el nombre va en la clave).
/// Marcador del formato de registro de trigger v2 (con `for_each` e INSTEAD OF).
/// El v1 empezaba con el byte de `timing` (0/1), así que `0xF1` lo distingue sin
/// ambigüedad: un registro antiguo nunca empieza por `0xF1`.
const TRIGGER_FMT_V2: u8 = 0xF1;

fn encode_trigger(t: &TriggerDef) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + t.table.len() + t.body.len());
    out.push(TRIGGER_FMT_V2);
    out.push(t.timing.as_u8());
    out.push(t.event.as_u8());
    out.push(t.for_each.as_u8());
    put_varint(&mut out, t.table.len() as u64);
    out.extend_from_slice(t.table.as_bytes());
    out.extend_from_slice(t.body.as_bytes());
    out
}

fn decode_trigger(name: &str, buf: &[u8]) -> Result<TriggerDef> {
    let bad = || Error::CorruptRecord("trigger mal formado");
    // v2: `[0xF1][timing][event][for_each][tlen][table][body]`. v1 (antiguo):
    // `[timing][event][tlen][table][body]`, row-level y sin INSTEAD OF.
    let (timing_at, for_each) = if buf.first().copied() == Some(TRIGGER_FMT_V2) {
        (
            1usize,
            TriggerForEach::from_u8(*buf.get(3).ok_or_else(bad)?).ok_or_else(bad)?,
        )
    } else {
        (0usize, TriggerForEach::Row)
    };
    let timing = TriggerTiming::from_u8(*buf.get(timing_at).ok_or_else(bad)?).ok_or_else(bad)?;
    let event = TriggerEvent::from_u8(*buf.get(timing_at + 1).ok_or_else(bad)?).ok_or_else(bad)?;
    // El v2 inserta el byte `for_each` entre `event` y `tlen`; el v1 no.
    let mut pos = timing_at + 2 + usize::from(timing_at == 1);
    let tlen = take_varint(buf, &mut pos).ok_or_else(bad)? as usize;
    let table =
        String::from_utf8(buf.get(pos..pos + tlen).ok_or_else(bad)?.to_vec()).map_err(|_| bad())?;
    pos += tlen;
    let body = String::from_utf8(buf.get(pos..).ok_or_else(bad)?.to_vec()).map_err(|_| bad())?;
    Ok(TriggerDef {
        name: name.to_owned(),
        timing,
        event,
        for_each,
        table,
        body,
    })
}

/// Crea un trigger. Falla si ya existe uno con ese nombre o si la tabla destino
/// no existe.
pub fn create_trigger<S: NodeStore>(s: &mut S, root: PageId, t: &TriggerDef) -> Result<PageId> {
    validate_name(&t.name, "nombre de trigger vacío o de más de 128 bytes")?;
    if get_trigger(s, root, &t.name)?.is_some() {
        return Err(Error::Constraint("el trigger ya existe"));
    }
    // INSTEAD OF → solo sobre una vista y row-level; BEFORE/AFTER → solo tabla.
    match t.timing {
        TriggerTiming::InsteadOf => {
            if get_view(s, root, &t.table)?.is_none() {
                return Err(Error::Constraint("INSTEAD OF requiere una vista existente"));
            }
            if t.for_each != TriggerForEach::Row {
                return Err(Error::Constraint("INSTEAD OF debe ser FOR EACH ROW"));
            }
        }
        TriggerTiming::Before | TriggerTiming::After => {
            if get_table(s, root, &t.table)?.is_none() {
                return Err(Error::Constraint(
                    "un trigger BEFORE/AFTER requiere una tabla existente",
                ));
            }
        }
    }
    btree::insert(s, root, &trigger_key(&t.name), &encode_trigger(t))
}

/// Borra un trigger. `false` si no existía.
pub fn drop_trigger<S: NodeStore>(s: &mut S, root: PageId, name: &str) -> Result<(PageId, bool)> {
    if get_trigger(s, root, name)?.is_none() {
        return Ok((root, false));
    }
    let (root, _) = btree::delete(s, root, &trigger_key(name))?;
    Ok((root, true))
}

pub fn get_trigger<S: NodeSource>(src: &S, root: PageId, name: &str) -> Result<Option<TriggerDef>> {
    match btree::get(src, root, &trigger_key(name))? {
        Some(bytes) => Ok(Some(decode_trigger(name, &bytes)?)),
        None => Ok(None),
    }
}

/// Todos los triggers, en orden de nombre.
pub fn list_triggers<S: NodeSource>(src: &S, root: PageId) -> Result<Vec<TriggerDef>> {
    let prefix = [KS_CATALOG, CAT_TRIGGER];
    let mut out = Vec::new();
    for item in btree::scan_from(src, root, &prefix)? {
        let (key, val) = item?;
        if !key.starts_with(&prefix) {
            break;
        }
        let name = std::str::from_utf8(&key[prefix.len()..])
            .map_err(|_| Error::CorruptRecord("nombre de trigger no UTF-8"))?;
        out.push(decode_trigger(name, &val)?);
    }
    Ok(out)
}

// --- índices secundarios: creación, borrado, mantenimiento y consulta ---

fn alloc_index_id<S: NodeStore>(s: &mut S, root: PageId) -> Result<(u32, PageId)> {
    let next = match btree::get(s, root, &index_counter_key())? {
        Some(v) => u32::from_le_bytes(
            v.as_slice()
                .try_into()
                .map_err(|_| Error::CorruptRecord("contador de índices"))?,
        ),
        None => 1,
    };
    let root = btree::insert(s, root, &index_counter_key(), &(next + 1).to_le_bytes())?;
    Ok((next, root))
}

/// Crea un índice sobre `columns` de `table_name` y lo **rellena** con las filas
/// existentes. El nombre es único en toda la base. No se indexa la PRIMARY KEY
/// (ya es la clave primaria). `unique` se guarda; su enforcement llega en el
/// slice 2.
pub fn create_index<S: NodeStore>(
    s: &mut S,
    root: PageId,
    table_name: &str,
    index_name: &str,
    columns: &[usize],
    unique: bool,
) -> Result<PageId> {
    validate_name(index_name, "nombre de índice vacío o de más de 128 bytes")?;
    let mut def = get_table(s, root, table_name)?.ok_or(Error::Constraint("tabla desconocida"))?;
    if columns.is_empty() || columns.len() > MAX_COLUMNS {
        return Err(Error::InvalidInput(
            "un índice necesita entre 1 y 255 columnas",
        ));
    }
    for &c in columns {
        if c >= def.columns.len() {
            return Err(Error::InvalidInput("columna de índice inexistente"));
        }
        if Some(c) == def.rowid_alias {
            return Err(Error::InvalidInput(
                "no se indexa la PRIMARY KEY: ya es la clave primaria",
            ));
        }
    }
    if btree::get(s, root, &index_ref_key(index_name))?.is_some() {
        return Err(Error::Constraint("ya existe un índice con ese nombre"));
    }

    let (index_id, mut root) = alloc_index_id(s, root)?;
    let idx = IndexDef {
        name: index_name.to_owned(),
        index_id,
        columns: columns.to_vec(),
        unique,
    };
    // Backfill: una entrada por cada fila existente. Un índice UNIQUE rechaza la
    // creación si las filas existentes ya contienen valores duplicados.
    let rows: Vec<(i64, Vec<Value>)> = scan_table(s, root, &def)?.collect::<Result<_>>()?;
    for (rowid, values) in rows {
        if idx.unique && !any_indexed_null(&idx, &values) {
            let prefix = index_value_prefix_of(&idx, &values);
            if value_held_by_other(s, root, &prefix, rowid)? {
                return Err(Error::Constraint(
                    "CREATE UNIQUE INDEX: ya hay valores duplicados",
                ));
            }
        }
        root = btree::insert(s, root, &index_entry_key(&idx, &values, rowid), &[])?;
    }
    // Persistir: ref de nombre global + esquema con el índice.
    root = btree::insert(s, root, &index_ref_key(index_name), table_name.as_bytes())?;
    def.indexes.push(idx);
    root = btree::insert(s, root, &table_key(table_name), &encode_def(&def))?;
    Ok(root)
}

/// Borra un índice por su nombre global: sus entradas, su ref y su entrada en el
/// esquema de la tabla. `false` si no existía.
pub fn drop_index<S: NodeStore>(
    s: &mut S,
    root: PageId,
    index_name: &str,
) -> Result<(PageId, bool)> {
    let Some(table_bytes) = btree::get(s, root, &index_ref_key(index_name))? else {
        return Ok((root, false));
    };
    let table_name = String::from_utf8(table_bytes)
        .map_err(|_| Error::CorruptRecord("ref de índice no UTF-8"))?;
    let mut def =
        get_table(s, root, &table_name)?.ok_or(Error::CorruptRecord("índice sin su tabla"))?;
    let Some(pos) = def.indexes.iter().position(|i| i.name == index_name) else {
        return Err(Error::CorruptRecord(
            "ref de índice sin entrada en el esquema",
        ));
    };
    let idx = def.indexes.remove(pos);
    let mut root = delete_all_index_entries(s, root, idx.index_id)?;
    (root, _) = btree::delete(s, root, &index_ref_key(index_name))?;
    root = btree::insert(s, root, &table_key(&table_name), &encode_def(&def))?;
    Ok((root, true))
}

/// `true` si existe un índice con ese nombre global (ref en `[0x00,0x04,nombre]`).
pub fn index_exists<S: NodeSource>(src: &S, root: PageId, name: &str) -> Result<bool> {
    Ok(btree::get(src, root, &index_ref_key(name))?.is_some())
}

fn delete_all_index_entries<S: NodeStore>(
    s: &mut S,
    root: PageId,
    index_id: u32,
) -> Result<PageId> {
    let prefix = index_id_prefix(index_id);
    let mut keys = Vec::new();
    for item in btree::scan_from(s, root, &prefix)? {
        let (k, _) = item?;
        if !k.starts_with(&prefix) {
            break;
        }
        keys.push(k);
    }
    let mut root = root;
    for k in keys {
        (root, _) = btree::delete(s, root, &k)?;
    }
    Ok(root)
}

/// Inserta las entradas de índice de una fila (todos los índices de la tabla).
fn insert_index_entries<S: NodeStore>(
    s: &mut S,
    root: PageId,
    table: &TableDef,
    rowid: i64,
    record: &[Value],
) -> Result<PageId> {
    let mut root = root;
    for idx in &table.indexes {
        if idx.unique && !any_indexed_null(idx, record) {
            // UNIQUE: ese valor no debe existir ya en otra fila (SQL permite
            // varios NULL, así que solo se comprueba si ninguna columna es NULL).
            let prefix = index_value_prefix_of(idx, record);
            if value_held_by_other(s, root, &prefix, rowid)? {
                return Err(Error::Constraint("violación de restricción UNIQUE"));
            }
        }
        root = btree::insert(s, root, &index_entry_key(idx, record, rowid), &[])?;
    }
    Ok(root)
}

fn any_indexed_null(idx: &IndexDef, record: &[Value]) -> bool {
    idx.columns
        .iter()
        .any(|&c| matches!(record[c], Value::Null))
}

/// Prefijo de valor de una fila para un índice: la clave de entrada sin el rowid.
fn index_value_prefix_of(idx: &IndexDef, record: &[Value]) -> Vec<u8> {
    let mut k = index_id_prefix(idx.index_id).to_vec();
    for &col in &idx.columns {
        keyenc::encode_index_value(&record[col], &mut k);
    }
    k
}

/// `true` si alguna entrada con ese prefijo de valor es de un rowid **distinto**
/// (dup-check de UNIQUE).
fn value_held_by_other<S: NodeSource>(
    s: &S,
    root: PageId,
    prefix: &[u8],
    rowid: i64,
) -> Result<bool> {
    let mut held = false;
    btree::for_each_prefix(s, root, prefix, |key| {
        if rowid_from_index_key(key)? != rowid {
            held = true;
        }
        Ok(())
    })?;
    Ok(held)
}

/// Borra las entradas de índice de una fila.
fn delete_index_entries<S: NodeStore>(
    s: &mut S,
    root: PageId,
    table: &TableDef,
    rowid: i64,
    record: &[Value],
) -> Result<PageId> {
    let mut root = root;
    for idx in &table.indexes {
        (root, _) = btree::delete(s, root, &index_entry_key(idx, record, rowid))?;
    }
    Ok(root)
}

// --- índices full-text (FTS): postings + stats BM25 (docs/12-fts.md) ---

/// `[0x00,0x07]` → contador de `fts_id`.
fn fts_counter_key() -> [u8; 2] {
    [KS_CATALOG, CAT_FTS_COUNTER]
}

/// `[0x00,0x08, nombre]` → tabla: ref global del índice FTS (nombre único en toda
/// la base, para que `DROP FULLTEXT INDEX nombre` lo ubique).
fn fts_ref_key(name: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(2 + name.len());
    k.extend_from_slice(&[KS_CATALOG, CAT_FTS_REF]);
    k.extend_from_slice(name.as_bytes());
    k
}

/// `[0x03, fts_id BE]` — prefijo de TODO el índice (postings + stats); un solo
/// barrido lo borra entero.
fn fts_id_prefix(fts_id: u32) -> [u8; 5] {
    let mut p = [0u8; 5];
    p[0] = KS_FTS;
    p[1..5].copy_from_slice(&fts_id.to_be_bytes());
    p
}

/// Prefijo `[0x03, fts_id, sub]`.
fn fts_sub_prefix(fts_id: u32, sub: u8) -> Vec<u8> {
    let mut k = fts_id_prefix(fts_id).to_vec();
    k.push(sub);
    k
}

/// Clave de un posting: `[0x03, fts_id, 0x00, term, rowid BE, field, pos]`. El
/// `term` va memcomparable (self-delimitado), así el `rowid` que sigue no se
/// confunde con su cola de longitud variable.
fn fts_posting_key(fts_id: u32, term: &str, rowid: i64, field: u8, pos: u32) -> Vec<u8> {
    let mut k = fts_sub_prefix(fts_id, FTS_POSTING);
    keyenc::encode_index_value_ref(ValueRef::Text(term), &mut k);
    k.extend_from_slice(&record::rowid_be(rowid));
    k.push(field);
    put_varint(&mut k, pos as u64);
    k
}

fn fts_doclen_key(fts_id: u32, rowid: i64) -> Vec<u8> {
    let mut k = fts_sub_prefix(fts_id, FTS_DOCLEN);
    k.extend_from_slice(&record::rowid_be(rowid));
    k
}

fn fts_df_key(fts_id: u32, term: &str) -> Vec<u8> {
    let mut k = fts_sub_prefix(fts_id, FTS_DF);
    keyenc::encode_index_value_ref(ValueRef::Text(term), &mut k);
    k
}

fn fts_global_key(fts_id: u32) -> Vec<u8> {
    fts_sub_prefix(fts_id, FTS_GLOBAL)
}

fn alloc_fts_id<S: NodeStore>(s: &mut S, root: PageId) -> Result<(u32, PageId)> {
    let next = match btree::get(s, root, &fts_counter_key())? {
        Some(v) => u32::from_le_bytes(
            v.as_slice()
                .try_into()
                .map_err(|_| Error::CorruptRecord("contador FTS"))?,
        ),
        None => 1,
    };
    let root = btree::insert(s, root, &fts_counter_key(), &(next + 1).to_le_bytes())?;
    Ok((next, root))
}

/// Lee un contador varint (`df`/`doclen`); ausente ⇒ 0.
fn fts_read_varint<S: NodeSource>(src: &S, root: PageId, key: &[u8]) -> Result<u64> {
    match btree::get(src, root, key)? {
        Some(v) => {
            let mut p = 0;
            take_varint(&v, &mut p).ok_or(Error::CorruptRecord("contador FTS varint"))
        }
        None => Ok(0),
    }
}

/// Lee las globales `{N docs, Σ tokens}`; ausente ⇒ (0, 0).
fn fts_read_global<S: NodeSource>(src: &S, root: PageId, fts_id: u32) -> Result<(u64, u64)> {
    match btree::get(src, root, &fts_global_key(fts_id))? {
        Some(v) => {
            let mut p = 0;
            let n = take_varint(&v, &mut p).ok_or(Error::CorruptRecord("FTS global N"))?;
            let total = take_varint(&v, &mut p).ok_or(Error::CorruptRecord("FTS global suma"))?;
            Ok((n, total))
        }
        None => Ok((0, 0)),
    }
}

/// Suma/resta `delta` a un contador varint; si llega a 0, borra la clave.
fn fts_adjust_varint<S: NodeStore>(
    s: &mut S,
    root: PageId,
    key: &[u8],
    add: bool,
    delta: u64,
) -> Result<PageId> {
    let cur = fts_read_varint(s, root, key)?;
    let new = if add {
        cur + delta
    } else {
        cur.saturating_sub(delta)
    };
    if new == 0 {
        let (root, _) = btree::delete(s, root, key)?;
        Ok(root)
    } else {
        let mut v = Vec::new();
        put_varint(&mut v, new);
        btree::insert(s, root, key, &v)
    }
}

/// Ajusta las globales: ±1 doc y ±`doclen` tokens; si N llega a 0, borra la clave.
fn fts_adjust_global<S: NodeStore>(
    s: &mut S,
    root: PageId,
    fts_id: u32,
    add: bool,
    doclen: u64,
) -> Result<PageId> {
    let (n, total) = fts_read_global(s, root, fts_id)?;
    let (n, total) = if add {
        (n + 1, total + doclen)
    } else {
        (n.saturating_sub(1), total.saturating_sub(doclen))
    };
    let key = fts_global_key(fts_id);
    if n == 0 {
        let (root, _) = btree::delete(s, root, &key)?;
        Ok(root)
    } else {
        let mut v = Vec::new();
        put_varint(&mut v, n);
        put_varint(&mut v, total);
        btree::insert(s, root, &key, &v)
    }
}

/// Aplica (`add`) o revierte (`!add`) la contribución de una fila a un índice FTS:
/// sus postings (uno por token de cada columna indexada) y las stats —`doclen`,
/// `df` por término distinto y globales—. Re-tokeniza el registro; como la
/// tokenización es determinista, revertir reconstruye exactamente las claves que
/// se insertaron.
fn apply_fts_row<S: NodeStore>(
    s: &mut S,
    mut root: PageId,
    fts: &FtsIndexDef,
    rowid: i64,
    record: &[Value],
    add: bool,
) -> Result<PageId> {
    let tk = crate::fts::tokenizer_for(&fts.tokenizer)?;
    let mut buf = Vec::new();
    let mut doc_terms: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut doclen: u64 = 0;
    for &col in &fts.columns {
        buf.clear();
        if let Value::Text(text) = &record[col] {
            tk.tokenize(text, &mut buf);
        }
        for token in &buf {
            let key = fts_posting_key(fts.fts_id, &token.text, rowid, col as u8, token.position);
            if add {
                root = btree::insert(s, root, &key, &[])?;
            } else {
                (root, _) = btree::delete(s, root, &key)?;
            }
            doclen += 1;
            doc_terms.insert(token.text.clone());
        }
    }
    // `df`: una vez por término distinto del documento.
    for term in &doc_terms {
        root = fts_adjust_varint(s, root, &fts_df_key(fts.fts_id, term), add, 1)?;
    }
    // `doclen` del documento (no escribimos clave para longitud 0; ausente ⇒ 0).
    let doclen_key = fts_doclen_key(fts.fts_id, rowid);
    if add {
        if doclen > 0 {
            let mut v = Vec::new();
            put_varint(&mut v, doclen);
            root = btree::insert(s, root, &doclen_key, &v)?;
        }
    } else {
        (root, _) = btree::delete(s, root, &doclen_key)?;
    }
    fts_adjust_global(s, root, fts.fts_id, add, doclen)
}

/// Inserta las entradas FTS de una fila (todos los índices FTS de la tabla).
pub fn insert_fts_entries<S: NodeStore>(
    s: &mut S,
    root: PageId,
    table: &TableDef,
    rowid: i64,
    record: &[Value],
) -> Result<PageId> {
    let mut root = root;
    for fts in &table.fts_indexes {
        root = apply_fts_row(s, root, fts, rowid, record, true)?;
    }
    Ok(root)
}

/// Igual que [`insert_fts_entries`] pero **materializa** el registro desde los
/// valores crudos del bulk-load (rellena defaults y normaliza vía
/// [`validate_record_into`]) antes de tokenizar, para que un bulk-insert que
/// omita una columna TEXT con `DEFAULT` produzca exactamente los mismos postings
/// que el camino normal. Reutiliza `rec_buf`.
pub(crate) fn insert_fts_entries_bulk<S: NodeStore>(
    s: &mut S,
    root: PageId,
    table: &TableDef,
    rowid: i64,
    values: &[Value],
    rec_buf: &mut Vec<Value>,
) -> Result<PageId> {
    validate_record_into(table, values, rec_buf)?;
    insert_fts_entries(s, root, table, rowid, rec_buf)
}

/// Borra las entradas FTS de una fila (todos los índices FTS de la tabla).
pub fn delete_fts_entries<S: NodeStore>(
    s: &mut S,
    root: PageId,
    table: &TableDef,
    rowid: i64,
    record: &[Value],
) -> Result<PageId> {
    let mut root = root;
    for fts in &table.fts_indexes {
        root = apply_fts_row(s, root, fts, rowid, record, false)?;
    }
    Ok(root)
}

/// Crea un índice FTS sobre columnas TEXT y lo **rellena** con las filas
/// existentes (backfill). Nombre único en toda la base.
pub fn create_fts_index<S: NodeStore>(
    s: &mut S,
    root: PageId,
    table_name: &str,
    index_name: &str,
    columns: &[usize],
    tokenizer: &str,
) -> Result<PageId> {
    validate_name(
        index_name,
        "nombre de índice FTS vacío o de más de 128 bytes",
    )?;
    crate::fts::tokenizer_for(tokenizer)?; // valida el nombre del tokenizer
    let mut def = get_table(s, root, table_name)?.ok_or(Error::Constraint("tabla desconocida"))?;
    if columns.is_empty() || columns.len() > MAX_COLUMNS {
        return Err(Error::InvalidInput(
            "un índice FTS necesita entre 1 y 255 columnas",
        ));
    }
    for &c in columns {
        if c >= def.columns.len() {
            return Err(Error::InvalidInput("columna de índice FTS inexistente"));
        }
        if !matches!(def.columns[c].col_type, ColType::Text) {
            return Err(Error::InvalidInput("FULLTEXT solo indexa columnas TEXT"));
        }
    }
    if btree::get(s, root, &fts_ref_key(index_name))?.is_some() {
        return Err(Error::Constraint("ya existe un índice FTS con ese nombre"));
    }
    let (fts_id, mut root) = alloc_fts_id(s, root)?;
    let idx = FtsIndexDef {
        name: index_name.to_owned(),
        fts_id,
        columns: columns.to_vec(),
        tokenizer: tokenizer.to_owned(),
    };
    // Backfill: una entrada por token de cada fila existente.
    let rows: Vec<(i64, Vec<Value>)> = scan_table(s, root, &def)?.collect::<Result<_>>()?;
    for (rowid, values) in rows {
        root = apply_fts_row(s, root, &idx, rowid, &values, true)?;
    }
    root = btree::insert(s, root, &fts_ref_key(index_name), table_name.as_bytes())?;
    def.fts_indexes.push(idx);
    root = btree::insert(s, root, &table_key(table_name), &encode_def(&def))?;
    Ok(root)
}

/// Borra un índice FTS por su nombre global: todas sus claves (`[0x03, fts_id]`),
/// su ref y su entrada en el esquema. `false` si no existía.
pub fn drop_fts_index<S: NodeStore>(
    s: &mut S,
    root: PageId,
    index_name: &str,
) -> Result<(PageId, bool)> {
    let Some(table_bytes) = btree::get(s, root, &fts_ref_key(index_name))? else {
        return Ok((root, false));
    };
    let table_name =
        String::from_utf8(table_bytes).map_err(|_| Error::CorruptRecord("ref FTS no UTF-8"))?;
    let mut def =
        get_table(s, root, &table_name)?.ok_or(Error::CorruptRecord("índice FTS sin su tabla"))?;
    let Some(pos) = def.fts_indexes.iter().position(|i| i.name == index_name) else {
        return Err(Error::CorruptRecord("ref FTS sin entrada en el esquema"));
    };
    let idx = def.fts_indexes.remove(pos);
    let mut root = delete_all_fts_entries(s, root, idx.fts_id)?;
    (root, _) = btree::delete(s, root, &fts_ref_key(index_name))?;
    root = btree::insert(s, root, &table_key(&table_name), &encode_def(&def))?;
    Ok((root, true))
}

/// `true` si existe un índice FTS con ese nombre global.
pub fn fts_index_exists<S: NodeSource>(src: &S, root: PageId, name: &str) -> Result<bool> {
    Ok(btree::get(src, root, &fts_ref_key(name))?.is_some())
}

fn delete_all_fts_entries<S: NodeStore>(s: &mut S, root: PageId, fts_id: u32) -> Result<PageId> {
    let prefix = fts_id_prefix(fts_id);
    let mut keys = Vec::new();
    for item in btree::scan_from(s, root, &prefix)? {
        let (k, _) = item?;
        if !k.starts_with(&prefix) {
            break;
        }
        keys.push(k);
    }
    let mut root = root;
    for k in keys {
        (root, _) = btree::delete(s, root, &k)?;
    }
    Ok(root)
}

// --- lectura FTS (la usan los tests y, más adelante, el planner/ranking) ---

/// rowids **distintos** (ordenados) que contienen `term` en este índice. La base
/// de `MATCH`: por término, los documentos que lo tienen.
pub fn fts_term_rowids<S: NodeSource>(
    src: &S,
    root: PageId,
    fts_id: u32,
    term: &str,
) -> Result<Vec<i64>> {
    let mut prefix = fts_sub_prefix(fts_id, FTS_POSTING);
    keyenc::encode_index_value_ref(ValueRef::Text(term), &mut prefix);
    let plen = prefix.len();
    let mut rowids = Vec::new();
    let mut last: Option<i64> = None;
    btree::for_each_prefix(src, root, &prefix, |key| {
        let rid = key
            .get(plen..plen + 8)
            .and_then(record::rowid_from_be)
            .ok_or(Error::CorruptRecord("rowid en posting FTS"))?;
        if last != Some(rid) {
            rowids.push(rid);
            last = Some(rid);
        }
        Ok(())
    })?;
    Ok(rowids)
}

/// Frecuencia documental de `term` (nº de docs que lo contienen).
pub fn fts_doc_freq<S: NodeSource>(src: &S, root: PageId, fts_id: u32, term: &str) -> Result<u64> {
    fts_read_varint(src, root, &fts_df_key(fts_id, term))
}

/// Longitud (nº de tokens) del documento `rowid`; 0 si no tiene.
pub fn fts_doc_len<S: NodeSource>(src: &S, root: PageId, fts_id: u32, rowid: i64) -> Result<u64> {
    fts_read_varint(src, root, &fts_doclen_key(fts_id, rowid))
}

/// Globales del índice: `(N docs, Σ tokens)` — para la longitud media de BM25.
pub fn fts_global_stats<S: NodeSource>(src: &S, root: PageId, fts_id: u32) -> Result<(u64, u64)> {
    fts_read_global(src, root, fts_id)
}

/// Stats que necesita BM25 para una consulta: `df` por cada término (en orden) y
/// las globales `(N docs, Σ tokens)`. Una sola pasada de lecturas.
pub fn fts_query_stats<S: NodeSource>(
    src: &S,
    root: PageId,
    fts_id: u32,
    terms: &[String],
) -> Result<(Vec<u64>, u64, u64)> {
    let df = terms
        .iter()
        .map(|t| fts_doc_freq(src, root, fts_id, t))
        .collect::<Result<Vec<u64>>>()?;
    let (n, total) = fts_global_stats(src, root, fts_id)?;
    Ok((df, n, total))
}

// --- evaluación de consultas MATCH contra el índice (docs/12-fts.md, fase 4) ---

/// Decodifica el sufijo `rowid BE(8) ‖ field(1) ‖ pos(varint)` de un posting.
fn decode_posting_suffix(suffix: &[u8]) -> Result<(i64, u8, u32)> {
    let rid = suffix
        .get(0..8)
        .and_then(record::rowid_from_be)
        .ok_or(Error::CorruptRecord("rowid en posting FTS"))?;
    let field = *suffix
        .get(8)
        .ok_or(Error::CorruptRecord("field en posting FTS"))?;
    let mut p = 9;
    let pos = take_varint(suffix, &mut p).ok_or(Error::CorruptRecord("pos en posting FTS"))? as u32;
    Ok((rid, field, pos))
}

/// Postings `(rowid, field, pos)` de un término exacto.
fn fts_term_postings<S: NodeSource>(
    src: &S,
    root: PageId,
    fts_id: u32,
    term: &str,
) -> Result<Vec<(i64, u8, u32)>> {
    let mut prefix = fts_sub_prefix(fts_id, FTS_POSTING);
    keyenc::encode_index_value_ref(ValueRef::Text(term), &mut prefix);
    let plen = prefix.len();
    let mut out = Vec::new();
    btree::for_each_prefix(src, root, &prefix, |key| {
        out.push(decode_posting_suffix(&key[plen..])?);
        Ok(())
    })?;
    Ok(out)
}

/// Postings de los términos que empiezan por `prefix_text` (`term*`). Los tokens
/// son texto normalizado sin `0x00`, así que el terminador del término es el
/// primer `0x00 0x00` tras el prefijo de escaneo.
fn fts_prefix_postings<S: NodeSource>(
    src: &S,
    root: PageId,
    fts_id: u32,
    prefix_text: &str,
) -> Result<Vec<(i64, u8, u32)>> {
    let mut scan = fts_sub_prefix(fts_id, FTS_POSTING);
    keyenc::encode_text_prefix(prefix_text, &mut scan);
    let slen = scan.len();
    let mut out = Vec::new();
    btree::for_each_prefix(src, root, &scan, |key| {
        let tail = key
            .get(slen..)
            .ok_or(Error::CorruptRecord("posting FTS truncado"))?;
        let term_end = tail
            .windows(2)
            .position(|w| w == [0x00, 0x00])
            .ok_or(Error::CorruptRecord("terminador de término FTS"))?;
        out.push(decode_posting_suffix(&tail[term_end + 2..])?);
        Ok(())
    })?;
    Ok(out)
}

/// rowids distintos y ordenados de unos postings, opcionalmente por `field`.
fn distinct_rowids(postings: &[(i64, u8, u32)], field: Option<u8>) -> Vec<i64> {
    let mut ids: Vec<i64> = postings
        .iter()
        .filter(|(_, f, _)| field.is_none_or(|ff| *f == ff))
        .map(|(r, _, _)| *r)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn intersect_sorted(a: &[i64], b: &[i64]) -> Vec<i64> {
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < a.len() && j < b.len() {
        if a[i] < b[j] {
            i += 1;
        } else if a[i] > b[j] {
            j += 1;
        } else {
            out.push(a[i]);
            i += 1;
            j += 1;
        }
    }
    out
}

fn union_sorted(a: &[i64], b: &[i64]) -> Vec<i64> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] < b[j] {
            out.push(a[i]);
            i += 1;
        } else if a[i] > b[j] {
            out.push(b[j]);
            j += 1;
        } else {
            out.push(a[i]);
            i += 1;
            j += 1;
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

fn difference_sorted(a: &[i64], b: &[i64]) -> Vec<i64> {
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < a.len() {
        if j >= b.len() || a[i] < b[j] {
            out.push(a[i]);
            i += 1;
        } else if a[i] > b[j] {
            j += 1;
        } else {
            i += 1;
            j += 1;
        }
    }
    out
}

/// Tokeniza un texto de consulta crudo a su secuencia de términos normalizados
/// (igual que se indexaron).
fn tokenize_all(tk: &dyn crate::fts::Tokenizer, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    tk.tokenize(text, &mut out);
    out.into_iter().map(|t| t.text).collect()
}

/// rowids de una secuencia de tokens **adyacentes y en orden** en el mismo field
/// (frase). Un solo token degenera en término exacto.
fn phrase_rowids<S: NodeSource>(
    src: &S,
    root: PageId,
    fts: &FtsIndexDef,
    tokens: &[String],
    field: Option<u8>,
) -> Result<Vec<i64>> {
    if tokens.is_empty() {
        return Ok(Vec::new());
    }
    if tokens.len() == 1 {
        let p = fts_term_postings(src, root, fts.fts_id, &tokens[0])?;
        return Ok(distinct_rowids(&p, field));
    }
    let first = fts_term_postings(src, root, fts.fts_id, &tokens[0])?;
    let rest: Vec<std::collections::HashSet<(i64, u8, u32)>> = tokens[1..]
        .iter()
        .map(|t| fts_term_postings(src, root, fts.fts_id, t).map(|v| v.into_iter().collect()))
        .collect::<Result<_>>()?;
    let mut rowids = Vec::new();
    for &(rid, fld, pos) in &first {
        if field.is_some_and(|f| f != fld) {
            continue;
        }
        let ok = rest.iter().enumerate().all(|(k, set)| {
            pos.checked_add(k as u32 + 1)
                .is_some_and(|want| set.contains(&(rid, fld, want)))
        });
        if ok {
            rowids.push(rid);
        }
    }
    rowids.sort_unstable();
    rowids.dedup();
    Ok(rowids)
}

/// Span mínimo (max-min de una posición por grupo) de una ventana que cubre todos
/// los grupos; `None` si algún grupo está vacío. Ventana deslizante sobre las
/// posiciones etiquetadas por grupo.
fn min_window_span(groups: &[Vec<u32>]) -> Option<u32> {
    if groups.iter().any(|g| g.is_empty()) {
        return None;
    }
    let mut items: Vec<(u32, usize)> = Vec::new();
    for (g, ps) in groups.iter().enumerate() {
        for &p in ps {
            items.push((p, g));
        }
    }
    items.sort_unstable();
    let n = groups.len();
    let mut have = vec![0usize; n];
    let mut covered = 0usize;
    let mut best: Option<u32> = None;
    let mut l = 0usize;
    let mut r = 0usize;
    while r < items.len() {
        if have[items[r].1] == 0 {
            covered += 1;
        }
        have[items[r].1] += 1;
        while covered == n {
            best = Some(best.map_or(items[r].0 - items[l].0, |b| b.min(items[r].0 - items[l].0)));
            have[items[l].1] -= 1;
            if have[items[l].1] == 0 {
                covered -= 1;
            }
            l += 1;
        }
        r += 1;
    }
    best
}

/// rowids donde todos los `tokens` aparecen en el mismo field dentro de una
/// ventana de `distance` posiciones (NEAR; distancia = diferencia máxima de
/// posición entre los términos de la ventana).
fn near_rowids<S: NodeSource>(
    src: &S,
    root: PageId,
    fts: &FtsIndexDef,
    tokens: &[String],
    distance: u32,
    field: Option<u8>,
) -> Result<Vec<i64>> {
    if tokens.len() < 2 {
        return Ok(Vec::new());
    }
    let mut per_token: Vec<std::collections::HashMap<(i64, u8), Vec<u32>>> = Vec::new();
    for t in tokens {
        let mut map: std::collections::HashMap<(i64, u8), Vec<u32>> =
            std::collections::HashMap::new();
        for (rid, fld, pos) in fts_term_postings(src, root, fts.fts_id, t)? {
            if field.is_some_and(|f| f != fld) {
                continue;
            }
            map.entry((rid, fld)).or_default().push(pos);
        }
        per_token.push(map);
    }
    let mut rowids = Vec::new();
    for key in per_token[0].keys() {
        if !per_token[1..].iter().all(|m| m.contains_key(key)) {
            continue;
        }
        let groups: Vec<Vec<u32>> = per_token.iter().map(|m| m[key].clone()).collect();
        if min_window_span(&groups).is_some_and(|s| s <= distance) {
            rowids.push(key.0);
        }
    }
    rowids.sort_unstable();
    rowids.dedup();
    Ok(rowids)
}

/// Evalúa un `Query` AST contra el índice y devuelve los rowids (ordenados,
/// distintos). `field` restringe a una columna (filtro `col:`).
fn eval_query<S: NodeSource>(
    src: &S,
    root: PageId,
    table: &TableDef,
    fts: &FtsIndexDef,
    tk: &dyn crate::fts::Tokenizer,
    q: &crate::fts::Query,
    field: Option<u8>,
) -> Result<Vec<i64>> {
    use crate::fts::Query;
    match q {
        Query::Term { text, prefix } => {
            let tokens = tokenize_all(tk, text);
            if tokens.is_empty() {
                Ok(Vec::new())
            } else if tokens.len() == 1 {
                let postings = if *prefix {
                    fts_prefix_postings(src, root, fts.fts_id, &tokens[0])?
                } else {
                    fts_term_postings(src, root, fts.fts_id, &tokens[0])?
                };
                Ok(distinct_rowids(&postings, field))
            } else {
                // Un bareword que tokeniza en varios ⇒ frase de esos tokens.
                phrase_rowids(src, root, fts, &tokens, field)
            }
        }
        Query::Phrase(words) => {
            let mut tokens = Vec::new();
            for w in words {
                tokens.extend(tokenize_all(tk, w));
            }
            phrase_rowids(src, root, fts, &tokens, field)
        }
        Query::Near { terms, distance } => {
            let mut tokens = Vec::new();
            for t in terms {
                tokens.extend(tokenize_all(tk, t));
            }
            near_rowids(src, root, fts, &tokens, *distance, field)
        }
        Query::Column { column, query } => {
            let pos = table
                .columns
                .iter()
                .position(|c| c.name == *column)
                .ok_or_else(|| Error::Sql {
                    msg: format!("columna «{column}» desconocida en filtro MATCH"),
                    pos: None,
                })?;
            if !fts.columns.contains(&pos) {
                return Err(Error::Sql {
                    msg: format!("la columna «{column}» no está en el índice FTS"),
                    pos: None,
                });
            }
            eval_query(src, root, table, fts, tk, query, Some(pos as u8))
        }
        Query::And(a, b) => {
            let ra = eval_query(src, root, table, fts, tk, a, field)?;
            let rb = eval_query(src, root, table, fts, tk, b, field)?;
            Ok(intersect_sorted(&ra, &rb))
        }
        Query::Or(a, b) => {
            let ra = eval_query(src, root, table, fts, tk, a, field)?;
            let rb = eval_query(src, root, table, fts, tk, b, field)?;
            Ok(union_sorted(&ra, &rb))
        }
        Query::Not(a, b) => {
            let ra = eval_query(src, root, table, fts, tk, a, field)?;
            let rb = eval_query(src, root, table, fts, tk, b, field)?;
            Ok(difference_sorted(&ra, &rb))
        }
    }
}

/// Busca en un índice FTS con un `Query` ya parseado y devuelve los rowids que
/// casan (ordenados, distintos). Entrada que usará el planner para
/// `col MATCH 'consulta'`.
pub fn fts_search<S: NodeSource>(
    src: &S,
    root: PageId,
    table: &TableDef,
    fts: &FtsIndexDef,
    query: &crate::fts::Query,
) -> Result<Vec<i64>> {
    let tk = crate::fts::tokenizer_for(&fts.tokenizer)?;
    eval_query(src, root, table, fts, tk.as_ref(), query, None)
}

// --- evaluación de MATCH contra una sola fila (sin índice): el planner re-aplica
// el WHERE por fila, y así `MATCH` funciona en cualquier posición (OR/NOT). El
// índice queda como optimización de narrowing aparte. ---

/// `true` si algún token del documento `(token, field, pos)` casa el término
/// (exacto o por prefijo), respetando el filtro de columna.
fn doc_has_term(doc: &[(String, u8, u32)], term: &str, prefix: bool, field: Option<u8>) -> bool {
    doc.iter().any(|(t, f, _)| {
        field.is_none_or(|ff| ff == *f)
            && if prefix {
                t.starts_with(term)
            } else {
                t == term
            }
    })
}

/// `true` si los tokens aparecen adyacentes y en orden en el mismo field (frase).
fn doc_has_phrase(doc: &[(String, u8, u32)], tokens: &[String], field: Option<u8>) -> bool {
    if tokens.is_empty() {
        return false;
    }
    if tokens.len() == 1 {
        return doc_has_term(doc, &tokens[0], false, field);
    }
    doc.iter().any(|(t, f, p)| {
        if t != &tokens[0] || field.is_some_and(|ff| ff != *f) {
            return false;
        }
        tokens[1..].iter().enumerate().all(|(k, tok)| {
            p.checked_add(k as u32 + 1).is_some_and(|want| {
                doc.iter()
                    .any(|(t2, f2, p2)| t2 == tok && f2 == f && *p2 == want)
            })
        })
    })
}

/// `true` si todos los tokens aparecen en un mismo field dentro de una ventana de
/// `distance` posiciones (NEAR).
fn doc_has_near(
    doc: &[(String, u8, u32)],
    tokens: &[String],
    distance: u32,
    field: Option<u8>,
) -> bool {
    if tokens.len() < 2 {
        return false;
    }
    let mut fields: Vec<u8> = doc
        .iter()
        .map(|(_, f, _)| *f)
        .filter(|f| field.is_none_or(|ff| ff == *f))
        .collect();
    fields.sort_unstable();
    fields.dedup();
    fields.into_iter().any(|f| {
        let groups: Vec<Vec<u32>> = tokens
            .iter()
            .map(|tok| {
                doc.iter()
                    .filter(|(t, ff, _)| t == tok && *ff == f)
                    .map(|(_, _, p)| *p)
                    .collect()
            })
            .collect();
        min_window_span(&groups).is_some_and(|s| s <= distance)
    })
}

/// Evalúa un `Query` AST contra un documento en memoria (los tokens de una fila),
/// devolviendo si casa. `field` restringe a una columna; `columns` mapea nombre →
/// field para los filtros `col:`.
fn eval_doc(
    q: &crate::fts::Query,
    doc: &[(String, u8, u32)],
    columns: &[(String, u8)],
    tk: &dyn crate::fts::Tokenizer,
    field: Option<u8>,
) -> Result<bool> {
    use crate::fts::Query;
    match q {
        Query::Term { text, prefix } => {
            let toks = tokenize_all(tk, text);
            Ok(match toks.len() {
                0 => false,
                1 => doc_has_term(doc, &toks[0], *prefix, field),
                _ => doc_has_phrase(doc, &toks, field),
            })
        }
        Query::Phrase(words) => {
            let mut toks = Vec::new();
            for w in words {
                toks.extend(tokenize_all(tk, w));
            }
            Ok(doc_has_phrase(doc, &toks, field))
        }
        Query::Near { terms, distance } => {
            let mut toks = Vec::new();
            for t in terms {
                toks.extend(tokenize_all(tk, t));
            }
            Ok(doc_has_near(doc, &toks, *distance, field))
        }
        Query::Column { column, query } => {
            let f = columns
                .iter()
                .find(|(n, _)| n == column)
                .map(|(_, f)| *f)
                .ok_or_else(|| Error::Sql {
                    msg: format!("la columna «{column}» no está en el índice FTS"),
                    pos: None,
                })?;
            eval_doc(query, doc, columns, tk, Some(f))
        }
        Query::And(a, b) => {
            Ok(eval_doc(a, doc, columns, tk, field)? && eval_doc(b, doc, columns, tk, field)?)
        }
        Query::Or(a, b) => {
            Ok(eval_doc(a, doc, columns, tk, field)? || eval_doc(b, doc, columns, tk, field)?)
        }
        Query::Not(a, b) => {
            Ok(eval_doc(a, doc, columns, tk, field)? && !eval_doc(b, doc, columns, tk, field)?)
        }
    }
}

/// `true` si la fila casa la consulta `MATCH` para el índice `fts`. Tokeniza las
/// columnas indexadas de `row` (valores locales de la tabla) y evalúa el `Query`
/// contra ese documento, con el **mismo** tokenizer con que se indexó.
pub fn fts_row_matches(
    def: &TableDef,
    fts: &FtsIndexDef,
    row: &[Value],
    query: &crate::fts::Query,
) -> Result<bool> {
    let tk = crate::fts::tokenizer_for(&fts.tokenizer)?;
    let mut doc: Vec<(String, u8, u32)> = Vec::new();
    let mut buf = Vec::new();
    for &col in &fts.columns {
        buf.clear();
        if let Some(Value::Text(text)) = row.get(col) {
            tk.tokenize(text, &mut buf);
        }
        for tok in &buf {
            doc.push((tok.text.clone(), col as u8, tok.position));
        }
    }
    let columns: Vec<(String, u8)> = fts
        .columns
        .iter()
        .map(|&c| (def.columns[c].name.clone(), c as u8))
        .collect();
    eval_doc(query, &doc, &columns, tk.as_ref(), None)
}

// --- índices vectoriales IVF: centroides + postings por cluster (docs/13) ---

fn vector_counter_key() -> [u8; 2] {
    [KS_CATALOG, CAT_VECTOR_COUNTER]
}

fn vector_ref_key(name: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(2 + name.len());
    k.extend_from_slice(&[KS_CATALOG, CAT_VECTOR_REF]);
    k.extend_from_slice(name.as_bytes());
    k
}

/// `[0x04, vidx_id BE]` — prefijo de TODO el índice vectorial.
fn vidx_prefix(vidx_id: u32) -> [u8; 5] {
    let mut p = [0u8; 5];
    p[0] = KS_VECTOR;
    p[1..5].copy_from_slice(&vidx_id.to_be_bytes());
    p
}

fn vec_centroid_key(vidx_id: u32, cid: u16) -> Vec<u8> {
    let mut k = vidx_prefix(vidx_id).to_vec();
    k.push(VEC_CENTROID);
    k.extend_from_slice(&cid.to_be_bytes());
    k
}

/// Prefijo `[0x04, vidx_id, 0x01, cid]` de los postings de un cluster.
fn vec_cluster_prefix(vidx_id: u32, cid: u16) -> Vec<u8> {
    let mut k = vidx_prefix(vidx_id).to_vec();
    k.push(VEC_POSTING);
    k.extend_from_slice(&cid.to_be_bytes());
    k
}

fn vec_posting_key(vidx_id: u32, cid: u16, rowid: i64) -> Vec<u8> {
    let mut k = vec_cluster_prefix(vidx_id, cid);
    k.extend_from_slice(&record::rowid_be(rowid));
    k
}

fn alloc_vidx_id<S: NodeStore>(s: &mut S, root: PageId) -> Result<(u32, PageId)> {
    let next = match btree::get(s, root, &vector_counter_key())? {
        Some(v) => u32::from_le_bytes(
            v.as_slice()
                .try_into()
                .map_err(|_| Error::CorruptRecord("contador vectorial"))?,
        ),
        None => 1,
    };
    let root = btree::insert(s, root, &vector_counter_key(), &(next + 1).to_le_bytes())?;
    Ok((next, root))
}

fn vec_l2_sq(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(&x, &y)| (x - y) * (x - y)).sum()
}

/// Vector f32 de la columna indexada de una fila; `None` si NULL/no-blob.
/// Normalizado si la métrica es coseno (entonces L2 ordena como coseno).
fn row_vector(record: &[Value], column: usize, metric: VectorMetric) -> Result<Option<Vec<f32>>> {
    match record.get(column) {
        Some(Value::Blob(b)) => {
            let v = crate::vector::to_f32(b)?;
            Ok(Some(if metric == VectorMetric::Cosine {
                crate::vector::normalize(&v)
            } else {
                v
            }))
        }
        _ => Ok(None),
    }
}

/// Construye un índice vectorial IVF: entrena k-means sobre los vectores
/// existentes (un evento discreto, determinista), asigna cada fila a su cluster y
/// persiste centroides + postings.
pub fn create_vector_index<S: NodeStore>(
    s: &mut S,
    root: PageId,
    table_name: &str,
    index_name: &str,
    column: usize,
    lists: u16,
    metric: VectorMetric,
) -> Result<PageId> {
    validate_name(
        index_name,
        "nombre de índice vectorial vacío o de más de 128 bytes",
    )?;
    let mut def = get_table(s, root, table_name)?.ok_or(Error::Constraint("tabla desconocida"))?;
    if column >= def.columns.len() {
        return Err(Error::InvalidInput(
            "columna de índice vectorial inexistente",
        ));
    }
    if !matches!(def.columns[column].col_type, ColType::Blob) {
        return Err(Error::InvalidInput(
            "un índice vectorial requiere una columna BLOB",
        ));
    }
    if lists == 0 {
        return Err(Error::InvalidInput("lists debe ser mayor que 0"));
    }
    if btree::get(s, root, &vector_ref_key(index_name))?.is_some() {
        return Err(Error::Constraint(
            "ya existe un índice vectorial con ese nombre",
        ));
    }
    // Vectores existentes (filas con vector no-NULL de la misma dimensión).
    let mut vectors: Vec<Vec<f32>> = Vec::new();
    let mut rowids: Vec<i64> = Vec::new();
    let mut dim = 0usize;
    for item in scan_table(s, root, &def)? {
        let (rowid, values) = item?;
        if let Some(v) = row_vector(&values, column, metric)? {
            if vectors.is_empty() {
                dim = v.len();
            } else if v.len() != dim {
                return Err(Error::InvalidInput(
                    "vectores de distinta dimensión en la columna",
                ));
            }
            vectors.push(v);
            rowids.push(rowid);
        }
    }
    let (vidx_id, mut root) = alloc_vidx_id(s, root)?;
    // Núcleo IVF: entrena los centroides y asigna cada vector a su cluster.
    let centroids = crate::ivf::train(&vectors, lists as usize, 25);
    let assignments = crate::ivf::assign(&vectors, &centroids);
    for (cid, c) in centroids.iter().enumerate() {
        root = btree::insert(
            s,
            root,
            &vec_centroid_key(vidx_id, cid as u16),
            &crate::vector::pack_f32(c),
        )?;
    }
    for (cid, members) in assignments.iter().enumerate() {
        for &i in members {
            root = btree::insert(
                s,
                root,
                &vec_posting_key(vidx_id, cid as u16, rowids[i]),
                &[],
            )?;
        }
    }
    let idx = VectorIndexDef {
        name: index_name.to_owned(),
        vidx_id,
        column,
        lists,
        metric,
        dim: dim as u32,
    };
    root = btree::insert(s, root, &vector_ref_key(index_name), table_name.as_bytes())?;
    def.vector_indexes.push(idx);
    root = btree::insert(s, root, &table_key(table_name), &encode_def(&def))?;
    Ok(root)
}

/// Borra un índice vectorial por su nombre global: todas sus claves
/// (`[0x04, vidx_id]`), su ref y su entrada en el esquema. `false` si no existía.
pub fn drop_vector_index<S: NodeStore>(
    s: &mut S,
    root: PageId,
    index_name: &str,
) -> Result<(PageId, bool)> {
    let Some(table_bytes) = btree::get(s, root, &vector_ref_key(index_name))? else {
        return Ok((root, false));
    };
    let table_name = String::from_utf8(table_bytes)
        .map_err(|_| Error::CorruptRecord("ref vectorial no UTF-8"))?;
    let mut def = get_table(s, root, &table_name)?
        .ok_or(Error::CorruptRecord("índice vectorial sin su tabla"))?;
    let Some(pos) = def.vector_indexes.iter().position(|i| i.name == index_name) else {
        return Err(Error::CorruptRecord(
            "ref vectorial sin entrada en el esquema",
        ));
    };
    let idx = def.vector_indexes.remove(pos);
    let prefix = vidx_prefix(idx.vidx_id);
    let mut keys = Vec::new();
    for item in btree::scan_from(s, root, &prefix)? {
        let (k, _) = item?;
        if !k.starts_with(&prefix) {
            break;
        }
        keys.push(k);
    }
    let mut root = root;
    for k in keys {
        (root, _) = btree::delete(s, root, &k)?;
    }
    (root, _) = btree::delete(s, root, &vector_ref_key(index_name))?;
    root = btree::insert(s, root, &table_key(&table_name), &encode_def(&def))?;
    Ok((root, true))
}

/// `true` si existe un índice vectorial con ese nombre global.
pub fn vector_index_exists<S: NodeSource>(src: &S, root: PageId, name: &str) -> Result<bool> {
    Ok(btree::get(src, root, &vector_ref_key(name))?.is_some())
}

/// Centroides del índice, ordenados por `cid`.
fn read_centroids<S: NodeSource>(src: &S, root: PageId, vidx_id: u32) -> Result<Vec<Vec<f32>>> {
    let mut prefix = vidx_prefix(vidx_id).to_vec();
    prefix.push(VEC_CENTROID);
    let mut cents = Vec::new();
    for item in btree::scan_from(src, root, &prefix)? {
        let (k, v) = item?;
        if !k.starts_with(&prefix) {
            break;
        }
        cents.push(crate::vector::to_f32(&v)?);
    }
    Ok(cents)
}

/// rowids candidatos de los `nprobe` clusters más cercanos a `query_raw` (ANN). El
/// planner los trae y rankea exacto. `query_raw` es el vector crudo de la query
/// (se normaliza aquí si la métrica es coseno, igual que al construir).
pub fn vector_search<S: NodeSource>(
    src: &S,
    root: PageId,
    vidx: &VectorIndexDef,
    query_raw: &[f32],
    nprobe: usize,
) -> Result<Vec<i64>> {
    let centroids = read_centroids(src, root, vidx.vidx_id)?;
    if centroids.is_empty() {
        return Ok(Vec::new());
    }
    let query = if vidx.metric == VectorMetric::Cosine {
        crate::vector::normalize(query_raw)
    } else {
        query_raw.to_vec()
    };
    let mut cents: Vec<(f32, usize)> = centroids
        .iter()
        .enumerate()
        .map(|(i, c)| (vec_l2_sq(c, &query), i))
        .collect();
    cents.sort_by(|a, b| a.0.total_cmp(&b.0));
    let probe = nprobe.clamp(1, cents.len());

    let mut rowids = Vec::new();
    for &(_, cid) in cents.iter().take(probe) {
        let pp = vec_cluster_prefix(vidx.vidx_id, cid as u16);
        btree::for_each_prefix(src, root, &pp, |key| {
            let rid = key
                .get(key.len() - 8..)
                .and_then(record::rowid_from_be)
                .ok_or(Error::CorruptRecord("rowid en posting vectorial"))?;
            rowids.push(rid);
            Ok(())
        })?;
    }
    Ok(rowids)
}

/// rowids cuyas columnas indexadas valen exactamente `values` (igualdad). El
/// planificador lo usa para `WHERE col = const`; luego `get_row` por cada rowid.
pub fn index_scan_eq<S: NodeSource>(
    src: &S,
    root: PageId,
    idx: &IndexDef,
    values: &[Value],
) -> Result<Vec<i64>> {
    let prefix = index_value_prefix(idx, values);
    let mut rowids = Vec::new();
    // `for_each_prefix` desciende in-page y recorre las entradas que casan el
    // prefijo sin materializar celdas ni copiar payloads (camino caliente).
    btree::for_each_prefix(src, root, &prefix, |key| {
        rowids.push(rowid_from_index_key(key)?);
        Ok(())
    })?;
    Ok(rowids)
}

/// rowid (últimos 8 bytes) de una clave de entrada de índice.
fn rowid_from_index_key(key: &[u8]) -> Result<i64> {
    key.len()
        .checked_sub(8)
        .and_then(|p| record::rowid_from_be(&key[p..]))
        .ok_or(Error::CorruptRecord("rowid en entrada de índice"))
}

/// rowids de un **rango** sobre un índice de una sola columna: `lo`/`hi` son
/// `(valor, inclusive)` opcionales (`col > V` ⇒ `lo=(V,false), hi=None`; etc.).
/// Recorre el índice ordenado desde la cota inferior y para al pasar la superior.
/// Excluye `NULL` (no satisface ningún rango en SQL). El planificador lo usa para
/// `WHERE col <op> const`; luego `get_row` por cada rowid.
pub fn index_scan_range<S: NodeSource>(
    src: &S,
    root: PageId,
    idx: &IndexDef,
    lo: Option<(&Value, bool)>,
    hi: Option<(&Value, bool)>,
) -> Result<Vec<i64>> {
    let enc = |v: &Value| {
        let mut e = Vec::new();
        keyenc::encode_index_value(v, &mut e);
        e
    };
    let lo_enc = lo.map(|(v, inc)| (enc(v), inc));
    let hi_enc = hi.map(|(v, inc)| (enc(v), inc));
    let id_prefix = index_id_prefix(idx.index_id);
    let mut start = id_prefix.to_vec();
    if let Some((e, _)) = &lo_enc {
        start.extend_from_slice(e);
    }
    let mut rowids = Vec::new();
    for item in btree::scan_from(src, root, &start)? {
        let (key, _) = item?;
        if !key.starts_with(&id_prefix) {
            break; // fuera del índice
        }
        let value_enc = &key[id_prefix.len()..key.len() - 8];
        if value_enc.first() == Some(&0x00) {
            continue; // NULL: no satisface < / > / etc.
        }
        if let Some((e, inc)) = &lo_enc {
            match value_enc.cmp(e.as_slice()) {
                std::cmp::Ordering::Less => continue,
                std::cmp::Ordering::Equal if !inc => continue,
                _ => {}
            }
        }
        if let Some((e, inc)) = &hi_enc {
            match value_enc.cmp(e.as_slice()) {
                std::cmp::Ordering::Greater => break, // ordenado: ya pasamos
                std::cmp::Ordering::Equal if !*inc => break,
                _ => {}
            }
        }
        let rowid = record::rowid_from_be(&key[key.len() - 8..])
            .ok_or(Error::CorruptRecord("rowid en entrada de índice"))?;
        rowids.push(rowid);
    }
    Ok(rowids)
}

// --- filas ---

/// Resuelve la columna `i` de una fila entrante: alias del rowid → NULL (se
/// reconstruye al leer), valor ausente → DEFAULT (o NULL), promoción
/// INTEGER → REAL y validación de tipo/NOT NULL. **Única fuente** de las
/// reglas de validación: la usan el camino caliente sin clones de
/// [`put_row_buffered`] y [`validate_record_into`]. Solo aplica a valores de
/// un INSERT/UPDATE — en un registro **almacenado** las columnas ausentes
/// significan NULL/DEFAULT según `finish_row`, no según estas reglas.
fn resolve_col<'a>(table: &'a TableDef, values: &'a [Value], i: usize) -> Result<ValueRef<'a>> {
    if table.rowid_alias == Some(i) {
        return Ok(ValueRef::Null); // reconstruido del rowid al leer
    }
    let col = &table.columns[i];
    let v = match values.get(i) {
        Some(v) => v,
        None => col.default.as_ref().unwrap_or(&Value::Null),
    };
    match (col.col_type, v) {
        (_, Value::Null) => {
            if col.not_null {
                return Err(Error::Constraint("NULL en columna NOT NULL"));
            }
            Ok(ValueRef::Null)
        }
        (ColType::Integer, Value::Integer(n)) => Ok(ValueRef::Integer(*n)),
        // Promoción sin pérdida: INTEGER → REAL (docs/04).
        (ColType::Real, Value::Integer(n)) => Ok(ValueRef::Real(*n as f64)),
        (ColType::Real, Value::Real(f)) => Ok(ValueRef::Real(*f)),
        (ColType::Text, Value::Text(s)) => Ok(ValueRef::Text(s.as_str())),
        (ColType::Blob, Value::Blob(b)) => Ok(ValueRef::Blob(b.as_slice())),
        (ColType::Boolean, Value::Bool(b)) => Ok(ValueRef::Bool(*b)),
        _ => Err(Error::Constraint(
            "tipo de valor incompatible con la columna",
        )),
    }
}

/// Valida tipos y restricciones, aplica defaults y devuelve el registro listo
/// para almacenar (el alias del rowid se guarda como NULL: se reconstruye).
fn validated_record(table: &TableDef, values: &[Value]) -> Result<Vec<Value>> {
    let mut record = Vec::with_capacity(table.columns.len());
    validate_record_into(table, values, &mut record)?;
    Ok(record)
}

/// Como [`validated_record`] pero en un `Vec` **reutilizado** (lo limpia antes):
/// materializa vía [`resolve_col`] para los caminos que necesitan el registro
/// entero en memoria (UPDATE y tablas con índices secundarios).
fn validate_record_into(table: &TableDef, values: &[Value], record: &mut Vec<Value>) -> Result<()> {
    record.clear();
    if values.len() > table.columns.len() {
        return Err(Error::InvalidInput("más valores que columnas"));
    }
    for i in 0..table.columns.len() {
        record.push(resolve_col(table, values, i)?.to_value());
    }
    Ok(())
}

/// Rowid explícito de `values` (si la columna alias trae un entero), validando
/// el tipo. `None` ⇒ rowid automático.
pub(crate) fn explicit_rowid(table: &TableDef, values: &[Value]) -> Result<Option<i64>> {
    match table.rowid_alias {
        Some(i) => match values.get(i) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Integer(n)) => Ok(Some(*n)),
            Some(_) => Err(Error::Constraint("la PRIMARY KEY debe ser un entero")),
        },
        None => Ok(None),
    }
}

/// Próximo rowid del contador de la tabla (1 si aún no existe).
pub(crate) fn read_counter<S: NodeSource>(src: &S, root: PageId, table_id: u32) -> Result<i64> {
    match btree::get(src, root, &counter_key(table_id))? {
        Some(v) => Ok(i64::from_le_bytes(
            v.as_slice()
                .try_into()
                .map_err(|_| Error::CorruptRecord("contador de rowid"))?,
        )),
        None => Ok(1),
    }
}

/// Resuelve `(rowid, próximo_contador)` a partir del explícito y el contador
/// actual. Dup-check del rowid explícito: `Constraint` si la fila ya existe.
pub(crate) fn resolve_rowid<S: NodeSource>(
    src: &S,
    root: PageId,
    table_id: u32,
    explicit: Option<i64>,
    next: i64,
) -> Result<(i64, i64)> {
    match explicit {
        None => Ok((
            next,
            next.checked_add(1)
                .ok_or(Error::Constraint("rowids agotados"))?,
        )),
        Some(n) => {
            // Invariante del contador: todo rowid existente es < `next` (cada
            // insert lo deja por encima y el merge lo reconcilia por máximo;
            // el camino automático ya confía en él: asigna `next` sin
            // dup-check). n >= next ⇒ la fila no puede existir, así que el
            // descenso de dup-check sobra — un INSERT con PK explícita
            // creciente queda O(1) como el automático. Salvedad: si `next`
            // saturó en i64::MAX, ese rowid sí puede existir (saturating_add
            // de abajo) y hay que mirar.
            if (n < next || next == i64::MAX) && btree::contains(src, root, &row_key(table_id, n))?
            {
                return Err(Error::Constraint("rowid duplicado"));
            }
            Ok((n, if n >= next { n.saturating_add(1) } else { next }))
        }
    }
}

/// Escribe el contador de rowid de una tabla.
pub(crate) fn write_counter<S: NodeStore>(
    s: &mut S,
    root: PageId,
    table_id: u32,
    next: i64,
) -> Result<PageId> {
    btree::insert(s, root, &counter_key(table_id), &next.to_le_bytes())
}

/// Inserta una fila con su rowid **ya resuelto**, sin tocar el contador: valida
/// el registro y escribe su clave. La capa de transacción usa esto + el contador
/// cacheado para no reescribir la hoja del contador en cada fila.
pub(crate) fn put_row<S: NodeStore>(
    s: &mut S,
    root: PageId,
    table: &TableDef,
    rowid: i64,
    values: &[Value],
) -> Result<PageId> {
    let record = validated_record(table, values)?;
    let mut root = btree::insert(
        s,
        root,
        &row_key(table.table_id, rowid),
        &record::encode_values(&record),
    )?;
    if !table.indexes.is_empty() {
        root = insert_index_entries(s, root, table, rowid, &record)?;
    }
    if !table.fts_indexes.is_empty() {
        root = insert_fts_entries(s, root, table, rowid, &record)?;
    }
    Ok(root)
}

/// Como [`put_row`] pero con buffers **reutilizados** para la fila validada y su
/// codificación: el camino caliente de `insert_row` no asigna un `Vec` por fila
/// (M10-perf). El cursor de append del b-tree hace el resto (insert O(1)).
pub(crate) fn put_row_buffered<S: NodeStore>(
    s: &mut S,
    root: PageId,
    table: &TableDef,
    rowid: i64,
    values: &[Value],
    rec_buf: &mut Vec<Value>,
    enc_buf: &mut Vec<u8>,
) -> Result<PageId> {
    if table.indexes.is_empty() && table.fts_indexes.is_empty() {
        // Sin índices (el caso del bulk-load típico): valida y codifica en una
        // pasada sobre los valores prestados, sin clonar texto/blob por fila
        // ni materializar el registro (M10-perf, fase 2).
        return put_row_data(s, root, table, rowid, values, enc_buf);
    }
    // Con índices (normales o FTS): las entradas necesitan el registro
    // materializado (claves de índice, dup-check UNIQUE, tokens FTS); se
    // reutilizan los buffers.
    validate_record_into(table, values, rec_buf)?;
    record::encode_values_into(rec_buf, enc_buf);
    let mut root = btree::insert(s, root, &row_key(table.table_id, rowid), enc_buf)?;
    if !table.indexes.is_empty() {
        root = insert_index_entries(s, root, table, rowid, rec_buf.as_slice())?;
    }
    if !table.fts_indexes.is_empty() {
        root = insert_fts_entries(s, root, table, rowid, rec_buf.as_slice())?;
    }
    Ok(root)
}

/// Valida y escribe **solo la fila** (sin entradas de índice), codificando en
/// una pasada sobre los valores prestados — el camino sin clones. La usa
/// [`put_row_buffered`] para tablas sin índices y el bulk-load, que difiere
/// las entradas y las inserta en bloque con [`flush_index_entries`].
pub(crate) fn put_row_data<S: NodeStore>(
    s: &mut S,
    root: PageId,
    table: &TableDef,
    rowid: i64,
    values: &[Value],
    enc_buf: &mut Vec<u8>,
) -> Result<PageId> {
    if values.len() > table.columns.len() {
        return Err(Error::InvalidInput("más valores que columnas"));
    }
    record::encode_resolved_into(
        table.columns.len(),
        |i| resolve_col(table, values, i),
        enc_buf,
    )?;
    btree::insert(s, root, &row_key(table.table_id, rowid), enc_buf)
}

/// Clave de entrada de índice resuelta desde los valores **crudos** de un
/// INSERT (misma resolución que el camino caliente: defaults, promoción,
/// alias) y si alguna columna indexada quedó NULL — los NULL no participan en
/// el dup-check UNIQUE (SQL permite varios). Mismos bytes que
/// `index_entry_key` sobre el registro materializado.
pub(crate) fn resolved_index_entry(
    table: &TableDef,
    values: &[Value],
    idx: &IndexDef,
    rowid: i64,
) -> Result<(Vec<u8>, bool)> {
    let mut key = index_id_prefix(idx.index_id).to_vec();
    let mut has_null = false;
    for &c in &idx.columns {
        let v = resolve_col(table, values, c)?;
        if matches!(v, ValueRef::Null) {
            has_null = true;
        }
        keyenc::encode_index_value_ref(v, &mut key);
    }
    key.extend_from_slice(&record::rowid_be(rowid));
    Ok((key, has_null))
}

/// Inserta en bloque las entradas **diferidas** de un índice (bulk-load).
/// Ordena primero: mejor localidad de descenso y, si el índice es la cola del
/// árbol (índice recién creado o el de mayor id), cada insert es un append.
/// El dup-check UNIQUE corre **antes de escribir nada**: intra-lote por
/// prefijos adyacentes tras ordenar, y contra lo existente con una sonda por
/// prefijo distinto (menos sondas que el camino fila a fila).
pub(crate) fn flush_index_entries<S: NodeStore>(
    s: &mut S,
    root: PageId,
    idx: &IndexDef,
    entries: &mut [(Vec<u8>, bool)],
) -> Result<PageId> {
    entries.sort_unstable();
    if idx.unique {
        for w in entries.windows(2) {
            if !w[0].1 && !w[1].1 && w[0].0[..w[0].0.len() - 8] == w[1].0[..w[1].0.len() - 8] {
                return Err(Error::Constraint("violación de restricción UNIQUE"));
            }
        }
        let mut prev: Option<&[u8]> = None;
        for (key, has_null) in entries.iter() {
            if *has_null {
                continue;
            }
            let prefix = &key[..key.len() - 8];
            if prev == Some(prefix) {
                continue; // ya sondado (los iguales son adyacentes tras ordenar)
            }
            prev = Some(prefix);
            let rowid = rowid_from_index_key(key)?;
            if value_held_by_other(s, root, prefix, rowid)? {
                return Err(Error::Constraint("violación de restricción UNIQUE"));
            }
        }
    }
    let mut root = root;
    for (key, _) in entries.iter() {
        root = btree::insert(s, root, key, &[])?;
    }
    Ok(root)
}

/// Inserta con rowid automático o explícito, contador incluido (camino directo
/// para llamadores sin caché de contador: tests y usos puntuales). Falla con
/// `Constraint` si el rowid explícito ya existe.
pub fn insert_row<S: NodeStore>(
    s: &mut S,
    root: PageId,
    table: &TableDef,
    values: &[Value],
) -> Result<(PageId, i64)> {
    let explicit = explicit_rowid(table, values)?;
    let next = read_counter(s, root, table.table_id)?;
    let (rowid, new_next) = resolve_rowid(s, root, table.table_id, explicit, next)?;
    let root = write_counter(s, root, table.table_id, new_next)?;
    let root = put_row(s, root, table, rowid, values)?;
    Ok((root, rowid))
}

/// Sobrescribe una fila existente. `false` si el rowid no existe.
pub fn update_row<S: NodeStore>(
    s: &mut S,
    root: PageId,
    table: &TableDef,
    rowid: i64,
    values: &[Value],
) -> Result<(PageId, bool)> {
    let key = row_key(table.table_id, rowid);
    let Some(old_bytes) = btree::get(s, root, &key)? else {
        return Ok((root, false));
    };
    let record = validated_record(table, values)?;
    let mut root = root;
    // Quita las entradas (índice y FTS) de la fila vieja antes de sobrescribir.
    if !table.indexes.is_empty() || !table.fts_indexes.is_empty() {
        let old = finish_row(table, rowid, record::decode_values(&old_bytes)?)?;
        if !table.indexes.is_empty() {
            root = delete_index_entries(s, root, table, rowid, &old)?;
        }
        if !table.fts_indexes.is_empty() {
            root = delete_fts_entries(s, root, table, rowid, &old)?;
        }
    }
    root = btree::insert(s, root, &key, &record::encode_values(&record))?;
    if !table.indexes.is_empty() {
        root = insert_index_entries(s, root, table, rowid, &record)?;
    }
    if !table.fts_indexes.is_empty() {
        root = insert_fts_entries(s, root, table, rowid, &record)?;
    }
    Ok((root, true))
}

pub fn get_row<S: NodeSource>(
    src: &S,
    root: PageId,
    table: &TableDef,
    rowid: i64,
) -> Result<Option<Vec<Value>>> {
    match btree::get(src, root, &row_key(table.table_id, rowid))? {
        Some(bytes) => Ok(Some(finish_row(
            table,
            rowid,
            record::decode_values(&bytes)?,
        )?)),
        None => Ok(None),
    }
}

pub fn delete_row<S: NodeStore>(
    s: &mut S,
    root: PageId,
    table: &TableDef,
    rowid: i64,
) -> Result<(PageId, bool)> {
    let key = row_key(table.table_id, rowid);
    if table.indexes.is_empty() && table.fts_indexes.is_empty() {
        return btree::delete(s, root, &key);
    }
    // Con índices (normales o FTS): hay que leer la fila para quitar sus entradas.
    let Some(bytes) = btree::get(s, root, &key)? else {
        return Ok((root, false));
    };
    let record = finish_row(table, rowid, record::decode_values(&bytes)?)?;
    let (mut root, _) = btree::delete(s, root, &key)?;
    if !table.indexes.is_empty() {
        root = delete_index_entries(s, root, table, rowid, &record)?;
    }
    if !table.fts_indexes.is_empty() {
        root = delete_fts_entries(s, root, table, rowid, &record)?;
    }
    Ok((root, true))
}

/// Rellena columnas ausentes con NULL (esquema más nuevo que la fila) y
/// reconstruye la columna alias desde el rowid.
fn finish_row(table: &TableDef, rowid: i64, mut values: Vec<Value>) -> Result<Vec<Value>> {
    if values.len() > table.columns.len() {
        return Err(Error::CorruptRecord(
            "la fila tiene más columnas que el esquema",
        ));
    }
    // Columnas que faltan = añadidas por `ALTER TABLE ADD COLUMN` tras escribir
    // la fila: se leen como su DEFAULT (o NULL), sin reescribir la fila.
    while values.len() < table.columns.len() {
        let col = &table.columns[values.len()];
        values.push(col.default.clone().unwrap_or(Value::Null));
    }
    if let Some(i) = table.rowid_alias {
        values[i] = Value::Integer(rowid);
    }
    Ok(values)
}

// --- scan de tabla ---

/// Proyección precompilada de un scan en streaming: qué columnas almacenadas
/// decodificar (ordenadas, únicas) y de dónde sale cada columna de salida.
/// Se construye una vez por consulta; `ScanState::next_into` la usa por fila.
pub struct ScanProjection {
    /// Índices de columna a decodificar del registro, crecientes y sin repetir.
    stored: Vec<usize>,
    /// Una entrada por columna de salida, en orden.
    slots: Vec<Slot>,
}

enum Slot {
    /// Alias del rowid: se reconstruye de la clave, no se decodifica.
    Rowid,
    /// Columna almacenada: posición en `stored` + índice de columna (para su
    /// DEFAULT si el registro es más corto) + si es el último slot que la usa
    /// (mueve el valor en vez de clonarlo).
    Stored {
        pos: usize,
        col: usize,
        last_use: bool,
    },
}

impl ScanProjection {
    /// `cols`: índice de columna de la tabla por cada columna de salida.
    pub fn new(table: &TableDef, cols: &[usize]) -> ScanProjection {
        let mut stored: Vec<usize> = cols
            .iter()
            .copied()
            .filter(|&c| table.rowid_alias != Some(c))
            .collect();
        stored.sort_unstable();
        stored.dedup();
        let slots = cols
            .iter()
            .enumerate()
            .map(|(slot_i, &c)| {
                if table.rowid_alias == Some(c) {
                    Slot::Rowid
                } else {
                    Slot::Stored {
                        pos: stored.binary_search(&c).expect("recién insertada"),
                        col: c,
                        last_use: !cols[slot_i + 1..].contains(&c),
                    }
                }
            })
            .collect();
        ScanProjection { stored, slots }
    }

    pub fn ncols(&self) -> usize {
        self.slots.len()
    }
}

/// Scan de tabla **sin préstamo de la fuente** (streaming hacia la API): el
/// `Rows` posee su `Snapshot` y este estado, y decodifica por fila SOLO las
/// columnas proyectadas, directo de la página sostenida (cero copias de
/// clave/valor). Semántica de fila idéntica a `finish_row`: alias del rowid
/// reconstruido y columnas ausentes con su DEFAULT (o NULL).
pub struct ScanState {
    cur: btree::CursorState,
    prefix: Vec<u8>,
    done: bool,
}

impl ScanState {
    pub fn start<S: NodeSource>(src: &S, root: PageId, table: &TableDef) -> Result<ScanState> {
        let prefix = row_prefix(table.table_id);
        Ok(ScanState {
            cur: btree::scan_state(src, root, Some(&prefix))?,
            prefix,
            done: false,
        })
    }

    /// Proyecta la próxima fila en `out` (limpiado). `Ok(false)` = fin del
    /// scan. `scratch` es un buffer reutilizado entre filas (del llamador).
    pub fn next_into<S: NodeSource>(
        &mut self,
        src: &S,
        table: &TableDef,
        proj: &ScanProjection,
        out: &mut Vec<Value>,
        scratch: &mut Vec<Option<Value>>,
    ) -> Result<bool> {
        if self.done {
            return Ok(false);
        }
        let prefix = &self.prefix;
        let row = self.cur.advance_view(src, |key, record| {
            if !key.starts_with(prefix) {
                return Ok(None); // primera clave fuera de la tabla: fin
            }
            let (_, rowid) =
                decode_row_key(key).ok_or(Error::CorruptRecord("clave de fila mal formada"))?;
            record::decode_cols_sorted(record, &proj.stored, scratch)?;
            Ok(Some(rowid))
        })?;
        let Some(Some(rowid)) = row else {
            self.done = true; // árbol agotado o fuera de prefijo
            return Ok(false);
        };
        out.clear();
        for slot in &proj.slots {
            out.push(match slot {
                Slot::Rowid => Value::Integer(rowid),
                Slot::Stored { pos, col, last_use } => {
                    let v = if *last_use {
                        scratch[*pos].take()
                    } else {
                        scratch[*pos].clone()
                    };
                    match v {
                        Some(v) => v,
                        None => table.columns[*col].default.clone().unwrap_or(Value::Null),
                    }
                }
            });
        }
        Ok(true)
    }
}

/// Iterador por rowid ascendente: el orden del scan ES el orden del rowid.
pub struct TableScan<'s, S: NodeSource> {
    cur: Cursor<'s, S>,
    prefix: Vec<u8>,
    /// Esquema (clonado) para reconstruir cada fila con `finish_row`: rellena
    /// las columnas añadidas por `ALTER TABLE` con su DEFAULT, igual que el
    /// camino de point lookup.
    def: TableDef,
    done: bool,
}

pub fn scan_table<'s, S: NodeSource>(
    src: &'s S,
    root: PageId,
    table: &TableDef,
) -> Result<TableScan<'s, S>> {
    let prefix = row_prefix(table.table_id);
    Ok(TableScan {
        cur: btree::scan_from(src, root, &prefix)?,
        prefix,
        def: table.clone(),
        done: false,
    })
}

impl<S: NodeSource> Iterator for TableScan<'_, S> {
    type Item = Result<(i64, Vec<Value>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let item = match self.cur.next()? {
            Ok(kv) => kv,
            Err(e) => return Some(Err(e)),
        };
        let (key, payload) = item;
        if !key.starts_with(&self.prefix) {
            self.done = true;
            return None;
        }
        let parse = || -> Result<(i64, Vec<Value>)> {
            let (_, rowid) =
                decode_row_key(&key).ok_or(Error::CorruptRecord("clave de fila mal formada"))?;
            let values = finish_row(&self.def, rowid, record::decode_values(&payload)?)?;
            Ok((rowid, values))
        };
        Some(parse())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::NO_ROOT;
    use crate::testutil::MemStore;

    fn col(name: &str, t: ColType) -> ColumnSpec {
        ColumnSpec {
            name: name.into(),
            col_type: t,
            not_null: false,
            primary_key: false,
            default: None,
            references: None,
            unique: false,
            check: None,
        }
    }

    fn facturas_spec() -> TableSpec {
        TableSpec {
            name: "facturas".into(),
            columns: vec![
                ColumnSpec {
                    primary_key: true,
                    ..col("id", ColType::Integer)
                },
                col("total", ColType::Real),
                ColumnSpec {
                    not_null: true,
                    default: Some(Value::Text("borrador".into())),
                    ..col("estado", ColType::Text)
                },
                col("pagada", ColType::Boolean),
                col("adjunto", ColType::Blob),
            ],
            foreign_keys: Vec::new(),
            uniques: Vec::new(),
            checks: Vec::new(),
        }
    }

    #[test]
    fn fts_index_schema_roundtrip() {
        let mut s = MemStore::new();
        let (_, def) = create_table(&mut s, NO_ROOT, &facturas_spec()).unwrap();

        // Sin índices FTS: el bloque v8 está presente pero vacío.
        assert!(def.fts_indexes.is_empty());
        assert_eq!(decode_def(&def.name, &encode_def(&def)).unwrap(), def);

        // Con dos índices FTS sobre columnas y tokenizers distintos.
        let mut d = def.clone();
        d.fts_indexes.push(FtsIndexDef {
            name: "fts_estado".into(),
            fts_id: 1,
            columns: vec![2],
            tokenizer: "unicode".into(),
        });
        d.fts_indexes.push(FtsIndexDef {
            name: "fts_multi".into(),
            fts_id: 2,
            columns: vec![2, 4],
            tokenizer: "ascii".into(),
        });
        let back = decode_def(&d.name, &encode_def(&d)).unwrap();
        assert_eq!(back, d);
        assert_eq!(back.fts_indexes[0].tokenizer, "unicode");
        assert_eq!(back.fts_indexes[1].columns, vec![2, 4]);
    }

    #[test]
    fn vector_index_schema_roundtrip() {
        let mut s = MemStore::new();
        let (_, def) = create_table(&mut s, NO_ROOT, &facturas_spec()).unwrap();
        // Sin índices vectoriales: el bloque v9 está presente pero vacío.
        assert!(def.vector_indexes.is_empty());
        assert_eq!(decode_def(&def.name, &encode_def(&def)).unwrap(), def);

        let mut d = def.clone();
        d.vector_indexes.push(VectorIndexDef {
            name: "v_cos".into(),
            vidx_id: 1,
            column: 4,
            lists: 16,
            metric: VectorMetric::Cosine,
            dim: 384,
        });
        d.vector_indexes.push(VectorIndexDef {
            name: "v_l2".into(),
            vidx_id: 2,
            column: 4,
            lists: 256,
            metric: VectorMetric::L2,
            dim: 768,
        });
        let back = decode_def(&d.name, &encode_def(&d)).unwrap();
        assert_eq!(back, d);
        assert_eq!(back.vector_indexes[0].metric, VectorMetric::Cosine);
        assert_eq!(back.vector_indexes[1].lists, 256);
        assert_eq!(back.vector_indexes[1].dim, 768);
    }

    #[test]
    fn vector_index_build_search_drop() {
        let mut s = MemStore::new();
        let spec = TableSpec {
            name: "docs".into(),
            columns: vec![
                ColumnSpec {
                    primary_key: true,
                    ..col("id", ColType::Integer)
                },
                col("emb", ColType::Blob),
            ],
            foreign_keys: Vec::new(),
            uniques: Vec::new(),
            checks: Vec::new(),
        };
        let (mut root, def) = create_table(&mut s, NO_ROOT, &spec).unwrap();
        // Dos clusters: A≈(0,0) (rowids 1-3), B≈(10,10) (rowids 4-6).
        let vecs = [
            vec![0.0, 0.0],
            vec![0.1, -0.1],
            vec![-0.1, 0.2],
            vec![10.0, 10.0],
            vec![9.9, 10.1],
            vec![10.2, 9.8],
        ];
        for v in &vecs {
            let blob = crate::vector::pack_f32(v);
            root = insert_row(&mut s, root, &def, &[Value::Null, Value::Blob(blob)])
                .unwrap()
                .0;
        }
        root = create_vector_index(&mut s, root, "docs", "v_idx", 1, 2, VectorMetric::L2).unwrap();
        let def = get_table(&s, root, "docs").unwrap().unwrap();
        let vidx = def.vector_indexes[0].clone();
        assert_eq!(vidx.dim, 2);

        // Query cerca de A: nprobe=1 ⇒ candidatos solo del cluster A.
        let mut near_a = vector_search(&s, root, &vidx, &[0.05, 0.05], 1).unwrap();
        near_a.sort_unstable();
        assert_eq!(near_a, vec![1, 2, 3]);
        // nprobe = nº de listas ⇒ candidatos de todos los clusters (los 6).
        assert_eq!(
            vector_search(&s, root, &vidx, &[0.05, 0.05], 2)
                .unwrap()
                .len(),
            6
        );

        // DROP borra todo.
        assert!(vector_index_exists(&s, root, "v_idx").unwrap());
        let (root, dropped) = drop_vector_index(&mut s, root, "v_idx").unwrap();
        assert!(dropped);
        assert!(
            get_table(&s, root, "docs")
                .unwrap()
                .unwrap()
                .vector_indexes
                .is_empty()
        );
        assert!(!vector_index_exists(&s, root, "v_idx").unwrap());
        assert!(
            vector_search(&s, root, &vidx, &[0.05, 0.05], 2)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn schema_v7_decodes_with_empty_fts() {
        // Compatibilidad hacia atrás: un esquema v7 (sin bloques FTS ni vectorial)
        // debe seguir leyéndose. Degradamos un v9 con 0 índices FTS/vectoriales
        // quitando sus dos bytes de count (vectorial v9, luego FTS v8) y marcando
        // la versión 7.
        let mut s = MemStore::new();
        let (_, def) = create_table(&mut s, NO_ROOT, &facturas_spec()).unwrap();
        let mut bytes = encode_def(&def);
        assert_eq!(bytes[0], 9);
        assert_eq!(bytes.pop(), Some(0), "count vectorial = 0 al final");
        assert_eq!(bytes.pop(), Some(0), "count FTS = 0");
        bytes[0] = 7;
        let back = decode_def(&def.name, &bytes).unwrap();
        assert!(back.fts_indexes.is_empty());
        assert!(back.vector_indexes.is_empty());
    }

    fn mail_spec() -> TableSpec {
        TableSpec {
            name: "mail".into(),
            columns: vec![
                ColumnSpec {
                    primary_key: true,
                    ..col("id", ColType::Integer)
                },
                col("subject", ColType::Text),
                col("body", ColType::Text),
            ],
            foreign_keys: Vec::new(),
            uniques: Vec::new(),
            checks: Vec::new(),
        }
    }

    fn insert_mail(
        s: &mut MemStore,
        root: PageId,
        def: &TableDef,
        sub: &str,
        body: &str,
    ) -> PageId {
        insert_row(
            s,
            root,
            def,
            &[
                Value::Null,
                Value::Text(sub.into()),
                Value::Text(body.into()),
            ],
        )
        .unwrap()
        .0
    }

    #[test]
    fn fts_backfill_postings_and_stats() {
        let mut s = MemStore::new();
        let (mut root, def) = create_table(&mut s, NO_ROOT, &mail_spec()).unwrap();
        root = insert_mail(&mut s, root, &def, "hola mundo", "el mundo es grande");
        root = insert_mail(&mut s, root, &def, "adios mundo", "hasta luego");
        root = create_fts_index(&mut s, root, "mail", "fts_mail", &[1, 2], "unicode").unwrap();

        let fts_id = get_table(&s, root, "mail").unwrap().unwrap().fts_indexes[0].fts_id;
        // "mundo" está en la fila 1 (subject y body) y en la 2 (subject) → 2 docs.
        assert_eq!(
            fts_term_rowids(&s, root, fts_id, "mundo").unwrap(),
            vec![1, 2]
        );
        assert_eq!(fts_doc_freq(&s, root, fts_id, "mundo").unwrap(), 2);
        assert_eq!(fts_doc_freq(&s, root, fts_id, "grande").unwrap(), 1);
        assert!(
            fts_term_rowids(&s, root, fts_id, "inexistente")
                .unwrap()
                .is_empty()
        );
        // doclen fila 1 = tokens(subject)+tokens(body) = 2 + 4 = 6; fila 2 = 2 + 2.
        assert_eq!(fts_doc_len(&s, root, fts_id, 1).unwrap(), 6);
        assert_eq!(fts_doc_len(&s, root, fts_id, 2).unwrap(), 4);
        // globales: N = 2 docs, Σ = 10 tokens.
        assert_eq!(fts_global_stats(&s, root, fts_id).unwrap(), (2, 10));
    }

    #[test]
    fn fts_insert_delete_maintenance_reverts_stats() {
        let mut s = MemStore::new();
        let (mut root, _) = create_table(&mut s, NO_ROOT, &mail_spec()).unwrap();
        root = create_fts_index(&mut s, root, "mail", "fts_mail", &[1, 2], "unicode").unwrap();
        let def = get_table(&s, root, "mail").unwrap().unwrap();
        let fts_id = def.fts_indexes[0].fts_id;
        assert_eq!(fts_global_stats(&s, root, fts_id).unwrap(), (0, 0));

        // "mundo" sale dos veces en la misma fila ⇒ df cuenta 1 doc distinto.
        let rec = [
            Value::Integer(1),
            Value::Text("hola mundo".into()),
            Value::Text("mundo cruel".into()),
        ];
        root = insert_fts_entries(&mut s, root, &def, 1, &rec).unwrap();
        assert_eq!(fts_term_rowids(&s, root, fts_id, "mundo").unwrap(), vec![1]);
        assert_eq!(fts_doc_freq(&s, root, fts_id, "mundo").unwrap(), 1);
        assert_eq!(fts_doc_len(&s, root, fts_id, 1).unwrap(), 4);
        assert_eq!(fts_global_stats(&s, root, fts_id).unwrap(), (1, 4));

        // Revertir lo deja todo a cero (índice consistente con el dato borrado).
        root = delete_fts_entries(&mut s, root, &def, 1, &rec).unwrap();
        assert!(
            fts_term_rowids(&s, root, fts_id, "mundo")
                .unwrap()
                .is_empty()
        );
        assert_eq!(fts_doc_freq(&s, root, fts_id, "mundo").unwrap(), 0);
        assert_eq!(fts_doc_len(&s, root, fts_id, 1).unwrap(), 0);
        assert_eq!(fts_global_stats(&s, root, fts_id).unwrap(), (0, 0));
    }

    #[test]
    fn fts_drop_removes_all_entries_and_schema() {
        let mut s = MemStore::new();
        let (mut root, def) = create_table(&mut s, NO_ROOT, &mail_spec()).unwrap();
        root = insert_mail(&mut s, root, &def, "hola mundo", "texto");
        root = create_fts_index(&mut s, root, "mail", "fts_mail", &[1, 2], "unicode").unwrap();
        let fts_id = get_table(&s, root, "mail").unwrap().unwrap().fts_indexes[0].fts_id;

        assert!(fts_index_exists(&s, root, "fts_mail").unwrap());
        let (root, dropped) = drop_fts_index(&mut s, root, "fts_mail").unwrap();
        assert!(dropped);
        assert!(
            get_table(&s, root, "mail")
                .unwrap()
                .unwrap()
                .fts_indexes
                .is_empty()
        );
        assert!(!fts_index_exists(&s, root, "fts_mail").unwrap());
        // Ni postings ni stats quedan bajo el fts_id.
        assert!(
            fts_term_rowids(&s, root, fts_id, "mundo")
                .unwrap()
                .is_empty()
        );
        assert_eq!(fts_global_stats(&s, root, fts_id).unwrap(), (0, 0));
        // Idempotente sobre un nombre inexistente.
        let (_, again) = drop_fts_index(&mut s, root, "fts_mail").unwrap();
        assert!(!again);
    }

    #[test]
    fn fts_create_validations() {
        let mut s = MemStore::new();
        let (root, _) = create_table(&mut s, NO_ROOT, &mail_spec()).unwrap();
        // Columna no-TEXT (id) → InvalidInput.
        assert!(matches!(
            create_fts_index(&mut s, root, "mail", "fx", &[0], "unicode"),
            Err(Error::InvalidInput(_))
        ));
        // Tokenizer desconocido → error SQL.
        assert!(matches!(
            create_fts_index(&mut s, root, "mail", "fx", &[1], "porter"),
            Err(Error::Sql { .. })
        ));
        // Nombre duplicado → Constraint.
        let root = create_fts_index(&mut s, root, "mail", "fx", &[1], "unicode").unwrap();
        assert!(matches!(
            create_fts_index(&mut s, root, "mail", "fx", &[2], "unicode"),
            Err(Error::Constraint(_))
        ));
    }

    #[test]
    fn fts_row_ops_maintain_index() {
        let mut s = MemStore::new();
        let (mut root, _) = create_table(&mut s, NO_ROOT, &mail_spec()).unwrap();
        root = create_fts_index(&mut s, root, "mail", "fts_mail", &[1, 2], "unicode").unwrap();
        let def = get_table(&s, root, "mail").unwrap().unwrap();
        let fts_id = def.fts_indexes[0].fts_id;

        // INSERT mantiene el índice (hook en put_row).
        let (r, rid) = insert_row(
            &mut s,
            root,
            &def,
            &[
                Value::Null,
                Value::Text("hola mundo".into()),
                Value::Text("texto".into()),
            ],
        )
        .unwrap();
        root = r;
        assert_eq!(
            fts_term_rowids(&s, root, fts_id, "mundo").unwrap(),
            vec![rid]
        );
        assert_eq!(fts_global_stats(&s, root, fts_id).unwrap(), (1, 3));

        // UPDATE re-tokeniza: quita los términos viejos y mete los nuevos.
        let (r, ok) = update_row(
            &mut s,
            root,
            &def,
            rid,
            &[
                Value::Integer(rid),
                Value::Text("adios".into()),
                Value::Text("planeta".into()),
            ],
        )
        .unwrap();
        root = r;
        assert!(ok);
        assert!(
            fts_term_rowids(&s, root, fts_id, "mundo")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            fts_term_rowids(&s, root, fts_id, "planeta").unwrap(),
            vec![rid]
        );
        assert_eq!(fts_global_stats(&s, root, fts_id).unwrap(), (1, 2));

        // DELETE limpia las entradas de la fila.
        let (r, ok) = delete_row(&mut s, root, &def, rid).unwrap();
        root = r;
        assert!(ok);
        assert!(
            fts_term_rowids(&s, root, fts_id, "planeta")
                .unwrap()
                .is_empty()
        );
        assert_eq!(fts_global_stats(&s, root, fts_id).unwrap(), (0, 0));
    }

    fn seed_corpus(s: &mut MemStore) -> (PageId, TableDef) {
        let (mut root, def) = create_table(s, NO_ROOT, &mail_spec()).unwrap();
        root = insert_mail(s, root, &def, "hola mundo", "el mundo es grande");
        root = insert_mail(s, root, &def, "adios planeta", "mundo cruel y frio");
        root = insert_mail(s, root, &def, "noticias", "el planeta tierra");
        root = create_fts_index(s, root, "mail", "fts_mail", &[1, 2], "unicode").unwrap();
        let def = get_table(s, root, "mail").unwrap().unwrap();
        (root, def)
    }

    fn search(s: &MemStore, root: PageId, def: &TableDef, q: &str) -> Vec<i64> {
        let query = crate::fts::parse_query(q).unwrap();
        fts_search(s, root, def, &def.fts_indexes[0], &query).unwrap()
    }

    #[test]
    fn fts_search_boolean_and_prefix() {
        let mut s = MemStore::new();
        let (root, def) = seed_corpus(&mut s);
        assert_eq!(search(&s, root, &def, "mundo"), vec![1, 2]);
        assert_eq!(search(&s, root, &def, "planeta"), vec![2, 3]);
        assert_eq!(search(&s, root, &def, "mundo AND planeta"), vec![2]);
        assert_eq!(search(&s, root, &def, "mundo OR planeta"), vec![1, 2, 3]);
        assert_eq!(search(&s, root, &def, "mundo NOT planeta"), vec![1]);
        assert_eq!(search(&s, root, &def, "mun*"), vec![1, 2]);
        assert!(search(&s, root, &def, "inexistente").is_empty());
    }

    #[test]
    fn fts_search_phrase_near_and_column() {
        let mut s = MemStore::new();
        let (root, def) = seed_corpus(&mut s);
        // Frase: "el mundo" adyacente solo en el body de la fila 1.
        assert_eq!(search(&s, root, &def, "\"el mundo\""), vec![1]);
        assert_eq!(search(&s, root, &def, "\"el planeta\""), vec![3]);
        // NEAR: mundo y grande a ≤5 posiciones (fila 1: pos 1 y 3).
        assert_eq!(search(&s, root, &def, "NEAR(mundo grande, 5)"), vec![1]);
        // …pero no a ≤1.
        assert!(search(&s, root, &def, "NEAR(mundo grande, 1)").is_empty());
        // Filtro por columna: mundo en subject ⇒ fila 1; en body ⇒ filas 1 y 2.
        assert_eq!(search(&s, root, &def, "subject:mundo"), vec![1]);
        assert_eq!(search(&s, root, &def, "body:mundo"), vec![1, 2]);
        assert_eq!(search(&s, root, &def, "body:planeta"), vec![3]);
    }

    #[test]
    fn schema_roundtrip_and_table_id_sequence() {
        let mut s = MemStore::new();
        let (root, def) = create_table(&mut s, NO_ROOT, &facturas_spec()).unwrap();
        assert_eq!(def.table_id, 1);
        assert_eq!(def.rowid_alias, Some(0));
        assert!(def.columns[0].not_null, "la PK implica NOT NULL");

        let loaded = get_table(&s, root, "facturas").unwrap().unwrap();
        assert_eq!(loaded, def);
        assert_eq!(get_table(&s, root, "nada").unwrap(), None);

        let spec2 = TableSpec {
            name: "clientes".into(),
            columns: vec![col("n", ColType::Text)],
            foreign_keys: Vec::new(),
            uniques: Vec::new(),
            checks: Vec::new(),
        };
        let (_, def2) = create_table(&mut s, root, &spec2).unwrap();
        assert_eq!(def2.table_id, 2);
        assert_eq!(def2.rowid_alias, None);
    }

    #[test]
    fn create_table_validations() {
        let mut s = MemStore::new();
        let dup = TableSpec {
            name: "t".into(),
            columns: vec![col("a", ColType::Text), col("a", ColType::Real)],
            foreign_keys: Vec::new(),
            uniques: Vec::new(),
            checks: Vec::new(),
        };
        assert!(matches!(
            create_table(&mut s, NO_ROOT, &dup),
            Err(Error::InvalidInput(_))
        ));

        let bad_pk = TableSpec {
            name: "t".into(),
            columns: vec![ColumnSpec {
                primary_key: true,
                ..col("k", ColType::Text)
            }],
            foreign_keys: Vec::new(),
            uniques: Vec::new(),
            checks: Vec::new(),
        };
        assert!(matches!(
            create_table(&mut s, NO_ROOT, &bad_pk),
            Err(Error::InvalidInput(_))
        ));

        let (root, _) = create_table(
            &mut s,
            NO_ROOT,
            &TableSpec {
                name: "t".into(),
                columns: vec![col("a", ColType::Text)],
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: Vec::new(),
            },
        )
        .unwrap();
        let again = TableSpec {
            name: "t".into(),
            columns: vec![col("b", ColType::Text)],
            foreign_keys: Vec::new(),
            uniques: Vec::new(),
            checks: Vec::new(),
        };
        assert!(matches!(
            create_table(&mut s, root, &again),
            Err(Error::Constraint(_))
        ));
    }

    #[test]
    fn insert_defaults_alias_and_validation() {
        let mut s = MemStore::new();
        let (mut root, def) = create_table(&mut s, NO_ROOT, &facturas_spec()).unwrap();

        // Rowid automático; 'estado' ausente toma el DEFAULT.
        let (r, id1) = insert_row(&mut s, root, &def, &[Value::Null, Value::Real(10.0)]).unwrap();
        root = r;
        assert_eq!(id1, 1);
        let row = get_row(&s, root, &def, 1).unwrap().unwrap();
        assert_eq!(
            row[0],
            Value::Integer(1),
            "el alias se reconstruye del rowid"
        );
        assert_eq!(row[2], Value::Text("borrador".into()));
        assert_eq!(row[3], Value::Null);

        // Promoción INTEGER → REAL.
        let (r, _) = insert_row(&mut s, root, &def, &[Value::Null, Value::Integer(7)]).unwrap();
        root = r;
        assert_eq!(
            get_row(&s, root, &def, 2).unwrap().unwrap()[1],
            Value::Real(7.0)
        );

        // Errores de tipo y NOT NULL.
        assert!(matches!(
            insert_row(&mut s, root, &def, &[Value::Null, Value::Text("no".into())]),
            Err(Error::Constraint(_))
        ));
        assert!(matches!(
            insert_row(&mut s, root, &def, &[Value::Null, Value::Null, Value::Null]),
            Err(Error::Constraint(_))
        ));
    }

    #[test]
    fn explicit_rowids_bump_counter_and_reject_duplicates() {
        let mut s = MemStore::new();
        let (mut root, def) = create_table(&mut s, NO_ROOT, &facturas_spec()).unwrap();

        let (r, id) = insert_row(&mut s, root, &def, &[Value::Integer(100)]).unwrap();
        root = r;
        assert_eq!(id, 100);
        // El contador salta por encima del explícito (estilo SQLite).
        let (r, id) = insert_row(&mut s, root, &def, &[Value::Null]).unwrap();
        root = r;
        assert_eq!(id, 101);
        // Negativos: válidos y no mueven el contador.
        let (r, id) = insert_row(&mut s, root, &def, &[Value::Integer(-5)]).unwrap();
        root = r;
        assert_eq!(id, -5);
        let (r, id) = insert_row(&mut s, root, &def, &[Value::Null]).unwrap();
        root = r;
        assert_eq!(id, 102);
        // Duplicado explícito.
        assert!(matches!(
            insert_row(&mut s, root, &def, &[Value::Integer(100)]),
            Err(Error::Constraint(_))
        ));
    }

    #[test]
    fn scan_order_is_rowid_order_with_negatives() {
        let mut s = MemStore::new();
        let (mut root, def) = create_table(&mut s, NO_ROOT, &facturas_spec()).unwrap();
        for id in [5i64, -3, 0, 99, -88, 7] {
            let (r, got) = insert_row(&mut s, root, &def, &[Value::Integer(id)]).unwrap();
            root = r;
            assert_eq!(got, id);
        }
        let ids: Vec<i64> = scan_table(&s, root, &def)
            .unwrap()
            .map(|r| r.unwrap().0)
            .collect();
        assert_eq!(ids, vec![-88, -3, 0, 5, 7, 99]);
    }

    #[test]
    fn tables_are_isolated_and_drop_removes_rows() {
        let mut s = MemStore::new();
        let (root, t1) = create_table(&mut s, NO_ROOT, &facturas_spec()).unwrap();
        let spec2 = TableSpec {
            name: "clientes".into(),
            columns: vec![col("nombre", ColType::Text)],
            foreign_keys: Vec::new(),
            uniques: Vec::new(),
            checks: Vec::new(),
        };
        let (mut root, t2) = create_table(&mut s, root, &spec2).unwrap();

        for i in 0..10 {
            let (r, _) = insert_row(&mut s, root, &t1, &[Value::Integer(i)]).unwrap();
            let (r, _) = insert_row(&mut s, r, &t2, &[Value::Text(format!("c{i}"))]).unwrap();
            root = r;
        }
        assert_eq!(scan_table(&s, root, &t1).unwrap().count(), 10);
        assert_eq!(scan_table(&s, root, &t2).unwrap().count(), 10);

        let (root, dropped) = drop_table(&mut s, root, "facturas").unwrap();
        assert!(dropped);
        assert_eq!(get_table(&s, root, "facturas").unwrap(), None);
        // Las filas de la otra tabla quedan intactas.
        assert_eq!(scan_table(&s, root, &t2).unwrap().count(), 10);
        // Recrear la tabla: id nuevo, sin filas fantasma.
        let (root, t1b) = create_table(&mut s, root, &facturas_spec()).unwrap();
        assert_eq!(t1b.table_id, 3);
        assert_eq!(scan_table(&s, root, &t1b).unwrap().count(), 0);
    }

    #[test]
    fn update_row_overwrites_in_place() {
        let mut s = MemStore::new();
        let (mut root, def) = create_table(&mut s, NO_ROOT, &facturas_spec()).unwrap();
        let (r, id) = insert_row(&mut s, root, &def, &[Value::Null, Value::Real(1.0)]).unwrap();
        root = r;
        let (r, ok) = update_row(
            &mut s,
            root,
            &def,
            id,
            &[Value::Null, Value::Real(2.0), Value::Text("emitida".into())],
        )
        .unwrap();
        root = r;
        assert!(ok);
        let row = get_row(&s, root, &def, id).unwrap().unwrap();
        assert_eq!(row[1], Value::Real(2.0));
        assert_eq!(row[2], Value::Text("emitida".into()));
        let (_, ok) = update_row(&mut s, root, &def, 999, &[Value::Null]).unwrap();
        assert!(!ok);
    }

    #[test]
    fn logical_order_roundtrip_and_v2_compat() {
        // is_permutation: la guardia de integridad al decodificar.
        assert!(is_permutation(&[0, 1, 2], 3));
        assert!(!is_permutation(&[0, 1, 1], 3), "duplicado");
        assert!(!is_permutation(&[0, 1, 3], 3), "fuera de rango");
        assert!(!is_permutation(&[0, 1], 3), "longitud distinta");

        let mut s = MemStore::new();
        let (_, def) = create_table(&mut s, NO_ROOT, &facturas_spec()).unwrap();

        // Retrocompat: un esquema **v2** hecho a mano (sin orden lógico, sin FKs,
        // sin flag `dropped`) decodifica con identidad, FKs vacías y `dropped=false`.
        let v2 = [
            2u8, // versión
            1,
            0,
            0,
            0,        // table_id = 1 (LE)
            NO_ALIAS, // sin PK
            1,        // ncols = 1
            1,
            b'x',                   // nombre "x"
            ColType::Integer as u8, // tipo
            0,                      // not_null = false
            0,                      // default: ninguno
            0,                      // nidx = 0
        ];
        let from_v2 = decode_def("t", &v2).unwrap();
        assert_eq!(from_v2.columns.len(), 1);
        assert_eq!(from_v2.columns[0].name, "x");
        assert!(!from_v2.columns[0].dropped);
        assert_eq!(from_v2.logical_order, vec![0]);
        assert!(from_v2.foreign_keys.is_empty());

        // v5: round-trip exacto de una permutación de orden lógico.
        let mut perm = def.clone();
        perm.logical_order = vec![2, 0, 4, 1, 3];
        let decoded = decode_def("facturas", &encode_def(&perm)).unwrap();
        assert_eq!(decoded.logical_order, vec![2, 0, 4, 1, 3]);
        assert_eq!(decoded, perm);

        // v5: round-trip del flag `dropped` (la columna se queda en logical_order;
        // la presentación la filtra).
        let mut dropped = def.clone();
        dropped.columns[2].dropped = true;
        let decoded = decode_def("facturas", &encode_def(&dropped)).unwrap();
        assert!(decoded.columns[2].dropped);
        assert_eq!(decoded.logical_order, dropped.logical_order); // intacto (5 entradas)
        assert_eq!(decoded, dropped);

        // v6: round-trip de una FK **compuesta** con ON DELETE/UPDATE y columnas
        // explícitas del padre.
        let mut withfk = def.clone();
        withfk.foreign_keys = vec![ForeignKey {
            columns: vec![1, 3],
            parent: "otra".into(),
            parent_columns: vec![0, 2],
            on_delete: FkAction::Cascade,
            on_update: FkAction::SetNull,
        }];
        let decoded = decode_def("facturas", &encode_def(&withfk)).unwrap();
        assert_eq!(decoded.foreign_keys, withfk.foreign_keys);

        // Retrocompat: una FK **v4** (una columna → PK, solo ON DELETE) hecha a
        // mano decodifica como `columns=[c]`, `parent_columns=[]` (rowid) y
        // `on_update=Restrict`.
        let v4 = [
            4u8, // versión
            1,
            0,
            0,
            0,        // table_id = 1 (LE)
            NO_ALIAS, // sin PK
            1,        // ncols = 1
            1,
            b'x', // nombre "x"
            ColType::Integer as u8,
            0, // not_null = false
            0, // default: ninguno
            0, // nidx = 0
            0, // orden lógico (v>=3): marcador 0 = identidad
            1, // nfk = 1
            0, // columna hija = 0
            1, // on_delete = Cascade
            1,
            b'p', // padre "p" (len 1)
        ];
        let from_v4 = decode_def("t", &v4).unwrap();
        assert_eq!(from_v4.foreign_keys.len(), 1);
        let fk = &from_v4.foreign_keys[0];
        assert_eq!(fk.columns, vec![0]);
        assert_eq!(fk.parent, "p");
        assert!(fk.parent_columns.is_empty()); // referencia la PK por rowid
        assert_eq!(fk.on_delete, FkAction::Cascade);
        assert_eq!(fk.on_update, FkAction::Restrict); // ausente en v4 ⇒ por defecto
    }

    #[test]
    fn move_and_reorder_columns_logical() {
        let mut s = MemStore::new();
        // físicos: 0 id, 1 total, 2 estado, 3 pagada, 4 adjunto.
        let (root, _) = create_table(&mut s, NO_ROOT, &facturas_spec()).unwrap();

        let (root, def) =
            move_column(&mut s, root, "facturas", "adjunto", &ColumnPos::First).unwrap();
        assert_eq!(def.logical_order, vec![4, 0, 1, 2, 3]);

        // [4,0,1,2,3]: saca id(0) → [4,1,2,3]; total=1 está en idx1; AFTER → idx2.
        let (root, def) = move_column(
            &mut s,
            root,
            "facturas",
            "id",
            &ColumnPos::After("total".into()),
        )
        .unwrap();
        assert_eq!(def.logical_order, vec![4, 1, 0, 2, 3]);

        // BEFORE: [4,1,0,2,3]: saca adjunto(4) → [1,0,2,3]; id=0 en idx1 → insert ahí.
        let (root, def) = move_column(
            &mut s,
            root,
            "facturas",
            "adjunto",
            &ColumnPos::Before("id".into()),
        )
        .unwrap();
        assert_eq!(def.logical_order, vec![1, 4, 0, 2, 3]);

        let cols = ["id", "total", "estado", "pagada", "adjunto"].map(String::from);
        let (root, def) = reorder_columns(&mut s, root, "facturas", &cols).unwrap();
        assert_eq!(def.logical_order, vec![0, 1, 2, 3, 4]);

        // La posición FÍSICA y los datos no se tocan nunca.
        assert_eq!(def.columns[0].name, "id");
        assert_eq!(def.rowid_alias, Some(0));

        // Errores.
        assert!(move_column(&mut s, root, "facturas", "nope", &ColumnPos::First).is_err());
        assert!(
            move_column(
                &mut s,
                root,
                "facturas",
                "id",
                &ColumnPos::After("id".into())
            )
            .is_err(),
            "referencia a sí misma"
        );
        assert!(
            move_column(
                &mut s,
                root,
                "facturas",
                "id",
                &ColumnPos::After("nope".into())
            )
            .is_err(),
            "referencia desconocida"
        );
        assert!(
            reorder_columns(&mut s, root, "facturas", &["id".to_string()]).is_err(),
            "faltan columnas"
        );
        let dup = ["id", "total", "estado", "pagada", "pagada"].map(String::from);
        assert!(
            reorder_columns(&mut s, root, "facturas", &dup).is_err(),
            "columna repetida"
        );
    }
}
