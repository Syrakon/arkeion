//! Pager: del archivo a páginas verificadas y cacheadas (capa física).
//!
//! Invariantes que esta capa garantiza:
//! - Las páginas de la zona append (id ≥ 3) son **inmutables** una vez
//!   escritas: la caché nunca necesita invalidación (R2).
//! - Toda página leída pasa por `CryptoProvider::open`: integridad siempre.
//! - Las únicas escrituras in-place son los dos meta slots, que solo apuntan
//!   hacia atrás (la durabilidad la da la página de commit, M1).

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::commit::Head;
use crate::compress::{Compressor, Lz};
use crate::crypto::{Aes256GcmProvider, CryptoProvider, Key, PlainProvider};
use crate::ecc;
use crate::error::{Error, Result};
use crate::format::{
    CRYPTO_RESERVE, FIRST_DATA_PAGE, FLAG_COMPRESSED, FLAG_ENCRYPTED, FileHeader, HEADER_PAGE,
    LEN_PREFIX_LEN, MAGIC_HEADER, META_PAGE_A, META_PAGE_B, MetaSlot, PageBuf, PageId,
};
use crate::io::{DbFile, sync_parent_dir};

/// Byte de método de una página de datos cuando la compresión está activa (v2,
/// M10): crudo o comprimido. Solo presente con `FLAG_COMPRESSED`.
const METHOD_RAW: u8 = 0;
const METHOD_LZ: u8 = 1;

/// Tope de la caché de páginas (M9): ~16 MiB de bodies. Las páginas son
/// inmutables, así que evictar nunca pierde datos: se releen del disco.
const CACHE_CAP: usize = 4096;

/// Construye el proveedor cripto para una clave opcional (D8): AES-256-GCM con
/// clave, integridad SHA-256 sin ella.
pub(crate) fn provider_for(key: Option<&Key>) -> Arc<dyn CryptoProvider> {
    match key {
        Some(k) => Arc::new(Aes256GcmProvider::new(k)),
        None => Arc::new(PlainProvider),
    }
}

pub struct Pager {
    file: DbFile,
    header: FileHeader,
    /// Páginas estructurales (cabecera, meta): siempre en claro (docs/02).
    plain: PlainProvider,
    /// Zona append: `PlainProvider` sin cifrado, `Aes256GcmProvider` con clave (M7).
    crypto: Arc<dyn CryptoProvider>,
    /// Compresor de página (v2, M10): `None` = off (formato sin byte de método),
    /// `Some` = on (cada página de datos lleva un byte de método). Se decide al
    /// crear (bit `FLAG_COMPRESSED`) y se lee del header al abrir.
    compressor: Option<Arc<dyn Compressor>>,
    /// Caché acotada de páginas inmutables (M9): LRU aproximada con tope.
    cache: Mutex<PageCache>,
    state: Mutex<AppendState>,
}

/// Caché de páginas con tope y eviction LRU aproximada (M9). Cada entrada lleva
/// el «tick» del último acceso; al superar el tope se evicta en lote (hasta 3/4
/// del tope) descartando las más antiguas. O(n log n) por lote, amortizado a
/// O(log n) por inserción (los lotes distan cap/4 inserciones).
struct PageCache {
    map: HashMap<PageId, (Arc<PageBuf>, u64)>,
    tick: u64,
    cap: usize,
}

impl PageCache {
    fn new(cap: usize) -> PageCache {
        PageCache {
            map: HashMap::new(),
            tick: 0,
            cap,
        }
    }

    fn get(&mut self, id: PageId) -> Option<Arc<PageBuf>> {
        self.tick += 1;
        let tick = self.tick;
        let (page, last) = self.map.get_mut(&id)?;
        *last = tick;
        Some(page.clone())
    }

    fn insert(&mut self, id: PageId, page: Arc<PageBuf>) {
        self.tick += 1;
        self.map.insert(id, (page, self.tick));
        if self.map.len() > self.cap {
            self.evict_batch();
        }
    }

