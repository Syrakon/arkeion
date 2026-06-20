//! Integración — full-text search (FTS): `CREATE/DROP FULLTEXT INDEX`,
//! mantenimiento del índice en insert/update/delete/bulk, y **búsqueda `MATCH`**
//! con resultados reales (booleanos, prefijo, frase, NEAR, filtro de columna,
//! negación y combinación con predicados normales). La corrección de postings y
//! stats BM25 se prueba a nivel de catálogo (`src/catalog.rs`).

use arkeion::{Connection, Database, Error, Options, Retention, params};

fn db() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("t.arkeion"), Options::default()).unwrap();
    (dir, db)
}

/// ids (col 0) de una consulta, ordenados (para comparar conjuntos).
fn ids(conn: &Connection, sql: &str) -> Vec<i64> {
    let mut v = ids_ordered(conn, sql);
    v.sort_unstable();
    v
}

/// ids (col 0) **en el orden que devuelve la consulta** (para verificar ORDER BY).
fn ids_ordered(conn: &Connection, sql: &str) -> Vec<i64> {
    conn.query(sql, &[])
        .unwrap()
        .map(|r| {
            let id: i64 = r.unwrap().get(0).unwrap();
            id
        })
        .collect()
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

/// Corpus de correo con un índice FTS sobre (subject, body).
fn mail_corpus(conn: &Connection) {
    conn.execute(
        "CREATE TABLE mail (id INTEGER PRIMARY KEY, subject TEXT, body TEXT, folder TEXT)",
        &[],
    )
    .unwrap();
    conn.execute("CREATE FULLTEXT INDEX fts ON mail (subject, body)", &[])
        .unwrap();
    let rows = [
        ("hola mundo", "el mundo es grande", "inbox"),
        ("adios planeta", "mundo cruel y frio", "spam"),
        ("noticias", "el planeta tierra", "inbox"),
    ];
    for (s, b, f) in rows {
        conn.execute(
            "INSERT INTO mail (subject, body, folder) VALUES (?1, ?2, ?3)",
            &params![s, b, f],
        )
        .unwrap();
    }
}

#[test]
fn match_boolean_prefix_and_negation() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    mail_corpus(&conn);
    let q = |sql: &str| ids(&conn, sql);
    // MATCH busca en todo el índice (subject + body).
    assert_eq!(
        q("SELECT id FROM mail WHERE body MATCH 'mundo'"),
        vec![1, 2]
    );
    assert_eq!(
        q("SELECT id FROM mail WHERE body MATCH 'planeta'"),
        vec![2, 3]
    );
    assert_eq!(
        q("SELECT id FROM mail WHERE body MATCH 'mundo AND planeta'"),
        vec![2]
    );
    assert_eq!(
        q("SELECT id FROM mail WHERE body MATCH 'mundo OR planeta'"),
        vec![1, 2, 3]
    );
    assert_eq!(
        q("SELECT id FROM mail WHERE body MATCH 'mundo NOT planeta'"),
        vec![1]
    );
    assert_eq!(q("SELECT id FROM mail WHERE body MATCH 'mun*'"), vec![1, 2]);
    // NOT MATCH: filas que NO casan (ni en subject ni en body).
    assert_eq!(
        q("SELECT id FROM mail WHERE body NOT MATCH 'mundo'"),
        vec![3]
    );
}

#[test]
fn match_phrase_near_and_column() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    mail_corpus(&conn);
    let q = |sql: &str| ids(&conn, sql);
    assert_eq!(
        q("SELECT id FROM mail WHERE body MATCH '\"el mundo\"'"),
        vec![1]
    );
    assert_eq!(
        q("SELECT id FROM mail WHERE body MATCH 'NEAR(mundo grande, 5)'"),
        vec![1]
    );
    assert!(q("SELECT id FROM mail WHERE body MATCH 'NEAR(mundo grande, 1)'").is_empty());
    // Filtro por columna dentro de la consulta.
    assert_eq!(
        q("SELECT id FROM mail WHERE body MATCH 'subject:mundo'"),
        vec![1]
    );
    assert_eq!(
        q("SELECT id FROM mail WHERE body MATCH 'body:planeta'"),
        vec![3]
    );
}

#[test]
fn match_combines_with_normal_predicates() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    mail_corpus(&conn);
    let q = |sql: &str| ids(&conn, sql);
    // MATCH ANDado con un filtro normal.
    assert_eq!(
        q("SELECT id FROM mail WHERE body MATCH 'mundo' AND folder = 'inbox'"),
        vec![1]
    );
    // MATCH dentro de un OR (el eval per-fila lo maneja en cualquier posición).
    assert_eq!(
        q("SELECT id FROM mail WHERE body MATCH 'planeta' OR folder = 'spam'"),
        vec![2, 3]
    );
}

