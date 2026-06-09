//! Integración — índices secundarios (Slice 1b): `CREATE INDEX` + plan de
//! índice para `WHERE col = ?`, con mantenimiento correcto en insert/update/
//! delete y backfill. La prueba clave es que el index scan devuelve EXACTAMENTE
//! lo mismo que el full scan tras cualquier secuencia de cambios: si el índice
//! tuviera entradas obsoletas, el plan de índice devolvería filas mal.

use arkeion::{Connection, Database, Error, Options, params};

fn db() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("t.arkeion"), Options::default()).unwrap();
    (dir, db)
}

/// ids (col 0) de todas las filas de una consulta, ordenados.
fn ids(conn: &Connection, sql: &str) -> Vec<i64> {
    let mut v: Vec<i64> = conn
        .query(sql, &[])
        .unwrap()
        .map(|r| {
            let row = r.unwrap();
            let id: i64 = row.get(0).unwrap();
            id
        })
        .collect();
    v.sort_unstable();
    v
}

fn schema(conn: &Connection) {
    conn.execute(
        "CREATE TABLE u (id INTEGER PRIMARY KEY, email TEXT NOT NULL, age INTEGER)",
        &[],
    )
    .unwrap();
}

fn seed(conn: &Connection) {
    // emails y edades con duplicados a propósito.
    let rows = [
        ("ana@x", 30),
        ("ben@x", 25),
        ("ana@x", 40),  // email duplicado
        ("cleo@x", 30), // edad duplicada
        ("dan@x", 25),
    ];
    for (email, age) in rows {
        conn.execute(
            "INSERT INTO u (email, age) VALUES (?1, ?2)",
            &params![email, age],
        )
        .unwrap();
    }
}

#[test]
fn index_scan_matches_truth_text_and_int() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    schema(&conn);
    seed(&conn);
    conn.execute("CREATE INDEX ix_email ON u (email)", &[])
        .unwrap();
    conn.execute("CREATE INDEX ix_age ON u (age)", &[]).unwrap();

    // TEXT: valor presente (duplicado), presente (único), ausente.
    assert_eq!(ids(&conn, "SELECT id FROM u WHERE email = 'ana@x'"), [1, 3]);
    assert_eq!(ids(&conn, "SELECT id FROM u WHERE email = 'ben@x'"), [2]);
    assert_eq!(
        ids(&conn, "SELECT id FROM u WHERE email = 'zzz@x'"),
        Vec::<i64>::new()
    );

    // INTEGER: duplicado, único, ausente.
    assert_eq!(ids(&conn, "SELECT id FROM u WHERE age = 25"), [2, 5]);
    assert_eq!(ids(&conn, "SELECT id FROM u WHERE age = 40"), [3]);
    assert_eq!(
        ids(&conn, "SELECT id FROM u WHERE age = 99"),
        Vec::<i64>::new()
    );
}

#[test]
fn maintenance_insert_update_delete() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    schema(&conn);
    seed(&conn);
    conn.execute("CREATE INDEX ix_email ON u (email)", &[])
        .unwrap();

    // INSERT: la nueva fila aparece por el índice.
    conn.execute("INSERT INTO u (email, age) VALUES ('eve@x', 22)", &[])
        .unwrap();
    assert_eq!(ids(&conn, "SELECT id FROM u WHERE email = 'eve@x'"), [6]);

    // UPDATE: cambiar el valor indexado mueve la fila de cubo.
    conn.execute("UPDATE u SET email = 'ana2@x' WHERE id = 1", &[])
        .unwrap();
    assert_eq!(ids(&conn, "SELECT id FROM u WHERE email = 'ana@x'"), [3]); // ya no la 1
    assert_eq!(ids(&conn, "SELECT id FROM u WHERE email = 'ana2@x'"), [1]);

    // DELETE: la fila desaparece del índice.
    conn.execute("DELETE FROM u WHERE id = 3", &[]).unwrap();
    assert_eq!(
        ids(&conn, "SELECT id FROM u WHERE email = 'ana@x'"),
        Vec::<i64>::new()
    );
}

