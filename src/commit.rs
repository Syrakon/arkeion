//! Páginas de commit, hash chain y recuperación (docs/02-formato-archivo.md).
//!
//! Un commit es válido solo si su página valida (tag + magic + chain_hash
//! autoconsistente). La recuperación parte del mejor meta slot, degrada al
//! otro o a génesis si hace falta, y escanea hacia delante adoptando commits
//! correctamente encadenados. La cola rota se ignora: ese es todo el «replay».

use sha2::{Digest, Sha256};

use crate::error::Result;
use crate::format::{FIRST_DATA_PAGE, MAGIC_COMMIT, MetaSlot, PageId};
use crate::pager::Pager;

/// Longitud fija del campo de nombre de rama.
pub const BRANCH_MAX: usize = 64;

/// Bit 0 de flags: commit checkpoint de compactación (vacuum, M9).
pub const COMMIT_FLAG_CHECKPOINT: u32 = 1;

/// Cabecera de commit (body de una página tipo commit). Layout:
///
/// ```text
/// 0..8    magic "ARKCMT01"   8..12   flags u32        12..16  reservado
/// 16..24  version u64        24..32  parent_page u64  32..40  prev_page u64
/// 40..48  timestamp_ms u64   48..56  data_root u64    56..64  meta_root u64
/// 64..72  nonce_counter u64  72..80  pages_written    80..144 branch [64]
/// 144..176 content_hash      176..208 prev_chain      208..240 chain_hash
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitHeader {
    pub flags: u32,
    pub version: u64,
    /// Commit padre en la rama (ascendencia de datos). 0 = génesis.
    pub parent_page: u64,
    /// Commit anterior en orden global (la cadena). 0 = génesis.
    pub prev_page: u64,
    pub timestamp_ms: u64,
    pub data_root: PageId,
    pub meta_root: PageId,
    /// Contador de nonces tras este commit.
    pub nonce_counter: u64,
    /// Páginas añadidas por este commit (incluida esta).
    pub pages_written: u64,
    pub branch: String,
    /// SHA-256 de los bodies en claro escritos por el commit, en orden de id
    /// (la propia página de commit queda fuera: la cubre `chain_hash`).
    pub content_hash: [u8; 32],
    /// `chain_hash` del commit global anterior (génesis: ver `genesis_chain`).
    pub prev_chain: [u8; 32],
    pub chain_hash: [u8; 32],
}

impl CommitHeader {
    pub fn encode_into(&self, body: &mut [u8]) {
        debug_assert!(self.branch.len() <= BRANCH_MAX);
        body[0..8].copy_from_slice(MAGIC_COMMIT);
        body[8..12].copy_from_slice(&self.flags.to_le_bytes());
        body[12..16].fill(0);
        body[16..24].copy_from_slice(&self.version.to_le_bytes());
        body[24..32].copy_from_slice(&self.parent_page.to_le_bytes());
        body[32..40].copy_from_slice(&self.prev_page.to_le_bytes());
        body[40..48].copy_from_slice(&self.timestamp_ms.to_le_bytes());
        body[48..56].copy_from_slice(&self.data_root.0.to_le_bytes());
        body[56..64].copy_from_slice(&self.meta_root.0.to_le_bytes());
        body[64..72].copy_from_slice(&self.nonce_counter.to_le_bytes());
        body[72..80].copy_from_slice(&self.pages_written.to_le_bytes());
        body[80..144].fill(0);
        body[80..80 + self.branch.len()].copy_from_slice(self.branch.as_bytes());
        body[144..176].copy_from_slice(&self.content_hash);
        body[176..208].copy_from_slice(&self.prev_chain);
        body[208..240].copy_from_slice(&self.chain_hash);
    }

    /// `None` si el body no es una página de commit bien formada.
    pub fn decode(body: &[u8]) -> Option<CommitHeader> {
        if &body[0..8] != MAGIC_COMMIT {
            return None;
        }
        let u64_at = |off: usize| {
            u64::from_le_bytes(
                body[off..off + 8]
                    .try_into()
                    .expect("rango fijo de 8 bytes"),
            )
        };
        let arr32 = |off: usize| {
            let mut a = [0u8; 32];
            a.copy_from_slice(&body[off..off + 32]);
            a
        };
        let raw_branch = &body[80..144];
        let end = raw_branch
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(BRANCH_MAX);
        let branch = std::str::from_utf8(&raw_branch[..end]).ok()?.to_owned();
        Some(CommitHeader {
            flags: u32::from_le_bytes(body[8..12].try_into().expect("rango fijo de 4 bytes")),
            version: u64_at(16),
            parent_page: u64_at(24),
            prev_page: u64_at(32),
            timestamp_ms: u64_at(40),
            data_root: PageId(u64_at(48)),
            meta_root: PageId(u64_at(56)),
            nonce_counter: u64_at(64),
            pages_written: u64_at(72),
            branch,
            content_hash: arr32(144),
            prev_chain: arr32(176),
            chain_hash: arr32(208),
        })
    }

    /// Recalcula el hash de cadena a partir de los campos (docs/02):
    /// `SHA-256(prev_chain ‖ content_hash ‖ version ‖ ts ‖ data_root ‖ meta_root ‖ branch[64])`.
    pub fn compute_chain(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(self.prev_chain);
        h.update(self.content_hash);
        h.update(self.version.to_le_bytes());
        h.update(self.timestamp_ms.to_le_bytes());
        h.update(self.data_root.0.to_le_bytes());
        h.update(self.meta_root.0.to_le_bytes());
        let mut branch = [0u8; BRANCH_MAX];
        branch[..self.branch.len()].copy_from_slice(self.branch.as_bytes());
        h.update(branch);
        h.finalize().into()
    }

