//! Sellado e integridad de páginas tras el `trait CryptoProvider` (D8):
//! el motor nunca habla con una primitiva criptográfica directamente, lo que
//! deja el backend sustituible (p. ej. por una implementación europea
//! certificada) sin tocar formato ni motor.

use aes_gcm::aead::AeadInPlace;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce, Tag};
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

use crate::error::{Error, Result};
use crate::format::{BODY_SIZE, CRYPTO_RESERVE, NONCE_LEN, PAGE_SIZE, PageBuf, PageId, TAG_LEN};

/// Clave de cifrado cruda de 32 bytes (AES-256). El KDF queda **fuera** del
/// motor (D7): el llamador entrega los 32 B ya derivados desde su keystore. Se
/// zeroiza al soltarla y su `Debug` está redactado para que no aflore en logs.
#[derive(Clone)]
pub struct Key([u8; 32]);

impl Key {
    pub fn new(bytes: [u8; 32]) -> Key {
        Key(bytes)
    }

    pub(crate) fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(***)")
    }
}

/// Transforma páginas en el borde del disco. En caché y en el motor las
/// páginas viven siempre en claro.
///
/// Las primitivas son `seal_bytes`/`open_bytes`, que operan sobre un body en
/// claro de **longitud arbitraria** y producen/consumen el payload almacenado
/// `nonce ‖ tag ‖ ciphertext` (M10: páginas de tamaño variable). El sellado va
/// sobre los bytes **finales**, así que cualquier corrupción —incluida la del
/// `stored_len` del marco— se detecta antes de descomprimir. `seal`/`open` son
/// envoltorios para las páginas estructurales de slot fijo (header, meta slots).
pub trait CryptoProvider: Send + Sync {
    /// Sella `body` (claro) en el payload almacenado `nonce(12) ‖ tag(16) ‖
    /// ciphertext(body.len())`. `nonce_counter` es único por escritura sellada
    /// bajo una misma clave (D6); los proveedores sin cifrado lo ignoran. El tag
    /// liga el contenido a su `page_id` (AAD): una página recolocada es
    /// corrupción, no datos válidos en el sitio equivocado.
    fn seal_bytes(&self, body: &[u8], page_id: PageId, nonce_counter: u64) -> Vec<u8>;

    /// Inverso de `seal_bytes`: verifica (y descifra) `stored`
    /// (`nonce ‖ tag ‖ ciphertext`) y devuelve el body en claro. `Corrupt` si el
    /// registro es más corto que la reserva cripto o el tag no valida.
    fn open_bytes(&self, stored: &[u8], page_id: PageId) -> Result<Vec<u8>>;

    /// Sella una página estructural de slot fijo in place (header, meta).
    fn seal(&self, page: &mut PageBuf, page_id: PageId, nonce_counter: u64) {
        let stored = self.seal_bytes(page.body(), page_id, nonce_counter);
        debug_assert_eq!(stored.len(), PAGE_SIZE, "página sellada de tamaño fijo");
        page.as_bytes_mut().copy_from_slice(&stored);
    }

    /// Verifica (y descifra) una página estructural de slot fijo in place.
    fn open(&self, page: &mut PageBuf, page_id: PageId) -> Result<()> {
        let body = self.open_bytes(page.as_bytes(), page_id)?;
        debug_assert_eq!(body.len(), BODY_SIZE, "body estructural de tamaño fijo");
        page.body_mut().copy_from_slice(&body);
        Ok(())
    }
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
    fn seal_bytes(&self, body: &[u8], page_id: PageId, _nonce_counter: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(CRYPTO_RESERVE + body.len());
        out.extend_from_slice(&[0u8; NONCE_LEN]); // nonce a ceros sin cifrado
        out.extend_from_slice(&plain_tag(page_id, body));
        out.extend_from_slice(body); // "ciphertext" = body en claro
        out
    }

    fn open_bytes(&self, stored: &[u8], page_id: PageId) -> Result<Vec<u8>> {
        let (nonce, tag, body) = split_stored(stored, page_id)?;
        if nonce.iter().any(|&b| b != 0) {
            return Err(Error::Corrupt {
                page: page_id.0,
                reason: "nonce no nulo sin cifrado",
            });
        }
        if tag != plain_tag(page_id, body).as_slice() {
            return Err(Error::Corrupt {
                page: page_id.0,
                reason: "tag de integridad inválido",
            });
        }
        Ok(body.to_vec())
    }
}

