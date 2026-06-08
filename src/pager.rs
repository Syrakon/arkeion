//! Pager: del archivo a páginas verificadas y cacheadas (capa física).
//!
//! Invariantes que esta capa garantiza:
//! - Las páginas de la zona append (id ≥ 3) son **inmutables** una vez
//!   escritas: la caché nunca necesita invalidación (R2).
//! - Toda página leída pasa por `CryptoProvider::open`: integridad siempre.
//! - Las únicas escrituras in-place son los dos meta slots, que solo apuntan
//!   hacia atrás (la durabilidad la da la página de commit, M1).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::crypto::{Aes256GcmProvider, CryptoProvider, Key, PlainProvider};
use crate::error::{Error, Result};
use crate::format::{
    FIRST_DATA_PAGE, FLAG_ENCRYPTED, FileHeader, HEADER_PAGE, MAGIC_HEADER, META_PAGE_A,
    META_PAGE_B, MetaSlot, PAGE_SIZE, PageBuf, PageId,
};
use crate::io::{DbFile, sync_parent_dir};

pub struct Pager {
    file: DbFile,
    header: FileHeader,
    /// Páginas estructurales (cabecera, meta): siempre en claro (docs/02).
    plain: PlainProvider,
    /// Zona append: `PlainProvider` sin cifrado, `Aes256GcmProvider` con clave (M7).
    crypto: Arc<dyn CryptoProvider>,
    /// Caché de páginas inmutables. Sin tope en M0; eviction en M9.
    cache: Mutex<HashMap<PageId, Arc<PageBuf>>>,
    state: Mutex<AppendState>,
}

#[derive(Debug)]
struct AppendState {
    next_page: u64,
    /// Contador de nonces GCM (D6). `PlainProvider` lo ignora; su valor real
    /// se persiste y recupera vía páginas de commit (M1/M7).
    nonce_counter: u64,
}

impl Pager {
    /// Crea una base de datos nueva sin cifrar.
    pub fn create(path: &Path) -> Result<Pager> {
        Self::create_keyed(path, None)
    }

    /// Crea una base de datos nueva: cabecera + meta slots v0, fsync de datos
    /// y de directorio. Con `key`, marca el archivo como cifrado (D6) y la zona
    /// append se sella con AES-256-GCM; cabecera y meta slots van siempre en
    /// claro (no contienen datos de usuario).
    pub fn create_keyed(path: &Path, key: Option<&Key>) -> Result<Pager> {
        let file = DbFile::create_new(path)?;
        if !file.try_lock_exclusive()? {
            return Err(Error::Busy);
        }

        let mut file_id = [0u8; 16];
        getrandom::fill(&mut file_id).map_err(|e| Error::Io(std::io::Error::other(e)))?;
        let mut kdf_salt = [0u8; 16];
        getrandom::fill(&mut kdf_salt).map_err(|e| Error::Io(std::io::Error::other(e)))?;
        let header = FileHeader {
            flags: if key.is_some() { FLAG_ENCRYPTED } else { 0 },
            file_id,
            kdf_salt,
        };
        let crypto: Arc<dyn CryptoProvider> = match key {
            Some(k) => Arc::new(Aes256GcmProvider::new(k)),
            None => Arc::new(PlainProvider),
        };

        let pager = Pager {
            file,
            header,
            plain: PlainProvider,
            crypto,
            cache: Mutex::new(HashMap::new()),
            state: Mutex::new(AppendState {
                next_page: FIRST_DATA_PAGE.0,
                nonce_counter: 0,
            }),
        };

        let mut page = PageBuf::zeroed();
        pager.header.encode_into(page.body_mut());
        pager.plain.seal(&mut page, HEADER_PAGE, 0);
        pager
            .file
            .write_all_at(page.as_bytes(), HEADER_PAGE.byte_offset())?;

        // v0 idéntica en ambos slots: cualquiera de los dos arranca la base.
        let meta = MetaSlot {
            version: 0,
            last_commit_page: PageId(0),
            n_pages: FIRST_DATA_PAGE.0,
        };
        pager.write_meta_at(&meta, META_PAGE_A)?;
        pager.write_meta_at(&meta, META_PAGE_B)?;

        pager.file.sync_data()?;
        sync_parent_dir(path)?;
        Ok(pager)
    }

