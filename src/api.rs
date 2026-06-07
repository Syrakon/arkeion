//! API pública de Arkeion (docs/03-api.md): ergonomía estilo rusqlite.
//!
//! `execute`/`query` fuera de transacción son autocommit (una transacción por
//! sentencia). `BEGIN` SQL abre una transacción ligada a la conexión hasta
//! `COMMIT`/`ROLLBACK`; [`Connection::begin`] devuelve una [`Transaction`]
//! tipada. Ambas se apoyan en el escritor único: una segunda escritura
//! concurrente es [`Error::Busy`], nunca un bloqueo.

use std::cell::RefCell;
use std::marker::PhantomData;
use std::path::Path;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::exec;
use crate::record::Value;
use crate::sql::{self, ast::Stmt};
use crate::tx::{Store, WriteTx};

/// Opciones de apertura. *Zero-config*: los valores por defecto funcionan.
#[derive(Clone, Debug)]
pub struct Options {
    pub create_if_missing: bool,
    // M7: key: Option<Key> (cifrado en reposo).
}

impl Default for Options {
    fn default() -> Options {
        Options {
            create_if_missing: true,
        }
    }
}

impl Options {
    pub fn create_if_missing(mut self, create: bool) -> Options {
        self.create_if_missing = create;
        self
    }
}

/// Una base de datos Arkeion: un único archivo. Handle clonable y compartible
/// entre hilos.
#[derive(Clone)]
pub struct Database {
    store: Arc<Store>,
}

impl Database {
    pub fn open(path: impl AsRef<Path>, opts: Options) -> Result<Database> {
        let path = path.as_ref();
        let store = if opts.create_if_missing {
            match Store::create(path) {
                Ok(s) => s,
                // Ya existe (o carrera con otro creador): abrir.
                Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    Store::open(path)?
                }
                Err(e) => return Err(e),
            }
        } else {
            Store::open(path)?
        };
        Ok(Database {
            store: Arc::new(store),
        })
    }

    /// Conexión sobre la rama principal.
    pub fn connect(&self) -> Result<Connection> {
        Ok(Connection {
            store: self.store.clone(),
            open_tx: RefCell::new(None),
        })
    }
}

fn sql_err(msg: impl Into<String>) -> Error {
    Error::Sql {
        msg: msg.into(),
        pos: None,
    }
}

/// Conexión: ejecuta SQL. Las lecturas usan snapshots inmutables y jamás
/// bloquean ni son bloqueadas por la escritura en curso.
pub struct Connection {
    store: Arc<Store>,
    /// Transacción abierta con `BEGIN` (SQL). `None` = autocommit. Soltarla
    /// sin `COMMIT` (p. ej. al soltar la conexión) es un rollback.
    open_tx: RefCell<Option<WriteTx>>,
}

impl Connection {
    /// Sentencias que no devuelven filas (DDL, INSERT, UPDATE, DELETE,
    /// BEGIN/COMMIT/ROLLBACK). Devuelve filas afectadas. Fuera de `BEGIN`
    /// es autocommit: o toda la sentencia, o nada.
    pub fn execute(&self, sql: &str, params: &[Value]) -> Result<usize> {
        self.execute_stmt(&sql::parse(sql)?, params)
    }

    fn execute_stmt(&self, stmt: &Stmt, params: &[Value]) -> Result<usize> {
        match stmt {
            Stmt::Select(_) => Err(sql_err("SELECT devuelve filas: usa query")),
            Stmt::Begin => {
                let mut open = self.open_tx.borrow_mut();
                if open.is_some() {
                    return Err(sql_err(
                        "ya hay una transacción abierta: COMMIT o ROLLBACK antes de BEGIN",
                    ));
                }
                *open = Some(self.store.begin()?);
                Ok(0)
            }
            Stmt::Commit => {
                let tx = self
                    .open_tx
                    .borrow_mut()
                    .take()
                    .ok_or_else(|| sql_err("COMMIT sin transacción abierta"))?;
                tx.commit()?;
                Ok(0)
            }
            Stmt::Rollback => {
                let tx = self
                    .open_tx
                    .borrow_mut()
                    .take()
                    .ok_or_else(|| sql_err("ROLLBACK sin transacción abierta"))?;
                drop(tx); // soltar la transacción ES el rollback
                Ok(0)
            }
            stmt => match self.open_tx.borrow_mut().as_mut() {
                // Dentro de BEGIN: ejecutar sin publicar.
                Some(tx) => exec::run_execute(tx, stmt, params),
                // Autocommit: o toda la sentencia, o nada.
                None => {
                    let mut tx = self.store.begin()?;
                    let n = exec::run_execute(&mut tx, stmt, params)?;
                    tx.commit()?;
                    Ok(n)
                }
            },
        }
    }

