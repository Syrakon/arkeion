//! Catálogo relacional sobre el árbol de datos (docs/02-formato-archivo.md).
//!
//! El esquema vive en el árbol que **ramifica**: una migración en una rama
//! cambia el esquema solo en esa rama (M8). Espacios de claves:
//!
//! ```text
//! [0x00,0x00]                       → próximo table_id (u32 LE)
//! [0x00,0x01, nombre UTF-8]         → esquema serializado
//! [0x00,0x02, table_id BE]          → próximo rowid (i64 LE)
//! [0x01, table_id BE, rowid BE*]    → registro      (* bit de signo invertido)
//! [0x02, …]                           reservado: índices secundarios (v1.1)
//! ```

use crate::btree::{self, Cursor, NodeSource, NodeStore};
use crate::error::{Error, Result};
use crate::format::{PageId, put_varint, take_varint};
use crate::record::{self, Value};

pub const MAX_COLUMNS: usize = 255;
pub const MAX_NAME_LEN: usize = 128;

const KS_CATALOG: u8 = 0x00;
const KS_ROW: u8 = 0x01;
const CAT_META: u8 = 0x00;
const CAT_TABLE: u8 = 0x01;
const CAT_COUNTER: u8 = 0x02;

const SCHEMA_VERSION: u8 = 1;
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

#[derive(Clone, Debug, PartialEq)]
pub struct TableDef {
    pub name: String,
    pub table_id: u32,
    /// Columna `INTEGER PRIMARY KEY` = alias del rowid (estilo SQLite, D11).
    /// Su valor no se almacena en el registro: se reconstruye del rowid.
    pub rowid_alias: Option<usize>,
    pub columns: Vec<ColumnDef>,
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
    };
    root = btree::insert(s, root, &table_key(&spec.name), &encode_def(&def))?;
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
    (root, _) = btree::delete(s, root, &counter_key(def.table_id))?;
    (root, _) = btree::delete(s, root, &table_key(name))?;
    Ok((root, true))
}

// --- filas ---

/// Valida tipos y restricciones, aplica defaults y devuelve el registro listo
/// para almacenar (el alias del rowid se guarda como NULL: se reconstruye).
fn validated_record(table: &TableDef, values: &[Value], rowid: i64) -> Result<Vec<Value>> {
    if values.len() > table.columns.len() {
        return Err(Error::InvalidInput("más valores que columnas"));
    }
    let mut record = Vec::with_capacity(table.columns.len());
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
    Ok(record)
}

/// Inserta con rowid automático o explícito (si la columna alias trae un
/// entero). Falla con `Constraint` si el rowid explícito ya existe.
pub fn insert_row<S: NodeStore>(
    s: &mut S,
    root: PageId,
    table: &TableDef,
    values: &[Value],
) -> Result<(PageId, i64)> {
    let explicit = match table.rowid_alias {
        Some(i) => match values.get(i) {
            None | Some(Value::Null) => None,
            Some(Value::Integer(n)) => Some(*n),
            Some(_) => return Err(Error::Constraint("la PRIMARY KEY debe ser un entero")),
        },
        None => None,
    };

    let next = match btree::get(s, root, &counter_key(table.table_id))? {
        Some(v) => i64::from_le_bytes(
            v.as_slice()
                .try_into()
                .map_err(|_| Error::CorruptRecord("contador de rowid"))?,
        ),
        None => 1,
    };
    let (rowid, new_next) = match explicit {
        None => (
            next,
            next.checked_add(1)
                .ok_or(Error::Constraint("rowids agotados"))?,
        ),
        Some(n) => {
            if btree::contains(s, root, &row_key(table.table_id, n))? {
                return Err(Error::Constraint("rowid duplicado"));
            }
            (n, if n >= next { n.saturating_add(1) } else { next })
        }
    };

    let record = validated_record(table, values, rowid)?;
    let mut root = btree::insert(
        s,
        root,
        &counter_key(table.table_id),
        &new_next.to_le_bytes(),
    )?;
    root = btree::insert(
        s,
        root,
        &row_key(table.table_id, rowid),
        &record::encode_values(&record),
    )?;
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
    if btree::get(s, root, &key)?.is_none() {
        return Ok((root, false));
    }
    let record = validated_record(table, values, rowid)?;
    let root = btree::insert(s, root, &key, &record::encode_values(&record))?;
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
    btree::delete(s, root, &row_key(table.table_id, rowid))
}

/// Rellena columnas ausentes con NULL (esquema más nuevo que la fila) y
/// reconstruye la columna alias desde el rowid.
fn finish_row(table: &TableDef, rowid: i64, mut values: Vec<Value>) -> Result<Vec<Value>> {
    if values.len() > table.columns.len() {
        return Err(Error::CorruptRecord(
            "la fila tiene más columnas que el esquema",
        ));
    }
    values.resize(table.columns.len(), Value::Null);
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
    ncols: usize,
    rowid_alias: Option<usize>,
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
        ncols: table.columns.len(),
        rowid_alias: table.rowid_alias,
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
            let mut values = record::decode_values(&payload)?;
            if values.len() > self.ncols {
                return Err(Error::CorruptRecord(
                    "la fila tiene más columnas que el esquema",
                ));
            }
            values.resize(self.ncols, Value::Null);
            if let Some(i) = self.rowid_alias {
                values[i] = Value::Integer(rowid);
            }
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
