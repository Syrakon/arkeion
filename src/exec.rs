//! Ejecutor SQL (docs/04-sql.md): planificador mínimo (full scan o *point
//! lookup* por rowid), joins nested-loop, agregados con/sin `GROUP BY` (+ `HAVING`)
//! y evaluación de expresiones con lógica trivalente.
//!
//! Filosofía de tipos (human-first): comparar tipos distintos es un error,
//! no una coerción silenciosa. Única promoción: INTEGER ↔ REAL.

use std::cmp::Ordering;
use std::collections::HashSet;

use crate::catalog::{
    self, ColType, ColumnDef, ColumnSpec, IndexDef, TableDef, TableSpec, TriggerDef, TriggerEvent,
    TriggerForEach, TriggerTiming,
};
use crate::error::{Error, Result};
use crate::record::Value;
use crate::sql::ast::{
    AggFunc, BinOp, ColumnAst, Cte, Expr, JoinKind, SelectItem, SelectStmt, SetOp, Stmt, TableRef,
    UnOp,
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
    /// El SELECT (texto) de una vista con ese nombre, o `None`. Lo usa el overlay
    /// que materializa las vistas.
    fn view(&self, name: &str) -> Result<Option<String>>;
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

    fn view(&self, name: &str) -> Result<Option<String>> {
        Snapshot::view(self, name)
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

    fn view(&self, name: &str) -> Result<Option<String>> {
        WriteTx::view(self, name)
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
                    .position(|c| !c.dropped && c.name == name)
                    .ok_or_else(|| sql_err(format!("columna desconocida: {q}.{name}")))?;
                Ok(t.offset + i)
            }
            None => {
                let mut found = None;
                for t in &self.tables {
                    if let Some(i) = t
                        .def
                        .columns
                        .iter()
                        .position(|c| !c.dropped && c.name == name)
                    {
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
        Expr::Function { args, .. } => args.iter().try_for_each(|a| validate_columns(a, schema)),
        Expr::In { expr, list, .. } => {
            validate_columns(expr, schema)?;
            list.iter().try_for_each(|e| validate_columns(e, schema))
        }
        Expr::Cast { expr, .. } => validate_columns(expr, schema),
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(o) = operand {
                validate_columns(o, schema)?;
            }
            for (c, r) in whens {
                validate_columns(c, schema)?;
                validate_columns(r, schema)?;
            }
            else_
                .as_deref()
                .map_or(Ok(()), |e| validate_columns(e, schema))
        }
        // El cuerpo de la subconsulta se valida al ejecutarse (otro ámbito); aquí
        // solo la parte externa del `IN`.
        Expr::ScalarSubquery(_) | Expr::Exists(_) => Ok(()),
        Expr::InSubquery { expr, .. } => validate_columns(expr, schema),
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
            foreign_keys,
        } => {
            if tx.table(name)?.is_some() {
                if *if_not_exists {
                    return Ok(0);
                }
                return Err(Error::Constraint("la tabla ya existe"));
            }
            tx.create_table(&table_spec(name, columns, foreign_keys)?)?;
            Ok(0)
        }
        Stmt::DropTable { if_exists, name } => {
            let dropped = tx.drop_table(name)?;
            if !dropped && !if_exists {
                return Err(sql_err(format!("tabla desconocida: {name}")));
            }
            Ok(0)
        }
        Stmt::CreateView {
            if_not_exists,
            name,
            select_sql,
        } => {
            if tx.view(name)?.is_some() {
                if *if_not_exists {
                    return Ok(0);
                }
                return Err(Error::Constraint("la vista ya existe"));
            }
            tx.create_view(name, select_sql)?;
            Ok(0)
        }
        Stmt::DropView { if_exists, name } => {
            let dropped = tx.drop_view(name)?;
            if !dropped && !if_exists {
                return Err(sql_err(format!("vista desconocida: {name}")));
            }
            Ok(0)
        }
        Stmt::CreateTrigger {
            if_not_exists,
            name,
            timing,
            event,
            for_each,
            table,
            body_sql,
        } => {
            if tx.trigger(name)?.is_some() {
                if *if_not_exists {
                    return Ok(0);
                }
                return Err(Error::Constraint("el trigger ya existe"));
            }
            // Valida el cuerpo (DML) por adelantado: re-parsea y exige INSERT/
            // UPDATE/DELETE.
            for s in &crate::sql::parser::parse_many(body_sql)? {
                if !matches!(
                    s,
                    Stmt::Insert { .. } | Stmt::Update { .. } | Stmt::Delete { .. }
                ) {
                    return Err(sql_err(
                        "el cuerpo de un trigger solo admite INSERT/UPDATE/DELETE",
                    ));
                }
            }
            tx.create_trigger(&catalog::TriggerDef {
                name: name.clone(),
                timing: *timing,
                event: *event,
                for_each: *for_each,
                table: table.clone(),
                body: body_sql.clone(),
            })?;
            Ok(0)
        }
        Stmt::DropTrigger { if_exists, name } => {
            let dropped = tx.drop_trigger(name)?;
            if !dropped && !if_exists {
                return Err(sql_err(format!("trigger desconocido: {name}")));
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
            returning,
        } => Ok(run_insert(
            tx,
            table,
            columns.as_deref(),
            rows,
            params,
            returning.as_deref(),
        )?
        .0),
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
                    dropped: false,
                },
            )?;
            Ok(0)
        }
        Stmt::AlterTableMoveColumn { table, column, pos } => {
            tx.move_column(table, column, pos)?;
            Ok(0)
        }
        Stmt::AlterTableReorderColumns { table, order } => {
            tx.reorder_columns(table, order)?;
            Ok(0)
        }
        Stmt::AlterTableRenameColumn { table, old, new } => {
            tx.rename_column(table, old, new)?;
            Ok(0)
        }
        Stmt::AlterTableDropColumn { table, column } => {
            tx.drop_column(table, column)?;
            Ok(0)
        }
        Stmt::Update {
            table,
            sets,
            where_clause,
            returning,
        } => Ok(run_update(
            tx,
            table,
            sets,
            where_clause.as_ref(),
            params,
            returning.as_deref(),
        )?
        .0),
        Stmt::Delete {
            table,
            where_clause,
            returning,
        } => Ok(run_delete(
            tx,
            table,
            where_clause.as_ref(),
            params,
            returning.as_deref(),
        )?
        .0),
        // La conexión intercepta estas tres antes de llegar aquí (api.rs).
        Stmt::Begin | Stmt::Commit | Stmt::Rollback => Err(sql_err(
            "BEGIN/COMMIT/ROLLBACK los gestiona la conexión, no el ejecutor",
        )),
        Stmt::Select(_) => Err(sql_err("SELECT devuelve filas: usa query, no execute")),
    }
}

/// Ejecuta una escritura **con `RETURNING`** y devuelve sus filas. La usa el
/// camino de consulta de la conexión (la escritura se realiza igual que en
/// `run_execute`; aquí además se proyectan las filas afectadas).
pub fn run_returning(tx: &mut WriteTx, stmt: &Stmt, params: &[Value]) -> Result<SelectOut> {
    let out = match stmt {
        Stmt::Insert {
            table,
            columns,
            rows,
            returning,
        } => run_insert(
            tx,
            table,
            columns.as_deref(),
            rows,
            params,
            returning.as_deref(),
        )?,
        Stmt::Update {
            table,
            sets,
            where_clause,
            returning,
        } => run_update(
            tx,
            table,
            sets,
            where_clause.as_ref(),
            params,
            returning.as_deref(),
        )?,
        Stmt::Delete {
            table,
            where_clause,
            returning,
        } => run_delete(
            tx,
            table,
            where_clause.as_ref(),
            params,
            returning.as_deref(),
        )?,
        _ => {
            return Err(sql_err(
                "solo INSERT/UPDATE/DELETE … RETURNING devuelven filas",
            ));
        }
    };
    out.1
        .ok_or_else(|| sql_err("la sentencia no lleva RETURNING"))
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

fn run_insert(
    tx: &mut WriteTx,
    table: &str,
    columns: Option<&[String]>,
    rows: &[Vec<Expr>],
    params: &[Value],
    returning: Option<&[SelectItem]>,
) -> Result<(usize, Option<SelectOut>)> {
    // Escribir en una vista: lo gestiona un trigger INSTEAD OF (la vista no almacena).
    if tx.table(table)?.is_none() && tx.view(table)?.is_some() {
        if returning.is_some() {
            return Err(sql_err("RETURNING no se admite al escribir en una vista"));
        }
        return Ok((
            run_instead_of_insert(tx, table, columns, rows, params)?,
            None,
        ));
    }
    // `table_cached`: un INSERT-por-fila en un lote no re-desciende el catálogo por
    // cada sentencia (el esquema no cambia entre filas).
    let def = tx
        .table_cached(table)?
        .ok_or_else(|| sql_err(format!("tabla desconocida: {table}")))?;
    let (before_row, before_stmt) =
        split_for_each(tx.triggers_for(&def.name, TriggerEvent::Insert, TriggerTiming::Before)?);
    let (after_row, after_stmt) =
        split_for_each(tx.triggers_for(&def.name, TriggerEvent::Insert, TriggerTiming::After)?);
    fire(tx, &def, &before_stmt, None, None)?; // BEFORE … FOR EACH STATEMENT
    // La fila NEW (con el rowid asignado) solo se necesita para AFTER INSERT o RETURNING.
    let want_new = !after_row.is_empty() || returning.is_some();
    let mut returned = Vec::new();
    // Buffer prestado de la tx: ni un `Vec` de valores por fila. Si una fila falla,
    // el `?` lo pierde — camino frío, el próximo take asigna.
    let mut values = tx.take_values_buf();
    for row in rows {
        insert_values_into(&def, columns, row, params, &mut values)?;
        fire(tx, &def, &before_row, None, Some(&values))?; // BEFORE INSERT
        let rowid = tx.insert_row(&def, &values)?;
        if want_new {
            let mut new_after = values.clone();
            if let Some(i) = def.rowid_alias {
                new_after[i] = Value::Integer(rowid);
            }
            fire(tx, &def, &after_row, None, Some(&new_after))?; // AFTER INSERT
            if returning.is_some() {
                returned.push(new_after);
            }
        }
    }
    tx.put_values_buf(values);
    fire(tx, &def, &after_stmt, None, None)?; // AFTER … FOR EACH STATEMENT
    let out = returning
        .map(|items| eval_returning(table, &def, items, &returned, params))
        .transpose()?;
    Ok((rows.len(), out))
}

fn run_update(
    tx: &mut WriteTx,
    table: &str,
    sets: &[(String, Expr)],
    where_clause: Option<&Expr>,
    params: &[Value],
    returning: Option<&[SelectItem]>,
) -> Result<(usize, Option<SelectOut>)> {
    if tx.table(table)?.is_none() && tx.view(table)?.is_some() {
        if returning.is_some() {
            return Err(sql_err("RETURNING no se admite al escribir en una vista"));
        }
        return Ok((
            run_instead_of_update(tx, table, sets, where_clause, params)?,
            None,
        ));
    }
    let def = tx
        .table_cached(table)?
        .ok_or_else(|| sql_err(format!("tabla desconocida: {table}")))?;
    let schema = QuerySchema::single(table, (*def).clone());

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
    let (before_row, before_stmt) =
        split_for_each(tx.triggers_for(&def.name, TriggerEvent::Update, TriggerTiming::Before)?);
    let (after_row, after_stmt) =
        split_for_each(tx.triggers_for(&def.name, TriggerEvent::Update, TriggerTiming::After)?);

    // Materializar primero (point lookup por PK o full scan): el scan toma
    // prestada la tx, que luego necesitamos en exclusiva para escribir.
    let mut updates: Vec<(i64, Vec<Value>, Vec<Value>)> = Vec::new(); // rowid, OLD, NEW
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
        updates.push((rowid, row, new_row));
    }
    let n = updates.len();
    let mut returned = Vec::new(); // filas NEW para RETURNING
    fire(tx, &def, &before_stmt, None, None)?; // BEFORE … FOR EACH STATEMENT
    for (rowid, old_row, new_row) in updates {
        fire(tx, &def, &before_row, Some(&old_row), Some(&new_row))?;
        tx.update_row(&def, rowid, &new_row)?;
        if returning.is_some() {
            returned.push(new_row.clone());
        }
        fire(tx, &def, &after_row, Some(&old_row), Some(&new_row))?;
    }
    fire(tx, &def, &after_stmt, None, None)?; // AFTER … FOR EACH STATEMENT
    let out = returning
        .map(|items| eval_returning(table, &def, items, &returned, params))
        .transpose()?;
    Ok((n, out))
}

fn run_delete(
    tx: &mut WriteTx,
    table: &str,
    where_clause: Option<&Expr>,
    params: &[Value],
    returning: Option<&[SelectItem]>,
) -> Result<(usize, Option<SelectOut>)> {
    if tx.table(table)?.is_none() && tx.view(table)?.is_some() {
        if returning.is_some() {
            return Err(sql_err("RETURNING no se admite al escribir en una vista"));
        }
        return Ok((
            run_instead_of_delete(tx, table, where_clause, params)?,
            None,
        ));
    }
    let def = tx
        .table_cached(table)?
        .ok_or_else(|| sql_err(format!("tabla desconocida: {table}")))?;
    let schema = QuerySchema::single(table, (*def).clone());
    if let Some(cond) = where_clause {
        no_aggregates(cond, "WHERE")?;
        validate_columns(cond, &schema)?;
    }
    let (before_row, before_stmt) =
        split_for_each(tx.triggers_for(&def.name, TriggerEvent::Delete, TriggerTiming::Before)?);
    let (after_row, after_stmt) =
        split_for_each(tx.triggers_for(&def.name, TriggerEvent::Delete, TriggerTiming::After)?);
    let mut doomed: Vec<(i64, Vec<Value>)> = Vec::new(); // rowid, OLD
    for (rowid, row) in candidate_rows(tx, &def, &schema, where_clause, params)? {
        if let Some(cond) = where_clause
            && !truthy(eval(cond, Some((&schema, &row)), params)?)?
        {
            continue;
        }
        doomed.push((rowid, row));
    }
    let n = doomed.len();
    let mut returned = Vec::new(); // filas OLD (borradas) para RETURNING
    fire(tx, &def, &before_stmt, None, None)?; // BEFORE … FOR EACH STATEMENT
    for (rowid, old_row) in doomed {
        fire(tx, &def, &before_row, Some(&old_row), None)?;
        tx.delete_row(&def, rowid)?;
        if returning.is_some() {
            returned.push(old_row.clone());
        }
        fire(tx, &def, &after_row, Some(&old_row), None)?;
    }
    fire(tx, &def, &after_stmt, None, None)?; // AFTER … FOR EACH STATEMENT
    let out = returning
        .map(|items| eval_returning(table, &def, items, &returned, params))
        .transpose()?;
    Ok((n, out))
}