#[test]
fn snippet_and_highlight_via_sql() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    mail_corpus(&conn);

    // highlight con marcadores explícitos sobre las filas que casan.
    let hls: Vec<String> = conn
        .query(
            "SELECT highlight(body, 'mundo', '<b>', '</b>') FROM mail \
             WHERE body MATCH 'mundo' ORDER BY id",
            &[],
        )
        .unwrap()
        .map(|r| {
            let s: String = r.unwrap().get(0).unwrap();
            s
        })
        .collect();
    assert_eq!(
        hls,
        vec![
            "el <b>mundo</b> es grande".to_string(),
            "<b>mundo</b> cruel y frio".to_string(),
        ]
    );

    // snippet con marcadores por defecto ([ ]).
    let mut rows = conn
        .query("SELECT snippet(body, 'grande') FROM mail WHERE id = 1", &[])
        .unwrap();
    let snip: String = rows.next().unwrap().unwrap().get(0).unwrap();
    assert!(snip.contains("[grande]"), "snippet: {snip}");

    // El primer argumento debe ser una columna FTS.
    let bad = conn
        .query("SELECT highlight(folder, 'x') FROM mail WHERE id = 1", &[])
        .and_then(|rows| rows.map(|r| r.map(|_| ())).collect::<Result<Vec<()>, _>>());
    assert!(matches!(bad, Err(Error::Sql { .. })));
}

#[test]
fn bm25_ranks_by_relevance() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .unwrap();
    conn.execute("CREATE FULLTEXT INDEX f ON docs (body)", &[])
        .unwrap();
    // 1: el término 3 veces en un doc corto → más relevante.
    // 2: una vez en un doc más largo. 3: no aparece.
    for body in [
        "rust rust rust",
        "rust programming language systems",
        "python java go",
    ] {
        conn.execute("INSERT INTO docs (body) VALUES (?1)", &params![body])
            .unwrap();
    }

    // ORDER BY relevancia descendente: el doc 1 antes que el doc 2.
    let ranked = ids_ordered(
        &conn,
        "SELECT id FROM docs WHERE body MATCH 'rust' ORDER BY bm25(body, 'rust') DESC",
    );
    assert_eq!(ranked, vec![1, 2]);

    // Las puntuaciones son positivas y doc1 > doc2.
    let scores: Vec<f64> = conn
        .query(
            "SELECT bm25(body, 'rust') FROM docs WHERE body MATCH 'rust' ORDER BY id",
            &[],
        )
        .unwrap()
        .map(|r| {
            let s: f64 = r.unwrap().get(0).unwrap();
            s
        })
        .collect();
    assert_eq!(scores.len(), 2);
    assert!(
        scores[0] > scores[1] && scores[1] > 0.0,
        "bm25 scores: {scores:?}"
    );
}

#[test]
fn match_on_unindexed_column_errors() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    mail_corpus(&conn);
    // `folder` no está en ningún índice FULLTEXT.
    let result = conn
        .query("SELECT id FROM mail WHERE folder MATCH 'inbox'", &[])
        .and_then(|rows| rows.map(|r| r.map(|_| ())).collect::<Result<Vec<()>, _>>());
    assert!(matches!(result, Err(Error::Sql { .. })));
}

#[test]
fn match_searches_the_past_with_as_of() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE mail (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .unwrap();
    conn.execute("CREATE FULLTEXT INDEX f ON mail (body)", &[])
        .unwrap();
    conn.execute("INSERT INTO mail (body) VALUES ('hola mundo')", &[])
        .unwrap();
    let v1 = conn.version(); // solo 'hola mundo' existe
    conn.execute("INSERT INTO mail (body) VALUES ('adios planeta')", &[])
        .unwrap();

    // Estado actual: ambos términos casan.
    assert_eq!(
        ids(&conn, "SELECT id FROM mail WHERE body MATCH 'mundo'"),
        vec![1]
    );
    assert_eq!(
        ids(&conn, "SELECT id FROM mail WHERE body MATCH 'planeta'"),
        vec![2]
    );

    // AS OF la versión 1: 'planeta' aún no existía (índice versionado) ⇒ vacío;
    // 'mundo' sí estaba. El índice FTS busca en el pasado.
    assert!(
        ids(
            &conn,
            &format!("SELECT id FROM mail WHERE body MATCH 'planeta' AS OF VERSION {v1}")
        )
        .is_empty()
    );
    assert_eq!(
        ids(
            &conn,
            &format!("SELECT id FROM mail WHERE body MATCH 'mundo' AS OF VERSION {v1}")
        ),
        vec![1]
    );
}

#[test]
fn match_survives_vacuum() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE mail (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .unwrap();
    conn.execute("CREATE FULLTEXT INDEX f ON mail (body)", &[])
        .unwrap();
    conn.execute("INSERT INTO mail (body) VALUES ('hola mundo')", &[])
        .unwrap();
    conn.execute("INSERT INTO mail (body) VALUES ('mundo cruel')", &[])
        .unwrap();
    // Compacta el fichero; el índice FTS (keyspace 0x03) va en el mismo árbol.
    db.vacuum(Retention::KeepAll).unwrap();
    assert_eq!(
        ids(&conn, "SELECT id FROM mail WHERE body MATCH 'mundo'"),
        vec![1, 2]
    );
    // Los inserts posteriores al vacuum se mantienen.
    conn.execute("INSERT INTO mail (body) VALUES ('mundo nuevo')", &[])
        .unwrap();
    assert_eq!(
        ids(&conn, "SELECT id FROM mail WHERE body MATCH 'mundo'"),
        vec![1, 2, 3]
    );
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
