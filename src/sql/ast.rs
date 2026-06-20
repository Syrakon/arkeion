//! AST del subconjunto SQL v1 (docs/04-sql.md).

use crate::catalog::{
    ColType, ColumnFk, ColumnPos, ForeignKeySpec, TriggerEvent, TriggerForEach, TriggerTiming,
};
use crate::record::Value;

// `Select` es bastante más grande que el resto de variantes, pero `Stmt` es un
// AST transitorio (se parsea, se ejecuta y se descarta; nunca se almacena en
// masa), así que la diferencia de tamaño no penaliza nada y no merece un `Box`.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    CreateTable {
        if_not_exists: bool,
        name: String,
        columns: Vec<ColumnAst>,
        /// FKs a nivel de tabla: `FOREIGN KEY (c…) REFERENCES padre (p…) …`
        /// (compuestas). Las de columna van en `ColumnAst.references`.
        foreign_keys: Vec<ForeignKeySpec>,
        /// `UNIQUE (c…)` a nivel de tabla (composables); cada uno crea un índice
        /// `UNIQUE`. Los de columna van en `ColumnAst.unique`.
        uniques: Vec<Vec<String>>,
        /// `CHECK (expr)` a nivel de tabla (texto del predicado). Los de columna van
        /// en `ColumnAst.check`.
        checks: Vec<String>,
    },
    /// `CREATE TABLE [IF NOT EXISTS] t AS SELECT …`: crea la tabla con las columnas
    /// (nombres y tipos inferidos) de la consulta y la rellena con sus filas.
    CreateTableAs {
        if_not_exists: bool,
        name: String,
        query: Box<SelectStmt>,
    },
    DropTable {
        if_exists: bool,
        name: String,
    },
    /// `CREATE VIEW [IF NOT EXISTS] nombre AS <select>`. El SELECT se guarda como
    /// texto (`select_sql`) y se re-parsea al usar la vista.
    CreateView {
        if_not_exists: bool,
        name: String,
        select_sql: String,
    },
    /// `DROP VIEW [IF EXISTS] nombre`.
    DropView {
        if_exists: bool,
        name: String,
    },
    /// `CREATE TRIGGER … {BEFORE|AFTER|INSTEAD OF} {INSERT|UPDATE|DELETE} ON t
    /// [FOR EACH {ROW|STATEMENT}] BEGIN … END`. El cuerpo (DML) se guarda como
    /// texto y se re-parsea al disparar.
    CreateTrigger {
        if_not_exists: bool,
        name: String,
        timing: TriggerTiming,
        event: TriggerEvent,
        for_each: TriggerForEach,
        table: String,
        body_sql: String,
    },
    /// `DROP TRIGGER [IF EXISTS] nombre`.
    DropTrigger {
        if_exists: bool,
        name: String,
    },
    /// `CREATE [UNIQUE] INDEX [IF NOT EXISTS] nombre ON tabla (col, …)`.
    CreateIndex {
        if_not_exists: bool,
        unique: bool,
        name: String,
        table: String,
        columns: Vec<String>,
    },
    /// `DROP INDEX [IF EXISTS] nombre` (nombre global, estilo SQLite).
    DropIndex {
        if_exists: bool,
        name: String,
    },
    /// `CREATE FULLTEXT INDEX [IF NOT EXISTS] nombre ON tabla (col, …) [USING tok]`.
    CreateFtsIndex {
        if_not_exists: bool,
        name: String,
        table: String,
        columns: Vec<String>,
        /// Tokenizer (`USING <name>`); `None` ⇒ `unicode` por defecto.
        tokenizer: Option<String>,
    },
    /// `DROP FULLTEXT INDEX [IF EXISTS] nombre` (nombre global, como `DropIndex`).
    DropFtsIndex {
        if_exists: bool,
        name: String,
    },
    Insert {
        table: String,
        columns: Option<Vec<String>>,
        /// Origen de las filas: `VALUES (…), …` o `SELECT …`.
        source: InsertSource,
        /// `ON CONFLICT [(cols)] DO {NOTHING | UPDATE SET …}` (UPSERT). Si está, una
        /// fila que choque con la PK o un índice UNIQUE no falla: se omite o se
        /// actualiza la existente.
        on_conflict: Option<OnConflict>,
        /// `RETURNING <select-list>`: si está, la sentencia devuelve filas (las
        /// insertadas) en vez de solo el recuento.
        returning: Option<Vec<SelectItem>>,
    },
    Select(SelectStmt),
    /// `ALTER TABLE t ADD [COLUMN] coldef`. La columna se añade al final; las
    /// filas existentes la leen como su `DEFAULT` (o NULL) sin reescribirse.
    AlterTableAddColumn {
        table: String,
        column: ColumnAst,
    },
    /// `ALTER TABLE t MOVE COLUMN c {FIRST | BEFORE x | AFTER x}` — reorden
    /// **lógico** (de presentación): no reescribe filas, time-travel intacto.
    AlterTableMoveColumn {
        table: String,
        column: String,
        pos: ColumnPos,
    },
    /// `ALTER TABLE t REORDER COLUMNS (a, b, …)` — fija el orden lógico completo.
    AlterTableReorderColumns {
        table: String,
        order: Vec<String>,
    },
    /// `ALTER TABLE t RENAME [COLUMN] old TO new` — solo metadato (nombre).
    AlterTableRenameColumn {
        table: String,
        old: String,
        new: String,
    },
    /// `ALTER TABLE t DROP [COLUMN] col` — DROP lógico (tombstone), no reescribe filas.
    AlterTableDropColumn {
        table: String,
        column: String,
    },
    Update {
        table: String,
        sets: Vec<(String, Expr)>,
        where_clause: Option<Expr>,
        returning: Option<Vec<SelectItem>>,
    },
    Delete {
        table: String,
        where_clause: Option<Expr>,
        returning: Option<Vec<SelectItem>>,
    },
    Begin,
    Commit,
    Rollback,
    /// `SAVEPOINT nombre`: punto de retorno dentro de una transacción.
    Savepoint(String),
    /// `RELEASE [SAVEPOINT] nombre`: descarta el savepoint (los cambios quedan).
    ReleaseSavepoint(String),
    /// `ROLLBACK TO [SAVEPOINT] nombre`: revierte al savepoint (que sigue activo).
    RollbackTo(String),
}

