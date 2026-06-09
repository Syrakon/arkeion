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
use std::time::{Duration, UNIX_EPOCH};

use crate::branch::Diff;
use crate::commit::{AuditAnchor, AuditReport};
use crate::crypto::Key;
use crate::error::{Error, Result};
use crate::exec;
use crate::record::Value;
use crate::sql::{
    self,
    ast::{AsOfClause, Stmt},
};
use crate::tx::{
    AsOf, BranchInfo, MAIN_BRANCH, MergePolicy, MergeReport, Retention, Revision, Snapshot, Store,
    VacuumReport, WriteTx,
};

/// Opciones de apertura. *Zero-config*: los valores por defecto funcionan.
#[derive(Clone, Debug)]
pub struct Options {
    pub create_if_missing: bool,
    /// Clave de cifrado en reposo (M7, D7). `Some` ⇒ al crear se cifra el
    /// archivo, al abrir se descifra. `None` ⇒ sin cifrado.
    pub key: Option<Key>,
    /// Compresión de página (M10). Solo aplica **al crear** (queda marcada en el
    /// header); al abrir un archivo existente se respeta lo que diga el header.
    /// `false` por defecto (D8: el core mínimo no comprime).
    pub compress: bool,
    /// Corrección de errores por página (M10): bytes de paridad Reed-Solomon por
    /// bloque de 255 (par; corrige `ecc_nsym/2` bytes corruptos por bloque). Solo
    /// aplica **al crear**. `0` = sin ECC (por defecto, D8).
    pub ecc_nsym: u8,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            create_if_missing: true,
            key: None,
            compress: false,
            ecc_nsym: 0,
        }
    }
}

impl Options {
    pub fn create_if_missing(mut self, create: bool) -> Options {
        self.create_if_missing = create;
        self
    }

    /// Activa el cifrado en reposo con una clave cruda de 32 B (M7). El KDF
    /// queda fuera del motor (D7): el llamador entrega la clave ya derivada.
    /// Abrir un archivo cifrado sin clave da [`Error::KeyRequired`]; con una
    /// clave que no encaja, [`Error::WrongKey`].
    ///
    /// ```
    /// use arkeion::{Database, Key, Options};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let path = dir.path().join("cifrada.arkeion");
    /// let clave = || Key::new([0x42; 32]);
    ///
    /// // Crear cifrada y escribir.
    /// let db = Database::open(&path, Options::default().key(clave())).unwrap();
    /// db.connect()
    ///     .unwrap()
    ///     .execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", &[])
    ///     .unwrap();
    /// drop(db);
    ///
    /// // Reabrir exige la clave correcta.
    /// assert!(Database::open(&path, Options::default().create_if_missing(false)).is_err());
    /// assert!(Database::open(&path, Options::default().key(clave())).is_ok());
    /// ```
    pub fn key(mut self, key: Key) -> Options {
        self.key = Some(key);
        self
    }

    /// Activa la compresión de página al crear (M10). Tras un `trait` y con un
    /// backend pure-Rust por defecto (D8); off por defecto. No afecta a abrir un
    /// archivo ya existente (su compresión la fija el header). Nota CRIME: con
    /// cifrado, comprimir filtra información gruesa por el tamaño de página.
    pub fn compress(mut self, on: bool) -> Options {
        self.compress = on;
        self
    }