    /// Descarta las entradas más antiguas hasta dejar la caché a 3/4 del tope.
    fn evict_batch(&mut self) {
        let target = self.cap * 3 / 4;
        let drop_n = self.map.len() - target;
        let mut ticks: Vec<u64> = self.map.values().map(|(_, t)| *t).collect();
        ticks.select_nth_unstable(drop_n - 1);
        let threshold = ticks[drop_n - 1];
        self.map.retain(|_, (_, t)| *t > threshold);
    }
}

/// Localización física de una página lógica en la zona append (v2, M10): dónde
/// empieza su payload sellado y cuánto mide. El directorio del pager traduce
/// `page_id → PhysLoc`; las páginas son inmutables, así que una entrada nunca
/// cambia una vez escrita (el directorio es append-only como el resto).
#[derive(Clone, Copy, Debug)]
pub(crate) struct PhysLoc {
    /// Offset del payload sellado, tras el prefijo de longitud del registro.
    offset: u64,
    /// Longitud del payload sellado (`nonce ‖ tag ‖ ciphertext`).
    len: u32,
}

#[derive(Debug)]
struct AppendState {
    next_page: u64,
    /// Contador de nonces GCM (D6). `PlainProvider` lo ignora; su valor real
    /// se persiste y recupera vía páginas de commit (M1/M7).
    nonce_counter: u64,
    /// Offset físico donde se añade el próximo registro (fin de la zona append).
    write_offset: u64,
    /// Directorio de páginas (v2, M10): `dir[i]` localiza la página lógica
    /// `FIRST_DATA_PAGE + i`. Se reconstruye al abrir barriendo los prefijos de
    /// longitud de la zona append.
    dir: Vec<PhysLoc>,
}

impl AppendState {
    /// Localización de una página de datos, o `None` si su id no está en el
    /// directorio (estructural, fuera de rango o aún sin publicar).
    fn loc(&self, id: PageId) -> Option<PhysLoc> {
        let idx = id.0.checked_sub(FIRST_DATA_PAGE.0)?;
        self.dir.get(idx as usize).copied()
    }
}

/// Enmarca un payload sellado: `[u32 LE len][payload]` (v2, M10).
fn frame(stored: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(LEN_PREFIX_LEN + stored.len());
    out.extend_from_slice(&(stored.len() as u32).to_le_bytes());
    out.extend_from_slice(stored);
    out
}

