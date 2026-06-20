//! Capa transaccional: `Store` (KV ACID), snapshots de lectura sin locks y
//! escritor único serializado (D9).
//!
//! Una transacción de escritura acumula páginas **sucias en memoria** con ids
//! ya definitivos (posiciones ≥ EOF lógico, posible porque el escritor es
//! único). El commit escribe las páginas de datos, añade la página de commit
//! (hash chain incluida) y hace **un** fsync: ese es el punto de durabilidad
//! (M9-perf). Un solo fsync basta porque el tag de integridad por página vuelve
//! ilegible cualquier escritura a medias y la recuperación se detiene en ella.
//! Un rollback es simplemente soltar el estado en memoria.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::btree::{self, Body, Cursor, NodeSource, NodeStore};
use crate::catalog::{self, FtsIndexDef, IndexDef, TableDef, TableScan, TableSpec};
use crate::commit::{self, COMMIT_FLAG_CHECKPOINT, CommitHeader, Head};
use crate::compress::{Compressor, Densa};
use crate::crypto::Key;
use crate::error::{Error, Result};
use crate::format::{MIN_RECORD_LEN, PageBuf, PageId};
use crate::io::sync_parent_dir;
use crate::pager::{CACHE_CAP, Pager, ScrubReport, provider_for};
use crate::record::Value;

/// Rama única de M1; el branching llega en M8.
pub const MAIN_BRANCH: &str = "main";

// Espacios de claves del árbol meta global (docs/02). `0x03` (índice temporal)
// quedó libre en M9-perf: `AS OF TIMESTAMP` se resuelve desde `META_HIST`.
const META_REF: u8 = 0x01;
const META_HIST: u8 = 0x02;

/// Punto en la historia al que fijar una lectura (time-travel, M5). El
/// timestamp es informativo; la versión es la autoridad (docs/05, D12).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AsOf {
    /// Head actual de la rama (equivale a [`Store::snapshot`]).
    Head,
    /// Versión exacta (número de commit). `VersionNotFound` si es futura o ya
    /// la compactó `vacuum` (M9).
    Version(u64),
    /// Mayor versión cuyo commit tiene timestamp ≤ el dado.
    Timestamp(SystemTime),
}

/// Cabeza resuelta de una rama: su estado de datos y versión (privado, M8).
struct BranchHead {
    version: u64,
    data_root: PageId,
    /// Página de commit cabeza; informativa salvo en la rama tip (ver
    /// `write_meta_indices`).
    commit_page: u64,
}

/// Información pública de una rama (M8). El timestamp de creación (`created`)
/// llegará en v1.x.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchInfo {
    pub name: String,
    /// Versión (commit) a la que apunta la rama.
    pub head: u64,
}

/// Una entrada de la línea temporal de versiones: el "git log" de los datos
/// (post-M9). Cada commit confirmado es una revisión consultable con `AS OF`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revision {
    /// Número de versión (monótono global).
    pub version: u64,
    /// Timestamp del commit (informativo; la versión es la autoridad, D12).
    pub timestamp: SystemTime,
    /// Versión padre en la ascendencia de datos (0 = enraizada en génesis).
    pub parent: u64,
}

/// Política de resolución de conflictos de `merge` (M8). v1: solo abortar; las
/// políticas `Theirs`/`Ours`/resolver llegan en v1.x.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergePolicy {
    /// Aborta con [`Error::Conflict`] ante cualquier conflicto.
    FailOnConflict,
}

/// Resultado de un merge limpio (M8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeReport {
    /// Versión nueva de la rama destino (o su head si no había nada que aplicar).
    pub version: u64,
    /// Número de cambios de la rama origen aplicados.
    pub applied: usize,
}

/// Un conflicto de merge: una clave del árbol de datos que ambas ramas llevaron
/// a estados distintos (M8). `from`/`into` son el estado final en cada rama
/// (`None` = borrada). La clave cruda (`[0x01,table_id,rowid]` para filas,
/// `[0x00,0x01,nombre]` para esquema) la decodifica la capa pública.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeConflict {
    pub key: Vec<u8>,
    pub from: Option<Vec<u8>>,
    pub into: Option<Vec<u8>>,
}

/// Política de retención de historia para [`Store::vacuum`] (M9): fija la
/// frontera K (versión más antigua conservada). Las versiones `< K` se compactan
/// (su `AS OF` pasa a [`Error::VersionNotFound`]); las `≥ K` se conservan y
/// siguen respondiendo `AS OF`. El presente (head) **siempre** se conserva.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retention {
    /// Conserva toda la historia: `vacuum` solo desfragmenta (y, si se pide,
    /// rota la clave). K = 1.
    KeepAll,
    /// Conserva las últimas `n` versiones (head incluido). `KeepLast(0)` y
    /// `KeepLast(1)` conservan solo el head.
    KeepLast(u64),
    /// Conserva las versiones con timestamp de commit ≥ el dado. Si todas son
    /// más antiguas, conserva solo el head.
    KeepSince(SystemTime),
}

/// Resumen de un `vacuum` (M9): qué se conservó y cuánto se recuperó.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VacuumReport {
    /// Frontera K: versión más antigua conservada (1 si se conservó todo). 0 si
    /// la base no tenía commits.
    pub kept_from: u64,
    /// Versión head: invariante, `vacuum` nunca pierde el presente.
    pub head: u64,
    /// Versiones compactadas (descartadas): `kept_from - 1` (0 si K ≤ 1).
    pub reclaimed_versions: u64,
    /// Páginas del archivo antes de compactar.
    pub pages_before: u64,
    /// Páginas del archivo después: la desfragmentación medible.
    pub pages_after: u64,
}

/// Estado mutable del almacén: el pager activo y la cabeza global. Viven juntos
/// bajo un único `Mutex` para que `vacuum` (M9) pueda reescribir el archivo y
/// publicar el par `(pager, head)` nuevo de forma atómica: ningún lector llega
/// a ver un pager y un head descasados (un `data_root` de un archivo leído del
/// otro). Los snapshots ya en vuelo siguen siendo dueños de su `Arc<Pager>`
/// anterior, así que leen su archivo (el inodo viejo) hasta soltarse.
/// Compuerta del **escritor único**: solo una transacción de escritura a la vez.
/// El autocommit la adquiere **bloqueando** ([`acquire`](WriterGate::acquire) — cola:
/// bajo contención los escritores esperan su turno en vez de girar reintentando
/// `Busy`); las transacciones explícitas y las ops de mantenimiento usan
/// [`try_acquire`](WriterGate::try_acquire) (no bloqueante → `Busy`), porque retienen
/// el escritor un tiempo indefinido y no deben colgar a otros. Con group commit el
/// turno dura microsegundos (el commit suelta el escritor antes del fsync).
/// Tope de espera del autocommit por el escritor: normalmente el turno se sirve en
/// microsegundos (la write-phase suelta el escritor antes del fsync); este tope solo
/// salta ante un escritor atascado o una tx explícita retenida sin fin, devolviendo
/// `Busy` en vez de colgar para siempre (como `busy_timeout` de SQLite).
const WRITER_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);

struct WriterGate {
    held: Mutex<bool>,
    cv: Condvar,
}

impl WriterGate {
    fn new() -> WriterGate {
        WriterGate {
            held: Mutex::new(false),
            cv: Condvar::new(),
        }
    }

    /// Bloquea (encola) hasta adquirir el escritor o agotar `timeout`. `true` si lo
    /// adquirió; `false` si expiró el tope (válvula de seguridad contra un escritor
    /// atascado o una tx explícita retenida sin fin → el llamador recibe `Busy`).
    fn acquire_timeout(&self, timeout: Duration) -> bool {
        let held = self.held.lock().expect("compuerta del escritor envenenada");
        let (mut held, _) = self
            .cv
            .wait_timeout_while(held, timeout, |h| *h)
            .expect("compuerta del escritor envenenada");
        if *held {
            return false; // sigue ocupado tras el tope
        }
        *held = true;
        true
    }

    /// No bloqueante: `true` si lo adquirió, `false` si estaba ocupado.
    fn try_acquire(&self) -> bool {
        let mut held = self.held.lock().expect("compuerta del escritor envenenada");
        if *held {
            false
        } else {
            *held = true;
            true
        }
    }

    /// Libera el escritor y despierta a un esperante (si lo hay).
    fn release(&self) {
        let mut held = self.held.lock().expect("compuerta del escritor envenenada");
        *held = false;
        self.cv.notify_one();
    }
}

/// **Group commit** (líder-seguidor): agrupa los `fdatasync` de commits
/// concurrentes en uno solo, sin debilitar la durabilidad. El commit que llega
/// cuando no hay sello en curso se vuelve **líder** y lanza su fsync de inmediato
/// (un commit solo nunca espera — cero latencia añadida); los que llegan mientras
/// ese fsync está en vuelo se encolan y el líder los sella a todos de una pasada.
/// La propia latencia del fsync ES la ventana de agrupación: sin temporizadores.
///
/// Vive **por generación de pager** (uno nuevo en cada `vacuum`): así un commit en
/// vuelo siempre sella su propio archivo aunque `vacuum` cambie el `DbState`.
struct SyncGroup {
    inner: Mutex<SyncProgress>,
    cv: Condvar,
}

struct SyncProgress {
    /// Hay un fsync en curso (un líder dentro de `pager.sync()`).
    in_progress: bool,
    /// Mayor versión cuyas páginas ya están **durables** (fsync completado).
    durable_version: u64,
    /// Mayor versión cuyas páginas ya están **escritas** en el archivo (marca de
    /// agua; un fsync ahora la haría durable). Se actualiza bajo el lock del escritor.
    written_version: u64,
    /// Head que casa con `written_version`, para `write_meta` tras sellar.
    written_head: Head,
}

impl SyncGroup {
    fn new(head: Head) -> SyncGroup {
        SyncGroup {
            inner: Mutex::new(SyncProgress {
                in_progress: false,
                durable_version: head.version,
                written_version: head.version,
                written_head: head,
            }),
            cv: Condvar::new(),
        }
    }

    /// Marca (bajo el lock del escritor, durante la write-phase) que las páginas de
    /// `head` ya están escritas en el archivo: sube la marca de agua de «escrito».
    fn mark_written(&self, head: Head) {
        let mut p = self.inner.lock().expect("sync group envenenado");
        if head.version >= p.written_version {
            p.written_version = head.version;
            p.written_head = head;
        }
    }

    /// Asegura que el commit `version` (en `pager`) sea durable, agrupando con los
    /// fsync concurrentes. Devuelve cuando `version` está sellado.
    fn make_durable(&self, pager: &Pager, version: u64) -> Result<()> {
        let mut p = self.inner.lock().expect("sync group envenenado");
        loop {
            if p.durable_version >= version {
                return Ok(()); // ya lo selló otro líder
            }
            if p.in_progress {
                p = self.cv.wait(p).expect("sync group envenenado");
                continue; // seguidor: espera al líder en curso
            }
            // Líder: captura la marca de agua y sella TODO hasta ahí de una pasada.
            // `fdatasync` vuelca todo lo escrito antes de la llamada, así que cubre a
            // los demás committers cuyas páginas ya estaban en el archivo.
            p.in_progress = true;
            let target_version = p.written_version;
            let target_head = p.written_head.clone();
            drop(p);
            let res = pager
                .sync()
                .and_then(|()| pager.write_meta(&commit::meta_for(&target_head)));
            p = self.inner.lock().expect("sync group envenenado");
            p.in_progress = false;
            self.cv.notify_all();
            match res {
                Ok(()) => {
                    if target_version > p.durable_version {
                        p.durable_version = target_version;
                    }
                    // El bucle re-chequea: si mi versión ya es durable, retorna;
                    // si llegué después del corte, me vuelvo el siguiente líder.
                }
                Err(e) => return Err(e),
            }
        }
    }
}

struct DbState {
    pager: Arc<Pager>,
    head: Head,
    /// Coordinador de group commit de ESTA generación de pager (nuevo en `vacuum`).
    sync_group: Arc<SyncGroup>,
}

/// Almacén clave-valor transaccional sobre un único archivo. Todos sus campos
/// son `Arc`: clonarlo es barato y las transacciones son dueñas de lo que
/// necesitan (sin lifetimes hacia el `Store`).
#[derive(Clone)]
pub struct Store {
    state: Arc<Mutex<DbState>>,
    writer: Arc<WriterGate>,
    /// Ruta del archivo: la necesita `vacuum` para el archivo temporal y el
    /// rename atómico (M9).
    path: PathBuf,
    /// Tope de la caché de páginas (nº de páginas) con el que se abrió, derivado
    /// de [`Options::cache_bytes`](crate::Options::cache_bytes). Se conserva para
    /// que `vacuum` reabra el archivo compactado con la misma configuración.
    cache_pages: usize,
}

impl Store {
    pub fn create(path: &Path) -> Result<Store> {
        Self::create_keyed(path, None)
    }

    /// Crea el almacén; con `key`, cifrado en reposo (M7, D6).
    pub fn create_keyed(path: &Path, key: Option<&Key>) -> Result<Store> {
        Self::create_with(path, key, false, 0)
    }

    /// Crea el almacén con cifrado (`key`), compresión de página (`compress`) y/o
    /// corrección de errores (`ecc_nsym` bytes de paridad RS por bloque; 0 = off)
    /// opcionales (M7/M10). Todo se decide al crear (queda en el header) y al
    /// abrir se lee de ahí; off por defecto (D8).
    pub fn create_with(
        path: &Path,
        key: Option<&Key>,
        compress: bool,
        ecc_nsym: u8,
    ) -> Result<Store> {
        Self::create_with_cache(path, key, compress, ecc_nsym, CACHE_CAP)
    }

    /// Como [`create_with`](Store::create_with) pero con un tope de caché explícito
    /// (`cache_pages`), propagado desde [`Options::cache_bytes`](crate::Options::cache_bytes).
    pub fn create_with_cache(
        path: &Path,
        key: Option<&Key>,
        compress: bool,
        ecc_nsym: u8,
        cache_pages: usize,
    ) -> Result<Store> {
        let compressor = compress.then(|| Arc::new(Densa) as Arc<dyn Compressor>);
        let pager = Pager::create_with_crypto(
            path,
            key.is_some(),
            provider_for(key),
            compressor,
            ecc_nsym,
            cache_pages,
        )?;
        let head = commit::genesis_head(&pager.header().file_id);
        Ok(Store::from_parts(path, pager, head, cache_pages))
    }

    pub fn open(path: &Path) -> Result<Store> {
        Self::open_keyed(path, None)
    }

    /// Abre el almacén; con `key`, descifra la zona append (M7). `KeyRequired`
    /// si el archivo está cifrado y falta clave; `WrongKey` si no encaja.
    pub fn open_keyed(path: &Path, key: Option<&Key>) -> Result<Store> {
        Self::open_keyed_with_cache(path, key, CACHE_CAP)
    }

    /// Como [`open_keyed`](Store::open_keyed) pero con un tope de caché explícito
    /// (`cache_pages`), propagado desde [`Options::cache_bytes`](crate::Options::cache_bytes).
    pub fn open_keyed_with_cache(
        path: &Path,
        key: Option<&Key>,
        cache_pages: usize,
    ) -> Result<Store> {
        let pager = Pager::open_keyed_with(path, key, cache_pages)?;
        let head = commit::recover(&pager)?;
        // Margen de nonce (D6, R7): tras recortar al head, la cola rota es la
        // región física `[fin del head, EOF)`. Cada escritura sellada que dejó
        // bytes en disco ocupa al menos `MIN_RECORD_LEN`, así que
        // `ceil(bytes / MIN_RECORD_LEN)` es una cota superior de los contadores
        // consumidos por la cola. Retomar el contador pasado ese margen hace
        // estructuralmente imposible reutilizar un par (clave, nonce), incluso
        // tras crashes repetidos (una escritura que no dejó bytes no aporta
        // ciphertext en disco, así que reusar su contador es inocuo).
        let file_len = pager.byte_len()?;
        pager.truncate_to_head(&head);
        let torn_bytes = file_len.saturating_sub(pager.write_offset());
        pager.set_nonce_counter(head.nonce_counter + torn_bytes.div_ceil(MIN_RECORD_LEN));
        Ok(Store::from_parts(path, pager, head, cache_pages))
    }

    fn from_parts(path: &Path, pager: Pager, head: Head, cache_pages: usize) -> Store {
        Store {
            state: Arc::new(Mutex::new(DbState {
                pager: Arc::new(pager),
                sync_group: Arc::new(SyncGroup::new(head.clone())),
                head,
            })),
            writer: Arc::new(WriterGate::new()),
            path: path.to_owned(),
            cache_pages,
        }
    }

