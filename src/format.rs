//! Constantes y layouts binarios del formato de archivo.
//!
//! Fuente de verdad: `docs/02-formato-archivo.md`. Enteros de cabecera en
//! little-endian; claves de B-tree (M1+) en encoding memcomparable big-endian.

use crate::error::{Error, Result};

/// Tamaño de página en disco. 4 KiB: página de SO y sector lógico común (D2).
/// Desde v2 (M10) es el tamaño del **body lógico in-memory** y de los slots
/// estructurales (header, meta); en disco las páginas de datos son registros de
/// **longitud variable** (ver el log enmarcado de la zona append más abajo).
pub const PAGE_SIZE: usize = 4096;
/// Nonce GCM de 96 bits al inicio de cada página (ceros sin cifrado).
pub const NONCE_LEN: usize = 12;
/// Tag de integridad: GCM con cifrado, SHA-256 truncado sin él.
pub const TAG_LEN: usize = 16;
/// Reserva criptográfica uniforme al inicio de cada página en disco (D2).
pub const CRYPTO_RESERVE: usize = NONCE_LEN + TAG_LEN;
/// Bytes útiles del body de una página.
pub const BODY_SIZE: usize = PAGE_SIZE - CRYPTO_RESERVE;

/// Prefijo de longitud de un registro de la zona append (v2, M10): `u32` LE con
/// la longitud del payload sellado que sigue. Hace cada registro **auto-
/// delimitado** para que la recuperación camine el log sin stride fijo.
pub const LEN_PREFIX_LEN: usize = 4;
/// Registro más pequeño posible: prefijo + payload de un body vacío
/// (`nonce ‖ tag` sin ciphertext). Cota inferior segura del tamaño de un
/// registro, usada para acotar la cola rota en bytes (margen de nonce, R7).
pub const MIN_RECORD_LEN: u64 = LEN_PREFIX_LEN as u64 + CRYPTO_RESERVE as u64;

/// Magic de la cabecera de archivo (página 0).
pub const MAGIC_HEADER: &[u8; 8] = b"ARKEION1";
/// Magic de los meta slots (páginas 1 y 2).
pub const MAGIC_META: &[u8; 8] = b"ARKMETA1";
/// Magic de las páginas de commit (zona append).
pub const MAGIC_COMMIT: &[u8; 8] = b"ARKCMT01";
/// Versión del formato que escribe esta build. v2 (M10): zona append como log de
/// registros de longitud variable + directorio de páginas. v3: nodos b-tree con
/// **array de punteros a celda** (búsqueda binaria in-page). v4: **clave de fila
/// de longitud variable** (`[0x01][enc_oint(table_id)][enc_oint(rowid)]`, ~6 B vs
/// 13 fijos). Ruptura limpia entre versiones (pre-1.0 no hay DBs persistidas): no
/// se implementa lectura dual; una migración entre formatos sería un rewrite
/// estilo `vacuum`.
pub const FORMAT_VERSION: u32 = 4;

/// `FileHeader.flags` bit 0: cifrado en reposo activo.
pub const FLAG_ENCRYPTED: u32 = 1;
/// `FileHeader.flags` bit 1: compresión de página activa (v2, M10). Marca que las
/// páginas de datos llevan un byte de método (crudo/comprimido) antes del payload.
pub const FLAG_COMPRESSED: u32 = 2;

/// Página 0: cabecera de archivo (inmutable tras la creación).
pub const HEADER_PAGE: PageId = PageId(0);
/// Página 1: meta slot A.
pub const META_PAGE_A: PageId = PageId(1);
/// Página 2: meta slot B.
pub const META_PAGE_B: PageId = PageId(2);
/// Primera página de la zona append-only.
pub const FIRST_DATA_PAGE: PageId = PageId(3);

/// Identificador de página: índice desde el inicio del archivo.
///
/// Los nodos del B-tree (M1) se referencian entre sí por `PageId`, nunca por
/// referencia: así el aliasing de estructuras enlazadas no existe (R1).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PageId(pub u64);

