//! Parámetros nombrados (`:nombre`, post-M9): enlace por nombre vía
//! `execute_named`/`query_named` y `named_params!`. El parser los convierte a
//! índices posicionales; la API construye el vector desde el binding.

use arkeion::{Database, Options, named_params, params};

fn db() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("t.arkeion"), Options::default()).unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER, label TEXT)",
        &[],
    )
    .unwrap();
    drop(conn);
    (dir, db)
}

#[test]
fn bind_by_name_regardless_of_order() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    // El orden del binding no coincide con el de aparición en el SQL.
    conn.execute_named(
        "INSERT INTO t (a, b, label) VALUES (:x, :y, :s)",
        &named_params! { ":s" => "hola", ":y" => 2, ":x" => 1 },
    )
    .unwrap();

    // Los dos puntos en el binding son opcionales.
    let row = conn
        .query_named(
            "SELECT a, b FROM t WHERE label = :s",
            &named_params! { "s" => "hola" },
        )
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(row.get::<i64>("a").unwrap(), 1);
    assert_eq!(row.get::<i64>("b").unwrap(), 2);
}

#[test]
fn repeated_named_param() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    // El mismo parámetro usado dos veces se enlaza una vez.
    conn.execute_named(
        "INSERT INTO t (a, b) VALUES (:n, :n)",
        &named_params! { ":n" => 9 },
    )
    .unwrap();
    assert_eq!(
        conn.query("SELECT a FROM t WHERE a = b AND a = 9", &[])
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn prepared_statement_with_named_params() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    conn.execute_named(
        "INSERT INTO t (a, label) VALUES (:v, :s)",
        &named_params! { ":v" => 5, ":s" => "x" },
    )
    .unwrap();

    let stmt = conn.prepare("SELECT label FROM t WHERE a = :v").unwrap();
    let s: String = stmt
        .query_named(&named_params! { ":v" => 5 })
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .get("label")
        .unwrap();
    assert_eq!(s, "x");
}

#[test]
fn missing_binding_is_an_error() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    let r = conn.query_named(
        "SELECT a FROM t WHERE a = :missing",
        &named_params! { ":other" => 1 },
    );
    assert!(r.is_err());
}

#[test]
fn mixing_positional_and_named_is_rejected() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    // ?N y :nombre en la misma sentencia: error de parseo.
    assert!(
        conn.query("SELECT a FROM t WHERE a = ?1 AND b = :y", &params![1])
            .is_err()
    );
}