/// Proyecta `RETURNING` sobre las filas afectadas (esquema de una sola tabla).
/// `*` se expande a las columnas visibles (orden lógico, sin las borradas). No
/// admite agregados.
fn eval_returning(
    table: &str,
    def: &TableDef,
    items: &[SelectItem],
    rows: &[Vec<Value>],
    params: &[Value],
) -> Result<SelectOut> {
    let schema = QuerySchema::single(table, def.clone());
    let visible: Vec<usize> = def
        .logical_order
        .iter()
        .copied()
        .filter(|&p| !def.columns[p].dropped)
        .collect();
    let mut columns = Vec::new();
    let mut projs: Vec<Option<&Expr>> = Vec::new(); // None = Star
    for (i, item) in items.iter().enumerate() {
        match item {
            SelectItem::Star => {
                for &p in &visible {
                    columns.push(def.columns[p].name.clone());
                }
                projs.push(None);
            }
            SelectItem::Expr { expr, alias } => {
                no_aggregates(expr, "RETURNING")?;
                validate_columns(expr, &schema)?;
                columns.push(alias.clone().unwrap_or_else(|| match expr {
                    Expr::Column { name, .. } => name.clone(),
                    _ => format!("col{}", i + 1),
                }));
                projs.push(Some(expr));
            }
        }
    }
    let mut out_rows = Vec::with_capacity(rows.len());
    for row in rows {
        let mut out = Vec::with_capacity(columns.len());
        for proj in &projs {
            match proj {
                None => out.extend(visible.iter().map(|&i| row[i].clone())),
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

// --- INSTEAD OF: escrituras sobre vistas reemplazadas por el cuerpo del trigger ---

/// Materializa una vista contra el estado actual de la tx: devuelve su `TableDef`
/// sintética (nombres de columna de salida) y sus filas. Es lo que ve `OLD`/`NEW`
/// de un trigger INSTEAD OF.
fn materialize_view(tx: &WriteTx, name: &str) -> Result<(TableDef, Vec<Vec<Value>>)> {
    let vs = ViewSource::new(tx);
    let def = vs
        .table(name)?
        .ok_or_else(|| sql_err(format!("vista desconocida: {name}")))?;
    let rows = vs.scan_rows(&def)?;
    Ok((def, rows))
}

/// `INSERT INTO vista …`: dispara el trigger INSTEAD OF INSERT con `NEW` = la fila
/// propuesta (sobre el esquema de columnas de la vista).
fn run_instead_of_insert(
    tx: &mut WriteTx,
    view: &str,
    columns: Option<&[String]>,
    rows: &[Vec<Expr>],
    params: &[Value],
) -> Result<usize> {
    let trigs = tx.triggers_for(view, TriggerEvent::Insert, TriggerTiming::InsteadOf)?;
    if trigs.is_empty() {
        return Err(sql_err(
            "no se puede INSERT en una vista sin un trigger INSTEAD OF",
        ));
    }
    let (vdef, _) = materialize_view(tx, view)?;
    for row in rows {
        let mut new = vec![Value::Null; vdef.columns.len()];
        match columns {
            Some(names) => {
                if names.len() != row.len() {
                    return Err(sql_err("número distinto de columnas y de valores"));
                }
                for (nm, e) in names.iter().zip(row) {
                    let i = vdef
                        .columns
                        .iter()
                        .position(|c| &c.name == nm)
                        .ok_or_else(|| sql_err(format!("columna desconocida en la vista: {nm}")))?;
                    new[i] = eval_const(e, params)?;
                }
            }
            None => {
                if row.len() != vdef.columns.len() {
                    return Err(sql_err(format!(
                        "la vista tiene {} columnas pero se dieron {} valores",
                        vdef.columns.len(),
                        row.len()
                    )));
                }
                for (i, e) in row.iter().enumerate() {
                    new[i] = eval_const(e, params)?;
                }
            }
        }
        fire(tx, &vdef, &trigs, None, Some(&new))?;
    }
    Ok(rows.len())
}

/// `UPDATE vista SET … WHERE …`: por cada fila de la vista que casa el WHERE,
/// dispara el trigger INSTEAD OF UPDATE con `OLD` = la fila y `NEW` = OLD con el
/// SET aplicado.
fn run_instead_of_update(
    tx: &mut WriteTx,
    view: &str,
    sets: &[(String, Expr)],
    where_clause: Option<&Expr>,
    params: &[Value],
) -> Result<usize> {
    let trigs = tx.triggers_for(view, TriggerEvent::Update, TriggerTiming::InsteadOf)?;
    if trigs.is_empty() {
        return Err(sql_err(
            "no se puede UPDATE una vista sin un trigger INSTEAD OF",
        ));
    }
    let (vdef, rows) = materialize_view(tx, view)?;
    let schema = QuerySchema::single(view, vdef.clone());
    let mut set_idx = Vec::with_capacity(sets.len());
    for (name, expr) in sets {
        let i = vdef
            .columns
            .iter()
            .position(|c| &c.name == name)
            .ok_or_else(|| sql_err(format!("columna desconocida en la vista: {name}")))?;
        no_aggregates(expr, "SET")?;
        validate_columns(expr, &schema)?;
        set_idx.push(i);
    }
    if let Some(cond) = where_clause {
        no_aggregates(cond, "WHERE")?;
        validate_columns(cond, &schema)?;
    }
    let mut n = 0;
    for row in rows {
        if let Some(cond) = where_clause
            && !truthy(eval(cond, Some((&schema, &row)), params)?)?
        {
            continue;
        }
        let mut new_row = row.clone();
        for (&i, (_, expr)) in set_idx.iter().zip(sets) {
            new_row[i] = eval(expr, Some((&schema, &row)), params)?;
        }
        fire(tx, &vdef, &trigs, Some(&row), Some(&new_row))?;
        n += 1;
    }
    Ok(n)
}

/// `DELETE FROM vista WHERE …`: por cada fila de la vista que casa el WHERE,
/// dispara el trigger INSTEAD OF DELETE con `OLD` = la fila.
fn run_instead_of_delete(
    tx: &mut WriteTx,
    view: &str,
    where_clause: Option<&Expr>,
    params: &[Value],
) -> Result<usize> {
    let trigs = tx.triggers_for(view, TriggerEvent::Delete, TriggerTiming::InsteadOf)?;
    if trigs.is_empty() {
        return Err(sql_err(
            "no se puede DELETE en una vista sin un trigger INSTEAD OF",
        ));
    }
    let (vdef, rows) = materialize_view(tx, view)?;
    let schema = QuerySchema::single(view, vdef.clone());
    if let Some(cond) = where_clause {
        no_aggregates(cond, "WHERE")?;
        validate_columns(cond, &schema)?;
    }
    let mut n = 0;
    for row in rows {
        if let Some(cond) = where_clause
            && !truthy(eval(cond, Some((&schema, &row)), params)?)?
        {
            continue;
        }
        fire(tx, &vdef, &trigs, Some(&row), None)?;
        n += 1;
    }
    Ok(n)
}

// --- triggers: disparo row-level con sustitución de OLD/NEW ---

/// Sustituye `OLD.col`/`NEW.col` por el valor de la fila (cuerpo de trigger). Lo
/// demás queda igual: columnas sin calificar y otras tablas se resuelven normal al
/// ejecutar la sentencia.
fn subst_expr(
    e: &Expr,
    table: &TableDef,
    old: Option<&[Value]>,
    new: Option<&[Value]>,
) -> Result<Expr> {
    let r = |x: &Expr| subst_expr(x, table, old, new);
    let b = |x: &Expr| r(x).map(Box::new);
    let opt =
        |x: &Option<Box<Expr>>| -> Result<Option<Box<Expr>>> { x.as_deref().map(b).transpose() };
    Ok(match e {
        Expr::Column {
            table: Some(q),
            name,
        } if q.eq_ignore_ascii_case("OLD") || q.eq_ignore_ascii_case("NEW") => {
            let is_new = q.eq_ignore_ascii_case("NEW");
            let vals = if is_new { new } else { old }.ok_or_else(|| {
                sql_err(format!(
                    "{} no está disponible en este trigger",
                    if is_new { "NEW" } else { "OLD" }
                ))
            })?;
            let i = table
                .columns
                .iter()
                .position(|c| c.name == *name)
                .ok_or_else(|| sql_err(format!("columna desconocida: {q}.{name}")))?;
            Expr::Literal(vals[i].clone())
        }
        Expr::Literal(_) | Expr::Column { .. } | Expr::Param(_) => e.clone(),
        Expr::Unary(op, x) => Expr::Unary(*op, b(x)?),
        Expr::Binary(a, op, c) => Expr::Binary(b(a)?, *op, b(c)?),
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: b(expr)?,
            negated: *negated,
        },
        Expr::Like {
            expr,
            pattern,
            negated,
        } => Expr::Like {
            expr: b(expr)?,
            pattern: b(pattern)?,
            negated: *negated,
        },
        Expr::Aggregate {
            func,
            arg,
            distinct,
            sep,
        } => Expr::Aggregate {
            func: *func,
            arg: opt(arg)?,
            distinct: *distinct,
            sep: opt(sep)?,
        },
        Expr::Function { name, args } => Expr::Function {
            name: name.clone(),
            args: args.iter().map(r).collect::<Result<_>>()?,
        },
        Expr::In {
            expr,
            list,
            negated,
        } => Expr::In {
            expr: b(expr)?,
            list: list.iter().map(r).collect::<Result<_>>()?,
            negated: *negated,
        },
        Expr::Cast { expr, to } => Expr::Cast {
            expr: b(expr)?,
            to: *to,
        },
        Expr::Case {
            operand,
            whens,
            else_,
        } => Expr::Case {
            operand: opt(operand)?,
            whens: whens
                .iter()
                .map(|(c, rr)| Ok((r(c)?, r(rr)?)))
                .collect::<Result<_>>()?,
            else_: opt(else_)?,
        },
        Expr::ScalarSubquery(_) | Expr::Exists(_) | Expr::InSubquery { .. } => e.clone(),
    })
}

/// Sustituye `OLD`/`NEW` en una sentencia DML del cuerpo de un trigger.
fn subst_stmt(
    stmt: &Stmt,
    table: &TableDef,
    old: Option<&[Value]>,
    new: Option<&[Value]>,
) -> Result<Stmt> {
    let r = |e: &Expr| subst_expr(e, table, old, new);
    Ok(match stmt {
        Stmt::Insert {
            table: t,
            columns,
            rows,
            returning,
        } => Stmt::Insert {
            table: t.clone(),
            columns: columns.clone(),
            rows: rows
                .iter()
                .map(|row| row.iter().map(&r).collect::<Result<Vec<_>>>())
                .collect::<Result<Vec<_>>>()?,
            returning: returning.clone(),
        },
        Stmt::Update {
            table: t,
            sets,
            where_clause,
            returning,
        } => Stmt::Update {
            table: t.clone(),
            sets: sets
                .iter()
                .map(|(c, e)| Ok((c.clone(), r(e)?)))
                .collect::<Result<_>>()?,
            where_clause: where_clause.as_ref().map(&r).transpose()?,
            returning: returning.clone(),
        },
        Stmt::Delete {
            table: t,
            where_clause,
            returning,
        } => Stmt::Delete {
            table: t.clone(),
            where_clause: where_clause.as_ref().map(&r).transpose()?,
            returning: returning.clone(),
        },
        other => other.clone(),
    })
}

/// Particiona los triggers de un (evento, momento) en `(row-level, statement-level)`.
/// Los row-level se disparan por fila; los statement-level una vez por sentencia.
fn split_for_each(triggers: Vec<TriggerDef>) -> (Vec<TriggerDef>, Vec<TriggerDef>) {
    triggers
        .into_iter()
        .partition(|t| t.for_each == TriggerForEach::Row)
}

/// Dispara `triggers` (mismo evento+momento) para una fila, con guarda de recursión.
fn fire(
    tx: &mut WriteTx,
    table: &TableDef,
    triggers: &[TriggerDef],
    old: Option<&[Value]>,
    new: Option<&[Value]>,
) -> Result<()> {
    if triggers.is_empty() {
        return Ok(());
    }
    tx.enter_trigger()?;
    let r = fire_inner(tx, table, triggers, old, new);
    tx.exit_trigger();
    r
}

fn fire_inner(
    tx: &mut WriteTx,
    table: &TableDef,
    triggers: &[TriggerDef],
    old: Option<&[Value]>,
    new: Option<&[Value]>,
) -> Result<()> {
    for trig in triggers {
        for stmt in crate::sql::parser::parse_many(&trig.body)? {
            let sub = subst_stmt(&stmt, table, old, new)?;
            run_execute(tx, &sub, &[])?;
        }
    }
    Ok(())
}

fn table_spec(
    name: &str,
    columns: &[ColumnAst],
    foreign_keys: &[catalog::ForeignKeySpec],
) -> Result<TableSpec> {
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
            references: col.references.clone(),
        });
    }
    Ok(TableSpec {
        name: name.to_owned(),
        columns: specs,
        foreign_keys: foreign_keys.to_vec(),
    })
}

/// Evalúa la fila de un INSERT en `out` (un `Vec` **reutilizado**: lo limpia
/// antes y no asigna por fila, M10-perf fase 2).
fn insert_values_into(
    def: &TableDef,
    columns: Option<&[String]>,
    exprs: &[Expr],
    params: &[Value],
    out: &mut Vec<Value>,
) -> Result<()> {
    out.clear();
    match columns {
        // Posicional (sin lista de columnas): hay que dar un valor por **cada**
        // columna, ni más ni menos (como SQLite). Aceptar de menos rellenaría
        // columnas con NULL que el usuario no escribió — para nombrar un
        // subconjunto está la forma `INSERT … (col, …) VALUES …`.
        None => {
            // Un valor por cada columna **visible** (orden lógico, sin las borradas);
            // las posiciones físicas borradas quedan NULL.
            let visible: Vec<usize> = def
                .logical_order
                .iter()
                .copied()
                .filter(|&p| !def.columns[p].dropped)
                .collect();
            if exprs.len() != visible.len() {
                return Err(sql_err(format!(
                    "la tabla tiene {} columnas pero se dieron {} valores",
                    visible.len(),
                    exprs.len()
                )));
            }
            out.resize(def.columns.len(), Value::Null);
            for (i, e) in exprs.iter().enumerate() {
                out[visible[i]] = eval_const(e, params)?;
            }
            Ok(())
        }
        Some(names) => {
            if names.len() != exprs.len() {
                return Err(sql_err("número distinto de columnas y de valores"));
            }
            // Las no nombradas toman su DEFAULT (o NULL); el alias del rowid
            // queda NULL ⇒ rowid automático.
            for (i, c) in def.columns.iter().enumerate() {
                out.push(if def.rowid_alias == Some(i) {
                    Value::Null
                } else {
                    c.default.clone().unwrap_or(Value::Null)
                });
            }
            for (name, e) in names.iter().zip(exprs) {
                let i = col_index(def, name)?;
                if names.iter().filter(|n| *n == name).count() > 1 {
                    return Err(sql_err(format!("columna repetida en INSERT: {name}")));
                }
                out[i] = eval_const(e, params)?;
            }
            Ok(())
        }
    }
}

fn col_index(def: &TableDef, name: &str) -> Result<usize> {
    def.columns
        .iter()
        .position(|c| !c.dropped && c.name == name)
        .ok_or_else(|| sql_err(format!("columna desconocida: {name}")))
}

// --- SELECT ---

