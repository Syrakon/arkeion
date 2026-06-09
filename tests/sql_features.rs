//! Features de superficie SQL añadidas tras el dogfooding: alias de columna,
//! funciones escalares, `IN (...)` y `SELECT <expr>` sin `FROM`.

use arkeion::{Database, Options, Value};

fn db() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("t.arkeion"), Options::default()).unwrap();
    (dir, db)
}

fn ids(conn: &arkeion::Connection, sql: &str) -> Vec<i64> {
    let mut v: Vec<i64> = conn
        .query(sql, &[])
        .unwrap()
        .map(|r| r.unwrap().get::<i64>(0).unwrap())
        .collect();
    v.sort_unstable();
    v
}

fn one(conn: &arkeion::Connection, sql: &str) -> Value {
    conn.query(sql, &[])
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .get::<Value>(0)
        .unwrap()
}

#[test]
fn column_aliases_name_the_output() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)", &[])
        .unwrap();
    conn.execute("INSERT INTO t (n) VALUES (5)", &[]).unwrap();

    let rows = conn
        .query("SELECT n AS valor, n + 1 AS sig FROM t", &[])
        .unwrap();
    assert_eq!(rows.columns(), ["valor".to_string(), "sig".to_string()]);
    // Alias sobre un agregado.
    let rows = conn.query("SELECT count(*) AS total FROM t", &[]).unwrap();
    assert_eq!(rows.columns(), ["total".to_string()]);
}

#[test]
fn scalar_functions() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT, n INTEGER)",
        &[],
    )
    .unwrap();
    conn.execute("INSERT INTO t (s, n) VALUES ('  Hi  ', -5)", &[])
        .unwrap();

    assert_eq!(
        one(&conn, "SELECT UPPER(s) FROM t"),
        Value::Text("  HI  ".into())
    );
    assert_eq!(
        one(&conn, "SELECT LENGTH(TRIM(s)) FROM t"),
        Value::Integer(2)
    ); // anidada
    assert_eq!(one(&conn, "SELECT ABS(n) FROM t"), Value::Integer(5));
    assert_eq!(
        one(&conn, "SELECT COALESCE(NULL, n) FROM t"),
        Value::Integer(-5)
    );
    assert_eq!(
        one(&conn, "SELECT ROUND(1.23456, 2) FROM t"),
        Value::Real(1.23)
    );
    assert_eq!(
        one(&conn, "SELECT SUBSTR(s, 3, 2) FROM t"),
        Value::Text("Hi".into())
    );
    assert_eq!(
        one(&conn, "SELECT TYPEOF(n) FROM t"),
        Value::Text("INTEGER".into())
    );
    // NULL se propaga; el tipo equivocado y la función desconocida son errores.
    assert_eq!(one(&conn, "SELECT UPPER(NULL) FROM t"), Value::Null);
    assert!(conn.query("SELECT UPPER(n) FROM t", &[]).is_err());
    assert!(conn.query("SELECT NOPE(1) FROM t", &[]).is_err());
}

#[test]
fn in_operator() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER, s TEXT)",
        &[],
    )
    .unwrap();
    for (n, s) in [(1, "a"), (2, "b"), (3, "c"), (4, "d")] {
        conn.execute(&format!("INSERT INTO t (n, s) VALUES ({n}, '{s}')"), &[])
            .unwrap();
    }
    assert_eq!(ids(&conn, "SELECT id FROM t WHERE n IN (1, 3)"), [1, 3]);
    assert_eq!(ids(&conn, "SELECT id FROM t WHERE n NOT IN (1, 3)"), [2, 4]);
    assert_eq!(ids(&conn, "SELECT id FROM t WHERE s IN ('b', 'd')"), [2, 4]);
    assert_eq!(
        ids(&conn, "SELECT id FROM t WHERE n IN (99)"),
        Vec::<i64>::new()
    );
    // NULL en la lista: no hallado + NULL ⇒ desconocido ⇒ la fila no pasa el WHERE.
    assert_eq!(
        ids(&conn, "SELECT id FROM t WHERE n IN (99, NULL)"),
        Vec::<i64>::new()
    );
}

