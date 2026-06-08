//! Integración M6 — auditoría tamper-evident: `Database::verify()` da
//! `chain_ok` en una base intacta y `ChainBroken` al manipular una página
//! histórica (criterio "hecho cuando" del hito).
//!
//! El módulo `crypto` ya prueba que *cualquiera* de los 4096 bytes de una
//! página se detecta al abrirla (`detects_flip_of_any_byte`). Aquí se prueba lo
//! complementario: que `verify` recorre y revalida **cada** página histórica,
//! de modo que un tamper en cualquiera de ellas aflora como `ChainBroken`. El
//! barrido byte-a-byte completo (la conjunción literal del criterio) está en
//! `fuzz_every_historical_byte`, marcado `#[ignore]` por coste (~25 s).

use arkeion::format::{FIRST_DATA_PAGE, PAGE_SIZE};
use arkeion::{Database, Error, Options, params};

fn build(path: &std::path::Path, inserts: i64) {
    let db = Database::open(path, Options::default()).unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER NOT NULL)",
        &[],
    )
    .unwrap();
    for i in 0..inserts {
        conn.execute("INSERT INTO t (n) VALUES (?1)", &params![i])
            .unwrap();
    }
}

/// Rango de páginas históricas: de la primera de datos a justo antes de la de
/// commit del head (la última), que no es histórica. Las páginas 0..3 (cabecera
/// y meta slots) no forman parte de la cadena.
fn historical_pages(file_len: usize) -> std::ops::Range<usize> {
    let last = file_len / PAGE_SIZE - 1;
    FIRST_DATA_PAGE.0 as usize..last
}

#[test]
fn verify_is_chain_ok_on_a_clean_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.arkeion");
    build(&path, 5);

    // Recién construida y tras reabrir (auditoría externa).
    let report = Database::open(&path, Options::default())
        .unwrap()
        .verify()
        .unwrap();
    assert!(report.chain_ok);
    assert_eq!(report.head, report.commits);
    assert!(report.commits >= 6); // 1 CREATE + 5 INSERT
}

#[test]
fn tampering_a_historical_page_breaks_the_chain() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.arkeion");
    build(&path, 3);

    let original = std::fs::read(&path).unwrap();
    let pages = historical_pages(original.len());
    assert!(pages.len() >= 2, "deberían existir páginas históricas");

    // Un byte de body en cada página histórica ⇒ ChainBroken.
    for page in pages {
        let off = page * PAGE_SIZE + 100;
        let mut bytes = original.clone();
        bytes[off] ^= 0x01;
        std::fs::write(&path, &bytes).unwrap();

        let result = Database::open(&path, Options::default()).and_then(|db| db.verify());
        assert!(
            matches!(result, Err(Error::ChainBroken { .. })),
            "página {page}: {result:?}"
        );
    }

    // Restaurar el original: vuelve a auditar en verde.
    std::fs::write(&path, &original).unwrap();
    assert!(
        Database::open(&path, Options::default())
            .unwrap()
            .verify()
            .unwrap()
            .chain_ok
    );
}

/// Conjunción literal del criterio del hito: **cualquier** byte de **cualquier**
/// página histórica, al manipularse, se detecta. Costoso (abre+audita por byte);
/// se ejecuta con `cargo test -- --ignored`.
#[test]
#[ignore = "barrido exhaustivo (~25 s); el caso por página corre por defecto"]
fn fuzz_every_historical_byte() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.arkeion");
    build(&path, 1); // mínimo con una página de commit histórica

    let original = std::fs::read(&path).unwrap();
    let pages = historical_pages(original.len());
    let (start, end) = (pages.start * PAGE_SIZE, pages.end * PAGE_SIZE);

    for off in start..end {
        let mut bytes = original.clone();
        bytes[off] ^= 0x01;
        std::fs::write(&path, &bytes).unwrap();
        let detected = Database::open(&path, Options::default())
            .and_then(|db| db.verify())
            .is_err();
        assert!(
            detected,
            "byte {off} (página {}) no detectado",
            off / PAGE_SIZE
        );
    }
}