/// Origen de las filas de un `INSERT`: literal (`VALUES`) o una consulta.
#[derive(Clone, Debug, PartialEq)]
pub enum InsertSource {
    /// `VALUES (e, …)[, (e, …)…]`.
    Values(Vec<Vec<Expr>>),
    /// `SELECT …` (o `WITH … SELECT …`): se materializa y sus filas se insertan.
    Select(Box<SelectStmt>),
}

/// Acción de `ON CONFLICT` (UPSERT). La columna(s) objetivo opcional
/// (`ON CONFLICT (col) …`) se acepta pero no restringe: el conflicto se detecta
/// sobre la PK o cualquier índice UNIQUE.
#[derive(Clone, Debug, PartialEq)]
pub enum OnConflict {
    /// `DO NOTHING`: omite la fila en conflicto (sin error).
    Nothing,
    /// `DO UPDATE SET … [WHERE …]`: actualiza la fila existente. Las expresiones
    /// pueden referirse a `excluded.col` (la fila propuesta) y a las columnas de la
    /// fila existente.
    Update {
        sets: Vec<(String, Expr)>,
        where_clause: Option<Expr>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColumnAst {
    pub name: String,
    pub col_type: ColType,
    pub not_null: bool,
    pub primary_key: bool,
    /// Debe evaluar a constante sin parámetros (se valida en exec).
    pub default: Option<Expr>,
    /// `REFERENCES padre [(col)] [ON DELETE acción] [ON UPDATE acción]` en línea
    /// con la columna. `parent_column` `None` = la PK del padre.
    pub references: Option<ColumnFk>,
    /// `UNIQUE` en línea: crea un índice `UNIQUE` sobre esta columna.
    pub unique: bool,
    /// `CHECK (expr)` en línea: texto del predicado (se valida por fila).
    pub check: Option<String>,
}

/// Tabla con alias opcional: `facturas f` o `facturas AS f`.
#[derive(Clone, Debug, PartialEq)]
pub struct TableRef {
    pub name: String,
    pub alias: Option<String>,
    /// Tabla **derivada**: `FROM (SELECT …) AS alias`. Si está, `name` va vacío y el
    /// `alias` es obligatorio (es el qualifier). Se materializa como una CTE anónima.
    pub subquery: Option<Box<SelectStmt>>,
}

impl TableRef {
    /// Nombre por el que se resuelven las columnas cualificadas.
    pub fn qualifier(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Join {
    pub kind: JoinKind,
    pub table: TableRef,
    pub on: Expr,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OrderBy {
    /// Clave de ordenación: una expresión, una columna, un **alias** de la
    /// proyección, o un **literal entero** = posición ordinal (`ORDER BY 1`).
    pub expr: Expr,
    pub desc: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SelectStmt {
    /// `SELECT DISTINCT`: deduplica las filas ya proyectadas.
    pub distinct: bool,
    pub projection: Vec<SelectItem>,
    /// `FROM` es opcional: `SELECT <expr>, …` sin `FROM` evalúa expresiones
    /// constantes contra una única fila implícita (estilo SQLite). Sin tabla, la
    /// proyección no puede referenciar columnas/`*`/agregados y las cláusulas que
    /// necesitan filas (JOIN/GROUP BY/HAVING/ORDER BY/AS OF) se rechazan en exec.
    pub from: Option<TableRef>,
    pub joins: Vec<Join>,
    pub where_clause: Option<Expr>,
    /// `GROUP BY e1, e2, …`: agrupa las filas por el valor de estas expresiones
    /// (normalmente columnas) y pliega los agregados de la proyección por grupo.
    pub group_by: Vec<Expr>,
    /// `HAVING`: filtro sobre los grupos ya agregados (solo con `GROUP BY`).
    pub having: Option<Expr>,
    pub order_by: Vec<OrderBy>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
    /// `AS OF` (time-travel, M5): si está, toda la consulta se evalúa contra
    /// un único snapshot histórico. Cierra la sentencia.
    pub as_of: Option<AsOfClause>,
    /// `UNION [ALL]`: núcleos encadenados a este. Las cláusulas finales
    /// (`order_by`/`limit`/`offset`/`as_of`) las lleva este líder y aplican al
    /// conjunto entero; los núcleos de `compound` no las tienen.
    pub compound: Vec<CompoundSelect>,
    /// `WITH …`: CTEs (tablas con nombre) visibles en este SELECT, materializadas
    /// en orden (cada una ve las anteriores). Solo lo lleva el SELECT de nivel
    /// superior; v1: no recursivo.
    pub with: Vec<Cte>,
}

/// Una CTE: `nombre AS (SELECT …)`.
#[derive(Clone, Debug, PartialEq)]
pub struct Cte {
    pub name: String,
    pub query: SelectStmt,
}

/// Operador de conjunto entre SELECTs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetOp {
    /// `UNION`: une y deduplica.
    Union,
    /// `UNION ALL`: une conservando duplicados.
    UnionAll,
    /// `INTERSECT`: filas en ambos lados, deduplicadas.
    Intersect,
    /// `EXCEPT`: filas del acumulado que no están en el derecho, deduplicadas.
    Except,
}

/// Un núcleo SELECT encadenado con un operador de conjunto.
#[derive(Clone, Debug, PartialEq)]
pub struct CompoundSelect {
    pub op: SetOp,
    pub select: SelectStmt,
}

/// Punto histórico de un `SELECT … AS OF` (docs/04-sql). La versión es la
/// autoridad; el timestamp ya viene resuelto a epoch ms desde el literal
/// RFC 3339 en el parser (docs/05, D12).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AsOfClause {
    Version(u64),
    Timestamp(u64),
}

#[derive(Clone, Debug, PartialEq)]
pub enum SelectItem {
    Star,
    /// Una expresión proyectada, con su alias opcional (`expr AS nombre`). El
    /// alias da nombre a la columna de salida; sin él se deriva del propio `expr`.
    Expr {
        expr: Expr,
        alias: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    /// `GROUP_CONCAT(x [, sep])`: concatena los valores TEXT del grupo con un
    /// separador (por defecto `,`).
    GroupConcat,
}

/// Función de ventana (`… OVER (…)`). Las de ranking/posición y las agregadas
/// reusadas como ventana.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowFunc {
    RowNumber,
    Rank,
    DenseRank,
    Ntile,
    Lag,
    Lead,
    FirstValue,
    LastValue,
    Sum,
    Count,
    Avg,
    Min,
    Max,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Literal(Value),
    Column {
        table: Option<String>,
        name: String,
    },
    /// `?N`, 1-based.
    Param(usize),
    Unary(UnOp, Box<Expr>),
    Binary(Box<Expr>, BinOp, Box<Expr>),
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        negated: bool,
    },
    /// `columna MATCH 'consulta'` (full-text). `column` referencia una columna
    /// indexada con `CREATE FULLTEXT INDEX`; `query` es el texto del
    /// mini-lenguaje de consulta FTS. Ver `docs/12-fts.md`.
    Match {
        column: Box<Expr>,
        query: Box<Expr>,
        negated: bool,
    },
    /// `arg = None` solo para `COUNT(*)`. `distinct` para `COUNT(DISTINCT x)` etc.
    /// `sep` solo lo usa `GROUP_CONCAT(x, sep)` (separador constante).
    Aggregate {
        func: AggFunc,
        arg: Option<Box<Expr>>,
        distinct: bool,
        sep: Option<Box<Expr>>,
    },
    /// Llamada a una función **escalar** built-in: `nombre(arg, …)` (no agregado).
    /// El nombre se resuelve en exec (insensible a mayúsculas); aridad y tipos se
    /// validan ahí. P. ej. `UPPER(s)`, `COALESCE(a, b)`, `ROUND(x, 2)`.
    Function {
        name: String,
        args: Vec<Expr>,
    },
    /// `expr [NOT] IN (v1, v2, …)`: pertenencia a un conjunto de valores. Semántica
    /// trivalente de SQL (si `expr` es NULL, o no está y la lista trae NULL → NULL).
    In {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    /// `CAST(expr AS tipo)`: conversión **explícita** de tipo (la válvula de escape
    /// del tipado estricto del motor). NULL se propaga.
    Cast {
        expr: Box<Expr>,
        to: ColType,
    },
    /// `CASE [operand] WHEN c THEN r … [ELSE e] END`. Sin `operand`: forma
    /// "buscada" (cada `c` es una condición booleana). Con `operand`: forma
    /// "simple" (compara `operand = c`). Sin rama que cuadre ⇒ `ELSE` o NULL.
    Case {
        operand: Option<Box<Expr>>,
        whens: Vec<(Expr, Expr)>,
        else_: Option<Box<Expr>>,
    },
    /// `(SELECT …)` escalar: una pre-pasada (exec) la ejecuta y la sustituye por su
    /// único valor (0 filas → NULL, >1 → error). v1: **no correlacionada**.
    ScalarSubquery(Box<SelectStmt>),
    /// `EXISTS (SELECT …)`: `true` si la subconsulta devuelve alguna fila. `NOT
    /// EXISTS` lo envuelve el `NOT` exterior. v1: **no correlacionada**.
    Exists(Box<SelectStmt>),
    /// `expr [NOT] IN (SELECT …)`: pertenencia al conjunto de la subconsulta
    /// (una columna). v1: **no correlacionada**.
    InSubquery {
        expr: Box<Expr>,
        query: Box<SelectStmt>,
        negated: bool,
    },
    /// Función de **ventana**: `func(args) OVER ([PARTITION BY …] [ORDER BY …]
    /// [ROWS …])`. Se evalúa sobre el conjunto de filas (tras `WHERE`), particionado
    /// y ordenado; no es un agregado de grupo. `args` vacío para `ROW_NUMBER()`/
    /// `COUNT(*)`. `frame` = marco `ROWS` explícito (si no, el marco por defecto).
    Window {
        func: WindowFunc,
        args: Vec<Expr>,
        partition_by: Vec<Expr>,
        order_by: Vec<OrderBy>,
        frame: Option<WindowFrame>,
    },
}

/// Marco de ventana `ROWS BETWEEN inicio AND fin` (físico, por número de filas).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowFrame {
    pub start: FrameBound,
    pub end: FrameBound,
}

/// Extremo de un marco de ventana `ROWS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameBound {
    UnboundedPreceding,
    Preceding(usize),
    CurrentRow,
    Following(usize),
    UnboundedFollowing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    /// `||` — concatenación de texto.
    Concat,
    /// `->` — extracción JSON, resultado **como JSON** (texto JSON del nodo).
    JsonGet,
    /// `->>` — extracción JSON, resultado **como valor SQL** (escalar desenvuelto).
    JsonGetText,
}

impl Expr {
    /// `true` si la expresión no referencia columnas ni agregados
    /// (evaluable sin fila).
    pub fn is_const(&self) -> bool {
        match self {
            Expr::Literal(_) | Expr::Param(_) => true,
            Expr::Column { .. } | Expr::Aggregate { .. } => false,
            Expr::Unary(_, e) => e.is_const(),
            Expr::Binary(a, _, b) => a.is_const() && b.is_const(),
            Expr::IsNull { expr, .. } => expr.is_const(),
            Expr::Like { expr, pattern, .. } => expr.is_const() && pattern.is_const(),
            Expr::Match { column, query, .. } => column.is_const() && query.is_const(),
            Expr::Function { args, .. } => args.iter().all(Expr::is_const),
            Expr::In { expr, list, .. } => expr.is_const() && list.iter().all(Expr::is_const),
            Expr::Cast { expr, .. } => expr.is_const(),
            Expr::Case {
                operand,
                whens,
                else_,
            } => {
                operand.as_ref().is_none_or(|o| o.is_const())
                    && whens.iter().all(|(c, r)| c.is_const() && r.is_const())
                    && else_.as_ref().is_none_or(|e| e.is_const())
            }
            // Una subconsulta o ventana depende de los datos: no es constante.
            Expr::ScalarSubquery(_)
            | Expr::Exists(_)
            | Expr::InSubquery { .. }
            | Expr::Window { .. } => false,
        }
    }

    pub fn contains_param(&self) -> bool {
        match self {
            Expr::Param(_) => true,
            Expr::Literal(_) | Expr::Column { .. } => false,
            Expr::Unary(_, e) => e.contains_param(),
            Expr::Binary(a, _, b) => a.contains_param() || b.contains_param(),
            Expr::IsNull { expr, .. } => expr.contains_param(),
            Expr::Like { expr, pattern, .. } => expr.contains_param() || pattern.contains_param(),
            Expr::Match { column, query, .. } => column.contains_param() || query.contains_param(),
            Expr::Aggregate { arg, .. } => arg.as_ref().is_some_and(|e| e.contains_param()),
            Expr::Function { args, .. } => args.iter().any(Expr::contains_param),
            Expr::In { expr, list, .. } => {
                expr.contains_param() || list.iter().any(Expr::contains_param)
            }
            Expr::Cast { expr, .. } => expr.contains_param(),
            Expr::Case {
                operand,
                whens,
                else_,
            } => {
                operand.as_ref().is_some_and(|o| o.contains_param())
                    || whens
                        .iter()
                        .any(|(c, r)| c.contains_param() || r.contains_param())
                    || else_.as_ref().is_some_and(|e| e.contains_param())
            }
            // Las subconsultas no aparecen en contextos con parámetros enlazados.
            Expr::ScalarSubquery(_) | Expr::Exists(_) | Expr::InSubquery { .. } => false,
            Expr::Window {
                args, partition_by, ..
            } => {
                args.iter().any(Expr::contains_param)
                    || partition_by.iter().any(Expr::contains_param)
            }
        }
    }

    pub fn has_aggregate(&self) -> bool {
        match self {
            Expr::Aggregate { .. } => true,
            Expr::Literal(_) | Expr::Column { .. } | Expr::Param(_) => false,
            Expr::Unary(_, e) => e.has_aggregate(),
            Expr::Binary(a, _, b) => a.has_aggregate() || b.has_aggregate(),
            Expr::IsNull { expr, .. } => expr.has_aggregate(),
            Expr::Like { expr, pattern, .. } => expr.has_aggregate() || pattern.has_aggregate(),
            Expr::Match { column, query, .. } => column.has_aggregate() || query.has_aggregate(),
            Expr::Function { args, .. } => args.iter().any(Expr::has_aggregate),
            Expr::In { expr, list, .. } => {
                expr.has_aggregate() || list.iter().any(Expr::has_aggregate)
            }
            Expr::Cast { expr, .. } => expr.has_aggregate(),
            Expr::Case {
                operand,
                whens,
                else_,
            } => {
                operand.as_ref().is_some_and(|o| o.has_aggregate())
                    || whens
                        .iter()
                        .any(|(c, r)| c.has_aggregate() || r.has_aggregate())
                    || else_.as_ref().is_some_and(|e| e.has_aggregate())
            }
            // Un agregado dentro de la subconsulta es de su propio ámbito.
            Expr::ScalarSubquery(_) | Expr::Exists(_) | Expr::InSubquery { .. } => false,
            // Una ventana NO es un agregado de grupo: la maneja su propio camino.
            Expr::Window { .. } => false,
        }
    }

    /// `true` si la expresión contiene alguna función de ventana (`… OVER (…)`).
    pub fn has_window(&self) -> bool {
        match self {
            Expr::Window { .. } => true,
            Expr::Literal(_) | Expr::Column { .. } | Expr::Param(_) => false,
            Expr::Unary(_, e) => e.has_window(),
            Expr::Binary(a, _, b) => a.has_window() || b.has_window(),
            Expr::IsNull { expr, .. } => expr.has_window(),
            Expr::Like { expr, pattern, .. } => expr.has_window() || pattern.has_window(),
            Expr::Match { column, query, .. } => column.has_window() || query.has_window(),
            Expr::Aggregate { arg, .. } => arg.as_ref().is_some_and(|e| e.has_window()),
            Expr::Function { args, .. } => args.iter().any(Expr::has_window),
            Expr::In { expr, list, .. } => expr.has_window() || list.iter().any(Expr::has_window),
            Expr::Cast { expr, .. } => expr.has_window(),
            Expr::Case {
                operand,
                whens,
                else_,
            } => {
                operand.as_ref().is_some_and(|o| o.has_window())
                    || whens.iter().any(|(c, r)| c.has_window() || r.has_window())
                    || else_.as_ref().is_some_and(|e| e.has_window())
            }
            Expr::ScalarSubquery(_) | Expr::Exists(_) | Expr::InSubquery { .. } => false,
        }
    }
}
