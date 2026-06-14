//! Parser descendente recursivo (docs/04-sql.md). El propio código es la
//! gramática; los errores llevan la posición en bytes del token conflictivo.
//!
//! ```text
//! expr := or          or  := and (OR and)*        and := not (AND not)*
//! not  := [NOT] cmp
//! cmp  := add ((=|!=|<|<=|>|>=) add | IS [NOT] NULL | [NOT] LIKE add | [NOT] IN (lista))?
//! add  := mul ((+|-) mul)*                        mul := unary ((*|/|%) unary)*
//! unary := [-] primary
//! primary := literal | columna | tabla.columna | func(args) | agregado(expr|*) | ?N | ( expr )
//! ```

use crate::catalog::{ColType, ColumnPos};
use crate::error::{Error, Result};
use crate::record::Value;
use crate::sql::ast::*;
use crate::sql::datetime;
use crate::sql::lexer::{Kw, Spanned, Tok, lex};

const MIX_PARAMS: &str = "no se pueden mezclar parámetros posicionales (?N) y nombrados (:nombre)";

pub fn parse(sql: &str) -> Result<Stmt> {
    Ok(parse_full(sql)?.0)
}

/// Como [`parse`] pero devuelve además los nombres de los parámetros `:nombre`
/// (índice → nombre); vacío con parámetros posicionales. Lo usa el binding por
/// nombre de la API para construir el vector posicional.
pub fn parse_full(sql: &str) -> Result<(Stmt, Vec<String>)> {
    let toks = lex(sql)?;
    let mut p = Parser {
        toks,
        i: 0,
        end: sql.len(),
        param_names: Vec::new(),
        positional_seen: false,
        depth: 0,
    };
    let stmt = p.statement()?;
    p.eat(&Tok::Semi);
    if let Some(s) = p.peek_spanned() {
        return Err(err_at(s.pos, "se esperaba el final de la sentencia"));
    }
    Ok((stmt, p.param_names))
}

fn err_at(pos: usize, msg: impl Into<String>) -> Error {
    Error::Sql {
        msg: msg.into(),
        pos: Some(pos),
    }
}

struct Parser {
    toks: Vec<Spanned>,
    i: usize,
    end: usize,
    /// Nombres de los parámetros `:nombre` por orden de aparición (índice → nombre,
    /// reusado para repetidos). Vacío con parámetros posicionales `?N`.
    param_names: Vec<String>,
    /// `true` si se ha visto algún `?N`: no se mezcla con `:nombre`.
    positional_seen: bool,
    /// Profundidad de anidamiento de expresiones en curso. El parser es de
    /// descenso recursivo (un marco de pila por nivel de paréntesis/operador), así
    /// que sin tope una expresión muy anidada desbordaría la pila y **abortaría el
    /// proceso** — inaceptable en un motor embebido que prohíbe `unsafe`. Se acota.
    depth: usize,
}

/// Tope de anidamiento de expresiones (paréntesis/operadores). Holgado para SQL
/// humano, muy por debajo del desbordamiento de pila.
const MAX_EXPR_DEPTH: usize = 256;

impl Parser {
    fn peek_spanned(&self) -> Option<&Spanned> {
        self.toks.get(self.i)
    }

    fn peek(&self) -> Option<&Tok> {
        self.peek_spanned().map(|s| &s.tok)
    }

    fn peek2(&self) -> Option<&Tok> {
        self.toks.get(self.i + 1).map(|s| &s.tok)
    }

    fn pos(&self) -> usize {
        self.peek_spanned().map_or(self.end, |s| s.pos)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.i).map(|s| s.tok.clone());
        if t.is_some() {
            self.i += 1;
        }
        t
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == Some(t) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn eat_kw(&mut self, k: Kw) -> bool {
        self.eat(&Tok::Kw(k))
    }

    fn expect(&mut self, t: &Tok, what: &str) -> Result<()> {
        if self.eat(t) {
            Ok(())
        } else {
            Err(err_at(self.pos(), format!("se esperaba {what}")))
        }
    }

    fn expect_kw(&mut self, k: Kw, what: &str) -> Result<()> {
        self.expect(&Tok::Kw(k), what)
    }

    fn ident(&mut self, what: &str) -> Result<String> {
        match self.peek() {
            Some(Tok::Ident(_)) => match self.next() {
                Some(Tok::Ident(s)) => Ok(s),
                _ => unreachable!("peek garantiza Ident"),
            },
            _ => Err(err_at(self.pos(), format!("se esperaba {what}"))),
        }
    }

    // --- sentencias ---

    fn statement(&mut self) -> Result<Stmt> {
        match self.peek() {
            Some(Tok::Kw(Kw::Create)) => self.create(),
            Some(Tok::Kw(Kw::Alter)) => self.alter_table(),
            Some(Tok::Kw(Kw::Drop)) => self.drop(),
            Some(Tok::Kw(Kw::Insert)) => self.insert(),
            Some(Tok::Kw(Kw::Select)) => Ok(Stmt::Select(self.select()?)),
            Some(Tok::Kw(Kw::Update)) => self.update(),
            Some(Tok::Kw(Kw::Delete)) => self.delete(),
            Some(Tok::Kw(Kw::Begin)) => {
                self.i += 1;
                Ok(Stmt::Begin)
            }
            Some(Tok::Kw(Kw::Commit)) => {
                self.i += 1;
                Ok(Stmt::Commit)
            }
            Some(Tok::Kw(Kw::Rollback)) => {
                self.i += 1;
                Ok(Stmt::Rollback)
            }
            _ => Err(err_at(
                self.pos(),
                "se esperaba CREATE, DROP, INSERT, SELECT, UPDATE, DELETE, BEGIN, COMMIT o ROLLBACK",
            )),
        }
    }

