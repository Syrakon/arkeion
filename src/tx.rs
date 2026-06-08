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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::btree::{self, Body, Cursor, NodeSource, NodeStore};
use crate::catalog::{self, TableDef, TableScan, TableSpec};
use crate::commit::{self, COMMIT_FLAG_CHECKPOINT, CommitHeader, Head};
use crate::crypto::Key;
use crate::error::{Error, Result};
use crate::format::{PageBuf, PageId};
use crate::io::sync_parent_dir;
use crate::pager::{Pager, provider_for};
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
struct DbState {
    pager: Arc<Pager>,
    head: Head,
}

/// Almacén clave-valor transaccional sobre un único archivo. Todos sus campos
/// son `Arc`: clonarlo es barato y las transacciones son dueñas de lo que
/// necesitan (sin lifetimes hacia el `Store`).
#[derive(Clone)]
pub struct Store {
    state: Arc<Mutex<DbState>>,
    writer: Arc<AtomicBool>,
    /// Ruta del archivo: la necesita `vacuum` para el archivo temporal y el
    /// rename atómico (M9).
    path: PathBuf,
}

impl Store {
    pub fn create(path: &Path) -> Result<Store> {
        Self::create_keyed(path, None)
    }

    /// Crea el almacén; con `key`, cifrado en reposo (M7, D6).
    pub fn create_keyed(path: &Path, key: Option<&Key>) -> Result<Store> {
        let pager = Pager::create_keyed(path, key)?;
        let head = commit::genesis_head(&pager.header().file_id);
        Ok(Store::from_parts(path, pager, head))
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
        Ok(Store::from_parts(path, pager, head))
    }

