//! Ejecutor SQL (docs/04-sql.md): planificador mínimo (full scan o *point
//! lookup* por rowid), joins nested-loop, agregados con/sin `GROUP BY` (+ `HAVING`)
//! y evaluación de expresiones con lógica trivalente.
//!
//! Filosofía de tipos (human-first): comparar tipos distintos es un error,
//! no una coerción silenciosa. Única promoción: INTEGER ↔ REAL.

use std::cmp::Ordering;

use crate::catalog::{ColType, ColumnDef, ColumnSpec, IndexDef, TableDef, TableSpec};
use crate::error::{Error, Result};
use crate::record::Value;
use crate::sql::ast::{
    AggFunc, BinOp, ColumnAst, Expr, JoinKind, SelectItem, SelectStmt, Stmt, TableRef, UnOp,
};
use crate::tx::{Snapshot, WriteTx};

pub struct SelectOut {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

/// Fuente de datos de un SELECT: un snapshot inmutable o una transacción en
/// curso (que ve sus propias escrituras, docs/03-api.md).
pub trait DataSource {
    fn table(&self, name: &str) -> Result<Option<TableDef>>;
    fn get_row(&self, table: &TableDef, rowid: i64) -> Result<Option<Vec<Value>>>;
    /// Filas completas de la tabla, en orden de rowid.
    fn scan_rows(&self, table: &TableDef) -> Result<Vec<Vec<Value>>>;
    /// rowids cuyas columnas indexadas valen `values` (igualdad, un valor por
    /// columna del índice), vía un índice.
    fn index_lookup(&self, table: &TableDef, idx: &IndexDef, values: &[Value]) -> Result<Vec<i64>>;
    /// rowids de un rango (`lo`/`hi` opcionales, inclusivos o no), vía un índice.
    fn index_range(
        &self,
        table: &TableDef,
        idx: &IndexDef,
        lo: Option<(&Value, bool)>,
        hi: Option<(&Value, bool)>,
    ) -> Result<Vec<i64>>;
}

impl DataSource for Snapshot {
    fn table(&self, name: &str) -> Result<Option<TableDef>> {
        Snapshot::table(self, name)
    }

    fn get_row(&self, table: &TableDef, rowid: i64) -> Result<Option<Vec<Value>>> {
        Snapshot::get_row(self, table, rowid)
    }

    fn scan_rows(&self, table: &TableDef) -> Result<Vec<Vec<Value>>> {
        self.scan_table(table)?
            .map(|r| r.map(|(_, values)| values))
            .collect()
    }

    fn index_lookup(
        &self,
        _table: &TableDef,
        idx: &IndexDef,
        values: &[Value],
    ) -> Result<Vec<i64>> {
        Snapshot::index_lookup(self, idx, values)
    }

    fn index_range(
        &self,
        _table: &TableDef,
        idx: &IndexDef,
        lo: Option<(&Value, bool)>,
        hi: Option<(&Value, bool)>,
    ) -> Result<Vec<i64>> {
        Snapshot::index_range(self, idx, lo, hi)
    }
}

impl DataSource for WriteTx {
    fn table(&self, name: &str) -> Result<Option<TableDef>> {
        WriteTx::table(self, name)
    }

    fn get_row(&self, table: &TableDef, rowid: i64) -> Result<Option<Vec<Value>>> {
        WriteTx::get_row(self, table, rowid)
    }

    fn scan_rows(&self, table: &TableDef) -> Result<Vec<Vec<Value>>> {
        self.scan_table(table)?
            .map(|r| r.map(|(_, values)| values))
            .collect()
    }

    fn index_lookup(
        &self,
        _table: &TableDef,
        idx: &IndexDef,
        values: &[Value],
    ) -> Result<Vec<i64>> {
        WriteTx::index_lookup(self, idx, values)
    }

    fn index_range(
        &self,
        _table: &TableDef,
        idx: &IndexDef,
        lo: Option<(&Value, bool)>,
        hi: Option<(&Value, bool)>,
    ) -> Result<Vec<i64>> {
        WriteTx::index_range(self, idx, lo, hi)
    }
}

fn sql_err(msg: impl Into<String>) -> Error {
    Error::Sql {
        msg: msg.into(),
        pos: None,
    }
}

// --- esquema de la sentencia ---

/// Tablas visibles en una sentencia (FROM + JOINs) con su calificador (alias
/// o nombre). La fila combinada es la concatenación de las filas de cada
/// tabla en orden de aparición.
struct QuerySchema {
    tables: Vec<SchemaTable>,
}

struct SchemaTable {
    qualifier: String,
    def: TableDef,
    offset: usize,
}

impl QuerySchema {
    fn new() -> QuerySchema {
        QuerySchema { tables: Vec::new() }
    }

    fn single(qualifier: &str, def: TableDef) -> QuerySchema {
        let mut s = QuerySchema::new();
        s.push(qualifier, def)
            .expect("la primera tabla no puede colisionar");
        s
    }

    fn push(&mut self, qualifier: &str, def: TableDef) -> Result<()> {
        if self.tables.iter().any(|t| t.qualifier == qualifier) {
            return Err(sql_err(format!("alias de tabla duplicado: {qualifier}")));
        }
        let offset = self.width();
        self.tables.push(SchemaTable {
            qualifier: qualifier.to_owned(),
            def,
            offset,
        });
        Ok(())
    }

    /// Anchura de la fila combinada.
    fn width(&self) -> usize {
        self.tables
            .last()
            .map_or(0, |t| t.offset + t.def.columns.len())
    }