#[test]
fn select_without_from() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();

    // Evaluar expresiones constantes sin tabla (estilo SQLite).
    assert_eq!(one(&conn, "SELECT 1 + 1"), Value::Integer(2));
    assert_eq!(one(&conn, "SELECT UPPER('hi')"), Value::Text("HI".into()));
    assert_eq!(one(&conn, "SELECT ABS(-3) * 2"), Value::Integer(6));

    // El alias nombra la columna de salida; sin él se deriva `colN`.
    let rows = conn.query("SELECT 2 * 3 AS seis, 9", &[]).unwrap();
    assert_eq!(rows.columns(), ["seis".to_string(), "col2".to_string()]);

    // `WHERE` constante sobre la única fila implícita.
    let n = conn.query("SELECT 1 WHERE 1 = 0", &[]).unwrap().count();
    assert_eq!(n, 0);
    let n = conn.query("SELECT 1 WHERE 1 = 1", &[]).unwrap().count();
    assert_eq!(n, 1);

    // Sin tabla no caben columnas, `*` ni agregados.
    assert!(conn.query("SELECT *", &[]).is_err());
    assert!(conn.query("SELECT foo", &[]).is_err());
    assert!(conn.query("SELECT COUNT(*)", &[]).is_err());

    // Una fila filtrada (por WHERE o LIMIT 0) no materializa la proyección, así que
    // una expresión que erraría no aflora — coherente con el camino con FROM.
    assert_eq!(
        conn.query("SELECT 'a' + 1 WHERE 1 = 0", &[])
            .unwrap()
            .count(),
        0
    );
    assert_eq!(
        conn.query("SELECT 'a' + 1 LIMIT 0", &[]).unwrap().count(),
        0
    );

    // `AS OF` sin `FROM` no se confunde con un alias y da un mensaje claro.
    let e = conn.query("SELECT 1 AS OF VERSION 0", &[]).err().unwrap();
    assert!(e.to_string().contains("AS OF requiere FROM"));
}

#[test]
fn quoted_and_unicode_identifiers() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();

    // Comillas dobles: una palabra reservada y un nombre con espacios como columnas;
    // un identificador unicode sin comillas como nombre de columna.
    conn.execute(
        r#"CREATE TABLE t (id INTEGER PRIMARY KEY, "select" INTEGER, "mi col" TEXT, café INTEGER)"#,
        &[],
    )
    .unwrap();
    conn.execute(
        r#"INSERT INTO t ("select", "mi col", café) VALUES (7, 'hola', 42)"#,
        &[],
    )
    .unwrap();

    assert_eq!(one(&conn, r#"SELECT "select" FROM t"#), Value::Integer(7));
    assert_eq!(
        one(&conn, r#"SELECT "mi col" FROM t WHERE café = 42"#),
        Value::Text("hola".into())
    );
    // El identificador unicode sin comillas funciona igual.
    assert_eq!(one(&conn, "SELECT café FROM t"), Value::Integer(42));
    // La columna de salida conserva el nombre entrecomillado.
    let rows = conn.query(r#"SELECT "mi col" FROM t"#, &[]).unwrap();
    assert_eq!(rows.columns(), ["mi col".to_string()]);
}

#[test]
fn division_by_zero_is_null() {
    let (_d, db) = db();
    let conn = db.connect().unwrap();

    // Entero y real, división y módulo: cero divisor ⇒ NULL (compat SQLite).
    assert_eq!(one(&conn, "SELECT 7 / 0"), Value::Null);
    assert_eq!(one(&conn, "SELECT 7 % 0"), Value::Null);
    assert_eq!(one(&conn, "SELECT 7.0 / 0"), Value::Null);
    assert_eq!(one(&conn, "SELECT 7 / 0.0"), Value::Null);
    assert_eq!(one(&conn, "SELECT 7.5 % 0"), Value::Null);
    // Divisor no nulo sigue calculando.
    assert_eq!(one(&conn, "SELECT 7 / 2"), Value::Integer(3));

    // En una expresión sobre una tabla: el divisor cero da NULL por fila.
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, d INTEGER)", &[])
        .unwrap();
    conn.execute("INSERT INTO t (d) VALUES (0), (2)", &[])
        .unwrap();
    let mut got: Vec<Value> = conn
        .query("SELECT 10 / d FROM t ORDER BY id", &[])
        .unwrap()
        .map(|r| r.unwrap().get::<Value>(0).unwrap())
        .collect();
    got.sort_by_key(|v| matches!(v, Value::Null)); // Null al final, determinista
    assert_eq!(got, vec![Value::Integer(5), Value::Null]);
}