impl PageId {
    /// Offset en bytes de un slot **estructural** de tamaño fijo (header, meta).
    /// Desde v2 (M10) las páginas de datos (id ≥ [`FIRST_DATA_PAGE`]) ya **no**
    /// viven en `id · PAGE_SIZE`: su posición la da el directorio de páginas del
    /// pager. `FIRST_DATA_PAGE.byte_offset()` sigue marcando el **inicio** de la
    /// zona append (primer registro).
    pub fn byte_offset(self) -> u64 {
        self.0 * PAGE_SIZE as u64
    }
}

/// Buffer de una página completa: reserva criptográfica + body.
///
/// En caché viven siempre en claro (el sellado/descifrado ocurre en el borde
/// del disco, ver `crypto`).
#[derive(Clone)]
pub struct PageBuf {
    bytes: [u8; PAGE_SIZE],
}

impl PageBuf {
    pub fn zeroed() -> PageBuf {
        PageBuf {
            bytes: [0; PAGE_SIZE],
        }
    }

    pub fn as_bytes(&self) -> &[u8; PAGE_SIZE] {
        &self.bytes
    }

    pub fn as_bytes_mut(&mut self) -> &mut [u8; PAGE_SIZE] {
        &mut self.bytes
    }

    pub fn nonce(&self) -> &[u8] {
        &self.bytes[..NONCE_LEN]
    }

    pub fn nonce_mut(&mut self) -> &mut [u8] {
        &mut self.bytes[..NONCE_LEN]
    }

    pub fn tag(&self) -> &[u8] {
        &self.bytes[NONCE_LEN..CRYPTO_RESERVE]
    }

    pub fn tag_mut(&mut self) -> &mut [u8] {
        &mut self.bytes[NONCE_LEN..CRYPTO_RESERVE]
    }

    pub fn body(&self) -> &[u8] {
        &self.bytes[CRYPTO_RESERVE..]
    }

    pub fn body_mut(&mut self) -> &mut [u8] {
        &mut self.bytes[CRYPTO_RESERVE..]
    }
}

impl std::fmt::Debug for PageBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PageBuf({} B)", PAGE_SIZE)
    }
}

/// Cabecera de archivo (body de la página 0). Layout:
///
/// ```text
/// 0..8    magic "ARKEION1"      8..12   format_version u32
/// 12..16  page_size u32         16..20  flags u32 (bit0 = cifrado, bit1 = comprimido)
/// 20..36  file_id [16]          36..52  kdf_salt [16] (reservado, v1 sin KDF)
/// 52..53  ecc_nsym u8 (M10: bytes de paridad RS por bloque; 0 = sin ECC)
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileHeader {
    pub flags: u32,
    pub file_id: [u8; 16],
    pub kdf_salt: [u8; 16],
    /// Bytes de paridad Reed-Solomon por bloque (v2, M10). 0 = sin corrección.
    pub ecc_nsym: u8,
}

impl FileHeader {
    pub fn encode_into(&self, body: &mut [u8]) {
        body[0..8].copy_from_slice(MAGIC_HEADER);
        body[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        body[12..16].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
        body[16..20].copy_from_slice(&self.flags.to_le_bytes());
        body[20..36].copy_from_slice(&self.file_id);
        body[36..52].copy_from_slice(&self.kdf_salt);
        body[52] = self.ecc_nsym;
    }

    pub fn decode(body: &[u8]) -> Result<FileHeader> {
        if &body[0..8] != MAGIC_HEADER {
            return Err(Error::NotADatabase);
        }
        let version = le_u32(body, 8);
        if version > FORMAT_VERSION {
            return Err(Error::UnsupportedFormat { version });
        }
        let page_size = le_u32(body, 12);
        if page_size as usize != PAGE_SIZE {
            return Err(Error::UnsupportedPageSize { page_size });
        }
        let mut file_id = [0u8; 16];
        file_id.copy_from_slice(&body[20..36]);
        let mut kdf_salt = [0u8; 16];
        kdf_salt.copy_from_slice(&body[36..52]);
        Ok(FileHeader {
            flags: le_u32(body, 16),
            file_id,
            kdf_salt,
            ecc_nsym: body[52],
        })
    }
}

/// Meta slot (body de las páginas 1 y 2): caché del último commit para abrir
/// rápido. Se reescribe in-place de forma alternada; siempre apunta hacia
/// atrás, nunca condiciona la durabilidad (esa la da la página de commit).
///
/// ```text
/// 0..8   magic "ARKMETA1"    8..16   version u64
/// 16..24 last_commit_page    24..32  n_pages u64
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MetaSlot {
    pub version: u64,
    pub last_commit_page: PageId,
    pub n_pages: u64,
}

impl MetaSlot {
    /// Slot que corresponde a una versión: alterna A/B por paridad,
    /// determinista y sin estado.
    pub fn slot_page(version: u64) -> PageId {
        PageId(META_PAGE_A.0 + version % 2)
    }

