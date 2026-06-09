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

use arkeion::format::{CRYPTO_RESERVE, FIRST_DATA_PAGE, LEN_PREFIX_LEN};
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

/// Spans `(offset payload, len)` de los registros de la zona append, barriendo el
/// log público v2 `[u32 len][payload]` en orden de id (igual que el pager).
fn record_spans(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut off = FIRST_DATA_PAGE.byte_offset() as usize;
    while off + LEN_PREFIX_LEN <= bytes.len() {
        let len = u32::from_le_bytes(bytes[off..off + LEN_PREFIX_LEN].try_into().unwrap()) as usize;
        let payload = off + LEN_PREFIX_LEN;
        if len < CRYPTO_RESERVE || payload + len > bytes.len() {
            break;
        }
        spans.push((payload, len));
        off = payload + len;
    }
    spans
}

/// Registros históricos: todos menos el último (la página de commit del head, que
/// no es histórica). Incluye las páginas de datos del head, que `verify` revalida
/// vía `content_hash`. Manipular la página de commit del head no daría
/// `ChainBroken` sino una recuperación a una versión previa —eso lo cubren las
/// anclas de auditoría—, por eso se excluye. Cabecera y meta slots (páginas 0..3)
/// no forman parte de la cadena. Solo se manipula el **payload** sellado: tocar el
/// prefijo de longitud es un daño de framing (lo ve el re-open/ancla, no `verify`).
fn historical_records(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut spans = record_spans(bytes);
    spans.pop(); // descarta la página de commit del head
    spans
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
    let records = historical_records(&original);
    assert!(records.len() >= 2, "deberían existir registros históricos");

    // Un byte del payload de cada registro histórico ⇒ ChainBroken.
    for (payload, len) in records {
        let off = payload + len / 2;
        let mut bytes = original.clone();
        bytes[off] ^= 0x01;
        std::fs::write(&path, &bytes).unwrap();

        let result = Database::open(&path, Options::default()).and_then(|db| db.verify());
        assert!(
            matches!(result, Err(Error::ChainBroken { .. })),
            "registro en {payload}: {result:?}"
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
    // Cada byte del payload sellado de cada registro histórico.
    for (payload, len) in historical_records(&original) {
        for off in payload..payload + len {
            let mut bytes = original.clone();
            bytes[off] ^= 0x01;
            std::fs::write(&path, &bytes).unwrap();
            let detected = Database::open(&path, Options::default())
                .and_then(|db| db.verify())
                .is_err();
            assert!(detected, "byte {off} no detectado");
        }
    }
}