    /// Consultas. Devuelve las filas materializadas del snapshot actual; con
    /// una transacción abierta (`BEGIN`), de la transacción (que ve sus
    /// propias escrituras).
    pub fn query(&self, sql: &str, params: &[Value]) -> Result<Rows> {
        self.query_stmt(&sql::parse(sql)?, params)
    }

    fn query_stmt(&self, stmt: &Stmt, params: &[Value]) -> Result<Rows> {
        let Stmt::Select(select) = stmt else {
            return Err(sql_err("solo SELECT devuelve filas: usa execute"));
        };
        let out = match self.open_tx.borrow().as_ref() {
            Some(tx) => exec::run_select(tx, select, params)?,
            None => exec::run_select(&self.store.snapshot(), select, params)?,
        };
        Ok(Rows {
            columns: Arc::from(out.columns),
            rows: out.rows.into_iter(),
        })
    }

    /// Sentencia preparada: se parsea una vez y se ejecuta cuantas veces
    /// haga falta, con parámetros distintos.
    pub fn prepare(&self, sql: &str) -> Result<Statement<'_>> {
        Ok(Statement {
            conn: self,
            stmt: sql::parse(sql)?,
        })
    }

    /// Transacción explícita multi-sentencia. Adquiere el escritor único:
    /// si ya hay una escritura en curso (incluido un `BEGIN` SQL en esta
    /// misma conexión), devuelve [`Error::Busy`].
    pub fn begin(&self) -> Result<Transaction<'_>> {
        Ok(Transaction {
            tx: RefCell::new(self.store.begin()?),
            _conn: PhantomData,
        })
    }

    /// Versión actual (número de commit) de la base.
    pub fn version(&self) -> u64 {
        self.store.version()
    }
}

/// Sentencia preparada por [`Connection::prepare`]. Respeta el modo de la
/// conexión: dentro de un `BEGIN` ejecuta en esa transacción.
pub struct Statement<'conn> {
    conn: &'conn Connection,
    stmt: Stmt,
}

impl Statement<'_> {
    /// Como [`Connection::execute`], sin reparsear el SQL.
    pub fn execute(&self, params: &[Value]) -> Result<usize> {
        self.conn.execute_stmt(&self.stmt, params)
    }

    /// Como [`Connection::query`], sin reparsear el SQL.
    pub fn query(&self, params: &[Value]) -> Result<Rows> {
        self.conn.query_stmt(&self.stmt, params)
    }
}

/// Transacción explícita multi-sentencia (docs/03-api.md). Soltarla sin
/// [`commit`](Transaction::commit) es un rollback implícito: el archivo no
/// se ha tocado.
pub struct Transaction<'conn> {
    tx: RefCell<WriteTx>,
    /// Ata la transacción a su conexión (y por tanto a su hilo).
    _conn: PhantomData<&'conn Connection>,
}

impl Transaction<'_> {
    /// Ejecuta dentro de la transacción, sin publicar nada todavía.
    pub fn execute(&self, sql: &str, params: &[Value]) -> Result<usize> {
        match sql::parse(sql)? {
            Stmt::Begin => Err(sql_err("ya hay una transacción abierta")),
            Stmt::Commit | Stmt::Rollback => Err(sql_err(
                "dentro de una Transaction usa commit() o rollback()",
            )),
            Stmt::Select(_) => Err(sql_err("SELECT devuelve filas: usa query")),
            stmt => exec::run_execute(&mut self.tx.borrow_mut(), &stmt, params),
        }
    }

    /// Consulta que ve las escrituras propias de la transacción.
    pub fn query(&self, sql: &str, params: &[Value]) -> Result<Rows> {
        let Stmt::Select(select) = sql::parse(sql)? else {
            return Err(sql_err("solo SELECT devuelve filas: usa execute"));
        };
        let out = exec::run_select(&*self.tx.borrow(), &select, params)?;
        Ok(Rows {
            columns: Arc::from(out.columns),
            rows: out.rows.into_iter(),
        })
    }

    /// Publica la transacción. Devuelve la versión nueva (o la actual si la
    /// transacción no tocó nada).
    pub fn commit(self) -> Result<u64> {
        self.tx.into_inner().commit()
    }

    /// Descarta la transacción. Equivale a soltarla; existe por simetría y
    /// para que el rollback sea explícito en el código del llamador.
    pub fn rollback(self) -> Result<()> {
        drop(self.tx);
        Ok(())
    }
}