    fn from_parts(path: &Path, pager: Pager, head: Head) -> Store {
        Store {
            state: Arc::new(Mutex::new(DbState {
                pager: Arc::new(pager),
                head,
            })),
            writer: Arc::new(AtomicBool::new(false)),
            path: path.to_owned(),
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
        if self.writer.swap(true, Ordering::Acquire) {
            return Err(Error::Busy);
        }
        let (pager, global) = {
            let st = self.lock();
            (st.pager.clone(), st.head.clone())
        };
        let bh = match resolve_branch_head(&pager, &global, branch) {
            Ok(bh) => bh,
            Err(e) => {
                self.writer.store(false, Ordering::Release); // liberar el escritor
                return Err(e);
            }
        };
        Ok(WriteTx {
            ts: TxStore::new(pager.clone()),
            pager,
            state: self.state.clone(),
            writer: self.writer.clone(),
            branch: branch.to_owned(),
            parent_page: bh.commit_page,
            parent_version: bh.version,
            data_root: bh.data_root,
            meta_root: global.meta_root,
            base: global,
            rowid_cache: HashMap::new(),
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
        if self.writer.swap(true, Ordering::Acquire) {
            return Err(Error::Busy);
        }
        let result = self.create_branch_locked(name, from);
        self.writer.store(false, Ordering::Release);
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
        if self.writer.swap(true, Ordering::Acquire) {
            return Err(Error::Busy);
        }
        let result = self.drop_branch_locked(name);
        self.writer.store(false, Ordering::Release);
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

    /// Snapshot histórico (M5, time-travel). `AsOf::Head` equivale a
    /// [`snapshot`](Store::snapshot). Funciona porque el b-tree es CoW
    /// append-only: la raíz de cada versión sigue en disco hasta que `vacuum`
    /// (M9) la compacte, y el índice histórico del árbol meta (escrito en cada
    /// commit, `META_HIST`/`META_TS`) la localiza.
    pub fn snapshot_at(&self, at: AsOf) -> Result<Snapshot> {
        let (pager, head) = {
            let st = self.lock();
            (st.pager.clone(), st.head.clone())
        };
        snapshot_at(&pager, &head, at)
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
        if self.writer.swap(true, Ordering::Acquire) {
            return Err(Error::Busy);
        }
        let result = self.vacuum_locked(retention, rekey);
        self.writer.store(false, Ordering::Release);
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

        // Pager temporal con la cripto destino: misma clave (Keep) o nueva (To).
        let new_pager = match rekey {
            Rekey::Keep => {
                Pager::create_with_crypto(&temp, old_pager.is_encrypted(), old_pager.crypto())?
            }
            Rekey::To(key) => Pager::create_with_crypto(&temp, key.is_some(), provider_for(key))?,
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

        // Intercambia pager y head juntos, bajo un único lock.
        {
            let mut st = self.lock();
            st.pager = new_pager;
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
            // El `commit_page` solo lo tiene a mano la rama tip (head global);
            // para las demás es informativo (no lo usa la lógica).
            let commit_page = if version == global.version {
                global.commit_page
            } else {
                0
            };
            Ok(BranchHead {
                version,
                data_root: read_data_root(pager, global.meta_root, version)?,
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

/// Mayor versión con timestamp ≤ `ms` (índice `META_TS`, ordenado por
/// `(ts, version)` en big-endian): recorre el espacio hacia delante y se queda
/// con la última entrada ≤ `ms`. Antes del primer commit (o de la frontera de
/// `vacuum`) ⇒ estado génesis. Coste O(commits hasta `ms`), aceptable en v1.
fn snapshot_at_timestamp(pager: &Arc<Pager>, head: &Head, ms: u64) -> Result<Snapshot> {
    let src = PagerSource(pager.clone());
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
    /// Estado compartido del almacén: el commit publica el head nuevo aquí. El
    /// escritor único (este `writer`) impide que `vacuum` sustituya el pager
    /// mientras la tx vive, así que `pager` y `state.pager` coinciden al commit.
    state: Arc<Mutex<DbState>>,
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
    /// Contadores de rowid pendientes por `table_id` (M9-perf): se leen del árbol
    /// la primera vez, se incrementan en memoria y se vuelcan **una vez** en el
    /// commit. Evita reescribir la hoja del contador en cada `insert_row`. El
    /// resultado en disco es idéntico (solo persiste el valor final).
    rowid_cache: HashMap<u32, i64>,
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

    /// Inserta y devuelve el rowid (automático o explícito vía columna alias).
    /// El contador de rowid se cachea en la tx y se vuelca en el commit, así que
    /// solo se escribe la hoja de la fila (no la del contador) por inserción.
    pub fn insert_row(&mut self, table: &TableDef, values: &[Value]) -> Result<i64> {
        let explicit = catalog::explicit_rowid(table, values)?;
        let next = match self.rowid_cache.get(&table.table_id) {
            Some(&c) => c,
            None => catalog::read_counter(&self.ts, self.data_root, table.table_id)?,
        };
        let (rowid, new_next) =
            catalog::resolve_rowid(&self.ts, self.data_root, table.table_id, explicit, next)?;
        // Solo se persiste tras escribir la fila: un insert fallido no deja
        // contador a medias en la caché.
        self.data_root = catalog::put_row(&mut self.ts, self.data_root, table, rowid, values)?;
        self.rowid_cache.insert(table.table_id, new_next);
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

        // Páginas sucias + página de commit + un fsync (durabilidad, M1/M9).
        let head = publish_commit(
            &self.pager,
            &mut self.ts,
            CommitParams::after(&self.base, &self.branch, self.parent_page),
            self.data_root,
            meta_root,
            timestamp_ms,
        )?;
        self.state.lock().expect("estado del store envenenado").head = head;
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
    pager.write_reserved_page(commit_page, &page)?;
    pager.cache_insert(commit_page, Arc::new(page));
    // **Único** fsync del commit (M9-perf): páginas de datos y de commit escritas,
    // una sola barrera. Basta para la durabilidad porque la recuperación no
    // confía en el orden de flush del SO: el tag de integridad por página hace
    // **ilegible** cualquier página escrita a medias o no escrita, y el escaneo
    // hacia delante (que lee en orden y las páginas de datos van antes que la de
    // commit) se detiene en la primera ilegible. Así, un commit a medio escribir
    // jamás se adopta; solo se adopta uno cuyas páginas son **todas** legibles
    // (i.e., el fsync llegó a completarse). ~2× en escrituras durables.
    pager.sync()?;

    let head = Head {
        version: header.version,
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
                },
                ColumnSpec {
                    name: "n".into(),
                    col_type: ColType::Integer,
                    not_null: true,
                    primary_key: false,
                    default: None,
                },
            ],
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
        let n_after_2 = store.lock().head.n_pages; // 1ª página del commit 3

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
        // que recover use el de la versión 2.
        flip(crate::format::META_PAGE_B.0 * ps + 64);
        // Una página de datos del commit 3 quedó sin escribir: ilegible (el tag no
        // cuadra), como una escritura desgarrada real.
        flip(n_after_2 * ps + 64);
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
}
