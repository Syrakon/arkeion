//! Tests de integración del motor KV (hito M1, criterios en docs/06-hitos.md).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use arkeion::commit::CommitHeader;
use arkeion::format::{CRYPTO_RESERVE, MetaSlot, PAGE_SIZE};
use arkeion::tx::Store;

fn db(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("t.arkeion")
}

/// xorshift* determinista: misma semilla, misma secuencia, sin dependencias.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn full_state(store: &Store) -> BTreeMap<Vec<u8>, Vec<u8>> {
    store
        .snapshot()
        .scan()
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

#[test]
fn put_get_persist_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = db(&dir);

    let store = Store::create(&path).unwrap();
    let mut tx = store.begin().unwrap();
    tx.put(b"hola", b"mundo").unwrap();
    tx.put(b"adios", b"luna").unwrap();
    assert_eq!(tx.commit().unwrap(), 1);
    drop(store);

    let store = Store::open(&path).unwrap();
    assert_eq!(store.version(), 1);
    let snap = store.snapshot();
    assert_eq!(snap.get(b"hola").unwrap().unwrap(), b"mundo");
    assert_eq!(snap.get(b"adios").unwrap().unwrap(), b"luna");
    assert_eq!(snap.get(b"nada").unwrap(), None);
}

#[test]
fn rollback_discards_everything() {
    let dir = tempfile::tempdir().unwrap();
    let path = db(&dir);
    let store = Store::create(&path).unwrap();

    let mut tx = store.begin().unwrap();
    tx.put(b"persistida", b"si").unwrap();
    tx.commit().unwrap();

    let mut tx = store.begin().unwrap();
    tx.put(b"fantasma", b"no").unwrap();
    tx.delete(b"persistida").unwrap();
    drop(tx); // rollback implícito

    let snap = store.snapshot();
    assert_eq!(snap.get(b"fantasma").unwrap(), None);
    assert_eq!(snap.get(b"persistida").unwrap().unwrap(), b"si");
    assert_eq!(store.version(), 1);

    // Y tras reabrir, idéntico (el rollback no tocó nada durable).
    // Un snapshot vivo retiene el archivo abierto (Arc<Pager>): soltarlo.
    drop(snap);
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(store.version(), 1);
    assert_eq!(store.snapshot().get(b"fantasma").unwrap(), None);
}

#[test]
fn big_values_overflow_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = db(&dir);
    let store = Store::create(&path).unwrap();

    let big: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    let medium: Vec<u8> = vec![0xCD; 12_000];
    let mut tx = store.begin().unwrap();
    tx.put(b"big", &big).unwrap();
    tx.put(b"medium", &medium).unwrap();
    tx.put(b"small", b"x").unwrap();
    tx.commit().unwrap();
    drop(store);

    let store = Store::open(&path).unwrap();
    let snap = store.snapshot();
    assert_eq!(snap.get(b"big").unwrap().unwrap(), big);
    assert_eq!(snap.get(b"medium").unwrap().unwrap(), medium);
    assert_eq!(snap.get(b"small").unwrap().unwrap(), b"x");
}

#[test]
fn large_tree_multilevel() {
    let dir = tempfile::tempdir().unwrap();
    let path = db(&dir);
    let store = Store::create(&path).unwrap();

    const N: u32 = 3000;
    let val = |i: u32| -> Vec<u8> {
        let mut v = vec![0u8; 500];
        v[..4].copy_from_slice(&i.to_le_bytes());
        v
    };
    let mut tx = store.begin().unwrap();
    for i in 0..N {
        tx.put(format!("k{i:08}").as_bytes(), &val(i)).unwrap();
    }
    tx.commit().unwrap();

    // Borrar la mitad par en otra tx.
    let mut tx = store.begin().unwrap();
    for i in (0..N).step_by(2) {
        assert!(tx.delete(format!("k{i:08}").as_bytes()).unwrap());
    }
    tx.commit().unwrap();
    drop(store);

    let store = Store::open(&path).unwrap();
    let all: Vec<_> = store
        .snapshot()
        .scan()
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(all.len(), (N / 2) as usize);
    for (j, (k, v)) in all.iter().enumerate() {
        let i = 2 * j as u32 + 1; // sobreviven los impares
        assert_eq!(k, format!("k{i:08}").as_bytes());
        assert_eq!(v, &val(i));
    }
}