/// Resultado de una consulta. Itera `Result<Row>`.
pub struct Rows {
    columns: Arc<[String]>,
    rows: std::vec::IntoIter<Vec<Value>>,
}

impl Rows {
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
}

impl Iterator for Rows {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        let values = self.rows.next()?;
        Some(Ok(Row {
            columns: self.columns.clone(),
            values,
        }))
    }
}

pub struct Row {
    columns: Arc<[String]>,
    values: Vec<Value>,
}

impl Row {
    /// Acceso por índice (`0`) o por nombre (`"total"`).
    pub fn get<T: FromValue>(&self, col: impl ColIndex) -> Result<T> {
        T::from_value(&self.values[col.resolve(&self.columns)?])
    }

    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    pub fn values(&self) -> &[Value] {
        &self.values
    }
}

/// Índice de columna: posición o nombre.
pub trait ColIndex {
    fn resolve(&self, columns: &[String]) -> Result<usize>;
}

impl ColIndex for usize {
    fn resolve(&self, columns: &[String]) -> Result<usize> {
        if *self < columns.len() {
            Ok(*self)
        } else {
            Err(Error::InvalidInput("índice de columna fuera de rango"))
        }
    }
}

impl ColIndex for &str {
    fn resolve(&self, columns: &[String]) -> Result<usize> {
        columns
            .iter()
            .position(|c| c == self)
            .ok_or(Error::InvalidInput("no existe una columna con ese nombre"))
    }
}

/// Conversión tipada desde un `Value` de resultado.
pub trait FromValue: Sized {
    fn from_value(v: &Value) -> Result<Self>;
}

fn conv_err(expected: &'static str, v: &Value) -> Error {
    Error::Conversion {
        expected,
        got: v.type_name(),
    }
}

impl FromValue for Value {
    fn from_value(v: &Value) -> Result<Value> {
        Ok(v.clone())
    }
}

impl FromValue for i64 {
    fn from_value(v: &Value) -> Result<i64> {
        match v {
            Value::Integer(n) => Ok(*n),
            _ => Err(conv_err("INTEGER", v)),
        }
    }
}

impl FromValue for f64 {
    fn from_value(v: &Value) -> Result<f64> {
        match v {
            Value::Real(f) => Ok(*f),
            Value::Integer(n) => Ok(*n as f64), // promoción sin pérdida práctica
            _ => Err(conv_err("REAL", v)),
        }
    }
}

impl FromValue for bool {
    fn from_value(v: &Value) -> Result<bool> {
        match v {
            Value::Bool(b) => Ok(*b),
            _ => Err(conv_err("BOOLEAN", v)),
        }
    }
}

impl FromValue for String {
    fn from_value(v: &Value) -> Result<String> {
        match v {
            Value::Text(s) => Ok(s.clone()),
            _ => Err(conv_err("TEXT", v)),
        }
    }
}

impl FromValue for Vec<u8> {
    fn from_value(v: &Value) -> Result<Vec<u8>> {
        match v {
            Value::Blob(b) => Ok(b.clone()),
            _ => Err(conv_err("BLOB", v)),
        }
    }
}

impl<T: FromValue> FromValue for Option<T> {
    fn from_value(v: &Value) -> Result<Option<T>> {
        match v {
            Value::Null => Ok(None),
            v => Ok(Some(T::from_value(v)?)),
        }
    }
}

// --- conversiones hacia Value (parámetros) ---

impl From<i64> for Value {
    fn from(n: i64) -> Value {
        Value::Integer(n)
    }
}

impl From<i32> for Value {
    fn from(n: i32) -> Value {
        Value::Integer(n.into())
    }
}

