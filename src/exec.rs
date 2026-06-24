//! Ejecutor SQL (docs/04-sql.md): planificador mínimo (full scan o *point
//! lookup* por rowid), joins nested-loop, agregados con/sin `GROUP BY` (+ `HAVING`)
//! y evaluación de expresiones con lógica trivalente.
//!
//! Filosofía de tipos (human-first): comparar tipos distintos es un error,
//! no una coerción silenciosa. Única promoción: INTEGER ↔ REAL.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use crate::catalog::{
    self, ColType, ColumnDef, ColumnSpec, FtsIndexDef, IndexDef, TableDef, TableSpec, TriggerDef,
    TriggerEvent, TriggerForEach, TriggerTiming, VectorIndexDef, VectorMetric,
};
use crate::error::{Error, Result};
use crate::record::Value;
use crate::sql::ast::{
    AggFunc, BinOp, ColumnAst, Cte, Expr, FrameBound, InsertSource, JoinKind, OnConflict, OrderBy,
    SelectItem, SelectStmt, SetOp, Stmt, TableRef, UnOp, WindowFrame, WindowFunc,
};
use crate::sql::json;
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
    /// rowids que casan una consulta `MATCH` vía el índice full-text (narrowing).
    fn fts_search(
        &self,
        table: &TableDef,
        fts: &FtsIndexDef,
        query: &crate::fts::Query,
    ) -> Result<Vec<i64>>;
    /// Stats BM25 de una consulta: `df` por término (en orden) + globales
    /// `(N docs, Σ tokens)`.
    fn fts_stats(&self, fts_id: u32, terms: &[String]) -> Result<(Vec<u64>, u64, u64)>;
    /// rowids candidatos del índice vectorial IVF (los `nprobe` clusters más
    /// cercanos a `query`).
    fn vector_search(
        &self,
        vidx: &VectorIndexDef,
        query: &[f32],
        nprobe: usize,
        limit: usize,
    ) -> Result<Vec<i64>>;
    /// Top-k rowids por distancia EXACTA `metric(col, query)` (KNN sin índice).
    fn knn_exact(
        &self,
        table: &TableDef,
        col: usize,
        query: &[u8],
        metric: VectorMetric,
        k: usize,
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

    fn view(&self, name: &str) -> Result<Option<String>> {
        Snapshot::view(self, name)
    }

    fn fts_search(
        &self,
        table: &TableDef,
        fts: &FtsIndexDef,
        query: &crate::fts::Query,
    ) -> Result<Vec<i64>> {
        Snapshot::fts_search(self, table, fts, query)
    }

    fn fts_stats(&self, fts_id: u32, terms: &[String]) -> Result<(Vec<u64>, u64, u64)> {
        Snapshot::fts_stats(self, fts_id, terms)
    }

    fn vector_search(
        &self,
        vidx: &VectorIndexDef,
        query: &[f32],
        nprobe: usize,
        limit: usize,
    ) -> Result<Vec<i64>> {
        Snapshot::vector_search(self, vidx, query, nprobe, limit)
    }
    fn knn_exact(
        &self,
        table: &TableDef,
        col: usize,
        query: &[u8],
        metric: VectorMetric,
        k: usize,
    ) -> Result<Vec<i64>> {
        Snapshot::knn_exact(self, table, col, query, metric, k)
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

    fn fts_search(
        &self,
        table: &TableDef,
        fts: &FtsIndexDef,
        query: &crate::fts::Query,
    ) -> Result<Vec<i64>> {
        WriteTx::fts_search(self, table, fts, query)
    }

    fn fts_stats(&self, fts_id: u32, terms: &[String]) -> Result<(Vec<u64>, u64, u64)> {
        WriteTx::fts_stats(self, fts_id, terms)
    }

    fn vector_search(
        &self,
        vidx: &VectorIndexDef,
        query: &[f32],
        nprobe: usize,
        limit: usize,
    ) -> Result<Vec<i64>> {
        WriteTx::vector_search(self, vidx, query, nprobe, limit)
    }
    fn knn_exact(
        &self,
        table: &TableDef,
        col: usize,
        query: &[u8],
        metric: VectorMetric,
        k: usize,
    ) -> Result<Vec<i64>> {
        WriteTx::knn_exact(self, table, col, query, metric, k)
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
    /// Contexto BM25 precomputado por `(fts_id, texto de consulta)`: lo llena
    /// `run_select` (que tiene acceso al store) antes del bucle de filas, y lo lee
    /// `eval` de `bm25(col, q)` (que solo tiene la fila + este esquema).
    fts_rank: HashMap<(u32, String), FtsRankCtx>,
}

/// Pesos IDF (por término normalizado) y longitud media de documento de una
/// consulta BM25; constantes en todas las filas.
struct FtsRankCtx {
    idf: HashMap<String, f64>,
    avgdl: f64,
}

struct SchemaTable {
    qualifier: String,
    def: TableDef,
    offset: usize,
}

impl QuerySchema {
    fn new() -> QuerySchema {
        QuerySchema {
            tables: Vec::new(),
            fts_rank: HashMap::new(),
        }
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

    /// Como [`resolve`](Self::resolve) pero devuelve la tabla dueña de la columna
    /// (su `def`), la posición **local** de la columna en esa tabla y el offset de
    /// la tabla en la fila combinada. Lo usa la evaluación de `MATCH`.
    fn owner_of(&self, table: Option<&str>, name: &str) -> Result<(&TableDef, usize, usize)> {
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
                Ok((&t.def, i, t.offset))
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
                        found = Some((&t.def, i, t.offset));
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
        Expr::Match { column, query, .. } => {
            validate_columns(column, schema)?;
            validate_columns(query, schema)
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
        Expr::Window {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args.iter().chain(partition_by) {
                validate_columns(a, schema)?;
            }
            for o in order_by {
                validate_columns(&o.expr, schema)?;
            }
            Ok(())
        }
    }
}

fn no_aggregates(e: &Expr, clause: &str) -> Result<()> {
    if e.has_aggregate() {
        return Err(sql_err(format!("los agregados no se permiten en {clause}")));
    }
    Ok(())
}

/// Resuelve un destino de `ORDER BY` a un índice de columna de **salida**: por
/// nombre (columna/alias de la proyección) o por posición ordinal 1-based
/// (`ORDER BY 2`). Para los caminos que ordenan filas ya proyectadas
/// (UNION, GROUP BY, ventanas).
fn order_output_index(o: &OrderBy, columns: &[String]) -> Result<usize> {
    match &o.expr {
        Expr::Column { table: None, name } => columns
            .iter()
            .position(|c| c == name)
            .ok_or_else(|| sql_err(format!("ORDER BY: «{name}» no está en la proyección"))),
        Expr::Literal(Value::Integer(n)) if *n >= 1 && (*n as usize) <= columns.len() => {
            Ok(*n as usize - 1)
        }
        _ => Err(sql_err(
            "ORDER BY aquí solo admite una columna de salida o su posición",
        )),
    }
}

/// Para el `ORDER BY` del SELECT principal: la expresión a evaluar sobre la fila
/// de **entrada**. Un literal entero N → la N-ésima expresión de la proyección;
/// un alias → su expresión; si no, la propia expresión (columna o cálculo).
fn order_key_expr<'a>(o: &'a OrderBy, projection: &'a [SelectItem]) -> Result<&'a Expr> {
    match &o.expr {
        Expr::Literal(Value::Integer(n)) if *n >= 1 => match projection.get(*n as usize - 1) {
            Some(SelectItem::Expr { expr, .. }) => Ok(expr),
            _ => Err(sql_err("ORDER BY: posición fuera de rango o sobre '*'")),
        },
        Expr::Column { table: None, name } => Ok(projection
            .iter()
            .find_map(|it| match it {
                SelectItem::Expr {
                    expr,
                    alias: Some(a),
                } if a == name => Some(expr),
                _ => None,
            })
            .unwrap_or(&o.expr)),
        _ => Ok(&o.expr),
    }
}

// --- sentencias de escritura (autocommit lo gestiona la API) ---

pub fn run_execute(tx: &mut WriteTx, stmt: &Stmt, params: &[Value]) -> Result<usize> {
    match stmt {
        Stmt::CreateTable {
            if_not_exists,
            name,
            columns,
            foreign_keys,
            uniques,
            checks,
        } => {
            if tx.table(name)?.is_some() {
                if *if_not_exists {
                    return Ok(0);
                }
                return Err(Error::Constraint("la tabla ya existe"));
            }
            tx.create_table(&table_spec(name, columns, foreign_keys, uniques, checks)?)?;
            Ok(0)
        }
        Stmt::CreateTableAs {
            if_not_exists,
            name,
            query,
        } => {
            if tx.table(name)?.is_some() || tx.view(name)?.is_some() {
                if *if_not_exists {
                    return Ok(0);
                }
                return Err(Error::Constraint("la tabla ya existe"));
            }
            // Materializa la consulta; infiere el tipo de cada columna de sus valores.
            let out = run_query(&*tx, query, params)?;
            if out.columns.is_empty() {
                return Err(sql_err(
                    "CREATE TABLE AS SELECT necesita al menos una columna",
                ));
            }
            let mut col_specs = Vec::with_capacity(out.columns.len());
            for (i, cname) in out.columns.iter().enumerate() {
                let col_type = infer_coltype(out.rows.iter().map(|r| &r[i]))?;
                col_specs.push(ColumnSpec {
                    name: cname.clone(),
                    col_type,
                    not_null: false,
                    primary_key: false,
                    default: None,
                    references: None,
                    unique: false,
                    check: None,
                });
            }
            let spec = TableSpec {
                name: name.clone(),
                columns: col_specs,
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: Vec::new(),
            };
            let def = tx.create_table(&spec)?;
            let n = out.rows.len();
            for row in out.rows {
                tx.insert_row(&def, &row)?;
            }
            Ok(n)
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
        Stmt::CreateFtsIndex {
            if_not_exists,
            name,
            table,
            columns,
            tokenizer,
        } => {
            if *if_not_exists && tx.fts_index_exists(name)? {
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
            // Sin `USING` ⇒ tokenizer por defecto.
            let tok = tokenizer.as_deref().unwrap_or("unicode");
            tx.create_fts_index(table, name, &positions, tok)?;
            Ok(0)
        }
        Stmt::DropFtsIndex { if_exists, name } => {
            let dropped = tx.drop_fts_index(name)?;
            if !dropped && !if_exists {
                return Err(sql_err(format!("índice FTS desconocido: {name}")));
            }
            Ok(0)
        }
        Stmt::CreateVectorIndex {
            if_not_exists,
            name,
            table,
            column,
            metric,
            lists,
            probes,
        } => {
            if *if_not_exists && tx.vector_index_exists(name)? {
                return Ok(0);
            }
            let def = tx
                .table(table)?
                .ok_or_else(|| sql_err(format!("tabla desconocida: {table}")))?;
            let col = def
                .columns
                .iter()
                .position(|c| &c.name == column)
                .ok_or_else(|| sql_err(format!("columna desconocida: {column}")))?;
            let metric = match metric.as_deref() {
                None | Some("cosine") => catalog::VectorMetric::Cosine,
                Some("l2") => catalog::VectorMetric::L2,
                Some(m) => return Err(sql_err(format!("métrica vectorial desconocida: {m}"))),
            };
            // Por defecto 100 listas (k-means las recorta a ≤ nº de filas) y
            // nprobe ≈ 10% de las listas (recall vs velocidad).
            let lists = (*lists).unwrap_or(100).clamp(1, u16::MAX as u32) as u16;
            let nprobe = (*probes)
                .unwrap_or_else(|| (lists as u32).div_ceil(10))
                .clamp(1, u16::MAX as u32) as u16;
            tx.create_vector_index(table, name, col, lists, metric, nprobe)?;
            Ok(0)
        }
        Stmt::DropVectorIndex { if_exists, name } => {
            let dropped = tx.drop_vector_index(name)?;
            if !dropped && !if_exists {
                return Err(sql_err(format!("índice vectorial desconocido: {name}")));
            }
            Ok(0)
        }
        Stmt::RebuildVectorIndex { name } => {
            if !tx.rebuild_vector_index(name)? {
                return Err(sql_err(format!("índice vectorial desconocido: {name}")));
            }
            Ok(0)
        }
        Stmt::Insert {
            table,
            columns,
            source,
            on_conflict,
            returning,
        } => Ok(run_insert(
            tx,
            table,
            columns.as_deref(),
            source,
            params,
            on_conflict.as_ref(),
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
        // La conexión intercepta las de transacción/savepoint antes de llegar aquí.
        Stmt::Begin
        | Stmt::Commit
        | Stmt::Rollback
        | Stmt::Savepoint(_)
        | Stmt::ReleaseSavepoint(_)
        | Stmt::RollbackTo(_) => Err(sql_err(
            "las sentencias de transacción/savepoint las gestiona la conexión",
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
            source,
            on_conflict,
            returning,
        } => run_insert(
            tx,
            table,
            columns.as_deref(),
            source,
            params,
            on_conflict.as_ref(),
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
    // FTS narrowing: un `MATCH` no negado como conjunto de nivel superior usa el
    // índice full-text para acotar candidatos (evita el full scan). El WHERE
    // completo —incluido el propio MATCH— se re-aplica por fila en el caller, así
    // que basta con devolver un superconjunto exacto.
    if let Some(m) = where_clause.and_then(find_match_conjunct)
        && let Expr::Match {
            column,
            query,
            negated,
        } = m
        && !*negated
        && let Expr::Column { name, .. } = column.as_ref()
        && let Some(local_pos) = def
            .columns
            .iter()
            .position(|c| !c.dropped && c.name == *name)
        && let Some(fts) = def
            .fts_indexes
            .iter()
            .find(|f| f.columns.contains(&local_pos))
        && let Value::Text(q_str) = eval_const(query, params)?
    {
        let q = crate::fts::parse_query(&q_str)?;
        let mut out = Vec::new();
        for rowid in tx.fts_search(def, fts, &q)? {
            if let Some(row) = tx.get_row(def, rowid)? {
                out.push((rowid, row));
            }
        }
        return Ok(out);
    }
    let mut all = Vec::new();
    for item in tx.scan_table(def)? {
        all.push(item?);
    }
    Ok(all)
}

/// El primer `MATCH` accesible a través de ANDs de nivel superior del WHERE (un
/// conjunto): se puede usar el índice para acotar. Un `MATCH` bajo `OR`/`NOT` no
/// aparece aquí ⇒ cae al full scan (el eval per-fila lo resuelve correctamente).
fn find_match_conjunct(e: &Expr) -> Option<&Expr> {
    match e {
        Expr::Match { .. } => Some(e),
        Expr::Binary(a, BinOp::And, b) => find_match_conjunct(a).or_else(|| find_match_conjunct(b)),
        _ => None,
    }
}

/// Narrowing FTS para `run_select`: si el WHERE tiene un `MATCH` no negado de
/// nivel superior sobre una columna indexada con consulta constante, devuelve las
/// filas que casan vía el índice (evita el full scan). El WHERE completo se
/// re-aplica por fila después, así que basta con un superconjunto exacto.
fn fts_plan(
    src: &impl DataSource,
    def: &TableDef,
    where_clause: Option<&Expr>,
    params: &[Value],
) -> Result<Option<Vec<Vec<Value>>>> {
    let Some(Expr::Match {
        column,
        query,
        negated,
    }) = where_clause.and_then(find_match_conjunct)
    else {
        return Ok(None);
    };
    if *negated {
        return Ok(None);
    }
    let Expr::Column { name, .. } = column.as_ref() else {
        return Ok(None);
    };
    let Some(local_pos) = def
        .columns
        .iter()
        .position(|c| !c.dropped && c.name == *name)
    else {
        return Ok(None);
    };
    let Some(fts) = def
        .fts_indexes
        .iter()
        .find(|f| f.columns.contains(&local_pos))
    else {
        return Ok(None);
    };
    let Value::Text(q_str) = eval_const(query, params)? else {
        return Ok(None);
    };
    let q = crate::fts::parse_query(&q_str)?;
    let mut rows = Vec::new();
    for rowid in src.fts_search(def, fts, &q)? {
        if let Some(row) = src.get_row(def, rowid)? {
            rows.push(row);
        }
    }
    Ok(Some(rows))
}

/// Plan vectorial (ANN) para `run_select`: si la consulta es un KNN
/// —`ORDER BY cosine_distance|l2_distance(col, qvec) [ASC] LIMIT k`, sin WHERE—
/// sobre una columna con índice IVF de la misma métrica, devuelve los candidatos
/// de los `nprobe` clusters más cercanos. El `ORDER BY`/`LIMIT` los rankea exacto
/// después. Sin índice apto ⇒ `None` (cae al full scan = KNN exacto).
fn vector_plan(
    src: &impl DataSource,
    def: &TableDef,
    stmt: &SelectStmt,
    params: &[Value],
) -> Result<Option<Vec<Vec<Value>>>> {
    if stmt.where_clause.is_some() || stmt.limit.is_none() || stmt.order_by.len() != 1 {
        return Ok(None);
    }
    let ob = &stmt.order_by[0];
    if ob.desc {
        return Ok(None); // distancia: menor = más cercano ⇒ ASC
    }
    let Expr::Function { name, args } = &ob.expr else {
        return Ok(None);
    };
    let metric = match name.as_str() {
        "cosine_distance" => VectorMetric::Cosine,
        "l2_distance" => VectorMetric::L2,
        _ => return Ok(None),
    };
    if args.len() != 2 {
        return Ok(None);
    }
    let Expr::Column { name: col, .. } = &args[0] else {
        return Ok(None);
    };
    let Some(col_pos) = def
        .columns
        .iter()
        .position(|c| !c.dropped && c.name == *col)
    else {
        return Ok(None);
    };
    let Some(vidx) = def
        .vector_indexes
        .iter()
        .find(|v| v.column == col_pos && v.metric == metric)
    else {
        return Ok(None);
    };
    let Value::Blob(qb) = eval_const(&args[1], params)? else {
        return Ok(None);
    };
    let query = crate::vector::to_f32(&qb)?;
    // nprobe del índice (recall vs velocidad), fijado al crearlo con `PROBES`.
    let nprobe = (vidx.nprobe as usize).max(1);
    // Shortlist: el índice rankea aprox por int8 y devuelve k·8 candidatos (cota
    // de recall); el `ORDER BY` exacto de abajo elige los k finales fetcheando solo
    // esas pocas filas (antes fetcheaba TODOS los candidatos de los clusters).
    let k = match &stmt.limit {
        Some(e) => usize_const(e, params, "LIMIT")?,
        None => return Ok(None),
    };
    let limit = k.saturating_mul(32).max(64);
    let mut rows = Vec::new();
    for rowid in src.vector_search(vidx, &query, nprobe, limit)? {
        if let Some(row) = src.get_row(def, rowid)? {
            rows.push(row);
        }
    }
    Ok(Some(rows))
}

/// Plan KNN **EXACTO** (sin índice IVF): `ORDER BY cosine_distance|l2_distance(col,
/// qvec) ASC LIMIT k` sin WHERE/JOIN/etc. ⇒ top-k en **streaming** vía
/// `knn_exact` (decodifica solo la columna del vector, no materializa la tabla).
/// Devuelve solo las `k` filas (`get_row`). Gateado estricto: como pre-limita a k,
/// cualquier cosa que cambie la cardinalidad después (join, distinct, group by,
/// having, offset, union) lo desactiva y cae al full scan general.
fn exact_knn_plan(
    src: &impl DataSource,
    def: &TableDef,
    stmt: &SelectStmt,
    params: &[Value],
) -> Result<Option<Vec<Vec<Value>>>> {
    if stmt.where_clause.is_some()
        || stmt.limit.is_none()
        || stmt.order_by.len() != 1
        || !stmt.joins.is_empty()
        || !stmt.group_by.is_empty()
        || stmt.having.is_some()
        || stmt.distinct
        || stmt.offset.is_some()
        || !stmt.compound.is_empty()
    {
        return Ok(None);
    }
    let ob = &stmt.order_by[0];
    if ob.desc {
        return Ok(None);
    }
    let Expr::Function { name, args } = &ob.expr else {
        return Ok(None);
    };
    let metric = match name.as_str() {
        "cosine_distance" => VectorMetric::Cosine,
        "l2_distance" => VectorMetric::L2,
        _ => return Ok(None),
    };
    if args.len() != 2 {
        return Ok(None);
    }
    let Expr::Column { name: col, .. } = &args[0] else {
        return Ok(None);
    };
    let Some(col_pos) = def.columns.iter().position(|c| !c.dropped && c.name == *col) else {
        return Ok(None);
    };
    let Value::Blob(qb) = eval_const(&args[1], params)? else {
        return Ok(None);
    };
    let k = match &stmt.limit {
        Some(e) => usize_const(e, params, "LIMIT")?,
        None => return Ok(None),
    };
    let mut rows = Vec::new();
    for rowid in src.knn_exact(def, col_pos, &qb, metric, k)? {
        if let Some(row) = src.get_row(def, rowid)? {
            rows.push(row);
        }
    }
    Ok(Some(rows))
}

/// Recolecta las llamadas `bm25(col, q)` de una expresión.
fn collect_bm25<'a>(e: &'a Expr, out: &mut Vec<(&'a Expr, &'a Expr)>) {
    match e {
        Expr::Function { name, args } if name == "bm25" && args.len() == 2 => {
            out.push((&args[0], &args[1]));
        }
        Expr::Function { args, .. } => args.iter().for_each(|a| collect_bm25(a, out)),
        Expr::Unary(_, x) | Expr::Cast { expr: x, .. } | Expr::IsNull { expr: x, .. } => {
            collect_bm25(x, out)
        }
        Expr::Binary(a, _, b) => {
            collect_bm25(a, out);
            collect_bm25(b, out);
        }
        Expr::Like { expr, pattern, .. } => {
            collect_bm25(expr, out);
            collect_bm25(pattern, out);
        }
        Expr::Match { column, query, .. } => {
            collect_bm25(column, out);
            collect_bm25(query, out);
        }
        Expr::In { expr, list, .. } => {
            collect_bm25(expr, out);
            list.iter().for_each(|x| collect_bm25(x, out));
        }
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(o) = operand {
                collect_bm25(o, out);
            }
            whens.iter().for_each(|(c, r)| {
                collect_bm25(c, out);
                collect_bm25(r, out);
            });
            if let Some(x) = else_ {
                collect_bm25(x, out);
            }
        }
        Expr::Aggregate { arg: Some(a), .. } => collect_bm25(a, out),
        Expr::Window {
            args,
            partition_by,
            order_by,
            ..
        } => {
            args.iter().for_each(|a| collect_bm25(a, out));
            partition_by.iter().for_each(|a| collect_bm25(a, out));
            order_by.iter().for_each(|o| collect_bm25(&o.expr, out));
        }
        _ => {}
    }
}

/// Precomputa el contexto BM25 (`idf` por término + `avgdl`) de cada `bm25(col,q)`
/// de la proyección/`ORDER BY`, leyendo las stats del índice vía `src`, y lo deja
/// en `schema` para que `eval` lo use por fila. `idf(t) = ln(1 + (N-df+0.5)/(df+0.5))`.
fn precompute_bm25(
    src: &impl DataSource,
    schema: &mut QuerySchema,
    stmt: &SelectStmt,
    params: &[Value],
) -> Result<()> {
    let mut calls: Vec<(&Expr, &Expr)> = Vec::new();
    for item in &stmt.projection {
        if let SelectItem::Expr { expr, .. } = item {
            collect_bm25(expr, &mut calls);
        }
    }
    for o in &stmt.order_by {
        collect_bm25(&o.expr, &mut calls);
    }
    for (col, query) in calls {
        let Expr::Column { table: tq, name } = col else {
            return Err(sql_err(
                "bm25(): el primer argumento debe ser una columna indexada".to_string(),
            ));
        };
        let q_str = match eval_const(query, params)? {
            Value::Text(s) => s,
            _ => {
                return Err(sql_err(
                    "bm25(): la consulta debe ser texto constante".to_string(),
                ));
            }
        };
        // Resuelve la columna en un bloque para soltar el préstamo inmutable de
        // `schema` antes de mutar `fts_rank`.
        let (fts_id, tok_name) = {
            let (def, local_pos, _) = schema.owner_of(tq.as_deref(), name)?;
            let fts = def
                .fts_indexes
                .iter()
                .find(|f| f.columns.contains(&local_pos))
                .ok_or_else(|| {
                    sql_err(format!("la columna «{name}» no tiene un índice FULLTEXT"))
                })?;
            (fts.fts_id, fts.tokenizer.clone())
        };
        if schema.fts_rank.contains_key(&(fts_id, q_str.clone())) {
            continue;
        }
        let tk = crate::fts::tokenizer_for(&tok_name)?;
        let q = crate::fts::parse_query(&q_str)?;
        let terms = crate::fts::query_terms(&q, tk.as_ref());
        let (df, n, total) = src.fts_stats(fts_id, &terms)?;
        let nf = n as f64;
        let avgdl = if n > 0 { total as f64 / nf } else { 0.0 };
        let idf: HashMap<String, f64> = terms
            .into_iter()
            .zip(df.iter().map(|&d| {
                let d = d as f64;
                (1.0 + (nf - d + 0.5) / (d + 0.5)).ln()
            }))
            .collect();
        schema
            .fts_rank
            .insert((fts_id, q_str), FtsRankCtx { idf, avgdl });
    }
    Ok(())
}

/// Re-parsea los predicados `CHECK` de una tabla (guardados como texto) a `Expr`,
/// una vez por sentencia. Cada texto se envuelve en `SELECT <expr>` y se extrae.
fn parse_checks(def: &TableDef) -> Result<Vec<Expr>> {
    def.checks
        .iter()
        .map(|text| {
            let Stmt::Select(mut s) = crate::sql::parse(&format!("SELECT {text}"))? else {
                return Err(sql_err("CHECK debe ser una expresión"));
            };
            match s.projection.drain(..).next() {
                Some(SelectItem::Expr { expr, .. }) => Ok(expr),
                _ => Err(sql_err("CHECK debe ser una expresión")),
            }
        })
        .collect()
}

/// Comprueba los `CHECK` (ya parseados) contra una fila. Falla si alguno evalúa a
/// **FALSE**; NULL/TRUE pasan (semántica SQL).
fn check_row(checks: &[Expr], schema: &QuerySchema, row: &[Value], params: &[Value]) -> Result<()> {
    for expr in checks {
        validate_columns(expr, schema)?;
        let v = eval(expr, Some((schema, row)), params)?;
        if !matches!(v, Value::Null) && !truthy(v)? {
            return Err(Error::Constraint("violación de restricción CHECK"));
        }
    }
    Ok(())
}

fn run_insert(
    tx: &mut WriteTx,
    table: &str,
    columns: Option<&[String]>,
    source: &InsertSource,
    params: &[Value],
    on_conflict: Option<&OnConflict>,
    returning: Option<&[SelectItem]>,
) -> Result<(usize, Option<SelectOut>)> {
    // Origen de las filas → filas de literales. El `SELECT` se materializa **entero**
    // antes de insertar nada, así que `INSERT INTO t SELECT … FROM t` es seguro.
    let select_rows;
    let rows: &[Vec<Expr>] = match source {
        InsertSource::Values(v) => v,
        InsertSource::Select(sel) => {
            let out = run_query(&*tx, sel, params)?;
            select_rows = out
                .rows
                .into_iter()
                .map(|r| r.into_iter().map(Expr::Literal).collect::<Vec<Expr>>())
                .collect::<Vec<Vec<Expr>>>();
            &select_rows
        }
    };
    // Resolver el esquema con la versión **cacheada**: un INSERT-por-fila en un lote
    // no re-desciende el catálogo por cada sentencia (el esquema no cambia entre
    // filas). Si no es una tabla, puede ser una vista (la escritura la atiende un
    // trigger INSTEAD OF); solo entonces se consulta el catálogo de vistas — antes se
    // decodificaba la tabla SIN cachear en cada sentencia solo para descartar la vista.
    let def = match tx.table_cached(table)? {
        Some(def) => def,
        None => {
            if tx.view(table)?.is_some() {
                if returning.is_some() || on_conflict.is_some() {
                    return Err(sql_err(
                        "RETURNING / ON CONFLICT no se admiten al escribir en una vista",
                    ));
                }
                return Ok((
                    run_instead_of_insert(tx, table, columns, rows, params)?,
                    None,
                ));
            }
            return Err(sql_err(format!("tabla desconocida: {table}")));
        }
    };
    let (ins_before_row, before_stmt) =
        split_for_each(tx.triggers_for(&def.name, TriggerEvent::Insert, TriggerTiming::Before)?);
    let (ins_after_row, after_stmt) =
        split_for_each(tx.triggers_for(&def.name, TriggerEvent::Insert, TriggerTiming::After)?);
    // La rama DO UPDATE de un UPSERT dispara triggers de UPDATE (row-level).
    let (upd_before_row, _) = split_for_each(if on_conflict.is_some() {
        tx.triggers_for(&def.name, TriggerEvent::Update, TriggerTiming::Before)?
    } else {
        Vec::new()
    });
    let (upd_after_row, _) = split_for_each(if on_conflict.is_some() {
        tx.triggers_for(&def.name, TriggerEvent::Update, TriggerTiming::After)?
    } else {
        Vec::new()
    });
    // Predicados CHECK (parseados una vez; evaluados por fila antes de escribir). El
    // esquema para evaluarlos solo se construye si HAY checks: clonar el `TableDef`
    // por sentencia para nada hundía el INSERT-por-fila en lote (los valores se
    // calculan posicionalmente con `def`, sin esquema).
    let checks = parse_checks(&def)?;
    let check_schema = (!checks.is_empty()).then(|| QuerySchema::single(table, (*def).clone()));
    fire(tx, &def, &before_stmt, None, None)?; // BEFORE … FOR EACH STATEMENT
    // La fila NEW (con el rowid asignado) solo se necesita para AFTER INSERT o RETURNING.
    let want_new = !ins_after_row.is_empty() || returning.is_some();
    let mut returned = Vec::new();
    let mut affected = 0usize;
    // Buffer prestado de la tx: ni un `Vec` de valores por fila. Si una fila falla,
    // el `?` lo pierde — camino frío, el próximo take asigna.
    let mut values = tx.take_values_buf();
    for row in rows {
        insert_values_into(&def, columns, row, params, &mut values)?;
        // UPSERT: ¿la fila propuesta choca con la PK o un índice UNIQUE?
        if let Some(oc) = on_conflict
            && let Some(rid) = find_conflict(tx, &def, &values)?
        {
            match oc {
                OnConflict::Nothing => continue, // omitir (no cuenta como afectada)
                OnConflict::Update { sets, where_clause } => {
                    if let Some(new_row) = upsert_update(
                        tx,
                        &def,
                        table,
                        rid,
                        sets,
                        where_clause.as_ref(),
                        &values,
                        params,
                        &checks,
                        &upd_before_row,
                        &upd_after_row,
                    )? {
                        if returning.is_some() {
                            returned.push(new_row);
                        }
                        affected += 1;
                    }
                    continue;
                }
            }
        }
        if let Some(schema) = &check_schema {
            check_row(&checks, schema, &values, params)?; // CHECK antes de insertar
        }
        fire(tx, &def, &ins_before_row, None, Some(&values))?; // BEFORE INSERT
        let rowid = tx.insert_row(&def, &values)?;
        if want_new {
            let mut new_after = values.clone();
            if let Some(i) = def.rowid_alias {
                new_after[i] = Value::Integer(rowid);
            }
            fire(tx, &def, &ins_after_row, None, Some(&new_after))?; // AFTER INSERT
            if returning.is_some() {
                returned.push(new_after);
            }
        }
        affected += 1;
    }
    tx.put_values_buf(values);
    fire(tx, &def, &after_stmt, None, None)?; // AFTER … FOR EACH STATEMENT
    let out = returning
        .map(|items| eval_returning(table, &def, items, &returned, params))
        .transpose()?;
    Ok((affected, out))
}

/// Rowid de una fila existente que choca con la propuesta `values` (la PK, o un
/// índice UNIQUE), o `None` si no hay conflicto.
fn find_conflict(tx: &WriteTx, def: &TableDef, values: &[Value]) -> Result<Option<i64>> {
    // PK: un rowid explícito que ya existe.
    if let Some(pk) = def.rowid_alias
        && let Some(Value::Integer(n)) = values.get(pk)
        && tx.get_row(def, *n)?.is_some()
    {
        return Ok(Some(*n));
    }
    // UNIQUE: cualquier índice unique cuya clave ya esté presente (un NULL no choca).
    for idx in &def.indexes {
        if !idx.unique {
            continue;
        }
        let key: Vec<Value> = idx.columns.iter().map(|&c| values[c].clone()).collect();
        if key.iter().any(|v| matches!(v, Value::Null)) {
            continue;
        }
        if let Some(&rid) = tx.index_lookup(idx, &key)?.first() {
            return Ok(Some(rid));
        }
    }
    Ok(None)
}

/// Rama `DO UPDATE` de un UPSERT: actualiza la fila en conflicto `rid` con `sets`
/// (donde `excluded.col` = la fila propuesta `proposed`). Devuelve la fila NEW si
/// se aplicó, o `None` si el `WHERE` la descartó.
#[allow(clippy::too_many_arguments)]
fn upsert_update(
    tx: &mut WriteTx,
    def: &TableDef,
    table: &str,
    rid: i64,
    sets: &[(String, Expr)],
    where_clause: Option<&Expr>,
    proposed: &[Value],
    params: &[Value],
    checks: &[Expr],
    before_row: &[TriggerDef],
    after_row: &[TriggerDef],
) -> Result<Option<Vec<Value>>> {
    let schema = QuerySchema::single(table, def.clone());
    let existing = tx
        .get_row(def, rid)?
        .ok_or_else(|| sql_err("la fila en conflicto desapareció"))?;
    // `excluded.col` (la fila propuesta) se sustituye como `NEW`; lo demás se evalúa
    // contra la fila existente.
    if let Some(cond) = where_clause {
        let c = subst_expr(cond, def, None, Some(proposed))?;
        validate_columns(&c, &schema)?;
        if !truthy(eval(&c, Some((&schema, &existing)), params)?)? {
            return Ok(None);
        }
    }
    let mut new_row = existing.clone();
    for (name, expr) in sets {
        let i = col_index(def, name)?;
        if def.rowid_alias == Some(i) {
            return Err(sql_err(
                "ON CONFLICT DO UPDATE no puede cambiar la PRIMARY KEY",
            ));
        }
        let e = subst_expr(expr, def, None, Some(proposed))?;
        no_aggregates(&e, "SET")?;
        validate_columns(&e, &schema)?;
        new_row[i] = eval(&e, Some((&schema, &existing)), params)?;
    }
    check_row(checks, &schema, &new_row, params)?; // CHECK sobre la fila actualizada
    fire(tx, def, before_row, Some(&existing), Some(&new_row))?;
    tx.update_row(def, rid, &new_row)?;
    fire(tx, def, after_row, Some(&existing), Some(&new_row))?;
    Ok(Some(new_row))
}

fn run_update(
    tx: &mut WriteTx,
    table: &str,
    sets: &[(String, Expr)],
    where_clause: Option<&Expr>,
    params: &[Value],
    returning: Option<&[SelectItem]>,
) -> Result<(usize, Option<SelectOut>)> {
    // Esquema cacheado (ver `run_insert`): nada de decodificar la tabla sin cachear
    // por sentencia solo para descartar el caso «vista».
    let def = match tx.table_cached(table)? {
        Some(def) => def,
        None => {
            if tx.view(table)?.is_some() {
                if returning.is_some() {
                    return Err(sql_err("RETURNING no se admite al escribir en una vista"));
                }
                return Ok((
                    run_instead_of_update(tx, table, sets, where_clause, params)?,
                    None,
                ));
            }
            return Err(sql_err(format!("tabla desconocida: {table}")));
        }
    };
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
    let checks = parse_checks(&def)?;
    let mut returned = Vec::new(); // filas NEW para RETURNING
    fire(tx, &def, &before_stmt, None, None)?; // BEFORE … FOR EACH STATEMENT
    for (rowid, old_row, new_row) in updates {
        check_row(&checks, &schema, &new_row, params)?; // CHECK sobre la fila NEW
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
    // Esquema cacheado (ver `run_insert`): nada de decodificar la tabla sin cachear
    // por sentencia solo para descartar el caso «vista».
    let def = match tx.table_cached(table)? {
        Some(def) => def,
        None => {
            if tx.view(table)?.is_some() {
                if returning.is_some() {
                    return Err(sql_err("RETURNING no se admite al escribir en una vista"));
                }
                return Ok((
                    run_instead_of_delete(tx, table, where_clause, params)?,
                    None,
                ));
            }
            return Err(sql_err(format!("tabla desconocida: {table}")));
        }
    };
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
        } if q.eq_ignore_ascii_case("OLD")
            || q.eq_ignore_ascii_case("NEW")
            || q.eq_ignore_ascii_case("EXCLUDED") =>
        {
            // `EXCLUDED` (la fila propuesta en un UPSERT) se trata como `NEW`.
            let is_new = !q.eq_ignore_ascii_case("OLD");
            let vals = if is_new { new } else { old }.ok_or_else(|| {
                sql_err(format!(
                    "{} no está disponible aquí",
                    q.to_ascii_uppercase()
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
        Expr::Match {
            column,
            query,
            negated,
        } => Expr::Match {
            column: b(column)?,
            query: b(query)?,
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
        Expr::Window {
            func,
            args,
            partition_by,
            order_by,
            frame,
        } => Expr::Window {
            func: *func,
            args: args.iter().map(r).collect::<Result<_>>()?,
            partition_by: partition_by.iter().map(r).collect::<Result<_>>()?,
            order_by: order_by.clone(),
            frame: *frame,
        },
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
            source,
            on_conflict,
            returning,
        } => Stmt::Insert {
            table: t.clone(),
            columns: columns.clone(),
            // OLD/NEW se sustituyen en las filas literales; un `INSERT … SELECT` en el
            // cuerpo de un trigger se deja igual (otro ámbito).
            source: match source {
                InsertSource::Values(rows) => InsertSource::Values(
                    rows.iter()
                        .map(|row| row.iter().map(&r).collect::<Result<Vec<_>>>())
                        .collect::<Result<Vec<_>>>()?,
                ),
                InsertSource::Select(s) => InsertSource::Select(s.clone()),
            },
            on_conflict: on_conflict.clone(),
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

/// Infiere el `ColType` de una columna de un `CREATE TABLE AS SELECT` a partir de
/// sus valores (los NULL se ignoran). El tipo elegido debe **aceptar todos** los
/// valores (Arkeion es de tipado estricto, INTEGER→REAL es la única promoción).
/// Columna vacía o toda NULL ⇒ TEXT; mezcla incompatible ⇒ error.
fn infer_coltype<'a>(values: impl Iterator<Item = &'a Value>) -> Result<ColType> {
    let (mut int, mut real, mut text, mut boolean, mut blob) = (false, false, false, false, false);
    for v in values {
        match v {
            Value::Null => {}
            Value::Integer(_) => int = true,
            Value::Real(_) => real = true,
            Value::Text(_) => text = true,
            Value::Bool(_) => boolean = true,
            Value::Blob(_) => blob = true,
        }
    }
    let numeric = int || real;
    let families = [numeric, text, boolean, blob]
        .iter()
        .filter(|x| **x)
        .count();
    if families > 1 {
        return Err(sql_err(
            "CREATE TABLE AS SELECT: una columna mezcla tipos incompatibles",
        ));
    }
    Ok(if blob {
        ColType::Blob
    } else if boolean {
        ColType::Boolean
    } else if text {
        ColType::Text
    } else if real {
        ColType::Real
    } else if int {
        ColType::Integer
    } else {
        ColType::Text // toda NULL / vacía
    })
}

fn table_spec(
    name: &str,
    columns: &[ColumnAst],
    foreign_keys: &[catalog::ForeignKeySpec],
    uniques: &[Vec<String>],
    checks: &[String],
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
            unique: col.unique,
            check: col.check.clone(),
        });
    }
    Ok(TableSpec {
        name: name.to_owned(),
        columns: specs,
        foreign_keys: foreign_keys.to_vec(),
        uniques: uniques.to_vec(),
        checks: checks.to_vec(),
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
    // Precomputa el contexto BM25 de las llamadas `bm25(col, q)` (necesita stats
    // del índice, disponibles aquí vía `src`); `eval` lo lee por fila del schema.
    precompute_bm25(src, &mut schema, stmt, params)?;

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
        } else if let Some(fts_rows) = fts_plan(src, &from_def, stmt.where_clause.as_ref(), params)?
        {
            // Narrowing FTS: un `MATCH` no negado de nivel superior usa el índice
            // en vez del full scan. El WHERE completo se re-aplica por fila.
            fts_rows
        } else if let Some(vec_rows) = vector_plan(src, &from_def, stmt, params)? {
            // KNN por índice IVF (ANN): candidatos de los clusters más cercanos;
            // el ORDER BY/LIMIT los rankea exacto después.
            vec_rows
        } else if let Some(knn_rows) = exact_knn_plan(src, &from_def, stmt, params)? {
            // KNN EXACTO sin índice: top-k en streaming, sin materializar la tabla.
            knn_rows
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

    // Funciones de ventana en la proyección: camino propio (calculan sobre el
    // conjunto filtrado, particionado y ordenado, antes del ORDER BY/LIMIT externo).
    if stmt
        .projection
        .iter()
        .any(|it| matches!(it, SelectItem::Expr { expr, .. } if expr.has_window()))
    {
        return run_windowed(stmt, &schema, rows, params);
    }

    if !stmt.order_by.is_empty() {
        // Cada clave de ORDER BY es una expresión sobre la fila de entrada (una
        // columna, un alias/posición de la proyección, o cualquier cálculo). Se
        // pre-calculan los valores de clave por fila y se ordena por ellos.
        let mut keyed: Vec<(Vec<Value>, Vec<Value>)> = Vec::with_capacity(rows.len());
        for row in rows {
            let mut keys = Vec::with_capacity(stmt.order_by.len());
            for o in &stmt.order_by {
                let e = order_key_expr(o, &stmt.projection)?;
                validate_columns(e, &schema)?;
                keys.push(eval(e, Some((&schema, &row)), params)?);
            }
            keyed.push((keys, row));
        }
        let descs: Vec<bool> = stmt.order_by.iter().map(|o| o.desc).collect();
        let mut first_err: Option<Error> = None;
        keyed.sort_by(|(ka, _), (kb, _)| {
            for (i, desc) in descs.iter().enumerate() {
                match cmp_nulls_first(&ka[i], &kb[i]) {
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
        rows = keyed.into_iter().map(|(_, row)| row).collect();
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

    fn fts_search(
        &self,
        table: &TableDef,
        fts: &FtsIndexDef,
        query: &crate::fts::Query,
    ) -> Result<Vec<i64>> {
        self.inner.fts_search(table, fts, query)
    }

    fn fts_stats(&self, fts_id: u32, terms: &[String]) -> Result<(Vec<u64>, u64, u64)> {
        self.inner.fts_stats(fts_id, terms)
    }

    fn vector_search(
        &self,
        vidx: &VectorIndexDef,
        query: &[f32],
        nprobe: usize,
        limit: usize,
    ) -> Result<Vec<i64>> {
        self.inner.vector_search(vidx, query, nprobe, limit)
    }
    fn knn_exact(
        &self,
        table: &TableDef,
        col: usize,
        query: &[u8],
        metric: VectorMetric,
        k: usize,
    ) -> Result<Vec<i64>> {
        self.inner.knn_exact(table, col, query, metric, k)
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

    fn fts_search(
        &self,
        table: &TableDef,
        fts: &FtsIndexDef,
        query: &crate::fts::Query,
    ) -> Result<Vec<i64>> {
        self.inner.fts_search(table, fts, query)
    }

    fn fts_stats(&self, fts_id: u32, terms: &[String]) -> Result<(Vec<u64>, u64, u64)> {
        self.inner.fts_stats(fts_id, terms)
    }

    fn vector_search(
        &self,
        vidx: &VectorIndexDef,
        query: &[f32],
        nprobe: usize,
        limit: usize,
    ) -> Result<Vec<i64>> {
        self.inner.vector_search(vidx, query, nprobe, limit)
    }
    fn knn_exact(
        &self,
        table: &TableDef,
        col: usize,
        query: &[u8],
        metric: VectorMetric,
        k: usize,
    ) -> Result<Vec<i64>> {
        self.inner.knn_exact(table, col, query, metric, k)
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
        fts_indexes: Vec::new(),
        vector_indexes: Vec::new(),
        logical_order: (0..n).collect(),
        foreign_keys: Vec::new(),
        checks: Vec::new(),
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
        Expr::Match { column, query, .. } => expr_has_subquery(column) || expr_has_subquery(query),
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
        Expr::Window {
            args, partition_by, ..
        } => args.iter().any(expr_has_subquery) || partition_by.iter().any(expr_has_subquery),
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
        Expr::Match { column, query, .. } => {
            expr_refs_quals(column, quals) || expr_refs_quals(query, quals)
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
        Expr::Window {
            args,
            partition_by,
            order_by,
            ..
        } => args
            .iter()
            .chain(partition_by)
            .chain(order_by.iter().map(|o| &o.expr))
            .any(|x| expr_refs_quals(x, quals)),
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
        Expr::Match {
            column,
            query,
            negated,
        } => Expr::Match {
            column: boxed(column)?,
            query: boxed(query)?,
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
        Expr::Window {
            func,
            args,
            partition_by,
            order_by,
            frame,
        } => Expr::Window {
            func: *func,
            args: args.iter().map(res).collect::<Result<_>>()?,
            partition_by: partition_by.iter().map(res).collect::<Result<_>>()?,
            order_by: order_by.clone(),
            frame: *frame,
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
        Expr::Match {
            column,
            query,
            negated,
        } => Expr::Match {
            column: b(column)?,
            query: b(query)?,
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
        Expr::Window {
            func,
            args,
            partition_by,
            order_by,
            frame,
        } => Expr::Window {
            func: *func,
            args: args.iter().map(r).collect::<Result<_>>()?,
            partition_by: partition_by.iter().map(r).collect::<Result<_>>()?,
            order_by: order_by.clone(),
            frame: *frame,
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
        Expr::Match {
            column,
            query,
            negated,
        } => Expr::Match {
            column: b(column)?,
            query: b(query)?,
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
        Expr::Window {
            func,
            args,
            partition_by,
            order_by,
            frame,
        } => Expr::Window {
            func: *func,
            args: args.iter().map(rec).collect::<Result<_>>()?,
            partition_by: partition_by.iter().map(rec).collect::<Result<_>>()?,
            order_by: order_by.clone(),
            frame: *frame,
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

    // ORDER BY sobre las columnas de SALIDA de la unión (por nombre o posición).
    if !stmt.order_by.is_empty() {
        let order: Vec<(usize, bool)> = stmt
            .order_by
            .iter()
            .map(|o| Ok((order_output_index(o, &columns)?, o.desc)))
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
        Expr::Match { column, query, .. } => {
            col_outside_agg(column).or_else(|| col_outside_agg(query))
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
        // Las ventanas no conviven con GROUP BY (se rechazan antes); por completitud.
        Expr::Window { .. } => None,
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
        Expr::Match { column, query, .. } => {
            collect_columns(column, skip_agg, out);
            collect_columns(query, skip_agg, out);
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
        Expr::Window {
            args, partition_by, ..
        } => {
            args.iter()
                .chain(partition_by)
                .for_each(|x| collect_columns(x, skip_agg, out));
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

    // ORDER BY sobre las columnas de SALIDA (por nombre o posición).
    if !stmt.order_by.is_empty() {
        let order: Vec<(usize, bool)> = stmt
            .order_by
            .iter()
            .map(|o| Ok((order_output_index(o, &columns)?, o.desc)))
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

// --- funciones de ventana (OVER) ---

/// Ejecuta un SELECT con funciones de ventana en la proyección. Las ventanas se
/// calculan sobre `rows` (el conjunto ya filtrado), particionadas por
/// `PARTITION BY` y ordenadas por el `ORDER BY` de cada `OVER`; luego la
/// proyección sustituye cada ventana por su valor de fila y se aplican
/// `DISTINCT`/`ORDER BY` externos y `LIMIT`.
fn run_windowed(
    stmt: &SelectStmt,
    schema: &QuerySchema,
    rows: Vec<Vec<Value>>,
    params: &[Value],
) -> Result<SelectOut> {
    // Validar columnas y recolectar las expresiones de ventana DISTINTAS.
    let mut windows: Vec<&Expr> = Vec::new();
    for item in &stmt.projection {
        if let SelectItem::Expr { expr, .. } = item {
            validate_columns(expr, schema)?;
            collect_windows(expr, &mut windows);
        }
    }
    // Calcular cada ventana → una columna de valores (uno por fila de `rows`).
    let computed: Vec<Vec<Value>> = windows
        .iter()
        .map(|w| compute_window(w, schema, &rows, params))
        .collect::<Result<_>>()?;

    // Mapa de expansión de `*` (orden lógico, sin columnas borradas).
    let mut star_map: Vec<usize> = Vec::new();
    let mut base = 0;
    for t in &schema.tables {
        for &phys in &t.def.logical_order {
            if !t.def.columns[phys].dropped {
                star_map.push(base + phys);
            }
        }
        base += t.def.columns.len();
    }

    // Nombres de columnas de salida.
    let mut columns = Vec::new();
    for (i, item) in stmt.projection.iter().enumerate() {
        match item {
            SelectItem::Star => {
                for t in &schema.tables {
                    for &phys in &t.def.logical_order {
                        if !t.def.columns[phys].dropped {
                            columns.push(t.def.columns[phys].name.clone());
                        }
                    }
                }
            }
            SelectItem::Expr { alias: Some(a), .. } => columns.push(a.clone()),
            SelectItem::Expr {
                expr: Expr::Column { name, .. },
                ..
            } => columns.push(name.clone()),
            SelectItem::Expr { .. } => columns.push(format!("col{}", i + 1)),
        }
    }

    // Proyectar: por fila, sustituir las ventanas por su literal y evaluar.
    let mut out_rows = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let mut out = Vec::with_capacity(columns.len());
        for item in &stmt.projection {
            match item {
                SelectItem::Star => out.extend(star_map.iter().map(|&p| row[p].clone())),
                SelectItem::Expr { expr, .. } => {
                    let e = subst_windows(expr, &windows, &computed, i);
                    out.push(eval(&e, Some((schema, row)), params)?);
                }
            }
        }
        out_rows.push(out);
    }

    // ORDER BY externo: por columna de SALIDA (alias/proyección) o por una columna
    // de tabla del esquema (que sigue disponible en `rows`, en paralelo a `out_rows`).
    if !stmt.order_by.is_empty() {
        enum Key {
            Out(usize),
            Schema(usize),
        }
        let mut keys: Vec<(Key, bool)> = Vec::new();
        for o in &stmt.order_by {
            match &o.expr {
                Expr::Column { table: None, name } if columns.iter().any(|c| c == name) => {
                    let idx = columns
                        .iter()
                        .position(|c| c == name)
                        .expect("recién comprobado");
                    keys.push((Key::Out(idx), o.desc));
                }
                Expr::Literal(Value::Integer(n)) if *n >= 1 && (*n as usize) <= columns.len() => {
                    keys.push((Key::Out(*n as usize - 1), o.desc));
                }
                Expr::Column { table, name } => {
                    keys.push((Key::Schema(schema.resolve(table.as_deref(), name)?), o.desc));
                }
                _ => {
                    return Err(sql_err(
                        "ORDER BY con ventanas solo admite una columna o su posición",
                    ));
                }
            }
        }
        let mut order: Vec<usize> = (0..out_rows.len()).collect();
        let mut first_err: Option<Error> = None;
        order.sort_by(|&a, &b| {
            for (k, desc) in &keys {
                let (va, vb) = match k {
                    Key::Out(i) => (&out_rows[a][*i], &out_rows[b][*i]),
                    Key::Schema(i) => (&rows[a][*i], &rows[b][*i]),
                };
                match cmp_nulls_first(va, vb) {
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
        out_rows = order.into_iter().map(|i| out_rows[i].clone()).collect();
    }
    if stmt.distinct {
        out_rows = dedup_preserving(out_rows);
    }
    let out_rows = limit_offset(out_rows, stmt, params)?;
    Ok(SelectOut {
        columns,
        rows: out_rows,
    })
}

/// Recolecta (sin repetir) los nodos `Window` de `e`.
fn collect_windows<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
    match e {
        Expr::Window { .. } if !out.iter().any(|w| **w == *e) => out.push(e),
        Expr::Window { .. } => {}
        Expr::Unary(_, x) | Expr::IsNull { expr: x, .. } | Expr::Cast { expr: x, .. } => {
            collect_windows(x, out)
        }
        Expr::Binary(a, _, b)
        | Expr::Like {
            expr: a,
            pattern: b,
            ..
        } => {
            collect_windows(a, out);
            collect_windows(b, out);
        }
        Expr::Function { args, .. } => args.iter().for_each(|a| collect_windows(a, out)),
        Expr::In { expr, list, .. } => {
            collect_windows(expr, out);
            list.iter().for_each(|x| collect_windows(x, out));
        }
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(o) = operand {
                collect_windows(o, out);
            }
            for (c, r) in whens {
                collect_windows(c, out);
                collect_windows(r, out);
            }
            if let Some(x) = else_ {
                collect_windows(x, out);
            }
        }
        _ => {}
    }
}

/// Sustituye cada nodo `Window` de `e` por el literal precalculado para la fila `i`.
fn subst_windows(e: &Expr, windows: &[&Expr], col: &[Vec<Value>], i: usize) -> Expr {
    if matches!(e, Expr::Window { .. }) {
        if let Some(idx) = windows.iter().position(|w| **w == *e) {
            return Expr::Literal(col[idx][i].clone());
        }
        return e.clone();
    }
    let r = |x: &Expr| subst_windows(x, windows, col, i);
    let b = |x: &Expr| Box::new(r(x));
    match e {
        Expr::Unary(op, x) => Expr::Unary(*op, b(x)),
        Expr::Binary(a, op, c) => Expr::Binary(b(a), *op, b(c)),
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: b(expr),
            negated: *negated,
        },
        Expr::Like {
            expr,
            pattern,
            negated,
        } => Expr::Like {
            expr: b(expr),
            pattern: b(pattern),
            negated: *negated,
        },
        Expr::Function { name, args } => Expr::Function {
            name: name.clone(),
            args: args.iter().map(r).collect(),
        },
        Expr::In {
            expr,
            list,
            negated,
        } => Expr::In {
            expr: b(expr),
            list: list.iter().map(r).collect(),
            negated: *negated,
        },
        Expr::Cast { expr, to } => Expr::Cast {
            expr: b(expr),
            to: *to,
        },
        Expr::Case {
            operand,
            whens,
            else_,
        } => Expr::Case {
            operand: operand.as_deref().map(b),
            whens: whens.iter().map(|(c, rr)| (r(c), r(rr))).collect(),
            else_: else_.as_deref().map(b),
        },
        other => other.clone(),
    }
}

/// Calcula una función de ventana sobre todas las filas: devuelve un valor por
/// fila (en el orden original de `rows`).
fn compute_window(
    w: &Expr,
    schema: &QuerySchema,
    rows: &[Vec<Value>],
    params: &[Value],
) -> Result<Vec<Value>> {
    let Expr::Window {
        func,
        args,
        partition_by,
        order_by,
        frame,
    } = w
    else {
        unreachable!("compute_window recibe un Window")
    };
    let mut result = vec![Value::Null; rows.len()];

    // Particionar los índices de fila por la clave de PARTITION BY.
    let mut parts: Vec<Vec<usize>> = Vec::new();
    let mut pindex: std::collections::HashMap<Vec<u8>, usize> = std::collections::HashMap::new();
    for (i, row) in rows.iter().enumerate() {
        let key: Vec<Value> = partition_by
            .iter()
            .map(|e| eval(e, Some((schema, row)), params))
            .collect::<Result<_>>()?;
        let kb = crate::record::encode_values(&key);
        let pi = *pindex.entry(kb).or_insert_with(|| {
            parts.push(Vec::new());
            parts.len() - 1
        });
        parts[pi].push(i);
    }

    // Pre-evaluar las claves de ORDER BY de la ventana por fila (expresiones), en
    // paralelo a `rows`. El ORDER BY de `OVER` admite cualquier expresión.
    let order_keys: Vec<Vec<Value>> = rows
        .iter()
        .map(|row| {
            order_by
                .iter()
                .map(|o| eval(&o.expr, Some((schema, row)), params))
                .collect::<Result<_>>()
        })
        .collect::<Result<_>>()?;
    let descs: Vec<bool> = order_by.iter().map(|o| o.desc).collect();
    let has_order = !order_by.is_empty();

    for part in &mut parts {
        // Ordenar los índices de la partición por las claves de ventana (estable).
        if has_order {
            let mut err: Option<Error> = None;
            part.sort_by(|&a, &b| {
                for (i, desc) in descs.iter().enumerate() {
                    match cmp_nulls_first(&order_keys[a][i], &order_keys[b][i]) {
                        Ok(Ordering::Equal) => continue,
                        Ok(o) => return if *desc { o.reverse() } else { o },
                        Err(e) => {
                            err.get_or_insert(e);
                            return Ordering::Equal;
                        }
                    }
                }
                Ordering::Equal
            });
            if let Some(e) = err {
                return Err(e);
            }
        }
        window_partition(
            *func,
            args,
            frame.as_ref(),
            &order_keys,
            has_order,
            part,
            schema,
            rows,
            params,
            &mut result,
        )?;
    }
    Ok(result)
}

/// Rango `[inicio, fin]` (inclusive, índices dentro de la partición) del marco para
/// la fila `k` de una partición de tamaño `m`. `None` si el marco está vacío. Sin
/// marco explícito: desde el inicio a la fila actual (con ORDER BY) o a toda la
/// partición (sin ORDER BY).
fn frame_range(
    frame: Option<&WindowFrame>,
    k: usize,
    m: usize,
    has_order: bool,
) -> Option<(usize, usize)> {
    let (start_b, end_b) = match frame {
        Some(f) => (f.start, f.end),
        None if has_order => (FrameBound::UnboundedPreceding, FrameBound::CurrentRow),
        None => (
            FrameBound::UnboundedPreceding,
            FrameBound::UnboundedFollowing,
        ),
    };
    let (k, m) = (k as isize, m as isize);
    let bound = |b: FrameBound, is_start: bool| -> isize {
        match b {
            FrameBound::UnboundedPreceding if is_start => 0,
            FrameBound::UnboundedPreceding => -1, // como fin ⇒ vacío
            FrameBound::Preceding(n) => k - n as isize,
            FrameBound::CurrentRow => k,
            FrameBound::Following(n) => k + n as isize,
            FrameBound::UnboundedFollowing if is_start => m, // como inicio ⇒ vacío
            FrameBound::UnboundedFollowing => m - 1,
        }
    };
    let start = bound(start_b, true).max(0);
    let end = bound(end_b, false).min(m - 1);
    (start <= end).then_some((start as usize, end as usize))
}

/// Aplica la función de ventana dentro de UNA partición ya ordenada (`part` son
/// los índices de fila en orden de ventana), escribiendo en `result`.
#[allow(clippy::too_many_arguments)]
fn window_partition(
    func: WindowFunc,
    args: &[Expr],
    frame: Option<&WindowFrame>,
    order_keys: &[Vec<Value>],
    has_order: bool,
    part: &[usize],
    schema: &QuerySchema,
    rows: &[Vec<Value>],
    params: &[Value],
    result: &mut [Value],
) -> Result<()> {
    let m = part.len();
    // El argumento de valor (LAG/LEAD/FIRST/LAST/SUM/…) es `args[0]`.
    let arg0 =
        |idx: usize| -> Result<Value> { eval(&args[0], Some((schema, &rows[part[idx]])), params) };
    match func {
        WindowFunc::RowNumber => {
            for (k, &i) in part.iter().enumerate() {
                result[i] = Value::Integer((k + 1) as i64);
            }
        }
        WindowFunc::Rank | WindowFunc::DenseRank => {
            let mut rank = 0i64;
            let mut dense = 0i64;
            let mut prev: Option<&Vec<Value>> = None;
            for (k, &i) in part.iter().enumerate() {
                let key = &order_keys[i];
                if !has_order || prev != Some(key) {
                    rank = (k + 1) as i64;
                    dense += 1;
                }
                result[i] = Value::Integer(if matches!(func, WindowFunc::Rank) {
                    rank
                } else {
                    dense
                });
                prev = Some(key);
            }
        }
        WindowFunc::Ntile => {
            let k = match eval_const(&args[0], params)? {
                Value::Integer(n) if n >= 1 => n as usize,
                _ => return Err(sql_err("NTILE() requiere un entero positivo")),
            };
            let base = m / k;
            let rem = m % k;
            for (p, &i) in part.iter().enumerate() {
                let big = (base + 1) * rem; // filas en los `rem` primeros tiles (de tamaño base+1)
                let tile = if p < big {
                    p / (base + 1) + 1
                } else {
                    rem + (p - big) / base.max(1) + 1
                };
                result[i] = Value::Integer(tile as i64);
            }
        }
        WindowFunc::Lag | WindowFunc::Lead => {
            let offset = match args.get(1) {
                None => 1isize,
                Some(e) => match eval_const(e, params)? {
                    Value::Integer(n) => n as isize,
                    _ => return Err(sql_err("el desplazamiento de LAG/LEAD debe ser entero")),
                },
            };
            for (k, &i) in part.iter().enumerate() {
                let target = if matches!(func, WindowFunc::Lag) {
                    k as isize - offset
                } else {
                    k as isize + offset
                };
                result[i] = if target >= 0 && (target as usize) < m {
                    arg0(target as usize)?
                } else if let Some(def) = args.get(2) {
                    eval(def, Some((schema, &rows[i])), params)?
                } else {
                    Value::Null
                };
            }
        }
        WindowFunc::FirstValue | WindowFunc::LastValue => {
            for (k, &i) in part.iter().enumerate() {
                result[i] = match frame_range(frame, k, m, has_order) {
                    Some((s, e)) => {
                        let target = if matches!(func, WindowFunc::FirstValue) {
                            s
                        } else {
                            e
                        };
                        arg0(target)?
                    }
                    None => Value::Null, // marco vacío
                };
            }
        }
        WindowFunc::Sum
        | WindowFunc::Count
        | WindowFunc::Avg
        | WindowFunc::Min
        | WindowFunc::Max => {
            for (k, &i) in part.iter().enumerate() {
                let frame_slice: &[usize] = match frame_range(frame, k, m, has_order) {
                    Some((s, e)) => &part[s..=e],
                    None => &[],
                };
                result[i] = window_aggregate(func, args, frame_slice, schema, rows, params)?;
            }
        }
    }
    Ok(())
}

/// Agrega `args[0]` (o cuenta filas, en `COUNT(*)`) sobre las filas `frame` de una
/// partición. NULL no cuenta; con todo NULL/0 filas devuelve NULL (salvo COUNT → 0).
fn window_aggregate(
    func: WindowFunc,
    args: &[Expr],
    frame: &[usize],
    schema: &QuerySchema,
    rows: &[Vec<Value>],
    params: &[Value],
) -> Result<Value> {
    let val = |i: usize| -> Result<Value> { eval(&args[0], Some((schema, &rows[i])), params) };
    match func {
        WindowFunc::Count => {
            if args.is_empty() {
                return Ok(Value::Integer(frame.len() as i64));
            }
            let mut c = 0i64;
            for &i in frame {
                if !matches!(val(i)?, Value::Null) {
                    c += 1;
                }
            }
            Ok(Value::Integer(c))
        }
        WindowFunc::Sum | WindowFunc::Avg => {
            let (mut isum, mut fsum, mut all_int, mut cnt) = (0i64, 0f64, true, 0i64);
            for &i in frame {
                match val(i)? {
                    Value::Null => {}
                    Value::Integer(n) => {
                        cnt += 1;
                        isum = isum.wrapping_add(n);
                        fsum += n as f64;
                    }
                    Value::Real(f) => {
                        cnt += 1;
                        all_int = false;
                        fsum += f;
                    }
                    v => {
                        return Err(sql_err(format!(
                            "SUM/AVG requieren números, no {}",
                            v.type_name()
                        )));
                    }
                }
            }
            if cnt == 0 {
                return Ok(Value::Null);
            }
            Ok(if matches!(func, WindowFunc::Sum) {
                if all_int {
                    Value::Integer(isum)
                } else {
                    Value::Real(fsum)
                }
            } else {
                Value::Real(fsum / cnt as f64)
            })
        }
        WindowFunc::Min | WindowFunc::Max => {
            let mut best: Option<Value> = None;
            for &i in frame {
                let v = val(i)?;
                if matches!(v, Value::Null) {
                    continue;
                }
                best = Some(match best {
                    None => v,
                    Some(b) => match cmp_values(&v, &b)? {
                        Some(Ordering::Less) if matches!(func, WindowFunc::Min) => v,
                        Some(Ordering::Greater) if matches!(func, WindowFunc::Max) => v,
                        _ => b,
                    },
                });
            }
            Ok(best.unwrap_or(Value::Null))
        }
        _ => unreachable!("window_aggregate solo agregados"),
    }
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
        Expr::Match {
            column,
            query,
            negated,
        } => Expr::Match {
            column: Box::new(fold_aggregates(column, schema, rows, params)?),
            query: Box::new(fold_aggregates(query, schema, rows, params)?),
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
        // Las ventanas no conviven con GROUP BY/HAVING (se rechazan antes).
        Expr::Window { .. } => e.clone(),
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

/// `snippet(col, q[, open, close, ellipsis, max])` / `highlight(col, q[, open,
/// close])`: resuelven `col` a su índice FTS (para usar el **mismo** tokenizer) y
/// generan el extracto del texto de la fila. Puro, sin tocar el índice.
fn eval_excerpt(
    name: &str,
    args: &[Expr],
    row: Option<(&QuerySchema, &[Value])>,
    params: &[Value],
) -> Result<Value> {
    let Some((schema, full_row)) = row else {
        return Err(sql_err(format!("{name}() necesita una fila")));
    };
    if args.len() < 2 {
        return Err(sql_err(format!(
            "{name}(columna, consulta, …) requiere al menos 2 argumentos"
        )));
    }
    let Expr::Column {
        table: tq,
        name: col,
    } = &args[0]
    else {
        return Err(sql_err(format!(
            "{name}(): el primer argumento debe ser una columna indexada"
        )));
    };
    let (def, local_pos, offset) = schema.owner_of(tq.as_deref(), col)?;
    let fts = def
        .fts_indexes
        .iter()
        .find(|f| f.columns.contains(&local_pos))
        .ok_or_else(|| sql_err(format!("la columna «{col}» no tiene un índice FULLTEXT")))?;
    let tk = crate::fts::tokenizer_for(&fts.tokenizer)?;
    let text = match &full_row[offset + local_pos] {
        Value::Text(t) => t.clone(),
        Value::Null => return Ok(Value::Null),
        _ => return Err(sql_err(format!("{name}() solo aplica a columnas TEXT"))),
    };
    let q = match eval(&args[1], row, params)? {
        Value::Text(s) => crate::fts::parse_query(&s)?,
        Value::Null => return Ok(Value::Null),
        _ => return Err(sql_err(format!("{name}(): la consulta debe ser texto"))),
    };
    // Argumentos de texto opcionales (marcadores / elipsis) con valor por defecto.
    let str_arg = |i: usize, default: &str| -> Result<String> {
        match args.get(i) {
            None => Ok(default.to_string()),
            Some(e) => match eval(e, row, params)? {
                Value::Text(s) => Ok(s),
                _ => Err(sql_err(format!(
                    "{name}(): el argumento {i} debe ser texto"
                ))),
            },
        }
    };
    let open = str_arg(2, "[")?;
    let close = str_arg(3, "]")?;
    if name == "highlight" {
        Ok(Value::Text(crate::fts::highlight(
            &text,
            &q,
            tk.as_ref(),
            &open,
            &close,
        )))
    } else {
        let ellipsis = str_arg(4, "…")?;
        let max_tokens = match args.get(5) {
            None => 15usize,
            Some(e) => match eval(e, row, params)? {
                Value::Integer(n) if n > 0 => n as usize,
                _ => {
                    return Err(sql_err(
                        "snippet(): max_tokens debe ser un entero positivo".to_string(),
                    ));
                }
            },
        };
        Ok(Value::Text(crate::fts::snippet(
            &text,
            &q,
            tk.as_ref(),
            &open,
            &close,
            &ellipsis,
            max_tokens,
        )))
    }
}

/// `bm25(col, q)`: relevancia Okapi BM25 de la fila (mayor = más relevante). Usa
/// el contexto precomputado (`idf`/`avgdl`) del `schema` + la frecuencia de
/// término y la longitud del documento de esta fila (todas sus columnas
/// indexadas). `ORDER BY bm25(col, q) DESC` ordena por relevancia.
fn eval_bm25(
    args: &[Expr],
    row: Option<(&QuerySchema, &[Value])>,
    params: &[Value],
) -> Result<Value> {
    const K1: f64 = 1.2;
    const B: f64 = 0.75;
    let Some((schema, full_row)) = row else {
        return Err(sql_err("bm25() necesita una fila".to_string()));
    };
    if args.len() != 2 {
        return Err(sql_err(
            "bm25(columna, consulta) requiere 2 argumentos".to_string(),
        ));
    }
    let Expr::Column { table: tq, name } = &args[0] else {
        return Err(sql_err(
            "bm25(): el primer argumento debe ser una columna indexada".to_string(),
        ));
    };
    let q_str = match eval(&args[1], row, params)? {
        Value::Text(s) => s,
        Value::Null => return Ok(Value::Null),
        _ => return Err(sql_err("bm25(): la consulta debe ser texto".to_string())),
    };
    let (def, local_pos, offset) = schema.owner_of(tq.as_deref(), name)?;
    let fts = def
        .fts_indexes
        .iter()
        .find(|f| f.columns.contains(&local_pos))
        .ok_or_else(|| sql_err(format!("la columna «{name}» no tiene un índice FULLTEXT")))?;
    let ctx = schema.fts_rank.get(&(fts.fts_id, q_str)).ok_or_else(|| {
        sql_err("bm25() solo se admite en un SELECT directo (sin join/subconsulta)".to_string())
    })?;
    // Documento = tokens de todas las columnas indexadas de esta fila.
    let tk = crate::fts::tokenizer_for(&fts.tokenizer)?;
    let local = &full_row[offset..offset + def.columns.len()];
    let mut tf: HashMap<String, u32> = HashMap::new();
    let mut dl: u64 = 0;
    let mut buf = Vec::new();
    for &col in &fts.columns {
        buf.clear();
        if let Some(Value::Text(text)) = local.get(col) {
            tk.tokenize(text, &mut buf);
        }
        for token in &buf {
            dl += 1;
            if ctx.idf.contains_key(&token.text) {
                *tf.entry(token.text.clone()).or_insert(0) += 1;
            }
        }
    }
    let dl = dl as f64;
    let mut score = 0.0;
    for (term, idf) in &ctx.idf {
        let f = *tf.get(term).unwrap_or(&0) as f64;
        if f == 0.0 {
            continue;
        }
        let norm = if ctx.avgdl > 0.0 { dl / ctx.avgdl } else { 1.0 };
        score += idf * (f * (K1 + 1.0)) / (f + K1 * (1.0 - B + B * norm));
    }
    Ok(Value::Real(score))
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
        // Las ventanas se resuelven a literales por fila antes de eval; si una
        // llega aquí, está en un contexto no soportado (WHERE, agregado, …).
        Expr::Window { .. } => Err(sql_err(
            "las funciones de ventana (OVER) solo se permiten en la lista del SELECT",
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
        Expr::Match {
            column,
            query,
            negated,
        } => {
            let Expr::Column { table: tq, name } = column.as_ref() else {
                return Err(sql_err(
                    "MATCH requiere una columna indexada a la izquierda".to_string(),
                ));
            };
            let Some((schema, full_row)) = row else {
                return Err(sql_err(
                    "MATCH no puede evaluarse fuera de una fila".to_string(),
                ));
            };
            let (def, local_pos, offset) = schema.owner_of(tq.as_deref(), name)?;
            let fts = def
                .fts_indexes
                .iter()
                .find(|f| f.columns.contains(&local_pos))
                .ok_or_else(|| {
                    sql_err(format!("la columna «{name}» no tiene un índice FULLTEXT"))
                })?;
            match eval(query, row, params)? {
                Value::Null => Ok(Value::Null),
                Value::Text(q_str) => {
                    let q = crate::fts::parse_query(&q_str)?;
                    let local = &full_row[offset..offset + def.columns.len()];
                    let matched = crate::catalog::fts_row_matches(def, fts, local, &q)?;
                    Ok(Value::Bool(matched != *negated))
                }
                _ => Err(sql_err("la consulta MATCH debe ser texto".to_string())),
            }
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
                // `->`/`->>`: extracción JSON. La clave/ruta derecha puede ser una
                // ruta `$.a.b`, una clave de objeto (`'a'` → `$.a`) o un índice
                // entero (`0` → `$[0]`). `->` devuelve el nodo como JSON (texto),
                // `->>` como valor SQL. Ruta inexistente ⇒ NULL.
                BinOp::JsonGet | BinOp::JsonGetText => {
                    json_arrow(*op, l, eval(right, row, params)?)
                }
            }
        }
        Expr::Function { name, args } if matches!(name.as_str(), "snippet" | "highlight") => {
            eval_excerpt(name, args, row, params)
        }
        Expr::Function { name, args } if name == "bm25" => eval_bm25(args, row, params),
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
        // --- JSON (estilo SQLite JSON1; parser en Rust puro, sin deps) ---
        "json" => match args {
            [Value::Null] => Ok(Value::Null),
            [Value::Text(s)] => json::parse(s)
                .map(|j| Value::Text(json::to_string(&j)))
                .ok_or_else(|| sql_err("json(): texto JSON inválido")),
            [v] => Err(sql_err(format!("json() espera TEXT, no {}", v.type_name()))),
            _ => Err(bad_arity()),
        },
        "json_valid" => match args {
            [Value::Null] => Ok(Value::Null),
            [Value::Text(s)] => Ok(Value::Bool(json::parse(s).is_some())),
            [_] => Ok(Value::Bool(false)),
            _ => Err(bad_arity()),
        },
        "json_type" => {
            let (text, path) = match args {
                [Value::Null] | [Value::Null, _] | [_, Value::Null] => return Ok(Value::Null),
                [Value::Text(s)] => (s, None),
                [Value::Text(s), Value::Text(p)] => (s, Some(p.as_str())),
                _ => return Err(sql_err("json_type(json TEXT [, path TEXT])")),
            };
            let root = json::parse(text).ok_or_else(|| sql_err("json_type(): JSON inválido"))?;
            let node = match path {
                Some(p) => json::extract(&root, p),
                None => Some(&root),
            };
            Ok(node.map_or(Value::Null, |j| Value::Text(json::type_name(j).to_string())))
        }
        "json_extract" => match args {
            [Value::Null, ..] => Ok(Value::Null),
            [Value::Text(s), paths @ ..] if !paths.is_empty() => {
                let root =
                    json::parse(s).ok_or_else(|| sql_err("json_extract(): JSON inválido"))?;
                if let [p] = paths {
                    let path = json_path(p)?;
                    Ok(json::extract(&root, &path).map_or(Value::Null, json_to_value))
                } else {
                    // Varias rutas → array JSON con un elemento por ruta.
                    let mut arr = Vec::with_capacity(paths.len());
                    for p in paths {
                        let path = json_path(p)?;
                        arr.push(
                            json::extract(&root, &path)
                                .cloned()
                                .unwrap_or(json::Json::Null),
                        );
                    }
                    Ok(Value::Text(json::to_string(&json::Json::Array(arr))))
                }
            }
            _ => Err(sql_err("json_extract(json TEXT, path TEXT, …)")),
        },
        "json_array_length" => {
            let (text, path) = match args {
                [Value::Null] | [Value::Null, _] | [_, Value::Null] => return Ok(Value::Null),
                [Value::Text(s)] => (s, None),
                [Value::Text(s), Value::Text(p)] => (s, Some(p.as_str())),
                _ => return Err(sql_err("json_array_length(json TEXT [, path TEXT])")),
            };
            let root =
                json::parse(text).ok_or_else(|| sql_err("json_array_length(): JSON inválido"))?;
            let node = match path {
                Some(p) => json::extract(&root, p),
                None => Some(&root),
            };
            Ok(match node {
                Some(json::Json::Array(a)) => Value::Integer(a.len() as i64),
                _ => Value::Integer(0), // como SQLite: 0 si no es un array
            })
        }
        "json_object" => {
            if !args.len().is_multiple_of(2) {
                return Err(sql_err("json_object() requiere pares clave/valor"));
            }
            let mut o = Vec::with_capacity(args.len() / 2);
            for pair in args.chunks(2) {
                let key = match &pair[0] {
                    Value::Null => {
                        return Err(sql_err("json_object(): la clave no puede ser NULL"));
                    }
                    v => value_text(v).ok_or_else(|| need_text(v))?,
                };
                o.push((key, value_to_json(&pair[1])?));
            }
            Ok(Value::Text(json::to_string(&json::Json::Object(o))))
        }
        "json_array" => {
            let mut a = Vec::with_capacity(args.len());
            for v in args {
                a.push(value_to_json(v)?);
            }
            Ok(Value::Text(json::to_string(&json::Json::Array(a))))
        }
        "json_quote" => match args {
            [v] => Ok(Value::Text(json::to_string(&value_to_json(v)?))),
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
        // --- búsqueda vectorial: el vector es un BLOB de f32 (docs/13) ---
        "vector" | "vector_i8" => {
            // Constructores: empaquetan los argumentos numéricos como un vector
            // f32 (`vector`) o int8 quantizado (`vector_i8`, ~4× menos storage).
            let mut vals = Vec::with_capacity(args.len());
            for v in args {
                match v {
                    Value::Real(r) => vals.push(*r as f32),
                    Value::Integer(n) => vals.push(*n as f32),
                    Value::Null => return Ok(Value::Null),
                    _ => return Err(need_num(v)),
                }
            }
            let blob = if lname == "vector_i8" {
                crate::vector::pack_i8(&vals)
            } else {
                crate::vector::pack_f32(&vals)
            };
            Ok(Value::Blob(blob))
        }
        "cosine_distance" | "l2_distance" | "dot" => match args {
            [Value::Null, _] | [_, Value::Null] => Ok(Value::Null),
            [Value::Blob(a), Value::Blob(b)] => {
                let d = match lname.as_str() {
                    "cosine_distance" => crate::vector::cosine_distance(a, b),
                    "l2_distance" => crate::vector::l2_distance(a, b),
                    _ => crate::vector::dot(a, b),
                }?;
                Ok(Value::Real(d))
            }
            [_, _] => Err(sql_err(format!("{lname}() requiere dos BLOB de vector"))),
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

/// Un valor SQL como nodo JSON (para `json_object`/`json_array`/`json_quote`).
/// Los BLOB no tienen representación JSON natural ⇒ error.
fn value_to_json(v: &Value) -> Result<json::Json> {
    Ok(match v {
        Value::Null => json::Json::Null,
        Value::Bool(b) => json::Json::Bool(*b),
        Value::Integer(n) => json::Json::Int(*n),
        Value::Real(f) => json::Json::Float(*f),
        Value::Text(s) => json::Json::Str(s.clone()),
        Value::Blob(_) => return Err(sql_err("JSON no admite BLOB")),
    })
}

/// Un nodo JSON como valor SQL (para `json_extract`): escalares se desenvuelven;
/// arrays/objetos se devuelven como su texto JSON.
fn json_to_value(j: &json::Json) -> Value {
    match j {
        json::Json::Null => Value::Null,
        json::Json::Bool(b) => Value::Integer(i64::from(*b)),
        json::Json::Int(n) => Value::Integer(*n),
        json::Json::Float(f) => Value::Real(*f),
        json::Json::Str(s) => Value::Text(s.clone()),
        json::Json::Array(_) | json::Json::Object(_) => Value::Text(json::to_string(j)),
    }
}

/// La ruta de `json_extract` debe ser TEXT (`$.a.b[0]`).
fn json_path(v: &Value) -> Result<String> {
    match v {
        Value::Text(s) => Ok(s.clone()),
        _ => Err(sql_err(
            "la ruta de json_extract debe ser TEXT (p. ej. '$.a')",
        )),
    }
}

/// `->` / `->>`: extrae de `doc` (TEXT JSON) en la clave/ruta `key`. La clave es
/// una ruta (`$.…`), una clave de objeto simple (`'a'` → `$.a`) o un índice entero
/// (`0` → `$[0]`). `->` devuelve el nodo como texto JSON; `->>` como valor SQL.
/// Cualquier operando NULL, o ruta inexistente ⇒ NULL.
fn json_arrow(op: BinOp, doc: Value, key: Value) -> Result<Value> {
    if matches!(doc, Value::Null) || matches!(key, Value::Null) {
        return Ok(Value::Null);
    }
    let text = match doc {
        Value::Text(s) => s,
        v => {
            return Err(sql_err(format!(
                "->/->> requieren TEXT JSON a la izquierda, no {}",
                v.type_name()
            )));
        }
    };
    let path = match key {
        Value::Integer(n) => format!("$[{n}]"),
        Value::Text(s) if s.starts_with('$') => s,
        Value::Text(s) => format!("$.{s}"),
        v => {
            return Err(sql_err(format!(
                "la clave de ->/->> debe ser texto o entero, no {}",
                v.type_name()
            )));
        }
    };
    let root = json::parse(&text).ok_or_else(|| sql_err("->/->>: JSON inválido"))?;
    Ok(match json::extract(&root, &path) {
        None => Value::Null,
        Some(node) if matches!(op, BinOp::JsonGet) => Value::Text(json::to_string(node)),
        Some(node) => json_to_value(node),
    })
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
            fts_indexes: Vec::new(),
            vector_indexes: Vec::new(),
            logical_order: vec![0],
            foreign_keys: Vec::new(),
            checks: Vec::new(),
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
            fts_indexes: Vec::new(),
            vector_indexes: Vec::new(),
            logical_order: vec![0],
            foreign_keys: Vec::new(),
            checks: Vec::new(),
        };
        let mut schema = QuerySchema::single("a", table("a"));
        schema.push("b", table("b")).unwrap();
        assert!(schema.resolve(None, "id").is_err(), "ambigua");
        assert_eq!(schema.resolve(Some("b"), "id").unwrap(), 1);
        assert!(schema.resolve(Some("zz"), "id").is_err());
        assert!(schema.push("a", table("a")).is_err(), "alias duplicado");
    }
}
