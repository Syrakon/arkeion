//! Runner de la suite de aceptación SQL (hito M3).
//!
//! Lee `tests/sql/*.sqltest`: archivos legibles con sentencias (`> …;`) y la
//! salida esperada justo debajo. Formatos de expectativa:
//!
//! ```text
//! ok               la sentencia ejecuta sin error (filas afectadas irrelevantes)
//! affected N       ejecuta y afecta exactamente N filas
//! empty            la consulta no devuelve filas
//! error <texto>    falla y el mensaje contiene <texto>
//! v1|v2|…          una línea por fila, valores separados por '|'
//! ```

use std::fs;
use std::path::Path;

use arkeion::{Connection, Database, Options, Row, Value};

#[test]
fn sql_acceptance_suite() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/sql");
    let mut files: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "sqltest"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no hay archivos .sqltest en {dir:?}");
    for file in files {
        run_file(&file);
    }
}

fn run_file(path: &Path) {
    let content = fs::read_to_string(path).unwrap();
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    let tmp = tempfile::tempdir().unwrap();
    let db = Database::open(tmp.path().join("t.arkeion"), Options::default()).unwrap();
    let conn = db.connect().unwrap();

    let mut lines = content.lines().enumerate().peekable();
    while let Some((idx, raw)) = lines.next() {
        let line = raw.trim_end();
        if line.is_empty() || line.starts_with("--") {
            continue;
        }
        let stmt = line
            .strip_prefix("> ")
            .unwrap_or_else(|| panic!("{name}:{}: se esperaba '> ', hay: {line}", idx + 1));

        let mut expected: Vec<String> = Vec::new();
        while let Some((_, peeked)) = lines.peek() {
            let l = peeked.trim_end();
            if l.is_empty() || l.starts_with("> ") {
                break;
            }
            if !l.starts_with("--") {
                expected.push(l.to_owned());
            }
            lines.next();
        }
        run_case(&conn, stmt, &expected, &name, idx + 1);
    }
}

fn run_case(conn: &Connection, stmt: &str, expected: &[String], file: &str, line: usize) {
    let ctx = format!("{file}:{line}: {stmt}");
    let is_select = stmt
        .trim_start()
        .get(..6)
        .is_some_and(|s| s.eq_ignore_ascii_case("select"));

    // ¿Se espera un error?
    if let Some(sub) = expected.first().and_then(|e| e.strip_prefix("error")) {
        let sub = sub.trim();
        let err = if is_select {
            conn.query(stmt, &[]).map(|_| ()).err()
        } else {
            conn.execute(stmt, &[]).map(|_| ()).err()
        };
        let msg = err
            .unwrap_or_else(|| panic!("{ctx}\n  se esperaba un error y tuvo éxito"))
            .to_string();
        assert!(
            msg.contains(sub),
            "{ctx}\n  error real:      {msg}\n  debía contener:  {sub}"
        );
        return;
    }

    if is_select {
        let rows = conn
            .query(stmt, &[])
            .unwrap_or_else(|e| panic!("{ctx}\n  {e}"));
        let got: Vec<String> = rows.map(|r| render_row(&r.unwrap())).collect();
        let want: Vec<String> = if expected.len() == 1 && expected[0] == "empty" {
            Vec::new()
        } else {
            expected.to_vec()
        };
        assert_eq!(got, want, "{ctx}");
    } else {
        let n = conn
            .execute(stmt, &[])
            .unwrap_or_else(|e| panic!("{ctx}\n  {e}"));
        match expected.first().map(String::as_str) {
            Some("ok") => {}
            Some(a) if a.starts_with("affected ") => {
                let want: usize = a["affected ".len()..].parse().unwrap();
                assert_eq!(n, want, "{ctx}");
            }
            other => panic!("{ctx}\n  expectativa no reconocida: {other:?}"),
        }
    }
}

fn render_row(row: &Row) -> String {
    row.values()
        .iter()
        .map(render)
        .collect::<Vec<_>>()
        .join("|")
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_owned(),
        Value::Bool(true) => "TRUE".to_owned(),
        Value::Bool(false) => "FALSE".to_owned(),
        Value::Integer(n) => n.to_string(),
        Value::Real(f) => format!("{f:?}"),
        Value::Text(s) => s.clone(),
        Value::Blob(b) => {
            let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
            format!("x'{hex}'")
        }
    }
}
