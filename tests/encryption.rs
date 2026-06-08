//! Integración M7 — cifrado en reposo: la misma API funciona sobre una base
//! cifrada (CRUD + time-travel + auditoría), el archivo no revela plaintext, y
//! la gestión de clave da `WrongKey`/`KeyRequired` (criterio del hito).

use arkeion::{Connection, Database, Error, Key, Options, params};

fn opts(seed: u8) -> Options {
    Options::default().key(Key::new([seed; 32]))
}

fn clientes(conn: &Connection, sql: &str) -> Vec<String> {
    conn.query(sql, &[])
        .unwrap()
        .map(|r| r.unwrap().get::<String>("cliente").unwrap())
        .collect()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn encrypted_crud_timetravel_and_audit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.arkeion");

    let db = Database::open(&path, opts(7)).unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE facturas (id INTEGER PRIMARY KEY, cliente TEXT NOT NULL, total REAL)",
        &[],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO facturas (cliente, total) VALUES (?1, ?2)",
        &params!["Acme GmbH", 1250.0],
    )
    .unwrap();
    let v1 = conn.version();
    conn.execute(
        "INSERT INTO facturas (cliente, total) VALUES (?1, ?2)",
        &params!["Globex SL", 990.0],
    )
    .unwrap();

    // CRUD normal, time-travel y auditoría, todo sobre cifrado.
    assert_eq!(
        clientes(&conn, "SELECT cliente FROM facturas ORDER BY id"),
        vec!["Acme GmbH", "Globex SL"]
    );
    assert_eq!(
        clientes(
            &conn,
            &format!("SELECT cliente FROM facturas ORDER BY id AS OF VERSION {v1}")
        ),
        vec!["Acme GmbH"]
    );
    assert!(db.verify().unwrap().chain_ok);
    // Soltar conexión y handle: ambos comparten el lock exclusivo del archivo.
    drop(conn);
    drop(db);

    // Reabrir con la clave: datos y cadena intactos.
    let db = Database::open(&path, opts(7)).unwrap();
    let conn = db.connect().unwrap();
    assert_eq!(
        clientes(&conn, "SELECT cliente FROM facturas ORDER BY id"),
        vec!["Acme GmbH", "Globex SL"]
    );
    assert!(db.verify().unwrap().chain_ok);
}

#[test]
fn file_reveals_no_known_plaintext() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.arkeion");
    {
        let db = Database::open(&path, opts(9)).unwrap();
        let conn = db.connect().unwrap();
        conn.execute(
            "CREATE TABLE secretos (id INTEGER PRIMARY KEY, dato TEXT NOT NULL)",
            &[],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO secretos (dato) VALUES (?1)",
            &params!["SECRETO-ABC123"],
        )
        .unwrap();
    }

    let raw = std::fs::read(&path).unwrap();
    assert!(
        !contains(&raw, b"secretos"),
        "el nombre de tabla aflora en claro"
    );
    assert!(
        !contains(&raw, b"SECRETO-ABC123"),
        "el dato de usuario aflora en claro"
    );
}

#[test]
fn wrong_key_is_wrong_key_and_missing_key_is_required() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.arkeion");
    {
        let db = Database::open(&path, opts(1)).unwrap();
        db.connect()
            .unwrap()
            .execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", &[])
            .unwrap();
    }

    // Clave errónea ⇒ WrongKey, nunca datos corruptos ni base vacía.
    assert!(matches!(
        Database::open(&path, opts(2)),
        Err(Error::WrongKey)
    ));

    // Cifrado y sin clave ⇒ KeyRequired.
    assert!(matches!(
        Database::open(&path, Options::default().create_if_missing(false)),
        Err(Error::KeyRequired)
    ));
}

#[test]
fn plaintext_and_encrypted_are_distinct_files() {
    // Una base sin clave sí contiene el plaintext: confirma que el test de
    // no-aparición mide el cifrado, no un artefacto del encoding.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plano.arkeion");
    {
        let db = Database::open(&path, Options::default()).unwrap();
        let conn = db.connect().unwrap();
        conn.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, dato TEXT NOT NULL)",
            &[],
        )
        .unwrap();
        conn.execute("INSERT INTO t (dato) VALUES (?1)", &params!["VISIBLE-XYZ"])
            .unwrap();
    }
    let raw = std::fs::read(&path).unwrap();
    assert!(
        contains(&raw, b"VISIBLE-XYZ"),
        "sin cifrar, el dato debería verse"
    );
}