#[test]
fn property_against_btreemap() {
    // Operaciones aleatorias deterministas; el estado tras cada commit debe
    // ser idéntico al de un BTreeMap de referencia, también tras reabrir.
    for seed in [0xA11CE5EEDu64, 0xB0CA00, 0xC0FFEE42] {
        let dir = tempfile::tempdir().unwrap();
        let path = db(&dir);
        let mut rng = Rng(seed);
        let mut reference: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut store = Store::create(&path).unwrap();

        let key_pool: Vec<Vec<u8>> = (0..48)
            .map(|i| {
                let len = 1 + rng.below(40) as usize;
                let mut k = format!("p{i:02}-").into_bytes();
                k.extend((0..len).map(|_| b'a' + rng.below(26) as u8));
                k
            })
            .collect();
        let value_sizes = [0usize, 1, 13, 400, 1100, 2000, 6000];

        for round in 0..40 {
            let mut tx = store.begin().unwrap();
            let mut model = reference.clone();
            for _ in 0..(1 + rng.below(25)) {
                let key = &key_pool[rng.below(key_pool.len() as u64) as usize];
                if rng.below(100) < 70 {
                    let size = value_sizes[rng.below(value_sizes.len() as u64) as usize];
                    let mut val = vec![0u8; size];
                    for b in val.iter_mut() {
                        *b = rng.next() as u8;
                    }
                    tx.put(key, &val).unwrap();
                    model.insert(key.clone(), val);
                } else {
                    let existed = tx.delete(key).unwrap();
                    assert_eq!(existed, model.remove(key).is_some(), "seed {seed:#x}");
                }
                // La tx ve sus propias escrituras.
                let probe = &key_pool[rng.below(key_pool.len() as u64) as usize];
                assert_eq!(tx.get(probe).unwrap(), model.get(probe).cloned());
            }
            if rng.below(100) < 75 {
                tx.commit().unwrap();
                reference = model;
            } else {
                drop(tx); // rollback: el modelo no avanza
            }
            assert_eq!(
                full_state(&store),
                reference,
                "seed {seed:#x} ronda {round}"
            );

            if rng.below(100) < 20 {
                drop(store);
                store = Store::open(&path).unwrap();
                assert_eq!(
                    full_state(&store),
                    reference,
                    "tras reabrir, seed {seed:#x}"
                );
            }
        }
    }
}

type KvState = BTreeMap<Vec<u8>, Vec<u8>>;

/// Construye una base con varios commits y devuelve (bytes del archivo,
/// estados esperados por versión, fin en bytes de cada commit).
fn build_committed_db(path: &Path) -> (Vec<u8>, Vec<KvState>, Vec<(u64, u64)>) {
    let store = Store::create(path).unwrap();
    let mut rng = Rng(0xDEC0DE);
    let mut states = vec![BTreeMap::new()]; // estado de la versión 0: vacío
    let mut commit_ends = Vec::new(); // (versión, fin del commit en bytes)

    for c in 0..4u32 {
        let mut tx = store.begin().unwrap();
        let mut model = states.last().unwrap().clone();
        for i in 0..25u32 {
            let key = format!("c{c}-k{:02}", rng.below(40)).into_bytes();
            if rng.below(100) < 75 {
                let size = if i % 7 == 0 {
                    5000
                } else {
                    30 + rng.below(200) as usize
                };
                let mut val = vec![0u8; size];
                for b in val.iter_mut() {
                    *b = rng.next() as u8;
                }
                tx.put(&key, &val).unwrap();
                model.insert(key, val);
            } else {
                tx.delete(&key).unwrap();
                model.remove(&key);
            }
        }
        let version = tx.commit().unwrap();
        states.push(model);
        // La página de commit es la última escritura: el final del commit es
        // exactamente la longitud del archivo en este momento.
        commit_ends.push((version, std::fs::metadata(path).unwrap().len()));
    }
    drop(store);
    (std::fs::read(path).unwrap(), states, commit_ends)
}

#[test]
fn crash_truncation_never_loses_a_commit_nor_resurrects_one() {
    // Cualquier truncamiento del archivo debe recuperar exactamente el último
    // commit cuyo final quepa en la longitud truncada.
    //
    // Cobertura exhaustiva por clases de equivalencia: truncar en cualquier
    // punto INTERIOR de una página deja esa página incompleta e ilegible —
    // el comportamiento solo cambia en las fronteras de página. Probamos
    // cada frontera y tres offsets interiores por página: equivale a probar
    // todos los offsets posibles.
    let dir = tempfile::tempdir().unwrap();
    let path = db(&dir);
    let (bytes, states, commit_ends) = build_committed_db(&path);
    let total_pages = bytes.len() / PAGE_SIZE;
    assert_eq!(bytes.len() % PAGE_SIZE, 0);
    assert_eq!(commit_ends.len(), 4);

    let mut cuts: Vec<u64> = Vec::new();
    for p in 3..=total_pages as u64 {
        let base = p * PAGE_SIZE as u64;
        cuts.push(base);
        if p < total_pages as u64 {
            cuts.extend([base + 1, base + 2048, base + PAGE_SIZE as u64 - 1]);
        }
    }

    let work = dir.path().join("cut.arkeion");
    for &cut in &cuts {
        let expected_version = commit_ends
            .iter()
            .filter(|(_, end)| *end <= cut)
            .map(|(v, _)| *v)
            .max()
            .unwrap_or(0);

        std::fs::write(&work, &bytes[..cut as usize]).unwrap();
        let store = Store::open(&work).unwrap();
        assert_eq!(
            store.version(),
            expected_version,
            "truncado en {cut}: versión recuperada incorrecta"
        );
        assert_eq!(
            full_state(&store),
            states[expected_version as usize],
            "truncado en {cut}: estado distinto del commit {expected_version}"
        );
        drop(store);
        std::fs::remove_file(&work).unwrap();
    }
}

