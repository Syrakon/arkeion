//! Regresiones de los bugs que destapó el dogfooding por el CLI `ark`:
//! - el parser no debe abortar el proceso ante una expresión muy anidada;
//! - el INSERT posicional exige un valor por columna (no rellena NULL en silencio);
//! - `changes(v)` diffea contra el **padre registrado**, no contra `v-1` (correcto
//!   en merges/puntos de bifurcación).

use arkeion::{AsOf, Database, MergePolicy, Options};

fn db() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("t.arkeion"), Options::default()).unwrap();
    (dir, db)
}

#[test]
fn parser_rejects_deep_nesting_without_panic() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)", &[])
        .unwrap();
    conn.execute("INSERT INTO t (v) VALUES (1)", &[]).unwrap();

    // ~300 niveles de paréntesis: antes desbordaba la pila (SIGABRT); ahora es un
    // error SQL limpio (el cap salta a los 256, muy por debajo del overflow).
    let deep = format!("SELECT {}1{} FROM t", "(".repeat(300), ")".repeat(300));
    assert!(conn.query(&deep, &[]).is_err());

    // El anidamiento normal sigue funcionando.
    assert!(conn.query("SELECT (((1 + 2))) FROM t", &[]).is_ok());
}

#[test]
fn positional_insert_requires_a_value_per_column() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE p (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER)",
        &[],
    )
    .unwrap();

    // Posicional con MENOS valores que columnas: error (antes se aceptaba y `b`
    // quedaba NULL sin que el usuario lo escribiera).
    assert!(conn.execute("INSERT INTO p VALUES (1, 2)", &[]).is_err());
    // Posicional exacto: ok.
    conn.execute("INSERT INTO p VALUES (1, 2, 3)", &[]).unwrap();
    // Para nombrar un subconjunto está la forma con lista de columnas.
    conn.execute("INSERT INTO p (a, b) VALUES (4, 5)", &[]).unwrap();
}

#[test]
fn changes_uses_recorded_parent_not_numeric_predecessor() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)", &[])
        .unwrap();
    conn.execute("INSERT INTO t (v) VALUES (10)", &[]).unwrap();

    db.create_branch("feat", AsOf::Head).unwrap();
    let fconn = db.connect_branch("feat").unwrap();
    fconn.execute("INSERT INTO t (v) VALUES (20)", &[]).unwrap();

    let report = db.merge("feat", "main", MergePolicy::FailOnConflict).unwrap();
    // El commit de merge tiene como padre el merge-base, no `version-1`. `changes`
    // debe mostrar el insert aplicado, no «(sin cambios)» que daría `v-1`.
    let d = db.changes(report.version).unwrap();
    assert!(
        !d.rows.is_empty(),
        "changes(merge) debería mostrar la fila aplicada por el merge"
    );
}
