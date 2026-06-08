//! Capa transaccional: `Store` (KV ACID), snapshots de lectura sin locks y
//! escritor único serializado (D9).
//!
//! Una transacción de escritura acumula páginas **sucias en memoria** con ids
//! ya definitivos (posiciones ≥ EOF lógico, posible porque el escritor es
//! único). El commit las escribe en orden, hace fsync, añade la página de
//! commit (hash chain incluida) y vuelve a hacer fsync: ese es el punto de
//! durabilidad. Un rollback es simplemente soltar el estado en memoria.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::btree::{self, Body, Cursor, NodeSource, NodeStore};
use crate::catalog::{self, TableDef, TableScan, TableSpec};
use crate::commit::{self, CommitHeader, Head};
use crate::crypto::Key;
use crate::error::{Error, Result};
use crate::format::{PageBuf, PageId};
use crate::pager::Pager;
use crate::record::Value;

/// Rama única de M1; el branching llega en M8.
pub const MAIN_BRANCH: &str = "main";

// Espacios de claves del árbol meta global (docs/02).
const META_REF: u8 = 0x01;
const META_HIST: u8 = 0x02;
const META_TS: u8 = 0x03;

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

/// Almacén clave-valor transaccional sobre un único archivo. Todos sus
/// campos son `Arc`: clonarlo es barato y las transacciones son dueñas de
/// lo que necesitan (sin lifetimes hacia el `Store`).
#[derive(Clone)]
pub struct Store {
    pager: Arc<Pager>,
    head: Arc<Mutex<Head>>,
    writer: Arc<AtomicBool>,
}

impl Store {
    pub fn create(path: &Path) -> Result<Store> {
        Self::create_keyed(path, None)
    }