    /// Índice de `[tabla.]columna` en la fila combinada. Sin calificador, la
    /// columna debe ser única entre todas las tablas.
    fn resolve(&self, table: Option<&str>, name: &str) -> Result<usize> {
        match table {
            Some(q) => {
                let t = self
                    .tables
                    .iter()
                    .find(|t| t.qualifier == q)
                    .ok_or_else(|| sql_err(format!("tabla desconocida en la consulta: {q}")))?;
                let i = t
                    .def
                    .columns
                    .iter()
                    .position(|c| c.name == name)
                    .ok_or_else(|| sql_err(format!("columna desconocida: {q}.{name}")))?;
                Ok(t.offset + i)
            }
            None => {
                let mut found = None;
                for t in &self.tables {
                    if let Some(i) = t.def.columns.iter().position(|c| c.name == name) {
                        if found.is_some() {
                            return Err(sql_err(format!("columna ambigua: {name}")));
                        }
                        found = Some(t.offset + i);
                    }
                }
                found.ok_or_else(|| sql_err(format!("columna desconocida: {name}")))
            }
        }
    }
}

/// Valida que toda columna referenciada exista, aunque no haya filas que
/// evaluar: los errores de la sentencia no dependen de los datos.
fn validate_columns(e: &Expr, schema: &QuerySchema) -> Result<()> {
    match e {
        Expr::Column { table, name } => schema.resolve(table.as_deref(), name).map(drop),
        Expr::Literal(_) | Expr::Param(_) => Ok(()),
        Expr::Unary(_, inner) => validate_columns(inner, schema),
        Expr::Binary(a, _, b) => {
            validate_columns(a, schema)?;
            validate_columns(b, schema)
        }
        Expr::IsNull { expr, .. } => validate_columns(expr, schema),
        Expr::Like { expr, pattern, .. } => {
            validate_columns(expr, schema)?;
            validate_columns(pattern, schema)
        }
        Expr::Aggregate { arg, .. } => arg
            .as_deref()
            .map_or(Ok(()), |e| validate_columns(e, schema)),
    }
}

fn no_aggregates(e: &Expr, clause: &str) -> Result<()> {
    if e.has_aggregate() {
        return Err(sql_err(format!("los agregados no se permiten en {clause}")));
    }
    Ok(())
}

// --- sentencias de escritura (autocommit lo gestiona la API) ---

pub fn run_execute(tx: &mut WriteTx, stmt: &Stmt, params: &[Value]) -> Result<usize> {
    match stmt {
        Stmt::CreateTable {
            if_not_exists,
            name,
            columns,
        } => {
            if tx.table(name)?.is_some() {
                if *if_not_exists {
                    return Ok(0);
                }
                return Err(Error::Constraint("la tabla ya existe"));
            }
            tx.create_table(&table_spec(name, columns)?)?;
            Ok(0)
        }
        Stmt::DropTable { if_exists, name } => {
            let dropped = tx.drop_table(name)?;
            if !dropped && !if_exists {
                return Err(sql_err(format!("tabla desconocida: {name}")));
            }
            Ok(0)
        }
        Stmt::CreateIndex {
            if_not_exists,
            unique,
            name,
            table,
            columns,
        } => {
            if *if_not_exists && tx.index_exists(name)? {
                return Ok(0);
            }
            let def = tx
                .table(table)?
                .ok_or_else(|| sql_err(format!("tabla desconocida: {table}")))?;
            let mut positions = Vec::with_capacity(columns.len());
            for cname in columns {
                let pos = def
                    .columns
                    .iter()
                    .position(|c| &c.name == cname)
                    .ok_or_else(|| sql_err(format!("columna desconocida: {cname}")))?;
                positions.push(pos);
            }
            tx.create_index(table, name, &positions, *unique)?;
            Ok(0)
        }
        Stmt::DropIndex { if_exists, name } => {
            let dropped = tx.drop_index(name)?;
            if !dropped && !if_exists {
                return Err(sql_err(format!("índice desconocido: {name}")));
            }
            Ok(0)
        }
        Stmt::Insert {
            table,
            columns,
            rows,
        } => {
            let def = tx
                .table(table)?
                .ok_or_else(|| sql_err(format!("tabla desconocida: {table}")))?;
            for row in rows {
                let values = insert_values(&def, columns.as_deref(), row, params)?;
                tx.insert_row(&def, &values)?;
            }
            Ok(rows.len())
        }
        Stmt::AlterTableAddColumn { table, column } => {
            if column.primary_key {
                return Err(sql_err(
                    "ALTER TABLE ADD: no se puede añadir una PRIMARY KEY",
                ));
            }
            if tx.table(table)?.is_none() {
                return Err(sql_err(format!("tabla desconocida: {table}")));
            }
            let default = match &column.default {
                None => None,
                Some(e) => {
                    if e.contains_param() {
                        return Err(sql_err("DEFAULT no admite parámetros"));
                    }
                    match eval_const(e, params)? {
                        Value::Null => None,
                        v => Some(v),
                    }
                }
            };
            tx.add_column(
                table,
                ColumnDef {
                    name: column.name.clone(),
                    col_type: column.col_type,
                    not_null: column.not_null,
                    default,
                },
            )?;
            Ok(0)
        }
        Stmt::Update {
            table,
            sets,
            where_clause,
        } => run_update(tx, table, sets, where_clause.as_ref(), params),
        Stmt::Delete {
            table,
            where_clause,
        } => run_delete(tx, table, where_clause.as_ref(), params),
        // La conexión intercepta estas tres antes de llegar aquí (api.rs).
        Stmt::Begin | Stmt::Commit | Stmt::Rollback => Err(sql_err(
            "BEGIN/COMMIT/ROLLBACK los gestiona la conexión, no el ejecutor",
        )),
        Stmt::Select(_) => Err(sql_err("SELECT devuelve filas: usa query, no execute")),
    }
}

/// Filas candidatas a un UPDATE/DELETE. Si el `WHERE` es exactamente
/// `alias_rowid = const`, va por el **point lookup** (`get_row`, O(log n)) — la
/// misma optimización que `run_select`; si no, full scan. Sin esto, tocar una
/// sola fila por PK recorría toda la tabla (O(filas)).
fn candidate_rows(
    tx: &WriteTx,
    def: &TableDef,
    schema: &QuerySchema,
    where_clause: Option<&Expr>,
    params: &[Value],
) -> Result<Vec<(i64, Vec<Value>)>> {
    if let Some(key_expr) = point_lookup_key(schema, where_clause)
        && let Value::Integer(rowid) = eval_const(key_expr, params)?
    {
        return Ok(tx
            .get_row(def, rowid)?
            .map(|row| (rowid, row))
            .into_iter()
            .collect());
    }
    // Index scan de igualdad (índice single o multi-columna); NULL/tipo
    // incompatible en cualquier columna → cae al full scan.
    if let Some((idx, const_exprs)) = index_eq_plan(schema, where_clause) {
        let mut values = Vec::with_capacity(const_exprs.len());
        let mut usable = true;
        for (&col, ex) in idx.columns.iter().zip(&const_exprs) {
            let v = eval_const(ex, params)?;
            if matches!(v, Value::Null) {
                usable = false;
                break;
            }
            match coerce_value(v, def.columns[col].col_type) {
                Some(cv) => values.push(cv),
                None => {
                    usable = false;
                    break;
                }
            }
        }
        if usable {
            let mut out = Vec::new();
            for rowid in tx.index_lookup(idx, &values)? {
                if let Some(row) = tx.get_row(def, rowid)? {
                    out.push((rowid, row));
                }
            }
            return Ok(out);
        }
    }
    // Index scan de rango (col indexada <op> const).
    if let Some((idx, op, const_expr)) = index_range_plan(schema, where_clause) {
        let value = eval_const(const_expr, params)?;
        let col = idx.columns[0];
        if !matches!(value, Value::Null)
            && let Some(v) = coerce_value(value, def.columns[col].col_type)
        {
            let (lo, hi) = match op {
                BinOp::Gt => (Some((&v, false)), None),
                BinOp::Ge => (Some((&v, true)), None),
                BinOp::Lt => (None, Some((&v, false))),
                BinOp::Le => (None, Some((&v, true))),
                _ => (None, None),
            };
            let mut out = Vec::new();
            for rowid in tx.index_range(idx, lo, hi)? {
                if let Some(row) = tx.get_row(def, rowid)? {
                    out.push((rowid, row));
                }
            }
            return Ok(out);
        }
    }
    let mut all = Vec::new();
    for item in tx.scan_table(def)? {
        all.push(item?);
    }
    Ok(all)
}

fn run_update(
    tx: &mut WriteTx,
    table: &str,
    sets: &[(String, Expr)],
    where_clause: Option<&Expr>,
    params: &[Value],
) -> Result<usize> {
    let def = tx
        .table(table)?
        .ok_or_else(|| sql_err(format!("tabla desconocida: {table}")))?;
    let schema = QuerySchema::single(table, def.clone());

    // Resolver SET y validar el WHERE antes de tocar filas. Reasignar el
    // alias del rowid sería mover la fila de clave: fuera de v1.
    let mut set_idx = Vec::with_capacity(sets.len());
    for (name, expr) in sets {
        let i = col_index(&def, name)?;
        if def.rowid_alias == Some(i) {
            return Err(sql_err(format!(
                "no se puede actualizar la PRIMARY KEY: {name}"
            )));
        }
        no_aggregates(expr, "SET")?;
        validate_columns(expr, &schema)?;
        set_idx.push(i);
    }
    if let Some(cond) = where_clause {
        no_aggregates(cond, "WHERE")?;
        validate_columns(cond, &schema)?;
    }

    // Materializar primero (point lookup por PK o full scan): el scan toma
    // prestada la tx, que luego necesitamos en exclusiva para escribir.
    let mut updates: Vec<(i64, Vec<Value>)> = Vec::new();
    for (rowid, row) in candidate_rows(tx, &def, &schema, where_clause, params)? {
        if let Some(cond) = where_clause
            && !truthy(eval(cond, Some((&schema, &row)), params)?)?
        {
            continue;
        }
        // Todas las expresiones de SET ven la fila antigua.
        let mut new_row = row.clone();
        for (&i, (_, expr)) in set_idx.iter().zip(sets) {
            new_row[i] = eval(expr, Some((&schema, &row)), params)?;
        }
        updates.push((rowid, new_row));
    }
    let n = updates.len();
    for (rowid, values) in updates {
        tx.update_row(&def, rowid, &values)?;
    }
    Ok(n)
}

fn run_delete(
    tx: &mut WriteTx,
    table: &str,
    where_clause: Option<&Expr>,
    params: &[Value],
) -> Result<usize> {
    let def = tx
        .table(table)?
        .ok_or_else(|| sql_err(format!("tabla desconocida: {table}")))?;
    let schema = QuerySchema::single(table, def.clone());
    if let Some(cond) = where_clause {
        no_aggregates(cond, "WHERE")?;
        validate_columns(cond, &schema)?;
    }
    let mut doomed = Vec::new();
    for (rowid, row) in candidate_rows(tx, &def, &schema, where_clause, params)? {
        if let Some(cond) = where_clause
            && !truthy(eval(cond, Some((&schema, &row)), params)?)?
        {
            continue;
        }
        doomed.push(rowid);
    }
    for rowid in &doomed {
        tx.delete_row(&def, *rowid)?;
    }
    Ok(doomed.len())
}

fn table_spec(name: &str, columns: &[ColumnAst]) -> Result<TableSpec> {
    let mut specs = Vec::with_capacity(columns.len());
    for col in columns {
        let default = match &col.default {
            None => None,
            Some(e) => {
                if e.contains_param() {
                    return Err(sql_err("DEFAULT no admite parámetros"));
                }
                match eval_const(e, &[])? {
                    Value::Null => None,
                    v => Some(v),
                }
            }
        };
        specs.push(ColumnSpec {
            name: col.name.clone(),
            col_type: col.col_type,
            not_null: col.not_null,
            primary_key: col.primary_key,
            default,
        });
    }
    Ok(TableSpec {
        name: name.to_owned(),
        columns: specs,
    })
}

fn insert_values(
    def: &TableDef,
    columns: Option<&[String]>,
    exprs: &[Expr],
    params: &[Value],
) -> Result<Vec<Value>> {
    let values: Vec<Value> = exprs
        .iter()
        .map(|e| eval_const(e, params))
        .collect::<Result<_>>()?;
    match columns {
        // Posicional: las columnas finales ausentes toman DEFAULT en catalog.
        None => {
            if values.len() > def.columns.len() {
                return Err(sql_err("más valores que columnas"));
            }
            Ok(values)
        }
        Some(names) => {
            if names.len() != values.len() {
                return Err(sql_err("número distinto de columnas y de valores"));
            }
            // Las no nombradas toman su DEFAULT (o NULL); el alias del rowid
            // queda NULL ⇒ rowid automático.
            let mut out: Vec<Value> = def
                .columns
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    if def.rowid_alias == Some(i) {
                        Value::Null
                    } else {
                        c.default.clone().unwrap_or(Value::Null)
                    }
                })
                .collect();
            for (name, value) in names.iter().zip(values) {
                let i = col_index(def, name)?;
                if names.iter().filter(|n| *n == name).count() > 1 {
                    return Err(sql_err(format!("columna repetida en INSERT: {name}")));
                }
                out[i] = value;
            }
            Ok(out)
        }
    }
}