    pub fn encode_into(&self, body: &mut [u8]) {
        body[0..8].copy_from_slice(MAGIC_META);
        body[8..16].copy_from_slice(&self.version.to_le_bytes());
        body[16..24].copy_from_slice(&self.last_commit_page.0.to_le_bytes());
        body[24..32].copy_from_slice(&self.n_pages.to_le_bytes());
    }

    /// `None` si el slot no es válido (el otro slot puede seguir siéndolo).
    pub fn decode(body: &[u8]) -> Option<MetaSlot> {
        if &body[0..8] != MAGIC_META {
            return None;
        }
        Some(MetaSlot {
            version: le_u64(body, 8),
            last_commit_page: PageId(le_u64(body, 16)),
            n_pages: le_u64(body, 24),
        })
    }
}

// --- varint LEB128 (nodos B-tree y registros) ---

pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

pub fn varint_len(v: u64) -> usize {
    (64 - v.leading_zeros() as usize).max(1).div_ceil(7)
}

/// `None` si el buffer se agota o el varint excede 64 bits.
pub fn take_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    let mut shift = 0u32;
    loop {
        let b = *buf.get(*pos)?;
        *pos += 1;
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

pub(crate) fn le_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().expect("rango fijo de 4 bytes"))
}

pub(crate) fn le_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().expect("rango fijo de 8 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_id_offset() {
        assert_eq!(PageId(0).byte_offset(), 0);
        assert_eq!(PageId(3).byte_offset(), 3 * 4096);
    }

    #[test]
    fn header_roundtrip() {
        let h = FileHeader {
            flags: 0,
            file_id: [7; 16],
            kdf_salt: [9; 16],
            ecc_nsym: 16,
        };
        let mut body = vec![0u8; BODY_SIZE];
        h.encode_into(&mut body);
        assert_eq!(FileHeader::decode(&body).unwrap(), h);
    }

    #[test]
    fn header_rejects_bad_magic() {
        let body = vec![0u8; BODY_SIZE];
        assert!(matches!(
            FileHeader::decode(&body),
            Err(Error::NotADatabase)
        ));
    }

    #[test]
    fn header_rejects_future_version() {
        let h = FileHeader {
            flags: 0,
            file_id: [0; 16],
            kdf_salt: [0; 16],
            ecc_nsym: 0,
        };
        let mut body = vec![0u8; BODY_SIZE];
        h.encode_into(&mut body);
        body[8..12].copy_from_slice(&99u32.to_le_bytes());
        assert!(matches!(
            FileHeader::decode(&body),
            Err(Error::UnsupportedFormat { version: 99 })
        ));
    }

    #[test]
    fn meta_roundtrip_and_alternation() {
        let m = MetaSlot {
            version: 5,
            last_commit_page: PageId(42),
            n_pages: 100,
        };
        let mut body = vec![0u8; BODY_SIZE];
        m.encode_into(&mut body);
        assert_eq!(MetaSlot::decode(&body), Some(m));

        assert_eq!(MetaSlot::slot_page(0), META_PAGE_A);
        assert_eq!(MetaSlot::slot_page(1), META_PAGE_B);
        assert_eq!(MetaSlot::slot_page(2), META_PAGE_A);
    }

    #[test]
    fn meta_rejects_bad_magic() {
        let body = vec![0u8; BODY_SIZE];
        assert_eq!(MetaSlot::decode(&body), None);
    }

    #[test]
    fn varint_roundtrip() {
        for v in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            let mut out = Vec::new();
            put_varint(&mut out, v);
            assert_eq!(out.len(), varint_len(v));
            let mut pos = 0;
            assert_eq!(take_varint(&out, &mut pos), Some(v));
            assert_eq!(pos, out.len());
        }
        // Truncado y desbordado: None.
        assert_eq!(take_varint(&[0x80], &mut 0), None);
        assert_eq!(take_varint(&[0xFF; 11], &mut 0), None);
    }
}
