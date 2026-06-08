//! Integración M8 — branching, diff y merge por la API pública: el flujo de
//! migración de 03-api (branch → migrar → diff → merge), conflicto de fila, y
//! gestión de ramas (criterio "hecho cuando" del hito).

use arkeion::{AsOf, ChangeKind, Connection, Database, Error, MergePolicy, Options, params};

fn fresh() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("b.arkeion"), Options::default()).unwrap();
    (dir, db)
}

fn totals(conn: &Connection, sql: &str) -> Vec<f64> {
    conn.query(sql, &[])
        .unwrap()
        .map(|r| r.unwrap().get::<f64>("total").unwrap())
        .collect()
}

#[test]
fn migration_branch_diff_and_merge() {
    let (_d, db) = fresh();
    let main = db.connect().unwrap();
    main.execute(
        "CREATE TABLE facturas (id INTEGER PRIMARY KEY, total REAL NOT NULL)",
        &[],
    )
    .unwrap();
    main.execute("INSERT INTO facturas (total) VALUES (?1)", &params![100.0])
        .unwrap();
    main.execute("INSERT INTO facturas (total) VALUES (?1)", &params![200.0])
        .unwrap();

    // Rama de migración desde el head; aplicar IVA solo en ella.
    db.create_branch("migracion-iva", AsOf::Head).unwrap();
    let mig = db.connect_branch("migracion-iva").unwrap();
    mig.execute("UPDATE facturas SET total = total * ?1", &params![1.21])
        .unwrap();

    // Revisar antes de fusionar: la migración cambió 2 filas, ningún esquema.
    let diff = db.diff("main", "migracion-iva").unwrap();
    assert_eq!(diff.rows.len(), 2, "{diff:?}");
    assert!(diff.rows.iter().all(|r| r.kind == ChangeKind::Modified));
    assert!(diff.schema.is_empty());

    // main intacta mientras tanto.
    assert_eq!(
        totals(&main, "SELECT total FROM facturas ORDER BY id"),
        vec![100.0, 200.0]
    );

    // Merge limpio: aplica exactamente las 2 filas migradas.
    let report = db
        .merge("migracion-iva", "main", MergePolicy::FailOnConflict)
        .unwrap();
    assert_eq!(report.applied, 2);
    assert_eq!(
        totals(&main, "SELECT total FROM facturas ORDER BY id"),
        vec![121.0, 242.0]
    );

    // La auditoría sigue verde con la cadena global cruzando ramas.
    assert!(db.verify().unwrap().chain_ok);

    // Re-merge no hace nada.
    assert_eq!(
        db.merge("migracion-iva", "main", MergePolicy::FailOnConflict)
            .unwrap()
            .applied,
        0
    );
}

#[test]
fn same_row_modified_in_both_branches_conflicts() {
    let (_d, db) = fresh();
    let main = db.connect().unwrap();
    main.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER NOT NULL)",
        &[],
    )
    .unwrap();
    main.execute("INSERT INTO t (n) VALUES (1)", &[]).unwrap();

    db.create_branch("dev", AsOf::Head).unwrap();
    let dev = db.connect_branch("dev").unwrap();
    dev.execute("UPDATE t SET n = 100 WHERE id = 1", &[])
        .unwrap();
    main.execute("UPDATE t SET n = 200 WHERE id = 1", &[])
        .unwrap();

    // La misma fila cambiada distinto en ambas ramas ⇒ Conflict.
    let err = db
        .merge("dev", "main", MergePolicy::FailOnConflict)
        .err()
        .unwrap();
    assert!(
        matches!(err, Error::Conflict(ref c) if c.len() == 1),
        "fue {err:?}"
    );

    // El merge fallido no tocó main.
    let n: i64 = main
        .query("SELECT n FROM t WHERE id = 1", &[])
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .get("n")
        .unwrap();
    assert_eq!(n, 200);
}

#[test]
fn divergent_schema_conflicts() {
    let (_d, db) = fresh();
    let main = db.connect().unwrap();

    // Ambas ramas crean la misma tabla con esquemas distintos desde génesis.
    db.create_branch("dev", AsOf::Head).unwrap();
    db.connect_branch("dev")
        .unwrap()
        .execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER)", &[])
        .unwrap();
    main.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, b TEXT)", &[])
        .unwrap();

    let err = db
        .merge("dev", "main", MergePolicy::FailOnConflict)
        .err()
        .unwrap();
    assert!(matches!(err, Error::Conflict(_)), "fue {err:?}");

    // main conserva su propio esquema (columna `b`, no `a`).
    assert!(main.query("SELECT b FROM t", &[]).is_ok());
    assert!(main.query("SELECT a FROM t", &[]).is_err());
}

#[test]
fn branches_listed_and_dropped() {
    let (_d, db) = fresh();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", &[])
        .unwrap();

    db.create_branch("dev", AsOf::Head).unwrap();
    db.create_branch("x", AsOf::Head).unwrap();

    let mut names: Vec<String> = db.branches().unwrap().into_iter().map(|b| b.name).collect();
    names.sort();
    assert_eq!(names, vec!["dev", "main", "x"]);

    // Rama duplicada.
    assert!(matches!(
        db.create_branch("dev", AsOf::Head),
        Err(Error::BranchExists(_))
    ));

    // Borrar `x`: desaparece y conectar a ella falla.
    db.drop_branch("x").unwrap();
    assert!(matches!(
        db.connect_branch("x"),
        Err(Error::BranchNotFound(_))
    ));
    // No se puede borrar main.
    assert!(matches!(
        db.drop_branch("main"),
        Err(Error::InvalidInput(_))
    ));
}