fn col_index(def: &TableDef, name: &str) -> Result<usize> {
    def.columns
        .iter()
        .position(|c| c.name == name)
        .ok_or_else(|| sql_err(format!("columna desconocida: {name}")))
}

// --- SELECT ---

pub fn run_select(src: &impl DataSource, stmt: &SelectStmt, params: &[Value]) -> Result<SelectOut> {
    let from_def = lookup(src, &stmt.from)?;
    let mut schema = QuerySchema::single(stmt.from.qualifier(), from_def.clone());

    // Planificador (solo sin joins): point lookup por PK (`alias_rowid = const`),
    // si no, index scan por un índice secundario (`col_indexada = const`), si no,
    // full scan. El filtro WHERE general se aplica después en todos los casos.
    let mut rows: Vec<Vec<Value>> = if stmt.joins.is_empty() {
        if let Some(key_expr) = point_lookup_key(&schema, stmt.where_clause.as_ref()) {
            match eval_const(key_expr, params)? {
                Value::Integer(rowid) => src.get_row(&from_def, rowid)?.into_iter().collect(),
                // Tipo no entero: que el filtro general emita su error de tipos.
                _ => src.scan_rows(&from_def)?,
            }
        } else if let Some((idx, const_exprs)) = index_eq_plan(&schema, stmt.where_clause.as_ref())
        {
            index_eq_rows(src, &from_def, idx, &const_exprs, params)?
        } else if let Some((idx, op, const_expr)) =
            index_range_plan(&schema, stmt.where_clause.as_ref())
        {
            index_range_rows(src, &from_def, idx, op, const_expr, params)?
        } else {
            src.scan_rows(&from_def)?
        }
    } else {
        src.scan_rows(&from_def)?
    };

    // Joins nested-loop, materializando de izquierda a derecha.
    for join in &stmt.joins {
        let right_def = lookup(src, &join.table)?;
        let right_rows = src.scan_rows(&right_def)?;
        schema.push(join.table.qualifier(), right_def)?;
        no_aggregates(&join.on, "ON")?;
        validate_columns(&join.on, &schema)?;
        let width = schema.width();
        let mut joined = Vec::new();
        for left in &rows {
            let mut matched = false;
            for right in &right_rows {
                let mut combined = left.clone();
                combined.extend(right.iter().cloned());
                if truthy(eval(&join.on, Some((&schema, &combined)), params)?)? {
                    joined.push(combined);
                    matched = true;
                }
            }
            // LEFT JOIN conserva la fila izquierda sin pareja, con NULLs.
            if !matched && join.kind == JoinKind::Left {
                let mut combined = left.clone();
                combined.resize(width, Value::Null);
                joined.push(combined);
            }
        }
        rows = joined;
    }

    if let Some(cond) = &stmt.where_clause {
        no_aggregates(cond, "WHERE")?;
        validate_columns(cond, &schema)?;
        let mut kept = Vec::with_capacity(rows.len());
        for row in rows {
            if truthy(eval(cond, Some((&schema, &row)), params)?)? {
                kept.push(row);
            }
        }
        rows = kept;
    }

    // GROUP BY: una fila por grupo (HAVING filtra grupos, ORDER BY ordena la
    // salida). Camino propio.
    if !stmt.group_by.is_empty() {
        return run_grouped(stmt, &schema, rows, params);
    }

    // Agregados sin GROUP BY: el resultado es una única fila (un grupo implícito);
    // ORDER BY es un no-op sobre ella. HAVING, si está, la filtra.
    let has_agg = stmt
        .projection
        .iter()
        .any(|i| matches!(i, SelectItem::Expr(e) if e.has_aggregate()))
        || stmt.having.as_ref().is_some_and(|h| h.has_aggregate());
    if has_agg {
        let (columns, row) = aggregate_projection(&stmt.projection, &schema, &rows, params)?;
        if let Some(h) = &stmt.having {
            validate_columns(h, &schema)?;
            let folded = fold_aggregates(h, &schema, &rows, params)?;
            if !truthy(eval_const(&folded, params)?)? {
                return Ok(SelectOut {
                    columns,
                    rows: vec![],
                });
            }
        }
        return Ok(SelectOut {
            columns,
            rows: limit_offset(vec![row], stmt, params)?,
        });
    }
    if stmt.having.is_some() {
        return Err(sql_err("HAVING requiere GROUP BY o agregados"));
    }

    if !stmt.order_by.is_empty() {
        let order: Vec<(usize, bool)> = stmt
            .order_by
            .iter()
            .map(|o| Ok((schema.resolve(o.table.as_deref(), &o.column)?, o.desc)))
            .collect::<Result<_>>()?;
        let mut first_err: Option<Error> = None;
        rows.sort_by(|a, b| {
            for (idx, desc) in &order {
                match cmp_nulls_first(&a[*idx], &b[*idx]) {
                    Ok(Ordering::Equal) => continue,
                    Ok(o) => return if *desc { o.reverse() } else { o },
                    Err(e) => {
                        first_err.get_or_insert(e);
                        return Ordering::Equal;
                    }
                }
            }
            Ordering::Equal
        });
        if let Some(e) = first_err {
            return Err(e);
        }
    }

    let rows = limit_offset(rows, stmt, params)?;

    // Proyección.
    let mut columns = Vec::new();
    let mut projections: Vec<Option<&Expr>> = Vec::new(); // None = toda la fila (Star)
    for (i, item) in stmt.projection.iter().enumerate() {
        match item {
            SelectItem::Star => {
                columns.extend(
                    schema
                        .tables
                        .iter()
                        .flat_map(|t| t.def.columns.iter().map(|c| c.name.clone())),
                );
                projections.push(None);
            }
            SelectItem::Expr(e) => {
                validate_columns(e, &schema)?;
                columns.push(match e {
                    Expr::Column { name, .. } => name.clone(),
                    _ => format!("col{}", i + 1),
                });
                projections.push(Some(e));
            }
        }
    }
    let mut out_rows = Vec::with_capacity(rows.len());
    for row in &rows {
        let mut out = Vec::with_capacity(columns.len());
        for proj in &projections {
            match proj {
                None => out.extend(row.iter().cloned()),
                Some(e) => out.push(eval(e, Some((&schema, row)), params)?),
            }
        }
        out_rows.push(out);
    }
    Ok(SelectOut {
        columns,
        rows: out_rows,
    })
}