/// Body sin su cola de ceros: la compresión gratuita de v2. El body lógico sigue
/// siendo `BODY_SIZE`; al leer se rellena con ceros, así que el round-trip y el
/// `content_hash` son idénticos. Un body todo ceros se recorta a vacío.
fn trim_trailing_zeros(body: &[u8]) -> &[u8] {
    let end = body.iter().rposition(|&b| b != 0).map_or(0, |p| p + 1);
    &body[..end]
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
        Self::create_with_crypto(path, key.is_some(), provider_for(key), None, 0)
    }

    /// Núcleo de la creación, parametrizado por el proveedor cripto y el
    /// compresor ya construidos (M9/M10). Lo usa `vacuum` para crear el archivo
    /// temporal con el **mismo** proveedor (misma clave) y la **misma**
    /// compresión sin volver a manejar la `Key` cruda, o con cripto nueva
    /// (rotación de clave). Genera una identidad de archivo nueva: el archivo
    /// compactado arranca una cadena de auditoría fresca.
    pub(crate) fn create_with_crypto(
        path: &Path,
        encrypted: bool,
        crypto: Arc<dyn CryptoProvider>,
        compressor: Option<Arc<dyn Compressor>>,
        ecc_nsym: u8,
    ) -> Result<Pager> {
        let file = DbFile::create_new(path)?;
        if !file.try_lock_exclusive()? {
            return Err(Error::Busy);
        }

        let mut file_id = [0u8; 16];
        getrandom::fill(&mut file_id).map_err(|e| Error::Io(std::io::Error::other(e)))?;
        let mut kdf_salt = [0u8; 16];
        getrandom::fill(&mut kdf_salt).map_err(|e| Error::Io(std::io::Error::other(e)))?;
        let flags = (if encrypted { FLAG_ENCRYPTED } else { 0 })
            | (if compressor.is_some() {
                FLAG_COMPRESSED
            } else {
                0
            });
        let header = FileHeader {
            flags,
            file_id,
            kdf_salt,
            ecc_nsym,
        };

        let pager = Pager {
            file,
            header,
            plain: PlainProvider,
            crypto,
            compressor,
            cache: Mutex::new(PageCache::new(CACHE_CAP)),
            state: Mutex::new(AppendState {
                next_page: FIRST_DATA_PAGE.0,
                nonce_counter: 0,
                write_offset: FIRST_DATA_PAGE.byte_offset(),
                dir: Vec::new(),
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
        // Compresión activa (v2, M10): el bit del header fija qué backend abre las
        // páginas. v1.x tiene un único backend (Lz); un segundo añadiría un id en
        // el header. Off ⇒ formato sin byte de método (idéntico a A2).
        let compressor: Option<Arc<dyn Compressor>> =
            (header.flags & FLAG_COMPRESSED != 0).then(|| Arc::new(Lz) as Arc<dyn Compressor>);

        let pager = Pager {
            file,
            header,
            plain: PlainProvider,
            crypto,
            compressor,
            cache: Mutex::new(PageCache::new(CACHE_CAP)),
            state: Mutex::new(AppendState {
                next_page: FIRST_DATA_PAGE.0,
                nonce_counter: 0,
                write_offset: FIRST_DATA_PAGE.byte_offset(),
                dir: Vec::new(),
            }),
        };
        pager.read_meta()?;
        // Reconstruye el directorio de páginas (v2, M10) barriendo los prefijos
        // de longitud de la zona append; fija `next_page` y `write_offset` al
        // extremo físico (incluida la cola rota, que recover/`truncate_to_head`
        // descartan después).
        pager.sweep_append_region(len)?;

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

    /// Barrido de la zona append (v2, M10): camina los registros
    /// `[u32 len][payload]` en orden de id desde `FIRST_DATA_PAGE`, anotando cada
    /// localización en el directorio. Para en el primer registro incompleto o con
    /// `len` imposible (la cola rota): truncado lógico. No descifra —los prefijos
    /// son texto plano—; la validación de cada página es perezosa (`read_page`) y
    /// la de la cadena la hace `recover`. Coste O(páginas); el directorio
    /// persistido (Slice D) lo bajará a O(log n).
    fn sweep_append_region(&self, file_len: u64) -> Result<()> {
        let mut state = self.state.lock().expect("estado del pager envenenado");
        let mut offset = FIRST_DATA_PAGE.byte_offset();
        loop {
            if offset + LEN_PREFIX_LEN as u64 > file_len {
                break;
            }
            let mut prefix = [0u8; LEN_PREFIX_LEN];
            if self.file.read_exact_at(&mut prefix, offset).is_err() {
                break;
            }
            let stored_len = u32::from_le_bytes(prefix);
            let payload = offset + LEN_PREFIX_LEN as u64;
            // La paridad ECC sigue al payload; su longitud se deriva del header.
            let nsym = self.header.ecc_nsym as usize;
            let plen = if nsym > 0 {
                ecc::parity_len(stored_len as usize, nsym) as u64
            } else {
                0
            };
            // Un payload menor que la reserva cripto, o un registro (payload +
            // paridad) que no cabe en el archivo, marca el inicio de la cola rota.
            if (stored_len as usize) < CRYPTO_RESERVE
                || payload + stored_len as u64 + plen > file_len
            {
                break;
            }
            state.dir.push(PhysLoc {
                offset: payload,
                len: stored_len,
            });
            offset = payload + stored_len as u64 + plen;
        }
        state.next_page = FIRST_DATA_PAGE.0 + state.dir.len() as u64;
        state.write_offset = offset;
        Ok(())
    }

    /// Añade una página a la zona append como un registro enmarcado de longitud
    /// variable (v2, M10). `build` rellena el body en claro; al salir hacia el
    /// disco se recorta la cola de ceros, se sella y se enmarca. No hace fsync:
    /// el llamador agrupa appends y llama a `sync` (protocolo de commit, M1).
    pub fn append_page(&self, build: impl FnOnce(&mut [u8])) -> Result<PageId> {
        let mut page = PageBuf::zeroed();
        build(page.body_mut());
        let cached = Arc::new(page.clone());

        let mut state = self.state.lock().expect("estado del pager envenenado");
        let id = PageId(state.next_page);
        let offset = state.write_offset;
        let to_seal = self.body_to_seal(trim_trailing_zeros(page.body()));
        let stored = self.crypto.seal_bytes(&to_seal, id, state.nonce_counter);
        let framed = self.framed_record(&stored);
        self.file.write_all_at(&framed, offset)?;
        state.dir.push(PhysLoc {
            offset: offset + LEN_PREFIX_LEN as u64,
            len: stored.len() as u32,
        });
        state.next_page += 1;
        state.nonce_counter += 1;
        state.write_offset = offset + framed.len() as u64;
        drop(state);

        self.cache
            .lock()
            .expect("caché envenenada")
            .insert(id, cached);
        Ok(id)
    }

    /// Lee una página de la zona append, verificada y cacheada. Resuelve la
    /// posición física por el directorio (v2, M10), lee el payload sellado, lo
    /// abre y reconstruye el body lógico rellenando con ceros la cola recortada.
    pub fn read_page(&self, id: PageId) -> Result<Arc<PageBuf>> {
        debug_assert!(
            id >= FIRST_DATA_PAGE,
            "las páginas estructurales no pasan por read_page"
        );
        let loc = {
            let state = self.state.lock().expect("estado del pager envenenado");
            if id.0 >= state.next_page {
                return Err(Error::Corrupt {
                    page: id.0,
                    reason: "página fuera de rango",
                });
            }
            state.loc(id).ok_or(Error::Corrupt {
                page: id.0,
                reason: "página sin entrada en el directorio",
            })?
        };
        if let Some(p) = self.cache.lock().expect("caché envenenada").get(id) {
            return Ok(p);
        }

        let mut stored = vec![0u8; loc.len as usize];
        self.file.read_exact_at(&mut stored, loc.offset)?;
        let body = self.decode_body(self.open_with_ecc(&stored, id, loc)?, id)?;
        let mut page = PageBuf::zeroed();
        page.body_mut()[..body.len()].copy_from_slice(&body);
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

    /// Registro enmarcado de un payload sellado: `[u32 len][payload]` y, si el
    /// ECC está activo, la paridad RS del payload a continuación (v2, M10). La
    /// paridad protege los **bytes finales** (lo que sufre bit-rot); su longitud
    /// se deriva del `len` y `ecc_nsym`, así que no se guarda en el marco.
    fn framed_record(&self, stored: &[u8]) -> Vec<u8> {
        let mut out = frame(stored);
        let nsym = self.header.ecc_nsym as usize;
        if nsym > 0 {
            out.extend_from_slice(&ecc::parity(stored, nsym));
        }
        out
    }

    /// Abre el payload sellado de una página; si falla la autenticación y el ECC
    /// está activo, lee la paridad e intenta **corregir** los bytes corruptos
    /// antes de reintentar (M10, estabilidad #3). Camino rápido (sin corrupción):
    /// una sola lectura y `open`, sin tocar la paridad. Si la corrección no cabe
    /// en el presupuesto, falla limpio con el error original (nunca dato malo).
    fn open_with_ecc(&self, payload: &[u8], id: PageId, loc: PhysLoc) -> Result<Vec<u8>> {
        match self.crypto.open_bytes(payload, id) {
            Ok(body) => Ok(body),
            Err(e) => {
                let nsym = self.header.ecc_nsym as usize;
                if nsym == 0 {
                    return Err(e);
                }
                let plen = ecc::parity_len(payload.len(), nsym);
                let mut parity = vec![0u8; plen];
                self.file
                    .read_exact_at(&mut parity, loc.offset + loc.len as u64)?;
                let fixed = ecc::correct(payload, &parity, nsym).ok_or(e)?;
                // Reverifica el tag sobre los bytes corregidos: si aún no abre,
                // el daño excedía lo corregible ⇒ Corrupt (jamás dato plausible).
                self.crypto.open_bytes(&fixed, id)
            }
        }
    }

    /// Bytes a sellar para una página de datos (v2, M10): el body recortado tal
    /// cual si la compresión está off, o `[método][payload]` (comprimido si
    /// ahorra, crudo si no — nunca inflar) si está on. Transparente para
    /// `content_hash`/`verify`: el body lógico se reconstruye idéntico al leer.
    fn body_to_seal<'a>(&self, trimmed: &'a [u8]) -> Cow<'a, [u8]> {
        let Some(c) = &self.compressor else {
            return Cow::Borrowed(trimmed);
        };
        let (method, payload) = match c.compress(trimmed) {
            Some(comp) => (METHOD_LZ, comp),
            None => (METHOD_RAW, trimmed.to_vec()),
        };
        let mut out = Vec::with_capacity(1 + payload.len());
        out.push(method);
        out.extend_from_slice(&payload);
        Cow::Owned(out)
    }

    /// Inverso de `body_to_seal`: del payload abierto al body lógico recortado.
    fn decode_body(&self, opened: Vec<u8>, id: PageId) -> Result<Vec<u8>> {
        let Some(c) = &self.compressor else {
            return Ok(opened);
        };
        let (&method, payload) = opened.split_first().ok_or(Error::Corrupt {
            page: id.0,
            reason: "página de datos comprimida vacía",
        })?;
        match method {
            METHOD_RAW => Ok(payload.to_vec()),
            METHOD_LZ => c.decompress(payload).ok_or(Error::Corrupt {
                page: id.0,
                reason: "descompresión de página fallida",
            }),
            _ => Err(Error::Corrupt {
                page: id.0,
                reason: "método de compresión desconocido",
            }),
        }
    }

    /// Sella, enmarca y escribe `body` (claro) para la página lógica `id` en
    /// `offset` (v2, M10). Devuelve `(longitud total del registro, PhysLoc)`.
    /// Avanza el contador de nonce pero **no** toca el directorio ni
    /// `write_offset`: el commit los instala atómicamente con
    /// [`install_commit`](Pager::install_commit) tras el fsync de durabilidad.
    /// Un fallo a mitad de commit deja las escrituras como cola rota (sin estado
    /// en memoria que revertir): el próximo commit las pisa.
    pub(crate) fn write_record_at(
        &self,
        id: PageId,
        body: &[u8],
        offset: u64,
    ) -> Result<(u64, PhysLoc)> {
        debug_assert!(id >= FIRST_DATA_PAGE, "registro fuera de la zona append");
        let counter = {
            let mut state = self.state.lock().expect("estado del pager envenenado");
            let c = state.nonce_counter;
            state.nonce_counter += 1;
            c
        };
        let to_seal = self.body_to_seal(trim_trailing_zeros(body));
        let stored = self.crypto.seal_bytes(&to_seal, id, counter);
        let framed = self.framed_record(&stored);
        self.file.write_all_at(&framed, offset)?;
        let loc = PhysLoc {
            offset: offset + LEN_PREFIX_LEN as u64,
            len: stored.len() as u32,
        };
        Ok((framed.len() as u64, loc))
    }

    /// Offset físico donde se añadirá el próximo registro (base de un commit).
    pub(crate) fn write_offset(&self) -> u64 {
        self.state
            .lock()
            .expect("estado del pager envenenado")
            .write_offset
    }

    /// Instala atómicamente el resultado de un commit (escritor único): añade las
    /// entradas de directorio en orden de id, avanza `write_offset` y publica
    /// `next_page`. Hasta esta llamada las páginas recién escritas viven solo en
    /// la caché; una cola rota anterior queda lógicamente truncada al publicar.
    pub(crate) fn install_commit(
        &self,
        entries: &[(PageId, PhysLoc)],
        write_offset: u64,
        n_pages: u64,
    ) {
        let mut state = self.state.lock().expect("estado del pager envenenado");
        for &(id, loc) in entries {
            debug_assert_eq!(
                id.0,
                FIRST_DATA_PAGE.0 + state.dir.len() as u64,
                "el directorio es append-only en orden de id"
            );
            state.dir.push(loc);
        }
        state.write_offset = write_offset;
        state.next_page = n_pages;
    }

    /// Recorta el estado físico al head recuperado, descartando la cola rota: el
    /// directorio se queda con las páginas de `head`, `write_offset` apunta al fin
    /// del head y `next_page = head.n_pages`. El próximo append sobrescribe la
    /// cola rota (truncado lógico, M1).
    pub(crate) fn truncate_to_head(&self, head: &Head) {
        let mut state = self.state.lock().expect("estado del pager envenenado");
        let keep = head.n_pages.saturating_sub(FIRST_DATA_PAGE.0) as usize;
        let nsym = self.header.ecc_nsym as usize;
        let write_offset = match keep.checked_sub(1).and_then(|i| state.dir.get(i)) {
            // Fin del último registro retenido = payload + su paridad ECC.
            Some(last) => {
                let plen = if nsym > 0 {
                    ecc::parity_len(last.len as usize, nsym) as u64
                } else {
                    0
                };
                last.offset + last.len as u64 + plen
            }
            None => FIRST_DATA_PAGE.byte_offset(),
        };
        state.dir.truncate(keep);
        state.write_offset = write_offset;
        state.next_page = head.n_pages;
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

    /// Longitud física del archivo en bytes (EOF físico). Acota la cola rota en
    /// bytes para el margen de nonce de la recuperación (R7).
    pub(crate) fn byte_len(&self) -> Result<u64> {
        self.file.byte_len().map_err(Error::from)
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

    /// Proveedor cripto activo, para crear un archivo hermano con la **misma**
    /// clave (vacuum manteniendo el cifrado, M9).
    pub(crate) fn crypto(&self) -> Arc<dyn CryptoProvider> {
        self.crypto.clone()
    }

    /// Compresor activo (clon del `Arc`), para que `vacuum` cree el archivo
    /// compactado con la **misma** compresión que el original (M10).
    pub(crate) fn compressor(&self) -> Option<Arc<dyn Compressor>> {
        self.compressor.clone()
    }

    /// `true` si el archivo está cifrado (bit `FLAG_ENCRYPTED`).
    pub fn is_encrypted(&self) -> bool {
        self.header.flags & FLAG_ENCRYPTED != 0
    }

    #[cfg(test)]
    fn cache_len(&self) -> usize {
        self.cache.lock().expect("caché envenenada").map.len()
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::os::unix::fs::FileExt;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::format::{BODY_SIZE, PAGE_SIZE};

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
    fn page_cache_evicts_oldest_and_stays_bounded() {
        let mut c = PageCache::new(100);
        for i in 0..1000u64 {
            c.insert(PageId(i), Arc::new(PageBuf::zeroed()));
        }
        assert!(c.map.len() <= 100, "len {} > tope 100", c.map.len());
        // Las recién insertadas siguen; las primeras se evictaron.
        assert!(c.get(PageId(999)).is_some());
        assert!(c.get(PageId(0)).is_none());
    }

    #[test]
    fn cache_eviction_preserves_data_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        let pager = Pager::create(&path).unwrap();

        // Más páginas que el tope ⇒ se evicta durante los appends.
        let n = CACHE_CAP as u64 + 300;
        let ids: Vec<PageId> = (0..n)
            .map(|i| pager.append_page(pattern(i)).unwrap())
            .collect();
        pager.sync().unwrap();
        assert!(pager.cache_len() <= CACHE_CAP);

        // Todas se leen correctas: las evictadas, releídas del disco.
        for (i, id) in ids.iter().enumerate() {
            let page = pager.read_page(*id).unwrap();
            let mut expected = vec![0u8; BODY_SIZE];
            pattern(i as u64)(&mut expected);
            assert_eq!(page.body(), &expected[..], "página {i} difiere");
        }
        assert!(pager.cache_len() <= CACHE_CAP, "la caché no quedó acotada");
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
            ecc_nsym: 0,
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