/// Parte un payload almacenado en `(nonce, tag, ciphertext)`. `Corrupt` si no
/// alcanza siquiera la reserva criptográfica (marco truncado o `stored_len`
/// manipulado a un valor demasiado pequeño).
fn split_stored(stored: &[u8], page_id: PageId) -> Result<(&[u8], &[u8], &[u8])> {
    if stored.len() < CRYPTO_RESERVE {
        return Err(Error::Corrupt {
            page: page_id.0,
            reason: "registro más corto que la reserva criptográfica",
        });
    }
    let (nonce, rest) = stored.split_at(NONCE_LEN);
    let (tag, ct) = rest.split_at(TAG_LEN);
    Ok((nonce, tag, ct))
}

/// Cifrado en reposo con AES-256-GCM por página (D6). El nonce de 96 bits es el
/// contador monótono persistido (`nonce_counter`); el `page_id` va como AAD, de
/// modo que una página recolocada falla la autenticación (igual que el tag en
/// claro liga contenido a posición). El body se cifra in place; tag y nonce van
/// en la reserva criptográfica de la página.
pub struct Aes256GcmProvider {
    cipher: Aes256Gcm,
}

/// HMAC-SHA256 (RFC 2104) sobre una clave de 32 B, hecho a mano con `sha2` para no
/// añadir dependencia. La clave (32 B) es menor que el bloque de SHA-256 (64 B), así
/// que se rellena con ceros. Los intermedios (derivados de la clave) se zeroizan.
fn hmac_sha256(key: &[u8; 32], msg: &[u8]) -> [u8; 32] {
    let mut k_block = [0u8; 64];
    k_block[..32].copy_from_slice(key);
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= k_block[i];
        opad[i] ^= k_block[i];
    }
    let inner = {
        let mut h = Sha256::new();
        h.update(ipad);
        h.update(msg);
        h.finalize()
    };
    let outer = {
        let mut h = Sha256::new();
        h.update(opad);
        h.update(inner);
        h.finalize()
    };
    let mut out = [0u8; 32];
    out.copy_from_slice(&outer);
    k_block.zeroize();
    ipad.zeroize();
    opad.zeroize();
    out
}

impl Aes256GcmProvider {
    /// Deriva una clave AES **distinta por fichero**: `HMAC-SHA256(clave_maestra,
    /// file_id)`. Como `file_id` son 16 B aleatorios únicos por archivo, dos ficheros
    /// con la MISMA clave maestra obtienen claves AES distintas ⇒ el par (clave, nonce)
    /// **nunca colisiona entre ficheros** aunque el llamante reutilice una clave —
    /// defensa en profundidad frente al fallo catastrófico de reúso de nonce en GCM.
    pub fn new(key: &Key, file_id: &[u8; 16]) -> Aes256GcmProvider {
        let mut subkey = hmac_sha256(key.bytes(), file_id);
        let cipher = Aes256Gcm::new_from_slice(&subkey).expect("subclave AES-256 de 32 B");
        subkey.zeroize();
        Aes256GcmProvider { cipher }
    }

    /// Nonce de 96 bits = contador `u64` en LE + 4 bytes de padding a cero. La
    /// unicidad del contador (D6, garantizada por el pager) garantiza la del
    /// nonce: nunca se reutiliza un par (clave, nonce).
    fn nonce_bytes(counter: u64) -> [u8; NONCE_LEN] {
        let mut nonce = [0u8; NONCE_LEN];
        nonce[..8].copy_from_slice(&counter.to_le_bytes());
        nonce
    }
}

impl CryptoProvider for Aes256GcmProvider {
    fn seal_bytes(&self, body: &[u8], page_id: PageId, nonce_counter: u64) -> Vec<u8> {
        let nonce = Self::nonce_bytes(nonce_counter);
        let aad = page_id.0.to_le_bytes();
        let mut ct = body.to_vec();
        let tag = self
            .cipher
            .encrypt_in_place_detached(Nonce::from_slice(&nonce), &aad, &mut ct)
            .expect("AES-GCM no falla cifrando un body en RAM");
        let mut out = Vec::with_capacity(CRYPTO_RESERVE + body.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&tag);
        out.extend_from_slice(&ct);
        out
    }