fn lookup(src: &impl DataSource, table: &TableRef) -> Result<TableDef> {
    src.table(&table.name)?
        .ok_or_else(|| sql_err(format!("tabla desconocida: {}", table.name)))
}

fn limit_offset(
    rows: Vec<Vec<Value>>,
    stmt: &SelectStmt,
    params: &[Value],
) -> Result<Vec<Vec<Value>>> {
    let offset = match &stmt.offset {
        Some(e) => usize_const(e, params, "OFFSET")?,
        None => 0,
    };
    let limit = match &stmt.limit {
        Some(e) => usize_const(e, params, "LIMIT")?,
        None => usize::MAX,
    };
    Ok(rows.into_iter().skip(offset).take(limit).collect())
}

/// `WHERE alias_rowid = <const>` (en cualquier orden) ⇒ acceso directo.
fn point_lookup_key<'e>(schema: &QuerySchema, where_clause: Option<&'e Expr>) -> Option<&'e Expr> {
    let t = &schema.tables[0];
    let alias = t.def.rowid_alias?;
    let alias_name = &t.def.columns[alias].name;
    let Some(Expr::Binary(left, BinOp::Eq, right)) = where_clause else {
        return None;
    };
    let is_alias = |e: &Expr| match e {
        Expr::Column { table, name } => {
            name == alias_name && table.as_deref().is_none_or(|q| q == t.qualifier)
        }
        _ => false,
    };
    match (left.as_ref(), right.as_ref()) {
        (c, e) if is_alias(c) && e.is_const() => Some(e),
        (e, c) if is_alias(c) && e.is_const() => Some(e),
        _ => None,
    }
}

/// Recoge las igualdades `col = const` de un `WHERE` que es una conjunción de
/// ellas (`a = X AND b = Y AND …`), como `(posición de columna, const)`. Solo
/// recorre `AND` y `=`; cualquier otra cosa (OR, etc.) no aporta igualdades.
fn collect_equalities<'e>(e: &'e Expr, t: &SchemaTable, out: &mut Vec<(usize, &'e Expr)>) {
    match e {
        Expr::Binary(l, BinOp::And, r) => {
            collect_equalities(l, t, out);
            collect_equalities(r, t, out);
        }
        Expr::Binary(l, BinOp::Eq, r) => {
            let col_pos = |ex: &Expr| -> Option<usize> {
                match ex {
                    Expr::Column { table, name }
                        if table.as_deref().is_none_or(|q| q == t.qualifier) =>
                    {
                        t.def.columns.iter().position(|c| &c.name == name)
                    }
                    _ => None,
                }
            };
            if let Some(c) = col_pos(l)
                && r.is_const()
            {
                out.push((c, r.as_ref()));
            } else if let Some(c) = col_pos(r)
                && l.is_const()
            {
                out.push((c, l.as_ref()));
            }
        }
        _ => {}
    }
}

/// Mejor índice de igualdad para el `WHERE`: el índice (single o **multi-columna**)
/// con **todas** sus columnas cubiertas por una igualdad de la conjunción; entre
/// los candidatos, el de más columnas (más selectivo). Devuelve el índice y las
/// constantes en el orden de sus columnas. La PK no es índice secundario, así que
/// `id = const` lo sigue cubriendo el point lookup.
fn index_eq_plan<'a, 'e>(
    schema: &'a QuerySchema,
    where_clause: Option<&'e Expr>,
) -> Option<(&'a IndexDef, Vec<&'e Expr>)> {
    let t = &schema.tables[0];
    let mut eqs = Vec::new();
    collect_equalities(where_clause?, t, &mut eqs);
    if eqs.is_empty() {
        return None;
    }
    let mut best: Option<(&IndexDef, Vec<&Expr>)> = None;
    for idx in &t.def.indexes {
        let mut exprs = Vec::with_capacity(idx.columns.len());
        let covered = idx
            .columns
            .iter()
            .all(|&c| match eqs.iter().find(|(col, _)| *col == c) {
                Some((_, e)) => {
                    exprs.push(*e);
                    true
                }
                None => false,
            });
        if covered
            && best
                .as_ref()
                .is_none_or(|(b, _)| idx.columns.len() > b.columns.len())
        {
            best = Some((idx, exprs));
        }
    }
    best
}

