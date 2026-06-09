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
//! [0x01, table_id BE, rowid BE*]    → registro      (* bit de signo invertido)
//! [0x02, index_id BE, valor*, rowid BE*] → entrada de índice (valor memcomparable)
//! ```

use crate::btree::{self, Cursor, NodeSource, NodeStore};
use crate::error::{Error, Result};
use crate::format::{PageId, put_varint, take_varint};
use crate::keyenc;
use crate::record::{self, Value};

pub const MAX_COLUMNS: usize = 255;
pub const MAX_NAME_LEN: usize = 128;

const KS_CATALOG: u8 = 0x00;
const KS_ROW: u8 = 0x01;
const KS_INDEX: u8 = 0x02;
const CAT_META: u8 = 0x00;
const CAT_TABLE: u8 = 0x01;
const CAT_COUNTER: u8 = 0x02;
const CAT_INDEX_COUNTER: u8 = 0x03;
const CAT_INDEX_REF: u8 = 0x04;

/// v2: el esquema serializado incluye la lista de índices de la tabla.
const SCHEMA_VERSION: u8 = 2;
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

/// Definición de columna tal y como la pide el llamador.
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnSpec {
    pub name: String,
    pub col_type: ColType,
    pub not_null: bool,
    pub primary_key: bool,
    pub default: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TableSpec {
    pub name: String,
    pub columns: Vec<ColumnSpec>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub col_type: ColType,
    pub not_null: bool,
    pub default: Option<Value>,
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

pub fn row_key(table_id: u32, rowid: i64) -> [u8; 13] {
    let mut k = [0u8; 13];
    k[0] = KS_ROW;
    k[1..5].copy_from_slice(&table_id.to_be_bytes());
    k[5..13].copy_from_slice(&record::rowid_be(rowid));
    k
}

fn row_prefix(table_id: u32) -> [u8; 5] {
    let mut p = [0u8; 5];
    p[0] = KS_ROW;
    p[1..5].copy_from_slice(&table_id.to_be_bytes());
    p
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
    out
}

fn decode_def(name: &str, buf: &[u8]) -> Result<TableDef> {
    let bad = |reason: &'static str| Error::CorruptRecord(reason);
    let mut pos = 0usize;
    let take = |pos: &mut usize, n: usize| -> Result<&[u8]> {
        let s = buf.get(*pos..*pos + n).ok_or(bad("esquema truncado"))?;
        *pos += n;
        Ok(s)
    };

    if *take(&mut pos, 1)?.first().expect("len 1") != SCHEMA_VERSION {
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
        columns.push(ColumnDef {
            name: cname,
            col_type,
            not_null,
            default,
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
    })
}

// --- operaciones de catálogo ---

fn validate_name(name: &str, what: &'static str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return Err(Error::InvalidInput(what));
    }
    Ok(())
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
            })
            .collect(),
        indexes: Vec::new(),
    };
    root = btree::insert(s, root, &table_key(&spec.name), &encode_def(&def))?;
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
    let root = btree::insert(s, root, &table_key(table_name), &encode_def(&def))?;
    Ok((root, def))
}