    fn lock(&self) -> MutexGuard<'_, DbState> {
        self.state.lock().expect("estado del store envenenado")
    }

    /// Pager activo (clon barato del `Arc`). Cada llamada lo relee del estado:
    /// no cachear el resultado entre operaciones, pues `vacuum` puede sustituirlo.
    #[cfg(test)]
    fn pager(&self) -> Arc<Pager> {
        self.lock().pager.clone()
    }

    /// Snapshot de lectura: fija el commit actual y lee páginas inmutables.
    /// Nunca bloquea ni es bloqueado por el escritor.
    pub fn snapshot(&self) -> Snapshot {
        let st = self.lock();
        Snapshot {
            pager: st.pager.clone(),
            version: st.head.version,
            data_root: st.head.data_root,
        }
    }

    /// Snapshot de lectura fijado a la cabeza de una rama (M8). `BranchNotFound`
    /// si la rama no existe.
    pub fn snapshot_on(&self, branch: &str) -> Result<Snapshot> {
        let (pager, global) = {
            let st = self.lock();
            (st.pager.clone(), st.head.clone())
        };
        let bh = resolve_branch_head(&pager, &global, branch)?;
        Ok(Snapshot {
            pager,
            version: bh.version,
            data_root: bh.data_root,
        })
    }

    /// Transacción de escritura sobre la rama principal.
    pub fn begin(&self) -> Result<WriteTx> {
        self.begin_on(MAIN_BRANCH)
    }

    /// Transacción de escritura sobre una rama (M8). `Busy` si ya hay otra en
    /// curso (R3: no se bloquea, se informa); `BranchNotFound` si la rama no
    /// existe. Se libera al hacer commit o al soltarla (rollback).
    pub fn begin_on(&self, branch: &str) -> Result<WriteTx> {
        if !self.writer.try_acquire() {
            return Err(Error::Busy);
        }
        self.build_writetx(branch)
    }

    /// Como [`begin_on`](Store::begin_on) pero **bloquea** (encola) hasta adquirir
    /// el escritor único en vez de devolver `Busy`. Lo usa el **autocommit**: bajo
    /// contención los escritores esperan su turno (la write-phase dura microsegundos
    /// porque el commit suelta el escritor antes del fsync, ver group commit) en vez
    /// de girar reintentando. Las transacciones explícitas siguen en `begin_on` (no
    /// bloqueante): retienen el escritor un tiempo indefinido y no deben colgar a otros.
    pub fn begin_on_blocking(&self, branch: &str) -> Result<WriteTx> {
        if !self.writer.acquire_timeout(WRITER_ACQUIRE_TIMEOUT) {
            return Err(Error::Busy);
        }
        self.build_writetx(branch)
    }

    /// Construye la `WriteTx` con el escritor **ya adquirido**; lo libera si falla.
    fn build_writetx(&self, branch: &str) -> Result<WriteTx> {
        let (pager, sync_group, global) = {
            let st = self.lock();
            (st.pager.clone(), st.sync_group.clone(), st.head.clone())
        };
        let bh = match resolve_branch_head(&pager, &global, branch) {
            Ok(bh) => bh,
            Err(e) => {
                self.writer.release(); // liberar el escritor
                return Err(e);
            }
        };
        Ok(WriteTx {
            ts: TxStore::new(pager.clone()),
            pager,
            sync_group,
            writer_released: false,
            state: self.state.clone(),
            writer: self.writer.clone(),
            branch: branch.to_owned(),
            parent_page: bh.commit_page,
            parent_version: bh.version,
            data_root: bh.data_root,
            meta_root: global.meta_root,
            base: global,
            rowid_cache: HashMap::new(),
            rec_buf: Vec::new(),
            enc_buf: Vec::new(),
            values_buf: Vec::new(),
            schema_cache: HashMap::new(),
            trigger_cache: None,
            trigger_depth: 0,
            savepoints: Vec::new(),
        })
    }

    // --- gestión de ramas (M8) ---

    /// Crea una rama apuntando al estado `from` (M8, D5). Commit meta-only:
    /// añade `ref[name]` compartiendo el `data_root` de `from` (CoW: nada de
    /// datos se copia hasta que la rama diverge). `BranchExists` si ya existe.
    pub fn create_branch(&self, name: &str, from: AsOf) -> Result<()> {
        if name.is_empty() || name.len() > commit::BRANCH_MAX {
            return Err(Error::InvalidInput(
                "nombre de rama vacío o de más de 64 bytes",
            ));
        }
        if !self.writer.try_acquire() {
            return Err(Error::Busy);
        }
        let result = self.create_branch_locked(name, from);
        self.writer.release();
        result
    }

    fn create_branch_locked(&self, name: &str, from: AsOf) -> Result<()> {
        let (pager, global) = {
            let st = self.lock();
            (st.pager.clone(), st.head.clone())
        };
        if read_ref(&pager, global.meta_root, name)?.is_some() {
            return Err(Error::BranchExists(name.to_owned()));
        }
        let from_version = snapshot_at(&pager, &global, from)?.version();
        let from_data_root = read_data_root(&pager, global.meta_root, from_version)?;

        let version = global.version + 1;
        let timestamp_ms = now_ms();
        let mut ts = TxStore::new(pager.clone());
        let meta_root = write_meta_indices(
            &mut ts,
            global.meta_root,
            version,
            from_data_root,
            timestamp_ms,
            name,
            from_version,
        )?;
        let head = publish_commit(
            &pager,
            &mut ts,
            CommitParams::after(&global, name, global.commit_page),
            from_data_root,
            meta_root,
            timestamp_ms,
        )?;
        self.lock().head = head;
        Ok(())
    }

    /// Borra una rama (M8): elimina su `ref`; las páginas de datos quedan (las
    /// recupera `vacuum`, M9). Commit meta-only sobre `main` (datos sin cambio).
    /// No se puede borrar `main`.
    pub fn drop_branch(&self, name: &str) -> Result<()> {
        if name == MAIN_BRANCH {
            return Err(Error::InvalidInput("no se puede borrar la rama principal"));
        }
        if !self.writer.try_acquire() {
            return Err(Error::Busy);
        }
        let result = self.drop_branch_locked(name);
        self.writer.release();
        result
    }

    fn drop_branch_locked(&self, name: &str) -> Result<()> {
        let (pager, global) = {
            let st = self.lock();
            (st.pager.clone(), st.head.clone())
        };
        if read_ref(&pager, global.meta_root, name)?.is_none() {
            return Err(Error::BranchNotFound(name.to_owned()));
        }
        let main = resolve_branch_head(&pager, &global, MAIN_BRANCH)?;
        let version = global.version + 1;
        let timestamp_ms = now_ms();
        let mut ts = TxStore::new(pager.clone());
        let (meta_root, _) = btree::delete(&mut ts, global.meta_root, &ref_key(name))?;
        let meta_root = write_meta_indices(
            &mut ts,
            meta_root,
            version,
            main.data_root,
            timestamp_ms,
            MAIN_BRANCH,
            main.version,
        )?;
        let head = publish_commit(
            &pager,
            &mut ts,
            CommitParams::after(&global, MAIN_BRANCH, main.commit_page),
            main.data_root,
            meta_root,
            timestamp_ms,
        )?;
        self.lock().head = head;
        Ok(())
    }

    /// Lista todas las ramas y la versión a la que apunta cada una (M8). `main`
    /// siempre aparece (head 0 antes del primer commit).
    pub fn branches(&self) -> Result<Vec<BranchInfo>> {
        let (pager, global) = {
            let st = self.lock();
            (st.pager.clone(), st.head.clone())
        };
        let src = PagerSource(pager);
        let mut out = Vec::new();
        let mut has_main = false;
        for item in btree::scan_from(&src, global.meta_root, &[META_REF])? {
            let (key, val) = item?;
            if key.first() != Some(&META_REF) {
                break; // fuera del espacio de refs
            }
            let name = String::from_utf8_lossy(&key[1..]).into_owned();
            let head = u64::from_le_bytes(
                val.get(0..8)
                    .ok_or(Error::CorruptRecord("ref de rama truncada"))?
                    .try_into()
                    .expect("rango fijo de 8 bytes"),
            );
            has_main |= name == MAIN_BRANCH;
            out.push(BranchInfo { name, head });
        }
        if !has_main {
            out.insert(
                0,
                BranchInfo {
                    name: MAIN_BRANCH.to_owned(),
                    head: 0,
                },
            );
        }
        Ok(out)
    }

    /// Diferencias del árbol de datos entre dos ramas (M8): los cambios de
    /// `from` a `to`. O(cambios) gracias al skip de subárboles compartidos.
    pub fn diff(&self, from: &str, to: &str) -> Result<Vec<btree::KeyDiff>> {
        let (pager, global) = {
            let st = self.lock();
            (st.pager.clone(), st.head.clone())
        };
        let from_root = resolve_branch_head(&pager, &global, from)?.data_root;
        let to_root = resolve_branch_head(&pager, &global, to)?.data_root;
        let src = PagerSource(pager);
        btree::diff(&src, from_root, to_root)
    }

    /// Cambios introducidos por un commit concreto (post-M9): el "git show" de
    /// la versión `version` (equivale a `diff_versions(version-1, version)`).
    pub fn changes(&self, version: u64) -> Result<Vec<btree::KeyDiff>> {
        // El delta de un commit es contra su **padre registrado**, no contra el
        // predecesor numérico `version-1`: en merges y puntos de bifurcación
        // `version-1` no es el padre, y diffear contra él inventa borrados/oculta
        // inserts (procedencia falsa en una herramienta de auditoría).
        let (pager, head) = {
            let st = self.lock();
            (st.pager.clone(), st.head.clone())
        };
        let parent = read_parent_version(&pager, head.meta_root, version)?;
        self.diff_versions(parent, version)
    }

    /// Diferencias del árbol de datos entre dos **versiones** (post-M9): el "git
    /// diff" entre dos puntos de la historia (cf. [`history`](Store::history)).
    /// `0` = estado génesis; una versión futura o ya compactada por `vacuum` da
    /// `VersionNotFound`. O(cambios) como [`diff`](Store::diff).
    pub fn diff_versions(&self, from: u64, to: u64) -> Result<Vec<btree::KeyDiff>> {
        let (pager, head) = {
            let st = self.lock();
            (st.pager.clone(), st.head.clone())
        };
        let from_root = read_data_root(&pager, head.meta_root, from)?;
        let to_root = read_data_root(&pager, head.meta_root, to)?;
        let src = PagerSource(pager);
        btree::diff(&src, from_root, to_root)
    }

    /// Fusiona `from` en `into` (merge 3-way, M8). Encuentra el ancestro común
    /// por versión, compara `diff(base, from)` con `diff(base, into)` clave a
    /// clave: aplica los cambios que solo hizo `from`; ante una clave que ambas
    /// llevaron a estados distintos, conflicto. Un merge limpio aplica
    /// exactamente el diff y nada más (nuevo commit en `into`).
    pub fn merge(&self, from: &str, into: &str, policy: MergePolicy) -> Result<MergeReport> {
        let MergePolicy::FailOnConflict = policy; // v1: única política
        if !self.writer.try_acquire() {
            return Err(Error::Busy);
        }
        let result = self.merge_locked(from, into);
        self.writer.release();
        result
    }

    fn merge_locked(&self, from: &str, into: &str) -> Result<MergeReport> {
        let (pager, global) = {
            let st = self.lock();
            (st.pager.clone(), st.head.clone())
        };
        let vf = read_ref(&pager, global.meta_root, from)?
            .ok_or_else(|| Error::BranchNotFound(from.to_owned()))?;
        let vi = read_ref(&pager, global.meta_root, into)?
            .ok_or_else(|| Error::BranchNotFound(into.to_owned()))?;

        let base_v = merge_base(&pager, global.meta_root, vf, vi)?;
        let base_root = read_data_root(&pager, global.meta_root, base_v)?;
        let from_root = read_data_root(&pager, global.meta_root, vf)?;
        let into_root = read_data_root(&pager, global.meta_root, vi)?;

        let src = PagerSource(pager.clone());
        let from_changes = btree::diff(&src, base_root, from_root)?;
        let into_changes = btree::diff(&src, base_root, into_root)?;
        let into_targets: HashMap<&[u8], Option<Vec<u8>>> = into_changes
            .iter()
            .map(|c| (c.key.as_slice(), target_of(&c.change)))
            .collect();

        let mut to_apply: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
        let mut conflicts: Vec<MergeConflict> = Vec::new();
        for c in &from_changes {
            let tf = target_of(&c.change);
            match into_targets.get(c.key.as_slice()) {
                None => to_apply.push((c.key.clone(), tf)), // solo `from` lo cambió
                Some(ti) => {
                    if is_counter_key(&c.key) && tf.is_some() && ti.is_some() {
                        // Contador de rowid: tomar el mayor, sin conflicto.
                        let merged = max_counter(tf.as_deref().unwrap(), ti.as_deref().unwrap());
                        to_apply.push((c.key.clone(), Some(merged)));
                    } else if &tf == ti {
                        // Ambas ramas convergen al mismo estado: nada que aplicar.
                    } else {
                        conflicts.push(MergeConflict {
                            key: c.key.clone(),
                            from: tf,
                            into: ti.clone(),
                        });
                    }
                }
            }
        }

        if !conflicts.is_empty() {
            return Err(Error::Conflict(conflicts));
        }
        if to_apply.is_empty() {
            return Ok(MergeReport {
                version: vi,
                applied: 0,
            });
        }

        // Aplicar los cambios de `from` sobre `into`: nuevo commit en `into`.
        let mut ts = TxStore::new(pager.clone());
        let mut data_root = into_root;
        for (key, target) in &to_apply {
            data_root = match target {
                Some(v) => btree::insert(&mut ts, data_root, key, v)?,
                None => btree::delete(&mut ts, data_root, key)?.0,
            };
        }
        let version = global.version + 1;
        let timestamp_ms = now_ms();
        let into_commit_page = if vi == global.version {
            global.commit_page
        } else {
            0
        };
        let meta_root = write_meta_indices(
            &mut ts,
            global.meta_root,
            version,
            data_root,
            timestamp_ms,
            into,
            vi,
        )?;
        let head = publish_commit(
            &pager,
            &mut ts,
            CommitParams::after(&global, into, into_commit_page),
            data_root,
            meta_root,
            timestamp_ms,
        )?;
        self.lock().head = head;
        Ok(MergeReport {
            version,
            applied: to_apply.len(),
        })
    }

    pub fn version(&self) -> u64 {
        self.lock().head.version
    }

    /// Línea temporal de versiones confirmadas, de la más antigua a la más nueva
    /// (post-M9): el "git log" de los datos. Solo las versiones **retenidas**
    /// (vacuum compacta el resto). Cada una es consultable con `AS OF`. Lee el
    /// índice histórico `META_HIST` del head actual.
    pub fn history(&self) -> Result<Vec<Revision>> {
        let (pager, head) = {
            let st = self.lock();
            (st.pager.clone(), st.head.clone())
        };
        let src = PagerSource(pager);
        let mut out = Vec::new();
        for item in btree::scan_from(&src, head.meta_root, &[META_HIST])? {
            let (key, val) = item?;
            if key.first() != Some(&META_HIST) || key.len() != 1 + 8 {
                break; // fuera del espacio histórico
            }
            let version = u64::from_be_bytes(key[1..9].try_into().expect("rango fijo de 8 bytes"));
            // hist_val = data_root(8) ‖ ts(8) ‖ parent(8).
            let ms = u64::from_le_bytes(
                val.get(8..16)
                    .ok_or(Error::CorruptRecord("entrada histórica truncada"))?
                    .try_into()
                    .expect("rango fijo de 8 bytes"),
            );
            let parent = val
                .get(16..24)
                .map(|b| u64::from_le_bytes(b.try_into().expect("rango fijo de 8 bytes")))
                .unwrap_or(0); // entradas pre-M8 (16 B): enraizadas en génesis
            out.push(Revision {
                version,
                timestamp: ms_to_system_time(ms),
                parent,
            });
        }
        Ok(out)
    }

    /// Auditoría completa de la hash chain hasta el head actual (M6). Devuelve
    /// un [`commit::AuditReport`]; `ChainBroken` con la versión exacta si una
    /// página histórica fue manipulada.
    pub fn verify(&self) -> Result<commit::AuditReport> {
        let (pager, head) = {
            let st = self.lock();
            (st.pager.clone(), st.head.clone())
        };
        commit::verify(&pager, &head)
    }

    /// Auditoría + comprobación de un ancla externa (post-M9): además de la
    /// integridad, prueba que a la versión anclada el `chain_hash` sigue siendo
    /// el guardado. Detecta truncado/reescritura de la historia.
    pub fn verify_anchor(&self, anchor: &commit::AuditAnchor) -> Result<commit::AuditReport> {
        let (pager, head) = {
            let st = self.lock();
            (st.pager.clone(), st.head.clone())
        };
        commit::verify_anchored(&pager, &head, Some(anchor))
    }

    /// Scrubbing (M10 C3): barre toda la historia retenida desde el disco
    /// forzando la corrección ECC y reporta las páginas degradadas. Complementa a
    /// `verify`: como el ECC corrige al leer de forma transparente, `verify` pasa
    /// sobre una página que el ECC arregló y **no** delata el disco que se
    /// degrada; el scrubbing sí (`corrected > 0`). Pensado para correr periódico.
    pub fn scrub(&self) -> ScrubReport {
        let pager = self.lock().pager.clone();
        pager.scrub()
    }

    /// Snapshot histórico (M5, time-travel). `AsOf::Head` equivale a
    /// [`snapshot`](Store::snapshot). Funciona porque el b-tree es CoW
    /// append-only: la raíz de cada versión sigue en disco hasta que `vacuum`
    /// (M9) la compacte, y el índice histórico del árbol meta (`META_HIST`,
    /// escrito en cada commit) la localiza.
    pub fn snapshot_at(&self, at: AsOf) -> Result<Snapshot> {
        let (pager, head) = {
            let st = self.lock();
            (st.pager.clone(), st.head.clone())
        };
        snapshot_at(&pager, &head, at)
    }

    /// `snapshot_at` **acotado a la rama** (M8): `AS OF` solo resuelve versiones
    /// que estén en la **ascendencia** de la rama, no cualquier versión global. Sin
    /// esto, `AS OF VERSION n` desde una rama podía leer datos de otra línea
    /// temporal (las versiones son globales monótonas, pero el `AS OF` de una rama
    /// debe quedarse en su historia). El meta-índice es global (compartido); lo que
    /// cambia es la versión/`data_root` cabeza y el filtro de ascendencia.
    pub fn snapshot_at_on(&self, branch: &str, at: AsOf) -> Result<Snapshot> {
        let (pager, global) = {
            let st = self.lock();
            (st.pager.clone(), st.head.clone())
        };
        let bh = resolve_branch_head(&pager, &global, branch)?;
        let meta = global.meta_root;
        match at {
            AsOf::Head => Ok(snapshot_of(&pager, bh.version, bh.data_root)),
            AsOf::Version(v) => {
                if v == bh.version {
                    return Ok(snapshot_of(&pager, bh.version, bh.data_root));
                }
                // La versión debe existir Y estar en la línea temporal de la rama.
                if v > bh.version || !is_ancestor(&pager, meta, bh.version, v)? {
                    return Err(Error::VersionNotFound(AsOf::Version(v)));
                }
                if v == 0 {
                    return Ok(genesis_snapshot(&pager));
                }
                let data_root = read_data_root(&pager, meta, v)?;
                Ok(snapshot_of(&pager, v, data_root))
            }
            AsOf::Timestamp(t) => {
                // Mayor ancestro de la rama con `ts ≤` el dado (recorre la
                // ascendencia, no todo el índice global).
                let ms = system_time_to_ms(t);
                let mut v = bh.version;
                while v != 0 {
                    let (data_root, ts, parent) = read_hist(&pager, meta, v)?;
                    if ts <= ms {
                        return Ok(snapshot_of(&pager, v, data_root));
                    }
                    v = parent;
                }
                Ok(genesis_snapshot(&pager))
            }
        }
    }

    /// Compacta el archivo según `retention`, manteniendo la clave de cifrado
    /// actual (M9): reescribe a un temporal y publica con rename atómico.
    pub fn vacuum(&self, retention: Retention) -> Result<VacuumReport> {
        self.run_vacuum(retention, Rekey::Keep)
    }

    /// Como [`vacuum`](Store::vacuum) pero además rota la clave de cifrado a
    /// `new_key` (`None` = desactivar cifrado) (M9, D6).
    pub fn vacuum_rekey(
        &self,
        retention: Retention,
        new_key: Option<&Key>,
    ) -> Result<VacuumReport> {
        self.run_vacuum(retention, Rekey::To(new_key))
    }

    fn run_vacuum(&self, retention: Retention, rekey: Rekey<'_>) -> Result<VacuumReport> {
        // `vacuum` es un escritor: `Busy` si hay una tx (u otro vacuum) en curso.
        if !self.writer.try_acquire() {
            return Err(Error::Busy);
        }
        let result = self.vacuum_locked(retention, rekey);
        self.writer.release();
        result
    }

    /// Reescribe el archivo compactado en un temporal y lo publica con un rename
    /// atómico (M9). Un fallo (o un kill) en cualquier punto antes del rename
    /// deja el archivo original intacto: solo queda un temporal huérfano, que el
    /// próximo `vacuum` borra. Tras el rename, sustituye el par `(pager, head)`
    /// de forma atómica: las lecturas nuevas ven el archivo compactado; los
    /// snapshots ya en vuelo conservan su `Arc<Pager>` (el inodo viejo, ya sin
    /// nombre) hasta soltarse.
    fn vacuum_locked(&self, retention: Retention, rekey: Rekey<'_>) -> Result<VacuumReport> {
        let (old_pager, old_head) = {
            let st = self.lock();
            (st.pager.clone(), st.head.clone())
        };
        let pages_before = old_head.n_pages;

        // v1: vacuum linealiza la historia (solo conserva `main`). Con otras ramas
        // vivas, eso cambiaría en silencio lo que ve `main` (el head global puede
        // ser un commit de otra rama). Mejor negarse y pedir merge/drop antes.
        if old_head.version != 0 && !has_only_main_branch(&old_pager, &old_head)? {
            return Err(Error::InvalidInput(
                "vacuum requiere una sola rama: fusiona o borra las demás antes de compactar",
            ));
        }

        let temp = vacuum_temp_path(&self.path);
        let _ = std::fs::remove_file(&temp); // limpia un temporal de un vacuum abortado

        // Pager temporal con la cripto destino (misma clave (Keep) o nueva (To))
        // y la **misma** compresión y ECC que el original (M10).
        let ecc_nsym = old_pager.header().ecc_nsym;
        let new_pager = match rekey {
            Rekey::Keep => Pager::create_with_crypto(
                &temp,
                old_pager.is_encrypted(),
                old_pager.crypto(),
                old_pager.compressor(),
                ecc_nsym,
                self.cache_pages,
            )?,
            Rekey::To(key) => Pager::create_with_crypto(
                &temp,
                key.is_some(),
                provider_for(key),
                old_pager.compressor(),
                ecc_nsym,
                self.cache_pages,
            )?,
        };
        let new_pager = Arc::new(new_pager);

        // Continúa el contador de nonce del archivo viejo: con la **misma** clave,
        // reiniciarlo a 0 reutilizaría pares (clave, nonce) ya usados en el
        // archivo original — catastrófico en AES-GCM (D6). Inofensivo sin cifrado
        // (el proveedor lo ignora) y sobrado con clave nueva.
        new_pager.set_nonce_counter(old_pager.nonce_counter());

        let build = build_compacted(&old_pager, &old_head, &new_pager, retention);
        let (new_head, kept_from) = match build {
            Ok(v) => v,
            Err(e) => {
                drop(new_pager); // suelta el fd y el lock del temporal
                let _ = std::fs::remove_file(&temp);
                return Err(e);
            }
        };

        // Publicación atómica: rename (reemplaza el original) + fsync del dir.
        if let Err(e) =
            std::fs::rename(&temp, &self.path).and_then(|()| sync_parent_dir(&self.path))
        {
            drop(new_pager);
            let _ = std::fs::remove_file(&temp);
            return Err(Error::Io(e));
        }
        let pages_after = new_head.n_pages;

        // Intercambia pager, head y coordinador de sello juntos, bajo un único lock.
        // El `sync_group` se renueva con el pager: un commit en vuelo en el pager
        // viejo sigue sellando su propio archivo con su propio coordinador.
        {
            let mut st = self.lock();
            st.pager = new_pager;
            st.sync_group = Arc::new(SyncGroup::new(new_head.clone()));
            st.head = new_head;
        }

        Ok(VacuumReport {
            kept_from,
            head: old_head.version,
            reclaimed_versions: kept_from.saturating_sub(1),
            pages_before,
            pages_after,
        })
    }
}