/// Resuelve un index scan de igualdad (1+ columnas) a filas: evalúa y coacciona
/// cada constante a su columna, consulta el índice y trae cada fila por rowid.
/// NULL o tipo incompatible en cualquier columna ⇒ full scan (que decida el
/// filtro WHERE general).
fn index_eq_rows(
    src: &impl DataSource,
    from_def: &TableDef,
    idx: &IndexDef,
    const_exprs: &[&Expr],
    params: &[Value],
) -> Result<Vec<Vec<Value>>> {
    let mut values = Vec::with_capacity(const_exprs.len());
    for (&col, ex) in idx.columns.iter().zip(const_exprs) {
        let v = eval_const(ex, params)?;
        if matches!(v, Value::Null) {
            return src.scan_rows(from_def);
        }
        match coerce_value(v, from_def.columns[col].col_type) {
            Some(cv) => values.push(cv),
            None => return src.scan_rows(from_def),
        }
    }
    let rowids = src.index_lookup(from_def, idx, &values)?;
    let mut out = Vec::with_capacity(rowids.len());
    for rowid in rowids {
        if let Some(row) = src.get_row(from_def, rowid)? {
            out.push(row);
        }
    }
    Ok(out)
}

/// `WHERE col <op> const` (`op` ∈ `< <= > >=`, en cualquier orden) con `col`
/// cubierta por un índice de una sola columna (no la PK). Devuelve el índice, el
/// operador (normalizado a `col <op> const`) y la constante.
fn index_range_plan<'a, 'e>(
    schema: &'a QuerySchema,
    where_clause: Option<&'e Expr>,
) -> Option<(&'a IndexDef, BinOp, &'e Expr)> {
    let t = &schema.tables[0];
    let Some(Expr::Binary(left, op, right)) = where_clause else {
        return None;
    };
    if !matches!(op, BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge) {
        return None;
    }
    let col_pos = |e: &Expr| -> Option<usize> {
        match e {
            Expr::Column { table, name } if table.as_deref().is_none_or(|q| q == t.qualifier) => {
                t.def.columns.iter().position(|c| &c.name == name)
            }
            _ => None,
        }
    };
    let (col, const_expr, op) = if let Some(c) = col_pos(left)
        && right.is_const()
    {
        (c, right.as_ref(), *op)
    } else if let Some(c) = col_pos(right)
        && left.is_const()
    {
        (c, left.as_ref(), flip_op(*op)) // const <op> col  ⇒  col <flip(op)> const
    } else {
        return None;
    };
    if Some(col) == t.def.rowid_alias {
        return None; // rango sobre la PK: no hay índice secundario (full scan)
    }
    let idx = t
        .def
        .indexes
        .iter()
        .find(|i| i.columns.as_slice() == [col])?;
    Some((idx, op, const_expr))
}

/// Voltea un operador de comparación al pasar el operando de lado.
fn flip_op(op: BinOp) -> BinOp {
    match op {
        BinOp::Lt => BinOp::Gt,
        BinOp::Le => BinOp::Ge,
        BinOp::Gt => BinOp::Lt,
        BinOp::Ge => BinOp::Le,
        other => other,
    }
}

/// Resuelve un index scan de rango a filas: evalúa la constante, la coacciona al
/// tipo de la columna, consulta el rango del índice y trae cada fila por rowid.
fn index_range_rows(
    src: &impl DataSource,
    from_def: &TableDef,
    idx: &IndexDef,
    op: BinOp,
    const_expr: &Expr,
    params: &[Value],
) -> Result<Vec<Vec<Value>>> {
    let value = eval_const(const_expr, params)?;
    let col = idx.columns[0];
    if matches!(value, Value::Null) {
        return src.scan_rows(from_def); // rango contra NULL → 0 filas (filtro general)
    }
    let Some(v) = coerce_value(value, from_def.columns[col].col_type) else {
        return src.scan_rows(from_def); // tipo incompatible: que decida el filtro
    };
    let (lo, hi) = match op {
        BinOp::Gt => (Some((&v, false)), None),
        BinOp::Ge => (Some((&v, true)), None),
        BinOp::Lt => (None, Some((&v, false))),
        BinOp::Le => (None, Some((&v, true))),
        _ => return src.scan_rows(from_def),
    };
    let rowids = src.index_range(from_def, idx, lo, hi)?;
    let mut out = Vec::with_capacity(rowids.len());
    for rowid in rowids {
        if let Some(row) = src.get_row(from_def, rowid)? {
            out.push(row);
        }
    }
    Ok(out)
}

/// Coacciona `value` al tipo de columna igual que la validación de filas, para
/// que su codificación de índice coincida con la almacenada. `None` si el tipo
/// es incompatible (ninguna fila casaría).
fn coerce_value(value: Value, col_type: ColType) -> Option<Value> {
    match (col_type, value) {
        (_, Value::Null) => Some(Value::Null),
        (ColType::Integer, v @ Value::Integer(_)) => Some(v),
        (ColType::Real, Value::Integer(n)) => Some(Value::Real(n as f64)),
        (ColType::Real, v @ Value::Real(_)) => Some(v),
        (ColType::Text, v @ Value::Text(_)) => Some(v),
        (ColType::Blob, v @ Value::Blob(_)) => Some(v),
        (ColType::Boolean, v @ Value::Bool(_)) => Some(v),
        _ => None,
    }
}

fn usize_const(e: &Expr, params: &[Value], what: &str) -> Result<usize> {
    match eval_const(e, params)? {
        Value::Integer(n) if n >= 0 => Ok(n as usize),
        _ => Err(sql_err(format!("{what} debe ser un entero no negativo"))),
    }
}

// --- agregados sin GROUP BY ---

/// Proyección agregada: cada elemento debe ser una expresión cuyas columnas
/// estén dentro de un agregado. Devuelve los nombres y la única fila.
fn aggregate_projection(
    projection: &[SelectItem],
    schema: &QuerySchema,
    rows: &[Vec<Value>],
    params: &[Value],
) -> Result<(Vec<String>, Vec<Value>)> {
    let mut columns = Vec::with_capacity(projection.len());
    let mut out = Vec::with_capacity(projection.len());
    for (i, item) in projection.iter().enumerate() {
        let SelectItem::Expr(e) = item else {
            return Err(sql_err("'*' no se puede combinar con agregados"));
        };
        if let Some(name) = col_outside_agg(e) {
            return Err(sql_err(format!(
                "sin GROUP BY, la columna {name} debe ir dentro de un agregado"
            )));
        }
        validate_columns(e, schema)?;
        columns.push(format!("col{}", i + 1));
        let folded = fold_aggregates(e, schema, rows, params)?;
        out.push(eval_const(&folded, params)?);
    }
    Ok((columns, out))
}

/// Primera columna que aparece fuera de un agregado, si la hay.
fn col_outside_agg(e: &Expr) -> Option<&str> {
    match e {
        Expr::Column { name, .. } => Some(name),
        Expr::Literal(_) | Expr::Param(_) | Expr::Aggregate { .. } => None,
        Expr::Unary(_, inner) => col_outside_agg(inner),
        Expr::Binary(a, _, b) => col_outside_agg(a).or_else(|| col_outside_agg(b)),
        Expr::IsNull { expr, .. } => col_outside_agg(expr),
        Expr::Like { expr, pattern, .. } => {
            col_outside_agg(expr).or_else(|| col_outside_agg(pattern))
        }
    }
}

// --- GROUP BY (post-M9) ---

/// Identidad de una columna referenciada: `(tabla opcional, nombre)`.
type ColRef = (Option<String>, String);