    /// Abre una base existente sin cifrar (o cifrada si se pasa clave: ver
    /// [`open_keyed`](Pager::open_keyed)).
    pub fn open(path: &Path) -> Result<Pager> {
        Self::open_keyed(path, None)
    }

    /// Abre una base existente: valida cabecera y exige al menos un meta slot
    /// válido. Una cola rota (append sin commit) se ignora por construcción.
    ///
    /// Cifrado (M7): si el archivo tiene el bit `FLAG_ENCRYPTED`, se requiere
    /// `key` ([`Error::KeyRequired`] si falta) y se valida descifrando la
    /// primera página de datos ([`Error::WrongKey`] si la clave no encaja).
    /// Pasar clave para un archivo sin cifrar es un error de uso.
    pub fn open_keyed(path: &Path, key: Option<&Key>) -> Result<Pager> {
        let file = DbFile::open_rw(path)?;
        if !file.try_lock_exclusive()? {
            return Err(Error::Busy);
        }

        let len = file.byte_len()?;
        if len < FIRST_DATA_PAGE.byte_offset() {
            return Err(Error::NotADatabase);
        }

        let mut page = PageBuf::zeroed();
        file.read_exact_at(page.as_bytes_mut(), HEADER_PAGE.byte_offset())?;
        // Magic antes que tag: «no es Arkeion» es más útil que «corrupto».
        if &page.body()[0..8] != MAGIC_HEADER {
            return Err(Error::NotADatabase);
        }
        PlainProvider.open(&mut page, HEADER_PAGE)?;
        let header = FileHeader::decode(page.body())?;
        let encrypted = header.flags & FLAG_ENCRYPTED != 0;
        let crypto: Arc<dyn CryptoProvider> = match (encrypted, key) {
            (true, Some(k)) => Arc::new(Aes256GcmProvider::new(k)),
            (true, None) => return Err(Error::KeyRequired),
            (false, None) => Arc::new(PlainProvider),
            (false, Some(_)) => {
                return Err(Error::InvalidInput(
                    "se proporcionó clave para un archivo sin cifrar",
                ));
            }
        };

        let pager = Pager {
            file,
            header,
            plain: PlainProvider,
            crypto,
            cache: Mutex::new(HashMap::new()),
            // floor: una página final a medio escribir queda fuera y será
            // sobrescrita por el siguiente append (truncado lógico).
            state: Mutex::new(AppendState {
                next_page: (len / PAGE_SIZE as u64).max(FIRST_DATA_PAGE.0),
                nonce_counter: 0,
            }),
        };
        pager.read_meta()?;

        // Validación de clave (M7): la primera página de datos debe descifrar.
        // Una clave errónea aflora aquí como `WrongKey`, no como una base vacía
        // (la recuperación, sin esto, descartaría todo commit que no abre).
        if encrypted && pager.n_pages() > FIRST_DATA_PAGE.0 {
            pager
                .read_page(FIRST_DATA_PAGE)
                .map_err(|_| Error::WrongKey)?;
        }
        Ok(pager)
    }

    /// Añade una página a la zona append. `build` rellena el body en claro;
    /// el sellado ocurre al salir hacia el disco. No hace fsync: el llamador
    /// agrupa appends y llama a `sync` (protocolo de commit, M1).
    pub fn append_page(&self, build: impl FnOnce(&mut [u8])) -> Result<PageId> {
        let mut page = PageBuf::zeroed();
        build(page.body_mut());

        let mut state = self.state.lock().expect("estado del pager envenenado");
        let id = PageId(state.next_page);
        let cached = Arc::new(page.clone());
        self.crypto.seal(&mut page, id, state.nonce_counter);
        self.file.write_all_at(page.as_bytes(), id.byte_offset())?;
        state.next_page += 1;
        state.nonce_counter += 1;
        drop(state);

        self.cache
            .lock()
            .expect("caché envenenada")
            .insert(id, cached);
        Ok(id)
    }

