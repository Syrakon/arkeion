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
fn range_scan_matches_truth() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    schema(&conn);
    seed(&conn); // ages = [30,25,40,30,25] para ids 1..=5
    conn.execute("CREATE INDEX ix_age ON u (age)", &[]).unwrap();

    // Los cuatro operadores de rango, en cualquier orden.
    assert_eq!(ids(&conn, "SELECT id FROM u WHERE age > 25"), [1, 3, 4]);
    assert_eq!(ids(&conn, "SELECT id FROM u WHERE age >= 30"), [1, 3, 4]);
    assert_eq!(ids(&conn, "SELECT id FROM u WHERE age < 30"), [2, 5]);
    assert_eq!(ids(&conn, "SELECT id FROM u WHERE age <= 25"), [2, 5]);
    assert_eq!(
        ids(&conn, "SELECT id FROM u WHERE age > 40"),
        Vec::<i64>::new()
    );
    // Constante a la izquierda ⇒ se voltea el operador (25 < age  ≡  age > 25).
    assert_eq!(ids(&conn, "SELECT id FROM u WHERE 25 < age"), [1, 3, 4]);
    assert_eq!(ids(&conn, "SELECT id FROM u WHERE 30 >= age"), [1, 2, 4, 5]);
}

#[test]
fn range_scan_excludes_null_and_maintains() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    schema(&conn);
    seed(&conn);
    // Fila con age NULL: nunca debe aparecer en un rango.
    conn.execute("INSERT INTO u (email, age) VALUES ('nil@x', NULL)", &[])
        .unwrap();
    conn.execute("CREATE INDEX ix_age ON u (age)", &[]).unwrap();
    assert_eq!(
        ids(&conn, "SELECT id FROM u WHERE age >= 0"),
        [1, 2, 3, 4, 5]
    ); // sin la 6
    assert_eq!(
        ids(&conn, "SELECT id FROM u WHERE age < 1000"),
        [1, 2, 3, 4, 5]
    );

    // UPDATE por rango usa el índice y mantiene la verdad.
    let n = conn
        .execute("UPDATE u SET age = 99 WHERE age >= 40", &[])
        .unwrap();
    assert_eq!(n, 1); // solo la 3 (age 40)
    assert_eq!(ids(&conn, "SELECT id FROM u WHERE age = 99"), [3]);
    // DELETE por rango.
    let n = conn.execute("DELETE FROM u WHERE age < 30", &[]).unwrap();
    assert_eq!(n, 2); // ids 2 y 5 (age 25)
    assert_eq!(
        ids(&conn, "SELECT id FROM u WHERE age < 30"),
        Vec::<i64>::new()
    );
}

#[test]
fn unique_index_enforces_and_allows_nulls() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE u (id INTEGER PRIMARY KEY, email TEXT, age INTEGER)",
        &[],
    )
    .unwrap();
    conn.execute("CREATE UNIQUE INDEX ix_email ON u (email)", &[])
        .unwrap();

    conn.execute("INSERT INTO u (email, age) VALUES ('a@x', 1)", &[])
        .unwrap();
    // Duplicado ⇒ Constraint.
    assert!(matches!(
        conn.execute("INSERT INTO u (email, age) VALUES ('a@x', 2)", &[]),
        Err(Error::Constraint(_))
    ));
    // Valor distinto ⇒ ok.
    conn.execute("INSERT INTO u (email, age) VALUES ('b@x', 3)", &[])
        .unwrap();
    // Varios NULL están permitidos (UNIQUE no aplica a NULL).
    conn.execute("INSERT INTO u (email, age) VALUES (NULL, 4)", &[])
        .unwrap();
    conn.execute("INSERT INTO u (email, age) VALUES (NULL, 5)", &[])
        .unwrap();
    assert_eq!(ids(&conn, "SELECT id FROM u WHERE email = 'a@x'"), [1]);

    // UPDATE a un valor ya tomado ⇒ Constraint; a uno libre ⇒ ok.
    assert!(matches!(
        conn.execute("UPDATE u SET email = 'b@x' WHERE id = 1", &[]),
        Err(Error::Constraint(_))
    ));
    conn.execute("UPDATE u SET email = 'c@x' WHERE id = 1", &[])
        .unwrap();
    assert_eq!(ids(&conn, "SELECT id FROM u WHERE email = 'c@x'"), [1]);
    // Reasignar el mismo valor a la misma fila ⇒ ok (no choca consigo misma).
    conn.execute("UPDATE u SET age = 9 WHERE id = 1", &[])
        .unwrap();
}

#[test]
fn create_unique_index_rejects_existing_duplicates() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE u (id INTEGER PRIMARY KEY, email TEXT NOT NULL)",
        &[],
    )
    .unwrap();
    conn.execute("INSERT INTO u (email) VALUES ('dup@x')", &[])
        .unwrap();
    conn.execute("INSERT INTO u (email) VALUES ('dup@x')", &[])
        .unwrap();
    // Con duplicados preexistentes, CREATE UNIQUE INDEX falla y no deja índice.
    assert!(matches!(
        conn.execute("CREATE UNIQUE INDEX ix ON u (email)", &[]),
        Err(Error::Constraint(_))
    ));
    // El índice no quedó: un índice no-único sí se crea.
    conn.execute("CREATE INDEX ix ON u (email)", &[]).unwrap();
    assert_eq!(ids(&conn, "SELECT id FROM u WHERE email = 'dup@x'"), [1, 2]);
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