#[test]
fn backfill_indexes_existing_rows() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    schema(&conn);
    seed(&conn); // filas ANTES del índice
    conn.execute("CREATE INDEX ix_age ON u (age)", &[]).unwrap();
    // El backfill indexó las filas preexistentes.
    assert_eq!(ids(&conn, "SELECT id FROM u WHERE age = 30"), [1, 4]);
}

#[test]
fn update_and_delete_use_index_correctly() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    schema(&conn);
    seed(&conn);
    conn.execute("CREATE INDEX ix_age ON u (age)", &[]).unwrap();

    // UPDATE ... WHERE age = 25 toca exactamente las filas con esa edad.
    let n = conn
        .execute("UPDATE u SET age = 26 WHERE age = 25", &[])
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(
        ids(&conn, "SELECT id FROM u WHERE age = 25"),
        Vec::<i64>::new()
    );
    assert_eq!(ids(&conn, "SELECT id FROM u WHERE age = 26"), [2, 5]);

    // DELETE ... WHERE age = 30.
    let n = conn.execute("DELETE FROM u WHERE age = 30", &[]).unwrap();
    assert_eq!(n, 2);
    assert_eq!(
        ids(&conn, "SELECT id FROM u WHERE age = 30"),
        Vec::<i64>::new()
    );
}

#[test]
fn drop_index_keeps_queries_correct() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    schema(&conn);
    seed(&conn);
    conn.execute("CREATE INDEX ix_email ON u (email)", &[])
        .unwrap();
    conn.execute("DROP INDEX ix_email", &[]).unwrap();
    // Sin índice, la consulta cae a full scan: sigue correcta.
    assert_eq!(ids(&conn, "SELECT id FROM u WHERE email = 'ana@x'"), [1, 3]);
    // Recrearlo tras borrarlo funciona (id de índice nuevo, backfill).
    conn.execute("CREATE INDEX ix_email ON u (email)", &[])
        .unwrap();
    assert_eq!(ids(&conn, "SELECT id FROM u WHERE email = 'ana@x'"), [1, 3]);
}

#[test]
fn persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.arkeion");
    {
        let db = Database::open(&path, Options::default()).unwrap();
        let conn = db.connect().unwrap();
        schema(&conn);
        seed(&conn);
        conn.execute("CREATE INDEX ix_email ON u (email)", &[])
            .unwrap();
    }
    // Reabrir: el índice (en el esquema) y sus entradas siguen ahí.
    let db = Database::open(&path, Options::default().create_if_missing(false)).unwrap();
    let conn = db.connect().unwrap();
    assert_eq!(ids(&conn, "SELECT id FROM u WHERE email = 'ana@x'"), [1, 3]);
    conn.execute("INSERT INTO u (email, age) VALUES ('ana@x', 50)", &[])
        .unwrap();
    assert_eq!(
        ids(&conn, "SELECT id FROM u WHERE email = 'ana@x'"),
        [1, 3, 6]
    );
}

#[test]
fn errors_and_flags() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    schema(&conn);
    conn.execute("CREATE INDEX ix_email ON u (email)", &[])
        .unwrap();

    // Nombre duplicado.
    assert!(matches!(
        conn.execute("CREATE INDEX ix_email ON u (age)", &[]),
        Err(Error::Constraint(_))
    ));
    // IF NOT EXISTS lo traga.
    assert_eq!(
        conn.execute("CREATE INDEX IF NOT EXISTS ix_email ON u (age)", &[])
            .unwrap(),
        0
    );
    // Columna inexistente.
    assert!(conn.execute("CREATE INDEX ix_x ON u (nope)", &[]).is_err());
    // Indexar la PRIMARY KEY se rechaza (ya es la clave).
    assert!(conn.execute("CREATE INDEX ix_id ON u (id)", &[]).is_err());
    // DROP de índice inexistente: error, salvo IF EXISTS.
    assert!(conn.execute("DROP INDEX nope", &[]).is_err());
    assert_eq!(conn.execute("DROP INDEX IF EXISTS nope", &[]).unwrap(), 0);
}