    /// Activa la corrección de errores por página al crear (M10): `nsym` bytes de
    /// paridad Reed-Solomon por bloque de 255 (debe ser par; corrige `nsym/2`
    /// bytes corruptos por bloque). Convierte la detección en recuperación dentro
    /// del presupuesto; fuera de él, falla limpio. `0` = off. Gasta una fracción
    /// del espacio (≈ `nsym/255`); combínalo con compresión para netos pequeños.
    pub fn ecc(mut self, nsym: u8) -> Options {
        self.ecc_nsym = nsym;
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
        let key = opts.key.as_ref();
        let store = if opts.create_if_missing {
            match Store::create_with(path, key, opts.compress, opts.ecc_nsym) {
                Ok(s) => s,
                // Ya existe (o carrera con otro creador): abrir.
                Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    Store::open_keyed(path, key)?
                }
                Err(e) => return Err(e),
            }
        } else {
            Store::open_keyed(path, key)?
        };
        Ok(Database {
            store: Arc::new(store),
        })
    }

    /// Conexión sobre la rama principal.
    pub fn connect(&self) -> Result<Connection> {
        Ok(Connection {
            store: self.store.clone(),
            branch: MAIN_BRANCH.to_owned(),
            open_tx: RefCell::new(None),
            pinned: None,
        })
    }

    /// Conexión sobre una rama concreta (M8): sus lecturas y escrituras van a
    /// esa rama. `BranchNotFound` si no existe.
    pub fn connect_branch(&self, name: &str) -> Result<Connection> {
        self.store.snapshot_on(name)?; // valida que la rama existe
        Ok(Connection {
            store: self.store.clone(),
            branch: name.to_owned(),
            open_tx: RefCell::new(None),
            pinned: None,
        })
    }

    /// Crea una rama apuntando al estado `from` (M8, D5). `BranchExists` si ya
    /// existe. Es un commit meta-only: comparte físicamente las páginas con
    /// `from` hasta que diverja (CoW).
    pub fn create_branch(&self, name: &str, from: AsOf) -> Result<()> {
        self.store.create_branch(name, from)
    }

    /// Borra una rama (M8): elimina su ref; las páginas quedan hasta `vacuum`
    /// (M9). No se puede borrar `main`.
    pub fn drop_branch(&self, name: &str) -> Result<()> {
        self.store.drop_branch(name)
    }

    /// Lista todas las ramas y la versión a la que apunta cada una (M8).
    pub fn branches(&self) -> Result<Vec<BranchInfo>> {
        self.store.branches()
    }

    /// Diferencias de `from` a `to` entre dos ramas (M8): cambios de esquema y
    /// de fila. O(cambios) — salta los subárboles físicamente compartidos.
    pub fn diff(&self, from: &str, to: &str) -> Result<Diff> {
        Ok(crate::branch::decode(&self.store.diff(from, to)?))
    }

    /// Diferencias entre dos **versiones** de la historia (post-M9): el "git
    /// diff" entre dos puntos en el tiempo. Combínalo con [`history`](Database::history)
    /// (el "git log") para inspeccionar qué cambió y cuándo. `0` = estado vacío
    /// inicial; una versión futura o compactada por `vacuum` da
    /// [`Error::VersionNotFound`]. Para ver qué hizo un solo commit `v`, usa
    /// `diff_versions(v - 1, v)`.
    ///
    /// ```
    /// use arkeion::{Database, Options};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let db = Database::open(dir.path().join("t.arkeion"), Options::default()).unwrap();
    /// let conn = db.connect().unwrap();
    /// conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)", &[]).unwrap();
    /// conn.execute("INSERT INTO t (n) VALUES (10)", &[]).unwrap(); // v2
    /// conn.execute("INSERT INTO t (n) VALUES (20)", &[]).unwrap(); // v3
    ///
    /// // Qué cambió en el commit v3 (la segunda inserción): una fila nueva.
    /// let d = db.diff_versions(2, 3).unwrap();
    /// assert_eq!(d.rows.len(), 1);
    /// assert!(d.schema.is_empty());
    /// ```
    pub fn diff_versions(&self, from: u64, to: u64) -> Result<Diff> {
        Ok(crate::branch::decode(&self.store.diff_versions(from, to)?))
    }

    /// Los cambios que introdujo un commit concreto: el "git show" de la versión
    /// `version` (equivale a `diff_versions(version - 1, version)`).
    pub fn changes(&self, version: u64) -> Result<Diff> {
        Ok(crate::branch::decode(&self.store.changes(version)?))
    }

    /// Fusiona `from` en `into` (merge 3-way, M8). Un merge limpio aplica
    /// exactamente el diff de `from`; con [`MergePolicy::FailOnConflict`], una
    /// clave cambiada distinto en ambas ramas devuelve [`Error::Conflict`].
    pub fn merge(&self, from: &str, into: &str, policy: MergePolicy) -> Result<MergeReport> {
        self.store.merge(from, into, policy)
    }

    /// Auditoría completa de la hash chain (M6): recorre todos los commits de
    /// génesis a head y devuelve un [`AuditReport`]. Devuelve
    /// [`Error::ChainBroken`] con la versión exacta si una página histórica fue
    /// manipulada (D4). La cadena cubre el plaintext, así que la auditoría es
    /// independiente de si el archivo está cifrado.
    ///
    /// ```
    /// use arkeion::{Database, Options};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let db = Database::open(dir.path().join("t.arkeion"), Options::default()).unwrap();
    /// let conn = db.connect().unwrap();
    /// conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", &[]).unwrap();
    ///
    /// let report = db.verify().unwrap();
    /// assert!(report.chain_ok);
    /// assert_eq!(report.head, report.commits);
    /// ```
    pub fn verify(&self) -> Result<AuditReport> {
        self.store.verify()
    }

    /// Auditoría **contra un ancla externa** guardada antes (post-M9): además de
    /// la integridad completa que hace [`verify`](Database::verify), prueba que a
    /// la versión del ancla el `chain_hash` sigue siendo el mismo. Cierra el
    /// hueco de `verify`: detecta que alguien **trunque o reescriba** la historia
    /// para fabricar una cadena válida más corta (no podrá reproducir el hash
    /// anclado). Crea el ancla con [`AuditReport::anchor`] y guárdala/publícala.
    ///
    /// ```
    /// use arkeion::{Database, Options};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let path = dir.path().join("t.arkeion");
    /// let db = Database::open(&path, Options::default()).unwrap();
    /// let conn = db.connect().unwrap();
    /// conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)", &[]).unwrap();
    /// conn.execute("INSERT INTO t (n) VALUES (1)", &[]).unwrap();
    ///
    /// // Ancla este estado y guárdala fuera.
    /// let anchor = db.verify().unwrap().anchor();
    ///
    /// // Más adelante (incluso con más commits) sigue cuadrando.
    /// conn.execute("INSERT INTO t (n) VALUES (2)", &[]).unwrap();
    /// assert!(db.verify_anchor(&anchor).is_ok());
    /// ```
    pub fn verify_anchor(&self, anchor: &AuditAnchor) -> Result<AuditReport> {
        self.store.verify_anchor(anchor)
    }

    /// Línea temporal de versiones confirmadas, de la más antigua a la más nueva:
    /// el "git log" de los datos. Cada [`Revision`] es consultable con `AS OF`
    /// (vía SQL o [`Connection::snapshot`]). Tras `vacuum` solo aparecen las
    /// versiones retenidas.
    ///
    /// ```
    /// use arkeion::{Database, Options};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let db = Database::open(dir.path().join("t.arkeion"), Options::default()).unwrap();
    /// let conn = db.connect().unwrap();
    /// conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)", &[]).unwrap();
    /// conn.execute("INSERT INTO t (n) VALUES (1)", &[]).unwrap();
    ///
    /// let log = db.history().unwrap();
    /// assert_eq!(log.len(), 2); // CREATE TABLE + INSERT
    /// assert_eq!(log[0].version, 1);
    /// assert_eq!(log[1].version, 2);
    /// ```
    pub fn history(&self) -> Result<Vec<Revision>> {
        self.store.history()
    }

    /// Compacta el archivo según `retention` (M9): reescribe la base entera a un
    /// archivo temporal y la publica con un **rename atómico**. Tras vacuum,
    /// [`verify`](Database::verify) sigue dando OK (la cadena rearranca de un
    /// checkpoint) y las versiones **retenidas** siguen respondiendo `AS OF`; las
    /// compactadas dan [`Error::VersionNotFound`]. El presente nunca se pierde.
    ///
    /// Crash-safe: un kill en cualquier punto antes del rename deja el archivo
    /// original intacto (solo queda un temporal huérfano, que el próximo vacuum
    /// borra). El mismo handle y sus conexiones siguen válidos: las lecturas
    /// nuevas ven el archivo compactado; un snapshot histórico ya abierto sigue
    /// leyendo su versión hasta soltarse.
    ///
    /// Conserva la clave de cifrado actual. Requiere una sola rama: con otras
    /// ramas vivas devuelve [`Error::InvalidInput`] (vacuum linealiza la
    /// historia); fusiónalas o bórralas antes de compactar.
    ///
    /// ```
    /// use arkeion::{Database, Options, Retention};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let db = Database::open(dir.path().join("t.arkeion"), Options::default()).unwrap();
    /// let conn = db.connect().unwrap();
    /// conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)", &[]).unwrap();
    /// for i in 0..50 {
    ///     conn.execute("INSERT INTO t (n) VALUES (?1)", &arkeion::params![i]).unwrap();
    /// }
    /// let report = db.vacuum(Retention::KeepLast(5)).unwrap();
    /// assert!(report.pages_after <= report.pages_before);
    /// assert!(db.verify().unwrap().chain_ok);
    /// ```
    pub fn vacuum(&self, retention: Retention) -> Result<VacuumReport> {
        self.store.vacuum(retention)
    }

    /// Como [`vacuum`](Database::vacuum) pero además **rota la clave** de cifrado
    /// a `new_key` (M9, D6): `Some(k)` recifra con `k` desde la primera página;
    /// `None` desactiva el cifrado. El archivo nuevo se sella entero con la clave
    /// destino, así que es la vía soportada para cambiar de clave. Úsala con una
    /// clave genuinamente nueva.
    pub fn vacuum_rekey(&self, retention: Retention, new_key: Option<Key>) -> Result<VacuumReport> {
        self.store.vacuum_rekey(retention, new_key.as_ref())
    }
}