/// `SELECT <expr>, …` sin `FROM`: evalúa expresiones constantes contra una única
/// fila implícita (estilo SQLite). Sin tabla no hay columnas, así que `eval` con
/// fila `None` rechaza por sí mismo cualquier columna/agregado; aquí solo se
/// vetan las cláusulas que necesitan filas y la proyección `*`.
fn run_select_no_from(stmt: &SelectStmt, params: &[Value]) -> Result<SelectOut> {
    if !stmt.joins.is_empty() {
        return Err(sql_err("JOIN requiere FROM"));
    }
    if !stmt.group_by.is_empty() {
        return Err(sql_err("GROUP BY requiere FROM"));
    }
    if stmt.having.is_some() {
        return Err(sql_err("HAVING requiere FROM"));
    }
    if !stmt.order_by.is_empty() {
        return Err(sql_err("ORDER BY requiere FROM"));
    }
    if stmt.as_of.is_some() {
        return Err(sql_err("AS OF requiere FROM"));
    }

    // Nombres de columna (sin evaluar: las columnas existen aunque la fila se
    // filtre) y las expresiones a proyectar. Nombre = alias, el de la propia
    // expresión, o `colN`, igual que con FROM.
    let mut columns = Vec::with_capacity(stmt.projection.len());
    let mut exprs: Vec<&Expr> = Vec::with_capacity(stmt.projection.len());
    for (i, item) in stmt.projection.iter().enumerate() {
        let SelectItem::Expr { expr, alias } = item else {
            return Err(sql_err("SELECT * requiere FROM"));
        };
        if expr.has_aggregate() {
            return Err(sql_err("un agregado requiere FROM"));
        }
        columns.push(alias.clone().unwrap_or_else(|| match expr {
            Expr::Column { name, .. } => name.clone(),
            _ => format!("col{}", i + 1),
        }));
        exprs.push(expr);
    }

    // ¿Sobrevive la única fila implícita? El `WHERE` constante y `LIMIT/OFFSET`
    // deciden ANTES de materializar la proyección, igual que el camino con FROM
    // (que filtra y recorta y solo entonces proyecta): así un error de proyección
    // sobre una fila ya excluida no aflora (`SELECT 'a' + 1 WHERE 0` → cero filas).
    let keep = match &stmt.where_clause {
        Some(cond) => truthy(eval_const(cond, params)?)?,
        None => true,
    };
    let placeholder = if keep { vec![Vec::new()] } else { Vec::new() };
    let rows = if limit_offset(placeholder, stmt, params)?.is_empty() {
        Vec::new()
    } else {
        let row = exprs
            .iter()
            .map(|e| eval_const(e, params))
            .collect::<Result<Vec<_>>>()?;
        vec![row]
    };
    Ok(SelectOut { columns, rows })
}

/// Punto de entrada de la API para SELECT: resuelve **vistas** envolviendo la
/// fuente en un overlay que las materializa bajo demanda, y delega en `run_select`
/// (que ya maneja CTEs/subconsultas/UNION contra ese mismo overlay).
pub fn run_query(src: &impl DataSource, stmt: &SelectStmt, params: &[Value]) -> Result<SelectOut> {
    run_select(&ViewSource::new(src), stmt, params)
}