pub fn get_table<S: NodeSource>(src: &S, root: PageId, name: &str) -> Result<Option<TableDef>> {
    match btree::get(src, root, &table_key(name))? {
        Some(bytes) => Ok(Some(decode_def(name, &bytes)?)),
        None => Ok(None),
    }
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

/// Valida tipos y restricciones, aplica defaults y devuelve el registro listo
/// para almacenar (el alias del rowid se guarda como NULL: se reconstruye).
fn validated_record(table: &TableDef, values: &[Value], rowid: i64) -> Result<Vec<Value>> {
    let mut record = Vec::with_capacity(table.columns.len());
    validate_record_into(table, values, rowid, &mut record)?;
    Ok(record)
}

/// Como [`validated_record`] pero en un `Vec` **reutilizado** (lo limpia antes):
/// el camino caliente de `insert_row` evita asignar el vector por fila (M10-perf).
fn validate_record_into(
    table: &TableDef,
    values: &[Value],
    rowid: i64,
    record: &mut Vec<Value>,
) -> Result<()> {
    record.clear();
    if values.len() > table.columns.len() {
        return Err(Error::InvalidInput("más valores que columnas"));
    }
    for (i, col) in table.columns.iter().enumerate() {
        if table.rowid_alias == Some(i) {
            record.push(Value::Null); // reconstruido del rowid al leer
            continue;
        }
        let value = match values.get(i) {
            Some(v) => v.clone(),
            None => col.default.clone().unwrap_or(Value::Null),
        };
        let value = match (col.col_type, value) {
            (_, Value::Null) => {
                if col.not_null {
                    return Err(Error::Constraint("NULL en columna NOT NULL"));
                }
                Value::Null
            }
            (ColType::Integer, v @ Value::Integer(_)) => v,
            // Promoción sin pérdida: INTEGER → REAL (docs/04).
            (ColType::Real, Value::Integer(n)) => Value::Real(n as f64),
            (ColType::Real, v @ Value::Real(_)) => v,
            (ColType::Text, v @ Value::Text(_)) => v,
            (ColType::Blob, v @ Value::Blob(_)) => v,
            (ColType::Boolean, v @ Value::Bool(_)) => v,
            (expected, got) => {
                let _ = (expected, got);
                return Err(Error::Constraint(
                    "tipo de valor incompatible con la columna",
                ));
            }
        };
        record.push(value);
    }
    let _ = rowid;
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
            if btree::contains(src, root, &row_key(table_id, n))? {
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
    let record = validated_record(table, values, rowid)?;
    let mut root = btree::insert(
        s,
        root,
        &row_key(table.table_id, rowid),
        &record::encode_values(&record),
    )?;
    if !table.indexes.is_empty() {
        root = insert_index_entries(s, root, table, rowid, &record)?;
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
    validate_record_into(table, values, rowid, rec_buf)?;
    record::encode_values_into(rec_buf, enc_buf);
    let mut root = btree::insert(s, root, &row_key(table.table_id, rowid), enc_buf)?;
    if !table.indexes.is_empty() {
        root = insert_index_entries(s, root, table, rowid, rec_buf.as_slice())?;
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
    let record = validated_record(table, values, rowid)?;
    let mut root = root;
    // Quita las entradas de índice de la fila vieja antes de sobrescribir.
    if !table.indexes.is_empty() {
        let old = finish_row(table, rowid, record::decode_values(&old_bytes)?)?;
        root = delete_index_entries(s, root, table, rowid, &old)?;
    }
    root = btree::insert(s, root, &key, &record::encode_values(&record))?;
    if !table.indexes.is_empty() {
        root = insert_index_entries(s, root, table, rowid, &record)?;
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
    if table.indexes.is_empty() {
        return btree::delete(s, root, &key);
    }
    // Con índices: hay que leer la fila para quitar sus entradas.
    let Some(bytes) = btree::get(s, root, &key)? else {
        return Ok((root, false));
    };
    let record = finish_row(table, rowid, record::decode_values(&bytes)?)?;
    let (mut root, _) = btree::delete(s, root, &key)?;
    root = delete_index_entries(s, root, table, rowid, &record)?;
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

/// Iterador por rowid ascendente: el orden del scan ES el orden del rowid.
pub struct TableScan<'s, S: NodeSource> {
    cur: Cursor<'s, S>,
    prefix: [u8; 5],
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
            let rowid = record::rowid_from_be(&key[5..])
                .ok_or(Error::CorruptRecord("clave de fila mal formada"))?;
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
        }
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
            },
        )
        .unwrap();
        let again = TableSpec {
            name: "t".into(),
            columns: vec![col("b", ColType::Text)],
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
}