    /// Lee una página de la zona append, verificada y cacheada.
    pub fn read_page(&self, id: PageId) -> Result<Arc<PageBuf>> {
        debug_assert!(
            id >= FIRST_DATA_PAGE,
            "las páginas estructurales no pasan por read_page"
        );
        if id.0 >= self.n_pages() {
            return Err(Error::Corrupt {
                page: id.0,
                reason: "página fuera de rango",
            });
        }
        if let Some(p) = self.cache.lock().expect("caché envenenada").get(&id) {
            return Ok(p.clone());
        }

        let mut page = PageBuf::zeroed();
        self.file
            .read_exact_at(page.as_bytes_mut(), id.byte_offset())?;
        self.crypto.open(&mut page, id)?;
        let page = Arc::new(page);
        self.cache
            .lock()
            .expect("caché envenenada")
            .insert(id, page.clone());
        Ok(page)
    }

    /// fsync de datos: el punto de durabilidad del protocolo de commit.
    pub fn sync(&self) -> Result<()> {
        self.file.sync_data().map_err(Error::from)
    }

    /// Escribe una página en una posición de la zona append reservada por el
    /// escritor único (commit, M1). No toca `next_page`: el cursor lo publica
    /// el commit con `set_n_pages` tras el fsync de durabilidad.
    pub fn write_reserved_page(&self, id: PageId, page: &PageBuf) -> Result<()> {
        debug_assert!(
            id >= FIRST_DATA_PAGE,
            "posición reservada fuera de la zona append"
        );
        let mut sealed = page.clone();
        let mut state = self.state.lock().expect("estado del pager envenenado");
        self.crypto.seal(&mut sealed, id, state.nonce_counter);
        state.nonce_counter += 1;
        self.file
            .write_all_at(sealed.as_bytes(), id.byte_offset())?;
        Ok(())
    }

    /// Inserta una página (en claro) en la caché. La usa el commit para que
    /// las páginas recién escritas se relean sin tocar disco; sobreescribe
    /// entradas obsoletas de una cola rota anterior.
    pub fn cache_insert(&self, id: PageId, page: Arc<PageBuf>) {
        self.cache
            .lock()
            .expect("caché envenenada")
            .insert(id, page);
    }

    /// Publica el nuevo final lógico del archivo (solo el escritor único).
    pub fn set_n_pages(&self, n: u64) {
        self.state
            .lock()
            .expect("estado del pager envenenado")
            .next_page = n;
    }

    pub fn nonce_counter(&self) -> u64 {
        self.state
            .lock()
            .expect("estado del pager envenenado")
            .nonce_counter
    }

    /// Restaura el contador de nonces tras la recuperación (M1).
    ///
    /// M7: al activar cifrado, la restauración deberá reservar bloques de
    /// contador vía meta slot para que una cola rota jamás pueda provocar
    /// reutilización de nonce (D6, R7).
    pub fn set_nonce_counter(&self, counter: u64) {
        self.state
            .lock()
            .expect("estado del pager envenenado")
            .nonce_counter = counter;
    }

    /// Mejor meta slot disponible: el válido con mayor versión. Tolera un
    /// slot corrupto (crash a mitad de su reescritura).
    pub fn read_meta(&self) -> Result<MetaSlot> {
        let best = [META_PAGE_A, META_PAGE_B]
            .into_iter()
            .filter_map(|id| self.read_meta_slot(id))
            .max_by_key(|m| m.version);
        best.ok_or(Error::Corrupt {
            page: META_PAGE_A.0,
            reason: "ningún meta slot válido",
        })
    }