pub fn run_select(src: &impl DataSource, stmt: &SelectStmt, params: &[Value]) -> Result<SelectOut> {
    // `WITH …`: materializa las CTEs y resuelve el resto contra un overlay que las
    // expone como tablas. (`&dyn DataSource` para no recursar tipos sin fin.)
    if !stmt.with.is_empty() {
        return run_with_ctes(src, stmt, params);
    }
    // Tablas derivadas en FROM/JOIN: se materializan y se exponen como tablas.
    if select_has_derived(stmt) {
        return run_with_derived(src, stmt, params);
    }
    // Subconsultas NO correlacionadas: se ejecutan una vez y se sustituyen por su
    // valor (escalar → literal, IN → lista, EXISTS → bool) antes de evaluar nada.
    // Las correlacionadas (referencian la FROM externa) se dejan para resolución por
    // fila en los bucles de WHERE/proyección.
    let resolved;
    let stmt = if select_has_subquery(stmt) {
        resolved = resolve_select_subqueries(src, stmt, params, &outer_qualifiers(stmt))?;
        &resolved
    } else {
        stmt
    };
    // `UNION [ALL]`: combina varios núcleos; las cláusulas finales del líder
    // (ORDER BY/LIMIT) aplican al conjunto.
    if !stmt.compound.is_empty() {
        return run_compound(src, stmt, params);
    }
    // `SELECT <expr>, …` sin `FROM`: no hay tabla que escanear.
    let Some(from_ref) = &stmt.from else {
        return run_select_no_from(stmt, params);
    };
    let from_def = lookup(src, from_ref)?;
    let mut schema = QuerySchema::single(from_ref.qualifier(), from_def.clone());

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

    // Pushdown determinista: empuja los conjuntos del WHERE que referencian una
    // sola tabla (calificada) a su scan, antes del nested-loop — reduce las
    // entradas del join. El WHERE completo se re-aplica después (corrección).
    // Seguro: FROM (siempre preservada) y lados de INNER JOIN; nunca el lado
    // derecho de un LEFT JOIN.
    let pushdown: Vec<(Expr, Option<String>)> = if !stmt.joins.is_empty() {
        stmt.where_clause
            .as_ref()
            .map(|w| {
                let mut split = Vec::new();
                split_and(w, &mut split);
                split
                    .into_iter()
                    .map(|c| {
                        let q = conjunct_qualifier(&c);
                        (c, q)
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    if !pushdown.is_empty() {
        rows = prefilter(rows, &from_def, from_ref.qualifier(), &pushdown, params)?;
    }

    // Joins nested-loop, materializando de izquierda a derecha.
    for join in &stmt.joins {
        let right_def = lookup(src, &join.table)?;
        let right_rows = src.scan_rows(&right_def)?;
        // Pushdown solo a INNER (el lado derecho de un LEFT no se puede pre-filtrar).
        let right_rows = if join.kind == JoinKind::Inner && !pushdown.is_empty() {
            prefilter(
                right_rows,
                &right_def,
                join.table.qualifier(),
                &pushdown,
                params,
            )?
        } else {
            right_rows
        };
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
        // Subconsultas correlacionadas (las dejó el pre-pass): se resuelven por fila.
        let correlated = expr_has_subquery(cond);
        let mut kept = Vec::with_capacity(rows.len());
        for row in rows {
            let resolved;
            let cond = if correlated {
                resolved = resolve_correlated_expr(src, cond, &schema, &row, params)?;
                &resolved
            } else {
                cond
            };
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
        .any(|i| matches!(i, SelectItem::Expr { expr, .. } if expr.has_aggregate()))
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

    // Sin DISTINCT, LIMIT/OFFSET se aplica ya (cada fila de entrada da una de
    // salida). Con DISTINCT la proyección cambia la cardinalidad ⇒ se proyecta
    // todo, se deduplica y se recorta DESPUÉS.
    let rows = if stmt.distinct {
        rows
    } else {
        limit_offset(rows, stmt, params)?
    };

    // Mapa de la expansión de `*`: posición física (en la fila combinada) en orden
    // LÓGICO de presentación, tabla a tabla. Con orden lógico identidad en todas
    // las tablas es la identidad — y entonces el camino rápido (mover la fila tal
    // cual, sin permutar) se conserva.
    let mut star_map: Vec<usize> = Vec::new();
    let mut base = 0;
    for t in &schema.tables {
        for &phys in &t.def.logical_order {
            if t.def.columns[phys].dropped {
                continue; // columna borrada lógicamente: no aparece en `*`
            }
            star_map.push(base + phys);
        }
        base += t.def.columns.len();
    }
    let star_identity = star_map.iter().enumerate().all(|(i, &p)| i == p);

    // Proyección.
    let mut columns = Vec::new();
    let mut projections: Vec<Option<&Expr>> = Vec::new(); // None = toda la fila (Star)
    for (i, item) in stmt.projection.iter().enumerate() {
        match item {
            SelectItem::Star => {
                for t in &schema.tables {
                    for &phys in &t.def.logical_order {
                        if t.def.columns[phys].dropped {
                            continue;
                        }
                        columns.push(t.def.columns[phys].name.clone());
                    }
                }
                projections.push(None);
            }
            SelectItem::Expr { expr: e, alias } => {
                validate_columns(e, &schema)?;
                columns.push(alias.clone().unwrap_or_else(|| match e {
                    Expr::Column { name, .. } => name.clone(),
                    _ => format!("col{}", i + 1),
                }));
                projections.push(Some(e));
            }
        }
    }
    // `SELECT *` (única proyección que copia la fila combinada entera): la salida
    // ES la entrada, así que se mueve sin re-clonar valor a valor (camino canónico
    // del full scan). Con DISTINCT hay que materializar para deduplicar, así que se
    // salta este atajo.
    if !stmt.distinct && star_identity && matches!(projections.as_slice(), [None]) {
        return Ok(SelectOut { columns, rows });
    }
    let mut out_rows = Vec::with_capacity(rows.len());
    for row in &rows {
        let mut out = Vec::with_capacity(columns.len());
        for proj in &projections {
            match proj {
                None => out.extend(star_map.iter().map(|&i| row[i].clone())),
                // Subconsulta escalar correlacionada en la proyección: por fila.
                Some(e) if expr_has_subquery(e) => {
                    let r = resolve_correlated_expr(src, e, &schema, row, params)?;
                    out.push(eval(&r, Some((&schema, row)), params)?);
                }
                Some(e) => out.push(eval(e, Some((&schema, row)), params)?),
            }
        }
        out_rows.push(out);
    }
    // DISTINCT: deduplica conservando el orden (la primera ocurrencia), y recorta
    // ya sobre el resultado deduplicado.
    if stmt.distinct {
        out_rows = dedup_preserving(out_rows);
        out_rows = limit_offset(out_rows, stmt, params)?;
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

// --- pushdown determinista de predicados a las tablas del JOIN ---

/// Separa una conjunción `a AND b AND c` en sus conjuntos.
fn split_and(e: &Expr, out: &mut Vec<Expr>) {
    if let Expr::Binary(a, BinOp::And, b) = e {
        split_and(a, out);
        split_and(b, out);
    } else {
        out.push(e.clone());
    }
}

/// Si un conjunto referencia columnas de **una sola** tabla, todas **calificadas**
/// con el mismo qualifier, lo devuelve; si no (sin columnas, sin calificar, varias
/// tablas, o agregado/subconsulta), `None` — y entonces no se empuja.
fn conjunct_qualifier(e: &Expr) -> Option<String> {
    fn walk(e: &Expr, out: &mut Vec<String>, ok: &mut bool) {
        match e {
            Expr::Column { table: Some(q), .. } => out.push(q.clone()),
            Expr::Column { table: None, .. } => *ok = false,
            Expr::Literal(_) | Expr::Param(_) => {}
            Expr::Unary(_, x) | Expr::Cast { expr: x, .. } | Expr::IsNull { expr: x, .. } => {
                walk(x, out, ok)
            }
            Expr::Binary(a, _, b) => {
                walk(a, out, ok);
                walk(b, out, ok);
            }
            Expr::Like { expr, pattern, .. } => {
                walk(expr, out, ok);
                walk(pattern, out, ok);
            }
            Expr::Function { args, .. } => args.iter().for_each(|a| walk(a, out, ok)),
            Expr::In { expr, list, .. } => {
                walk(expr, out, ok);
                list.iter().for_each(|x| walk(x, out, ok));
            }
            Expr::Case {
                operand,
                whens,
                else_,
            } => {
                if let Some(o) = operand {
                    walk(o, out, ok);
                }
                whens.iter().for_each(|(c, r)| {
                    walk(c, out, ok);
                    walk(r, out, ok);
                });
                if let Some(x) = else_ {
                    walk(x, out, ok);
                }
            }
            // Agregados/subconsultas: no se empujan.
            _ => *ok = false,
        }
    }
    let mut quals = Vec::new();
    let mut ok = true;
    walk(e, &mut quals, &mut ok);
    let first = quals.first()?.clone();
    if ok && quals.iter().all(|q| *q == first) {
        Some(first)
    } else {
        None
    }
}

/// Filtra `rows` de la tabla `def` (qualifier `q`) por los conjuntos del pushdown
/// que le corresponden. El conjunto se evalúa contra un esquema de una sola tabla.
fn prefilter(
    rows: Vec<Vec<Value>>,
    def: &TableDef,
    q: &str,
    pushdown: &[(Expr, Option<String>)],
    params: &[Value],
) -> Result<Vec<Vec<Value>>> {
    let mine: Vec<&Expr> = pushdown
        .iter()
        .filter(|(_, qual)| qual.as_deref() == Some(q))
        .map(|(e, _)| e)
        .collect();
    if mine.is_empty() {
        return Ok(rows);
    }
    let schema = QuerySchema::single(q, def.clone());
    let mut kept = Vec::with_capacity(rows.len());
    for row in rows {
        let mut pass = true;
        for c in &mine {
            if !truthy(eval(c, Some((&schema, &row)), params)?)? {
                pass = false;
                break;
            }
        }
        if pass {
            kept.push(row);
        }
    }
    Ok(kept)
}

// --- CTEs (`WITH`): tablas con nombre materializadas, expuestas vía overlay ---

/// Una CTE materializada: una `TableDef` sintética y sus filas en memoria.
struct CteEntry {
    name: String,
    def: TableDef,
    rows: Vec<Vec<Value>>,
}

/// `DataSource` que superpone CTEs materializadas sobre otra fuente. Una CTE
/// **tapa** a una tabla real del mismo nombre (como en SQL). Usa `&dyn` para el
/// interior para que anidar `WITH` no genere tipos sin fin en la monomorfización.
struct CteSource<'a> {
    inner: &'a dyn DataSource,
    ctes: Vec<CteEntry>,
}

impl DataSource for CteSource<'_> {
    fn table(&self, name: &str) -> Result<Option<TableDef>> {
        match self.ctes.iter().find(|e| e.name == name) {
            Some(e) => Ok(Some(e.def.clone())),
            None => self.inner.table(name),
        }
    }
    fn get_row(&self, table: &TableDef, rowid: i64) -> Result<Option<Vec<Value>>> {
        // Las CTEs no tienen alias de rowid ⇒ el planificador nunca llega aquí por
        // ellas; se delega por seguridad.
        if self.ctes.iter().any(|e| e.def.table_id == table.table_id) {
            return Ok(None);
        }
        self.inner.get_row(table, rowid)
    }
    fn scan_rows(&self, table: &TableDef) -> Result<Vec<Vec<Value>>> {
        match self.ctes.iter().find(|e| e.def.table_id == table.table_id) {
            Some(e) => Ok(e.rows.clone()),
            None => self.inner.scan_rows(table),
        }
    }
    // Las CTEs no tienen índices (def sin `indexes`) ⇒ el planificador no las usa
    // por aquí; se delega siempre.
    fn index_lookup(&self, table: &TableDef, idx: &IndexDef, values: &[Value]) -> Result<Vec<i64>> {
        self.inner.index_lookup(table, idx, values)
    }
    fn index_range(
        &self,
        table: &TableDef,
        idx: &IndexDef,
        lo: Option<(&Value, bool)>,
        hi: Option<(&Value, bool)>,
    ) -> Result<Vec<i64>> {
        self.inner.index_range(table, idx, lo, hi)
    }
    fn view(&self, name: &str) -> Result<Option<String>> {
        self.inner.view(name)
    }
}

// --- vistas (`CREATE VIEW`): SELECT con nombre, materializado bajo demanda ---

/// Una vista materializada y cacheada: `TableDef` sintética + sus filas.
struct MatView {
    def: TableDef,
    rows: Vec<Vec<Value>>,
}

/// `DataSource` que resuelve **vistas**: si un nombre no es tabla base pero sí una
/// vista (catálogo), parsea su SELECT, lo ejecuta contra sí mismo (así una vista
/// sobre otra vista funciona) y lo cachea. Va envuelto sobre el snapshot/tx en la
/// API, así que toda consulta lo ve. `&dyn` interior ⇒ sin recursión de tipos.
struct ViewSource<'a> {
    inner: &'a dyn DataSource,
    /// Vistas ya materializadas, por nombre. `RefCell` porque `table()` (que toma
    /// `&self`) puede tener que materializar.
    cache: std::cell::RefCell<std::collections::HashMap<String, MatView>>,
    /// Vistas en curso de materialización: guarda contra ciclos (`v` usa `v`).
    resolving: std::cell::RefCell<std::collections::HashSet<String>>,
    /// Contador para el `table_id` sintético (rango alto, disjunto de tablas reales
    /// y de los de CTE).
    next_id: std::cell::Cell<u32>,
}

impl<'a> ViewSource<'a> {
    fn new(inner: &'a dyn DataSource) -> ViewSource<'a> {
        ViewSource {
            inner,
            cache: std::cell::RefCell::new(std::collections::HashMap::new()),
            resolving: std::cell::RefCell::new(std::collections::HashSet::new()),
            next_id: std::cell::Cell::new(0xFFFF_0000),
        }
    }

    /// Materializa una vista (parsea su SELECT, lo ejecuta, cachea) y devuelve su
    /// `TableDef` sintética.
    fn materialize(&self, name: &str, sql: &str) -> Result<TableDef> {
        if !self.resolving.borrow_mut().insert(name.to_string()) {
            return Err(sql_err(format!("vista recursiva: {name}")));
        }
        let stmt = match crate::sql::parse(sql) {
            Ok(Stmt::Select(s)) => s,
            Ok(_) => {
                self.resolving.borrow_mut().remove(name);
                return Err(sql_err(format!("la vista «{name}» no define un SELECT")));
            }
            Err(e) => {
                self.resolving.borrow_mut().remove(name);
                return Err(e);
            }
        };
        let out = run_select(self, &stmt, &[]);
        self.resolving.borrow_mut().remove(name);
        let out = out?;
        let id = self.next_id.get();
        self.next_id.set(id - 1);
        let def = synthetic_cte_def(name, &out.columns, 0);
        // `synthetic_cte_def` usa `u32::MAX - idx`; aquí fijamos un id propio del
        // rango de vistas para no chocar con CTEs.
        let def = TableDef {
            table_id: id,
            ..def
        };
        self.cache.borrow_mut().insert(
            name.to_string(),
            MatView {
                def: def.clone(),
                rows: out.rows,
            },
        );
        Ok(def)
    }
}

impl DataSource for ViewSource<'_> {
    fn table(&self, name: &str) -> Result<Option<TableDef>> {
        // Tabla base primero (una tabla tapa a una vista homónima no debería pasar:
        // el catálogo lo impide al crear, pero por si acaso).
        if let Some(def) = self.inner.table(name)? {
            return Ok(Some(def));
        }
        if let Some(v) = self.cache.borrow().get(name) {
            return Ok(Some(v.def.clone()));
        }
        match self.inner.view(name)? {
            Some(sql) => Ok(Some(self.materialize(name, &sql)?)),
            None => Ok(None),
        }
    }
    fn get_row(&self, table: &TableDef, rowid: i64) -> Result<Option<Vec<Value>>> {
        if self
            .cache
            .borrow()
            .values()
            .any(|v| v.def.table_id == table.table_id)
        {
            return Ok(None); // las vistas no tienen alias de rowid
        }
        self.inner.get_row(table, rowid)
    }
    fn scan_rows(&self, table: &TableDef) -> Result<Vec<Vec<Value>>> {
        if let Some(v) = self
            .cache
            .borrow()
            .values()
            .find(|v| v.def.table_id == table.table_id)
        {
            return Ok(v.rows.clone());
        }
        self.inner.scan_rows(table)
    }
    fn index_lookup(&self, table: &TableDef, idx: &IndexDef, values: &[Value]) -> Result<Vec<i64>> {
        self.inner.index_lookup(table, idx, values)
    }
    fn index_range(
        &self,
        table: &TableDef,
        idx: &IndexDef,
        lo: Option<(&Value, bool)>,
        hi: Option<(&Value, bool)>,
    ) -> Result<Vec<i64>> {
        self.inner.index_range(table, idx, lo, hi)
    }
    fn view(&self, name: &str) -> Result<Option<String>> {
        self.inner.view(name)
    }
}

/// `TableDef` sintética para una CTA con esas columnas de salida. El `table_id`
/// usa el rango alto (`u32::MAX - i`) para no chocar con tablas reales (que
/// arrancan en 1). El tipo de columna no se consulta al leer, así que es relleno.
fn synthetic_cte_def(name: &str, columns: &[String], idx: usize) -> TableDef {
    let cols: Vec<ColumnDef> = columns
        .iter()
        .map(|c| ColumnDef {
            name: c.clone(),
            col_type: ColType::Integer, // relleno: no se consulta en lectura
            not_null: false,
            default: None,
            dropped: false,
        })
        .collect();
    let n = cols.len();
    TableDef {
        name: name.to_string(),
        table_id: u32::MAX - idx as u32,
        rowid_alias: None,
        columns: cols,
        indexes: Vec::new(),
        logical_order: (0..n).collect(),
        foreign_keys: Vec::new(),
    }
}

/// Materializa las CTEs en orden (cada una ve las anteriores) y ejecuta el SELECT
/// principal contra el overlay. No recursivo (v1).
fn run_with_ctes(src: &dyn DataSource, stmt: &SelectStmt, params: &[Value]) -> Result<SelectOut> {
    let mut overlay = CteSource {
        inner: src,
        ctes: Vec::new(),
    };
    for (i, cte) in stmt.with.iter().enumerate() {
        if cte_is_recursive(cte) {
            materialize_recursive(&mut overlay, cte, i, params)?;
        } else {
            let out = run_select(&overlay, &cte.query, params)?;
            let def = synthetic_cte_def(&cte.name, &out.columns, i);
            overlay.ctes.push(CteEntry {
                name: cte.name.clone(),
                def,
                rows: out.rows,
            });
        }
    }
    let mut main = stmt.clone();
    main.with = Vec::new();
    run_select(&overlay, &main, params)
}

/// `true` si el cuerpo de la CTE se referencia a sí mismo (en una FROM/JOIN de la
/// query o de sus núcleos compuestos) ⇒ recursiva.
fn cte_is_recursive(cte: &Cte) -> bool {
    fn refs(stmt: &SelectStmt, name: &str) -> bool {
        let hit = |t: &TableRef| t.subquery.is_none() && t.name == name;
        stmt.from.as_ref().is_some_and(hit)
            || stmt.joins.iter().any(|j| hit(&j.table))
            || stmt.compound.iter().any(|c| refs(&c.select, name))
    }
    refs(&cte.query, &cte.name)
}

/// Materializa una CTE recursiva por **punto fijo**: ejecuta la semilla, y luego
/// repite el término recursivo (que ve la CTE = las filas del paso anterior) hasta
/// que no salen filas nuevas. Forma soportada (v1): `semilla UNION [ALL] término`.
fn materialize_recursive(
    overlay: &mut CteSource,
    cte: &Cte,
    idx: usize,
    params: &[Value],
) -> Result<()> {
    if cte.query.compound.len() != 1 {
        return Err(sql_err(
            "CTE recursiva: usa exactamente «semilla UNION [ALL] término»",
        ));
    }
    let union_all = matches!(cte.query.compound[0].op, SetOp::UnionAll);
    let recursive = cte.query.compound[0].select.clone();
    // Semilla = el cuerpo sin el término recursivo y sin ORDER BY/LIMIT (esos van
    // al SELECT externo, no a la recursión).
    let mut seed_stmt = cte.query.clone();
    seed_stmt.compound = Vec::new();
    seed_stmt.order_by = Vec::new();
    seed_stmt.limit = None;
    seed_stmt.offset = None;

    let seed = run_select(&*overlay, &seed_stmt, params)?;
    let ncols = seed.columns.len();
    let def = synthetic_cte_def(&cte.name, &seed.columns, idx);
    let mut all_rows = seed.rows.clone();
    overlay.ctes.push(CteEntry {
        name: cte.name.clone(),
        def,
        rows: seed.rows,
    });
    let entry = overlay.ctes.len() - 1;
    let mut working = all_rows.clone();

    const MAX_ROWS: usize = 1_000_000; // guarda contra recursión sin caso base
    loop {
        overlay.ctes[entry].rows = working; // el término ve la CTE = el paso anterior
        let step = run_select(&*overlay, &recursive, params)?;
        if step.columns.len() != ncols {
            return Err(sql_err(
                "CTE recursiva: el término tiene distinto número de columnas que la semilla",
            ));
        }
        let mut fresh = step.rows;
        if !union_all {
            fresh = dedup_preserving(fresh)
                .into_iter()
                .filter(|r| !all_rows.contains(r))
                .collect();
        }
        if fresh.is_empty() {
            break;
        }
        all_rows.extend(fresh.clone());
        if all_rows.len() > MAX_ROWS {
            return Err(sql_err(
                "CTE recursiva: demasiadas filas (¿recursión sin caso base?)",
            ));
        }
        working = fresh;
    }
    overlay.ctes[entry].rows = all_rows; // la CTE final = todo lo acumulado
    Ok(())
}

// --- tablas derivadas: `FROM (SELECT …) AS x` (CTE anónima) ---

/// `true` si la FROM o algún JOIN del SELECT es una tabla derivada.
fn select_has_derived(stmt: &SelectStmt) -> bool {
    stmt.from.as_ref().is_some_and(|t| t.subquery.is_some())
        || stmt.joins.iter().any(|j| j.table.subquery.is_some())
}

/// Materializa las tablas derivadas (cada subconsulta del FROM/JOIN, contra `src`
/// — sin LATERAL, no se ven entre sí) en un overlay y reescribe sus `TableRef` a
/// nombres planos (su alias). Usa un rango de `table_id` distinto del de CTEs.
fn run_with_derived(
    src: &dyn DataSource,
    stmt: &SelectStmt,
    params: &[Value],
) -> Result<SelectOut> {
    // Overlay base SIN derivadas: se comporta igual que `src` pero es `Sized`, así
    // que `run_select` (genérico sobre tipos `Sized`) lo acepta. Cada derivada se
    // materializa contra él ⇒ no se ven entre sí (sin LATERAL).
    let base = CteSource {
        inner: src,
        ctes: Vec::new(),
    };
    let mut entries: Vec<CteEntry> = Vec::new();
    let mut rewritten = stmt.clone();

    let materialize = |tr: &mut TableRef, entries: &mut Vec<CteEntry>| -> Result<()> {
        let Some(sub) = tr.subquery.take() else {
            return Ok(());
        };
        let out = run_select(&base, &sub, params)?;
        let alias = tr.alias.clone().expect("una derivada siempre lleva alias");
        let mut def = synthetic_cte_def(&alias, &out.columns, 0);
        def.table_id = 0xFFFE_0000 - entries.len() as u32; // rango propio (≠ CTE/vista)
        tr.name = alias.clone(); // el overlay la resuelve por su alias
        entries.push(CteEntry {
            name: alias,
            def,
            rows: out.rows,
        });
        Ok(())
    };

    if let Some(from) = rewritten.from.as_mut() {
        materialize(from, &mut entries)?;
    }
    for j in rewritten.joins.iter_mut() {
        materialize(&mut j.table, &mut entries)?;
    }

    let overlay = CteSource {
        inner: src,
        ctes: entries,
    };
    run_select(&overlay, &rewritten, params)
}

// --- subconsultas no correlacionadas (pre-pasada) ---

/// `true` si alguna expresión **propia** del SELECT (no de sus núcleos `UNION`,
/// que se resuelven aparte) lleva una subconsulta.
fn select_has_subquery(stmt: &SelectStmt) -> bool {
    let in_item = |it: &SelectItem| match it {
        SelectItem::Expr { expr, .. } => expr_has_subquery(expr),
        SelectItem::Star => false,
    };
    stmt.where_clause.as_ref().is_some_and(expr_has_subquery)
        || stmt.projection.iter().any(in_item)
        || stmt.having.as_ref().is_some_and(expr_has_subquery)
        || stmt.joins.iter().any(|j| expr_has_subquery(&j.on))
        || stmt.group_by.iter().any(expr_has_subquery)
}

fn expr_has_subquery(e: &Expr) -> bool {
    match e {
        Expr::ScalarSubquery(_) | Expr::Exists(_) | Expr::InSubquery { .. } => true,
        Expr::Literal(_) | Expr::Column { .. } | Expr::Param(_) => false,
        Expr::Unary(_, x) => expr_has_subquery(x),
        Expr::Binary(a, _, b) => expr_has_subquery(a) || expr_has_subquery(b),
        Expr::IsNull { expr, .. } => expr_has_subquery(expr),
        Expr::Like { expr, pattern, .. } => expr_has_subquery(expr) || expr_has_subquery(pattern),
        Expr::Aggregate { arg, sep, .. } => {
            arg.as_deref().is_some_and(expr_has_subquery)
                || sep.as_deref().is_some_and(expr_has_subquery)
        }
        Expr::Function { args, .. } => args.iter().any(expr_has_subquery),
        Expr::In { expr, list, .. } => {
            expr_has_subquery(expr) || list.iter().any(expr_has_subquery)
        }
        Expr::Cast { expr, .. } => expr_has_subquery(expr),
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            operand.as_deref().is_some_and(expr_has_subquery)
                || whens
                    .iter()
                    .any(|(c, r)| expr_has_subquery(c) || expr_has_subquery(r))
                || else_.as_deref().is_some_and(expr_has_subquery)
        }
    }
}

/// Devuelve una copia del SELECT con sus subconsultas (propias) sustituidas por su
/// valor. Los núcleos `UNION` no se tocan aquí: cada uno se resuelve cuando
/// `run_compound` lo pasa por `run_select`.
fn resolve_select_subqueries(
    src: &impl DataSource,
    stmt: &SelectStmt,
    params: &[Value],
    outer: &HashSet<String>,
) -> Result<SelectStmt> {
    let mut s = stmt.clone();
    if let Some(w) = s.where_clause.take() {
        s.where_clause = Some(resolve_expr(src, &w, params, outer)?);
    }
    if let Some(h) = s.having.take() {
        s.having = Some(resolve_expr(src, &h, params, outer)?);
    }
    for item in &mut s.projection {
        if let SelectItem::Expr { expr, .. } = item {
            let r = resolve_expr(src, expr, params, outer)?;
            *expr = r;
        }
    }
    for j in &mut s.joins {
        let on = resolve_expr(src, &j.on, params, outer)?;
        j.on = on;
    }
    for g in &mut s.group_by {
        let r = resolve_expr(src, g, params, outer)?;
        *g = r;
    }
    Ok(s)
}

/// Qualifiers (alias o nombre) de las tablas de la FROM/JOINs del SELECT — el
/// "ámbito externo" frente al que una subconsulta es correlacionada.
fn outer_qualifiers(stmt: &SelectStmt) -> HashSet<String> {
    let mut q = HashSet::new();
    if let Some(t) = &stmt.from {
        q.insert(t.qualifier().to_string());
    }
    for j in &stmt.joins {
        q.insert(j.table.qualifier().to_string());
    }
    q
}

/// `true` si `e` referencia alguna columna calificada con un qualifier de `quals`.
fn expr_refs_quals(e: &Expr, quals: &HashSet<String>) -> bool {
    match e {
        Expr::Column { table: Some(q), .. } => quals.contains(q),
        Expr::Column { .. } | Expr::Literal(_) | Expr::Param(_) => false,
        Expr::Unary(_, x) | Expr::Cast { expr: x, .. } | Expr::IsNull { expr: x, .. } => {
            expr_refs_quals(x, quals)
        }
        Expr::Binary(a, _, b) => expr_refs_quals(a, quals) || expr_refs_quals(b, quals),
        Expr::Like { expr, pattern, .. } => {
            expr_refs_quals(expr, quals) || expr_refs_quals(pattern, quals)
        }
        Expr::Aggregate { arg, sep, .. } => {
            arg.as_deref().is_some_and(|x| expr_refs_quals(x, quals))
                || sep.as_deref().is_some_and(|x| expr_refs_quals(x, quals))
        }
        Expr::Function { args, .. } => args.iter().any(|x| expr_refs_quals(x, quals)),
        Expr::In { expr, list, .. } => {
            expr_refs_quals(expr, quals) || list.iter().any(|x| expr_refs_quals(x, quals))
        }
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            operand
                .as_deref()
                .is_some_and(|x| expr_refs_quals(x, quals))
                || whens
                    .iter()
                    .any(|(c, r)| expr_refs_quals(c, quals) || expr_refs_quals(r, quals))
                || else_.as_deref().is_some_and(|x| expr_refs_quals(x, quals))
        }
        Expr::ScalarSubquery(q) | Expr::Exists(q) => stmt_refs_quals(q, quals),
        Expr::InSubquery { expr, query, .. } => {
            expr_refs_quals(expr, quals) || stmt_refs_quals(query, quals)
        }
    }
}

/// `true` si alguna expresión del SELECT referencia un qualifier externo (⇒ la
/// subconsulta es correlacionada respecto a ese ámbito).
fn stmt_refs_quals(stmt: &SelectStmt, quals: &HashSet<String>) -> bool {
    stmt.where_clause
        .as_ref()
        .is_some_and(|e| expr_refs_quals(e, quals))
        || stmt
            .having
            .as_ref()
            .is_some_and(|e| expr_refs_quals(e, quals))
        || stmt
            .projection
            .iter()
            .any(|it| matches!(it, SelectItem::Expr { expr, .. } if expr_refs_quals(expr, quals)))
        || stmt.joins.iter().any(|j| expr_refs_quals(&j.on, quals))
        || stmt.group_by.iter().any(|e| expr_refs_quals(e, quals))
        || stmt
            .compound
            .iter()
            .any(|c| stmt_refs_quals(&c.select, quals))
}

/// Una subconsulta escalar `(SELECT …)` → su único valor (0 filas → NULL, >1 →
/// error; exactamente una columna).
fn eval_scalar_subquery(src: &impl DataSource, q: &SelectStmt, params: &[Value]) -> Result<Value> {
    let out = run_select(src, q, params)?;
    if out.columns.len() != 1 {
        return Err(sql_err(
            "una subconsulta escalar debe devolver exactamente una columna",
        ));
    }
    match out.rows.len() {
        0 => Ok(Value::Null),
        1 => Ok(out.rows.into_iter().next().expect("1 fila").remove(0)),
        _ => Err(sql_err("una subconsulta escalar devolvió más de una fila")),
    }
}

/// Reescribe un `Expr` sustituyendo cada subconsulta por su resultado. Las
/// subconsultas se ejecutan **una vez**, sin la fila exterior: v1 no soporta
/// correlación (una referencia a una columna externa, o falla al resolverse, o
/// —si coincide el nombre— se resuelve dentro de la subconsulta).
fn resolve_expr(
    src: &impl DataSource,
    e: &Expr,
    params: &[Value],
    outer: &HashSet<String>,
) -> Result<Expr> {
    let res = |x: &Expr| resolve_expr(src, x, params, outer);
    let boxed = |x: &Expr| res(x).map(Box::new);
    let opt = |x: &Option<Box<Expr>>| -> Result<Option<Box<Expr>>> {
        x.as_deref().map(boxed).transpose()
    };
    Ok(match e {
        // Correlacionada (referencia una tabla externa) ⇒ se deja para la resolución
        // POR FILA (WHERE/proyección). Si no, se resuelve aquí una sola vez.
        Expr::ScalarSubquery(q) => {
            if stmt_refs_quals(q, outer) {
                e.clone()
            } else {
                Expr::Literal(eval_scalar_subquery(src, q, params)?)
            }
        }
        Expr::Exists(q) => {
            if stmt_refs_quals(q, outer) {
                e.clone()
            } else {
                Expr::Literal(Value::Bool(!run_select(src, q, params)?.rows.is_empty()))
            }
        }
        Expr::InSubquery {
            expr,
            query,
            negated,
        } => {
            if stmt_refs_quals(query, outer) {
                Expr::InSubquery {
                    expr: boxed(expr)?,
                    query: query.clone(),
                    negated: *negated,
                }
            } else {
                let out = run_select(src, query, params)?;
                if out.columns.len() != 1 {
                    return Err(sql_err("IN (SELECT …) requiere exactamente una columna"));
                }
                let list = out
                    .rows
                    .into_iter()
                    .map(|mut r| Expr::Literal(r.remove(0)))
                    .collect();
                Expr::In {
                    expr: boxed(expr)?,
                    list,
                    negated: *negated,
                }
            }
        }
        Expr::Literal(_) | Expr::Column { .. } | Expr::Param(_) => e.clone(),
        Expr::Unary(op, x) => Expr::Unary(*op, boxed(x)?),
        Expr::Binary(a, op, b) => Expr::Binary(boxed(a)?, *op, boxed(b)?),
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: boxed(expr)?,
            negated: *negated,
        },
        Expr::Like {
            expr,
            pattern,
            negated,
        } => Expr::Like {
            expr: boxed(expr)?,
            pattern: boxed(pattern)?,
            negated: *negated,
        },
        Expr::Aggregate {
            func,
            arg,
            distinct,
            sep,
        } => Expr::Aggregate {
            func: *func,
            arg: opt(arg)?,
            distinct: *distinct,
            sep: opt(sep)?,
        },
        Expr::Function { name, args } => Expr::Function {
            name: name.clone(),
            args: args.iter().map(res).collect::<Result<_>>()?,
        },
        Expr::In {
            expr,
            list,
            negated,
        } => Expr::In {
            expr: boxed(expr)?,
            list: list.iter().map(res).collect::<Result<_>>()?,
            negated: *negated,
        },
        Expr::Cast { expr, to } => Expr::Cast {
            expr: boxed(expr)?,
            to: *to,
        },
        Expr::Case {
            operand,
            whens,
            else_,
        } => Expr::Case {
            operand: opt(operand)?,
            whens: whens
                .iter()
                .map(|(c, r)| Ok((res(c)?, res(r)?)))
                .collect::<Result<_>>()?,
            else_: opt(else_)?,
        },
    })
}

// --- subconsultas correlacionadas: resolución POR FILA ---

/// Sustituye en `e` (y dentro de los cuerpos de sus subconsultas) las columnas que
/// resuelven en el esquema EXTERNO por su valor en `row`. Las que no resuelven
/// (columnas propias de la subconsulta) se dejan.
fn subst_outer_expr(e: &Expr, schema: &QuerySchema, row: &[Value]) -> Result<Expr> {
    let r = |x: &Expr| subst_outer_expr(x, schema, row);
    let b = |x: &Expr| r(x).map(Box::new);
    let opt =
        |x: &Option<Box<Expr>>| -> Result<Option<Box<Expr>>> { x.as_deref().map(b).transpose() };
    Ok(match e {
        Expr::Column {
            table: Some(q),
            name,
        } => match schema.resolve(Some(q), name) {
            Ok(i) => Expr::Literal(row[i].clone()), // columna externa → literal
            Err(_) => e.clone(),                    // propia de la subconsulta
        },
        Expr::Literal(_) | Expr::Column { .. } | Expr::Param(_) => e.clone(),
        Expr::Unary(op, x) => Expr::Unary(*op, b(x)?),
        Expr::Binary(a, op, c) => Expr::Binary(b(a)?, *op, b(c)?),
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: b(expr)?,
            negated: *negated,
        },
        Expr::Like {
            expr,
            pattern,
            negated,
        } => Expr::Like {
            expr: b(expr)?,
            pattern: b(pattern)?,
            negated: *negated,
        },
        Expr::Aggregate {
            func,
            arg,
            distinct,
            sep,
        } => Expr::Aggregate {
            func: *func,
            arg: opt(arg)?,
            distinct: *distinct,
            sep: opt(sep)?,
        },
        Expr::Function { name, args } => Expr::Function {
            name: name.clone(),
            args: args.iter().map(r).collect::<Result<_>>()?,
        },
        Expr::In {
            expr,
            list,
            negated,
        } => Expr::In {
            expr: b(expr)?,
            list: list.iter().map(r).collect::<Result<_>>()?,
            negated: *negated,
        },
        Expr::Cast { expr, to } => Expr::Cast {
            expr: b(expr)?,
            to: *to,
        },
        Expr::Case {
            operand,
            whens,
            else_,
        } => Expr::Case {
            operand: opt(operand)?,
            whens: whens
                .iter()
                .map(|(c, rr)| Ok((r(c)?, r(rr)?)))
                .collect::<Result<_>>()?,
            else_: opt(else_)?,
        },
        Expr::ScalarSubquery(q) => {
            Expr::ScalarSubquery(Box::new(subst_outer_stmt(q, schema, row)?))
        }
        Expr::Exists(q) => Expr::Exists(Box::new(subst_outer_stmt(q, schema, row)?)),
        Expr::InSubquery {
            expr,
            query,
            negated,
        } => Expr::InSubquery {
            expr: b(expr)?,
            query: Box::new(subst_outer_stmt(query, schema, row)?),
            negated: *negated,
        },
    })
}

/// `subst_outer_expr` sobre todas las expresiones de un SELECT (cuerpo de una
/// subconsulta correlacionada).
fn subst_outer_stmt(stmt: &SelectStmt, schema: &QuerySchema, row: &[Value]) -> Result<SelectStmt> {
    let mut s = stmt.clone();
    if let Some(w) = s.where_clause.take() {
        s.where_clause = Some(subst_outer_expr(&w, schema, row)?);
    }
    if let Some(h) = s.having.take() {
        s.having = Some(subst_outer_expr(&h, schema, row)?);
    }
    for item in &mut s.projection {
        if let SelectItem::Expr { expr, .. } = item {
            *expr = subst_outer_expr(expr, schema, row)?;
        }
    }
    for j in &mut s.joins {
        j.on = subst_outer_expr(&j.on, schema, row)?;
    }
    for g in &mut s.group_by {
        *g = subst_outer_expr(g, schema, row)?;
    }
    for c in &mut s.compound {
        c.select = subst_outer_stmt(&c.select, schema, row)?;
    }
    Ok(s)
}

/// Resuelve POR FILA las subconsultas correlacionadas de `e`: sustituye las
/// columnas externas por los valores de `row` dentro de cada subconsulta y la
/// ejecuta. Las columnas de nivel superior (propias de la consulta) se dejan para
/// `eval`.
fn resolve_correlated_expr(
    src: &impl DataSource,
    e: &Expr,
    schema: &QuerySchema,
    row: &[Value],
    params: &[Value],
) -> Result<Expr> {
    let rec = |x: &Expr| resolve_correlated_expr(src, x, schema, row, params);
    let b = |x: &Expr| rec(x).map(Box::new);
    let opt =
        |x: &Option<Box<Expr>>| -> Result<Option<Box<Expr>>> { x.as_deref().map(b).transpose() };
    Ok(match e {
        Expr::ScalarSubquery(q) => {
            let q2 = subst_outer_stmt(q, schema, row)?;
            Expr::Literal(eval_scalar_subquery(src, &q2, params)?)
        }
        Expr::Exists(q) => {
            let q2 = subst_outer_stmt(q, schema, row)?;
            Expr::Literal(Value::Bool(!run_select(src, &q2, params)?.rows.is_empty()))
        }
        Expr::InSubquery {
            expr,
            query,
            negated,
        } => {
            let q2 = subst_outer_stmt(query, schema, row)?;
            let out = run_select(src, &q2, params)?;
            if out.columns.len() != 1 {
                return Err(sql_err("IN (SELECT …) requiere exactamente una columna"));
            }
            let list = out
                .rows
                .into_iter()
                .map(|mut r| Expr::Literal(r.remove(0)))
                .collect();
            Expr::In {
                expr: b(expr)?,
                list,
                negated: *negated,
            }
        }
        Expr::Literal(_) | Expr::Column { .. } | Expr::Param(_) => e.clone(),
        Expr::Unary(op, x) => Expr::Unary(*op, b(x)?),
        Expr::Binary(a, op, c) => Expr::Binary(b(a)?, *op, b(c)?),
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: b(expr)?,
            negated: *negated,
        },
        Expr::Like {
            expr,
            pattern,
            negated,
        } => Expr::Like {
            expr: b(expr)?,
            pattern: b(pattern)?,
            negated: *negated,
        },
        Expr::Aggregate {
            func,
            arg,
            distinct,
            sep,
        } => Expr::Aggregate {
            func: *func,
            arg: opt(arg)?,
            distinct: *distinct,
            sep: opt(sep)?,
        },
        Expr::Function { name, args } => Expr::Function {
            name: name.clone(),
            args: args.iter().map(rec).collect::<Result<_>>()?,
        },
        Expr::In {
            expr,
            list,
            negated,
        } => Expr::In {
            expr: b(expr)?,
            list: list.iter().map(rec).collect::<Result<_>>()?,
            negated: *negated,
        },
        Expr::Cast { expr, to } => Expr::Cast {
            expr: b(expr)?,
            to: *to,
        },
        Expr::Case {
            operand,
            whens,
            else_,
        } => Expr::Case {
            operand: opt(operand)?,
            whens: whens
                .iter()
                .map(|(c, r)| Ok((rec(c)?, rec(r)?)))
                .collect::<Result<_>>()?,
            else_: opt(else_)?,
        },
    })
}

/// `UNION [ALL]`: ejecuta cada núcleo, los combina por la izquierda (cada `UNION`
/// deduplica el acumulado; `UNION ALL` conserva duplicados), y aplica las
/// cláusulas finales del líder — `ORDER BY` por **columna de salida** y luego
/// `LIMIT`/`OFFSET` — al conjunto entero.
fn run_compound(src: &impl DataSource, stmt: &SelectStmt, params: &[Value]) -> Result<SelectOut> {
    // El núcleo líder es este mismo SELECT sin la cola (compound/order/limit).
    let mut core = stmt.clone();
    core.compound = Vec::new();
    core.order_by = Vec::new();
    core.limit = None;
    core.offset = None;
    let lead = run_select(src, &core, params)?;
    let columns = lead.columns;
    let mut rows = lead.rows;

    for cu in &stmt.compound {
        let part = run_select(src, &cu.select, params)?;
        if part.columns.len() != columns.len() {
            return Err(sql_err(format!(
                "operador de conjunto: los SELECT tienen distinto número de columnas ({} vs {})",
                columns.len(),
                part.columns.len()
            )));
        }
        // Combinación asociativa por la izquierda. UNION/INTERSECT/EXCEPT deduplican;
        // UNION ALL conserva duplicados.
        match cu.op {
            SetOp::UnionAll => rows.extend(part.rows),
            SetOp::Union => {
                rows.extend(part.rows);
                rows = dedup_preserving(rows);
            }
            SetOp::Intersect => {
                rows = dedup_preserving(rows)
                    .into_iter()
                    .filter(|r| part.rows.contains(r))
                    .collect();
            }
            SetOp::Except => {
                rows = dedup_preserving(rows)
                    .into_iter()
                    .filter(|r| !part.rows.contains(r))
                    .collect();
            }
        }
    }

    // ORDER BY sobre las columnas de SALIDA de la unión (por nombre).
    if !stmt.order_by.is_empty() {
        let order: Vec<(usize, bool)> = stmt
            .order_by
            .iter()
            .map(|o| {
                columns
                    .iter()
                    .position(|c| *c == o.column)
                    .map(|i| (i, o.desc))
                    .ok_or_else(|| {
                        sql_err(format!(
                            "ORDER BY en UNION: columna desconocida «{}»",
                            o.column
                        ))
                    })
            })
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
    Ok(SelectOut { columns, rows })
}

/// Plan de **streaming** para un SELECT elegible: full scan + proyección de
/// columnas simples (o `*`), sin WHERE/JOIN/GROUP BY/HAVING/ORDER BY ni
/// agregados. La capa API responde sin materializar el resultado — el coste
/// estructural del full scan. `None` = no elegible; cae al camino normal, que
/// también es quien produce los errores (columna desconocida, etc.), así que
/// ambos caminos son indistinguibles salvo en coste.
pub struct StreamSelect {
    pub def: TableDef,
    /// Nombres de columna de salida (alias o nombre tal y como se escribió).
    pub columns: Vec<String>,
    /// Índice de columna de la tabla por cada columna de salida.
    pub cols: Vec<usize>,
    pub offset: usize,
    /// `usize::MAX` sin `LIMIT`.
    pub limit: usize,
}

pub fn stream_select(
    src: &impl DataSource,
    stmt: &SelectStmt,
    params: &[Value],
) -> Result<Option<StreamSelect>> {
    let Some(from_ref) = &stmt.from else {
        return Ok(None);
    };
    if !stmt.joins.is_empty()
        || stmt.where_clause.is_some()
        || !stmt.group_by.is_empty()
        || stmt.having.is_some()
        || !stmt.order_by.is_empty()
        || stmt.distinct // el streaming no deduplica
        || !stmt.compound.is_empty() // UNION va por el camino compuesto
        || !stmt.with.is_empty()
    // las CTEs necesitan el overlay (no el src crudo)
    {
        return Ok(None);
    }
    // Si el FROM no es una tabla base (puede ser una vista, que `src` crudo no
    // resuelve), se cae al camino normal (`run_query`, con overlay de vistas).
    let Some(def) = src.table(&from_ref.name)? else {
        return Ok(None);
    };
    let qualifier = from_ref.qualifier();
    let mut columns = Vec::new();
    let mut cols = Vec::new();
    for item in &stmt.projection {
        match item {
            SelectItem::Star => {
                // Orden LÓGICO de presentación; `cols` mapea salida→posición física.
                // Las columnas borradas no aparecen.
                for &phys in &def.logical_order {
                    if def.columns[phys].dropped {
                        continue;
                    }
                    columns.push(def.columns[phys].name.clone());
                    cols.push(phys);
                }
            }
            SelectItem::Expr {
                expr: Expr::Column { table, name },
                alias,
            } => {
                if table.as_deref().is_some_and(|q| q != qualifier) {
                    return Ok(None); // calificador ajeno: que el camino normal dé su error
                }
                let Some(c) = def
                    .columns
                    .iter()
                    .position(|col| !col.dropped && col.name == *name)
                else {
                    return Ok(None); // columna desconocida (o borrada): camino normal
                };
                columns.push(alias.clone().unwrap_or_else(|| name.clone()));
                cols.push(c);
            }
            _ => return Ok(None), // expresiones/agregados: camino normal
        }
    }
    // Mismos errores y semántica que `limit_offset` (constantes, no negativos).
    let offset = match &stmt.offset {
        Some(e) => usize_const(e, params, "OFFSET")?,
        None => 0,
    };
    let limit = match &stmt.limit {
        Some(e) => usize_const(e, params, "LIMIT")?,
        None => usize::MAX,
    };
    Ok(Some(StreamSelect {
        def,
        columns,
        cols,
        offset,
        limit,
    }))
}

/// Deduplica filas conservando la primera ocurrencia (`SELECT DISTINCT`). O(n²):
/// `Value` no es `Hash` (REAL/BLOB), pero n suele ser pequeño tras los filtros.
fn dedup_preserving(rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    let mut out: Vec<Vec<Value>> = Vec::with_capacity(rows.len());
    for r in rows {
        if !out.contains(&r) {
            out.push(r);
        }
    }
    out
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
        let SelectItem::Expr { expr: e, alias } = item else {
            return Err(sql_err("'*' no se puede combinar con agregados"));
        };
        if let Some(name) = col_outside_agg(e) {
            return Err(sql_err(format!(
                "sin GROUP BY, la columna {name} debe ir dentro de un agregado"
            )));
        }
        validate_columns(e, schema)?;
        columns.push(alias.clone().unwrap_or_else(|| format!("col{}", i + 1)));
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
        Expr::Function { args, .. } => args.iter().find_map(col_outside_agg),
        Expr::In { expr, list, .. } => {
            col_outside_agg(expr).or_else(|| list.iter().find_map(col_outside_agg))
        }
        Expr::Cast { expr, .. } => col_outside_agg(expr),
        Expr::Case {
            operand,
            whens,
            else_,
        } => operand
            .as_deref()
            .and_then(col_outside_agg)
            .or_else(|| {
                whens
                    .iter()
                    .find_map(|(c, r)| col_outside_agg(c).or_else(|| col_outside_agg(r)))
            })
            .or_else(|| else_.as_deref().and_then(col_outside_agg)),
        Expr::ScalarSubquery(_) | Expr::Exists(_) => None,
        Expr::InSubquery { expr, .. } => col_outside_agg(expr),
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
        Expr::Function { args, .. } => {
            args.iter().for_each(|a| collect_columns(a, skip_agg, out));
        }
        Expr::In { expr, list, .. } => {
            collect_columns(expr, skip_agg, out);
            list.iter().for_each(|e| collect_columns(e, skip_agg, out));
        }
        Expr::Cast { expr, .. } => collect_columns(expr, skip_agg, out),
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(o) = operand {
                collect_columns(o, skip_agg, out);
            }
            for (c, r) in whens {
                collect_columns(c, skip_agg, out);
                collect_columns(r, skip_agg, out);
            }
            if let Some(e) = else_ {
                collect_columns(e, skip_agg, out);
            }
        }
        // El cuerpo de una subconsulta es otro ámbito; solo cuenta la parte externa.
        Expr::ScalarSubquery(_) | Expr::Exists(_) => {}
        Expr::InSubquery { expr, .. } => collect_columns(expr, skip_agg, out),
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
            SelectItem::Expr { expr: e, .. } => check_grouped(e)?,
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
            SelectItem::Expr { alias: Some(a), .. } => a.clone(),
            SelectItem::Expr {
                expr: Expr::Column { name, .. },
                ..
            } => name.clone(),
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
            let SelectItem::Expr { expr: e, .. } = item else {
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
        Expr::Aggregate {
            func,
            arg,
            distinct,
            sep,
        } => Expr::Literal(compute_aggregate(
            *func,
            arg.as_deref(),
            *distinct,
            sep.as_deref(),
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
        Expr::Function { name, args } => Expr::Function {
            name: name.clone(),
            args: args
                .iter()
                .map(|a| fold_aggregates(a, schema, rows, params))
                .collect::<Result<_>>()?,
        },
        Expr::In {
            expr,
            list,
            negated,
        } => Expr::In {
            expr: Box::new(fold_aggregates(expr, schema, rows, params)?),
            list: list
                .iter()
                .map(|e| fold_aggregates(e, schema, rows, params))
                .collect::<Result<_>>()?,
            negated: *negated,
        },
        Expr::Cast { expr, to } => Expr::Cast {
            expr: Box::new(fold_aggregates(expr, schema, rows, params)?),
            to: *to,
        },
        Expr::Case {
            operand,
            whens,
            else_,
        } => Expr::Case {
            operand: match operand {
                Some(o) => Some(Box::new(fold_aggregates(o, schema, rows, params)?)),
                None => None,
            },
            whens: whens
                .iter()
                .map(|(c, r)| {
                    Ok((
                        fold_aggregates(c, schema, rows, params)?,
                        fold_aggregates(r, schema, rows, params)?,
                    ))
                })
                .collect::<Result<_>>()?,
            else_: match else_ {
                Some(e) => Some(Box::new(fold_aggregates(e, schema, rows, params)?)),
                None => None,
            },
        },
        // Las subconsultas ya se resolvieron a literales; no contienen agregados
        // del SELECT exterior, así que pasan tal cual.
        Expr::ScalarSubquery(_) | Expr::Exists(_) | Expr::InSubquery { .. } => e.clone(),
    })
}

/// Los agregados ignoran NULL (estándar SQL): `COUNT(col)` cuenta no-NULL,
/// `SUM`/`AVG`/`MIN`/`MAX` sobre cero valores devuelven NULL.
fn compute_aggregate(
    func: AggFunc,
    arg: Option<&Expr>,
    distinct: bool,
    sep: Option<&Expr>,
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
    // Valores no-NULL del argumento (NULL se ignora, estándar SQL). DISTINCT
    // deduplica conservando el orden antes de plegar (Value no es Hash → O(n²),
    // pero n es el de un grupo, pequeño).
    let mut values: Vec<Value> = Vec::new();
    for row in rows {
        let v = eval(arg, Some((schema, row)), params)?;
        if !matches!(v, Value::Null) {
            values.push(v);
        }
    }
    if distinct {
        let mut seen: Vec<Value> = Vec::with_capacity(values.len());
        for v in values {
            if !seen.contains(&v) {
                seen.push(v);
            }
        }
        values = seen;
    }
    // GROUP_CONCAT no encaja en el plegado numérico: junta los TEXT con el
    // separador (constante, por defecto ","). Grupo vacío → NULL (como SQLite).
    if func == AggFunc::GroupConcat {
        if values.is_empty() {
            return Ok(Value::Null);
        }
        let sep_str = match sep {
            None => ",".to_string(),
            Some(e) => match eval_const(e, params)? {
                Value::Text(s) => s,
                Value::Null => ",".to_string(),
                v => {
                    return Err(sql_err(format!(
                        "GROUP_CONCAT: el separador debe ser TEXT, no {}",
                        v.type_name()
                    )));
                }
            },
        };
        let mut parts = Vec::with_capacity(values.len());
        for v in values {
            match v {
                Value::Text(s) => parts.push(s),
                v => {
                    return Err(sql_err(format!(
                        "GROUP_CONCAT requiere TEXT, no {} (usa CAST)",
                        v.type_name()
                    )));
                }
            }
        }
        return Ok(Value::Text(parts.join(&sep_str)));
    }
    let mut count: i64 = 0;
    let mut sum_i: i64 = 0;
    let mut sum_f: f64 = 0.0;
    let mut real_seen = false;
    let mut best: Option<Value> = None;
    for v in values {
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
            AggFunc::GroupConcat => unreachable!("GROUP_CONCAT se resuelve antes del bucle"),
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
        AggFunc::GroupConcat => unreachable!("GROUP_CONCAT se resuelve antes del bucle"),
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
                // `||`: TEXT || TEXT (NULL propaga). Sin coerción implícita
                // (filosofía del motor) — para concatenar otros tipos, `CAST`.
                BinOp::Concat => match (l, eval(right, row, params)?) {
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (Value::Text(a), Value::Text(b)) => Ok(Value::Text(a + &b)),
                    (a, b) => Err(sql_err(format!(
                        "|| requiere TEXT, no {} y {} (usa CAST)",
                        a.type_name(),
                        b.type_name()
                    ))),
                },
            }
        }
        Expr::Function { name, args } => {
            let vals: Vec<Value> = args
                .iter()
                .map(|a| eval(a, row, params))
                .collect::<Result<_>>()?;
            call_function(name, &vals)
        }
        Expr::In {
            expr,
            list,
            negated,
        } => {
            let v = eval(expr, row, params)?;
            if matches!(v, Value::Null) {
                return Ok(Value::Null); // NULL IN (…) es desconocido
            }
            let mut saw_null = false;
            let mut found = false;
            for item in list {
                let iv = eval(item, row, params)?;
                match cmp_values(&v, &iv)? {
                    Some(Ordering::Equal) => {
                        found = true;
                        break;
                    }
                    Some(_) => {}
                    None => saw_null = true, // un NULL en la lista → desconocido
                }
            }
            // Trivalente: hallado → true; no hallado con NULL → NULL; si no → false.
            // (`negated` invierte el booleano, no el NULL.)
            Ok(match (found, saw_null) {
                (true, _) => Value::Bool(!*negated),
                (false, true) => Value::Null,
                (false, false) => Value::Bool(*negated),
            })
        }
        Expr::Cast { expr, to } => cast_value(eval(expr, row, params)?, *to),
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            // Forma simple: el `operand` se evalúa una vez y se compara con cada
            // `WHEN`. Forma buscada (sin operand): cada `WHEN` es una condición.
            let op_val = match operand {
                Some(o) => Some(eval(o, row, params)?),
                None => None,
            };
            for (cond, res) in whens {
                let hit = match &op_val {
                    // simple: operand = cond (NULL en cualquiera ⇒ no cuadra)
                    Some(ov) => {
                        matches!(
                            cmp_values(ov, &eval(cond, row, params)?)?,
                            Some(Ordering::Equal)
                        )
                    }
                    // buscada: cond booleana (NULL/false ⇒ se salta)
                    None => truthy(eval(cond, row, params)?)?,
                };
                if hit {
                    return eval(res, row, params);
                }
            }
            match else_ {
                Some(e) => eval(e, row, params),
                None => Ok(Value::Null),
            }
        }
        // Las subconsultas se sustituyen en la pre-pasada (`resolve_select_subqueries`)
        // antes de evaluar; si una llega aquí es correlacionada (no soportada en v1).
        Expr::ScalarSubquery(_) | Expr::Exists(_) | Expr::InSubquery { .. } => Err(sql_err(
            "subconsulta sin resolver (las correlacionadas no se soportan en v1)",
        )),
    }
}

