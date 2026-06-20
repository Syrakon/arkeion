//! Integración — búsqueda **híbrida**: fusión del ranking léxico (`bm25`, FTS) y
//! el semántico (`cosine_distance`, vectores) por **RRF** (Reciprocal Rank
//! Fusion). RRF fusiona por *rango* en cada ranking, así que evita el problema de
//! escalas (BM25 no acotado vs coseno ∈ [0,2]). Se compone enteramente de SQL ya
//! existente: CTE + window functions + las funciones de FTS y de vectores.

use arkeion::{Connection, Database, Options, params};

fn db() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("t.arkeion"), Options::default()).unwrap();
    (dir, db)
}

fn ids_ordered(conn: &Connection, sql: &str) -> Vec<i64> {
    conn.query(sql, &[])
        .unwrap()
        .map(|r| {
            let id: i64 = r.unwrap().get(0).unwrap();
            id
        })
        .collect()
}

fn corpus(conn: &Connection) {
    conn.execute(
        "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT, emb BLOB)",
        &[],
    )
    .unwrap();
    conn.execute("CREATE FULLTEXT INDEX f ON docs (body)", &[])
        .unwrap();
    // Query: término 'rust' + vector (1,0,0).
    // doc1: léxico FUERTE (4× rust), semántico DÉBIL (ortogonal).
    // doc2: léxico DÉBIL (1× rust), semántico MEDIO (cercano).
    // doc3: léxico MEDIO (2× rust), semántico FUERTE (idéntico) → equilibrado.
    let rows = [
        ("rust rust rust rust", "0.0, 1.0, 0.0"),
        ("rust", "0.9, 0.1, 0.0"),
        ("rust rust", "1.0, 0.0, 0.0"),
    ];
    for (body, vec) in rows {
        conn.execute(
            &format!("INSERT INTO docs (body, emb) VALUES (?1, vector({vec}))"),
            &params![body],
        )
        .unwrap();
    }
}

#[test]
fn bm25_ranks_inside_a_window_function() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    corpus(&conn);
    // ROW_NUMBER por bm25 desc ⇒ orden léxico: doc1 (4×), doc3 (2×), doc2 (1×).
    let lex = ids_ordered(
        &conn,
        "SELECT id, ROW_NUMBER() OVER (ORDER BY bm25(body, 'rust') DESC) AS r \
         FROM docs ORDER BY r",
    );
    assert_eq!(lex, vec![1, 3, 2]);
}

#[test]
fn hybrid_rrf_fuses_lexical_and_semantic() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    corpus(&conn);
    // RRF: rango por bm25 (léxico) + rango por coseno (semántico), fusionados.
    let hybrid = ids_ordered(
        &conn,
        "WITH ranked AS (
           SELECT id,
             ROW_NUMBER() OVER (ORDER BY bm25(body, 'rust') DESC) AS lex,
             ROW_NUMBER() OVER (ORDER BY cosine_distance(emb, vector(1.0, 0.0, 0.0)) ASC) AS sem
           FROM docs
         )
         SELECT id FROM ranked ORDER BY 1.0/(60+lex) + 1.0/(60+sem) DESC",
    );
    // doc3 gana por ser bueno en AMBOS, batiendo al mejor solo-léxico (doc1) y al
    // mejor solo-semántico. Ni el ranking léxico ni el semántico solos lo ponían
    // primero.
    assert_eq!(hybrid, vec![3, 1, 2]);
    assert_ne!(hybrid, vec![1, 3, 2], "no es solo el ranking léxico");
    assert_ne!(hybrid, vec![3, 2, 1], "no es solo el ranking semántico");
}
