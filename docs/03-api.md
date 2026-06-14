# 03 — API pública de Rust

Ergonomía objetivo: que quien viene de `rusqlite` se sienta en casa en cinco minutos, y que las
capacidades de versionado (time-travel, ramas, auditoría) sean de primera clase, no un añadido.

Paquete: [`arkeion` en crates.io](https://crates.io/crates/arkeion) — nombre reservado con un
placeholder `0.0.1` (2026-06-07) y publicado de verdad desde `0.10.0` (2026-06-10; el *minor*
sigue los hitos M0–M10).

## Tipos principales

```rust
#![forbid(unsafe_code)]

pub struct Database;            // handle compartido y clonable (Arc interno)
pub struct Connection;          // vista sobre una rama (por defecto "main")
pub struct Transaction<'conn>;  // escritura explícita multi-sentencia
pub struct Rows;                // iterador de resultados
pub struct Row;

#[derive(Clone, Debug, PartialEq)]
pub enum Value { Null, Bool(bool), Integer(i64), Real(f64), Text(String), Blob(Vec<u8>) }

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u64);

#[derive(Clone, Copy)]
pub enum AsOf { Head, Version(u64), Timestamp(std::time::SystemTime) }

pub struct Options {
    pub create_if_missing: bool,        // true por defecto (zero-config)
    pub key: Option<Key>,               // Some(_) ⇒ AES-256-GCM en reposo
    pub compress: bool,                 // compresión de página al crear (M10, off por defecto)
    pub ecc_nsym: u8,                   // paridad Reed-Solomon por bloque al crear (M10, 0 = off)
    pub cache_bytes: usize,             // tope de caché de páginas (def. 64 MiB ≈ PRAGMA cache_size)
}
// builders: .create_if_missing(b) .key(k) .compress(b) .ecc(n) .cache_bytes(n)

pub struct Key([u8; 32]);               // clave cruda; Drop ⇒ zeroize. KDF: responsabilidad
                                        // del llamador en v1 (D7)

#[non_exhaustive]
pub enum Error { Io(..), Corrupt{..}, ChainBroken{at: Version, ..}, WrongKey,
                 Sql{msg: String, pos: usize}, Conflict(MergeConflicts),
                 BranchNotFound(String), VersionNotFound(AsOf), Busy, .. }
pub type Result<T> = std::result::Result<T, Error>;
```

## `Database` — ciclo de vida, ramas, auditoría

```rust
impl Database {
    pub fn open(path: impl AsRef<Path>, opts: Options) -> Result<Database>;

    pub fn connect(&self) -> Result<Connection>;                  // rama "main"
    pub fn connect_branch(&self, name: &str) -> Result<Connection>;

    pub fn create_branch(&self, name: &str, from: AsOf) -> Result<()>;
    pub fn drop_branch(&self, name: &str) -> Result<()>;          // borra la ref, no las páginas
    pub fn branches(&self) -> Result<Vec<BranchInfo>>;            // {name, head: Version, created}

    pub fn diff(&self, from: &str, to: &str) -> Result<Diff>;     // O(cambios), no O(datos)
    pub fn merge(&self, from: &str, into: &str, policy: MergePolicy) -> Result<MergeReport>;

    pub fn verify(&self) -> Result<AuditReport>;                  // recorre la hash chain entera
    pub fn verify_anchor(&self, anchor: &AuditAnchor) -> Result<AuditReport>;  // + ancla: detecta truncado/reescritura

    pub fn history(&self) -> Result<Vec<Revision>>;               // "git log": línea temporal de versiones
    pub fn diff_versions(&self, from: u64, to: u64) -> Result<Diff>;           // "git diff" entre dos versiones

    pub fn vacuum(&self, retention: Retention) -> Result<VacuumReport>;        // compacta + rename atómico
    pub fn vacuum_rekey(&self, retention: Retention, new_key: Option<Key>)     // compacta y rota la clave
        -> Result<VacuumReport>;
}

pub enum MergePolicy { FailOnConflict }                 // v1; futuras: Theirs, Ours, resolver
pub enum Retention   { KeepAll, KeepLast(u64), KeepSince(SystemTime) }   // frontera K conservada

pub struct Diff { pub tables: Vec<TableDiff> }          // altas/bajas/modificaciones por rowid,
                                                        // y diffs de esquema
pub struct AuditReport { pub head: u64, pub commits: u64, pub chain_ok: bool, pub chain_hash: [u8; 32] }
pub struct AuditAnchor { pub version: u64, pub chain_hash: [u8; 32] }   // AuditReport::anchor() lo crea
pub struct Revision { pub version: u64, pub timestamp: SystemTime, pub parent: u64 }
pub struct VacuumReport {                               // qué conservó y cuánto recuperó
    pub kept_from: u64, pub head: u64, pub reclaimed_versions: u64,
    pub pages_before: u64, pub pages_after: u64,
}
```

## `Connection` — SQL, transacciones, time-travel

```rust
impl Connection {
    pub fn execute(&self, sql: &str, params: &[Value]) -> Result<usize>;   // filas afectadas
    pub fn query(&self, sql: &str, params: &[Value]) -> Result<Rows>;
    pub fn prepare(&self, sql: &str) -> Result<Statement>;                 // parsea una vez

    /// Carga masiva: todas las filas en UNA transacción (1 fsync), sin executor
    /// SQL por fila; las entradas de índice se insertan en bloque (UNIQUE
    /// verificado). Solo en autocommit: o el lote entero o nada.
    pub fn bulk_insert<I, R>(&self, table: &str, rows: I) -> Result<usize>
    where I: IntoIterator<Item = R>, R: AsRef<[Value]>;

    pub fn begin(&self) -> Result<Transaction<'_>>;     // adquiere el escritor único

    pub fn snapshot(&self, at: AsOf) -> Result<Connection>;  // conexión SOLO LECTURA fijada
    pub fn version(&self) -> Version;                   // head actual de la rama
    pub fn branch(&self) -> &str;
}

impl Transaction<'_> {
    pub fn execute(&self, sql: &str, params: &[Value]) -> Result<usize>;
    pub fn query(&self, sql: &str, params: &[Value]) -> Result<Rows>;      // lee sus escrituras
    pub fn commit(self) -> Result<Version>;
    pub fn rollback(self) -> Result<()>;                // Drop sin commit ⇒ rollback implícito
}
```

`execute`/`query` fuera de transacción = autocommit (una transacción por sentencia).

Un SELECT simple (proyección de columnas o `*`, sin WHERE/JOIN/agregados/ORDER
BY) se sirve en **streaming**: `Rows` posee su snapshot y decodifica cada fila
al iterar — solo las columnas proyectadas, sin materializar el resultado. El
resto de consultas va por el executor clásico; el resultado es indistinguible
salvo en coste.

## Filas y parámetros

```rust
impl Rows { /* Iterator<Item = Result<Row>> */ }

impl Row {
    pub fn get<T: FromValue>(&self, col: impl ColIndex) -> Result<T>;  // por índice o nombre
}

// FromValue para: i64, f64, String, Vec<u8>, bool, Option<T>, Value
// Into<Value> para los mismos → macro de conveniencia:
let n = conn.execute(
    "INSERT INTO clientes (nombre, alta) VALUES (?1, ?2)",
    &params!["Acme GmbH", 1718000000_i64],
)?;
```

## Ejemplo integral

```rust
use arkeion::{Database, Options, AsOf, MergePolicy, params};

let db = Database::open("tenant-42.arkeion", Options::default().with_key(key))?;
let conn = db.connect()?;

conn.execute("CREATE TABLE facturas (id INTEGER PRIMARY KEY, total REAL, estado TEXT)", &[])?;

let tx = conn.begin()?;
tx.execute("INSERT INTO facturas (total, estado) VALUES (?1, ?2)", &params![120.0, "borrador"])?;
let v1 = tx.commit()?;

// — time-travel —
conn.execute("UPDATE facturas SET estado = 'emitida' WHERE id = 1", &[])?;
let antes = conn.snapshot(AsOf::Version(v1.0))?;
let estado: String = antes.query("SELECT estado FROM facturas WHERE id = 1", &[])?
                          .next().unwrap()?.get(0)?;          // "borrador"

// — branching para una migración —
db.create_branch("migracion-iva", AsOf::Head)?;
let mig = db.connect_branch("migracion-iva")?;
mig.execute("UPDATE facturas SET total = total * 1.21", &[])?;
let diff = db.diff("main", "migracion-iva")?;                 // revisar antes de fusionar
db.merge("migracion-iva", "main", MergePolicy::FailOnConflict)?;

// — auditoría —
assert!(db.verify()?.chain_ok);
```

## Garantías de la API

- `Database: Send + Sync` y barato de clonar; `Connection` por hilo (no `Sync`; `Send` sí).
- Lecturas jamás bloquean ni son bloqueadas por la escritura en curso.
- `snapshot()` y las conexiones de rama comparten la misma caché de páginas del `Database`.
- Toda lectura valida integridad (tag); `Corrupt`/`ChainBroken`/`WrongKey` son errores tipados,
  nunca datos silenciosamente malos.

## Acceso por red (cliente-servidor, M11)

La API de arriba es **embebida** (in-process). Para acceder por red, el módulo de
conexión (ver [11-cliente-servidor](11-cliente-servidor.md)) lo expone con un
protocolo nativo —rama por sesión, `AS OF` y `verify` de primera clase— en crates
aparte:

- **`arkeion-client`** (`Client`, MIT/Apache, repo propio): `connect`, `use_branch`,
  `execute`, `query` / `query_as_of`, `verify`. Espejo del subconjunto de
  `Connection` que viaja por el cable.
- **`arkeiond`** (`arkeion-server`, EUPL-1.2): el daemon, thread-por-conexión, que
  sirve cada sesión sobre una rama del mismo `Database`.

El protocolo va a mano (`arkeion-proto`), sin serde, reusando el `varint` y la
codificación de `Value` de este motor.