    /// `CREATE` → tabla o índice según lo que siga (`[UNIQUE] INDEX` vs `TABLE`).
    fn create(&mut self) -> Result<Stmt> {
        self.expect_kw(Kw::Create, "CREATE")?;
        if matches!(self.peek(), Some(Tok::Kw(Kw::Unique | Kw::Index))) {
            self.create_index()
        } else {
            self.create_table()
        }
    }

    /// `DROP` → tabla o índice.
    fn drop(&mut self) -> Result<Stmt> {
        self.expect_kw(Kw::Drop, "DROP")?;
        if matches!(self.peek(), Some(Tok::Kw(Kw::Index))) {
            self.drop_index()
        } else {
            self.drop_table()
        }
    }

    fn if_not_exists(&mut self) -> Result<bool> {
        if self.eat_kw(Kw::If) {
            self.expect_kw(Kw::Not, "NOT")?;
            self.expect_kw(Kw::Exists, "EXISTS")?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn if_exists(&mut self) -> Result<bool> {
        if self.eat_kw(Kw::If) {
            self.expect_kw(Kw::Exists, "EXISTS")?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Tras consumir `CREATE`: `TABLE [IF NOT EXISTS] nombre (col, …)`.
    fn create_table(&mut self) -> Result<Stmt> {
        self.expect_kw(Kw::Table, "TABLE")?;
        let if_not_exists = self.if_not_exists()?;
        let name = self.ident("un nombre de tabla")?;
        self.expect(&Tok::LParen, "'('")?;
        let mut columns = Vec::new();
        loop {
            columns.push(self.column_def()?);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RParen, "')'")?;
        Ok(Stmt::CreateTable {
            if_not_exists,
            name,
            columns,
        })
    }

    /// Tras consumir `CREATE`: `[UNIQUE] INDEX [IF NOT EXISTS] nombre ON tabla (col, …)`.
    fn create_index(&mut self) -> Result<Stmt> {
        let unique = self.eat_kw(Kw::Unique);
        self.expect_kw(Kw::Index, "INDEX")?;
        let if_not_exists = self.if_not_exists()?;
        let name = self.ident("un nombre de índice")?;
        self.expect_kw(Kw::On, "ON")?;
        let table = self.ident("un nombre de tabla")?;
        self.expect(&Tok::LParen, "'('")?;
        let mut columns = Vec::new();
        loop {
            columns.push(self.ident("un nombre de columna")?);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RParen, "')'")?;
        Ok(Stmt::CreateIndex {
            if_not_exists,
            unique,
            name,
            table,
            columns,
        })
    }

    /// Tras consumir `DROP`: `INDEX [IF EXISTS] nombre`.
    fn drop_index(&mut self) -> Result<Stmt> {
        self.expect_kw(Kw::Index, "INDEX")?;
        let if_exists = self.if_exists()?;
        let name = self.ident("un nombre de índice")?;
        Ok(Stmt::DropIndex { if_exists, name })
    }

    fn alter_table(&mut self) -> Result<Stmt> {
        self.expect_kw(Kw::Alter, "ALTER")?;
        self.expect_kw(Kw::Table, "TABLE")?;
        let table = self.ident("un nombre de tabla")?;
        if self.eat_kw(Kw::Add) {
            let _ = self.eat_kw(Kw::Column); // COLUMN es opcional
            let column = self.column_def()?;
            return Ok(Stmt::AlterTableAddColumn { table, column });
        }
        if self.eat_kw(Kw::Move) {
            // MOVE COLUMN c {FIRST | BEFORE x | AFTER x} — reorden lógico.
            self.expect_kw(Kw::Column, "COLUMN")?;
            let column = self.ident("un nombre de columna")?;
            let pos = if self.eat_kw(Kw::First) {
                ColumnPos::First
            } else if self.eat_kw(Kw::Before) {
                ColumnPos::Before(self.ident("un nombre de columna")?)
            } else if self.eat_kw(Kw::After) {
                ColumnPos::After(self.ident("un nombre de columna")?)
            } else {
                return Err(err_at(self.pos(), "se esperaba FIRST, BEFORE o AFTER"));
            };
            return Ok(Stmt::AlterTableMoveColumn { table, column, pos });
        }
        if self.eat_kw(Kw::Reorder) {
            // REORDER COLUMNS (a, b, …) — fija el orden lógico completo.
            self.expect_kw(Kw::Columns, "COLUMNS")?;
            self.expect(&Tok::LParen, "'('")?;
            let mut order = Vec::new();
            loop {
                order.push(self.ident("un nombre de columna")?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(&Tok::RParen, "')'")?;
            return Ok(Stmt::AlterTableReorderColumns { table, order });
        }
        Err(err_at(
            self.pos(),
            "se esperaba ADD, MOVE o REORDER tras ALTER TABLE",
        ))
    }

    /// Un tipo SQL (`INTEGER | REAL | TEXT | BLOB | BOOLEAN`). Compartido por
    /// `column_def` y `CAST(… AS tipo)`.
    fn coltype(&mut self) -> Result<ColType> {
        match self.next() {
            Some(Tok::Kw(Kw::Integer)) => Ok(ColType::Integer),
            Some(Tok::Kw(Kw::Real)) => Ok(ColType::Real),
            Some(Tok::Kw(Kw::Text)) => Ok(ColType::Text),
            Some(Tok::Kw(Kw::Blob)) => Ok(ColType::Blob),
            Some(Tok::Kw(Kw::Boolean)) => Ok(ColType::Boolean),
            _ => Err(err_at(
                self.pos(),
                "se esperaba un tipo: INTEGER, REAL, TEXT, BLOB o BOOLEAN",
            )),
        }
    }

    fn column_def(&mut self) -> Result<ColumnAst> {
        let name = self.ident("un nombre de columna")?;
        let col_type = self.coltype()?;
        let mut col = ColumnAst {
            name,
            col_type,
            not_null: false,
            primary_key: false,
            default: None,
        };
        loop {
            if self.eat_kw(Kw::Primary) {
                self.expect_kw(Kw::Key, "KEY")?;
                col.primary_key = true;
            } else if self.eat_kw(Kw::Not) {
                self.expect_kw(Kw::Null, "NULL")?;
                col.not_null = true;
            } else if self.eat_kw(Kw::Default) {
                col.default = Some(self.expr()?);
            } else {
                return Ok(col);
            }
        }
    }

    /// Tras consumir `DROP`: `TABLE [IF EXISTS] nombre`.
    fn drop_table(&mut self) -> Result<Stmt> {
        self.expect_kw(Kw::Table, "TABLE")?;
        let if_exists = self.if_exists()?;
        Ok(Stmt::DropTable {
            if_exists,
            name: self.ident("un nombre de tabla")?,
        })
    }

    fn insert(&mut self) -> Result<Stmt> {
        self.expect_kw(Kw::Insert, "INSERT")?;
        self.expect_kw(Kw::Into, "INTO")?;
        let table = self.ident("un nombre de tabla")?;
        let columns = if self.eat(&Tok::LParen) {
            let mut cols = vec![self.ident("un nombre de columna")?];
            while self.eat(&Tok::Comma) {
                cols.push(self.ident("un nombre de columna")?);
            }
            self.expect(&Tok::RParen, "')'")?;
            Some(cols)
        } else {
            None
        };
        self.expect_kw(Kw::Values, "VALUES")?;
        let mut rows = Vec::new();
        loop {
            self.expect(&Tok::LParen, "'('")?;
            let mut row = vec![self.expr()?];
            while self.eat(&Tok::Comma) {
                row.push(self.expr()?);
            }
            self.expect(&Tok::RParen, "')'")?;
            rows.push(row);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        Ok(Stmt::Insert {
            table,
            columns,
            rows,
        })
    }

    fn update(&mut self) -> Result<Stmt> {
        self.expect_kw(Kw::Update, "UPDATE")?;
        let table = self.ident("un nombre de tabla")?;
        self.expect_kw(Kw::Set, "SET")?;
        let mut sets = Vec::new();
        loop {
            let col = self.ident("un nombre de columna")?;
            self.expect(&Tok::Eq, "'='")?;
            sets.push((col, self.expr()?));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        let where_clause = if self.eat_kw(Kw::Where) {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Stmt::Update {
            table,
            sets,
            where_clause,
        })
    }

    fn delete(&mut self) -> Result<Stmt> {
        self.expect_kw(Kw::Delete, "DELETE")?;
        self.expect_kw(Kw::From, "FROM")?;
        let table = self.ident("un nombre de tabla")?;
        let where_clause = if self.eat_kw(Kw::Where) {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Stmt::Delete {
            table,
            where_clause,
        })
    }

    fn select(&mut self) -> Result<SelectStmt> {
        self.expect_kw(Kw::Select, "SELECT")?;
        let distinct = self.eat_kw(Kw::Distinct);
        let mut projection = vec![self.select_item()?];
        while self.eat(&Tok::Comma) {
            projection.push(self.select_item()?);
        }
        // `FROM` es opcional: `SELECT <expr>, …` evalúa expresiones constantes sin
        // tabla. Sin `FROM` no hay base para joins, así que el bucle se omite.
        let from = if self.eat_kw(Kw::From) {
            Some(self.table_ref()?)
        } else {
            None
        };
        let mut joins = Vec::new();
        if from.is_some() {
            loop {
                let kind = if self.eat_kw(Kw::Inner) {
                    self.expect_kw(Kw::Join, "JOIN")?;
                    JoinKind::Inner
                } else if self.eat_kw(Kw::Left) {
                    self.expect_kw(Kw::Join, "JOIN")?;
                    JoinKind::Left
                } else if self.eat_kw(Kw::Join) {
                    JoinKind::Inner
                } else {
                    break;
                };
                let table = self.table_ref()?;
                self.expect_kw(Kw::On, "ON")?;
                let on = self.expr()?;
                joins.push(Join { kind, table, on });
            }
        }
        let where_clause = if self.eat_kw(Kw::Where) {
            Some(self.expr()?)
        } else {
            None
        };
        let mut group_by = Vec::new();
        if self.eat_kw(Kw::Group) {
            self.expect_kw(Kw::By, "BY")?;
            loop {
                group_by.push(self.expr()?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        let having = if self.eat_kw(Kw::Having) {
            Some(self.expr()?)
        } else {
            None
        };
        let mut order_by = Vec::new();
        if self.eat_kw(Kw::Order) {
            self.expect_kw(Kw::By, "BY")?;
            loop {
                let first = self.ident("un nombre de columna")?;
                let (table, column) = if self.eat(&Tok::Dot) {
                    (Some(first), self.ident("un nombre de columna")?)
                } else {
                    (None, first)
                };
                let desc = self.eat_kw(Kw::Desc);
                if !desc {
                    let _ = self.eat_kw(Kw::Asc); // ASC es el valor por defecto
                }
                order_by.push(OrderBy {
                    table,
                    column,
                    desc,
                });
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        let limit = if self.eat_kw(Kw::Limit) {
            Some(self.expr()?)
        } else {
            None
        };
        let offset = if self.eat_kw(Kw::Offset) {
            Some(self.expr()?)
        } else {
            None
        };
        let as_of = self.as_of()?;
        Ok(SelectStmt {
            distinct,
            projection,
            from,
            joins,
            where_clause,
            group_by,
            having,
            order_by,
            limit,
            offset,
            as_of,
        })
    }

    /// `AS OF VERSION n | AS OF TIMESTAMP 'rfc3339'`, al cierre de un SELECT
    /// (docs/04-sql). El timestamp se resuelve aquí a epoch ms para que un
    /// literal mal formado falle con posición, como el resto del parser.
    fn as_of(&mut self) -> Result<Option<AsOfClause>> {
        if self.peek() != Some(&Tok::Kw(Kw::As)) || self.peek2() != Some(&Tok::Kw(Kw::Of)) {
            return Ok(None);
        }
        self.next(); // AS
        self.next(); // OF
        if self.eat_kw(Kw::Version) {
            let pos = self.pos();
            match self.next() {
                Some(Tok::Int(n)) if n >= 0 => Ok(Some(AsOfClause::Version(n as u64))),
                _ => Err(err_at(
                    pos,
                    "se esperaba un número de versión entero no negativo",
                )),
            }
        } else if self.eat_kw(Kw::Timestamp) {
            let pos = self.pos();
            match self.next() {
                Some(Tok::Str(s)) => {
                    let ms = datetime::parse_rfc3339_ms(&s)
                        .ok_or_else(|| err_at(pos, "timestamp RFC 3339 inválido"))?;
                    Ok(Some(AsOfClause::Timestamp(ms)))
                }
                _ => Err(err_at(
                    pos,
                    "se esperaba un literal de timestamp RFC 3339 entre comillas",
                )),
            }
        } else {
            Err(err_at(
                self.pos(),
                "se esperaba VERSION o TIMESTAMP tras AS OF",
            ))
        }
    }

    fn table_ref(&mut self) -> Result<TableRef> {
        let name = self.ident("un nombre de tabla")?;
        // `AS` exige el alias; un identificador suelto también lo es. Pero
        // `AS OF` es la cláusula temporal de sentencia, no un alias: no la
        // consumas aquí aunque empiece por `AS`.
        let as_of_ahead =
            self.peek() == Some(&Tok::Kw(Kw::As)) && self.peek2() == Some(&Tok::Kw(Kw::Of));
        let alias = if !as_of_ahead
            && (self.eat_kw(Kw::As) || matches!(self.peek(), Some(Tok::Ident(_))))
        {
            Some(self.ident("un alias de tabla")?)
        } else {
            None
        };
        Ok(TableRef { name, alias })
    }

    fn select_item(&mut self) -> Result<SelectItem> {
        if self.eat(&Tok::Star) {
            return Ok(SelectItem::Star);
        }
        let expr = self.expr()?;
        // Alias de columna: `expr AS nombre` (explícito, sin ambigüedad). Pero
        // `AS OF` es la cláusula temporal de sentencia, no un alias: no la consumas
        // (igual que en `table_ref`). Así `SELECT 1 AS OF VERSION n` parsea con
        // `from: None` y exec da el diagnóstico claro «AS OF requiere FROM».
        let as_of_ahead =
            self.peek() == Some(&Tok::Kw(Kw::As)) && self.peek2() == Some(&Tok::Kw(Kw::Of));
        let alias = if !as_of_ahead && self.eat_kw(Kw::As) {
            Some(self.ident("un alias de columna tras AS")?)
        } else {
            None
        };
        Ok(SelectItem::Expr { expr, alias })
    }

    // --- expresiones ---

    fn expr(&mut self) -> Result<Expr> {
        // Cada nivel de anidamiento (paréntesis) reentra por aquí: acota la pila.
        self.depth += 1;
        if self.depth > MAX_EXPR_DEPTH {
            self.depth -= 1;
            return Err(err_at(self.pos(), "expresión demasiado anidada"));
        }
        let r = self.or_expr();
        self.depth -= 1;
        r
    }

    fn or_expr(&mut self) -> Result<Expr> {
        let mut left = self.and_expr()?;
        while self.eat_kw(Kw::Or) {
            let right = self.and_expr()?;
            left = Expr::Binary(Box::new(left), BinOp::Or, Box::new(right));
        }
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<Expr> {
        let mut left = self.not_expr()?;
        while self.eat_kw(Kw::And) {
            let right = self.not_expr()?;
            left = Expr::Binary(Box::new(left), BinOp::And, Box::new(right));
        }
        Ok(left)
    }

    fn not_expr(&mut self) -> Result<Expr> {
        if self.eat_kw(Kw::Not) {
            return Ok(Expr::Unary(UnOp::Not, Box::new(self.not_expr()?)));
        }
        self.cmp_expr()
    }

    fn cmp_expr(&mut self) -> Result<Expr> {
        let left = self.add_expr()?;
        // IS [NOT] NULL
        if self.eat_kw(Kw::Is) {
            let negated = self.eat_kw(Kw::Not);
            self.expect_kw(Kw::Null, "NULL")?;
            return Ok(Expr::IsNull {
                expr: Box::new(left),
                negated,
            });
        }
        // [NOT] LIKE
        if self.peek() == Some(&Tok::Kw(Kw::Not)) && self.peek2() == Some(&Tok::Kw(Kw::Like)) {
            self.i += 2;
            let pattern = self.add_expr()?;
            return Ok(Expr::Like {
                expr: Box::new(left),
                pattern: Box::new(pattern),
                negated: true,
            });
        }
        if self.eat_kw(Kw::Like) {
            let pattern = self.add_expr()?;
            return Ok(Expr::Like {
                expr: Box::new(left),
                pattern: Box::new(pattern),
                negated: false,
            });
        }
        // [NOT] IN (v1, v2, …)
        let neg_in =
            self.peek() == Some(&Tok::Kw(Kw::Not)) && self.peek2() == Some(&Tok::Kw(Kw::In));
        if neg_in || self.peek() == Some(&Tok::Kw(Kw::In)) {
            self.i += if neg_in { 2 } else { 1 };
            self.expect(&Tok::LParen, "'(' tras IN")?;
            let mut list = Vec::new();
            if self.peek() != Some(&Tok::RParen) {
                loop {
                    list.push(self.expr()?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
            }
            self.expect(&Tok::RParen, "')'")?;
            return Ok(Expr::In {
                expr: Box::new(left),
                list,
                negated: neg_in,
            });
        }
        // [NOT] BETWEEN a AND b — desazucarado a comparaciones (el operando se
        // duplica; sin efectos secundarios salvo funciones volátiles como random()).
        let neg_btw =
            self.peek() == Some(&Tok::Kw(Kw::Not)) && self.peek2() == Some(&Tok::Kw(Kw::Between));
        if neg_btw || self.peek() == Some(&Tok::Kw(Kw::Between)) {
            self.i += if neg_btw { 2 } else { 1 };
            let lo = self.add_expr()?;
            self.expect_kw(Kw::And, "AND")?; // el AND de BETWEEN, no el booleano
            let hi = self.add_expr()?;
            let cmp = |l: Expr, op: BinOp, r: Expr| Expr::Binary(Box::new(l), op, Box::new(r));
            return Ok(if neg_btw {
                // x < a OR x > b
                cmp(
                    cmp(left.clone(), BinOp::Lt, lo),
                    BinOp::Or,
                    cmp(left, BinOp::Gt, hi),
                )
            } else {
                // x >= a AND x <= b
                cmp(
                    cmp(left.clone(), BinOp::Ge, lo),
                    BinOp::And,
                    cmp(left, BinOp::Le, hi),
                )
            });
        }
        let op = match self.peek() {
            Some(Tok::Eq) => BinOp::Eq,
            Some(Tok::Ne) => BinOp::Ne,
            Some(Tok::Lt) => BinOp::Lt,
            Some(Tok::Le) => BinOp::Le,
            Some(Tok::Gt) => BinOp::Gt,
            Some(Tok::Ge) => BinOp::Ge,
            _ => return Ok(left),
        };
        self.i += 1;
        let right = self.add_expr()?;
        Ok(Expr::Binary(Box::new(left), op, Box::new(right)))
    }

    fn add_expr(&mut self) -> Result<Expr> {
        let mut left = self.mul_expr()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Plus) => BinOp::Add,
                Some(Tok::Minus) => BinOp::Sub,
                _ => return Ok(left),
            };
            self.i += 1;
            let right = self.mul_expr()?;
            left = Expr::Binary(Box::new(left), op, Box::new(right));
        }
    }

    fn mul_expr(&mut self) -> Result<Expr> {
        let mut left = self.concat_expr()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Star) => BinOp::Mul,
                Some(Tok::Slash) => BinOp::Div,
                Some(Tok::Percent) => BinOp::Mod,
                _ => return Ok(left),
            };
            self.i += 1;
            let right = self.concat_expr()?;
            left = Expr::Binary(Box::new(left), op, Box::new(right));
        }
    }

    /// `||` (concatenación): entre `mul` y `unary`, como en SQLite (`||` liga más
    /// fuerte que `* / %` y más débil que el unario).
    fn concat_expr(&mut self) -> Result<Expr> {
        let mut left = self.unary_expr()?;
        while self.eat(&Tok::Concat) {
            let right = self.unary_expr()?;
            left = Expr::Binary(Box::new(left), BinOp::Concat, Box::new(right));
        }
        Ok(left)
    }

    fn unary_expr(&mut self) -> Result<Expr> {
        if self.eat(&Tok::Minus) {
            return Ok(Expr::Unary(UnOp::Neg, Box::new(self.unary_expr()?)));
        }
        self.primary()
    }

    /// `CASE [operand] WHEN c THEN r (WHEN c THEN r)* [ELSE e] END` (ya se consumió
    /// `CASE`). Sin `operand` (lo siguiente es `WHEN`) es la forma buscada.
    fn case_expr(&mut self) -> Result<Expr> {
        let operand = if matches!(self.peek(), Some(Tok::Kw(Kw::When))) {
            None
        } else {
            Some(Box::new(self.expr()?))
        };
        let mut whens = Vec::new();
        while self.eat_kw(Kw::When) {
            let cond = self.expr()?;
            self.expect_kw(Kw::Then, "THEN")?;
            whens.push((cond, self.expr()?));
        }
        if whens.is_empty() {
            return Err(err_at(self.pos(), "CASE necesita al menos un WHEN … THEN"));
        }
        let else_ = if self.eat_kw(Kw::Else) {
            Some(Box::new(self.expr()?))
        } else {
            None
        };
        self.expect_kw(Kw::End, "END")?;
        Ok(Expr::Case {
            operand,
            whens,
            else_,
        })
    }

    fn primary(&mut self) -> Result<Expr> {
        let pos = self.pos();
        match self.next() {
            Some(Tok::Int(n)) => Ok(Expr::Literal(Value::Integer(n))),
            Some(Tok::Float(f)) => Ok(Expr::Literal(Value::Real(f))),
            Some(Tok::Str(s)) => Ok(Expr::Literal(Value::Text(s))),
            Some(Tok::Blob(b)) => Ok(Expr::Literal(Value::Blob(b))),
            Some(Tok::Kw(Kw::Null)) => Ok(Expr::Literal(Value::Null)),
            Some(Tok::Kw(Kw::True)) => Ok(Expr::Literal(Value::Bool(true))),
            Some(Tok::Kw(Kw::False)) => Ok(Expr::Literal(Value::Bool(false))),
            Some(Tok::Kw(Kw::Cast)) => {
                self.expect(&Tok::LParen, "'(' tras CAST")?;
                let inner = self.expr()?;
                self.expect_kw(Kw::As, "AS")?;
                let to = self.coltype()?;
                self.expect(&Tok::RParen, "')'")?;
                Ok(Expr::Cast {
                    expr: Box::new(inner),
                    to,
                })
            }
            Some(Tok::Kw(Kw::Case)) => self.case_expr(),
            Some(Tok::Param(n)) => {
                if !self.param_names.is_empty() {
                    return Err(err_at(pos, MIX_PARAMS));
                }
                self.positional_seen = true;
                Ok(Expr::Param(n))
            }
            Some(Tok::NamedParam(name)) => {
                if self.positional_seen {
                    return Err(err_at(pos, MIX_PARAMS));
                }
                let idx = match self.param_names.iter().position(|n| n == &name) {
                    Some(i) => i,
                    None => {
                        self.param_names.push(name);
                        self.param_names.len() - 1
                    }
                };
                Ok(Expr::Param(idx + 1))
            }
            Some(Tok::Ident(name)) => {
                // tabla.columna
                if self.eat(&Tok::Dot) {
                    let col = self.ident("un nombre de columna")?;
                    return Ok(Expr::Column {
                        table: Some(name),
                        name: col,
                    });
                }
                // `nombre(...)`: agregado o función escalar.
                if self.eat(&Tok::LParen) {
                    let agg = match name.to_ascii_uppercase().as_str() {
                        "COUNT" => Some(AggFunc::Count),
                        "SUM" => Some(AggFunc::Sum),
                        "AVG" => Some(AggFunc::Avg),
                        "MIN" => Some(AggFunc::Min),
                        "MAX" => Some(AggFunc::Max),
                        _ => None,
                    };
                    if let Some(func) = agg {
                        let distinct = self.eat_kw(Kw::Distinct); // COUNT(DISTINCT x)
                        let arg = if self.eat(&Tok::Star) {
                            if distinct {
                                return Err(err_at(pos, "DISTINCT no admite '*'"));
                            }
                            if func != AggFunc::Count {
                                return Err(err_at(pos, "solo COUNT admite '*'"));
                            }
                            None
                        } else {
                            Some(Box::new(self.expr()?))
                        };
                        self.expect(&Tok::RParen, "')'")?;
                        return Ok(Expr::Aggregate {
                            func,
                            arg,
                            distinct,
                        });
                    }
                    // Función escalar built-in: `nombre(arg, …)` (el nombre se
                    // resuelve en exec; `*` no se admite como argumento).
                    let mut args = Vec::new();
                    if !matches!(self.peek(), Some(Tok::RParen)) {
                        loop {
                            args.push(self.expr()?);
                            if !self.eat(&Tok::Comma) {
                                break;
                            }
                        }
                    }
                    self.expect(&Tok::RParen, "')'")?;
                    return Ok(Expr::Function { name, args });
                }
                Ok(Expr::Column { table: None, name })
            }
            Some(Tok::LParen) => {
                let e = self.expr()?;
                self.expect(&Tok::RParen, "')'")?;
                Ok(e)
            }
            _ => Err(err_at(pos, "se esperaba una expresión")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str) -> Expr {
        Expr::Column {
            table: None,
            name: name.into(),
        }
    }

    #[test]
    fn select_full_shape() {
        let stmt = parse(
            "SELECT id, total * 2 FROM facturas WHERE estado = 'x' AND total > ?1 \
             ORDER BY total DESC, id LIMIT 10 OFFSET 5;",
        )
        .unwrap();
        let Stmt::Select(s) = stmt else {
            panic!("se esperaba SELECT")
        };
        assert_eq!(s.projection.len(), 2);
        assert_eq!(
            s.from,
            Some(TableRef {
                name: "facturas".into(),
                alias: None
            })
        );
        assert!(s.joins.is_empty());
        assert!(s.where_clause.is_some());
        assert_eq!(
            s.order_by,
            vec![
                OrderBy {
                    table: None,
                    column: "total".into(),
                    desc: true
                },
                OrderBy {
                    table: None,
                    column: "id".into(),
                    desc: false
                },
            ]
        );
        assert_eq!(s.limit, Some(Expr::Literal(Value::Integer(10))));
        assert_eq!(s.offset, Some(Expr::Literal(Value::Integer(5))));
    }

    #[test]
    fn joins_aliases_and_qualified_columns() {
        let Stmt::Select(s) = parse(
            "SELECT c.nombre, f.total FROM facturas f \
             LEFT JOIN clientes AS c ON f.cliente_id = c.id \
             ORDER BY c.id",
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(
            s.from,
            Some(TableRef {
                name: "facturas".into(),
                alias: Some("f".into())
            })
        );
        assert_eq!(s.joins.len(), 1);
        let j = &s.joins[0];
        assert_eq!(j.kind, JoinKind::Left);
        assert_eq!(
            j.table,
            TableRef {
                name: "clientes".into(),
                alias: Some("c".into())
            }
        );
        assert!(matches!(
            &s.projection[0],
            SelectItem::Expr { expr: Expr::Column { table: Some(t), name }, .. } if t == "c" && name == "nombre"
        ));
        assert_eq!(s.order_by[0].table.as_deref(), Some("c"));
    }

    #[test]
    fn as_of_version_and_timestamp() {
        // Tras WHERE/ORDER/LIMIT, sin ambigüedad de alias.
        let Stmt::Select(s) =
            parse("SELECT total FROM facturas WHERE id = 7 AS OF VERSION 1042").unwrap()
        else {
            panic!()
        };
        assert_eq!(s.as_of, Some(AsOfClause::Version(1042)));

        // `FROM tabla AS OF …`: el `AS` es de la cláusula temporal, no un
        // alias. La tabla queda sin alias.
        let Stmt::Select(s) =
            parse("SELECT * FROM facturas AS OF TIMESTAMP '1970-01-01T00:00:00.250Z'").unwrap()
        else {
            panic!()
        };
        assert_eq!(s.from.as_ref().unwrap().alias, None);
        assert_eq!(s.as_of, Some(AsOfClause::Timestamp(250)));

        // Sin AS OF, sigue siendo None; y un alias real sigue funcionando.
        let Stmt::Select(s) = parse("SELECT * FROM facturas f").unwrap() else {
            panic!()
        };
        assert_eq!(s.as_of, None);
        assert_eq!(s.from.as_ref().unwrap().alias.as_deref(), Some("f"));
    }

    #[test]
    fn as_of_rejects_malformed() {
        // Falta VERSION/TIMESTAMP.
        assert!(parse("SELECT * FROM t AS OF 5").is_err());
        // Versión no entera.
        assert!(parse("SELECT * FROM t AS OF VERSION 'x'").is_err());
        // Timestamp inválido: error con posición (es un error SQL posicionado).
        let err = parse("SELECT * FROM t AS OF TIMESTAMP 'ayer'").unwrap_err();
        assert!(matches!(err, Error::Sql { pos: Some(_), .. }));
        // AS OF solo cierra la sentencia: nada después.
        assert!(parse("SELECT * FROM t AS OF VERSION 1 LIMIT 5").is_err());
    }

    #[test]
    fn select_without_from() {
        // `FROM` opcional: la proyección queda con `from: None` y sin joins.
        let Stmt::Select(s) = parse("SELECT 1 + 1, UPPER('hi') AS g").unwrap() else {
            panic!()
        };
        assert_eq!(s.from, None);
        assert!(s.joins.is_empty());
        assert_eq!(s.projection.len(), 2);
        assert!(matches!(
            &s.projection[1],
            SelectItem::Expr { alias: Some(a), .. } if a == "g"
        ));
        // Un `WHERE` constante sigue parseando sin `FROM`.
        let Stmt::Select(s) = parse("SELECT 1 WHERE 1 = 0").unwrap() else {
            panic!()
        };
        assert_eq!(s.from, None);
        assert!(s.where_clause.is_some());

        // `AS OF` sin `FROM` no se confunde con un alias `expr AS nombre`: parsea
        // con `from: None` y `as_of: Some` (exec lo rechaza con un mensaje claro).
        let Stmt::Select(s) = parse("SELECT 1 AS OF VERSION 5").unwrap() else {
            panic!()
        };
        assert_eq!(s.from, None);
        assert_eq!(s.as_of, Some(AsOfClause::Version(5)));
    }

    #[test]
    fn update_delete_and_tx_statements() {
        let stmt = parse("UPDATE t SET a = a + 1, b = 'x' WHERE id = 7").unwrap();
        let Stmt::Update {
            table,
            sets,
            where_clause,
        } = stmt
        else {
            panic!()
        };
        assert_eq!(table, "t");
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[1].0, "b");
        assert!(where_clause.is_some());

        let stmt = parse("DELETE FROM t WHERE a IS NULL").unwrap();
        assert!(matches!(stmt, Stmt::Delete { ref table, where_clause: Some(_) } if table == "t"));
        assert!(matches!(
            parse("DELETE FROM t").unwrap(),
            Stmt::Delete {
                where_clause: None,
                ..
            }
        ));

        assert_eq!(parse("BEGIN;").unwrap(), Stmt::Begin);
        assert_eq!(parse("COMMIT").unwrap(), Stmt::Commit);
        assert_eq!(parse("ROLLBACK").unwrap(), Stmt::Rollback);
    }

    #[test]
    fn aggregates() {
        let Stmt::Select(s) = parse("SELECT COUNT(*), SUM(total), AVG(total + 1) FROM t").unwrap()
        else {
            panic!()
        };
        assert!(matches!(
            &s.projection[0],
            SelectItem::Expr {
                expr: Expr::Aggregate {
                    func: AggFunc::Count,
                    arg: None,
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            &s.projection[1],
            SelectItem::Expr {
                expr: Expr::Aggregate {
                    func: AggFunc::Sum,
                    arg: Some(_),
                    ..
                },
                ..
            }
        ));
        // Una función no-agregado parsea como Function (su validez la decide exec).
        assert!(matches!(
            parse("SELECT NOPE(1) FROM t"),
            Ok(Stmt::Select(_))
        ));
        assert!(parse("SELECT SUM(*) FROM t").is_err());
    }

    #[test]
    fn precedence_or_and_cmp_arith() {
        // a = 1 OR b = 2 AND c = 3  ⇒  a=1 OR ((b=2) AND (c=3))
        let Stmt::Select(s) = parse("SELECT * FROM t WHERE a = 1 OR b = 2 AND c = 3").unwrap()
        else {
            panic!()
        };
        let Some(Expr::Binary(_, BinOp::Or, right)) = s.where_clause else {
            panic!("OR debe ser la raíz")
        };
        assert!(matches!(*right, Expr::Binary(_, BinOp::And, _)));

        // 1 + 2 * 3  ⇒  1 + (2*3)
        let Stmt::Select(s) = parse("SELECT 1 + 2 * 3 FROM t").unwrap() else {
            panic!()
        };
        let SelectItem::Expr {
            expr: Expr::Binary(_, BinOp::Add, right),
            ..
        } = &s.projection[0]
        else {
            panic!("+ debe ser la raíz")
        };
        assert!(matches!(**right, Expr::Binary(_, BinOp::Mul, _)));
    }

    #[test]
    fn create_table_with_constraints() {
        let stmt = parse(
            "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, \
             estado TEXT NOT NULL DEFAULT 'borrador', total REAL)",
        )
        .unwrap();
        let Stmt::CreateTable {
            if_not_exists,
            name,
            columns,
        } = stmt
        else {
            panic!()
        };
        assert!(if_not_exists);
        assert_eq!(name, "t");
        assert!(columns[0].primary_key);
        assert!(columns[1].not_null);
        assert_eq!(
            columns[1].default,
            Some(Expr::Literal(Value::Text("borrador".into())))
        );
        assert_eq!(columns[2].col_type, ColType::Real);
    }

    #[test]
    fn insert_multi_row_and_named_columns() {
        let stmt = parse("INSERT INTO t (a, b) VALUES (1, 'x'), (?1, NULL)").unwrap();
        let Stmt::Insert { columns, rows, .. } = stmt else {
            panic!()
        };
        assert_eq!(columns, Some(vec!["a".into(), "b".into()]));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1][0], Expr::Param(1));
    }

    #[test]
    fn is_null_and_not_like() {
        let Stmt::Select(s) =
            parse("SELECT * FROM t WHERE a IS NOT NULL AND b NOT LIKE 'x%'").unwrap()
        else {
            panic!()
        };
        let Some(Expr::Binary(left, BinOp::And, right)) = s.where_clause else {
            panic!()
        };
        assert!(matches!(*left, Expr::IsNull { negated: true, .. }));
        assert!(matches!(*right, Expr::Like { negated: true, .. }));
        // Sin ambigüedad con el alias de tabla: `t.a`.
        let _ = col("sin_uso");
    }

    #[test]
    fn syntax_errors_have_positions() {
        match parse("SELEC 1 FROM t") {
            Err(Error::Sql { pos: Some(0), .. }) => {}
            other => panic!("se esperaba error en byte 0, llegó {other:?}"),
        }
        match parse("SELECT a FROM t WHERE") {
            Err(Error::Sql { pos: Some(p), .. }) => assert_eq!(p, 21),
            other => panic!("se esperaba error con posición, llegó {other:?}"),
        }
        match parse("SELECT a FROM t; extra") {
            Err(Error::Sql { pos: Some(17), .. }) => {}
            other => panic!("se esperaba error en byte 17, llegó {other:?}"),
        }
        match parse("UPDATE t SET = 1") {
            Err(Error::Sql { pos: Some(13), .. }) => {}
            other => panic!("se esperaba error en byte 13, llegó {other:?}"),
        }
    }
}