/// Destino de clave de un `vacuum` (M9): conservar la actual o rotar a otra.
enum Rekey<'a> {
    Keep,
    To(Option<&'a Key>),
}

/// Ruta del archivo temporal de `vacuum`, en el **mismo** directorio que el
/// original (un rename entre directorios distintos no sería atómico).
fn vacuum_temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".vacuum-tmp");
    path.with_file_name(name)
}

/// Frontera de retención K (versión más antigua conservada), en `[1, head]`. El
/// llamado garantiza `head ≥ 1`.
fn vacuum_frontier(old_pager: &Arc<Pager>, old_head: &Head, retention: Retention) -> Result<u64> {
    let head = old_head.version;
    let k = match retention {
        Retention::KeepAll => 1,
        // Últimas `n`: K = head-n+1, acotado a [1, head]. KeepLast(0|1) ⇒ K=head.
        Retention::KeepLast(n) => head.saturating_sub(n.saturating_sub(1)).max(1),
        // Primera versión con timestamp ≥ el dado; si ninguna, solo el head.
        Retention::KeepSince(t) => {
            let ms = system_time_to_ms(t);
            let mut k = head;
            for v in 1..=head {
                if read_hist(old_pager, old_head.meta_root, v)?.1 >= ms {
                    k = v;
                    break;
                }
            }
            k
        }
    };
    Ok(k)
}

/// Construye el archivo compactado en `new_pager` a partir de `old_pager`: un
/// **checkpoint** que materializa el estado completo en la frontera K, y luego
/// el **replay** de cada delta K+1..=head reusando `btree::diff` (O(cambios)) y
/// la misma maquinaria de commit. Devuelve `(head_nuevo, K)`.
///
/// El replay reproduce con fidelidad el estado de datos de cada versión retenida
/// (su `AS OF` sigue exacto) pero **linealiza** la historia: solo conserva la
/// ref `main` (apuntando al head) y un `parent_version` lineal. Para conservar
/// ramas, fusiónalas (o bórralas) antes de compactar.
fn build_compacted(
    old_pager: &Arc<Pager>,
    old_head: &Head,
    new_pager: &Arc<Pager>,
    retention: Retention,
) -> Result<(Head, u64)> {
    // Base vacía: el temporal ya es génesis; nada que materializar.
    if old_head.version == 0 {
        return Ok((commit::genesis_head(&new_pager.header().file_id), 0));
    }

    let old_meta = old_head.meta_root;
    let k = vacuum_frontier(old_pager, old_head, retention)?;

    // 1. Checkpoint en versión K: vuelca el árbol de datos de K en uno nuevo
    //    compacto. Su cadena arranca del eslabón cero del archivo nuevo, pero
    //    numera desde K (lo entiende `commit::verify`).
    let mut ts = TxStore::new(new_pager.clone());
    let snap_k = snapshot_at_version(old_pager, old_head, k)?;
    let mut data_root = btree::NO_ROOT;
    for item in snap_k.scan()? {
        let (key, val) = item?;
        data_root = btree::insert(&mut ts, data_root, &key, &val)?;
    }
    let ts_k = read_hist(old_pager, old_meta, k)?.1;
    let meta_root =
        write_meta_indices(&mut ts, btree::NO_ROOT, k, data_root, ts_k, MAIN_BRANCH, 0)?;
    let checkpoint = CommitParams {
        version: k,
        flags: COMMIT_FLAG_CHECKPOINT,
        prev_page: 0,
        prev_chain: commit::genesis_chain(&new_pager.header().file_id),
        branch: MAIN_BRANCH,
        parent_page: 0,
    };
    let mut base = publish_commit(new_pager, &mut ts, checkpoint, data_root, meta_root, ts_k)?;

    // 2. Replay de los deltas K+1..=head: el diff entre raíces consecutivas del
    //    archivo viejo, aplicado sobre el árbol nuevo (que ya contiene v-1).
    let old_src = PagerSource(old_pager.clone());
    for v in (k + 1)..=old_head.version {
        let prev_root = read_data_root(old_pager, old_meta, v - 1)?;
        let cur_root = read_data_root(old_pager, old_meta, v)?;
        let diffs = btree::diff(&old_src, prev_root, cur_root)?;

        let mut ts = TxStore::new(new_pager.clone());
        let mut data_root = base.data_root;
        for d in &diffs {
            data_root = match &d.change {
                btree::KeyChange::Added(val) | btree::KeyChange::Modified(_, val) => {
                    btree::insert(&mut ts, data_root, &d.key, val)?
                }
                btree::KeyChange::Removed(_) => btree::delete(&mut ts, data_root, &d.key)?.0,
            };
        }
        let ts_v = read_hist(old_pager, old_meta, v)?.1;
        let meta_root = write_meta_indices(
            &mut ts,
            base.meta_root,
            v,
            data_root,
            ts_v,
            MAIN_BRANCH,
            v - 1,
        )?;
        base = publish_commit(
            new_pager,
            &mut ts,
            CommitParams::after(&base, MAIN_BRANCH, base.commit_page),
            data_root,
            meta_root,
            ts_v,
        )?;
    }
    Ok((base, k))
}

// --- resolución de versiones e índices del árbol meta (libres de `self`) ---
//
// Toman el `pager` por parámetro en vez de leerlo de un `Store`: el llamador
// captura el par `(pager, head)` bajo un único lock y lo pasa, de modo que
// `vacuum` (M9) jamás puede sustituir el pager a media operación de lectura.

/// Resuelve la cabeza de una rama: su `data_root` y versión. `main` siempre
/// existe (antes del primer commit es génesis); otra rama inexistente da
/// `BranchNotFound`.
fn resolve_branch_head(pager: &Arc<Pager>, global: &Head, branch: &str) -> Result<BranchHead> {
    match read_ref(pager, global.meta_root, branch)? {
        Some(version) => {
            // En la punta global (rama tip; el caso común de cada query sobre
            // main) el `data_root` y el `commit_page` ya están en el head global:
            // se evita el 2º paseo de meta (`read_data_root`). Para versiones más
            // viejas u otras ramas sí hay que leerlo, y el commit_page es
            // informativo (no lo usa la lógica).
            let (data_root, commit_page) = if version == global.version {
                (global.data_root, global.commit_page)
            } else {
                (read_data_root(pager, global.meta_root, version)?, 0)
            };
            Ok(BranchHead {
                version,
                data_root,
                commit_page,
            })
        }
        None if branch == MAIN_BRANCH => Ok(BranchHead {
            version: 0,
            data_root: commit::genesis_head(&pager.header().file_id).data_root,
            commit_page: 0,
        }),
        None => Err(Error::BranchNotFound(branch.to_owned())),
    }
}

/// `true` si la única rama del árbol meta es `main` (precondición de `vacuum`,
/// M9). Recorre el espacio de refs; cualquier nombre distinto de `main` ⇒ falso.
fn has_only_main_branch(pager: &Arc<Pager>, head: &Head) -> Result<bool> {
    let src = PagerSource(pager.clone());
    for item in btree::scan_from(&src, head.meta_root, &[META_REF])? {
        let (key, _) = item?;
        if key.first() != Some(&META_REF) {
            break; // fuera del espacio de refs
        }
        if &key[1..] != MAIN_BRANCH.as_bytes() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Versión a la que apunta `ref[branch]` en un árbol meta, si existe.
fn read_ref(pager: &Arc<Pager>, meta_root: PageId, branch: &str) -> Result<Option<u64>> {
    let src = PagerSource(pager.clone());
    match btree::get(&src, meta_root, &ref_key(branch))? {
        Some(raw) => {
            let b: [u8; 8] = raw
                .get(0..8)
                .ok_or(Error::CorruptRecord("ref de rama truncada"))?
                .try_into()
                .expect("rango fijo de 8 bytes");
            Ok(Some(u64::from_le_bytes(b)))
        }
        None => Ok(None),
    }
}

/// `data_root` de una versión vía índice histórico (versión 0 = génesis).
fn read_data_root(pager: &Arc<Pager>, meta_root: PageId, version: u64) -> Result<PageId> {
    if version == 0 {
        return Ok(commit::genesis_head(&pager.header().file_id).data_root);
    }
    Ok(read_hist(pager, meta_root, version)?.0)
}

/// Entrada histórica completa de una versión: `(data_root, timestamp_ms,
/// parent_version)`. Las entradas pre-M8 (16 B) traen `parent_version = 0`.
fn read_hist(pager: &Arc<Pager>, meta_root: PageId, version: u64) -> Result<(PageId, u64, u64)> {
    let src = PagerSource(pager.clone());
    let raw = btree::get(&src, meta_root, &hist_key(version))?
        .ok_or(Error::VersionNotFound(AsOf::Version(version)))?;
    let u64_at = |off: usize| -> Result<u64> {
        Ok(u64::from_le_bytes(
            raw.get(off..off + 8)
                .ok_or(Error::CorruptRecord("entrada histórica truncada"))?
                .try_into()
                .expect("rango fijo de 8 bytes"),
        ))
    };
    let data_root = PageId(u64_at(0)?);
    let ts = u64_at(8)?;
    let parent = u64_at(16).unwrap_or(0); // pre-M8 (16 B): enraizada en génesis
    Ok((data_root, ts, parent))
}

/// Versión padre (en la rama) de una versión, vía índice histórico (M8).
fn read_parent_version(pager: &Arc<Pager>, meta_root: PageId, version: u64) -> Result<u64> {
    if version == 0 {
        return Ok(0);
    }
    Ok(read_hist(pager, meta_root, version)?.2)
}

/// `true` si `target` está en la ascendencia de `from` (caminando los padres
/// registrados): es el mismo commit o un antepasado por la línea de datos. Génesis
/// (0) es ancestro de todo. Acota `AS OF` a la historia de una rama.
fn is_ancestor(pager: &Arc<Pager>, meta_root: PageId, from: u64, target: u64) -> Result<bool> {
    let mut v = from;
    while v != 0 {
        if v == target {
            return Ok(true);
        }
        v = read_parent_version(pager, meta_root, v)?;
    }
    Ok(target == 0)
}

/// Ancestro común (merge base) de dos versiones, caminando `parent_version`
/// (M8). Génesis (0) es ancestro de todo: la búsqueda siempre termina.
fn merge_base(pager: &Arc<Pager>, meta_root: PageId, vf: u64, vi: u64) -> Result<u64> {
    let mut ancestors = std::collections::HashSet::new();
    let mut v = vf;
    loop {
        ancestors.insert(v);
        if v == 0 {
            break;
        }
        v = read_parent_version(pager, meta_root, v)?;
    }
    let mut v = vi;
    loop {
        if ancestors.contains(&v) {
            return Ok(v);
        }
        if v == 0 {
            return Ok(0);
        }
        v = read_parent_version(pager, meta_root, v)?;
    }
}

fn snapshot_of(pager: &Arc<Pager>, version: u64, data_root: PageId) -> Snapshot {
    Snapshot {
        pager: pager.clone(),
        version,
        data_root,
    }
}

/// Estado tras 0 commits: árbol de datos vacío, ligado a la identidad del archivo.
fn genesis_snapshot(pager: &Arc<Pager>) -> Snapshot {
    let data_root = commit::genesis_head(&pager.header().file_id).data_root;
    snapshot_of(pager, 0, data_root)
}

fn snapshot_at(pager: &Arc<Pager>, head: &Head, at: AsOf) -> Result<Snapshot> {
    match at {
        AsOf::Head => Ok(snapshot_of(pager, head.version, head.data_root)),
        AsOf::Version(v) => snapshot_at_version(pager, head, v),
        AsOf::Timestamp(t) => snapshot_at_timestamp(pager, head, system_time_to_ms(t)),
    }
}

fn snapshot_at_version(pager: &Arc<Pager>, head: &Head, v: u64) -> Result<Snapshot> {
    if v == head.version {
        return Ok(snapshot_of(pager, head.version, head.data_root));
    }
    if v > head.version {
        return Err(Error::VersionNotFound(AsOf::Version(v)));
    }
    if v == 0 {
        return Ok(genesis_snapshot(pager));
    }
    let data_root = read_data_root(pager, head.meta_root, v)?;
    Ok(snapshot_of(pager, v, data_root))
}

/// Mayor versión con timestamp ≤ `ms`: recorre el índice histórico `META_HIST`
/// (ordenado por versión ascendente) leyendo el `ts` de cada valor y se queda
/// con la mayor versión cuyo `ts ≤ ms` (no asume monotonía del reloj). Antes del
/// primer commit (o de la frontera de `vacuum`) ⇒ estado génesis. Coste
/// O(commits), aceptable en v1 (sin índice temporal aparte desde M9-perf).
fn snapshot_at_timestamp(pager: &Arc<Pager>, head: &Head, ms: u64) -> Result<Snapshot> {
    let src = PagerSource(pager.clone());
    let mut best: Option<u64> = None;
    for item in btree::scan_from(&src, head.meta_root, &[META_HIST])? {
        let (key, val) = item?;
        if key.first() != Some(&META_HIST) || key.len() != 1 + 8 {
            break; // fuera del espacio histórico
        }
        // hist_val = data_root(8) ‖ ts(8) ‖ parent(8).
        let ts = u64::from_le_bytes(
            val.get(8..16)
                .ok_or(Error::CorruptRecord("entrada histórica truncada"))?
                .try_into()
                .expect("rango fijo de 8 bytes"),
        );
        if ts <= ms {
            // Versiones ascendentes: la última con ts ≤ ms es la mayor.
            best = Some(u64::from_be_bytes(
                key[1..9].try_into().expect("rango fijo de 8 bytes"),
            ));
        }
    }
    match best {
        None => Ok(genesis_snapshot(pager)),
        Some(v) => snapshot_at_version(pager, head, v),
    }
}

// --- snapshot de lectura ---

#[derive(Clone)]
pub struct Snapshot {
    pager: Arc<Pager>,
    version: u64,
    data_root: PageId,
}

impl NodeSource for Snapshot {
    fn body(&self, id: PageId) -> Result<Body<'_>> {
        Ok(Body::Shared(self.pager.read_page(id)?))
    }
}

impl Snapshot {
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        btree::get(self, self.data_root, key)
    }

    pub fn scan(&self) -> Result<Cursor<'_, Snapshot>> {
        btree::scan(self, self.data_root)
    }

    pub fn scan_from(&self, start: &[u8]) -> Result<Cursor<'_, Snapshot>> {
        btree::scan_from(self, self.data_root, start)
    }

    // — capa relacional (M2) —

    pub fn table(&self, name: &str) -> Result<Option<TableDef>> {
        catalog::get_table(self, self.data_root, name)
    }

    pub fn get_row(&self, table: &TableDef, rowid: i64) -> Result<Option<Vec<Value>>> {
        catalog::get_row(self, self.data_root, table, rowid)
    }

    pub fn scan_table(&self, table: &TableDef) -> Result<TableScan<'_, Snapshot>> {
        catalog::scan_table(self, self.data_root, table)
    }

    /// Estado de scan **sin préstamo** de una tabla: el `Rows` en streaming de
    /// la API lo posee junto a este snapshot y avanza pasándole el snapshot.
    pub fn table_scan_state(&self, table: &TableDef) -> Result<catalog::ScanState> {
        catalog::ScanState::start(self, self.data_root, table)
    }

    /// Todas las tablas visibles en este snapshot (introspección de esquema).
    pub fn tables(&self) -> Result<Vec<TableDef>> {
        catalog::list_tables(self, self.data_root)
    }

    /// El SELECT (texto) de una vista, o `None`.
    pub fn view(&self, name: &str) -> Result<Option<String>> {
        catalog::get_view(self, self.data_root, name)
    }

    /// Todas las vistas `(nombre, SELECT)` (introspección).
    pub fn views(&self) -> Result<Vec<(String, String)>> {
        catalog::list_views(self, self.data_root)
    }

    /// Todos los triggers (introspección).
    pub fn triggers(&self) -> Result<Vec<catalog::TriggerDef>> {
        catalog::list_triggers(self, self.data_root)
    }

    /// rowids cuyas columnas indexadas valen `values` (igualdad, una entrada por
    /// columna del índice), vía el índice.
    pub fn index_lookup(&self, idx: &IndexDef, values: &[Value]) -> Result<Vec<i64>> {
        catalog::index_scan_eq(self, self.data_root, idx, values)
    }

    /// rowids de un rango sobre un índice de una columna (`lo`/`hi` inclusivos o
    /// no), vía el índice ordenado.
    pub fn index_range(
        &self,
        idx: &IndexDef,
        lo: Option<(&Value, bool)>,
        hi: Option<(&Value, bool)>,
    ) -> Result<Vec<i64>> {
        catalog::index_scan_range(self, self.data_root, idx, lo, hi)
    }

    /// rowids que casan una consulta `MATCH` vía el índice full-text.
    pub fn fts_search(
        &self,
        def: &TableDef,
        fts: &FtsIndexDef,
        query: &crate::fts::Query,
    ) -> Result<Vec<i64>> {
        catalog::fts_search(self, self.data_root, def, fts, query)
    }

    /// Stats BM25 de una consulta: `df` por término + globales `(N, Σ tokens)`.
    pub fn fts_stats(&self, fts_id: u32, terms: &[String]) -> Result<(Vec<u64>, u64, u64)> {
        catalog::fts_query_stats(self, self.data_root, fts_id, terms)
    }
}

