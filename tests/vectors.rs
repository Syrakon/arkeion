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
fn create_drop_vector_index_via_sql() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE docs (id INTEGER PRIMARY KEY, emb BLOB)", &[])
        .unwrap();
    for vals in ["1.0, 0.0", "0.0, 1.0", "0.9, 0.1"] {
        conn.execute(
            &format!("INSERT INTO docs (emb) VALUES (vector({vals}))"),
            &[],
        )
        .unwrap();
    }
    // Construye el índice IVF (entrena sobre las filas existentes).
    conn.execute(
        "CREATE VECTOR INDEX vi ON docs (emb) USING cosine LISTS 2",
        &[],
    )
    .unwrap();
    // IF NOT EXISTS es idempotente.
    conn.execute("CREATE VECTOR INDEX IF NOT EXISTS vi ON docs (emb)", &[])
        .unwrap();
    // INSERT con el índice activo: el hook lo mantiene sin error.
    conn.execute("INSERT INTO docs (emb) VALUES (vector(0.95, 0.05))", &[])
        .unwrap();
    // El KNN exacto sigue dando el resultado correcto (full scan).
    let knn = ids_ordered(
        &conn,
        "SELECT id FROM docs ORDER BY cosine_distance(emb, vector(1.0, 0.0)) LIMIT 1",
    );
    assert_eq!(knn, vec![1]);
    // DROP, idempotente con IF EXISTS.
    conn.execute("DROP VECTOR INDEX vi", &[]).unwrap();
    conn.execute("DROP VECTOR INDEX IF EXISTS vi", &[]).unwrap();
}

#[test]
fn knn_routes_through_ivf_index() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE docs (id INTEGER PRIMARY KEY, emb BLOB)", &[])
        .unwrap();
    // Cluster A≈(1,0): rowids 1-3. Cluster B≈(0,1): rowids 4-6.
    for vals in [
        "1.0, 0.0",
        "0.95, 0.05",
        "0.9, 0.1",
        "0.0, 1.0",
        "0.05, 0.95",
        "0.1, 0.9",
    ] {
        conn.execute(
            &format!("INSERT INTO docs (emb) VALUES (vector({vals}))"),
            &[],
        )
        .unwrap();
    }
    conn.execute(
        "CREATE VECTOR INDEX vi ON docs (emb) USING cosine LISTS 2",
        &[],
    )
    .unwrap();

    // KNN vía el índice: los 3 vecinos de (1,0) son el cluster A; el primero es
    // la identidad (rowid 1).
    let knn = ids_ordered(
        &conn,
        "SELECT id FROM docs ORDER BY cosine_distance(emb, vector(1.0, 0.0)) LIMIT 3",
    );
    assert_eq!(knn[0], 1);
    let mut sorted = knn.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, vec![1, 2, 3]);

    // PRUEBA de que va por el índice (nprobe=1 escanea solo 1 cluster = 3 docs):
    // aunque pidamos LIMIT 4, el índice solo tiene 3 candidatos. Un full scan
    // habría devuelto 4.
    let four = ids_ordered(
        &conn,
        "SELECT id FROM docs ORDER BY cosine_distance(emb, vector(1.0, 0.0)) LIMIT 4",
    );
    assert_eq!(
        four.len(),
        3,
        "el IVF acota a 1 cluster (nprobe=1) = 3 docs"
    );
}

#[test]
fn probes_controls_recall() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE docs (id INTEGER PRIMARY KEY, emb BLOB)", &[])
        .unwrap();
    for vals in [
        "1.0, 0.0",
        "0.95, 0.05",
        "0.9, 0.1",
        "0.0, 1.0",
        "0.05, 0.95",
        "0.1, 0.9",
    ] {
        conn.execute(
            &format!("INSERT INTO docs (emb) VALUES (vector({vals}))"),
            &[],
        )
        .unwrap();
    }
    // PROBES 2 escanea AMBOS clusters ⇒ los 6 son candidatos: LIMIT 4 da 4 filas
    // (vs PROBES por defecto = 1 cluster = 3, cf. knn_routes_through_ivf_index).
    conn.execute(
        "CREATE VECTOR INDEX vi ON docs (emb) USING cosine LISTS 2 PROBES 2",
        &[],
    )
    .unwrap();
    let four = ids_ordered(
        &conn,
        "SELECT id FROM docs ORDER BY cosine_distance(emb, vector(1.0, 0.0)) LIMIT 4",
    );
    assert_eq!(four.len(), 4, "PROBES 2 escanea todos los clusters");
}

#[test]
fn rebuild_refreshes_the_index() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE docs (id INTEGER PRIMARY KEY, emb BLOB)", &[])
        .unwrap();
    // Construye el índice cuando SOLO existe el cluster A (cerca de (1,0)): k-means
    // coloca ambos centroides dentro de A.
    for vals in ["1.0, 0.0", "0.95, 0.05", "0.9, 0.1"] {
        conn.execute(
            &format!("INSERT INTO docs (emb) VALUES (vector({vals}))"),
            &[],
        )
        .unwrap();
    }
    conn.execute(
        "CREATE VECTOR INDEX vi ON docs (emb) USING cosine LISTS 2",
        &[],
    )
    .unwrap();
    // Llegan filas de un cluster B nuevo (cerca de (0,1)); el hook las asigna al
    // centroide viejo más cercano, sin re-particionar (clustering degradado).
    for vals in ["0.0, 1.0", "0.05, 0.95", "0.1, 0.9"] {
        conn.execute(
            &format!("INSERT INTO docs (emb) VALUES (vector({vals}))"),
            &[],
        )
        .unwrap();
    }
    // REBUILD re-entrena sobre las 6 filas: ahora un centroide cubre B de verdad.
    conn.execute("REBUILD VECTOR INDEX vi", &[]).unwrap();
    // Una consulta dentro de B alcanza a sus 3 vecinos exactos (rowids 4-6) por el
    // índice, con el idéntico (rowid 4) primero.
    let knn = ids_ordered(
        &conn,
        "SELECT id FROM docs ORDER BY cosine_distance(emb, vector(0.0, 1.0)) LIMIT 3",
    );
    assert_eq!(knn[0], 4, "el vecino exacto va primero");
    let mut sorted = knn.clone();
    sorted.sort_unstable();
    assert_eq!(
        sorted,
        vec![4, 5, 6],
        "tras REBUILD el cluster B es alcanzable"
    );
    // REBUILD es idempotente; sobre un índice inexistente es error controlado.
    conn.execute("REBUILD VECTOR INDEX vi", &[]).unwrap();
    assert!(matches!(
        conn.execute("REBUILD VECTOR INDEX nope", &[]),
        Err(Error::Sql { .. })
    ));
}

#[test]
fn vector_index_errors() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER, e BLOB)",
        &[],
    )
    .unwrap();
    // Columna no-BLOB.
    assert!(matches!(
        conn.execute("CREATE VECTOR INDEX v ON t (n)", &[]),
        Err(Error::InvalidInput(_))
    ));
    // Métrica desconocida.
    assert!(matches!(
        conn.execute("CREATE VECTOR INDEX v ON t (e) USING manhattan", &[]),
        Err(Error::Sql { .. })
    ));
    // DROP de un índice inexistente sin IF EXISTS.
    assert!(matches!(
        conn.execute("DROP VECTOR INDEX nope", &[]),
        Err(Error::Sql { .. })
    ));
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
