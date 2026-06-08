//! Integración M9 — vacuum: compactación con checkpoint y rename atómico,
//! rotación de clave y robustez. Valida de extremo a extremo el criterio "hecho
//! cuando" del hito: tras vacuum `verify()` sigue OK, las versiones retenidas
//! responden `AS OF` (las compactadas dan `VersionNotFound`), el mismo handle y
//! sus conexiones siguen vivos, y la publicación es un reemplazo atómico (inodo
//! nuevo) que deja el original intacto hasta el rename.

use std::os::unix::fs::MetadataExt;

use arkeion::{Database, Error, Key, Options, Retention, params};

/// Base con una tabla y `n` inserciones (1 CREATE + n INSERT ⇒ versión n+1).
fn build(path: &std::path::Path, n: i64) -> Database {
    let db = Database::open(path, Options::default()).unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER NOT NULL)",
        &[],
    )
    .unwrap();
    for i in 0..n {
        conn.execute("INSERT INTO t (n) VALUES (?1)", &params![i])
            .unwrap();
    }
    db
}

/// Número de filas de la tabla en una consulta (con o sin `AS OF`).
fn count(conn: &arkeion::Connection, sql: &str) -> i64 {
    conn.query(sql, &[]).unwrap().count() as i64
}

#[test]
fn vacuum_compacts_keeps_recent_and_verifies() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.arkeion");
    let db = build(&path, 40); // versiones 1..=41
    let conn = db.connect().unwrap();
    let head = conn.version();

    let before = std::fs::metadata(&path).unwrap().len();
    let report = db.vacuum(Retention::KeepLast(5)).unwrap();
    let after = std::fs::metadata(&path).unwrap().len();

    assert_eq!(report.head, head);
    assert_eq!(report.kept_from, head - 4);
    assert!(after < before, "el archivo no encogió: {before} → {after}");
    assert!(report.pages_after < report.pages_before);

    // La cadena sigue íntegra; solo cuenta los commits retenidos.
    let audit = db.verify().unwrap();
    assert!(audit.chain_ok);
    assert_eq!(audit.head, head);
    assert_eq!(audit.commits, 5);

    // El presente (misma conexión, no hizo falta reabrir) ve las 40 filas.
    assert_eq!(count(&conn, "SELECT n FROM t"), 40);

    // Una versión retenida responde AS OF; una compactada da VersionNotFound.
    let kept = head - 2; // dentro de las últimas 5
    assert_eq!(
        count(&conn, &format!("SELECT n FROM t AS OF VERSION {kept}")),
        (kept - 1) as i64, // versión k = CREATE + (k-1) inserts
    );
    let gone = report.kept_from - 1;
    assert!(matches!(
        conn.query(&format!("SELECT n FROM t AS OF VERSION {gone}"), &[]),
        Err(Error::VersionNotFound(_))
    ));
}

#[test]
fn vacuum_replaces_the_inode_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.arkeion");
    let db = build(&path, 10);

    // El reemplazo atómico publica un inodo nuevo: el archivo original nunca se
    // muta en sitio, así que un kill antes del rename lo dejaría intacto.
    let ino_before = std::fs::metadata(&path).unwrap().ino();
    db.vacuum(Retention::KeepLast(2)).unwrap();
    let ino_after = std::fs::metadata(&path).unwrap().ino();
    assert_ne!(ino_before, ino_after, "vacuum no reemplazó el inodo");

    // No quedan temporales sueltos.
    let temp = path.with_file_name("v.arkeion.vacuum-tmp");
    assert!(!temp.exists());
    assert!(db.verify().unwrap().chain_ok);
}

#[test]
fn vacuum_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.arkeion");
    let db = build(&path, 20);
    db.vacuum(Retention::KeepLast(3)).unwrap();
    drop(db);

    // Reabrir el archivo compactado: datos del presente intactos, verify OK,
    // y se puede seguir escribiendo.
    let db = Database::open(&path, Options::default().create_if_missing(false)).unwrap();
    let conn = db.connect().unwrap();
    assert_eq!(count(&conn, "SELECT n FROM t"), 20);
    assert!(db.verify().unwrap().chain_ok);
    conn.execute("INSERT INTO t (n) VALUES (999)", &[]).unwrap();
    assert_eq!(count(&conn, "SELECT n FROM t"), 21);
    assert!(db.verify().unwrap().chain_ok);
}

#[test]
fn vacuum_rekey_rotates_key_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("enc.arkeion");
    let old = || Key::new([0x42; 32]);
    let new = || Key::new([0x7E; 32]);

    let db = Database::open(&path, Options::default().key(old())).unwrap();
    {
        let conn = db.connect().unwrap();
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT)", &[])
            .unwrap();
        conn.execute("INSERT INTO t (s) VALUES ('secreto')", &[])
            .unwrap();
    }

    db.vacuum_rekey(Retention::KeepAll, Some(new())).unwrap();
    assert!(db.verify().unwrap().chain_ok);
    drop(db); // suelta el lock: todo handle (db y conexiones) debe cerrarse

    // La clave vieja ya no abre; la nueva sí, con los datos intactos.
    assert!(matches!(
        Database::open(
            &path,
            Options::default().create_if_missing(false).key(old())
        ),
        Err(Error::WrongKey)
    ));
    let db = Database::open(
        &path,
        Options::default().create_if_missing(false).key(new()),
    )
    .unwrap();
    let conn = db.connect().unwrap();
    let row = conn
        .query("SELECT s FROM t", &[])
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(row.get::<String>("s").unwrap(), "secreto");
    assert!(db.verify().unwrap().chain_ok);
}

#[test]
fn vacuum_is_busy_inside_an_open_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.arkeion");
    let db = build(&path, 3);
    let conn = db.connect().unwrap();

    let tx = conn.begin().unwrap();
    assert!(matches!(db.vacuum(Retention::KeepAll), Err(Error::Busy)));
    drop(tx);
    db.vacuum(Retention::KeepAll).unwrap();
}
