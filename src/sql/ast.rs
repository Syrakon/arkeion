//! AST del subconjunto SQL v1 (docs/04-sql.md).

use crate::catalog::{ColType, ColumnPos, FkAction, TriggerEvent, TriggerTiming};
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
    /// `CREATE TRIGGER … {BEFORE|AFTER} {INSERT|UPDATE|DELETE} ON t [FOR EACH ROW]
    /// BEGIN … END`. El cuerpo (DML) se guarda como texto y se re-parsea al disparar.
    CreateTrigger {
        if_not_exists: bool,
        name: String,
        timing: TriggerTiming,
        event: TriggerEvent,
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
    Insert {
        table: String,
        columns: Option<Vec<String>>,
        rows: Vec<Vec<Expr>>,
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
    },
    Delete {
        table: String,
        where_clause: Option<Expr>,
    },
    Begin,
    Commit,
    Rollback,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColumnAst {
    pub name: String,
    pub col_type: ColType,
    pub not_null: bool,
    pub primary_key: bool,
    /// Debe evaluar a constante sin parámetros (se valida en exec).
    pub default: Option<Expr>,
    /// `REFERENCES padre [ON DELETE acción]` (la columna referenciada es la PK
    /// del padre).
    pub references: Option<(String, FkAction)>,
}

/// Tabla con alias opcional: `facturas f` o `facturas AS f`.
#[derive(Clone, Debug, PartialEq)]
pub struct TableRef {
    pub name: String,
    pub alias: Option<String>,
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
    pub table: Option<String>,
    pub column: String,
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
            // Una subconsulta depende de los datos: no es constante.
            Expr::ScalarSubquery(_) | Expr::Exists(_) | Expr::InSubquery { .. } => false,
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
        }
    }
}