    /// Crea el almacén; con `key`, cifrado en reposo (M7, D6).
    pub fn create_keyed(path: &Path, key: Option<&Key>) -> Result<Store> {
        let pager = Pager::create_keyed(path, key)?;
        let head = commit::genesis_head(&pager.header().file_id);
        Ok(Store {
            pager: Arc::new(pager),
            head: Arc::new(Mutex::new(head)),
            writer: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn open(path: &Path) -> Result<Store> {
        Self::open_keyed(path, None)
    }

    /// Abre el almacén; con `key`, descifra la zona append (M7). `KeyRequired`
    /// si el archivo está cifrado y falta clave; `WrongKey` si no encaja.
    pub fn open_keyed(path: &Path, key: Option<&Key>) -> Result<Store> {
        let pager = Pager::open_keyed(path, key)?;
        let head = commit::recover(&pager)?;
        // Margen de nonce (D6, R7): la cola rota ocupa como mucho
        // `físicas - n_pages` posiciones, selladas con contadores a partir de
        // `head.nonce_counter`. Retomar pasado ese margen hace estructuralmente
        // imposible reutilizar un nonce, incluso tras crashes repetidos.
        let torn_tail = pager.n_pages().saturating_sub(head.n_pages);
        pager.set_n_pages(head.n_pages);
        pager.set_nonce_counter(head.nonce_counter + torn_tail);
        Ok(Store {
            pager: Arc::new(pager),
            head: Arc::new(Mutex::new(head)),
            writer: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Snapshot de lectura: fija el commit actual y lee páginas inmutables.
    /// Nunca bloquea ni es bloqueado por el escritor.
    pub fn snapshot(&self) -> Snapshot {
        let h = self.head.lock().expect("head envenenado").clone();
        Snapshot {
            pager: self.pager.clone(),
            version: h.version,
            data_root: h.data_root,
        }
    }

    /// Transacción de escritura, dueña de su estado. `Busy` si ya hay una en
    /// curso (R3: no se bloquea, se informa). Se libera al hacer commit o al
    /// soltarla (rollback).
    pub fn begin(&self) -> Result<WriteTx> {
        if self.writer.swap(true, Ordering::Acquire) {
            return Err(Error::Busy);
        }
        let base = self.head.lock().expect("head envenenado").clone();
        Ok(WriteTx {
            pager: self.pager.clone(),
            head: self.head.clone(),
            writer: self.writer.clone(),
            data_root: base.data_root,
            meta_root: base.meta_root,
            ts: TxStore::new(self.pager.clone()),
            base,
        })
    }

    pub fn version(&self) -> u64 {
        self.head.lock().expect("head envenenado").version
    }

    /// Auditoría completa de la hash chain hasta el head actual (M6). Devuelve
    /// un [`commit::AuditReport`]; `ChainBroken` con la versión exacta si una
    /// página histórica fue manipulada.
    pub fn verify(&self) -> Result<commit::AuditReport> {
        let head = self.head.lock().expect("head envenenado").clone();
        commit::verify(&self.pager, &head)
    }

    /// Snapshot histórico (M5, time-travel). `AsOf::Head` equivale a
    /// [`snapshot`](Store::snapshot). Funciona porque el b-tree es CoW
    /// append-only: la raíz de cada versión sigue en disco hasta que `vacuum`
    /// (M9) la compacte, y el índice histórico del árbol meta (escrito en cada
    /// commit, `META_HIST`/`META_TS`) la localiza.
    pub fn snapshot_at(&self, at: AsOf) -> Result<Snapshot> {
        let head = self.head.lock().expect("head envenenado").clone();
        match at {
            AsOf::Head => Ok(self.snapshot_of(head.version, head.data_root)),
            AsOf::Version(v) => self.snapshot_at_version(&head, v),
            AsOf::Timestamp(t) => self.snapshot_at_timestamp(&head, system_time_to_ms(t)),
        }
    }

    fn snapshot_of(&self, version: u64, data_root: PageId) -> Snapshot {
        Snapshot {
            pager: self.pager.clone(),
            version,
            data_root,
        }
    }

    /// Estado tras 0 commits: árbol de datos vacío, ligado a la identidad del
    /// archivo.
    fn genesis_snapshot(&self) -> Snapshot {
        let data_root = commit::genesis_head(&self.pager.header().file_id).data_root;
        self.snapshot_of(0, data_root)
    }

    fn snapshot_at_version(&self, head: &Head, v: u64) -> Result<Snapshot> {
        if v == head.version {
            return Ok(self.snapshot_of(head.version, head.data_root));
        }
        if v > head.version {
            return Err(Error::VersionNotFound(AsOf::Version(v)));
        }
        if v == 0 {
            return Ok(self.genesis_snapshot());
        }
        let src = PagerSource(self.pager.clone());
        let raw = btree::get(&src, head.meta_root, &hist_key(v))?
            .ok_or(Error::VersionNotFound(AsOf::Version(v)))?;
        let bytes: [u8; 8] = raw
            .get(0..8)
            .ok_or(Error::CorruptRecord("entrada histórica truncada"))?
            .try_into()
            .expect("rango fijo de 8 bytes");
        Ok(self.snapshot_of(v, PageId(u64::from_le_bytes(bytes))))
    }

    /// Mayor versión con timestamp ≤ `ms` (índice `META_TS`, ordenado por
    /// `(ts, version)` en big-endian): recorre el espacio hacia delante y se
    /// queda con la última entrada ≤ `ms`. Antes del primer commit ⇒ estado
    /// génesis. Coste O(commits hasta `ms`), aceptable en v1 (sin índice
    /// secundario hasta v1.1).
    fn snapshot_at_timestamp(&self, head: &Head, ms: u64) -> Result<Snapshot> {
        let src = PagerSource(self.pager.clone());
        let mut best: Option<u64> = None;
        for item in btree::scan_from(&src, head.meta_root, &[META_TS])? {
            let (key, _) = item?;
            if key.first() != Some(&META_TS) || key.len() != 1 + 8 + 8 {
                break; // fuera del espacio de timestamps
            }
            let ts = u64::from_be_bytes(key[1..9].try_into().expect("rango fijo de 8 bytes"));
            if ts > ms {
                break;
            }
            best = Some(u64::from_be_bytes(
                key[9..17].try_into().expect("rango fijo de 8 bytes"),
            ));
        }
        match best {
            None => Ok(self.genesis_snapshot()),
            Some(v) => self.snapshot_at_version(head, v),
        }
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
}

// --- transacción de escritura ---

/// Soltarla sin `commit` es un rollback: el estado en memoria se descarta y
/// el archivo no se ha tocado más allá de su EOF lógico.
pub struct WriteTx {
    pager: Arc<Pager>,
    head: Arc<Mutex<Head>>,
    writer: Arc<AtomicBool>,
    base: Head,
    data_root: PageId,
    meta_root: PageId,
    ts: TxStore,
}

impl Drop for WriteTx {
    fn drop(&mut self) {
        // Libera el escritor único tanto en commit como en rollback.
        self.writer.store(false, Ordering::Release);
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
        let (root, def) = catalog::create_table(&mut self.ts, self.data_root, spec)?;
        self.data_root = root;
        Ok(def)
    }

    pub fn drop_table(&mut self, name: &str) -> Result<bool> {
        let (root, dropped) = catalog::drop_table(&mut self.ts, self.data_root, name)?;
        self.data_root = root;
        Ok(dropped)
    }

    pub fn table(&self, name: &str) -> Result<Option<TableDef>> {
        catalog::get_table(&self.ts, self.data_root, name)
    }

    /// Inserta y devuelve el rowid (automático o explícito vía columna alias).
    pub fn insert_row(&mut self, table: &TableDef, values: &[Value]) -> Result<i64> {
        let (root, rowid) = catalog::insert_row(&mut self.ts, self.data_root, table, values)?;
        self.data_root = root;
        Ok(rowid)
    }

    /// Sobrescribe una fila. `false` si el rowid no existe.
    pub fn update_row(&mut self, table: &TableDef, rowid: i64, values: &[Value]) -> Result<bool> {
        let (root, ok) = catalog::update_row(&mut self.ts, self.data_root, table, rowid, values)?;
        self.data_root = root;
        Ok(ok)
    }

    pub fn get_row(&self, table: &TableDef, rowid: i64) -> Result<Option<Vec<Value>>> {
        catalog::get_row(&self.ts, self.data_root, table, rowid)
    }

    pub fn delete_row(&mut self, table: &TableDef, rowid: i64) -> Result<bool> {
        let (root, existed) = catalog::delete_row(&mut self.ts, self.data_root, table, rowid)?;
        self.data_root = root;
        Ok(existed)
    }

    pub fn scan_table(&self, table: &TableDef) -> Result<TableScan<'_, TxStore>> {
        catalog::scan_table(&self.ts, self.data_root, table)
    }

    /// Publica la transacción. Devuelve la versión nueva (o la actual si la
    /// tx no tocó nada).
    pub fn commit(mut self) -> Result<u64> {
        if self.ts.dirty.is_empty() && self.data_root == self.base.data_root {
            return Ok(self.base.version);
        }
        let version = self.base.version + 1;
        let timestamp_ms = now_ms();

        // 1. Árbol meta global: índice histórico (versión → raíz de datos),
        //    índice temporal y ref de la rama. Ramifican CON el commit: van
        //    en las mismas páginas sucias.
        let mut hist_val = Vec::with_capacity(16);
        hist_val.extend_from_slice(&self.data_root.0.to_le_bytes());
        hist_val.extend_from_slice(&timestamp_ms.to_le_bytes());
        self.meta_root =
            btree::insert(&mut self.ts, self.meta_root, &hist_key(version), &hist_val)?;
        self.meta_root = btree::insert(
            &mut self.ts,
            self.meta_root,
            &ts_key(timestamp_ms, version),
            &[],
        )?;
        self.meta_root = btree::insert(
            &mut self.ts,
            self.meta_root,
            &ref_key(MAIN_BRANCH),
            &version.to_le_bytes(),
        )?;

        // 2. La página de commit va justo después de la última sucia.
        let commit_page = PageId(self.ts.alloc_next);
        let pager = self.pager.clone();

        // 3. Páginas sucias en orden de id; los huecos (ids liberados y no
        //    reutilizados) se rellenan con padding sellado. content_hash
        //    cubre los bodies en claro en este mismo orden.
        let mut hasher = Sha256::new();
        let mut pages_written = 0u64;
        for pid in self.ts.alloc_base..self.ts.alloc_next {
            let id = PageId(pid);
            let page = self.ts.dirty.remove(&id).unwrap_or_else(PageBuf::zeroed);
            hasher.update(page.body());
            pager.write_reserved_page(id, &page)?;
            pager.cache_insert(id, Arc::new(page));
            pages_written += 1;
        }
        pager.sync()?;

        // 4. Página de commit y segundo fsync: punto de durabilidad.
        let mut header = CommitHeader {
            flags: 0,
            version,
            parent_page: self.base.commit_page,
            prev_page: self.base.commit_page,
            timestamp_ms,
            data_root: self.data_root,
            meta_root: self.meta_root,
            nonce_counter: pager.nonce_counter() + 1, // +1: el sellado de esta página
            pages_written: pages_written + 1,
            branch: MAIN_BRANCH.to_owned(),
            content_hash: hasher.finalize().into(),
            prev_chain: self.base.chain_hash,
            chain_hash: [0; 32],
        };
        header.chain_hash = header.compute_chain();
        let mut page = PageBuf::zeroed();
        header.encode_into(page.body_mut());
        pager.write_reserved_page(commit_page, &page)?;
        pager.cache_insert(commit_page, Arc::new(page));
        pager.sync()?;

        // 5. Publicar: EOF lógico, meta slot (lazy, sin fsync) y head.
        let head = Head {
            version,
            commit_page: commit_page.0,
            data_root: self.data_root,
            meta_root: self.meta_root,
            chain_hash: header.chain_hash,
            nonce_counter: header.nonce_counter,
            n_pages: commit_page.0 + 1,
        };
        pager.set_n_pages(head.n_pages);
        pager.write_meta(&commit::meta_for(&head))?;
        *self.head.lock().expect("head envenenado") = head;
        Ok(version)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn system_time_to_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
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

fn ts_key(timestamp_ms: u64, version: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(17);
    k.push(META_TS);
    k.extend_from_slice(&timestamp_ms.to_be_bytes());
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
        let pages_before = store.pager.n_pages();
        let tx = store.begin().unwrap();
        assert_eq!(tx.commit().unwrap(), 0);
        assert_eq!(store.version(), 0);
        assert_eq!(store.pager.n_pages(), pages_before);
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
            let h = store.head.lock().unwrap();
            (h.n_pages, h.nonce_counter)
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
            store.head.lock().unwrap().n_pages,
            head_pages,
            "el head no debe moverse por la cola rota"
        );
        assert!(
            store.pager.nonce_counter() >= head_counter + extra,
            "el contador {} no superó el margen {}",
            store.pager.nonce_counter(),
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
}