/// Recolecta las columnas de `e`, `skip_agg=true` ignora las que estén dentro de
/// un agregado (las que deben estar en GROUP BY son justo esas: las de fuera).
fn collect_columns(e: &Expr, skip_agg: bool, out: &mut Vec<ColRef>) {
    match e {
        Expr::Column { table, name } => out.push((table.clone(), name.clone())),
        Expr::Literal(_) | Expr::Param(_) => {}
        Expr::Unary(_, x) => collect_columns(x, skip_agg, out),
        Expr::IsNull { expr, .. } => collect_columns(expr, skip_agg, out),
        Expr::Binary(a, _, b) => {
            collect_columns(a, skip_agg, out);
            collect_columns(b, skip_agg, out);
        }
        Expr::Like { expr, pattern, .. } => {
            collect_columns(expr, skip_agg, out);
            collect_columns(pattern, skip_agg, out);
        }
        Expr::Aggregate { arg, .. } => {
            if !skip_agg && let Some(a) = arg {
                collect_columns(a, skip_agg, out);
            }
        }
    }
}

/// `GROUP BY`: agrupa por el valor de las expresiones del GROUP BY y emite una
/// fila por grupo, plegando los agregados de la proyección sobre cada grupo. Una
/// columna fuera de un agregado debe aparecer en el GROUP BY (SQL estándar);
/// `HAVING` filtra grupos; `ORDER BY`/`LIMIT` actúan sobre la salida.
fn run_grouped(
    stmt: &SelectStmt,
    schema: &QuerySchema,
    rows: Vec<Vec<Value>>,
    params: &[Value],
) -> Result<SelectOut> {
    // Columnas del GROUP BY (para validar que la proyección no use otras sueltas).
    let mut group_cols: Vec<ColRef> = Vec::new();
    for e in &stmt.group_by {
        validate_columns(e, schema)?;
        collect_columns(e, false, &mut group_cols);
    }
    let in_group = |c: &ColRef| {
        group_cols
            .iter()
            .any(|g| g.1 == c.1 && (g.0.is_none() || c.0.is_none() || g.0 == c.0))
    };
    let check_grouped = |e: &Expr| -> Result<()> {
        validate_columns(e, schema)?;
        let mut cols = Vec::new();
        collect_columns(e, true, &mut cols);
        for c in &cols {
            if !in_group(c) {
                return Err(sql_err(format!(
                    "la columna {} debe ir en GROUP BY o dentro de un agregado",
                    c.1
                )));
            }
        }
        Ok(())
    };
    for item in &stmt.projection {
        match item {
            SelectItem::Star => return Err(sql_err("'*' no se puede combinar con GROUP BY")),
            SelectItem::Expr(e) => check_grouped(e)?,
        }
    }
    if let Some(h) = &stmt.having {
        check_grouped(h)?;
    }

    // Particionar en grupos por el valor (serializado) de las claves de GROUP BY,
    // preservando el orden de primera aparición.
    let mut groups: Vec<Vec<Vec<Value>>> = Vec::new();
    let mut index: std::collections::HashMap<Vec<u8>, usize> = std::collections::HashMap::new();
    for row in rows {
        let key: Vec<Value> = stmt
            .group_by
            .iter()
            .map(|e| eval(e, Some((schema, &row)), params))
            .collect::<Result<_>>()?;
        let kbytes = crate::record::encode_values(&key);
        match index.get(&kbytes) {
            Some(&i) => groups[i].push(row),
            None => {
                index.insert(kbytes, groups.len());
                groups.push(vec![row]);
            }
        }
    }

    // Nombres de columnas de salida (las columnas conservan su nombre; las demás
    // expresiones, `colN`).
    let columns: Vec<String> = stmt
        .projection
        .iter()
        .enumerate()
        .map(|(i, item)| match item {
            SelectItem::Expr(Expr::Column { name, .. }) => name.clone(),
            _ => format!("col{}", i + 1),
        })
        .collect();

    // Una fila por grupo; HAVING filtra. La fila representante da el valor de las
    // columnas del grupo (constantes dentro del grupo); los agregados se pliegan.
    let mut out_rows: Vec<Vec<Value>> = Vec::new();
    for group in &groups {
        let rep = &group[0];
        if let Some(h) = &stmt.having {
            let folded = fold_aggregates(h, schema, group, params)?;
            if !truthy(eval(&folded, Some((schema, rep)), params)?)? {
                continue;
            }
        }
        let mut out = Vec::with_capacity(columns.len());
        for item in &stmt.projection {
            let SelectItem::Expr(e) = item else {
                unreachable!("'*' ya rechazado")
            };
            let folded = fold_aggregates(e, schema, group, params)?;
            out.push(eval(&folded, Some((schema, rep)), params)?);
        }
        out_rows.push(out);
    }

    // ORDER BY sobre las columnas de SALIDA, por nombre.
    if !stmt.order_by.is_empty() {
        let order: Vec<(usize, bool)> = stmt
            .order_by
            .iter()
            .map(|o| {
                let idx = columns.iter().position(|c| c == &o.column).ok_or_else(|| {
                    sql_err(format!("ORDER BY: «{}» no está en la proyección", o.column))
                })?;
                Ok((idx, o.desc))
            })
            .collect::<Result<_>>()?;
        let mut first_err: Option<Error> = None;
        out_rows.sort_by(|a, b| {
            for (idx, desc) in &order {
                match cmp_nulls_first(&a[*idx], &b[*idx]) {
                    Ok(Ordering::Equal) => continue,
                    Ok(o) => return if *desc { o.reverse() } else { o },
                    Err(e) => {
                        first_err.get_or_insert(e);
                        return Ordering::Equal;
                    }
                }
            }
            Ordering::Equal
        });
        if let Some(e) = first_err {
            return Err(e);
        }
    }

    let out_rows = limit_offset(out_rows, stmt, params)?;
    Ok(SelectOut {
        columns,
        rows: out_rows,
    })
}

/// Sustituye cada nodo `Aggregate` por su valor calculado sobre `rows`; el
/// resultado se evalúa después como expresión constante.
fn fold_aggregates(
    e: &Expr,
    schema: &QuerySchema,
    rows: &[Vec<Value>],
    params: &[Value],
) -> Result<Expr> {
    Ok(match e {
        Expr::Aggregate { func, arg } => Expr::Literal(compute_aggregate(
            *func,
            arg.as_deref(),
            schema,
            rows,
            params,
        )?),
        Expr::Literal(_) | Expr::Column { .. } | Expr::Param(_) => e.clone(),
        Expr::Unary(op, inner) => {
            Expr::Unary(*op, Box::new(fold_aggregates(inner, schema, rows, params)?))
        }
        Expr::Binary(a, op, b) => Expr::Binary(
            Box::new(fold_aggregates(a, schema, rows, params)?),
            *op,
            Box::new(fold_aggregates(b, schema, rows, params)?),
        ),
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(fold_aggregates(expr, schema, rows, params)?),
            negated: *negated,
        },
        Expr::Like {
            expr,
            pattern,
            negated,
        } => Expr::Like {
            expr: Box::new(fold_aggregates(expr, schema, rows, params)?),
            pattern: Box::new(fold_aggregates(pattern, schema, rows, params)?),
            negated: *negated,
        },
    })
}