    /// Escribe el slot que corresponde a `slot.version` (alternancia A/B).
    pub fn write_meta(&self, slot: &MetaSlot) -> Result<()> {
        self.write_meta_at(slot, MetaSlot::slot_page(slot.version))
    }

    /// Todos los meta slots válidos (0, 1 o 2), para la recuperación: el más
    /// nuevo puede apuntar a una cola truncada y el viejo seguir siendo útil.
    pub fn meta_slots(&self) -> Vec<MetaSlot> {
        [META_PAGE_A, META_PAGE_B]
            .into_iter()
            .filter_map(|id| self.read_meta_slot(id))
            .collect()
    }

    fn write_meta_at(&self, slot: &MetaSlot, id: PageId) -> Result<()> {
        let mut page = PageBuf::zeroed();
        slot.encode_into(page.body_mut());
        self.plain.seal(&mut page, id, 0);
        self.file.write_all_at(page.as_bytes(), id.byte_offset())?;
        Ok(())
    }

    fn read_meta_slot(&self, id: PageId) -> Option<MetaSlot> {
        let mut page = PageBuf::zeroed();
        self.file
            .read_exact_at(page.as_bytes_mut(), id.byte_offset())
            .ok()?;
        self.plain.open(&mut page, id).ok()?;
        MetaSlot::decode(page.body())
    }

    /// Número de páginas del archivo (siguiente id de append).
    pub fn n_pages(&self) -> u64 {
        self.state
            .lock()
            .expect("estado del pager envenenado")
            .next_page
    }

    pub fn header(&self) -> &FileHeader {
        &self.header
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::os::unix::fs::FileExt;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::format::BODY_SIZE;

    fn db_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("t.arkeion")
    }

    /// Patrón determinista y distinto por página.
    fn pattern(i: u64) -> impl FnOnce(&mut [u8]) {
        move |body: &mut [u8]| {
            for (j, b) in body.iter_mut().enumerate() {
                *b = ((i as usize * 31 + j) % 251) as u8;
            }
        }
    }

    fn flip_byte(path: &Path, offset: u64) {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        let mut b = [0u8; 1];
        f.read_exact_at(&mut b, offset).unwrap();
        b[0] ^= 0x01;
        f.write_all_at(&b, offset).unwrap();
    }