// --- estado de páginas de una transacción ---

/// Páginas sucias de la tx + acceso a las durables del pager. Los ids sucios
/// son definitivos (≥ EOF lógico): el commit no necesita reubicar punteros.
/// (Público porque aparece en la firma de `WriteTx::scan`; no es API estable.)
pub struct TxStore {
    pager: Arc<Pager>,
    dirty: HashMap<PageId, PageBuf>,
    /// Ids sucios liberados, reutilizables en esta tx (evita huecos).
    freed: Vec<PageId>,
    alloc_base: u64,
    alloc_next: u64,
    /// Cursor de append a la hoja rightmost del árbol de datos (M10-perf): hace
    /// O(1) los inserts secuenciales. Lo gestiona `btree::insert`; se invalida
    /// solo (raíz/clave) y se reestablece desde el `tail` del insert completo.
    append_cursor: Option<btree::AppendCursor>,
}

impl TxStore {
    fn new(pager: Arc<Pager>) -> TxStore {
        let base = pager.n_pages();
        TxStore {
            pager,
            dirty: HashMap::new(),
            freed: Vec::new(),
            alloc_base: base,
            alloc_next: base,
            append_cursor: None,
        }
    }
}

impl NodeSource for TxStore {
    fn body(&self, id: PageId) -> Result<Body<'_>> {
        if let Some(p) = self.dirty.get(&id) {
            return Ok(Body::Local(p.body()));
        }
        if id.0 >= self.alloc_base {
            return Err(Error::Corrupt {
                page: id.0,
                reason: "página transitoria liberada",
            });
        }
        Ok(Body::Shared(self.pager.read_page(id)?))
    }
}

impl NodeStore for TxStore {
    fn alloc(&mut self) -> Result<PageId> {
        let id = self.freed.pop().unwrap_or_else(|| {
            let id = PageId(self.alloc_next);
            self.alloc_next += 1;
            id
        });
        self.dirty.insert(id, PageBuf::zeroed());
        Ok(id)
    }

    fn make_dirty(&mut self, id: PageId) -> Result<PageId> {
        if self.dirty.contains_key(&id) {
            return Ok(id);
        }
        let src = self.pager.read_page(id)?;
        let new = self.alloc()?;
        self.dirty.insert(new, (*src).clone());
        Ok(new)
    }

    fn body_mut(&mut self, id: PageId) -> &mut [u8] {
        self.dirty
            .get_mut(&id)
            .expect("body_mut sobre página no sucia")
            .body_mut()
    }

    fn free(&mut self, id: PageId) {
        if self.dirty.remove(&id).is_some() {
            self.freed.push(id);
        }
    }

    fn is_dirty(&self, id: PageId) -> bool {
        self.dirty.contains_key(&id)
    }

    fn take_append_cursor(&mut self) -> Option<btree::AppendCursor> {
        self.append_cursor.take()
    }

    fn set_append_cursor(&mut self, cursor: Option<btree::AppendCursor>) {
        self.append_cursor = cursor;
    }
}

// --- transacción de escritura ---

/// Índice `UNIQUE` del padre que cubre exactamente (como conjunto) las columnas
/// referenciadas por una FK no-PK. `create_table` ya garantiza que existe.
fn covering_unique_index<'a>(parent: &'a TableDef, parent_cols: &[usize]) -> Option<&'a IndexDef> {
    parent.indexes.iter().find(|ix| {
        ix.unique
            && ix.columns.len() == parent_cols.len()
            && parent_cols.iter().all(|p| ix.columns.contains(p))
    })
}

/// Clave de búsqueda para `index_scan_eq`: los valores hijos en el ORDEN de las
/// columnas del índice del padre. `fk.columns[k]` referencia a `fk.parent_columns[k]`.
fn fk_lookup_key(fk: &catalog::ForeignKey, idx: &IndexDef, child_row: &[Value]) -> Vec<Value> {
    idx.columns
        .iter()
        .map(|ic| {
            let k = fk
                .parent_columns
                .iter()
                .position(|p| p == ic)
                .expect("el índice cubre las columnas de la FK");
            child_row[fk.columns[k]].clone()
        })
        .collect()
}

/// Soltarla sin `commit` es un rollback: el estado en memoria se descarta y
/// el archivo no se ha tocado más allá de su EOF lógico.
pub struct WriteTx {
    pager: Arc<Pager>,
    /// Estado compartido del almacén: el commit publica el head nuevo aquí. El
    /// escritor único (este `writer`) impide que `vacuum` sustituya el pager
    /// mientras la tx vive, así que `pager` y `state.pager` coinciden al commit.
    state: Arc<Mutex<DbState>>,
    writer: Arc<WriterGate>,
    /// Coordinador de group commit de la generación de pager en que empezó esta tx
    /// (ver [`SyncGroup`]). El commit suelta el escritor tras escribir y sella aquí,
    /// agrupando su fsync con otros committers concurrentes.
    sync_group: Arc<SyncGroup>,
    /// `true` si `commit` ya soltó el escritor único (tras la write-phase, antes del
    /// fsync agrupado): el `Drop` no debe volver a soltarlo (otro committer podría
    /// tenerlo ya). En rollback queda `false` y lo suelta el `Drop`.
    writer_released: bool,
    /// Head **global** anterior (versión, `prev_page`/`prev_chain`, `meta_root`).
    base: Head,
    /// Rama sobre la que se publica (M8). `data_root`/`parent_*` son de esta rama.
    branch: String,
    /// Página de commit cabeza de la rama (informativa: ver `write_meta_indices`).
    parent_page: u64,
    /// Versión cabeza de la rama antes de esta tx (ascendencia para merge).
    parent_version: u64,
    data_root: PageId,
    meta_root: PageId,
    ts: TxStore,
    /// Contadores de rowid pendientes por `table_id` (M9-perf): se leen del árbol
    /// la primera vez, se incrementan en memoria y se vuelcan **una vez** en el
    /// commit. Evita reescribir la hoja del contador en cada `insert_row`. El
    /// resultado en disco es idéntico (solo persiste el valor final).
    rowid_cache: HashMap<u32, i64>,
    /// Buffers reutilizados del camino caliente de `insert_row` (M10-perf): la
    /// fila validada y su codificación, para no asignar un `Vec` por fila.
    rec_buf: Vec<Value>,
    enc_buf: Vec<u8>,
    /// Buffer de la fila evaluada por el executor (`INSERT`), prestado vía
    /// `take_values_buf`/`put_values_buf`: tampoco asigna un `Vec` por fila
    /// (M10-perf, fase 2).
    values_buf: Vec<Value>,
    /// Caché del esquema por nombre de tabla dentro de la tx: `get_table` desciende
    /// el catálogo y decodifica el `TableDef` en CADA sentencia (un INSERT por fila
    /// en un lote lo paga N veces). El esquema solo cambia con DDL (insert/update/
    /// delete de filas no lo tocan), así que se cachea y se **vacía al hacer DDL**.
    /// `Arc`: entregar el def cacheado es un bump de refcount, no un clone profundo
    /// de columnas e índices por sentencia (M10-perf, fase 2).
    schema_cache: HashMap<String, Arc<TableDef>>,
    /// Caché de **todos** los triggers de la tx (raros y pequeños). `list_triggers`
    /// escanea el catálogo; sin caché, un INSERT/UPDATE/DELETE-por-fila en un lote lo
    /// paga 2× por sentencia **aunque no haya ningún trigger**. Se llena perezosamente
    /// y se vacía solo cuando el catálogo de triggers cambia (crear/borrar trigger o
    /// revertir a un savepoint); insertar/actualizar/borrar filas NO lo invalida.
    /// `Arc`: filtrar la lista cacheada es un bump de refcount, no un re-escaneo.
    trigger_cache: Option<Arc<Vec<catalog::TriggerDef>>>,
    /// Profundidad de triggers en curso (un trigger puede disparar otra escritura
    /// que dispare más triggers): guarda contra recursión infinita.
    trigger_depth: usize,
    /// Pila de `SAVEPOINT`s: nombre + `data_root` + contadores de rowid en ese
    /// punto. Como el árbol es CoW (append-only), restaurar `data_root` revierte
    /// al subárbol anterior sin tocar el archivo (las páginas posteriores quedan
    /// inalcanzables; el vacuum las recupera).
    savepoints: Vec<Savepoint>,
}

/// Punto de retorno dentro de una transacción (`SAVEPOINT`). Captura el estado
/// del `TxStore` (las páginas sucias, que el b-tree CoW puede mutar **en sitio**
/// dentro de la tx, así que no basta con guardar `data_root`) y los contadores.
struct Savepoint {
    name: String,
    data_root: PageId,
    rowid_cache: HashMap<u32, i64>,
    dirty: HashMap<PageId, PageBuf>,
    freed: Vec<PageId>,
    alloc_next: u64,
}

impl Drop for WriteTx {
    fn drop(&mut self) {
        // Libera el escritor único en rollback (y en commit fallido antes de
        // soltarlo). El commit lo suelta él mismo tras la write-phase para no
        // retener el escritor durante el fsync agrupado; entonces `writer_released`
        // está puesto y aquí NO se vuelve a soltar (otro committer podría tenerlo).
        if !self.writer_released {
            self.writer.release();
        }
    }
}