/// Los agregados ignoran NULL (estándar SQL): `COUNT(col)` cuenta no-NULL,
/// `SUM`/`AVG`/`MIN`/`MAX` sobre cero valores devuelven NULL.
fn compute_aggregate(
    func: AggFunc,
    arg: Option<&Expr>,
    schema: &QuerySchema,
    rows: &[Vec<Value>],
    params: &[Value],
) -> Result<Value> {
    // COUNT(*): filas tal cual, sin evaluar nada.
    let Some(arg) = arg else {
        return Ok(Value::Integer(rows.len() as i64));
    };
    if arg.has_aggregate() {
        return Err(sql_err("agregados anidados"));
    }
    let mut count: i64 = 0;
    let mut sum_i: i64 = 0;
    let mut sum_f: f64 = 0.0;
    let mut real_seen = false;
    let mut best: Option<Value> = None;
    for row in rows {
        let v = eval(arg, Some((schema, row)), params)?;
        if matches!(v, Value::Null) {
            continue;
        }
        count += 1;
        match func {
            AggFunc::Count => {}
            // SUM conserva INTEGER salvo que aparezca un REAL (promoción).
            AggFunc::Sum => match v {
                Value::Integer(n) => {
                    if real_seen {
                        sum_f += n as f64;
                    } else {
                        sum_i = sum_i
                            .checked_add(n)
                            .ok_or(sql_err("desbordamiento de entero en SUM"))?;
                    }
                }
                Value::Real(f) => {
                    if !real_seen {
                        sum_f = sum_i as f64;
                        real_seen = true;
                    }
                    sum_f += f;
                }
                v => {
                    return Err(sql_err(format!(
                        "SUM requiere valores numéricos, no {}",
                        v.type_name()
                    )));
                }
            },
            // AVG acumula en f64: el promedio es REAL por definición.
            AggFunc::Avg => match v {
                Value::Integer(n) => sum_f += n as f64,
                Value::Real(f) => sum_f += f,
                v => {
                    return Err(sql_err(format!(
                        "AVG requiere valores numéricos, no {}",
                        v.type_name()
                    )));
                }
            },
            AggFunc::Min | AggFunc::Max => {
                best = Some(match best.take() {
                    None => v,
                    Some(b) => match cmp_values(&v, &b)? {
                        Some(Ordering::Less) if func == AggFunc::Min => v,
                        Some(Ordering::Greater) if func == AggFunc::Max => v,
                        _ => b,
                    },
                });
            }
        }
    }
    Ok(match func {
        AggFunc::Count => Value::Integer(count),
        AggFunc::Sum if count == 0 => Value::Null,
        AggFunc::Sum if real_seen => Value::Real(sum_f),
        AggFunc::Sum => Value::Integer(sum_i),
        AggFunc::Avg if count == 0 => Value::Null,
        AggFunc::Avg => Value::Real(sum_f / count as f64),
        AggFunc::Min | AggFunc::Max => best.unwrap_or(Value::Null),
    })
}

// --- evaluación de expresiones ---

fn eval_const(e: &Expr, params: &[Value]) -> Result<Value> {
    eval(e, None, params)
}

fn eval(e: &Expr, row: Option<(&QuerySchema, &[Value])>, params: &[Value]) -> Result<Value> {
    match e {
        Expr::Literal(v) => Ok(v.clone()),
        Expr::Param(n) => params
            .get(n - 1)
            .cloned()
            .ok_or_else(|| sql_err(format!("falta el parámetro ?{n}"))),
        Expr::Column { table, name } => {
            let (schema, values) =
                row.ok_or_else(|| sql_err(format!("aquí no se permiten columnas: {name}")))?;
            Ok(values[schema.resolve(table.as_deref(), name)?].clone())
        }
        Expr::Aggregate { .. } => Err(sql_err(
            "los agregados solo se permiten en la lista del SELECT",
        )),
        Expr::Unary(UnOp::Neg, inner) => match eval(inner, row, params)? {
            Value::Null => Ok(Value::Null),
            Value::Integer(n) => n
                .checked_neg()
                .map(Value::Integer)
                .ok_or(sql_err("desbordamiento de entero")),
            Value::Real(f) => Ok(Value::Real(-f)),
            v => Err(sql_err(format!("no se puede negar un {}", v.type_name()))),
        },
        Expr::Unary(UnOp::Not, inner) => match eval(inner, row, params)? {
            Value::Null => Ok(Value::Null),
            Value::Bool(b) => Ok(Value::Bool(!b)),
            v => Err(sql_err(format!(
                "NOT requiere BOOLEAN, no {}",
                v.type_name()
            ))),
        },
        Expr::IsNull { expr, negated } => {
            let is_null = matches!(eval(expr, row, params)?, Value::Null);
            Ok(Value::Bool(is_null != *negated))
        }
        Expr::Like {
            expr,
            pattern,
            negated,
        } => {
            let s = eval(expr, row, params)?;
            let p = eval(pattern, row, params)?;
            match (s, p) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Text(s), Value::Text(p)) => {
                    let m = like_match(
                        &p.chars().collect::<Vec<_>>(),
                        &s.chars().collect::<Vec<_>>(),
                    );
                    Ok(Value::Bool(m != *negated))
                }
                (a, b) => Err(sql_err(format!(
                    "LIKE requiere TEXT, no {} y {}",
                    a.type_name(),
                    b.type_name()
                ))),
            }
        }
        Expr::Binary(left, op, right) => {
            let l = eval(left, row, params)?;
            match op {
                // Lógica trivalente con cortocircuito.
                BinOp::And => match truthy3(l)? {
                    Some(false) => Ok(Value::Bool(false)),
                    l3 => match (l3, truthy3(eval(right, row, params)?)?) {
                        (_, Some(false)) => Ok(Value::Bool(false)),
                        (Some(true), Some(true)) => Ok(Value::Bool(true)),
                        _ => Ok(Value::Null),
                    },
                },
                BinOp::Or => match truthy3(l)? {
                    Some(true) => Ok(Value::Bool(true)),
                    l3 => match (l3, truthy3(eval(right, row, params)?)?) {
                        (_, Some(true)) => Ok(Value::Bool(true)),
                        (Some(false), Some(false)) => Ok(Value::Bool(false)),
                        _ => Ok(Value::Null),
                    },
                },
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                    let r = eval(right, row, params)?;
                    match cmp_values(&l, &r)? {
                        None => Ok(Value::Null),
                        Some(o) => Ok(Value::Bool(match op {
                            BinOp::Eq => o == Ordering::Equal,
                            BinOp::Ne => o != Ordering::Equal,
                            BinOp::Lt => o == Ordering::Less,
                            BinOp::Le => o != Ordering::Greater,
                            BinOp::Gt => o == Ordering::Greater,
                            BinOp::Ge => o != Ordering::Less,
                            _ => unreachable!("solo comparaciones"),
                        })),
                    }
                }
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
                    arith(*op, l, eval(right, row, params)?)
                }
            }
        }
    }
}

/// Lógica trivalente: `Some(bool)` o `None` (NULL = desconocido).
fn truthy3(v: Value) -> Result<Option<bool>> {
    match v {
        Value::Bool(b) => Ok(Some(b)),
        Value::Null => Ok(None),
        v => Err(sql_err(format!(
            "se esperaba BOOLEAN, no {}",
            v.type_name()
        ))),
    }
}

/// La condición de WHERE: NULL cuenta como falso.
fn truthy(v: Value) -> Result<bool> {
    Ok(truthy3(v)?.unwrap_or(false))
}

/// `None` = resultado desconocido (algún operando NULL o NaN involucrado).
fn cmp_values(a: &Value, b: &Value) -> Result<Option<Ordering>> {
    Ok(match (a, b) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Integer(x), Value::Integer(y)) => Some(x.cmp(y)),
        (Value::Integer(x), Value::Real(y)) => (*x as f64).partial_cmp(y),
        (Value::Real(x), Value::Integer(y)) => x.partial_cmp(&(*y as f64)),
        (Value::Real(x), Value::Real(y)) => x.partial_cmp(y),
        (Value::Text(x), Value::Text(y)) => Some(x.cmp(y)),
        (Value::Blob(x), Value::Blob(y)) => Some(x.cmp(y)),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        _ => {
            return Err(sql_err(format!(
                "tipos incomparables: {} y {}",
                a.type_name(),
                b.type_name()
            )));
        }
    })
}

