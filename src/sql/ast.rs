//! AST del subconjunto SQL v1 (docs/04-sql.md).

use crate::catalog::ColType;
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
    pub projection: Vec<SelectItem>,
    pub from: TableRef,
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
    Expr(Expr),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggFunc {
    Count,
    Sum,
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
    /// `arg = None` solo para `COUNT(*)`.
    Aggregate {
        func: AggFunc,
        arg: Option<Box<Expr>>,
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
        }
    }
}