fn sql_err(msg: impl Into<String>) -> Error {
    Error::Sql {
        msg: msg.into(),
        pos: None,
    }
}

/// Construye el vector posicional para una sentencia con parámetros nombrados,
/// a partir del binding `(:nombre, valor)`. El nombre del binding puede llevar
/// los dos puntos o no. Falla si falta el valor de algún parámetro de la query.
fn bind_named(names: &[String], bindings: &[(&str, Value)]) -> Result<Vec<Value>> {
    names
        .iter()
        .map(|name| {
            bindings
                .iter()
                .find(|(k, _)| k.trim_start_matches(':') == name)
                .map(|(_, v)| v.clone())
                .ok_or_else(|| sql_err(format!("falta el parámetro :{name}")))
        })
        .collect()
}

/// Traduce el `AS OF` ya parseado de un SELECT al punto temporal del almacén.
fn clause_to_asof(clause: &AsOfClause) -> AsOf {
    match *clause {
        AsOfClause::Version(v) => AsOf::Version(v),
        AsOfClause::Timestamp(ms) => AsOf::Timestamp(UNIX_EPOCH + Duration::from_millis(ms)),
    }
}

/// Conexión: ejecuta SQL. Las lecturas usan snapshots inmutables y jamás
/// bloquean ni son bloqueadas por la escritura en curso.
pub struct Connection {
    store: Arc<Store>,
    /// Rama sobre la que lee y escribe esta conexión (M8). `main` por defecto;
    /// otra vía [`Database::connect_branch`].
    branch: String,
    /// Transacción abierta con `BEGIN` (SQL). `None` = autocommit. Soltarla
    /// sin `COMMIT` (p. ej. al soltar la conexión) es un rollback.
    open_tx: RefCell<Option<WriteTx>>,
    /// Si está, la conexión es de **solo lectura** fijada a este snapshot
    /// histórico ([`Connection::snapshot`]): toda consulta lo ve y las
    /// escrituras devuelven error. Vacío en una conexión normal.
    pinned: Option<Snapshot>,
}