/// Para ORDER BY: NULL primero en orden ascendente (estilo SQLite).
fn cmp_nulls_first(a: &Value, b: &Value) -> Result<Ordering> {
    match (a, b) {
        (Value::Null, Value::Null) => Ok(Ordering::Equal),
        (Value::Null, _) => Ok(Ordering::Less),
        (_, Value::Null) => Ok(Ordering::Greater),
        _ => Ok(cmp_values(a, b)?.unwrap_or(Ordering::Equal)),
    }
}

fn arith(op: BinOp, a: Value, b: Value) -> Result<Value> {
    use Value::{Integer, Null, Real};
    match (a, b) {
        (Null, _) | (_, Null) => Ok(Null),
        (Integer(x), Integer(y)) => {
            let v = match op {
                BinOp::Add => x.checked_add(y),
                BinOp::Sub => x.checked_sub(y),
                BinOp::Mul => x.checked_mul(y),
                BinOp::Div => {
                    if y == 0 {
                        return Err(sql_err("división por cero"));
                    }
                    x.checked_div(y)
                }
                BinOp::Mod => {
                    if y == 0 {
                        return Err(sql_err("división por cero"));
                    }
                    x.checked_rem(y)
                }
                _ => unreachable!("solo aritmética"),
            };
            v.map(Integer).ok_or(sql_err("desbordamiento de entero"))
        }
        (a @ (Integer(_) | Real(_)), b @ (Integer(_) | Real(_))) => {
            let x = as_f64(&a);
            let y = as_f64(&b);
            Ok(Real(match op {
                BinOp::Add => x + y,
                BinOp::Sub => x - y,
                BinOp::Mul => x * y,
                BinOp::Div => x / y, // IEEE: ±inf
                BinOp::Mod => x % y,
                _ => unreachable!("solo aritmética"),
            }))
        }
        (a, b) => Err(sql_err(format!(
            "aritmética sobre {} y {}",
            a.type_name(),
            b.type_name()
        ))),
    }
}

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Integer(n) => *n as f64,
        Value::Real(f) => *f,
        _ => unreachable!("validado por el llamador"),
    }
}

/// LIKE con `%` (cualquier secuencia) y `_` (un carácter), sensible a mayúsculas.
fn like_match(pattern: &[char], s: &[char]) -> bool {
    match pattern.first() {
        None => s.is_empty(),
        Some('%') => (0..=s.len()).any(|i| like_match(&pattern[1..], &s[i..])),
        Some('_') => !s.is_empty() && like_match(&pattern[1..], &s[1..]),
        Some(c) => s.first() == Some(c) && like_match(&pattern[1..], &s[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn like_patterns() {
        assert!(like_match(&chars("ho%"), &chars("hola")));
        assert!(like_match(&chars("%la"), &chars("hola")));
        assert!(like_match(&chars("h_la"), &chars("hola")));
        assert!(like_match(&chars("%"), &chars("")));
        assert!(like_match(&chars("a%b%c"), &chars("aXXbYYc")));
        assert!(
            !like_match(&chars("Ho%"), &chars("hola")),
            "sensible a mayúsculas"
        );
        assert!(!like_match(&chars("h_"), &chars("hola")));
    }

    #[test]
    fn three_valued_logic() {
        use Value::{Bool, Null};
        // NULL AND FALSE = FALSE; NULL AND TRUE = NULL; NULL OR TRUE = TRUE.
        let and = |a: Value, b: Value| {
            eval(
                &Expr::Binary(
                    Box::new(Expr::Literal(a)),
                    BinOp::And,
                    Box::new(Expr::Literal(b)),
                ),
                None,
                &[],
            )
            .unwrap()
        };
        let or = |a: Value, b: Value| {
            eval(
                &Expr::Binary(
                    Box::new(Expr::Literal(a)),
                    BinOp::Or,
                    Box::new(Expr::Literal(b)),
                ),
                None,
                &[],
            )
            .unwrap()
        };
        assert_eq!(and(Null, Bool(false)), Bool(false));
        assert_eq!(and(Null, Bool(true)), Null);
        assert_eq!(or(Null, Bool(true)), Bool(true));
        assert_eq!(or(Null, Bool(false)), Null);
    }

    #[test]
    fn integer_overflow_and_div_zero_are_errors() {
        let bin = |a: i64, op: BinOp, b: i64| {
            eval(
                &Expr::Binary(
                    Box::new(Expr::Literal(Value::Integer(a))),
                    op,
                    Box::new(Expr::Literal(Value::Integer(b))),
                ),
                None,
                &[],
            )
        };
        assert!(bin(i64::MAX, BinOp::Add, 1).is_err());
        assert!(bin(7, BinOp::Div, 0).is_err());
        assert!(bin(7, BinOp::Mod, 0).is_err());
        assert_eq!(bin(7, BinOp::Div, 2).unwrap(), Value::Integer(3));
    }

    #[test]
    fn aggregates_over_values() {
        use crate::catalog::{ColType, ColumnDef};
        let def = TableDef {
            name: "t".into(),
            table_id: 1,
            rowid_alias: None,
            columns: vec![ColumnDef {
                name: "v".into(),
                col_type: ColType::Integer,
                not_null: false,
                default: None,
            }],
            indexes: Vec::new(),
        };
        let schema = QuerySchema::single("t", def);
        let rows: Vec<Vec<Value>> = vec![
            vec![Value::Integer(10)],
            vec![Value::Null],
            vec![Value::Integer(30)],
        ];
        let agg = |func: AggFunc, arg: Option<Expr>| {
            compute_aggregate(func, arg.as_ref(), &schema, &rows, &[]).unwrap()
        };
        let col = Expr::Column {
            table: None,
            name: "v".into(),
        };
        // COUNT(*) cuenta filas; COUNT(v) ignora NULL.
        assert_eq!(agg(AggFunc::Count, None), Value::Integer(3));
        assert_eq!(agg(AggFunc::Count, Some(col.clone())), Value::Integer(2));
        assert_eq!(agg(AggFunc::Sum, Some(col.clone())), Value::Integer(40));
        assert_eq!(agg(AggFunc::Avg, Some(col.clone())), Value::Real(20.0));
        assert_eq!(agg(AggFunc::Min, Some(col.clone())), Value::Integer(10));
        assert_eq!(agg(AggFunc::Max, Some(col)), Value::Integer(30));
        // Sobre cero valores: COUNT = 0, el resto NULL.
        let none = compute_aggregate(
            AggFunc::Sum,
            Some(&Expr::Column {
                table: None,
                name: "v".into(),
            }),
            &schema,
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(none, Value::Null);
    }

    #[test]
    fn unqualified_columns_must_be_unambiguous() {
        use crate::catalog::{ColType, ColumnDef};
        let table = |name: &str| TableDef {
            name: name.into(),
            table_id: 1,
            rowid_alias: None,
            columns: vec![ColumnDef {
                name: "id".into(),
                col_type: ColType::Integer,
                not_null: false,
                default: None,
            }],
            indexes: Vec::new(),
        };
        let mut schema = QuerySchema::single("a", table("a"));
        schema.push("b", table("b")).unwrap();
        assert!(schema.resolve(None, "id").is_err(), "ambigua");
        assert_eq!(schema.resolve(Some("b"), "id").unwrap(), 1);
        assert!(schema.resolve(Some("zz"), "id").is_err());
        assert!(schema.push("a", table("a")).is_err(), "alias duplicado");
    }
}