    fn open_bytes(&self, stored: &[u8], page_id: PageId) -> Result<Vec<u8>> {
        let (nonce, tag, ct) = split_stored(stored, page_id)?;
        let aad = page_id.0.to_le_bytes();
        let mut body = ct.to_vec();
        self.cipher
            .decrypt_in_place_detached(
                Nonce::from_slice(nonce),
                &aad,
                &mut body,
                Tag::from_slice(tag),
            )
            .map_err(|_| Error::Corrupt {
                page: page_id.0,
                reason: "fallo de autenticación AES-GCM",
            })?;
        Ok(body)
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

    // --- AES-256-GCM (M7) ---

    fn key_of(b: u8) -> Key {
        Key::new([b; 32])
    }

    /// `file_id` fijo para los tests de sellado/round-trip (misma clave derivada).
    const FID: [u8; 16] = [0xF1; 16];

    /// Página con un patrón reconocible en el body, sellada con AES-GCM.
    fn aes_sealed(key: &Key, page_id: PageId, counter: u64) -> PageBuf {
        let mut page = PageBuf::zeroed();
        for (i, b) in page.body_mut().iter_mut().enumerate() {
            *b = (i % 256) as u8;
        }
        Aes256GcmProvider::new(key, &FID).seal(&mut page, page_id, counter);
        page
    }

    #[test]
    fn per_file_key_prevents_nonce_reuse() {
        // MISMA clave maestra + MISMO (page_id, counter), distinto file_id ⇒ ciphertext
        // distinto (clave derivada distinta) ⇒ nunca se reutiliza el par (clave, nonce)
        // entre ficheros, AUNQUE el operador reutilice una clave maestra entre tenants.
        let key = key_of(0x9C);
        let seal = |fid: &[u8; 16]| -> PageBuf {
            let mut p = PageBuf::zeroed();
            p.body_mut()[0] = 0xCA;
            Aes256GcmProvider::new(&key, fid).seal(&mut p, PageId(3), 0);
            p
        };
        let a = seal(&[0xAA; 16]);
        let b = seal(&[0xBB; 16]);
        // Mismo counter ⇒ MISMO nonce; la separación NO está en el nonce sino en la
        // clave derivada por file_id, así que el ciphertext (keystream) ya difiere:
        assert_eq!(a.nonce(), b.nonce(), "mismo counter ⇒ mismo nonce (esperado)");
        assert_ne!(a.body(), b.body(), "distinto file_id debe dar ciphertext distinto");
        // Y el provider de un fichero NO abre la página del otro (clave derivada distinta):
        let mut attempt = a.clone();
        assert!(
            Aes256GcmProvider::new(&key, &[0xBB; 16])
                .open(&mut attempt, PageId(3))
                .is_err(),
            "clave derivada de otro file_id no debe descifrar"
        );
    }

    #[test]
    fn aes_seal_open_roundtrip() {
        let key = key_of(0x42);
        let provider = Aes256GcmProvider::new(&key, &FID);
        let mut page = aes_sealed(&key, PageId(7), 99);
        provider.open(&mut page, PageId(7)).unwrap();
        for (i, &b) in page.body().iter().enumerate() {
            assert_eq!(b, (i % 256) as u8, "body[{i}] no se recuperó");
        }
    }

    #[test]
    fn aes_wrong_key_fails_without_corrupting() {
        let page = aes_sealed(&key_of(0x01), PageId(3), 1);
        let mut attempt = page.clone();
        // Clave distinta ⇒ fallo de autenticación, no datos en claro.
        assert!(
            Aes256GcmProvider::new(&key_of(0x02), &FID)
                .open(&mut attempt, PageId(3))
                .is_err()
        );
    }

    #[test]
    fn aes_detects_flip_of_any_byte() {
        let key = key_of(0x5A);
        let reference = aes_sealed(&key, PageId(11), 7);
        let provider = Aes256GcmProvider::new(&key, &FID);
        for i in 0..reference.as_bytes().len() {
            let mut page = reference.clone();
            page.as_bytes_mut()[i] ^= 0x01;
            assert!(
                provider.open(&mut page, PageId(11)).is_err(),
                "flip en el byte {i} no detectado"
            );
        }
    }

    #[test]
    fn aes_detects_relocated_page() {
        // El page_id va como AAD: descifrar en otra posición falla.
        let key = key_of(0x33);
        let mut page = aes_sealed(&key, PageId(5), 3);
        assert!(
            Aes256GcmProvider::new(&key, &FID)
                .open(&mut page, PageId(6))
                .is_err()
        );
    }

    #[test]
    fn aes_distinct_counters_diverge_and_hide_plaintext() {
        let key = key_of(0x77);
        let a = aes_sealed(&key, PageId(3), 1);
        let b = aes_sealed(&key, PageId(3), 2);
        // Mismo plaintext y posición, distinto contador ⇒ nonce y ciphertext
        // distintos: nunca se reutiliza el par (clave, nonce).
        assert_ne!(a.nonce(), b.nonce());
        assert_ne!(a.body(), b.body());

        // El plaintext conocido (0,1,2,3,4,5,6,7) no aparece en la página sellada.
        let needle = [0u8, 1, 2, 3, 4, 5, 6, 7];
        assert!(
            !a.as_bytes().windows(needle.len()).any(|w| w == needle),
            "el plaintext aflora en la página cifrada"
        );
    }

    // --- primitivas de longitud variable (M10) ---

    /// Bodies de longitudes representativas: vacío, 1, mediano y el body completo
    /// de una página de slot fijo.
    fn bodies() -> Vec<Vec<u8>> {
        [0usize, 1, 100, BODY_SIZE]
            .into_iter()
            .map(|n| (0..n).map(|i| (i * 7 % 256) as u8).collect())
            .collect()
    }

    fn providers() -> Vec<(Box<dyn CryptoProvider>, &'static str)> {
        vec![
            (Box::new(PlainProvider), "plain"),
            (Box::new(Aes256GcmProvider::new(&key_of(0x6B), &FID)), "aes"),
        ]
    }

    #[test]
    fn bytes_roundtrip_any_length() {
        for (p, name) in providers() {
            for body in bodies() {
                let stored = p.seal_bytes(&body, PageId(9), 4);
                assert_eq!(
                    stored.len(),
                    CRYPTO_RESERVE + body.len(),
                    "{name}: payload = reserva + body"
                );
                let opened = p.open_bytes(&stored, PageId(9)).unwrap();
                assert_eq!(opened, body, "{name}: round-trip de {} B", body.len());
            }
        }
    }

    #[test]
    fn bytes_detects_flip_of_any_byte() {
        for (p, name) in providers() {
            let body: Vec<u8> = (0..200).map(|i| i as u8).collect();
            let reference = p.seal_bytes(&body, PageId(11), 7);
            for i in 0..reference.len() {
                let mut bad = reference.clone();
                bad[i] ^= 0x01;
                assert!(
                    p.open_bytes(&bad, PageId(11)).is_err(),
                    "{name}: flip del byte {i} no detectado"
                );
            }
        }
    }

    #[test]
    fn bytes_detects_relocation() {
        for (p, name) in providers() {
            let stored = p.seal_bytes(b"contenido", PageId(5), 1);
            assert!(
                matches!(
                    p.open_bytes(&stored, PageId(6)),
                    Err(Error::Corrupt { page: 6, .. })
                ),
                "{name}: abrir en otra posición debe fallar"
            );
        }
    }

    #[test]
    fn bytes_rejects_record_shorter_than_reserve() {
        for (p, name) in providers() {
            let stored = vec![0u8; CRYPTO_RESERVE - 1];
            assert!(
                matches!(
                    p.open_bytes(&stored, PageId(3)),
                    Err(Error::Corrupt { page: 3, .. })
                ),
                "{name}: un marco truncado debe fallar limpio"
            );
        }
    }

    /// El camino `PageBuf` (slot fijo) es exactamente `seal_bytes` sobre el body:
    /// prueba que reexpresarlo no cambió un solo byte en disco.
    #[test]
    fn pagebuf_path_matches_seal_bytes() {
        for (p, name) in providers() {
            let mut page = PageBuf::zeroed();
            for (i, b) in page.body_mut().iter_mut().enumerate() {
                *b = (i % 251) as u8;
            }
            let expected = p.seal_bytes(page.body(), PageId(13), 2);
            p.seal(&mut page, PageId(13), 2);
            assert_eq!(
                page.as_bytes().as_slice(),
                expected,
                "{name}: PageBuf == seal_bytes"
            );

            p.open(&mut page, PageId(13)).unwrap();
            for (i, &b) in page.body().iter().enumerate() {
                assert_eq!(b, (i % 251) as u8, "{name}: body[{i}] tras open");
            }
        }
    }
}
