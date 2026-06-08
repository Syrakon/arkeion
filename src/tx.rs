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

    /// Snapshot de lectura fijado a la cabeza de una rama (M8). `BranchNotFound`
    /// si la rama no existe.
    pub fn snapshot_on(&self, branch: &str) -> Result<Snapshot> {
        let global = self.head.lock().expect("head envenenado").clone();
        let bh = self.resolve_branch_head(&global, branch)?;
        Ok(Snapshot {
            pager: self.pager.clone(),
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
        if self.writer.swap(true, Ordering::Acquire) {
            return Err(Error::Busy);
        }
        let global = self.head.lock().expect("head envenenado").clone();
        let bh = match self.resolve_branch_head(&global, branch) {
            Ok(bh) => bh,
            Err(e) => {
                self.writer.store(false, Ordering::Release); // liberar el escritor
                return Err(e);
            }
        };
        Ok(WriteTx {
            pager: self.pager.clone(),
            head: self.head.clone(),
            writer: self.writer.clone(),
            branch: branch.to_owned(),
            parent_page: bh.commit_page,
            parent_version: bh.version,
            data_root: bh.data_root,
            meta_root: global.meta_root,
            ts: TxStore::new(self.pager.clone()),
            base: global,
        })
    }

    /// Resuelve la cabeza de una rama: su `data_root` y versión. `main` siempre
    /// existe (antes del primer commit es génesis); otra rama inexistente da
    /// `BranchNotFound`.
    fn resolve_branch_head(&self, global: &Head, branch: &str) -> Result<BranchHead> {
        match self.read_ref(global.meta_root, branch)? {
            Some(version) => {
                // El `commit_page` solo lo tiene a mano la rama tip (head
                // global); para las demás es informativo (no lo usa la lógica).
                let commit_page = if version == global.version {
                    global.commit_page
                } else {
                    0
                };
                Ok(BranchHead {
                    version,
                    data_root: self.read_data_root(global.meta_root, version)?,
                    commit_page,
                })
            }
            None if branch == MAIN_BRANCH => Ok(BranchHead {
                version: 0,
                data_root: commit::genesis_head(&self.pager.header().file_id).data_root,
                commit_page: 0,
            }),
            None => Err(Error::BranchNotFound(branch.to_owned())),
        }
    }

    /// Versión a la que apunta `ref[branch]` en un árbol meta, si existe.
    fn read_ref(&self, meta_root: PageId, branch: &str) -> Result<Option<u64>> {
        let src = PagerSource(self.pager.clone());
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
    fn read_data_root(&self, meta_root: PageId, version: u64) -> Result<PageId> {
        if version == 0 {
            return Ok(commit::genesis_head(&self.pager.header().file_id).data_root);
        }
        let src = PagerSource(self.pager.clone());
        let raw = btree::get(&src, meta_root, &hist_key(version))?
            .ok_or(Error::VersionNotFound(AsOf::Version(version)))?;
        let b: [u8; 8] = raw
            .get(0..8)
            .ok_or(Error::CorruptRecord("entrada histórica truncada"))?
            .try_into()
            .expect("rango fijo de 8 bytes");
        Ok(PageId(u64::from_le_bytes(b)))
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
        if self.writer.swap(true, Ordering::Acquire) {
            return Err(Error::Busy);
        }
        let result = self.create_branch_locked(name, from);
        self.writer.store(false, Ordering::Release);
        result
    }

    fn create_branch_locked(&self, name: &str, from: AsOf) -> Result<()> {
        let global = self.head.lock().expect("head envenenado").clone();
        if self.read_ref(global.meta_root, name)?.is_some() {
            return Err(Error::BranchExists(name.to_owned()));
        }
        let from_version = self.snapshot_at(from)?.version();
        let from_data_root = self.read_data_root(global.meta_root, from_version)?;

        let version = global.version + 1;
        let timestamp_ms = now_ms();
        let mut ts = TxStore::new(self.pager.clone());
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
            &self.pager,
            &mut ts,
            &global,
            name,
            global.commit_page,
            from_data_root,
            meta_root,
            timestamp_ms,
        )?;
        *self.head.lock().expect("head envenenado") = head;
        Ok(())
    }

    /// Borra una rama (M8): elimina su `ref`; las páginas de datos quedan (las
    /// recupera `vacuum`, M9). Commit meta-only sobre `main` (datos sin cambio).
    /// No se puede borrar `main`.
    pub fn drop_branch(&self, name: &str) -> Result<()> {
        if name == MAIN_BRANCH {
            return Err(Error::InvalidInput("no se puede borrar la rama principal"));
        }
        if self.writer.swap(true, Ordering::Acquire) {
            return Err(Error::Busy);
        }
        let result = self.drop_branch_locked(name);
        self.writer.store(false, Ordering::Release);
        result
    }

    fn drop_branch_locked(&self, name: &str) -> Result<()> {
        let global = self.head.lock().expect("head envenenado").clone();
        if self.read_ref(global.meta_root, name)?.is_none() {
            return Err(Error::BranchNotFound(name.to_owned()));
        }
        let main = self.resolve_branch_head(&global, MAIN_BRANCH)?;
        let version = global.version + 1;
        let timestamp_ms = now_ms();
        let mut ts = TxStore::new(self.pager.clone());
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
            &self.pager,
            &mut ts,
            &global,
            MAIN_BRANCH,
            main.commit_page,
            main.data_root,
            meta_root,
            timestamp_ms,
        )?;
        *self.head.lock().expect("head envenenado") = head;
        Ok(())
    }

    /// Lista todas las ramas y la versión a la que apunta cada una (M8). `main`
    /// siempre aparece (head 0 antes del primer commit).
    pub fn branches(&self) -> Result<Vec<BranchInfo>> {
        let global = self.head.lock().expect("head envenenado").clone();
        let src = PagerSource(self.pager.clone());
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
        let global = self.head.lock().expect("head envenenado").clone();
        let from_root = self.resolve_branch_head(&global, from)?.data_root;
        let to_root = self.resolve_branch_head(&global, to)?.data_root;
        let src = PagerSource(self.pager.clone());
        btree::diff(&src, from_root, to_root)
    }

    /// Fusiona `from` en `into` (merge 3-way, M8). Encuentra el ancestro común
    /// por versión, compara `diff(base, from)` con `diff(base, into)` clave a
    /// clave: aplica los cambios que solo hizo `from`; ante una clave que ambas
    /// llevaron a estados distintos, conflicto. Un merge limpio aplica
    /// exactamente el diff y nada más (nuevo commit en `into`).
    pub fn merge(&self, from: &str, into: &str, policy: MergePolicy) -> Result<MergeReport> {
        let MergePolicy::FailOnConflict = policy; // v1: única política
        if self.writer.swap(true, Ordering::Acquire) {
            return Err(Error::Busy);
        }
        let result = self.merge_locked(from, into);
        self.writer.store(false, Ordering::Release);
        result
    }

    fn merge_locked(&self, from: &str, into: &str) -> Result<MergeReport> {
        let global = self.head.lock().expect("head envenenado").clone();
        let vf = self
            .read_ref(global.meta_root, from)?
            .ok_or_else(|| Error::BranchNotFound(from.to_owned()))?;
        let vi = self
            .read_ref(global.meta_root, into)?
            .ok_or_else(|| Error::BranchNotFound(into.to_owned()))?;

        let base_v = self.merge_base(global.meta_root, vf, vi)?;
        let base_root = self.read_data_root(global.meta_root, base_v)?;
        let from_root = self.read_data_root(global.meta_root, vf)?;
        let into_root = self.read_data_root(global.meta_root, vi)?;

        let src = PagerSource(self.pager.clone());
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
        let mut ts = TxStore::new(self.pager.clone());
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
            &self.pager,
            &mut ts,
            &global,
            into,
            into_commit_page,
            data_root,
            meta_root,
            timestamp_ms,
        )?;
        *self.head.lock().expect("head envenenado") = head;
        Ok(MergeReport {
            version,
            applied: to_apply.len(),
        })
    }

    /// Ancestro común (merge base) de dos versiones, caminando `parent_version`
    /// (M8). Génesis (0) es ancestro de todo: la búsqueda siempre termina.
    fn merge_base(&self, meta_root: PageId, vf: u64, vi: u64) -> Result<u64> {
        let mut ancestors = std::collections::HashSet::new();
        let mut v = vf;
        loop {
            ancestors.insert(v);
            if v == 0 {
                break;
            }
            v = self.read_parent_version(meta_root, v)?;
        }
        let mut v = vi;
        loop {
            if ancestors.contains(&v) {
                return Ok(v);
            }
            if v == 0 {
                return Ok(0);
            }
            v = self.read_parent_version(meta_root, v)?;
        }
    }

    /// Versión padre (en la rama) de una versión, vía índice histórico (M8).
    fn read_parent_version(&self, meta_root: PageId, version: u64) -> Result<u64> {
        if version == 0 {
            return Ok(0);
        }
        let src = PagerSource(self.pager.clone());
        let raw = btree::get(&src, meta_root, &hist_key(version))?
            .ok_or(Error::VersionNotFound(AsOf::Version(version)))?;
        // hist_val = data_root(8) ‖ ts(8) ‖ parent_version(8).
        match raw.get(16..24) {
            Some(b) => Ok(u64::from_le_bytes(
                b.try_into().expect("rango fijo de 8 bytes"),
            )),
            None => Ok(0), // entradas pre-M8 (16 B): tratar como enraizadas en génesis
        }
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

    /// Publica la transacción en su rama. Devuelve la versión nueva (o la
    /// cabeza de la rama si la tx no tocó nada). Versiones monótonas globales;
    /// el commit lleva `parent_page` de la rama y `prev_page` global (D3).
    pub fn commit(mut self) -> Result<u64> {
        if self.ts.dirty.is_empty() && self.data_root == self.base.data_root {
            return Ok(self.parent_version);
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

        // Páginas sucias + página de commit + fsyncs (durabilidad, M1).
        let head = publish_commit(
            &self.pager,
            &mut self.ts,
            &self.base,
            &self.branch,
            self.parent_page,
            self.data_root,
            meta_root,
            timestamp_ms,
        )?;
        *self.head.lock().expect("head envenenado") = head;
        Ok(version)
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
    let mut hist_val = Vec::with_capacity(24);
    hist_val.extend_from_slice(&data_root.0.to_le_bytes());
    hist_val.extend_from_slice(&timestamp_ms.to_le_bytes());
    hist_val.extend_from_slice(&parent_version.to_le_bytes());
    let mut root = btree::insert(ts, meta_root, &hist_key(version), &hist_val)?;
    root = btree::insert(ts, root, &ts_key(timestamp_ms, version), &[])?;
    root = btree::insert(ts, root, &ref_key(branch), &version.to_le_bytes())?;
    Ok(root)
}

/// Escribe las páginas sucias de la tx, la página de commit y publica el nuevo
/// head (doble fsync: el punto de durabilidad de M1). `base` es el head global
/// anterior (da `prev_page`/`prev_chain`/`meta_root`/contadores).
#[allow(clippy::too_many_arguments)] // firma plana deliberada: el commit
fn publish_commit(
    pager: &Pager,
    ts: &mut TxStore,
    base: &Head,
    branch: &str,
    parent_page: u64,
    data_root: PageId,
    meta_root: PageId,
    timestamp_ms: u64,
) -> Result<Head> {
    let version = base.version + 1;
    let commit_page = PageId(ts.alloc_next);

    // Páginas sucias en orden de id; content_hash cubre los bodies en claro.
    let mut hasher = Sha256::new();
    let mut pages_written = 0u64;
    for pid in ts.alloc_base..ts.alloc_next {
        let id = PageId(pid);
        let page = ts.dirty.remove(&id).unwrap_or_else(PageBuf::zeroed);
        hasher.update(page.body());
        pager.write_reserved_page(id, &page)?;
        pager.cache_insert(id, Arc::new(page));
        pages_written += 1;
    }
    pager.sync()?;

    let mut header = CommitHeader {
        flags: 0,
        version,
        parent_page,
        prev_page: base.commit_page,
        timestamp_ms,
        data_root,
        meta_root,
        nonce_counter: pager.nonce_counter() + 1, // +1: el sellado de esta página
        pages_written: pages_written + 1,
        branch: branch.to_owned(),
        content_hash: hasher.finalize().into(),
        prev_chain: base.chain_hash,
        chain_hash: [0; 32],
    };
    header.chain_hash = header.compute_chain();
    let mut page = PageBuf::zeroed();
    header.encode_into(page.body_mut());
    pager.write_reserved_page(commit_page, &page)?;
    pager.cache_insert(commit_page, Arc::new(page));
    pager.sync()?;

    let head = Head {
        version,
        commit_page: commit_page.0,
        data_root,
        meta_root,
        chain_hash: header.chain_hash,
        nonce_counter: header.nonce_counter,
        n_pages: commit_page.0 + 1,
    };
    pager.set_n_pages(head.n_pages);
    pager.write_meta(&commit::meta_for(&head))?;
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
}