impl WriteTx {
    /// Lee viendo las escrituras propias de la transacción.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        btree::get(&self.ts, self.data_root, key)
    }

    pub fn scan(&self) -> Result<Cursor<'_, TxStore>> {
        btree::scan(&self.ts, self.data_root)
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.data_root = btree::insert(&mut self.ts, self.data_root, key, value)?;
        Ok(())
    }

    /// `true` si la clave existía.
    pub fn delete(&mut self, key: &[u8]) -> Result<bool> {
        let (root, existed) = btree::delete(&mut self.ts, self.data_root, key)?;
        self.data_root = root;
        Ok(existed)
    }

    // — capa relacional (M2): la tx ve su propio DDL y sus propias filas —

    pub fn create_table(&mut self, spec: &TableSpec) -> Result<TableDef> {
        self.schema_cache.clear();
        let (root, def) = catalog::create_table(&mut self.ts, self.data_root, spec)?;
        self.data_root = root;
        Ok(def)
    }

    /// Crea una vista (`CREATE VIEW`): guarda su SELECT como texto.
    pub fn create_view(&mut self, name: &str, select_sql: &str) -> Result<()> {
        self.data_root = catalog::create_view(&mut self.ts, self.data_root, name, select_sql)?;
        Ok(())
    }

    /// Borra una vista. `false` si no existía.
    pub fn drop_view(&mut self, name: &str) -> Result<bool> {
        let (root, dropped) = catalog::drop_view(&mut self.ts, self.data_root, name)?;
        self.data_root = root;
        Ok(dropped)
    }

    /// El SELECT (texto) de una vista visible en la tx, o `None`.
    pub fn view(&self, name: &str) -> Result<Option<String>> {
        catalog::get_view(&self.ts, self.data_root, name)
    }

    /// Crea un trigger.
    pub fn create_trigger(&mut self, t: &catalog::TriggerDef) -> Result<()> {
        self.data_root = catalog::create_trigger(&mut self.ts, self.data_root, t)?;
        self.trigger_cache = None; // cambió el catálogo de triggers
        Ok(())
    }

    /// Borra un trigger. `false` si no existía.
    pub fn drop_trigger(&mut self, name: &str) -> Result<bool> {
        let (root, dropped) = catalog::drop_trigger(&mut self.ts, self.data_root, name)?;
        self.data_root = root;
        self.trigger_cache = None; // cambió el catálogo de triggers
        Ok(dropped)
    }

    pub fn trigger(&self, name: &str) -> Result<Option<catalog::TriggerDef>> {
        catalog::get_trigger(&self.ts, self.data_root, name)
    }

    /// Todos los triggers de la tx, cacheados. La primera llamada escanea el catálogo;
    /// las siguientes devuelven el `Arc` (un bump de refcount). Se invalida al cambiar
    /// el catálogo de triggers (crear/borrar/rollback a savepoint).
    fn all_triggers(&mut self) -> Result<Arc<Vec<catalog::TriggerDef>>> {
        if let Some(t) = &self.trigger_cache {
            return Ok(t.clone());
        }
        let all = Arc::new(catalog::list_triggers(&self.ts, self.data_root)?);
        self.trigger_cache = Some(all.clone());
        Ok(all)
    }

    /// Triggers que casan con `(tabla, evento, momento)`, para disparar. Filtra la
    /// lista cacheada; el caso común (sin triggers) ni escanea el catálogo ni asigna.
    pub fn triggers_for(
        &mut self,
        table: &str,
        event: catalog::TriggerEvent,
        timing: catalog::TriggerTiming,
    ) -> Result<Vec<catalog::TriggerDef>> {
        let all = self.all_triggers()?;
        if all.is_empty() {
            return Ok(Vec::new());
        }
        Ok(all
            .iter()
            .filter(|t| t.table == table && t.event == event && t.timing == timing)
            .cloned()
            .collect())
    }

    /// Entra en el cuerpo de un trigger (sube la profundidad y la acota).
    pub fn enter_trigger(&mut self) -> Result<()> {
        const MAX: usize = 32;
        if self.trigger_depth >= MAX {
            return Err(Error::Constraint(
                "recursión de triggers demasiado profunda",
            ));
        }
        self.trigger_depth += 1;
        Ok(())
    }

    pub fn exit_trigger(&mut self) {
        self.trigger_depth -= 1;
    }

    /// `SAVEPOINT nombre`: captura el estado actual como punto de retorno.
    pub fn savepoint(&mut self, name: &str) {
        self.savepoints.push(Savepoint {
            name: name.to_string(),
            data_root: self.data_root,
            rowid_cache: self.rowid_cache.clone(),
            dirty: self.ts.dirty.clone(),
            freed: self.ts.freed.clone(),
            alloc_next: self.ts.alloc_next,
        });
    }

    /// `ROLLBACK TO nombre`: revierte al savepoint (que **sigue activo**) y
    /// descarta los savepoints internos. Falla si el nombre no existe.
    pub fn rollback_to_savepoint(&mut self, name: &str) -> Result<()> {
        let pos = self
            .savepoints
            .iter()
            .rposition(|s| s.name == name)
            .ok_or(Error::InvalidInput("savepoint desconocido"))?;
        let sp = &self.savepoints[pos];
        let (data_root, rowid_cache, dirty, freed, alloc_next) = (
            sp.data_root,
            sp.rowid_cache.clone(),
            sp.dirty.clone(),
            sp.freed.clone(),
            sp.alloc_next,
        );
        self.data_root = data_root;
        self.rowid_cache = rowid_cache;
        self.ts.dirty = dirty;
        self.ts.freed = freed;
        self.ts.alloc_next = alloc_next;
        self.ts.append_cursor = None; // el cursor de append pudo quedar obsoleto
        self.schema_cache.clear(); // el esquema pudo cambiar desde el savepoint
        self.trigger_cache = None; // pudieron crearse/borrarse triggers desde el savepoint
        self.savepoints.truncate(pos + 1); // conserva el savepoint, quita los internos
        Ok(())
    }

    /// `RELEASE nombre`: descarta el savepoint y los internos (sus cambios quedan
    /// para el COMMIT o el rollback de un ámbito exterior). Falla si no existe.
    pub fn release_savepoint(&mut self, name: &str) -> Result<()> {
        let pos = self
            .savepoints
            .iter()
            .rposition(|s| s.name == name)
            .ok_or(Error::InvalidInput("savepoint desconocido"))?;
        self.savepoints.truncate(pos);
        Ok(())
    }

    /// Crea un índice secundario sobre `columns` (posiciones) de `table` (M10.5).
    pub fn create_index(
        &mut self,
        table: &str,
        name: &str,
        columns: &[usize],
        unique: bool,
    ) -> Result<()> {
        self.schema_cache.clear();
        self.data_root =
            catalog::create_index(&mut self.ts, self.data_root, table, name, columns, unique)?;
        Ok(())
    }

    /// Borra un índice por su nombre global. `false` si no existía.
    pub fn drop_index(&mut self, name: &str) -> Result<bool> {
        self.schema_cache.clear();
        let (root, dropped) = catalog::drop_index(&mut self.ts, self.data_root, name)?;
        self.data_root = root;
        Ok(dropped)
    }

    /// `true` si existe un índice con ese nombre (para `IF NOT EXISTS`).
    pub fn index_exists(&self, name: &str) -> Result<bool> {
        catalog::index_exists(&self.ts, self.data_root, name)
    }

    /// Crea un índice full-text sobre `columns` (TEXT) con el tokenizer dado y lo
    /// rellena con las filas existentes.
    pub fn create_fts_index(
        &mut self,
        table: &str,
        name: &str,
        columns: &[usize],
        tokenizer: &str,
    ) -> Result<()> {
        self.schema_cache.clear();
        self.data_root = catalog::create_fts_index(
            &mut self.ts,
            self.data_root,
            table,
            name,
            columns,
            tokenizer,
        )?;
        Ok(())
    }

    /// Borra un índice FTS por su nombre global. `false` si no existía.
    pub fn drop_fts_index(&mut self, name: &str) -> Result<bool> {
        self.schema_cache.clear();
        let (root, dropped) = catalog::drop_fts_index(&mut self.ts, self.data_root, name)?;
        self.data_root = root;
        Ok(dropped)
    }

    /// `true` si existe un índice FTS con ese nombre (para `IF NOT EXISTS`).
    pub fn fts_index_exists(&self, name: &str) -> Result<bool> {
        catalog::fts_index_exists(&self.ts, self.data_root, name)
    }

    /// rowids cuyas columnas indexadas valen `values` (igualdad, una entrada por
    /// columna del índice), vía el índice.
    pub fn index_lookup(&self, idx: &IndexDef, values: &[Value]) -> Result<Vec<i64>> {
        catalog::index_scan_eq(&self.ts, self.data_root, idx, values)
    }

    /// rowids de un rango sobre un índice de una columna, vía el índice ordenado.
    pub fn index_range(
        &self,
        idx: &IndexDef,
        lo: Option<(&Value, bool)>,
        hi: Option<(&Value, bool)>,
    ) -> Result<Vec<i64>> {
        catalog::index_scan_range(&self.ts, self.data_root, idx, lo, hi)
    }

    /// rowids que casan una consulta `MATCH` vía el índice full-text (sin full
    /// scan). El WHERE completo se re-aplica por fila después.
    pub fn fts_search(
        &self,
        def: &TableDef,
        fts: &FtsIndexDef,
        query: &crate::fts::Query,
    ) -> Result<Vec<i64>> {
        catalog::fts_search(&self.ts, self.data_root, def, fts, query)
    }

    /// Stats BM25 de una consulta: `df` por término + globales `(N, Σ tokens)`.
    pub fn fts_stats(&self, fts_id: u32, terms: &[String]) -> Result<(Vec<u64>, u64, u64)> {
        catalog::fts_query_stats(&self.ts, self.data_root, fts_id, terms)
    }

    pub fn drop_table(&mut self, name: &str) -> Result<bool> {
        self.schema_cache.clear();
        // Si la tabla tenía un contador en vuelo, descártalo: el commit no debe
        // recrearlo tras borrarla.
        if let Some(def) = catalog::get_table(&self.ts, self.data_root, name)? {
            self.rowid_cache.remove(&def.table_id);
        }
        let (root, dropped) = catalog::drop_table(&mut self.ts, self.data_root, name)?;
        self.data_root = root;
        Ok(dropped)
    }

    pub fn table(&self, name: &str) -> Result<Option<TableDef>> {
        catalog::get_table(&self.ts, self.data_root, name)
    }

    /// Como [`table`](Self::table) pero **cacheando** el `TableDef` por la duración
    /// de la tx: el camino de escritura caliente (un INSERT por fila en un lote)
    /// no vuelve a descender el catálogo ni a decodificar el esquema por fila. La
    /// caché se vacía en cada DDL (insert/update/delete de filas no tocan el
    /// esquema, así que no la invalidan).
    pub fn table_cached(&mut self, name: &str) -> Result<Option<Arc<TableDef>>> {
        if let Some(def) = self.schema_cache.get(name) {
            return Ok(Some(def.clone()));
        }
        match catalog::get_table(&self.ts, self.data_root, name)? {
            Some(def) => {
                let def = Arc::new(def);
                self.schema_cache.insert(name.to_owned(), def.clone());
                Ok(Some(def))
            }
            None => Ok(None),
        }
    }

    /// Presta el buffer de valores del INSERT del executor (devuélvelo con
    /// [`put_values_buf`](Self::put_values_buf)). Si un error lo pierde por el
    /// camino, el siguiente `take` simplemente empieza con un `Vec` nuevo.
    pub(crate) fn take_values_buf(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.values_buf)
    }

    pub(crate) fn put_values_buf(&mut self, buf: Vec<Value>) {
        self.values_buf = buf;
    }

    /// Añade una columna a una tabla existente (`ALTER TABLE ADD COLUMN`). No
    /// reescribe las filas existentes.
    pub fn add_column(&mut self, table: &str, col: catalog::ColumnDef) -> Result<()> {
        self.schema_cache.clear();
        let (root, _) = catalog::add_column(&mut self.ts, self.data_root, table, col)?;
        self.data_root = root;
        Ok(())
    }

    /// Reordena lógicamente una columna (`ALTER TABLE … MOVE COLUMN`). Solo cambia
    /// el orden de presentación en el catálogo; no reescribe filas.
    pub fn move_column(&mut self, table: &str, col: &str, pos: &catalog::ColumnPos) -> Result<()> {
        self.schema_cache.clear();
        let (root, _) = catalog::move_column(&mut self.ts, self.data_root, table, col, pos)?;
        self.data_root = root;
        Ok(())
    }

    /// Fija el orden lógico completo de columnas (`ALTER TABLE … REORDER COLUMNS`).
    pub fn reorder_columns(&mut self, table: &str, order: &[String]) -> Result<()> {
        self.schema_cache.clear();
        let (root, _) = catalog::reorder_columns(&mut self.ts, self.data_root, table, order)?;
        self.data_root = root;
        Ok(())
    }

    /// Renombra una columna (`ALTER TABLE … RENAME COLUMN`).
    pub fn rename_column(&mut self, table: &str, old: &str, new: &str) -> Result<()> {
        self.schema_cache.clear();
        let (root, _) = catalog::rename_column(&mut self.ts, self.data_root, table, old, new)?;
        self.data_root = root;
        Ok(())
    }

    /// DROP COLUMN lógico (tombstone).
    pub fn drop_column(&mut self, table: &str, col: &str) -> Result<()> {
        self.schema_cache.clear();
        let (root, _) = catalog::drop_column(&mut self.ts, self.data_root, table, col)?;
        self.data_root = root;
        Ok(())
    }

    /// Inserta y devuelve el rowid (automático o explícito vía columna alias).
    /// El contador de rowid se cachea en la tx y se vuelca en el commit, así que
    /// solo se escribe la hoja de la fila (no la del contador) por inserción.
    pub fn insert_row(&mut self, table: &TableDef, values: &[Value]) -> Result<i64> {
        self.fk_check_parents_exist(table, values)?;
        let explicit = catalog::explicit_rowid(table, values)?;
        let next = match self.rowid_cache.get(&table.table_id) {
            Some(&c) => c,
            None => catalog::read_counter(&self.ts, self.data_root, table.table_id)?,
        };
        let (rowid, new_next) =
            catalog::resolve_rowid(&self.ts, self.data_root, table.table_id, explicit, next)?;
        // Solo se persiste tras escribir la fila: un insert fallido no deja
        // contador a medias en la caché.
        self.data_root = catalog::put_row_buffered(
            &mut self.ts,
            self.data_root,
            table,
            rowid,
            values,
            &mut self.rec_buf,
            &mut self.enc_buf,
        )?;
        self.rowid_cache.insert(table.table_id, new_next);
        Ok(rowid)
    }

    /// Carga masiva (bulk-load): inserta todas las filas en una pasada. El
    /// esquema se resuelve **una vez**, el contador de rowid vive en un local
    /// (ni HashMap por fila) y las entradas de índice se **difieren**: se
    /// insertan ordenadas al final, con el dup-check UNIQUE intra-lote y
    /// contra lo existente antes de escribir ninguna.
    ///
    /// Contrato: pensado para tx recién abierta + commit inmediato (lo
    /// garantiza [`Connection::bulk_insert`](crate::Connection::bulk_insert)).
    /// Si falla a mitad, la tx puede quedar con filas sin sus entradas de
    /// índice — **descártala**, no la confirmes.
    pub(crate) fn insert_rows<I, R>(&mut self, table: &str, rows: I) -> Result<usize>
    where
        I: IntoIterator<Item = R>,
        R: AsRef<[Value]>,
    {
        let def = self
            .table_cached(table)?
            .ok_or(Error::InvalidInput("tabla desconocida"))?;
        let mut next = match self.rowid_cache.get(&def.table_id) {
            Some(&c) => c,
            None => catalog::read_counter(&self.ts, self.data_root, def.table_id)?,
        };
        let mut pending: Vec<Vec<(Vec<u8>, bool)>> = vec![Vec::new(); def.indexes.len()];
        let mut n = 0usize;
        for row in rows {
            let values = row.as_ref();
            let explicit = catalog::explicit_rowid(&def, values)?;
            let (rowid, new_next) =
                catalog::resolve_rowid(&self.ts, self.data_root, def.table_id, explicit, next)?;
            next = new_next;
            self.data_root = catalog::put_row_data(
                &mut self.ts,
                self.data_root,
                &def,
                rowid,
                values,
                &mut self.enc_buf,
            )?;
            for (idx, entries) in def.indexes.iter().zip(&mut pending) {
                entries.push(catalog::resolved_index_entry(&def, values, idx, rowid)?);
            }
            // El FTS no se difiere (la tokenización es por fila): se mantiene en
            // línea para que el bulk-load no deje el índice full-text a medias.
            // Materializa el registro (defaults incl.) para coincidir con el
            // camino normal.
            if !def.fts_indexes.is_empty() {
                self.data_root = catalog::insert_fts_entries_bulk(
                    &mut self.ts,
                    self.data_root,
                    &def,
                    rowid,
                    values,
                    &mut self.rec_buf,
                )?;
            }
            // Vectorial en línea también (asigna a cluster por fila). Un BLOB no se
            // coacciona, así que los valores crudos sirven (un DEFAULT en columna
            // vectorial es inexistente en la práctica).
            if !def.vector_indexes.is_empty() {
                self.data_root = catalog::insert_vector_entries(
                    &mut self.ts,
                    self.data_root,
                    &def,
                    rowid,
                    values,
                )?;
            }
            n += 1;
        }
        self.rowid_cache.insert(def.table_id, next);
        for (idx, mut entries) in def.indexes.iter().zip(pending) {
            self.data_root =
                catalog::flush_index_entries(&mut self.ts, self.data_root, idx, &mut entries)?;
        }
        Ok(n)
    }

    /// Sobrescribe una fila. `false` si el rowid no existe. Si `table` es padre de
    /// alguna FK y la actualización cambia las columnas referenciadas, se aplica
    /// `ON UPDATE` a las filas hijas: RESTRICT (comprobado **antes** de escribir),
    /// y CASCADE / SET NULL **después** (para que el padre ya tenga el valor nuevo
    /// cuando el hijo revalide su propia FK).
    pub fn update_row(&mut self, table: &TableDef, rowid: i64, values: &[Value]) -> Result<bool> {
        self.fk_check_parents_exist(table, values)?;
        let old = self.get_row(table, rowid)?;
        if let Some(old) = &old {
            self.fk_handle_parent_update(table, rowid, old, values, true)?; // RESTRICT pre-escritura
        }
        let (root, ok) = catalog::update_row(&mut self.ts, self.data_root, table, rowid, values)?;
        self.data_root = root;
        if ok && let Some(old) = &old {
            self.fk_handle_parent_update(table, rowid, old, values, false)?; // cascada post-escritura
        }
        Ok(ok)
    }

    pub fn get_row(&self, table: &TableDef, rowid: i64) -> Result<Option<Vec<Value>>> {
        catalog::get_row(&self.ts, self.data_root, table, rowid)
    }

    pub fn delete_row(&mut self, table: &TableDef, rowid: i64) -> Result<bool> {
        // FKs: RESTRICT (falla si hay hijos), CASCADE (los borra) o SET NULL, ANTES
        // de borrar el padre.
        self.fk_handle_parent_delete(table, rowid)?;
        let (root, existed) = catalog::delete_row(&mut self.ts, self.data_root, table, rowid)?;
        self.data_root = root;
        Ok(existed)
    }

    /// INSERT/UPDATE: por cada FK de `table`, su(s) valor(es) (si ninguno es NULL)
    /// deben existir en el padre. Referencia por PK → `get_row` por rowid;
    /// referencia a columnas con índice UNIQUE → búsqueda por ese índice.
    fn fk_check_parents_exist(&self, table: &TableDef, values: &[Value]) -> Result<()> {
        for fk in &table.foreign_keys {
            // Valores hijos; si alguno es NULL no se comprueba (MATCH SIMPLE).
            let mut child_vals = Vec::with_capacity(fk.columns.len());
            let mut any_null = false;
            for &c in &fk.columns {
                match values.get(c) {
                    Some(Value::Null) | None => {
                        any_null = true;
                        break;
                    }
                    Some(v) => child_vals.push(v.clone()),
                }
            }
            if any_null {
                continue;
            }
            let parent = self
                .table(&fk.parent)?
                .ok_or(Error::Constraint("tabla padre de FK desconocida"))?;
            let exists = if fk.parent_columns.is_empty() {
                // Referencia la PK: el valor debe ser INTEGER y existir por rowid.
                match &child_vals[0] {
                    Value::Integer(n) => self.get_row(&parent, *n)?.is_some(),
                    _ => {
                        return Err(Error::Constraint("una columna FK a la PK debe ser INTEGER"));
                    }
                }
            } else {
                let idx = covering_unique_index(&parent, &fk.parent_columns).ok_or(
                    Error::Constraint("la FK referencia columnas sin índice UNIQUE"),
                )?;
                !self
                    .index_lookup(idx, &fk_lookup_key(fk, idx, values))?
                    .is_empty()
            };
            if !exists {
                return Err(Error::Constraint(
                    "violación de clave foránea: la fila padre no existe",
                ));
            }
        }
        Ok(())
    }

    /// Tablas (def hija, fk) con una FK que referencia a `parent_name`.
    fn fk_referencing(&self, parent_name: &str) -> Result<Vec<(TableDef, catalog::ForeignKey)>> {
        let mut out = Vec::new();
        for t in catalog::list_tables(&self.ts, self.data_root)? {
            for fk in &t.foreign_keys {
                if fk.parent == parent_name {
                    out.push((t.clone(), fk.clone()));
                }
            }
        }
        Ok(out)
    }

    /// Filas hijas `(rowid, valores)` de `child` cuyas columnas `fk.columns` igualan
    /// `ref_vals` (todas). Si algún valor de referencia es NULL, no hay match.
    fn fk_children_matching(
        &self,
        child: &TableDef,
        fk: &catalog::ForeignKey,
        ref_vals: &[Value],
    ) -> Result<Vec<(i64, Vec<Value>)>> {
        if ref_vals.iter().any(|v| matches!(v, Value::Null)) {
            return Ok(Vec::new());
        }
        self.scan_table(child)?
            .filter_map(|r| match r {
                Ok((id, vals)) => {
                    let m = fk
                        .columns
                        .iter()
                        .zip(ref_vals)
                        .all(|(&c, rv)| vals.get(c) == Some(rv));
                    m.then_some(Ok((id, vals)))
                }
                Err(e) => Some(Err(e)),
            })
            .collect()
    }

    /// DELETE del padre: aplica `ON DELETE` de cada FK que apunte a `parent` para
    /// las filas hijas que referencian la fila borrada.
    fn fk_handle_parent_delete(&mut self, parent: &TableDef, rowid: i64) -> Result<()> {
        let parent_row = self.get_row(parent, rowid)?;
        for (child, fk) in self.fk_referencing(&parent.name)? {
            // Valores referenciados de la fila padre (por rowid o por columnas).
            let ref_vals: Vec<Value> = if fk.parent_columns.is_empty() {
                vec![Value::Integer(rowid)]
            } else {
                match &parent_row {
                    Some(pr) => fk.parent_columns.iter().map(|&p| pr[p].clone()).collect(),
                    None => continue,
                }
            };
            let hits = self.fk_children_matching(&child, &fk, &ref_vals)?;
            if hits.is_empty() {
                continue;
            }
            match fk.on_delete {
                catalog::FkAction::Restrict => {
                    return Err(Error::Constraint(
                        "violación de clave foránea: hay filas hijas (ON DELETE RESTRICT)",
                    ));
                }
                catalog::FkAction::Cascade => {
                    for (id, _) in hits {
                        self.delete_row(&child, id)?; // recursivo: re-comprueba las FKs del hijo
                    }
                }
                catalog::FkAction::SetNull => {
                    for (id, mut vals) in hits {
                        for &c in &fk.columns {
                            vals[c] = Value::Null;
                        }
                        self.update_row(&child, id, &vals)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// UPDATE del padre: si la actualización cambia las columnas referenciadas por
    /// alguna FK, aplica `ON UPDATE`. Con `restrict_only` solo comprueba RESTRICT
    /// (antes de escribir el padre); si no, aplica CASCADE / SET NULL (después).
    fn fk_handle_parent_update(
        &mut self,
        parent: &TableDef,
        rowid: i64,
        old: &[Value],
        new: &[Value],
        restrict_only: bool,
    ) -> Result<()> {
        for (child, fk) in self.fk_referencing(&parent.name)? {
            let refs = |row: &[Value]| -> Vec<Value> {
                if fk.parent_columns.is_empty() {
                    vec![Value::Integer(rowid)] // la PK (rowid) no cambia en un UPDATE
                } else {
                    fk.parent_columns.iter().map(|&p| row[p].clone()).collect()
                }
            };
            let old_ref = refs(old);
            let new_ref = refs(new);
            if old_ref == new_ref {
                continue; // las columnas referenciadas no cambian
            }
            let hits = self.fk_children_matching(&child, &fk, &old_ref)?;
            if hits.is_empty() {
                continue;
            }
            match fk.on_update {
                catalog::FkAction::Restrict => {
                    return Err(Error::Constraint(
                        "violación de clave foránea: hay filas hijas (ON UPDATE RESTRICT)",
                    ));
                }
                _ if restrict_only => continue, // las cascadas se aplican tras escribir el padre
                catalog::FkAction::Cascade => {
                    for (id, mut vals) in hits {
                        for (k, &c) in fk.columns.iter().enumerate() {
                            vals[c] = new_ref[k].clone();
                        }
                        self.update_row(&child, id, &vals)?;
                    }
                }
                catalog::FkAction::SetNull => {
                    for (id, mut vals) in hits {
                        for &c in &fk.columns {
                            vals[c] = Value::Null;
                        }
                        self.update_row(&child, id, &vals)?;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn scan_table(&self, table: &TableDef) -> Result<TableScan<'_, TxStore>> {
        catalog::scan_table(&self.ts, self.data_root, table)
    }

    /// Publica la transacción en su rama. Devuelve la versión nueva (o la
    /// cabeza de la rama si la tx no tocó nada). Versiones monótonas globales;
    /// el commit lleva `parent_page` de la rama y `prev_page` global (D3).
    pub fn commit(mut self) -> Result<u64> {
        if self.ts.dirty.is_empty() && self.data_root == self.base.data_root {
            return Ok(self.parent_version);
        }
        // Volcar los contadores de rowid cacheados al árbol (una sola escritura
        // por tabla tocada, en vez de una por fila).
        let counters: Vec<(u32, i64)> = self.rowid_cache.iter().map(|(&k, &v)| (k, v)).collect();
        for (table_id, next) in counters {
            self.data_root = catalog::write_counter(&mut self.ts, self.data_root, table_id, next)?;
        }
        let version = self.base.version + 1;
        let timestamp_ms = now_ms();

        // Índices del árbol meta global (hist, ts, ref[rama]).
        let meta_root = write_meta_indices(
            &mut self.ts,
            self.meta_root,
            version,
            self.data_root,
            timestamp_ms,
            &self.branch,
            self.parent_version,
        )?;

        // --- write-phase (con el escritor único retenido) ---
        // Escribe las páginas sucias + la de commit y avanza el estado en memoria,
        // SIN sellar todavía. El fsync se hace fuera del lock (group commit).
        let head = write_commit_pages(
            &self.pager,
            &mut self.ts,
            CommitParams::after(&self.base, &self.branch, self.parent_page),
            self.data_root,
            meta_root,
            timestamp_ms,
        )?;
        // Publica el head (base del siguiente committer) y la marca de agua de
        // «escrito», ambos mientras aún se tiene el escritor (serializado).
        self.state.lock().expect("estado del store envenenado").head = head.clone();
        self.sync_group.mark_written(head.clone());

        // Suelta el escritor ANTES de sellar: así otro committer puede escribir sus
        // páginas mientras nuestro fsync está en vuelo, y un único fsync los cubre a
        // todos (group commit). El `Drop` ya no lo soltará.
        self.writer.release();
        self.writer_released = true;

        // --- durability-phase (escritor ya libre) ---
        // Sella agrupando con los fsync concurrentes. Un commit solo se vuelve líder
        // y sella al instante; bajo carga, comparte el fsync con los que se encolaron.
        self.sync_group.make_durable(&self.pager, head.version)?;
        Ok(version)
    }
}

/// Parámetros de cabecera de un commit que no se derivan de los datos. Para un
/// commit normal salen del head global anterior ([`CommitParams::after`]); el
/// checkpoint de `vacuum` (M9) los fija a mano (versión K, flag checkpoint,
/// cadena sembrada en génesis).
struct CommitParams<'a> {
    version: u64,
    flags: u32,
    /// Commit anterior en la cadena global (0 = génesis).
    prev_page: u64,
    /// `chain_hash` del commit anterior (génesis para el checkpoint).
    prev_chain: [u8; 32],
    branch: &'a str,
    /// Commit padre en la ascendencia de datos de la rama (informativo).
    parent_page: u64,
}

impl<'a> CommitParams<'a> {
    /// Commit normal que sucede a `base` (el head global): versión+1 y encadena
    /// tras él. `parent_page` es la cabeza de datos de la rama destino.
    fn after(base: &Head, branch: &'a str, parent_page: u64) -> CommitParams<'a> {
        CommitParams {
            version: base.version + 1,
            flags: 0,
            prev_page: base.commit_page,
            prev_chain: base.chain_hash,
            branch,
            parent_page,
        }
    }
}

/// Escribe los índices del árbol meta global para un commit (hist versión →
/// `data_root` ‖ ts ‖ `parent_version`, índice temporal, y `ref[branch]` →
/// versión) y devuelve la nueva raíz meta. `parent_version` es la cabeza de la
/// rama antes de este commit: la ascendencia para merge se rastrea por versión
/// (M8), no por `parent_page` (que la página de commit no puede conocer al
/// escribirse).
fn write_meta_indices(
    ts: &mut TxStore,
    meta_root: PageId,
    version: u64,
    data_root: PageId,
    timestamp_ms: u64,
    branch: &str,
    parent_version: u64,
) -> Result<PageId> {
    // `hist[version] → data_root ‖ ts ‖ parent_version` y `ref[branch] →
    // versión`. El timestamp vive en el valor de `hist`: `AS OF TIMESTAMP` lo
    // resuelve escaneando `hist` (M9-perf), así que no hace falta un índice
    // temporal aparte —una escritura de b-tree menos por commit—.
    let mut hist_val = Vec::with_capacity(24);
    hist_val.extend_from_slice(&data_root.0.to_le_bytes());
    hist_val.extend_from_slice(&timestamp_ms.to_le_bytes());
    hist_val.extend_from_slice(&parent_version.to_le_bytes());
    let mut root = btree::insert(ts, meta_root, &hist_key(version), &hist_val)?;
    root = btree::insert(ts, root, &ref_key(branch), &version.to_le_bytes())?;
    Ok(root)
}

/// Escribe las páginas sucias de la tx, la página de commit y publica el nuevo
/// head (un solo fsync: el punto de durabilidad, M1/M9-perf). Los campos de cabecera no
/// derivados de los datos (versión, flags, `prev_page`/`prev_chain`) vienen en
/// `params`: un commit normal los toma del head anterior; el checkpoint de
/// `vacuum` (M9) los fija explícitamente.
fn publish_commit(
    pager: &Pager,
    ts: &mut TxStore,
    params: CommitParams<'_>,
    data_root: PageId,
    meta_root: PageId,
    timestamp_ms: u64,
) -> Result<Head> {
    let head = write_commit_pages(pager, ts, params, data_root, meta_root, timestamp_ms)?;
    // **Único** fsync del commit (M9-perf): páginas de datos y de commit escritas,
    // una sola barrera. Basta para la durabilidad porque la recuperación no
    // confía en el orden de flush del SO: el tag de integridad por página hace
    // **ilegible** cualquier página escrita a medias o no escrita, y el escaneo
    // hacia delante (que lee en orden y las páginas de datos van antes que la de
    // commit) se detiene en la primera ilegible. Así, un commit a medio escribir
    // jamás se adopta; solo se adopta uno cuyas páginas son **todas** legibles
    // (i.e., el fsync llegó a completarse). Camino síncrono: lo usan ramas/merge/
    // `vacuum`; `WriteTx::commit` sella fuera del lock vía group commit ([`SyncGroup`]).
    pager.sync()?;
    pager.write_meta(&commit::meta_for(&head))?;
    Ok(head)
}

/// Escribe las páginas sucias de la tx y la de commit (append, en orden de id),
/// instala el directorio/`write_offset`/`next_page` en memoria y devuelve el head
/// — **sin sellar**. El fsync lo hace el llamador: `publish_commit` (síncrono) o el
/// group commit de [`WriteTx::commit`] (fuera del lock del escritor). Avanzar el
/// estado en memoria antes del fsync es seguro: al reabrir, el directorio se
/// reconstruye **escaneando el archivo** y `recover` solo adopta commits cuyas
/// páginas son TODAS legibles — un commit a medio sellar nunca se adopta.
fn write_commit_pages(
    pager: &Pager,
    ts: &mut TxStore,
    params: CommitParams<'_>,
    data_root: PageId,
    meta_root: PageId,
    timestamp_ms: u64,
) -> Result<Head> {
    let commit_page = PageId(ts.alloc_next);

    // Páginas sucias como registros enmarcados, en orden de id (v2, M10);
    // content_hash cubre los bodies lógicos en claro (BODY_SIZE, sin recortar).
    let mut cursor = pager.write_offset();
    let mut entries = Vec::new();
    let mut hasher = Sha256::new();
    let mut pages_written = 0u64;
    for pid in ts.alloc_base..ts.alloc_next {
        let id = PageId(pid);
        let page = ts.dirty.remove(&id).unwrap_or_else(PageBuf::zeroed);
        hasher.update(page.body());
        let (rec_len, loc) = pager.write_record_at(id, page.body(), cursor)?;
        cursor += rec_len;
        entries.push((id, loc));
        pager.cache_insert(id, Arc::new(page));
        pages_written += 1;
    }

    let mut header = CommitHeader {
        flags: params.flags,
        version: params.version,
        parent_page: params.parent_page,
        prev_page: params.prev_page,
        timestamp_ms,
        data_root,
        meta_root,
        nonce_counter: pager.nonce_counter() + 1, // +1: el sellado de esta página
        pages_written: pages_written + 1,
        branch: params.branch.to_owned(),
        content_hash: hasher.finalize().into(),
        prev_chain: params.prev_chain,
        chain_hash: [0; 32],
    };
    header.chain_hash = header.compute_chain();
    let mut page = PageBuf::zeroed();
    header.encode_into(page.body_mut());
    let (commit_rec_len, commit_loc) = pager.write_record_at(commit_page, page.body(), cursor)?;
    cursor += commit_rec_len;
    entries.push((commit_page, commit_loc));
    pager.cache_insert(commit_page, Arc::new(page));

    let head = Head {
        version: header.version,
        commit_page: commit_page.0,
        data_root,
        meta_root,
        chain_hash: header.chain_hash,
        nonce_counter: header.nonce_counter,
        n_pages: commit_page.0 + 1,
    };
    // Publica el directorio, write_offset y next_page en un solo paso atómico (aún
    // en memoria; la durabilidad la da el fsync que hará el llamador).
    pager.install_commit(&entries, cursor, head.n_pages);
    Ok(head)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Estado final al que un cambio lleva una clave: `Some(valor)` o `None` (borrada).
fn target_of(change: &btree::KeyChange) -> Option<Vec<u8>> {
    match change {
        btree::KeyChange::Added(v) | btree::KeyChange::Modified(_, v) => Some(v.clone()),
        btree::KeyChange::Removed(_) => None,
    }
}

/// `true` si la clave es el contador de rowid del catálogo (`[0x00, 0x02, …]`),
/// que en merge se reconcilia por máximo en vez de conflicto.
fn is_counter_key(key: &[u8]) -> bool {
    key.len() >= 2 && key[0] == 0x00 && key[1] == 0x02
}

/// El mayor de dos contadores `next_rowid` (i64 LE, docs/02): evita colisiones
/// de rowid al fusionar dos ramas que insertaron filas.
fn max_counter(a: &[u8], b: &[u8]) -> Vec<u8> {
    if decode_i64_le(a) >= decode_i64_le(b) {
        a.to_vec()
    } else {
        b.to_vec()
    }
}

fn decode_i64_le(b: &[u8]) -> i64 {
    let mut buf = [0u8; 8];
    let n = b.len().min(8);
    buf[..n].copy_from_slice(&b[..n]);
    i64::from_le_bytes(buf)
}

fn system_time_to_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

fn ms_to_system_time(ms: u64) -> SystemTime {
    UNIX_EPOCH + std::time::Duration::from_millis(ms)
}

/// Lector de páginas inmutables del pager: recorre el árbol meta (índice
/// histórico) sin montar un [`Snapshot`] de datos.
struct PagerSource(Arc<Pager>);

impl NodeSource for PagerSource {
    fn body(&self, id: PageId) -> Result<Body<'_>> {
        Ok(Body::Shared(self.0.read_page(id)?))
    }
}

fn hist_key(version: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(9);
    k.push(META_HIST);
    k.extend_from_slice(&version.to_be_bytes());
    k
}

fn ref_key(branch: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + branch.len());
    k.push(META_REF);
    k.extend_from_slice(branch.as_bytes());
    k
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tx_commits_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::create(&dir.path().join("t.arkeion")).unwrap();
        let pages_before = store.pager().n_pages();
        let tx = store.begin().unwrap();
        assert_eq!(tx.commit().unwrap(), 0);
        assert_eq!(store.version(), 0);
        assert_eq!(store.pager().n_pages(), pages_before);
    }

    #[test]
    fn busy_while_tx_open() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::create(&dir.path().join("t.arkeion")).unwrap();
        let tx = store.begin().unwrap();
        assert!(matches!(store.begin(), Err(Error::Busy)));
        drop(tx);
        store.begin().unwrap();
    }

    #[test]
    fn tx_sees_own_writes_snapshot_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::create(&dir.path().join("t.arkeion")).unwrap();
        let snap = store.snapshot();

        let mut tx = store.begin().unwrap();
        tx.put(b"k", b"v").unwrap();
        assert_eq!(tx.get(b"k").unwrap().unwrap(), b"v");
        assert_eq!(snap.get(b"k").unwrap(), None);
        tx.commit().unwrap();

        // El snapshot viejo sigue viendo el pasado; uno nuevo ve el presente.
        assert_eq!(snap.get(b"k").unwrap(), None);
        assert_eq!(store.snapshot().get(b"k").unwrap().unwrap(), b"v");
    }

    /// M5: tras K commits, `AS OF VERSION i` reproduce *exactamente* el estado
    /// i para todo i (0 = vacío). Versión futura ⇒ `VersionNotFound`.
    #[test]
    fn snapshot_at_version_reproduces_every_past_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::create(&dir.path().join("t.arkeion")).unwrap();

        // Estado de referencia por versión. Claves con padding para que el
        // orden de inserción coincida con el de scan (lexicográfico).
        let k = 12u64;
        let mut expected: Vec<Vec<(Vec<u8>, Vec<u8>)>> = vec![vec![]];
        for i in 1..=k {
            let key = format!("k{i:02}").into_bytes();
            let val = format!("v{i:02}").into_bytes();
            let mut tx = store.begin().unwrap();
            tx.put(&key, &val).unwrap();
            assert_eq!(tx.commit().unwrap(), i);
            let mut state = expected[(i - 1) as usize].clone();
            state.push((key, val));
            expected.push(state);
        }

        for i in 0..=k {
            let snap = store.snapshot_at(AsOf::Version(i)).unwrap();
            assert_eq!(snap.version(), i);
            let got: Vec<(Vec<u8>, Vec<u8>)> = snap.scan().unwrap().collect::<Result<_>>().unwrap();
            assert_eq!(got, expected[i as usize], "estado de la versión {i}");
        }

        assert!(matches!(
            store.snapshot_at(AsOf::Version(k + 1)),
            Err(Error::VersionNotFound(AsOf::Version(_)))
        ));
        // `Head` siempre apunta al estado vivo.
        assert_eq!(store.snapshot_at(AsOf::Head).unwrap().version(), k);
    }

    /// M5: `AS OF TIMESTAMP` resuelve a la mayor versión con ts ≤ t, con las
    /// fronteras (antes del primero, entre dos, después del último) correctas.
    #[test]
    fn snapshot_at_timestamp_resolves_floor_and_boundaries() {
        use std::thread::sleep;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let store = Store::create(&dir.path().join("t.arkeion")).unwrap();

        // Separación de 2 ms para garantizar timestamps distintos por commit.
        let before = SystemTime::now();
        sleep(Duration::from_millis(2));

        let mut tx = store.begin().unwrap();
        tx.put(b"a", b"1").unwrap();
        assert_eq!(tx.commit().unwrap(), 1);

        sleep(Duration::from_millis(2));
        let between = SystemTime::now();
        sleep(Duration::from_millis(2));

        let mut tx = store.begin().unwrap();
        tx.put(b"b", b"2").unwrap();
        assert_eq!(tx.commit().unwrap(), 2);

        sleep(Duration::from_millis(2));
        let after = SystemTime::now();

        // Antes del primer commit: estado génesis (vacío).
        let s0 = store.snapshot_at(AsOf::Timestamp(before)).unwrap();
        assert_eq!(s0.version(), 0);
        assert_eq!(s0.get(b"a").unwrap(), None);

        // Entre commit 1 y 2: ve `a`, no `b`.
        let s1 = store.snapshot_at(AsOf::Timestamp(between)).unwrap();
        assert_eq!(s1.version(), 1);
        assert_eq!(s1.get(b"a").unwrap().unwrap(), b"1");
        assert_eq!(s1.get(b"b").unwrap(), None);

        // Después del último: ve todo.
        let s2 = store.snapshot_at(AsOf::Timestamp(after)).unwrap();
        assert_eq!(s2.version(), 2);
        assert_eq!(s2.get(b"b").unwrap().unwrap(), b"2");
    }

    /// M6: una cadena intacta se audita en verde, incl. génesis y tras reabrir.
    #[test]
    fn verify_accepts_a_clean_chain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.arkeion");
        let store = Store::create(&path).unwrap();

        // Génesis: cadena trivialmente válida (0 commits).
        let r = store.verify().unwrap();
        assert_eq!((r.head, r.commits, r.chain_ok), (0, 0, true));

        for i in 1..=8 {
            let mut tx = store.begin().unwrap();
            tx.put(format!("k{i:02}").as_bytes(), b"v").unwrap();
            assert_eq!(tx.commit().unwrap(), i);
        }
        let r = store.verify().unwrap();
        assert_eq!((r.head, r.commits, r.chain_ok), (8, 8, true));

        // La cadena es externamente verificable: reabrir y re-auditar.
        drop(store);
        let store = Store::open(&path).unwrap();
        assert!(store.verify().unwrap().chain_ok);
    }

    /// M7: el almacén cifrado hace round-trip y se audita; clave errónea ⇒
    /// `WrongKey`.
    #[test]
    fn encrypted_store_roundtrips_and_audits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e.arkeion");
        let key = Key::new([0x44; 32]);

        let store = Store::create_keyed(&path, Some(&key)).unwrap();
        for i in 1..=6 {
            let mut tx = store.begin().unwrap();
            tx.put(format!("k{i:02}").as_bytes(), b"valor").unwrap();
            assert_eq!(tx.commit().unwrap(), i);
        }
        assert!(store.verify().unwrap().chain_ok);
        drop(store);

        // Reabrir cifrado: datos y cadena intactos.
        let store = Store::open_keyed(&path, Some(&key)).unwrap();
        assert_eq!(store.snapshot().get(b"k03").unwrap().unwrap(), b"valor");
        assert!(store.verify().unwrap().chain_ok);
        drop(store);

        // Clave errónea ⇒ WrongKey.
        assert!(matches!(
            Store::open_keyed(&path, Some(&Key::new([0x99; 32]))),
            Err(Error::WrongKey)
        ));
    }

    /// M7 (D6, R7): tras una cola rota, el contador de nonces retoma pasado el
    /// margen físico, haciendo imposible reutilizar un nonce.
    #[test]
    fn nonce_counter_resumes_past_a_torn_tail() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("n.arkeion");
        let key = Key::new([0x55; 32]);

        let store = Store::create_keyed(&path, Some(&key)).unwrap();
        for i in 1..=3 {
            let mut tx = store.begin().unwrap();
            tx.put(format!("k{i}").as_bytes(), b"v").unwrap();
            tx.commit().unwrap();
        }
        let (head_pages, head_counter) = {
            let st = store.lock();
            (st.head.n_pages, st.head.nonce_counter)
        };
        drop(store);

        // Cola rota: `extra` páginas de basura tras el head (commit abortado).
        let extra = 5u64;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(&vec![0u8; extra as usize * crate::format::PAGE_SIZE])
            .unwrap();
        f.sync_all().unwrap();
        drop(f);

        let store = Store::open_keyed(&path, Some(&key)).unwrap();
        assert_eq!(
            store.lock().head.n_pages,
            head_pages,
            "el head no debe moverse por la cola rota"
        );
        let counter = store.pager().nonce_counter();
        assert!(
            counter >= head_counter + extra,
            "el contador {counter} no superó el margen {}",
            head_counter + extra
        );
    }

    /// M6: voltear un byte de una página histórica ⇒ `ChainBroken`.
    #[test]
    fn verify_detects_a_tampered_historical_page() {
        use std::io::{Read, Seek, SeekFrom, Write};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.arkeion");
        let store = Store::create(&path).unwrap();
        for i in 1..=4 {
            let mut tx = store.begin().unwrap();
            tx.put(format!("k{i:02}").as_bytes(), b"valor").unwrap();
            tx.commit().unwrap();
        }
        drop(store); // cerrar el archivo antes de manipularlo

        // Un byte del body de la primera página de datos (página 3, commit 1).
        let off = crate::format::FIRST_DATA_PAGE.0 * crate::format::PAGE_SIZE as u64 + 100;
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        f.seek(SeekFrom::Start(off)).unwrap();
        let mut b = [0u8; 1];
        f.read_exact(&mut b).unwrap();
        b[0] ^= 0x01;
        f.seek(SeekFrom::Start(off)).unwrap();
        f.write_all(&b).unwrap();
        f.sync_all().unwrap();
        drop(f);

        let store = Store::open(&path).unwrap();
        let err = store.verify().unwrap_err();
        assert!(matches!(err, Error::ChainBroken { .. }), "fue {err:?}");
    }

    /// M8: dos ramas divergen de forma independiente; `branches` las lista y la
    /// auditoría sigue verde con la cadena global lineal.
    #[test]
    fn branches_diverge_independently() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::create(&dir.path().join("b.arkeion")).unwrap();

        let mut tx = store.begin().unwrap();
        tx.put(b"a", b"1").unwrap();
        tx.put(b"b", b"2").unwrap();
        tx.commit().unwrap(); // v1, main

        store.create_branch("feature", AsOf::Head).unwrap(); // v2

        let mut tx = store.begin_on("feature").unwrap();
        tx.put(b"c", b"3").unwrap();
        assert!(tx.delete(b"a").unwrap());
        tx.commit().unwrap(); // v3, feature

        let mut tx = store.begin().unwrap(); // main
        tx.put(b"d", b"4").unwrap();
        tx.commit().unwrap(); // v4, main

        // feature: a borrada; b, c; sin d.
        let feat = store.snapshot_on("feature").unwrap();
        assert_eq!(feat.get(b"a").unwrap(), None);
        assert_eq!(feat.get(b"b").unwrap().unwrap(), b"2");
        assert_eq!(feat.get(b"c").unwrap().unwrap(), b"3");
        assert_eq!(feat.get(b"d").unwrap(), None);

        // main: a, b, d; sin c.
        let main = store.snapshot_on("main").unwrap();
        assert_eq!(main.get(b"a").unwrap().unwrap(), b"1");
        assert_eq!(main.get(b"c").unwrap(), None);
        assert_eq!(main.get(b"d").unwrap().unwrap(), b"4");

        let mut names: Vec<String> = store
            .branches()
            .unwrap()
            .into_iter()
            .map(|b| b.name)
            .collect();
        names.sort();
        assert_eq!(names, vec!["feature".to_string(), "main".to_string()]);

        assert!(store.verify().unwrap().chain_ok);
    }

    /// M8: las ramas y sus datos sobreviven a reabrir el archivo.
    #[test]
    fn branches_persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.arkeion");
        {
            let store = Store::create(&path).unwrap();
            let mut tx = store.begin().unwrap();
            tx.put(b"main_key", b"m").unwrap();
            tx.commit().unwrap();
            store.create_branch("dev", AsOf::Head).unwrap();
            let mut tx = store.begin_on("dev").unwrap();
            tx.put(b"dev_key", b"d").unwrap();
            tx.commit().unwrap();
        }

        let store = Store::open(&path).unwrap();
        let dev = store.snapshot_on("dev").unwrap();
        assert_eq!(dev.get(b"dev_key").unwrap().unwrap(), b"d");
        assert_eq!(dev.get(b"main_key").unwrap().unwrap(), b"m");
        assert_eq!(
            store.snapshot_on("main").unwrap().get(b"dev_key").unwrap(),
            None
        );
        assert!(store.verify().unwrap().chain_ok);
    }

    /// M8: errores de gestión de ramas.
    #[test]
    fn branch_management_errors() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::create(&dir.path().join("b.arkeion")).unwrap();
        let mut tx = store.begin().unwrap();
        tx.put(b"k", b"v").unwrap();
        tx.commit().unwrap();

        assert!(matches!(
            store.begin_on("nope"),
            Err(Error::BranchNotFound(_))
        ));
        assert!(matches!(
            store.snapshot_on("nope"),
            Err(Error::BranchNotFound(_))
        ));
        assert!(matches!(
            store.drop_branch("nope"),
            Err(Error::BranchNotFound(_))
        ));

        store.create_branch("x", AsOf::Head).unwrap();
        assert!(matches!(
            store.create_branch("x", AsOf::Head),
            Err(Error::BranchExists(_))
        ));
        assert!(matches!(
            store.drop_branch("main"),
            Err(Error::InvalidInput(_))
        ));

        store.drop_branch("x").unwrap();
        assert!(matches!(store.begin_on("x"), Err(Error::BranchNotFound(_))));
    }

    /// M8: merge limpio (destino sin cambios) aplica exactamente el diff de la
    /// rama origen — el caso de la migración (branch → cambiar → merge).
    #[test]
    fn merge_clean_applies_exactly_the_diff() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::create(&dir.path().join("m.arkeion")).unwrap();

        let mut tx = store.begin().unwrap();
        tx.put(b"a", b"1").unwrap();
        tx.commit().unwrap(); // v1, main

        store.create_branch("feature", AsOf::Head).unwrap();
        let mut tx = store.begin_on("feature").unwrap();
        tx.put(b"b", b"2").unwrap(); // añade
        tx.put(b"a", b"9").unwrap(); // modifica
        tx.commit().unwrap(); // v3, feature (main intacta)

        let report = store
            .merge("feature", "main", MergePolicy::FailOnConflict)
            .unwrap();
        assert_eq!(report.applied, 2);

        let main = store.snapshot_on("main").unwrap();
        assert_eq!(main.get(b"a").unwrap().unwrap(), b"9");
        assert_eq!(main.get(b"b").unwrap().unwrap(), b"2");
        assert!(store.verify().unwrap().chain_ok);

        // Re-merge no aplica nada (ya está fusionado).
        assert_eq!(
            store
                .merge("feature", "main", MergePolicy::FailOnConflict)
                .unwrap()
                .applied,
            0
        );
    }

    /// M8: la misma clave cambiada distinto en ambas ramas ⇒ `Conflict` con el
    /// detalle de cada lado.
    #[test]
    fn merge_conflict_on_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::create(&dir.path().join("m.arkeion")).unwrap();

        let mut tx = store.begin().unwrap();
        tx.put(b"a", b"1").unwrap();
        tx.commit().unwrap(); // v1

        store.create_branch("dev", AsOf::Head).unwrap();
        let mut tx = store.begin_on("dev").unwrap();
        tx.put(b"a", b"DEV").unwrap();
        tx.commit().unwrap(); // v3

        let mut tx = store.begin().unwrap();
        tx.put(b"a", b"MAIN").unwrap();
        tx.commit().unwrap(); // v4 en main

        let err = store
            .merge("dev", "main", MergePolicy::FailOnConflict)
            .unwrap_err();
        match err {
            Error::Conflict(c) => {
                assert_eq!(c.len(), 1);
                assert_eq!(c[0].key, b"a");
                assert_eq!(c[0].from, Some(b"DEV".to_vec()));
                assert_eq!(c[0].into, Some(b"MAIN".to_vec()));
            }
            other => panic!("esperaba Conflict, fue {other:?}"),
        }
        // El merge fallido no tocó main.
        assert_eq!(
            store
                .snapshot_on("main")
                .unwrap()
                .get(b"a")
                .unwrap()
                .unwrap(),
            b"MAIN"
        );
    }

    /// M8: dos ramas que avanzan el contador de rowid se reconcilian por máximo
    /// (no es conflicto): merge limpio.
    #[test]
    fn merge_reconciles_rowid_counter_by_max() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::create(&dir.path().join("m.arkeion")).unwrap();

        let counter = [0x00u8, 0x02, 0, 0, 0, 1]; // [KS_CATALOG, CAT_COUNTER, table_id=1 BE]
        let mut tx = store.begin().unwrap();
        tx.put(&counter, &10i64.to_le_bytes()).unwrap();
        tx.commit().unwrap(); // v1, contador=10

        store.create_branch("dev", AsOf::Head).unwrap();
        let mut tx = store.begin_on("dev").unwrap();
        tx.put(&counter, &15i64.to_le_bytes()).unwrap();
        tx.commit().unwrap(); // v3, dev contador=15

        let mut tx = store.begin().unwrap();
        tx.put(&counter, &12i64.to_le_bytes()).unwrap();
        tx.commit().unwrap(); // v4, main contador=12

        let report = store
            .merge("dev", "main", MergePolicy::FailOnConflict)
            .unwrap();
        assert_eq!(report.applied, 1);
        assert_eq!(
            store
                .snapshot_on("main")
                .unwrap()
                .get(&counter)
                .unwrap()
                .unwrap(),
            15i64.to_le_bytes()
        );
    }

    // --- vacuum (M9) ---

    /// Crea un almacén con `n` versiones: la versión `v` deja `key_v = "k" →
    /// b"v{v}"` y reescribe una clave estable `b"head"` con el número de versión.
    /// Así cada versión tiene un estado de datos distinto y verificable.
    fn store_with_versions(path: &Path, n: u64) -> Store {
        let store = Store::create(path).unwrap();
        for v in 1..=n {
            let mut tx = store.begin().unwrap();
            tx.put(format!("k{v}").as_bytes(), format!("v{v}").as_bytes())
                .unwrap();
            tx.put(b"head", v.to_le_bytes().as_slice()).unwrap();
            tx.commit().unwrap();
        }
        store
    }

    /// Comprueba que `AS OF VERSION v` reproduce el estado exacto: `b"head" == v`
    /// y las claves `k1..=v` presentes, `k{v+1}..` ausentes.
    fn assert_state_at(store: &Store, v: u64, max: u64) {
        let snap = store.snapshot_at(AsOf::Version(v)).unwrap();
        assert_eq!(
            snap.get(b"head").unwrap().unwrap(),
            v.to_le_bytes(),
            "head en la versión {v}"
        );
        for j in 1..=max {
            let got = snap.get(format!("k{j}").as_bytes()).unwrap();
            if j <= v {
                assert_eq!(got.unwrap(), format!("v{j}").as_bytes(), "k{j} en v{v}");
            } else {
                assert!(got.is_none(), "k{j} no debería existir en v{v}");
            }
        }
    }

    #[test]
    fn vacuum_keeplast_retains_recent_and_compacts_old() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_versions(&dir.path().join("v.arkeion"), 10);
        let pages_before = store.pager().n_pages();

        let report = store.vacuum(Retention::KeepLast(3)).unwrap();
        assert_eq!(report.head, 10);
        assert_eq!(report.kept_from, 8); // 10 - 3 + 1
        assert_eq!(report.reclaimed_versions, 7);
        assert_eq!(report.pages_before, pages_before);
        assert!(
            report.pages_after < report.pages_before,
            "vacuum no compactó: {} → {}",
            report.pages_before,
            report.pages_after
        );

        // Cadena íntegra tras compactar, y solo cuenta los commits retenidos.
        let audit = store.verify().unwrap();
        assert!(audit.chain_ok);
        assert_eq!(audit.head, 10);
        assert_eq!(audit.commits, 3); // checkpoint(8) + delta(9) + delta(10)

        // Versiones retenidas: estado exacto. Compactadas: VersionNotFound.
        for v in 8..=10 {
            assert_state_at(&store, v, 10);
        }
        for v in 1..=7 {
            assert!(
                matches!(
                    store.snapshot_at(AsOf::Version(v)),
                    Err(Error::VersionNotFound(AsOf::Version(_)))
                ),
                "la versión {v} debería estar compactada"
            );
        }
        // El head sigue siendo el presente.
        assert_eq!(store.version(), 10);
        assert_eq!(
            store.snapshot().get(b"head").unwrap().unwrap(),
            10u64.to_le_bytes()
        );
    }

    #[test]
    fn vacuum_keepall_is_lossless_defrag() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_versions(&dir.path().join("v.arkeion"), 6);

        let report = store.vacuum(Retention::KeepAll).unwrap();
        assert_eq!(report.kept_from, 1);
        assert_eq!(report.reclaimed_versions, 0);

        assert!(store.verify().unwrap().chain_ok);
        for v in 1..=6 {
            assert_state_at(&store, v, 6);
        }
        // La versión 0 sigue siendo el estado vacío de génesis.
        assert!(
            store
                .snapshot_at(AsOf::Version(0))
                .unwrap()
                .get(b"head")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn vacuum_survives_reopen_with_data_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.arkeion");
        let store = store_with_versions(&path, 8);
        store.vacuum(Retention::KeepLast(4)).unwrap();
        drop(store);

        // Reabrir el archivo compactado: recover encuentra el head, verify OK.
        let store = Store::open(&path).unwrap();
        assert_eq!(store.version(), 8);
        assert!(store.verify().unwrap().chain_ok);
        for v in 5..=8 {
            assert_state_at(&store, v, 8);
        }
        assert!(store.snapshot_at(AsOf::Version(4)).is_err());

        // Y se puede seguir escribiendo encima.
        let mut tx = store.begin().unwrap();
        tx.put(b"after", b"vacuum").unwrap();
        assert_eq!(tx.commit().unwrap(), 9);
        assert!(store.verify().unwrap().chain_ok);
    }

    #[test]
    fn vacuum_keeplast_one_keeps_only_head() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_versions(&dir.path().join("v.arkeion"), 5);
        let report = store.vacuum(Retention::KeepLast(1)).unwrap();
        assert_eq!(report.kept_from, 5);
        assert_eq!(report.reclaimed_versions, 4);
        assert!(store.verify().unwrap().chain_ok);
        assert_state_at(&store, 5, 5);
        assert!(store.snapshot_at(AsOf::Version(4)).is_err());
    }

    #[test]
    fn vacuum_checkpoint_as_head_reopens_and_verifies() {
        // K == head: el archivo compactado tiene un único commit, el checkpoint.
        // recover (vía meta slot) y verify deben tratarlo como cabeza de cadena.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.arkeion");
        let store = store_with_versions(&path, 5);
        store.vacuum(Retention::KeepLast(1)).unwrap();
        drop(store);

        let store = Store::open(&path).unwrap();
        assert_eq!(store.version(), 5);
        let audit = store.verify().unwrap();
        assert!(audit.chain_ok);
        assert_eq!(audit.head, 5);
        assert_eq!(audit.commits, 1); // solo el checkpoint
        assert_state_at(&store, 5, 5);
    }

    #[test]
    fn vacuum_keepsince_extremes() {
        let dir = tempfile::tempdir().unwrap();

        // Timestamp 0: ninguna versión es anterior ⇒ se conservan todas (K=1).
        let store = store_with_versions(&dir.path().join("a.arkeion"), 5);
        let r = store.vacuum(Retention::KeepSince(UNIX_EPOCH)).unwrap();
        assert_eq!(r.kept_from, 1);
        assert_state_at(&store, 1, 5);

        // Timestamp muy futuro: todas son anteriores ⇒ solo el head (K=head).
        let store = store_with_versions(&dir.path().join("b.arkeion"), 5);
        let future = UNIX_EPOCH + std::time::Duration::from_secs(40_000_000_000);
        let r = store.vacuum(Retention::KeepSince(future)).unwrap();
        assert_eq!(r.kept_from, 5);
        assert!(store.snapshot_at(AsOf::Version(4)).is_err());
        assert_state_at(&store, 5, 5);
    }

    #[test]
    fn vacuum_on_empty_db_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::create(&dir.path().join("e.arkeion")).unwrap();
        let report = store.vacuum(Retention::KeepLast(3)).unwrap();
        assert_eq!(report.head, 0);
        assert_eq!(report.kept_from, 0);
        assert!(store.verify().unwrap().chain_ok);
        // Sigue usable.
        let mut tx = store.begin().unwrap();
        tx.put(b"k", b"v").unwrap();
        assert_eq!(tx.commit().unwrap(), 1);
    }

    #[test]
    fn vacuum_is_busy_during_a_write_tx() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_versions(&dir.path().join("v.arkeion"), 2);
        let tx = store.begin().unwrap();
        assert!(matches!(store.vacuum(Retention::KeepAll), Err(Error::Busy)));
        drop(tx);
        store.vacuum(Retention::KeepAll).unwrap();
    }

    #[test]
    fn snapshot_taken_before_vacuum_survives_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_versions(&dir.path().join("v.arkeion"), 6);

        // Fija una lectura en una versión que vacuum va a compactar.
        let pinned = store.snapshot_at(AsOf::Version(2)).unwrap();
        store.vacuum(Retention::KeepLast(2)).unwrap(); // conserva 5,6

        // El snapshot viejo sigue siendo dueño de su Arc<Pager> (inodo viejo):
        // lee su versión 2 aunque ya esté compactada en el archivo nuevo.
        assert_eq!(pinned.get(b"head").unwrap().unwrap(), 2u64.to_le_bytes());
        assert_eq!(pinned.get(b"k2").unwrap().unwrap(), b"v2");
        // Pero el almacén ya no la resuelve.
        assert!(store.snapshot_at(AsOf::Version(2)).is_err());
    }

    #[test]
    fn vacuum_keeps_key_and_continues_nonce_when_encrypted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("enc.arkeion");
        let key = Key::new([0x5A; 32]);

        let store = Store::create_keyed(&path, Some(&key)).unwrap();
        for v in 1..=6 {
            let mut tx = store.begin().unwrap();
            tx.put(format!("k{v}").as_bytes(), format!("v{v}").as_bytes())
                .unwrap();
            tx.commit().unwrap();
        }
        let nonce_before = store.pager().nonce_counter();

        store.vacuum(Retention::KeepLast(2)).unwrap();
        // El archivo nuevo continúa el contador de nonce (jamás reinicia a 0 con
        // la misma clave): su head queda por encima del contador previo.
        assert!(
            store.pager().nonce_counter() >= nonce_before,
            "el contador de nonce no continuó: {} < {nonce_before}",
            store.pager().nonce_counter()
        );
        assert!(store.verify().unwrap().chain_ok);
        drop(store);

        // Reabrir con la clave correcta: datos íntegros; sin clave, KeyRequired.
        assert!(matches!(Store::open(&path), Err(Error::KeyRequired)));
        let store = Store::open_keyed(&path, Some(&key)).unwrap();
        assert_eq!(store.snapshot().get(b"k6").unwrap().unwrap(), b"v6");
        assert!(store.verify().unwrap().chain_ok);
    }

    #[test]
    fn vacuum_rekey_rotates_the_encryption_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rk.arkeion");
        let old = Key::new([0x11; 32]);
        let new = Key::new([0x22; 32]);

        let store = Store::create_keyed(&path, Some(&old)).unwrap();
        let mut tx = store.begin().unwrap();
        tx.put(b"secret", b"value").unwrap();
        tx.commit().unwrap();

        store.vacuum_rekey(Retention::KeepAll, Some(&new)).unwrap();
        assert!(store.verify().unwrap().chain_ok);
        drop(store);

        // La clave vieja ya no abre; la nueva sí.
        assert!(matches!(
            Store::open_keyed(&path, Some(&old)),
            Err(Error::WrongKey)
        ));
        let store = Store::open_keyed(&path, Some(&new)).unwrap();
        assert_eq!(store.snapshot().get(b"secret").unwrap().unwrap(), b"value");
        assert!(store.verify().unwrap().chain_ok);
    }

    #[test]
    fn vacuum_rekey_can_remove_encryption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("de.arkeion");
        let key = Key::new([0x33; 32]);

        let store = Store::create_keyed(&path, Some(&key)).unwrap();
        let mut tx = store.begin().unwrap();
        tx.put(b"k", b"v").unwrap();
        tx.commit().unwrap();

        store.vacuum_rekey(Retention::KeepAll, None).unwrap();
        drop(store);

        // Ahora abre sin clave; pasar la vieja clave es un error de uso.
        let store = Store::open(&path).unwrap();
        assert_eq!(store.snapshot().get(b"k").unwrap().unwrap(), b"v");
        assert!(store.verify().unwrap().chain_ok);
        assert!(Store::open_keyed(&path, Some(&key)).is_err());
    }

    #[test]
    fn vacuum_refuses_when_other_branches_exist() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_versions(&dir.path().join("v.arkeion"), 3);
        store.create_branch("dev", AsOf::Head).unwrap();

        // Con `dev` viva, vacuum se niega (linealizaría la historia en silencio).
        assert!(matches!(
            store.vacuum(Retention::KeepAll),
            Err(Error::InvalidInput(_))
        ));

        // Tras borrar la rama, compacta sin problema.
        store.drop_branch("dev").unwrap();
        store.vacuum(Retention::KeepAll).unwrap();
        assert!(store.verify().unwrap().chain_ok);
    }

    #[test]
    fn vacuum_removes_a_stale_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.arkeion");
        let store = store_with_versions(&path, 3);

        // Simula un vacuum anterior abortado: un temporal huérfano.
        let temp = vacuum_temp_path(&path);
        std::fs::write(&temp, b"basura de un vacuum muerto").unwrap();

        store.vacuum(Retention::KeepAll).unwrap();
        assert!(store.verify().unwrap().chain_ok);
        assert!(
            !temp.exists(),
            "el temporal huérfano debería haberse borrado"
        );
    }

    /// El contador de rowid se cachea y se vuelca en el commit (M9-perf), pero el
    /// comportamiento observable es idéntico al de escribirlo por fila: secuencia
    /// dentro de la tx, persistencia entre commits y reabriendo, y reset en
    /// rollback.
    #[test]
    fn deferred_rowid_counter_is_observably_unchanged() {
        use crate::catalog::{ColType, ColumnSpec};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.arkeion");
        let store = Store::create(&path).unwrap();
        let spec = TableSpec {
            name: "t".into(),
            columns: vec![
                ColumnSpec {
                    name: "id".into(),
                    col_type: ColType::Integer,
                    not_null: false,
                    primary_key: true,
                    default: None,
                    references: None,
                    unique: false,
                    check: None,
                },
                ColumnSpec {
                    name: "n".into(),
                    col_type: ColType::Integer,
                    not_null: true,
                    primary_key: false,
                    default: None,
                    references: None,
                    unique: false,
                    check: None,
                },
            ],
            foreign_keys: Vec::new(),
            uniques: Vec::new(),
            checks: Vec::new(),
        };
        let def = {
            let mut tx = store.begin().unwrap();
            let d = tx.create_table(&spec).unwrap();
            tx.commit().unwrap();
            d
        };
        let auto = |n: i64| [Value::Null, Value::Integer(n)];

        // Secuencia dentro de una sola tx: 1,2,3.
        let mut tx = store.begin().unwrap();
        for (i, want) in (10..40).step_by(10).zip(1..=3) {
            assert_eq!(tx.insert_row(&def, &auto(i)).unwrap(), want);
        }
        tx.commit().unwrap();

        // Continúa entre commits (no reinicia): 4.
        let mut tx = store.begin().unwrap();
        assert_eq!(tx.insert_row(&def, &auto(40)).unwrap(), 4);
        tx.commit().unwrap();

        // Rollback: la tx avanza el contador en memoria y se suelta sin commit.
        {
            let mut tx = store.begin().unwrap();
            assert_eq!(tx.insert_row(&def, &auto(50)).unwrap(), 5);
            // soltar sin commit = rollback
        }
        // El contador durable no avanzó: el siguiente reusa el 5.
        let mut tx = store.begin().unwrap();
        assert_eq!(tx.insert_row(&def, &auto(55)).unwrap(), 5);
        tx.commit().unwrap();

        // El contador persistió en disco: tras reabrir, el siguiente es 6.
        drop(store);
        let store = Store::open(&path).unwrap();
        let mut tx = store.begin().unwrap();
        assert_eq!(tx.insert_row(&def, &auto(60)).unwrap(), 6);
        tx.commit().unwrap();
        assert!(store.verify().unwrap().chain_ok);
    }

    /// Seguridad del commit de un solo fsync (M9-perf): un commit a medio escribir
    /// (crash antes de que el fsync complete) no se adopta. Se simula el estado de
    /// crash: el slot meta del último commit no se actualizó (⇒ recover parte del
    /// anterior) y una de sus páginas de datos quedó sin escribir (⇒ ilegible por
    /// el tag). La recuperación se detiene en ella y cae al commit íntegro previo.
    #[test]
    fn single_fsync_rejects_a_half_written_tail_commit() {
        use std::os::unix::fs::FileExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.arkeion");
        let store = Store::create(&path).unwrap();

        // Commits 1 y 2: el estado durable a conservar.
        for v in 1..=2u64 {
            let mut tx = store.begin().unwrap();
            tx.put(format!("k{v}").as_bytes(), format!("v{v}").as_bytes())
                .unwrap();
            tx.commit().unwrap();
        }
        let start_of_3 = std::fs::metadata(&path).unwrap().len(); // inicio en bytes del commit 3

        // Commit 3: el que "se quedó a medias".
        let mut tx = store.begin().unwrap();
        tx.put(b"k3", b"v3").unwrap();
        tx.commit().unwrap();
        drop(store);

        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let flip = |off: u64| {
            let mut b = [0u8; 1];
            f.read_exact_at(&mut b, off).unwrap();
            b[0] ^= 0xFF;
            f.write_all_at(&b, off).unwrap();
        };
        let ps = crate::format::PAGE_SIZE as u64;
        // El slot meta de la versión 3 (impar ⇒ slot B) no llegó: corrómpelo para
        // que recover use el de la versión 2. Los slots meta siguen en posición fija.
        flip(crate::format::META_PAGE_B.0 * ps + 64);
        // La primera página de datos del commit 3 quedó sin escribir: ilegible (el
        // nonce/tag no cuadra), como una escritura desgarrada real. Su registro
        // empieza en `start_of_3`; corrompemos el primer byte de su payload sellado.
        flip(start_of_3 + crate::format::LEN_PREFIX_LEN as u64);
        drop(f);

        // Recuperación: el escaneo hacia delante se topa con la página ilegible
        // antes de la de commit y para; el head queda en el commit 2 íntegro.
        let store = Store::open(&path).unwrap();
        assert_eq!(store.version(), 2, "el commit a medias no debe adoptarse");
        let snap = store.snapshot();
        assert_eq!(snap.get(b"k2").unwrap().unwrap(), b"v2");
        assert!(snap.get(b"k3").unwrap().is_none(), "v3 no debe verse");
        assert!(store.verify().unwrap().chain_ok);
    }

    /// `history()` enumera la línea temporal de versiones; tras `vacuum` solo las
    /// retenidas.
    #[test]
    fn history_lists_the_version_timeline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.arkeion");
        let store = store_with_versions(&path, 4); // versiones 1..=4

        let log = store.history().unwrap();
        assert_eq!(
            log.iter().map(|r| r.version).collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        // Ascendencia lineal: cada versión enraíza en la anterior.
        assert_eq!(log[0].parent, 0);
        assert_eq!(log[3].parent, 3);
        // Timestamps no decrecientes.
        assert!(log.windows(2).all(|w| w[0].timestamp <= w[1].timestamp));

        // Tras vacuum, solo aparecen las versiones retenidas.
        store.vacuum(Retention::KeepLast(2)).unwrap();
        assert_eq!(
            store
                .history()
                .unwrap()
                .iter()
                .map(|r| r.version)
                .collect::<Vec<_>>(),
            vec![3, 4]
        );
    }

    /// `diff_versions` compara dos puntos de la historia (el "git diff").
    #[test]
    fn diff_versions_compares_two_points_in_time() {
        let dir = tempfile::tempdir().unwrap();
        // store_with_versions deja en cada versión v: kv "k{v}" y reescribe "head".
        let store = store_with_versions(&dir.path().join("d.arkeion"), 5);

        // De v2 a v4: aparecen k3 y k4, y "head" se modifica (2→4).
        let changes = store.diff_versions(2, 4).unwrap();
        let keys: std::collections::HashSet<Vec<u8>> =
            changes.iter().map(|c| c.key.clone()).collect();
        assert!(keys.contains(b"k3".as_slice()));
        assert!(keys.contains(b"k4".as_slice()));
        assert!(keys.contains(b"head".as_slice()));
        assert!(!keys.contains(b"k2".as_slice()), "k2 ya existía en v2");

        // Un solo commit: diff_versions(v-1, v) == changes(v).
        let one = store.diff_versions(4, 5).unwrap();
        let one_keys: std::collections::HashSet<Vec<u8>> =
            one.iter().map(|c| c.key.clone()).collect();
        assert!(one_keys.contains(b"k5".as_slice()));
        assert!(one_keys.contains(b"head".as_slice()));
        assert_eq!(one_keys.len(), 2);
        assert_eq!(store.changes(5).unwrap(), one);

        // Génesis (0) a head: todo es nuevo.
        assert!(!store.diff_versions(0, 5).unwrap().is_empty());
        // Versión futura ⇒ VersionNotFound.
        assert!(matches!(
            store.diff_versions(2, 99),
            Err(Error::VersionNotFound(_))
        ));
    }

    /// El ancla de auditoría detecta reescritura (chain_hash distinto) y truncado
    /// (versión futura), y sigue cuadrando aunque se añadan commits.
    #[test]
    fn audit_anchor_catches_rewrite_and_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_versions(&dir.path().join("a.arkeion"), 4);
        let anchor = store.verify().unwrap().anchor();
        assert_eq!(anchor.version, 4);

        // Más commits: el ancla (v4) sigue cuadrando.
        {
            let mut tx = store.begin().unwrap();
            tx.put(b"x", b"y").unwrap();
            tx.commit().unwrap();
        }
        assert!(store.verify_anchor(&anchor).is_ok());

        // chain_hash equivocado ⇒ historia reescrita.
        let mut bad = anchor;
        bad.chain_hash[0] ^= 0xFF;
        assert!(matches!(
            store.verify_anchor(&bad),
            Err(Error::ChainBroken { .. })
        ));

        // Ancla a una versión futura ⇒ faltan commits (truncado).
        let future = commit::AuditAnchor {
            version: 999,
            chain_hash: anchor.chain_hash,
        };
        assert!(matches!(
            store.verify_anchor(&future),
            Err(Error::ChainBroken { .. })
        ));
    }

    /// Un ancla a una versión que `vacuum` compactó ya no se puede comprobar.
    #[test]
    fn audit_anchor_for_a_compacted_version_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_versions(&dir.path().join("a.arkeion"), 2);
        let anchor = store.verify().unwrap().anchor(); // v2
        for v in 3..=6u64 {
            let mut tx = store.begin().unwrap();
            tx.put(format!("k{v}").as_bytes(), b"v").unwrap();
            tx.commit().unwrap();
        }
        store.vacuum(Retention::KeepLast(2)).unwrap(); // K=5: compacta 1..4
        assert!(matches!(
            store.verify_anchor(&anchor),
            Err(Error::ChainBroken { .. })
        ));
        // Sin ancla, la auditoría del archivo compactado sigue OK.
        assert!(store.verify().unwrap().chain_ok);
    }
}