impl From<f64> for Value {
    fn from(f: f64) -> Value {
        Value::Real(f)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Value {
        Value::Bool(b)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::Text(s.to_owned())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Value {
        Value::Text(s)
    }
}

impl From<Vec<u8>> for Value {
    fn from(b: Vec<u8>) -> Value {
        Value::Blob(b)
    }
}

impl From<&[u8]> for Value {
    fn from(b: &[u8]) -> Value {
        Value::Blob(b.to_vec())
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(o: Option<T>) -> Value {
        o.map_or(Value::Null, Into::into)
    }
}

/// Parámetros posicionales: `&params![1, "texto", 3.5]` ⇒ `&[Value]`.
#[macro_export]
macro_rules! params {
    () => { [] as [$crate::Value; 0] };
    ($($v:expr),+ $(,)?) => { [$($crate::Value::from($v)),+] };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_db() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("t.arkeion"), Options::default()).unwrap();
        (dir, db)
    }

    #[test]
    fn open_existing_and_create_if_missing_false() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.arkeion");
        assert!(matches!(
            Database::open(&path, Options::default().create_if_missing(false)),
            Err(Error::Io(_))
        ));
        drop(Database::open(&path, Options::default()).unwrap());
        // Reabrir el archivo existente con create_if_missing: true (carrera cubierta).
        Database::open(&path, Options::default()).unwrap();
    }

    #[test]
    fn execute_query_roundtrip_with_params() {
        let (_dir, db) = mem_db();
        let conn = db.connect().unwrap();
        conn.execute(
            "CREATE TABLE c (id INTEGER PRIMARY KEY, nombre TEXT, saldo REAL)",
            &[],
        )
        .unwrap();
        let n = conn
            .execute(
                "INSERT INTO c (nombre, saldo) VALUES (?1, ?2), (?3, ?4)",
                &params!["ana", 10.5, "bo", None::<i64>],
            )
            .unwrap();
        assert_eq!(n, 2);

        let mut rows = conn
            .query("SELECT * FROM c WHERE id = ?1", &params![2])
            .unwrap();
        let row = rows.next().unwrap().unwrap();
        assert_eq!(row.get::<String>("nombre").unwrap(), "bo");
        assert_eq!(row.get::<Option<f64>>("saldo").unwrap(), None);
        assert_eq!(row.get::<i64>(0).unwrap(), 2);
        assert!(rows.next().is_none());
        assert_eq!(conn.version(), 2); // dos sentencias = dos commits
    }

    #[test]
    fn wrong_statement_kind_is_an_error() {
        let (_dir, db) = mem_db();
        let conn = db.connect().unwrap();
        conn.execute("CREATE TABLE t (a INTEGER)", &[]).unwrap();
        assert!(matches!(
            conn.execute("SELECT * FROM t", &[]),
            Err(Error::Sql { .. })
        ));
        assert!(matches!(
            conn.query("INSERT INTO t VALUES (1)", &[]),
            Err(Error::Sql { .. })
        ));
    }