    fn self_consistent(&self) -> bool {
        self.chain_hash == self.compute_chain()
    }
}

/// Eslabón cero de la cadena, ligado a la identidad del archivo.
pub fn genesis_chain(file_id: &[u8; 16]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(crate::format::MAGIC_HEADER);
    h.update(file_id);
    h.finalize().into()
}

/// Estado vivo del último commit válido (o génesis).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Head {
    pub version: u64,
    /// Página del commit head. 0 = sin commits (génesis).
    pub commit_page: u64,
    pub data_root: PageId,
    pub meta_root: PageId,
    pub chain_hash: [u8; 32],
    pub nonce_counter: u64,
    /// Final lógico del archivo: primera página libre para el próximo commit.
    pub n_pages: u64,
}

pub fn genesis_head(file_id: &[u8; 16]) -> Head {
    Head {
        version: 0,
        commit_page: 0,
        data_root: PageId(0),
        meta_root: PageId(0),
        chain_hash: genesis_chain(file_id),
        nonce_counter: 0,
        n_pages: FIRST_DATA_PAGE.0,
    }
}

fn head_from(h: &CommitHeader, page: PageId) -> Head {
    Head {
        version: h.version,
        commit_page: page.0,
        data_root: h.data_root,
        meta_root: h.meta_root,
        chain_hash: h.chain_hash,
        nonce_counter: h.nonce_counter,
        n_pages: page.0 + 1,
    }
}

/// Intenta construir un head desde una página de commit; `None` si la página
/// no existe, no valida o no es autoconsistente.
fn try_head_at(pager: &Pager, page: PageId) -> Option<Head> {
    let buf = pager.read_page(page).ok()?;
    let h = CommitHeader::decode(buf.body())?;
    if !h.self_consistent() {
        return None;
    }
    Some(head_from(&h, page))
}

/// Recuperación al abrir. El meta slot es solo una pista de arranque
/// (puede ser viejo o apuntar a una cola truncada): se degrada con elegancia
/// y el escaneo hacia delante encuentra los commits que falten.
pub fn recover(pager: &Pager) -> Result<Head> {
    let genesis = genesis_head(&pager.header().file_id);

    // Candidatos por meta slot, de más nuevo a más viejo.
    let mut slots = pager.meta_slots();
    slots.sort_by_key(|m| std::cmp::Reverse(m.version));
    let mut head = slots
        .iter()
        .filter(|m| m.last_commit_page.0 >= FIRST_DATA_PAGE.0)
        .find_map(|m| try_head_at(pager, m.last_commit_page))
        .unwrap_or(genesis);

    // Escaneo hacia delante: adopta commits encadenados (versión consecutiva
    // y prev_chain == cadena del head). Se detiene en la cola rota.
    let total = pager.n_pages();
    for pid in head.n_pages..total {
        let page = PageId(pid);
        let Ok(buf) = pager.read_page(page) else {
            break; // página ilegible: empieza la cola rota
        };
        let Some(h) = CommitHeader::decode(buf.body()) else {
            continue; // página de datos de un commit posterior: seguir
        };
        if h.version == head.version + 1 && h.prev_chain == head.chain_hash && h.self_consistent() {
            head = head_from(&h, page);
        } else {
            // Commit que no encadena: imposible con escritor serial salvo
            // corrupción dirigida. Conservador: no adoptar, parar aquí.
            break;
        }
    }
    Ok(head)
}

/// Meta slot coherente con un head (lo escribe el commit y la recuperación no
/// lo necesita, pero acelera la próxima apertura).
pub fn meta_for(head: &Head) -> MetaSlot {
    MetaSlot {
        version: head.version,
        last_commit_page: PageId(head.commit_page),
        n_pages: head.n_pages,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> CommitHeader {
        let mut h = CommitHeader {
            flags: 0,
            version: 7,
            parent_page: 100,
            prev_page: 100,
            timestamp_ms: 1_700_000_000_000,
            data_root: PageId(120),
            meta_root: PageId(121),
            nonce_counter: 130,
            pages_written: 23,
            branch: "main".to_owned(),
            content_hash: [3; 32],
            prev_chain: [5; 32],
            chain_hash: [0; 32],
        };
        h.chain_hash = h.compute_chain();
        h
    }

    #[test]
    fn roundtrip_and_self_consistency() {
        let h = header();
        let mut body = vec![0u8; crate::format::BODY_SIZE];
        h.encode_into(&mut body);
        let parsed = CommitHeader::decode(&body).unwrap();
        assert_eq!(parsed, h);
        assert!(parsed.self_consistent());
    }

    #[test]
    fn tampering_breaks_chain_hash() {
        let mut h = header();
        h.version = 8; // manipular un campo sin recalcular la cadena
        assert!(!h.self_consistent());
    }

    #[test]
    fn decode_rejects_non_commit() {
        let body = vec![0u8; crate::format::BODY_SIZE];
        assert!(CommitHeader::decode(&body).is_none());
    }

    #[test]
    fn genesis_is_deterministic_and_file_bound() {
        let a = genesis_chain(&[1; 16]);
        assert_eq!(a, genesis_chain(&[1; 16]));
        assert_ne!(a, genesis_chain(&[2; 16]));
    }
}