/// `CAST(v AS to)` — conversión **explícita**. NULL → NULL en cualquier destino;
/// el resto sigue reglas human-first: lo que no tiene conversión natural es un
/// error, no un valor sorpresa.
fn cast_value(v: Value, to: crate::catalog::ColType) -> Result<Value> {
    use crate::catalog::ColType;
    if matches!(v, Value::Null) {
        return Ok(Value::Null);
    }
    let err = || {
        sql_err(format!(
            "no se puede convertir {} a {}",
            v.type_name(),
            to.name()
        ))
    };
    match to {
        ColType::Integer => match &v {
            Value::Integer(n) => Ok(Value::Integer(*n)),
            Value::Bool(b) => Ok(Value::Integer(i64::from(*b))),
            Value::Real(f) if f.is_nan() => Err(sql_err("no se puede convertir NaN a INTEGER")),
            // `as i64` satura (comportamiento definido en Rust) para |f| enorme.
            Value::Real(f) => Ok(Value::Integer(f.trunc() as i64)),
            Value::Text(s) => parse_int(s).ok_or_else(err),
            Value::Blob(_) => Err(err()),
            Value::Null => unreachable!(),
        },
        ColType::Real => match &v {
            Value::Real(f) => Ok(Value::Real(*f)),
            Value::Integer(n) => Ok(Value::Real(*n as f64)),
            Value::Bool(b) => Ok(Value::Real(if *b { 1.0 } else { 0.0 })),
            Value::Text(s) => s.trim().parse::<f64>().map(Value::Real).map_err(|_| err()),
            Value::Blob(_) => Err(err()),
            Value::Null => unreachable!(),
        },
        ColType::Text => match &v {
            Value::Text(s) => Ok(Value::Text(s.clone())),
            Value::Integer(n) => Ok(Value::Text(n.to_string())),
            Value::Real(f) => Ok(Value::Text(format!("{f:?}"))),
            Value::Bool(b) => Ok(Value::Text(if *b { "TRUE" } else { "FALSE" }.to_string())),
            Value::Blob(b) => String::from_utf8(b.clone())
                .map(Value::Text)
                .map_err(|_| sql_err("CAST de BLOB no-UTF-8 a TEXT")),
            Value::Null => unreachable!(),
        },
        ColType::Blob => match &v {
            Value::Blob(b) => Ok(Value::Blob(b.clone())),
            Value::Text(s) => Ok(Value::Blob(s.clone().into_bytes())),
            _ => Err(err()),
        },
        ColType::Boolean => match &v {
            Value::Bool(b) => Ok(Value::Bool(*b)),
            Value::Integer(n) => Ok(Value::Bool(*n != 0)),
            Value::Real(f) => Ok(Value::Bool(*f != 0.0)),
            Value::Text(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "1" => Ok(Value::Bool(true)),
                "false" | "0" => Ok(Value::Bool(false)),
                _ => Err(err()),
            },
            Value::Blob(_) => Err(err()),
            Value::Null => unreachable!(),
        },
    }
}