    #[test]
    fn failed_statement_is_fully_rolled_back() {
        let (_dir, db) = mem_db();
        let conn = db.connect().unwrap();
        conn.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL)",
            &[],
        )
        .unwrap();
        // La segunda fila viola NOT NULL ⇒ la sentencia entera se descarta.
        let res = conn.execute("INSERT INTO t (v) VALUES ('ok'), (NULL)", &[]);
        assert!(matches!(res, Err(Error::Constraint(_))));
        let rows: Vec<_> = conn.query("SELECT * FROM t", &[]).unwrap().collect();
        assert!(rows.is_empty(), "ni siquiera la primera fila debe quedar");
    }

    #[test]
    fn transaction_lifecycle_and_isolation() {
        let (_dir, db) = mem_db();
        let conn = db.connect().unwrap();
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[])
            .unwrap();
        let count = |c: &Connection| -> i64 {
            c.query("SELECT COUNT(*) FROM t", &[])
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .get(0)
                .unwrap()
        };

        // La transacción ve sus escrituras; la conexión (snapshot), no.
        let tx = conn.begin().unwrap();
        tx.execute("INSERT INTO t (v) VALUES ('a')", &[]).unwrap();
        let visto: i64 = tx
            .query("SELECT COUNT(*) FROM t", &[])
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .get(0)
            .unwrap();
        assert_eq!(visto, 1);
        assert_eq!(count(&conn), 0);
        tx.commit().unwrap();
        assert_eq!(count(&conn), 1);

        // Drop sin commit = rollback; rollback() explícito, lo mismo.
        let tx = conn.begin().unwrap();
        tx.execute("INSERT INTO t (v) VALUES ('b')", &[]).unwrap();
        drop(tx);
        let tx = conn.begin().unwrap();
        tx.execute("INSERT INTO t (v) VALUES ('c')", &[]).unwrap();
        tx.rollback().unwrap();
        assert_eq!(count(&conn), 1);
    }

    #[test]
    fn single_writer_and_tx_protocol_errors() {
        let (_dir, db) = mem_db();
        let conn = db.connect().unwrap();
        conn.execute("CREATE TABLE t (a INTEGER)", &[]).unwrap();

        // El escritor único: con una Transaction viva, todo lo demás es Busy.
        let tx = conn.begin().unwrap();
        assert!(matches!(conn.begin(), Err(Error::Busy)));
        assert!(matches!(conn.execute("BEGIN", &[]), Err(Error::Busy)));
        assert!(matches!(
            conn.execute("INSERT INTO t VALUES (1)", &[]),
            Err(Error::Busy)
        ));
        // Dentro de la Transaction, el protocolo se controla con tipos.
        assert!(matches!(tx.execute("BEGIN", &[]), Err(Error::Sql { .. })));
        assert!(matches!(tx.execute("COMMIT", &[]), Err(Error::Sql { .. })));
        assert!(matches!(
            tx.execute("SELECT * FROM t", &[]),
            Err(Error::Sql { .. })
        ));
        drop(tx);

        // BEGIN SQL: doble BEGIN y COMMIT/ROLLBACK huérfanos son errores SQL.
        conn.execute("BEGIN", &[]).unwrap();
        assert!(matches!(conn.execute("BEGIN", &[]), Err(Error::Sql { .. })));
        conn.execute("ROLLBACK", &[]).unwrap();
        assert!(matches!(
            conn.execute("COMMIT", &[]),
            Err(Error::Sql { .. })
        ));
        assert!(matches!(
            conn.execute("ROLLBACK", &[]),
            Err(Error::Sql { .. })
        ));
    }

    #[test]
    fn prepared_statements_reuse_and_follow_connection_mode() {
        let (_dir, db) = mem_db();
        let conn = db.connect().unwrap();
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[])
            .unwrap();

        let ins = conn.prepare("INSERT INTO t (v) VALUES (?1)").unwrap();
        ins.execute(&params!["x"]).unwrap();
        ins.execute(&params!["y"]).unwrap();
        let sel = conn.prepare("SELECT COUNT(*) FROM t WHERE v = ?1").unwrap();
        let n = |s: &Statement<'_>, v: &str| -> i64 {
            s.query(&params![v])
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .get(0)
                .unwrap()
        };
        assert_eq!(n(&sel, "x"), 1);

        // Dentro de un BEGIN, la preparada ejecuta y consulta esa transacción.
        conn.execute("BEGIN", &[]).unwrap();
        ins.execute(&params!["z"]).unwrap();
        assert_eq!(n(&sel, "z"), 1);
        conn.execute("ROLLBACK", &[]).unwrap();
        assert_eq!(n(&sel, "z"), 0);

        // El tipo de sentencia se valida al ejecutar, no al preparar.
        assert!(
            conn.prepare("SELECT 1 FROM t")
                .unwrap()
                .execute(&[])
                .is_err()
        );
        assert!(ins.query(&params!["w"]).is_err());
    }

    #[test]
    fn conversion_errors_are_typed() {
        let (_dir, db) = mem_db();
        let conn = db.connect().unwrap();
        conn.execute("CREATE TABLE t (a TEXT)", &[]).unwrap();
        conn.execute("INSERT INTO t VALUES ('hola')", &[]).unwrap();
        let row = conn
            .query("SELECT a FROM t", &[])
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        assert!(matches!(
            row.get::<i64>("a"),
            Err(Error::Conversion {
                expected: "INTEGER",
                got: "TEXT"
            })
        ));
        assert!(row.get::<i64>("zz").is_err());
    }
}
