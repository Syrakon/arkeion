//! Integración — full-text search (FTS), fase 2: plomería SQL de
//! `CREATE/DROP FULLTEXT INDEX` y mantenimiento del índice en
//! insert/update/delete. La corrección de los postings y las stats BM25 se prueba
//! a nivel de catálogo (`src/catalog.rs`); aquí se valida la ruta
//! SQL → exec → tx y el manejo de errores. La verificación por **resultados de
//! búsqueda** llegará con `MATCH` (fases 3–4).

use arkeion::{Database, Error, Options, params};

fn db() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("t.arkeion"), Options::default()).unwrap();
    (dir, db)
}

#[test]
fn create_drop_and_maintenance_via_sql() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE mail (id INTEGER PRIMARY KEY, subject TEXT, body TEXT)",
        &[],
    )
    .unwrap();
    // Una fila previa para forzar el backfill al crear el índice.
    conn.execute(
        "INSERT INTO mail (subject, body) VALUES (?1, ?2)",
        &params!["hola mundo", "el mundo es grande"],
    )
    .unwrap();

    // CREATE con tokenizer explícito; backfillea la fila existente.
    conn.execute(
        "CREATE FULLTEXT INDEX fts_mail ON mail (subject, body) USING unicode",
        &[],
    )
    .unwrap();
    // IF NOT EXISTS es idempotente (no falla por duplicado).
    conn.execute(
        "CREATE FULLTEXT INDEX IF NOT EXISTS fts_mail ON mail (subject, body)",
        &[],
    )
    .unwrap();

    // Con el índice activo, los hooks de insert/update/delete corren sin error.
    conn.execute(
        "INSERT INTO mail (subject, body) VALUES (?1, ?2)",
        &params!["adios mundo", "hasta luego"],
    )
    .unwrap();
    assert_eq!(
        conn.execute(
            "UPDATE mail SET body = ?1 WHERE id = 2",
            &params!["otro cuerpo"]
        )
        .unwrap(),
        1
    );
    assert_eq!(
        conn.execute("DELETE FROM mail WHERE id = 2", &[]).unwrap(),
        1
    );

    // DROP, idempotente con IF EXISTS.
    conn.execute("DROP FULLTEXT INDEX fts_mail", &[]).unwrap();
    conn.execute("DROP FULLTEXT INDEX IF EXISTS fts_mail", &[])
        .unwrap();
}

#[test]
fn default_tokenizer_without_using() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .unwrap();
    // Sin USING ⇒ tokenizer por defecto (unicode); el insert lo mantiene.
    conn.execute("CREATE FULLTEXT INDEX f ON docs (body)", &[])
        .unwrap();
    conn.execute(
        "INSERT INTO docs (body) VALUES (?1)",
        &params!["café Köln Straße"],
    )
    .unwrap();
}

#[test]
fn bulk_insert_maintains_fts() {
    // El camino bulk-load (Connection::bulk_insert) también mantiene el índice.
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .unwrap();
    conn.execute("CREATE FULLTEXT INDEX f ON docs (body)", &[])
        .unwrap();
    let rows = [
        vec![arkeion::Value::Null, arkeion::Value::Text("uno dos".into())],
        vec![arkeion::Value::Null, arkeion::Value::Text("tres".into())],
    ];
    assert_eq!(conn.bulk_insert("docs", rows).unwrap(), 2);
}

#[test]
fn match_is_a_clean_error_until_phase4() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE mail (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .unwrap();
    conn.execute("CREATE FULLTEXT INDEX f ON mail (body)", &[])
        .unwrap();
    conn.execute(
        "INSERT INTO mail (body) VALUES (?1)",
        &params!["hola mundo"],
    )
    .unwrap();
    // MATCH parsea y se evalúa con un error SQL controlado (no pánico ni datos
    // incorrectos) hasta que llegue la ejecución por índice (fase 4).
    let err = match conn.query("SELECT id FROM mail WHERE body MATCH 'mundo'", &[]) {
        Err(e) => e,
        Ok(rows) => rows
            .map(|r| r.map(|_| ()))
            .collect::<Result<Vec<()>, _>>()
            .expect_err("MATCH debería fallar de forma controlada"),
    };
    assert!(matches!(err, Error::Sql { .. }));
}

#[test]
fn fulltext_errors() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER, s TEXT)",
        &[],
    )
    .unwrap();

    // Columna no-TEXT.
    assert!(matches!(
        conn.execute("CREATE FULLTEXT INDEX f ON t (n)", &[]),
        Err(Error::InvalidInput(_))
    ));
    // Tokenizer desconocido.
    assert!(matches!(
        conn.execute("CREATE FULLTEXT INDEX f ON t (s) USING porter", &[]),
        Err(Error::Sql { .. })
    ));
    // Columna inexistente.
    assert!(matches!(
        conn.execute("CREATE FULLTEXT INDEX f ON t (nope)", &[]),
        Err(Error::Sql { .. })
    ));
    // DROP de un índice inexistente sin IF EXISTS.
    assert!(matches!(
        conn.execute("DROP FULLTEXT INDEX nope", &[]),
        Err(Error::Sql { .. })
    ));
    // Nombre duplicado.
    conn.execute("CREATE FULLTEXT INDEX f ON t (s)", &[])
        .unwrap();
    assert!(matches!(
        conn.execute("CREATE FULLTEXT INDEX f ON t (s)", &[]),
        Err(Error::Constraint(_))
    ));
}