/// TEXT → INTEGER para `CAST`: i64 directo y, si no, f64 finito truncado.
fn parse_int(s: &str) -> Option<Value> {
    let t = s.trim();
    if let Ok(n) = t.parse::<i64>() {
        return Some(Value::Integer(n));
    }
    t.parse::<f64>()
        .ok()
        .filter(|f| f.is_finite())
        .map(|f| Value::Integer(f.trunc() as i64))
}

/// Funciones escalares built-in (insensibles a mayúsculas). NULL se propaga
/// (salvo `coalesce`/`ifnull`); el tipo equivocado es un error, no una coerción
/// silenciosa (filosofía human-first del motor).
fn call_function(name: &str, args: &[Value]) -> Result<Value> {
    let lname = name.to_ascii_lowercase();
    let bad_arity = || sql_err(format!("número de argumentos inválido para {lname}()"));
    let need_num = |v: &Value| {
        sql_err(format!(
            "{lname}() requiere un número, no {}",
            v.type_name()
        ))
    };
    let need_text = |v: &Value| sql_err(format!("{lname}() requiere TEXT, no {}", v.type_name()));
    match lname.as_str() {
        "upper" | "lower" => match args {
            [Value::Null] => Ok(Value::Null),
            [Value::Text(s)] => Ok(Value::Text(if lname == "upper" {
                s.to_uppercase()
            } else {
                s.to_lowercase()
            })),
            [v] => Err(need_text(v)),
            _ => Err(bad_arity()),
        },
        "length" | "char_length" => match args {
            [Value::Null] => Ok(Value::Null),
            [Value::Text(s)] => Ok(Value::Integer(s.chars().count() as i64)),
            [Value::Blob(b)] => Ok(Value::Integer(b.len() as i64)),
            [v] => Err(sql_err(format!(
                "length() requiere TEXT o BLOB, no {}",
                v.type_name()
            ))),
            _ => Err(bad_arity()),
        },
        "trim" | "ltrim" | "rtrim" => match args {
            [Value::Null] => Ok(Value::Null),
            [Value::Text(s)] => Ok(Value::Text(match lname.as_str() {
                "ltrim" => s.trim_start().to_string(),
                "rtrim" => s.trim_end().to_string(),
                _ => s.trim().to_string(),
            })),
            [v] => Err(need_text(v)),
            _ => Err(bad_arity()),
        },
        "abs" => match args {
            [Value::Null] => Ok(Value::Null),
            [Value::Integer(n)] => n
                .checked_abs()
                .map(Value::Integer)
                .ok_or_else(|| sql_err("desbordamiento de entero en abs()")),
            [Value::Real(f)] => Ok(Value::Real(f.abs())),
            [v] => Err(need_num(v)),
            _ => Err(bad_arity()),
        },
        "round" => {
            let (x, digits) = match args {
                [x] => (x, 0i64),
                [x, Value::Integer(d)] => (x, *d),
                [_, v] => {
                    return Err(sql_err(format!(
                        "round(): el 2º argumento debe ser un entero, no {}",
                        v.type_name()
                    )));
                }
                _ => return Err(bad_arity()),
            };
            match x {
                Value::Null => Ok(Value::Null),
                Value::Integer(n) => Ok(Value::Real(*n as f64)),
                Value::Real(f) => {
                    let p = 10f64.powi(digits.clamp(0, 15) as i32);
                    Ok(Value::Real((f * p).round() / p))
                }
                v => Err(need_num(v)),
            }
        }
        "coalesce" => {
            if args.is_empty() {
                return Err(bad_arity());
            }
            Ok(args
                .iter()
                .find(|v| !matches!(v, Value::Null))
                .cloned()
                .unwrap_or(Value::Null))
        }
        "ifnull" => match args {
            [a, b] => Ok(if matches!(a, Value::Null) {
                b.clone()
            } else {
                a.clone()
            }),
            _ => Err(bad_arity()),
        },
        "typeof" => match args {
            [v] => Ok(Value::Text(v.type_name().to_string())),
            _ => Err(bad_arity()),
        },
        "substr" | "substring" => {
            let (s, start, len) = match args {
                [Value::Null, ..] => return Ok(Value::Null),
                [Value::Text(s), Value::Integer(start)] => (s, *start, None),
                [Value::Text(s), Value::Integer(start), Value::Integer(len)] => {
                    (s, *start, Some(*len))
                }
                _ => return Err(sql_err("substr(texto, inicio [, largo]) con enteros")),
            };
            let chars: Vec<char> = s.chars().collect();
            let n = chars.len() as i64;
            // 1-based como SQLite; `inicio` negativo cuenta desde el final.
            let begin = if start < 0 {
                (n + start).max(0)
            } else {
                (start - 1).max(0)
            };
            let end = match len {
                Some(l) => (begin + l.max(0)).min(n),
                None => n,
            };
            let (b, e) = (begin.min(n) as usize, end.max(begin).min(n) as usize);
            Ok(Value::Text(chars[b..e].iter().collect()))
        }
        "nullif" => match args {
            // a, salvo que a == b ⇒ NULL. Complemento de coalesce/ifnull.
            [a, b] => Ok(match cmp_values(a, b)? {
                Some(Ordering::Equal) => Value::Null,
                _ => a.clone(),
            }),
            _ => Err(bad_arity()),
        },
        "replace" => match args {
            [Value::Null, _, _] | [_, Value::Null, _] | [_, _, Value::Null] => Ok(Value::Null),
            [Value::Text(s), Value::Text(from), Value::Text(to)] => {
                Ok(Value::Text(if from.is_empty() {
                    s.clone() // patrón vacío no sustituye (como SQLite)
                } else {
                    s.replace(from.as_str(), to)
                }))
            }
            [_, _, _] => Err(sql_err("replace(texto, de, a) requiere TEXT")),
            _ => Err(bad_arity()),
        },
        "instr" => match args {
            [Value::Null, _] | [_, Value::Null] => Ok(Value::Null),
            [Value::Text(hay), Value::Text(needle)] => {
                // 1-based en caracteres; 0 si no aparece; aguja vacía ⇒ 1 (SQLite).
                let pos = if needle.is_empty() {
                    1
                } else {
                    match hay.find(needle.as_str()) {
                        Some(byte_idx) => hay[..byte_idx].chars().count() as i64 + 1,
                        None => 0,
                    }
                };
                Ok(Value::Integer(pos))
            }
            [_, _] => Err(sql_err("instr(texto, sub) requiere TEXT")),
            _ => Err(bad_arity()),
        },
        "reverse" => match args {
            [Value::Null] => Ok(Value::Null),
            [Value::Text(s)] => Ok(Value::Text(s.chars().rev().collect())),
            [v] => Err(need_text(v)),
            _ => Err(bad_arity()),
        },
        "hex" => match args {
            [Value::Null] => Ok(Value::Null),
            [Value::Text(s)] => Ok(Value::Text(to_hex(s.as_bytes()))),
            [Value::Blob(b)] => Ok(Value::Text(to_hex(b))),
            [v] => Err(sql_err(format!(
                "hex() requiere TEXT o BLOB, no {}",
                v.type_name()
            ))),
            _ => Err(bad_arity()),
        },
        "ceil" | "ceiling" | "floor" => match args {
            [Value::Null] => Ok(Value::Null),
            [Value::Integer(n)] => Ok(Value::Integer(*n)),
            [Value::Real(f)] => Ok(Value::Real(if lname == "floor" {
                f.floor()
            } else {
                f.ceil()
            })),
            [v] => Err(need_num(v)),
            _ => Err(bad_arity()),
        },
        "sqrt" => match args {
            // Raíz de negativo ⇒ NULL (como SQLite), no error.
            [Value::Null] => Ok(Value::Null),
            [Value::Integer(n)] => Ok(if *n < 0 {
                Value::Null
            } else {
                Value::Real((*n as f64).sqrt())
            }),
            [Value::Real(f)] => Ok(if *f < 0.0 {
                Value::Null
            } else {
                Value::Real(f.sqrt())
            }),
            [v] => Err(need_num(v)),
            _ => Err(bad_arity()),
        },
        "pow" | "power" => match args {
            [Value::Null, _] | [_, Value::Null] => Ok(Value::Null),
            [a, b] => {
                let x = num_f64(a).ok_or_else(|| need_num(a))?;
                let y = num_f64(b).ok_or_else(|| need_num(b))?;
                Ok(Value::Real(x.powf(y)))
            }
            _ => Err(bad_arity()),
        },
        "mod" => match args {
            // Mismo cero→NULL y desbordamiento que el operador `%`.
            [a, b] => arith(BinOp::Mod, a.clone(), b.clone()),
            _ => Err(bad_arity()),
        },
        "sign" => match args {
            [Value::Null] => Ok(Value::Null),
            [Value::Integer(n)] => Ok(Value::Integer(n.signum())),
            [Value::Real(f)] => Ok(if f.is_nan() {
                Value::Null
            } else if *f > 0.0 {
                Value::Integer(1)
            } else if *f < 0.0 {
                Value::Integer(-1)
            } else {
                Value::Integer(0)
            }),
            [v] => Err(need_num(v)),
            _ => Err(bad_arity()),
        },
        "random" => match args {
            // i64 aleatorio (como SQLite). No determinista: escribirlo en una fila
            // produce un valor irreproducible — elección del usuario, igual que
            // now().
            [] => Ok(Value::Integer(next_random())),
            _ => Err(bad_arity()),
        },
        // --- pack de escalares v1.x: mates, string y fecha ---
        "pi" => match args {
            [] => Ok(Value::Real(std::f64::consts::PI)),
            _ => Err(bad_arity()),
        },
        "exp" | "ln" | "log10" | "log2" | "sin" | "cos" | "tan" | "asin" | "acos" | "atan"
        | "radians" | "degrees" | "trunc" => match args {
            [Value::Null] => Ok(Value::Null),
            [v] => {
                let x = num_f64(v).ok_or_else(|| need_num(v))?;
                Ok(Value::Real(match lname.as_str() {
                    "exp" => x.exp(),
                    "ln" => x.ln(),
                    "log10" => x.log10(),
                    "log2" => x.log2(),
                    "sin" => x.sin(),
                    "cos" => x.cos(),
                    "tan" => x.tan(),
                    "asin" => x.asin(),
                    "acos" => x.acos(),
                    "atan" => x.atan(),
                    "radians" => x.to_radians(),
                    "degrees" => x.to_degrees(),
                    "trunc" => x.trunc(),
                    _ => unreachable!(),
                }))
            }
            _ => Err(bad_arity()),
        },
        "log" => match args {
            // log(x) = base 10; log(b, x) = base b (como las funciones math de SQLite).
            [Value::Null] | [Value::Null, _] | [_, Value::Null] => Ok(Value::Null),
            [v] => Ok(Value::Real(num_f64(v).ok_or_else(|| need_num(v))?.log10())),
            [b, v] => {
                let base = num_f64(b).ok_or_else(|| need_num(b))?;
                let x = num_f64(v).ok_or_else(|| need_num(v))?;
                Ok(Value::Real(x.log(base)))
            }
            _ => Err(bad_arity()),
        },
        "atan2" => match args {
            [Value::Null, _] | [_, Value::Null] => Ok(Value::Null),
            [y, x] => {
                let yy = num_f64(y).ok_or_else(|| need_num(y))?;
                let xx = num_f64(x).ok_or_else(|| need_num(x))?;
                Ok(Value::Real(yy.atan2(xx)))
            }
            _ => Err(bad_arity()),
        },
        // min/max ESCALARES (≥2 args): el parser deja los de 1 arg como agregados.
        "min" | "max" => {
            if args.len() < 2 {
                return Err(bad_arity());
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null); // cualquier NULL ⇒ NULL (como SQLite)
            }
            let want_max = lname == "max";
            let mut best = &args[0];
            for v in &args[1..] {
                if let Some(o) = cmp_values(v, best)?
                    && ((want_max && o == Ordering::Greater) || (!want_max && o == Ordering::Less))
                {
                    best = v;
                }
            }
            Ok(best.clone())
        }
        "concat" => {
            if args.is_empty() {
                return Err(bad_arity());
            }
            let mut s = String::new();
            for v in args {
                if matches!(v, Value::Null) {
                    continue; // NULL ⇒ '' (estilo Postgres)
                }
                s.push_str(&value_text(v).ok_or_else(|| need_text(v))?);
            }
            Ok(Value::Text(s))
        }
        "concat_ws" => match args {
            [] => Err(bad_arity()),
            [Value::Null, ..] => Ok(Value::Null), // separador NULL ⇒ NULL
            [sep, rest @ ..] => {
                let sep = value_text(sep).ok_or_else(|| need_text(sep))?;
                let parts = rest
                    .iter()
                    .filter(|v| !matches!(v, Value::Null))
                    .map(|v| value_text(v).ok_or_else(|| need_text(v)))
                    .collect::<Result<Vec<_>>>()?;
                Ok(Value::Text(parts.join(&sep)))
            }
        },
        "lpad" | "rpad" => {
            if matches!(args.first(), Some(Value::Null)) {
                return Ok(Value::Null);
            }
            let (s, len, pad) = match args {
                [s, Value::Integer(n)] => (s, *n, " ".to_string()),
                [s, Value::Integer(n), p] => (s, *n, value_text(p).ok_or_else(|| need_text(p))?),
                [_, v, ..] => return Err(need_num(v)),
                _ => return Err(bad_arity()),
            };
            let s = value_text(s).ok_or_else(|| need_text(s))?;
            Ok(Value::Text(pad_to(&s, len, &pad, lname == "lpad")))
        }
        "unicode" => match args {
            [Value::Null] => Ok(Value::Null),
            [Value::Text(s)] => Ok(s
                .chars()
                .next()
                .map_or(Value::Null, |c| Value::Integer(c as i64))),
            [v] => Err(need_text(v)),
            _ => Err(bad_arity()),
        },
        "char" => {
            let mut s = String::new();
            for v in args {
                match v {
                    Value::Integer(n) => {
                        let c =
                            u32::try_from(*n)
                                .ok()
                                .and_then(char::from_u32)
                                .ok_or_else(|| {
                                    sql_err(format!("char(): punto de código inválido: {n}"))
                                })?;
                        s.push(c);
                    }
                    _ => return Err(need_num(v)),
                }
            }
            Ok(Value::Text(s))
        }
        "quote" => match args {
            [v] => Ok(Value::Text(sql_quote(v))),
            _ => Err(bad_arity()),
        },
        "glob" => match args {
            [Value::Null, _] | [_, Value::Null] => Ok(Value::Null),
            [Value::Text(pat), Value::Text(s)] => {
                let p: Vec<char> = pat.chars().collect();
                let t: Vec<char> = s.chars().collect();
                Ok(Value::Bool(glob_match(&p, &t)))
            }
            [_, _] => Err(sql_err("glob(patrón TEXT, texto TEXT)")),
            _ => Err(bad_arity()),
        },
        "printf" | "format" => match args {
            [] => Err(bad_arity()),
            [Value::Null, ..] => Ok(Value::Null),
            [fmt, rest @ ..] => {
                let f = value_text(fmt).ok_or_else(|| need_text(fmt))?;
                Ok(Value::Text(sql_printf(&f, rest)?))
            }
        },
        // julianday/unixepoch: el tiempo interno es epoch ms (INTEGER).
        "julianday" => match args {
            [Value::Null] => Ok(Value::Null),
            [Value::Integer(ms)] => Ok(Value::Real(*ms as f64 / 86_400_000.0 + 2_440_587.5)),
            [v] => Err(sql_err(format!(
                "julianday() espera epoch ms (INTEGER), no {}",
                v.type_name()
            ))),
            _ => Err(bad_arity()),
        },
        "unixepoch" => match args {
            [Value::Null] => Ok(Value::Null),
            [Value::Integer(ms)] => Ok(Value::Integer(ms.div_euclid(1000))),
            [v] => Err(sql_err(format!(
                "unixepoch() espera epoch ms (INTEGER), no {}",
                v.type_name()
            ))),
            _ => Err(bad_arity()),
        },
        // Fecha/hora: el entero de tiempo de arkeion es **epoch en milisegundos**
        // (igual que los timestamps de auditoría), no el día juliano de SQLite.
        "now" => match args {
            [] => Ok(Value::Integer(now_ms())), // no determinista, como random()
            _ => Err(bad_arity()),
        },
        "date" => fmt_time(args, "%Y-%m-%d"),
        "time" => fmt_time(args, "%H:%M:%S"),
        "datetime" => fmt_time(args, "%Y-%m-%d %H:%M:%S"),
        "strftime" => match args {
            [Value::Null, _] | [_, Value::Null] => Ok(Value::Null),
            [Value::Text(f), Value::Integer(ms)] => {
                Ok(Value::Text(crate::sql::datetime::strftime(f, *ms)))
            }
            [_, _] => Err(sql_err("strftime(formato TEXT, epoch_ms INTEGER)")),
            _ => Err(bad_arity()),
        },
        _ => Err(sql_err(format!("función desconocida: {name}()"))),
    }
}

