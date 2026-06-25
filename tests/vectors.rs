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
fn vector_rerank_int8_via_sql() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    corpus(&conn); // doc1 = e_x, doc2 = e_y (ortogonal), doc3 ≈ e_x
    // RERANK int8: el posting guarda el vector int8 y el re-rank es inline (sin fetch
    // de fila por candidato). PROBES 2 ⇒ se escanean ambos clusters.
    conn.execute(
        "CREATE VECTOR INDEX vi ON docs (emb) USING cosine LISTS 2 PROBES 2 RERANK int8",
        &[],
    )
    .unwrap();
    // ANN por SQL: top-2 cercanos a e_x = doc1 (exacto) y doc3 (≈e_x); doc2 lejos.
    assert_eq!(
        ids_ordered(
            &conn,
            "SELECT id FROM docs ORDER BY cosine_distance(emb, vector(1.0, 0.0, 0.0)) LIMIT 2",
        ),
        vec![1, 3]
    );
    // INSERT con el índice int8 activo: el hook lo mantiene en int8 (sin codebooks PQ).
    conn.execute("INSERT INTO docs (emb) VALUES (vector(0.95, 0.05, 0.0))", &[])
        .unwrap();
    // doc1 (e_x exacto) sigue siendo el más cercano, incluso con la fila nueva.
    assert_eq!(
        ids_ordered(
            &conn,
            "SELECT id FROM docs ORDER BY cosine_distance(emb, vector(1.0, 0.0, 0.0)) LIMIT 1",
        ),
        vec![1]
    );
    // REBUILD conserva el modo int8 y sigue respondiendo correcto.
    conn.execute("REBUILD VECTOR INDEX vi", &[]).unwrap();
    assert_eq!(
        ids_ordered(
            &conn,
            "SELECT id FROM docs ORDER BY cosine_distance(emb, vector(1.0, 0.0, 0.0)) LIMIT 1",
        ),
        vec![1]
    );
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

/// Recall a escala del re-rank int8 inline (#3): con clusters con MUCHOS miembros
/// (donde el shortlist sí trunca y el int8 sí afecta), el top-k del índice IVF debe
/// coincidir casi exacto con el KNN por fuerza bruta. Determinista (datos sembrados).
#[test]
fn ivf_int8_rerank_recall_at_scale() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE docs (id INTEGER PRIMARY KEY, emb BLOB)", &[])
        .unwrap();
    let (nc, per, dim) = (30usize, 12usize, 8usize);
    let mut st = 0x2024u64;
    let mut rnd = || {
        st ^= st << 13;
        st ^= st >> 7;
        st ^= st << 17;
        (st >> 40) as f32 / (1u64 << 24) as f32
    };
    let fmt = |v: &[f32]| v.iter().map(|x| format!("{x:.4}")).collect::<Vec<_>>().join(", ");
    let mut all: Vec<Vec<f32>> = Vec::new();
    for c in 0..nc {
        let center: Vec<f32> = (0..dim).map(|d| ((c * 5 + d * 7) % 19) as f32).collect();
        for _ in 0..per {
            let v: Vec<f32> = center.iter().map(|&x| x + (rnd() - 0.5) * 0.5).collect();
            conn.execute(
                &format!("INSERT INTO docs (emb) VALUES (vector({}))", fmt(&v)),
                &[],
            )
            .unwrap();
            all.push(v);
        }
    }
    // Query = un punto perturbado. Exact top-10 ANTES del índice (full scan = exacto).
    let qs = fmt(&all[100]);
    let sql = format!("SELECT id FROM docs ORDER BY cosine_distance(emb, vector({qs})) LIMIT 10");
    let exact = ids_ordered(&conn, &sql);
    assert_eq!(exact.len(), 10);
    // Con índice IVF: rutea al índice (re-rank int8 + shortlist), re-rank exacto fuera.
    conn.execute(
        "CREATE VECTOR INDEX vi ON docs (emb) USING cosine LISTS 30 PROBES 8",
        &[],
    )
    .unwrap();
    let ann = ids_ordered(&conn, &sql);
    let truth: std::collections::HashSet<i64> = exact.iter().copied().collect();
    let hits = ann.iter().filter(|i| truth.contains(i)).count();
    assert!(
        hits >= 8,
        "recall@10 = {hits}/10 (≥ 8 esperado)\n exact={exact:?}\n ann={ann:?}"
    );
}

/// Re-rank ANN **solo-columna** sobre tabla ANCHA: el vector NO es la 1ª columna y
/// hay metadata; el re-rank del shortlist debe decodificar solo el vector (saltando
/// `label`) y la salida final traer todas las columnas correctas.
#[test]
fn ann_rerank_wide_table() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE docs (id INTEGER PRIMARY KEY, label TEXT, emb BLOB, score INTEGER)",
        &[],
    )
    .unwrap();
    for (lbl, vals, sc) in [("a", "1.0, 0.0", 10), ("b", "0.0, 1.0", 20), ("c", "0.95, 0.05", 30)]
    {
        conn.execute(
            &format!("INSERT INTO docs (label, emb, score) VALUES ('{lbl}', vector({vals}), {sc})"),
            &[],
        )
        .unwrap();
    }
    conn.execute(
        "CREATE VECTOR INDEX vi ON docs (emb) USING cosine LISTS 2 PROBES 2",
        &[],
    )
    .unwrap();
    // top-2 más cercanos a (1,0): id 1 (a, cos 0) y 3 (c, cos≈0.05); id 2 (b) es ortogonal.
    let rows: Vec<(i64, String, i64)> = conn
        .query(
            "SELECT id, label, score FROM docs ORDER BY cosine_distance(emb, vector(1.0, 0.0)) LIMIT 2",
            &[],
        )
        .unwrap()
        .map(|r| {
            let r = r.unwrap();
            (r.get(0).unwrap(), r.get(1).unwrap(), r.get(2).unwrap())
        })
        .collect();
    assert_eq!(rows, vec![(1, "a".to_string(), 10), (3, "c".to_string(), 30)]);
}