#[test]
fn recovery_scans_past_stale_meta_slots() {
    // Los meta slots son una pista, no la verdad: su escritura es lazy y
    // pueden quedar viejos. Caso extremo: degradarlos a versión 0 («sin
    // commits») y comprobar que el escaneo hacia delante, encadenando desde
    // génesis, recupera los 4 commits igualmente.
    use arkeion::crypto::{CryptoProvider, PlainProvider};
    use arkeion::format::{PageBuf, PageId};

    let dir = tempfile::tempdir().unwrap();
    let path = db(&dir);
    let (mut bytes, states, _) = build_committed_db(&path);

    let slot = MetaSlot {
        version: 0,
        last_commit_page: PageId(0),
        n_pages: 3,
    };
    for page_id in [PageId(1), PageId(2)] {
        let mut p = PageBuf::zeroed();
        slot.encode_into(p.body_mut());
        PlainProvider.seal(&mut p, page_id, 0);
        let off = page_id.0 as usize * PAGE_SIZE;
        bytes[off..off + PAGE_SIZE].copy_from_slice(p.as_bytes());
    }
    let work = dir.path().join("stale.arkeion");
    std::fs::write(&work, &bytes).unwrap();

    let store = Store::open(&work).unwrap();
    assert_eq!(store.version(), 4, "el escaneo debía adoptar los 4 commits");
    assert_eq!(full_state(&store), states[4]);
}

#[test]
fn hash_chain_is_linked_and_externally_readable() {
    // Lee el archivo SIN el motor (formato público, docs/02): localiza el
    // head por el meta slot y recorre la cadena hasta génesis verificando
    // versión, enlaces prev_chain y autoconsistencia de chain_hash.
    let dir = tempfile::tempdir().unwrap();
    let path = db(&dir);
    let (bytes, _, commit_ends) = build_committed_db(&path);

    let body_of = |page: u64| -> &[u8] {
        let off = page as usize * PAGE_SIZE;
        &bytes[off + CRYPTO_RESERVE..off + PAGE_SIZE]
    };

    let meta = [1u64, 2]
        .into_iter()
        .filter_map(|p| MetaSlot::decode(body_of(p)))
        .max_by_key(|m| m.version)
        .expect("algún meta slot válido");
    assert_eq!(meta.version, 4);

    let mut page = meta.last_commit_page.0;
    let mut expected_version = 4u64;
    let mut child_prev_chain: Option<[u8; 32]> = None;
    while page != 0 {
        let h = CommitHeader::decode(body_of(page)).expect("página de commit válida");
        assert_eq!(h.version, expected_version);
        assert_eq!(h.branch, "main");
        assert_eq!(
            h.chain_hash,
            h.compute_chain(),
            "chain_hash autoconsistente"
        );
        if let Some(prev) = child_prev_chain {
            assert_eq!(prev, h.chain_hash, "el hijo enlaza el chain_hash del padre");
        }
        child_prev_chain = Some(h.prev_chain);
        page = h.prev_page;
        expected_version -= 1;
    }
    assert_eq!(expected_version, 0, "la cadena llega hasta génesis");
    assert_eq!(commit_ends.len(), 4);
}

#[test]
fn concurrent_readers_during_writes() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let dir = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(Store::create(&db(&dir)).unwrap());
    let stop = std::sync::Arc::new(AtomicBool::new(false));

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let store = store.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut last_version = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let snap = store.snapshot();
                    assert!(snap.version() >= last_version, "la versión nunca retrocede");
                    last_version = snap.version();
                    // Un snapshot es internamente consistente: claves
                    // contiguas escritas en el mismo commit.
                    let state: BTreeMap<_, _> = snap.scan().unwrap().map(|r| r.unwrap()).collect();
                    if let Some(v) = state.get(b"contador".as_slice()) {
                        let n = u64::from_le_bytes(v[..8].try_into().unwrap());
                        // commit i deja: contador=i + filas 0..=i ⇒ i+2 claves.
                        assert_eq!(state.len() as u64, n + 2, "snapshot desgarrado");
                    }
                }
            })
        })
        .collect();

    for i in 0..60u64 {
        let mut tx = store.begin().unwrap();
        tx.put(b"contador", &i.to_le_bytes()).unwrap();
        tx.put(format!("fila-{i:04}").as_bytes(), &[i as u8; 100])
            .unwrap();
        tx.commit().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    for r in readers {
        r.join().unwrap();
    }
    assert_eq!(store.version(), 60);
}