/// Instante actual en epoch ms UTC (para `now()`).
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `date/time/datetime(epoch_ms)`: formatea un epoch ms (INTEGER) con `fmt`.
fn fmt_time(args: &[Value], fmt: &str) -> Result<Value> {
    match args {
        [Value::Null] => Ok(Value::Null),
        [Value::Integer(ms)] => Ok(Value::Text(crate::sql::datetime::strftime(fmt, *ms))),
        [v] => Err(sql_err(format!(
            "se esperaba epoch ms (INTEGER), no {}",
            v.type_name()
        ))),
        _ => Err(sql_err("se esperaba 1 argumento (epoch ms INTEGER)")),
    }
}

/// Bytes → hex en mayúsculas (para `hex()`).
fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02X}");
    }
    s
}

/// Representación TEXT de un valor (misma regla que `CAST … AS TEXT`). `None` si
/// no es representable como texto (NULL, o BLOB no-UTF8). La usan `concat`,
/// `concat_ws`, `lpad/rpad`, `printf`, …
fn value_text(v: &Value) -> Option<String> {
    match cast_value(v.clone(), crate::catalog::ColType::Text) {
        Ok(Value::Text(s)) => Some(s),
        _ => None,
    }
}

/// Rellena `s` a `len` caracteres con `pad` (por la izquierda o la derecha). Si
/// `s` ya es más largo, lo trunca a `len` (estilo Postgres/MySQL).
fn pad_to(s: &str, len: i64, pad: &str, left: bool) -> String {
    let target = len.max(0) as usize;
    let chars: Vec<char> = s.chars().collect();
    if chars.len() >= target {
        return chars[..target].iter().collect();
    }
    if pad.is_empty() {
        return s.to_string(); // sin relleno posible
    }
    let padding: String = pad.chars().cycle().take(target - chars.len()).collect();
    if left {
        format!("{padding}{s}")
    } else {
        format!("{s}{padding}")
    }
}