impl Connection {
    /// Sentencias que no devuelven filas (DDL, INSERT, UPDATE, DELETE,
    /// BEGIN/COMMIT/ROLLBACK). Devuelve filas afectadas. Fuera de `BEGIN`
    /// es autocommit: o toda la sentencia, o nada.
    pub fn execute(&self, sql: &str, params: &[Value]) -> Result<usize> {
        self.execute_stmt(&sql::parse(sql)?, params)
    }

    fn execute_stmt(&self, stmt: &Stmt, params: &[Value]) -> Result<usize> {
        if self.pinned.is_some() {
            return Err(sql_err(
                "conexión de solo lectura (snapshot histórico): no admite escrituras",
            ));
        }
        match stmt {
            Stmt::Select(_) => Err(sql_err("SELECT devuelve filas: usa query")),
            Stmt::Begin => {
                let mut open = self.open_tx.borrow_mut();
                if open.is_some() {
                    return Err(sql_err(
                        "ya hay una transacción abierta: COMMIT o ROLLBACK antes de BEGIN",
                    ));
                }
                *open = Some(self.store.begin_on(&self.branch)?);
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
                    let mut tx = self.store.begin_on(&self.branch)?;
                    let n = exec::run_execute(&mut tx, stmt, params)?;
                    tx.commit()?;
                    Ok(n)
                }
            },
        }
    }

    /// Como [`execute`](Connection::execute) pero con parámetros **nombrados**
    /// (`:nombre`), enlazados por nombre (los dos puntos del binding son
    /// opcionales). Cómodo con muchos parámetros o cuando se repite el mismo.
    ///
    /// ```
    /// use arkeion::{Database, Options, named_params};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let db = Database::open(dir.path().join("t.arkeion"), Options::default()).unwrap();
    /// let conn = db.connect().unwrap();
    /// conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER)", &[]).unwrap();
    /// conn.execute_named(
    ///     "INSERT INTO t (a, b) VALUES (:x, :x)",  // el mismo parámetro repetido
    ///     &named_params! { ":x" => 7 },
    /// ).unwrap();
    /// let n = conn.query("SELECT a FROM t WHERE a = b", &[]).unwrap().count();
    /// assert_eq!(n, 1);
    /// ```
    pub fn execute_named(&self, sql: &str, params: &[(&str, Value)]) -> Result<usize> {
        let (stmt, names) = sql::parse_full(sql)?;
        self.execute_stmt(&stmt, &bind_named(&names, params)?)
    }

    /// Consultas. Devuelve las filas materializadas del snapshot actual; con
    /// una transacción abierta (`BEGIN`), de la transacción (que ve sus
    /// propias escrituras).
    pub fn query(&self, sql: &str, params: &[Value]) -> Result<Rows> {
        self.query_stmt(&sql::parse(sql)?, params)
    }

    /// Como [`query`](Connection::query) pero con parámetros **nombrados**.
    pub fn query_named(&self, sql: &str, params: &[(&str, Value)]) -> Result<Rows> {
        let (stmt, names) = sql::parse_full(sql)?;
        self.query_stmt(&stmt, &bind_named(&names, params)?)
    }

    fn query_stmt(&self, stmt: &Stmt, params: &[Value]) -> Result<Rows> {
        let Stmt::Select(select) = stmt else {
            return Err(sql_err("solo SELECT devuelve filas: usa execute"));
        };
        let out = if let Some(snap) = &self.pinned {
            // Conexión ya fijada a un instante: no se puede re-fijar por sentencia.
            if select.as_of.is_some() {
                return Err(sql_err(
                    "la conexión ya está fijada a un snapshot: quita el AS OF de la consulta",
                ));
            }
            exec::run_select(snap, select, params)?
        } else if let Some(clause) = &select.as_of {
            // AS OF lee la historia confirmada; ignora una tx abierta (no se
            // escribe en el pasado: para eso están las ramas, M8).
            let snap = self.store.snapshot_at(clause_to_asof(clause))?;
            exec::run_select(&snap, select, params)?
        } else {
            match self.open_tx.borrow().as_ref() {
                Some(tx) => exec::run_select(tx, select, params)?,
                None => exec::run_select(&self.store.snapshot_on(&self.branch)?, select, params)?,
            }
        };
        Ok(Rows {
            columns: Arc::from(out.columns),
            rows: out.rows.into_iter(),
        })
    }

    /// Sentencia preparada: se parsea una vez y se ejecuta cuantas veces
    /// haga falta, con parámetros distintos.
    pub fn prepare(&self, sql: &str) -> Result<Statement<'_>> {
        let (stmt, param_names) = sql::parse_full(sql)?;
        Ok(Statement {
            conn: self,
            stmt,
            param_names,
        })
    }

    /// Transacción explícita multi-sentencia. Adquiere el escritor único:
    /// si ya hay una escritura en curso (incluido un `BEGIN` SQL en esta
    /// misma conexión), devuelve [`Error::Busy`].
    pub fn begin(&self) -> Result<Transaction<'_>> {
        if self.pinned.is_some() {
            return Err(sql_err(
                "conexión de solo lectura (snapshot histórico): no admite transacciones",
            ));
        }
        Ok(Transaction {
            tx: RefCell::new(self.store.begin_on(&self.branch)?),
            _conn: PhantomData,
        })
    }

    /// Conexión **de solo lectura** fijada a un punto de la historia
    /// (time-travel, M5): todas sus consultas ven ese instante y las
    /// escrituras devuelven error. Falla con [`Error::VersionNotFound`] si el
    /// punto no existe. La versión es la autoridad; el timestamp es
    /// informativo (docs/05, D12).
    ///
    /// ```
    /// use arkeion::{AsOf, Database, Options};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let db = Database::open(dir.path().join("t.arkeion"), Options::default()).unwrap();
    /// let conn = db.connect().unwrap();
    /// conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER NOT NULL)", &[])
    ///     .unwrap();
    /// conn.execute("INSERT INTO t (n) VALUES (1)", &[]).unwrap();
    /// let v1 = conn.version();
    /// conn.execute("INSERT INTO t (n) VALUES (2)", &[]).unwrap();
    ///
    /// // Una vista fijada a v1 solo ve la primera fila.
    /// let antes = conn.snapshot(AsOf::Version(v1)).unwrap();
    /// let mut rows = antes.query("SELECT n FROM t", &[]).unwrap();
    /// assert_eq!(rows.next().unwrap().unwrap().get::<i64>("n").unwrap(), 1);
    /// assert!(rows.next().is_none());
    ///
    /// // Es de solo lectura.
    /// assert!(antes.execute("INSERT INTO t (n) VALUES (3)", &[]).is_err());
    /// ```
    pub fn snapshot(&self, at: AsOf) -> Result<Connection> {
        // Resuelve ya: una versión inexistente falla aquí, no al consultar.
        let snap = self.store.snapshot_at(at)?;
        Ok(Connection {
            store: self.store.clone(),
            branch: self.branch.clone(),
            open_tx: RefCell::new(None),
            pinned: Some(snap),
        })
    }

    /// Versión actual de la conexión: el snapshot fijado si la conexión es de
    /// solo lectura ([`Connection::snapshot`]), o el head de su rama si no.
    pub fn version(&self) -> u64 {
        match &self.pinned {
            Some(snap) => snap.version(),
            None => self
                .store
                .snapshot_on(&self.branch)
                .map_or(0, |s| s.version()),
        }
    }
}

