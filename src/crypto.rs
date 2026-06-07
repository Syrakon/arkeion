//! Sellado e integridad de páginas tras el `trait CryptoProvider` (D8):
//! el motor nunca habla con una primitiva criptográfica directamente, lo que
//! deja el backend sustituible (p. ej. por una implementación europea
//! certificada) sin tocar formato ni motor.

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::format::{PageBuf, PageId, TAG_LEN};

/// Transforma páginas en el borde del disco. En caché y en el motor las
/// páginas viven siempre en claro.
pub trait CryptoProvider: Send + Sync {
    /// Sella la página antes de escribirla: rellena nonce y tag y, si cifra,
    /// transforma el body in place. `nonce_counter` es único por escritura
    /// sellada bajo una misma clave (D6); los proveedores sin cifrado lo ignoran.
    fn seal(&self, page: &mut PageBuf, page_id: PageId, nonce_counter: u64);

    /// Verifica integridad (y descifra) in place tras leer del disco.
    /// El tag liga el contenido a su `page_id`: una página recolocada es
    /// corrupción, no datos válidos en el sitio equivocado.
    fn open(&self, page: &mut PageBuf, page_id: PageId) -> Result<()>;
}

/// Modo sin cifrado: integridad por SHA-256 truncado a 16 bytes sobre
/// `LE(page_id) ‖ body`, con nonce a ceros. Cubre los 4096 bytes de la página:
/// tag y nonce manipulados también se detectan.
#[derive(Debug, Clone, Copy)]
pub struct PlainProvider;

fn plain_tag(page_id: PageId, body: &[u8]) -> [u8; TAG_LEN] {
    let mut hasher = Sha256::new();
    hasher.update(page_id.0.to_le_bytes());
    hasher.update(body);
    let digest = hasher.finalize();
    let mut tag = [0u8; TAG_LEN];
    tag.copy_from_slice(&digest[..TAG_LEN]);
    tag
}

impl CryptoProvider for PlainProvider {
    fn seal(&self, page: &mut PageBuf, page_id: PageId, _nonce_counter: u64) {
        page.nonce_mut().fill(0);
        let tag = plain_tag(page_id, page.body());
        page.tag_mut().copy_from_slice(&tag);
    }

    fn open(&self, page: &mut PageBuf, page_id: PageId) -> Result<()> {
        if page.nonce().iter().any(|&b| b != 0) {
            return Err(Error::Corrupt {
                page: page_id.0,
                reason: "nonce no nulo sin cifrado",
            });
        }
        if page.tag() != plain_tag(page_id, page.body()) {
            return Err(Error::Corrupt {
                page: page_id.0,
                reason: "tag de integridad inválido",
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sealed_page() -> PageBuf {
        let mut page = PageBuf::zeroed();
        page.body_mut()[0] = 0x01;
        page.body_mut()[100] = 0xAB;
        PlainProvider.seal(&mut page, PageId(7), 0);
        page
    }

    #[test]
    fn seal_open_roundtrip() {
        let mut page = sealed_page();
        PlainProvider.open(&mut page, PageId(7)).unwrap();
        assert_eq!(page.body()[100], 0xAB);
    }

    #[test]
    fn detects_flip_of_any_byte() {
        let reference = sealed_page();
        // Cada byte de la página (nonce, tag y body): un flip ⇒ Corrupt.
        for i in 0..reference.as_bytes().len() {
            let mut page = reference.clone();
            page.as_bytes_mut()[i] ^= 0x01;
            assert!(
                PlainProvider.open(&mut page, PageId(7)).is_err(),
                "flip en el byte {i} no detectado"
            );
        }
    }

    #[test]
    fn detects_relocated_page() {
        let mut page = sealed_page();
        assert!(matches!(
            PlainProvider.open(&mut page, PageId(8)),
            Err(Error::Corrupt { page: 8, .. })
        ));
    }
}
