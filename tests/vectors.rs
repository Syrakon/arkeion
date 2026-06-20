//! Integración — búsqueda vectorial (KNN exacto): el constructor `vector()`, las
//! distancias y el KNN por `ORDER BY <distancia> LIMIT k`. El vector es un BLOB
//! de f32; la corrección de las distancias se prueba a nivel de `src/vector.rs`.
//! Ver `docs/13-vectores.md`.

use arkeion::{Connection, Database, Error, Options};

fn db() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("t.arkeion"), Options::default()).unwrap();
    (dir, db)
}

/// ids (col 0) en el orden que devuelve la consulta.
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
    conn.execute("CREATE TABLE docs (id INTEGER PRIMARY KEY, emb BLOB)", &[])
        .unwrap();
    // doc1 = e_x, doc2 = e_y (ortogonal), doc3 ≈ e_x (cercano a doc1).
    for vals in ["1.0, 0.0, 0.0", "0.0, 1.0, 0.0", "0.9, 0.1, 0.0"] {
        conn.execute(
            &format!("INSERT INTO docs (emb) VALUES (vector({vals}))"),
            &[],
        )
        .unwrap();
    }
}

#[test]
fn knn_cosine_orders_by_similarity() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    corpus(&conn);
    // Vecinos de e_x: doc1 (idéntico) y doc3 (cercano) antes que doc2 (ortogonal).
    let knn = ids_ordered(
        &conn,
        "SELECT id FROM docs ORDER BY cosine_distance(emb, vector(1.0, 0.0, 0.0)) LIMIT 2",
    );
    assert_eq!(knn, vec![1, 3]);
    let all = ids_ordered(
        &conn,
        "SELECT id FROM docs ORDER BY cosine_distance(emb, vector(1.0, 0.0, 0.0))",
    );
    assert_eq!(all, vec![1, 3, 2]);
}

#[test]
fn knn_l2_and_dot() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    corpus(&conn);
    // El más cercano a e_x por distancia euclídea es doc1.
    let l2 = ids_ordered(
        &conn,
        "SELECT id FROM docs ORDER BY l2_distance(emb, vector(1.0, 0.0, 0.0)) LIMIT 1",
    );
    assert_eq!(l2, vec![1]);
    // El de mayor producto interno con e_x es doc1 (ORDER BY ... DESC).
    let dot = ids_ordered(
        &conn,
        "SELECT id FROM docs ORDER BY dot(emb, vector(1.0, 0.0, 0.0)) DESC LIMIT 1",
    );
    assert_eq!(dot, vec![1]);
}

#[test]
fn int8_quantized_storage_knn() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE docs (id INTEGER PRIMARY KEY, emb BLOB)", &[])
        .unwrap();
    // Vectores almacenados quantizados (int8) → ~4× menos storage.
    for vals in ["1.0, 0.0, 0.0", "0.0, 1.0, 0.0", "0.9, 0.1, 0.0"] {
        conn.execute(
            &format!("INSERT INTO docs (emb) VALUES (vector_i8({vals}))"),
            &[],
        )
        .unwrap();
    }
    // Query f32 contra almacenados int8 (formatos cruzados): mismo orden KNN.
    let knn = ids_ordered(
        &conn,
        "SELECT id FROM docs ORDER BY cosine_distance(emb, vector(1.0, 0.0, 0.0)) LIMIT 2",
    );
    assert_eq!(knn, vec![1, 3]);
}

#[test]
fn dimension_mismatch_errors() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    corpus(&conn);
    // emb es 3-dim; la query 2-dim ⇒ error controlado (no pánico).
    let r = conn
        .query(
            "SELECT cosine_distance(emb, vector(1.0, 0.0)) FROM docs",
            &[],
        )
        .and_then(|rows| rows.map(|r| r.map(|_| ())).collect::<Result<Vec<()>, _>>());
    assert!(matches!(r, Err(Error::InvalidInput(_))));
}