/// Sentencia preparada por [`Connection::prepare`]. Respeta el modo de la
/// conexión: dentro de un `BEGIN` ejecuta en esa transacción.
pub struct Statement<'conn> {
    conn: &'conn Connection,
    stmt: Stmt,
    /// Nombres de los parámetros `:nombre` (índice → nombre); vacío si la
    /// sentencia usa parámetros posicionales `?N`.
    param_names: Vec<String>,
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

    /// Como [`execute`](Statement::execute) con parámetros **nombrados**.
    pub fn execute_named(&self, params: &[(&str, Value)]) -> Result<usize> {
        self.conn
            .execute_stmt(&self.stmt, &bind_named(&self.param_names, params)?)
    }

    /// Como [`query`](Statement::query) con parámetros **nombrados**.
    pub fn query_named(&self, params: &[(&str, Value)]) -> Result<Rows> {
        self.conn
            .query_stmt(&self.stmt, &bind_named(&self.param_names, params)?)
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
        if select.as_of.is_some() {
            return Err(sql_err(
                "AS OF no está disponible dentro de una transacción de escritura: \
                 usa una consulta normal o Connection::snapshot",
            ));
        }
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

/// Parámetros nombrados: `&named_params!{ ":id" => 1, ":n" => "x" }` ⇒
/// `&[(&str, Value)]`. Los dos puntos del nombre son opcionales.
#[macro_export]
macro_rules! named_params {
    () => { [] as [(&str, $crate::Value); 0] };
    ($($k:expr => $v:expr),+ $(,)?) => { [$(($k, $crate::Value::from($v))),+] };
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