/// Literal SQL de un valor (para `quote()`): TEXT entre comillas con `'` doblada,
/// BLOB como `X'…'`, NULL como `NULL`, números tal cual.
fn sql_quote(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Integer(n) => n.to_string(),
        Value::Real(f) => format!("{f:?}"),
        Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
        Value::Blob(b) => format!("X'{}'", to_hex(b)),
    }
}

/// Emparejamiento estilo `GLOB`: `*` (cualquier secuencia), `?` (un carácter),
/// `[abc]`/`[a-z]`/`[^…]` (clases). Sensible a mayúsculas (como SQLite).
fn glob_match(pattern: &[char], s: &[char]) -> bool {
    match pattern.first() {
        None => s.is_empty(),
        Some('*') => (0..=s.len()).any(|i| glob_match(&pattern[1..], &s[i..])),
        Some('?') => !s.is_empty() && glob_match(&pattern[1..], &s[1..]),
        Some('[') => {
            if s.is_empty() {
                return false;
            }
            let (matched, rest) = glob_class(&pattern[1..], s[0]);
            matched && glob_match(rest, &s[1..])
        }
        Some(&c) => s.first() == Some(&c) && glob_match(&pattern[1..], &s[1..]),
    }
}

/// Evalúa una clase `[...]` de `glob_match` contra `ch`. Devuelve si casa y el
/// resto del patrón tras el `]` de cierre.
fn glob_class(p: &[char], ch: char) -> (bool, &[char]) {
    let mut i = 0;
    let negate = p.first() == Some(&'^');
    if negate {
        i += 1;
    }
    let mut matched = false;
    while i < p.len() && p[i] != ']' {
        if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
            if p[i] <= ch && ch <= p[i + 2] {
                matched = true;
            }
            i += 3;
        } else {
            if p[i] == ch {
                matched = true;
            }
            i += 1;
        }
    }
    let rest = if i < p.len() { &p[i + 1..] } else { &p[i..] };
    (matched ^ negate, rest)
}

/// `printf`/`format`: subconjunto C de SQLite. Flags `-`/`0`, anchura, `.precisión`
/// y conversiones `d i s f x X o %`. `%%` es un `%` literal.
fn sql_printf(fmt: &str, args: &[Value]) -> Result<String> {
    let f: Vec<char> = fmt.chars().collect();
    let mut out = String::new();
    let mut ai = 0usize;
    let mut i = 0usize;
    while i < f.len() {
        if f[i] != '%' {
            out.push(f[i]);
            i += 1;
            continue;
        }
        i += 1;
        if f.get(i) == Some(&'%') {
            out.push('%');
            i += 1;
            continue;
        }
        // flags
        let (mut left, mut zero) = (false, false);
        while let Some(c) = f.get(i) {
            match c {
                '-' => left = true,
                '0' => zero = true,
                _ => break,
            }
            i += 1;
        }
        // anchura
        let mut width = 0usize;
        while let Some(d) = f.get(i).and_then(|c| c.to_digit(10)) {
            width = width * 10 + d as usize;
            i += 1;
        }
        // precisión
        let mut prec: Option<usize> = None;
        if f.get(i) == Some(&'.') {
            i += 1;
            let mut p = 0usize;
            while let Some(d) = f.get(i).and_then(|c| c.to_digit(10)) {
                p = p * 10 + d as usize;
                i += 1;
            }
            prec = Some(p);
        }
        let conv = *f
            .get(i)
            .ok_or_else(|| sql_err("printf(): especificador incompleto"))?;
        i += 1;
        let arg = args
            .get(ai)
            .ok_or_else(|| sql_err("printf(): faltan argumentos para el formato"))?;
        ai += 1;
        let (body, numeric) = printf_one(conv, arg, prec)?;
        out.push_str(&printf_pad(&body, width, left, zero && numeric));
    }
    Ok(out)
}

/// Convierte un argumento de `printf` según la conversión. Devuelve el texto y si
/// es numérico (para decidir el relleno con ceros).
fn printf_one(conv: char, arg: &Value, prec: Option<usize>) -> Result<(String, bool)> {
    use crate::catalog::ColType;
    let as_int = |v: &Value| -> Result<i64> {
        match cast_value(v.clone(), ColType::Integer)? {
            Value::Integer(n) => Ok(n),
            _ => Ok(0),
        }
    };
    Ok(match conv {
        'd' | 'i' => (as_int(arg)?.to_string(), true),
        'x' => (format!("{:x}", as_int(arg)? as u64), true),
        'X' => (format!("{:X}", as_int(arg)? as u64), true),
        'o' => (format!("{:o}", as_int(arg)? as u64), true),
        'f' => {
            let x = num_f64(arg).unwrap_or(0.0);
            (format!("{:.*}", prec.unwrap_or(6), x), true)
        }
        's' => {
            let s = value_text(arg).unwrap_or_default();
            (
                match prec {
                    Some(p) => s.chars().take(p).collect(),
                    None => s,
                },
                false,
            )
        }
        other => {
            return Err(sql_err(format!(
                "printf(): conversión desconocida %{other}"
            )));
        }
    })
}

/// Rellena `body` a `width` (a la izquierda/derecha; con ceros tras un signo si
/// `zero`).
fn printf_pad(body: &str, width: usize, left: bool, zero: bool) -> String {
    let len = body.chars().count();
    if len >= width {
        return body.to_string();
    }
    let fill = width - len;
    if left {
        format!("{body}{}", " ".repeat(fill))
    } else if zero {
        if let Some(sign @ ('-' | '+')) = body.chars().next() {
            format!("{sign}{}{}", "0".repeat(fill), &body[1..])
        } else {
            format!("{}{body}", "0".repeat(fill))
        }
    } else {
        format!("{}{body}", " ".repeat(fill))
    }
}

/// Valor numérico como `f64` (INTEGER o REAL), o `None` si no es numérico.
fn num_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Integer(n) => Some(*n as f64),
        Value::Real(f) => Some(*f),
        _ => None,
    }
}

/// PRNG por hilo para `random()`: xorshift64 sembrado del reloj. Sin dependencias
/// externas (supply-chain mínima, D8) y sin `unsafe` (`Cell` basta).
fn next_random() -> i64 {
    use std::cell::Cell;
    fn seed() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        (nanos ^ 0x9E37_79B9_7F4A_7C15) | 1 // nunca 0 (xorshift se quedaría en 0)
    }
    thread_local! {
        static STATE: Cell<u64> = Cell::new(seed());
    }
    STATE.with(|s| {
        let mut x = s.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        x as i64
    })
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
                // División/módulo por cero → NULL (compat SQLite), no error.
                BinOp::Div => {
                    if y == 0 {
                        return Ok(Null);
                    }
                    x.checked_div(y)
                }
                BinOp::Mod => {
                    if y == 0 {
                        return Ok(Null);
                    }
                    // `i64::MIN % -1`: `checked_rem` da `None` por un quirk de la
                    // instrucción de CPU, pero el resultado real es 0 (`x % -1 ≡ 0`)
                    // y no hay desbordamiento. La división sí desborda ahí (→ error).
                    Some(x.checked_rem(y).unwrap_or(0))
                }
                _ => unreachable!("solo aritmética"),
            };
            v.map(Integer).ok_or(sql_err("desbordamiento de entero"))
        }
        (a @ (Integer(_) | Real(_)), b @ (Integer(_) | Real(_))) => {
            let x = as_f64(&a);
            let y = as_f64(&b);
            // División/módulo por cero → NULL (compat SQLite): el `/0.0` daría ±inf
            // y el `%0.0` daría NaN; SQLite devuelve NULL en ambos.
            if matches!(op, BinOp::Div | BinOp::Mod) && y == 0.0 {
                return Ok(Null);
            }
            let r = match op {
                BinOp::Add => x + y,
                BinOp::Sub => x - y,
                BinOp::Mul => x * y,
                BinOp::Div => x / y,
                BinOp::Mod => x % y,
                _ => unreachable!("solo aritmética"),
            };
            // NaN (p. ej. `inf - inf`, `inf / inf`) no es almacenable ni comparable;
            // SQLite lo normaliza a NULL. `±inf` (p. ej. `1e308 * 10`) sí se conserva,
            // como en SQLite, porque está totalmente ordenado.
            Ok(if r.is_nan() { Null } else { Real(r) })
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
    fn integer_overflow_errors_but_div_zero_is_null() {
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
        // División/módulo por cero → NULL (compat SQLite), no error.
        assert_eq!(bin(7, BinOp::Div, 0).unwrap(), Value::Null);
        assert_eq!(bin(7, BinOp::Mod, 0).unwrap(), Value::Null);
        assert_eq!(bin(7, BinOp::Div, 2).unwrap(), Value::Integer(3));
        // `i64::MIN / -1` desborda (error, coherente con +/-/*); `i64::MIN % -1`
        // es 0 (sin desbordamiento real).
        assert!(bin(i64::MIN, BinOp::Div, -1).is_err());
        assert_eq!(bin(i64::MIN, BinOp::Mod, -1).unwrap(), Value::Integer(0));
    }

    #[test]
    fn real_nan_is_null_but_inf_is_kept() {
        let bin = |a: f64, op: BinOp, b: f64| {
            eval(
                &Expr::Binary(
                    Box::new(Expr::Literal(Value::Real(a))),
                    op,
                    Box::new(Expr::Literal(Value::Real(b))),
                ),
                None,
                &[],
            )
            .unwrap()
        };
        let inf = f64::INFINITY;
        // NaN (inf−inf, inf/inf) → NULL, como SQLite.
        assert_eq!(bin(inf, BinOp::Sub, inf), Value::Null);
        assert_eq!(bin(inf, BinOp::Div, inf), Value::Null);
        // Overflow a ±inf se conserva (totalmente ordenado), como SQLite.
        assert_eq!(bin(1e308, BinOp::Mul, 10.0), Value::Real(inf));
        // División real por cero → NULL (no ±inf).
        assert_eq!(bin(7.0, BinOp::Div, 0.0), Value::Null);
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
                dropped: false,
            }],
            indexes: Vec::new(),
            logical_order: vec![0],
            foreign_keys: Vec::new(),
        };
        let schema = QuerySchema::single("t", def);
        let rows: Vec<Vec<Value>> = vec![
            vec![Value::Integer(10)],
            vec![Value::Null],
            vec![Value::Integer(30)],
        ];
        let agg = |func: AggFunc, arg: Option<Expr>| {
            compute_aggregate(func, arg.as_ref(), false, None, &schema, &rows, &[]).unwrap()
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
            false,
            None,
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
                dropped: false,
            }],
            indexes: Vec::new(),
            logical_order: vec![0],
            foreign_keys: Vec::new(),
        };
        let mut schema = QuerySchema::single("a", table("a"));
        schema.push("b", table("b")).unwrap();
        assert!(schema.resolve(None, "id").is_err(), "ambigua");
        assert_eq!(schema.resolve(Some("b"), "id").unwrap(), 1);
        assert!(schema.resolve(Some("zz"), "id").is_err());
        assert!(schema.push("a", table("a")).is_err(), "alias duplicado");
    }
}