    #[test]
    fn create_then_reopen_preserves_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);

        let pager = Pager::create(&path).unwrap();
        let file_id = pager.header().file_id;
        assert_eq!(pager.n_pages(), FIRST_DATA_PAGE.0);
        drop(pager);

        let pager = Pager::open(&path).unwrap();
        assert_eq!(pager.header().file_id, file_id);
        let meta = pager.read_meta().unwrap();
        assert_eq!(meta.version, 0);
        assert_eq!(meta.n_pages, FIRST_DATA_PAGE.0);
    }

    #[test]
    fn append_sync_reopen_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        const N: u64 = 20;

        let pager = Pager::create(&path).unwrap();
        let ids: Vec<PageId> = (0..N)
            .map(|i| pager.append_page(pattern(i)).unwrap())
            .collect();
        pager.sync().unwrap();
        assert_eq!(ids[0], FIRST_DATA_PAGE);
        assert_eq!(pager.n_pages(), FIRST_DATA_PAGE.0 + N);
        drop(pager);

        let pager = Pager::open(&path).unwrap();
        assert_eq!(pager.n_pages(), FIRST_DATA_PAGE.0 + N);
        for (i, id) in ids.iter().enumerate() {
            let page = pager.read_page(*id).unwrap();
            let mut expected = vec![0u8; BODY_SIZE];
            pattern(i as u64)(&mut expected);
            assert_eq!(
                page.body(),
                &expected[..],
                "página {i} difiere tras reabrir"
            );
        }
    }

    #[test]
    fn detects_single_byte_corruption_in_every_data_page_byte() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);

        let pager = Pager::create(&path).unwrap();
        for i in 0..5 {
            pager.append_page(pattern(i)).unwrap();
        }
        pager.sync().unwrap();
        drop(pager);

        // Exhaustivo en la primera página de datos: cada uno de sus 4096 bytes.
        let base = FIRST_DATA_PAGE.byte_offset();
        for off in 0..PAGE_SIZE as u64 {
            flip_byte(&path, base + off);
            let pager = Pager::open(&path).unwrap();
            assert!(
                matches!(
                    pager.read_page(FIRST_DATA_PAGE),
                    Err(Error::Corrupt { page, .. }) if page == FIRST_DATA_PAGE.0
                ),
                "flip del byte {off} no detectado"
            );
            drop(pager);
            flip_byte(&path, base + off); // restaurar
        }

        // Muestra en el resto de páginas de datos: un byte en medio del body.
        for i in 1..5u64 {
            let id = PageId(FIRST_DATA_PAGE.0 + i);
            flip_byte(&path, id.byte_offset() + 2048);
            let pager = Pager::open(&path).unwrap();
            assert!(
                pager.read_page(id).is_err(),
                "corrupción en página {} no detectada",
                id.0
            );
            drop(pager);
            flip_byte(&path, id.byte_offset() + 2048);
        }

        // Restaurado todo: vuelve a leer limpio.
        let pager = Pager::open(&path).unwrap();
        for i in 0..5u64 {
            pager.read_page(PageId(FIRST_DATA_PAGE.0 + i)).unwrap();
        }
    }

    #[test]
    fn meta_slots_alternate_and_newest_valid_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        let pager = Pager::create(&path).unwrap();

        let v1 = MetaSlot {
            version: 1,
            last_commit_page: PageId(10),
            n_pages: 11,
        };
        pager.write_meta(&v1).unwrap();
        assert_eq!(pager.read_meta().unwrap(), v1);

        let v2 = MetaSlot {
            version: 2,
            last_commit_page: PageId(20),
            n_pages: 21,
        };
        pager.write_meta(&v2).unwrap();
        assert_eq!(pager.read_meta().unwrap(), v2);
        pager.sync().unwrap();
        drop(pager);

        // Corromper el slot más nuevo (v2 = par ⇒ slot A): debe ganar v1.
        flip_byte(&path, MetaSlot::slot_page(2).byte_offset() + 100);
        let pager = Pager::open(&path).unwrap();
        assert_eq!(pager.read_meta().unwrap(), v1);
        drop(pager);

        // Corromper también el otro: abrir debe fallar como Corrupt.
        flip_byte(&path, MetaSlot::slot_page(1).byte_offset() + 100);
        assert!(matches!(Pager::open(&path), Err(Error::Corrupt { .. })));
    }

    #[test]
    fn torn_tail_is_logically_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);

        let pager = Pager::create(&path).unwrap();
        for i in 0..5 {
            pager.append_page(pattern(i)).unwrap();
        }
        pager.sync().unwrap();
        drop(pager);

        // Simular crash: la última página queda a medias.
        let full = (FIRST_DATA_PAGE.0 + 5) * PAGE_SIZE as u64;
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(full - 1000).unwrap();
        drop(f);

        let pager = Pager::open(&path).unwrap();
        assert_eq!(pager.n_pages(), FIRST_DATA_PAGE.0 + 4);
        // Las 4 completas leen bien; la rota está fuera de rango.
        for i in 0..4u64 {
            pager.read_page(PageId(FIRST_DATA_PAGE.0 + i)).unwrap();
        }
        assert!(pager.read_page(PageId(FIRST_DATA_PAGE.0 + 4)).is_err());

        // El siguiente append reutiliza el hueco de la página rota.
        let id = pager.append_page(pattern(99)).unwrap();
        assert_eq!(id, PageId(FIRST_DATA_PAGE.0 + 4));
    }

    #[test]
    fn second_open_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        let pager = Pager::create(&path).unwrap();
        assert!(matches!(Pager::open(&path), Err(Error::Busy)));
        drop(pager);
        Pager::open(&path).unwrap();
    }

    #[test]
    fn garbage_file_is_not_a_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        std::fs::write(&path, vec![0xAAu8; 4 * PAGE_SIZE]).unwrap();
        assert!(matches!(Pager::open(&path), Err(Error::NotADatabase)));

        let short = dir.path().join("corto.arkeion");
        std::fs::write(&short, b"x").unwrap();
        assert!(matches!(Pager::open(&short), Err(Error::NotADatabase)));
    }

    #[test]
    fn encrypted_flag_requires_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);

        // Construir a mano una cabecera con el bit de cifrado activo.
        let header = FileHeader {
            flags: FLAG_ENCRYPTED,
            file_id: [1; 16],
            kdf_salt: [2; 16],
        };
        let mut page = PageBuf::zeroed();
        header.encode_into(page.body_mut());
        PlainProvider.seal(&mut page, HEADER_PAGE, 0);

        let mut bytes = Vec::new();
        bytes.extend_from_slice(page.as_bytes());
        let meta = MetaSlot {
            version: 0,
            last_commit_page: PageId(0),
            n_pages: 3,
        };
        for id in [META_PAGE_A, META_PAGE_B] {
            let mut m = PageBuf::zeroed();
            meta.encode_into(m.body_mut());
            PlainProvider.seal(&mut m, id, 0);
            bytes.extend_from_slice(m.as_bytes());
        }
        std::fs::write(&path, bytes).unwrap();

        assert!(matches!(Pager::open(&path), Err(Error::KeyRequired)));
    }

    #[test]
    fn encrypted_roundtrip_key_required_and_wrong_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        let key = Key::new([0x11; 32]);

        let pager = Pager::create_keyed(&path, Some(&key)).unwrap();
        let ids: Vec<PageId> = (0..6)
            .map(|i| pager.append_page(pattern(i)).unwrap())
            .collect();
        pager.sync().unwrap();
        drop(pager);

        // Sin clave ⇒ KeyRequired.
        assert!(matches!(Pager::open(&path), Err(Error::KeyRequired)));

        // Con la clave correcta ⇒ datos íntegros.
        let pager = Pager::open_keyed(&path, Some(&key)).unwrap();
        for (i, id) in ids.iter().enumerate() {
            let page = pager.read_page(*id).unwrap();
            let mut expected = vec![0u8; BODY_SIZE];
            pattern(i as u64)(&mut expected);
            assert_eq!(
                page.body(),
                &expected[..],
                "página {i} difiere tras reabrir"
            );
        }
        drop(pager);

        // Clave errónea ⇒ WrongKey, nunca datos en claro.
        assert!(matches!(
            Pager::open_keyed(&path, Some(&Key::new([0x22; 32]))),
            Err(Error::WrongKey)
        ));
    }

    #[test]
    fn encrypted_file_hides_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        let key = Key::new([0x33; 32]);

        let pager = Pager::create_keyed(&path, Some(&key)).unwrap();
        let needle = [0xACu8; 48];
        pager
            .append_page(|body| body[..needle.len()].copy_from_slice(&needle))
            .unwrap();
        pager.sync().unwrap();
        drop(pager);

        let raw = std::fs::read(&path).unwrap();
        let data = &raw[FIRST_DATA_PAGE.byte_offset() as usize..];
        assert!(
            !data.windows(needle.len()).any(|w| w == needle),
            "el plaintext aflora en la zona append cifrada"
        );
    }

    #[test]
    fn key_on_a_plaintext_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        Pager::create(&path).unwrap(); // sin cifrar
        assert!(matches!(
            Pager::open_keyed(&path, Some(&Key::new([1; 32]))),
            Err(Error::InvalidInput(_))
        ));
    }
}
