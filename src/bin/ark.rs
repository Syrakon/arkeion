//! `ark` — shell de línea de comandos para Arkeion, al estilo `sqlite3`.
//!
//! Abre un archivo `.arkeion`, ofrece un REPL de SQL y **meta-comandos** (`.`) que
//! exponen las features propias del motor —historia, time-travel `AS OF`, `verify`
//! de la cadena de auditoría, ramas/merge, `scrub` y `vacuum`— para ejercitarlo a
//! mano e ir encontrando rincones que los tests no tocan (dogfooding).
//!
//! Sin dependencias nuevas (D8): lee stdin a mano y detecta el terminal con
//! `std::io::IsTerminal`. En modo no interactivo (pipe) ejecuta el guion y sale.
//!
//! ```text
//! cargo run --bin ark -- mibase.arkeion
//! cargo run --bin ark -- cifrada.arkeion --key <64-hex>
//! echo "CREATE TABLE t(id INTEGER PRIMARY KEY); .tables" | cargo run --bin ark -- t.arkeion
//! ```

use std::io::{self, BufRead, IsTerminal, Write};

use arkeion::catalog::TableDef;
use arkeion::{
    AsOf, AuditAnchor, ChangeKind, Database, Diff, Key, MergePolicy, Options, Retention, Value,
};

fn main() {
    let mut args = std::env::args().skip(1);
    let mut path: Option<String> = None;
    let mut opts = Options::default();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--compress" => opts = opts.compress(true),
            "--no-create" => opts = opts.create_if_missing(false),
            "--cache-mb" => {
                match args
                    .next()
                    .and_then(|s| s.parse::<usize>().ok())
                    .and_then(|mb| mb.checked_mul(1024 * 1024))
                {
                    Some(bytes) => opts = opts.cache_bytes(bytes),
                    None => fail("--cache-mb requiere un número de MiB en rango"),
                }
            }
            "--ecc" => match args.next().and_then(|s| s.parse::<u8>().ok()) {
                Some(n) => opts = opts.ecc(n),
                None => fail("--ecc requiere un número (símbolos de paridad)"),
            },
            "--key" => match args.next().as_deref().and_then(parse_key) {
                Some(k) => opts = opts.key(k),
                None => fail("--key requiere 64 dígitos hex (clave de 32 bytes)"),
            },
            "-h" | "--help" => {
                usage();
                return;
            }
            other if !other.starts_with('-') && path.is_none() => path = Some(other.into()),
            other => fail(&format!("argumento desconocido: {other}")),
        }
    }
    let Some(path) = path else {
        usage();
        std::process::exit(2);
    };

    let db = match Database::open(&path, opts) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("no se pudo abrir '{path}': {e}");
            std::process::exit(1);
        }
    };
    let conn = match db.connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    repl(&db, conn, &path);
}

/// Lo que un meta-comando le pide al bucle: seguir, salir, o cambiar de conexión
/// (rama / snapshot histórico / volver a la cabeza).
enum Action {
    Continue,
    Quit,
    Swap(Box<arkeion::Connection>),
}

fn repl(db: &Database, mut conn: arkeion::Connection, path: &str) {
    let interactive = io::stdin().is_terminal();
    if interactive {
        println!(
            "Arkeion · ark shell — '{path}' (v{}). `.help` para los comandos.",
            conn.version()
        );
    }
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut sql = String::new();
    loop {
        if interactive {
            print!("{}", if sql.is_empty() { "ark> " } else { "...> " });
            io::stdout().flush().ok();
        }
        let mut line = String::new();
        match input.read_line(&mut line) {
            Ok(0) => break, // EOF / Ctrl-D
            Ok(_) => {}
            Err(e) => {
                eprintln!("error de lectura: {e}");
                break;
            }
        }
        let line = strip_comment(&line).trim_end();

        // Meta-comando solo cuando no estamos a media sentencia SQL. Toleramos un
        // `;` final por si el dedo lo añade por costumbre (los meta no lo usan).
        if sql.is_empty() && line.trim_start().starts_with('.') {
            match meta(db, &conn, line.trim().trim_end_matches(';').trim_end()) {
                Action::Quit => break,
                Action::Continue => {}
                Action::Swap(c) => conn = *c,
            }
            continue;
        }
        if sql.is_empty() && line.trim().is_empty() {
            continue;
        }
        sql.push_str(line);
        sql.push('\n');
        if line.trim_end().ends_with(';') {
            let stmt = sql.trim().trim_end_matches(';').trim().to_string();
            sql.clear();
            if !stmt.is_empty() {
                run_sql(&conn, &stmt);
            }
        }
    }
}

/// Ejecuta una sentencia: `SELECT`/`VALUES` por `query` (imprime tabla), el resto
/// por `execute` (imprime filas afectadas).
fn run_sql(conn: &arkeion::Connection, sql: &str) {
    let first = sql
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if first == "select" || first == "values" {
        let rows = match conn.query(sql, &[]) {
            Ok(r) => r,
            Err(e) => return eprintln!("error: {e}"),
        };
        let cols: Vec<String> = rows.columns().to_vec();
        let n = cols.len();
        let mut data: Vec<Vec<String>> = Vec::new();
        for r in rows {
            let row = match r {
                Ok(row) => row,
                Err(e) => return eprintln!("error: {e}"),
            };
            let mut cells = Vec::with_capacity(n);
            for i in 0..n {
                match row.get::<Value>(i) {
                    Ok(v) => cells.push(show(&v)),
                    Err(e) => return eprintln!("error: {e}"),
                }
            }
            data.push(cells);
        }
        print_table(&cols, &data);
    } else {
        match conn.execute(sql, &[]) {
            Ok(n) => println!("OK · {n} fila{}", plural(n)),
            Err(e) => eprintln!("error: {e}"),
        }
    }
}

fn meta(db: &Database, conn: &arkeion::Connection, line: &str) -> Action {
    let mut p = line.split_whitespace();
    let cmd = p.next().unwrap_or("");
    match cmd {
        ".quit" | ".exit" | ".q" => return Action::Quit,
        ".help" | ".h" => help(),
        ".tables" => match conn.tables() {
            Ok(ts) if ts.is_empty() => println!("(sin tablas)"),
            Ok(ts) => ts.iter().for_each(|t| println!("{}", t.name)),
            Err(e) => eprintln!("error: {e}"),
        },
        ".schema" => {
            let only = p.next();
            match conn.tables() {
                Ok(ts) => ts
                    .iter()
                    .filter(|t| only.is_none_or(|n| n == t.name))
                    .for_each(print_schema),
                Err(e) => eprintln!("error: {e}"),
            }
        }
        ".version" | ".v" => println!("v{}", conn.version()),
        ".history" | ".log" => match db.history() {
            Ok(revs) => revs.iter().for_each(|r| {
                println!(
                    "v{:<5} padre v{:<5} ts={}",
                    r.version,
                    r.parent,
                    epoch(r.timestamp)
                );
            }),
            Err(e) => eprintln!("error: {e}"),
        },
        ".verify" => match (p.next(), p.next()) {
            // Con `<versión> <hash>`: verifica contra un ANCLA previa. Esto sí
            // detecta un rollback/truncado (un `.verify` pelado solo valida la
            // cadena que EXISTE, que tras un truncado es una cadena más corta y
            // válida — usa `.anchor` para capturar un ancla y guardarla aparte).
            (Some(vs), Some(hs)) => match (vs.parse::<u64>(), parse_hash(hs)) {
                (Ok(v), Some(chain_hash)) => {
                    let anchor = AuditAnchor {
                        version: v,
                        chain_hash,
                    };
                    match db.verify_anchor(&anchor) {
                        Ok(rep) => println!(
                            "ancla v{v}: cadena OK ✓ — sin rollback (head v{})",
                            rep.head
                        ),
                        Err(e) => eprintln!(
                            "ancla v{v} NO casa: {e}  ← rollback / truncado / reescritura de la historia"
                        ),
                    }
                }
                _ => eprintln!("uso: .verify [<versión> <hash-hex-64>]"),
            },
            _ => match db.verify() {
                Ok(rep) => println!(
                    "cadena {} · {} commits · head v{} · hash {}…",
                    if rep.chain_ok { "OK ✓" } else { "ROTA ✗" },
                    rep.commits,
                    rep.head,
                    hex8(&rep.chain_hash),
                ),
                Err(e) => eprintln!("error: {e}"),
            },
        },
        ".anchor" => match db.verify() {
            Ok(rep) => {
                let a = rep.anchor();
                println!("ancla  v{}  {}", a.version, hex_full(&a.chain_hash));
                println!(
                    "  (guárdala aparte; luego `.verify {} {}` delata un rollback/truncado)",
                    a.version,
                    hex_full(&a.chain_hash)
                );
            }
            Err(e) => eprintln!("error: {e}"),
        },
        ".scrub" => {
            // `scrub` es un DIAGNÓSTICO: el ECC corrige al leer (en memoria), pero
            // un store append-only no reescribe in situ — la reparación durable es
            // `vacuum` o restaurar de la historia. No sobrevender ("corrige").
            let r = db.scrub();
            println!(
                "scrub · {} páginas · {} con bit-rot (corregido al leer, NO persistido → re-vacuum) · {} irrecuperables",
                r.pages, r.corrected, r.broken
            );
            if r.corrected > 0 {
                println!(
                    "  ⚠ disco degradándose: re-ejecuta `.vacuum` o restaura una versión íntegra"
                );
            }
        }
        ".vacuum" => {
            // Validar el arg: un `.vacuum xyz` no debe compactar SILENCIOSAMENTE
            // toda la historia (riesgo de pérdida por typo); sin arg = conserva todo.
            let ret = match p.next() {
                None => Retention::KeepAll,
                Some(s) => match s.parse::<u64>() {
                    Ok(n) => Retention::KeepLast(n),
                    Err(_) => {
                        eprintln!(
                            "uso: .vacuum [n]  (n = nº de versiones a conservar; sin n = toda)"
                        );
                        return Action::Continue;
                    }
                },
            };
            match db.vacuum(ret) {
                Ok(rep) => println!(
                    "vacuum · head v{} · conservadas desde v{} · {} compactadas",
                    rep.head, rep.kept_from, rep.reclaimed_versions
                ),
                Err(e) => eprintln!("error: {e}"),
            }
        }
        ".branches" => match db.branches() {
            Ok(bs) => bs
                .iter()
                .for_each(|b| println!("{:<20} → v{}", b.name, b.head)),
            Err(e) => eprintln!("error: {e}"),
        },
        ".branch" => match p.next() {
            Some(name) => {
                let from = p.next().and_then(|s| s.parse::<u64>().ok());
                let asof = from.map_or(AsOf::Head, AsOf::Version);
                match db.create_branch(name, asof) {
                    Ok(()) => println!("rama '{name}' creada"),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
            None => eprintln!("uso: .branch <nombre> [versión-origen]"),
        },
        ".checkout" | ".use" => match p.next() {
            Some(name) => match db.connect_branch(name) {
                Ok(c) => {
                    println!("en rama '{name}' (v{})", c.version());
                    return Action::Swap(Box::new(c));
                }
                Err(e) => eprintln!("error: {e}"),
            },
            None => eprintln!("uso: .checkout <rama>"),
        },
        ".merge" => match (p.next(), p.next()) {
            (Some(from), Some(into)) => match db.merge(from, into, MergePolicy::FailOnConflict) {
                Ok(rep) => println!(
                    "merge '{from}' → '{into}' · {} cambios aplicados · v{}",
                    rep.applied, rep.version
                ),
                Err(e) => eprintln!("error: {e}"),
            },
            _ => eprintln!("uso: .merge <origen> <destino>"),
        },
        ".changes" => match p.next().and_then(|s| s.parse::<u64>().ok()) {
            Some(v) => match db.changes(v) {
                Ok(d) => print_diff(&d),
                Err(e) => eprintln!("error: {e}"),
            },
            None => eprintln!("uso: .changes <versión>  (qué cambió ESA versión)"),
        },
        ".diff" => match (
            p.next().and_then(|s| s.parse::<u64>().ok()),
            p.next().and_then(|s| s.parse::<u64>().ok()),
        ) {
            (Some(a), Some(b)) => match db.diff_versions(a, b) {
                Ok(d) => print_diff(&d),
                Err(e) => eprintln!("error: {e}"),
            },
            _ => eprintln!("uso: .diff <v1> <v2>"),
        },
        ".asof" => match p.next().and_then(|s| s.parse::<u64>().ok()) {
            Some(v) => match conn.snapshot(AsOf::Version(v)) {
                Ok(c) => {
                    println!("snapshot AS OF v{v} (solo lectura; `.live` para volver)");
                    return Action::Swap(Box::new(c));
                }
                Err(e) => eprintln!("error: {e}"),
            },
            None => eprintln!("uso: .asof <versión>"),
        },
        ".live" | ".head" => match db.connect() {
            Ok(c) => {
                println!("conexión en vivo (head v{})", c.version());
                return Action::Swap(Box::new(c));
            }
            Err(e) => eprintln!("error: {e}"),
        },
        other => eprintln!("comando desconocido: {other}  (.help para la lista)"),
    }
    Action::Continue
}

// --- presentación ---

fn show(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Integer(n) => n.to_string(),
        // Debug de f64: forma más corta que round-trippea, SIEMPRE con `.` o `e`,
        // así un REAL no se confunde con un INTEGER (5.0 → "5.0", 1e308 → "1e308").
        Value::Real(f) => format!("{f:?}"),
        Value::Bool(b) => b.to_string(),
        Value::Text(s) => s.clone(),
        Value::Blob(b) => format!(
            "x'{}'",
            b.iter().map(|x| format!("{x:02x}")).collect::<String>()
        ),
    }
}

fn print_table(cols: &[String], rows: &[Vec<String>]) {
    let mut w: Vec<usize> = cols.iter().map(|c| c.chars().count()).collect();
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            w[i] = w[i].max(c.chars().count());
        }
    }
    let join = |cells: &[String]| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| pad(c, w[i]))
            .collect::<Vec<_>>()
            .join("  ")
    };
    println!("{}", join(cols));
    println!(
        "{}",
        w.iter()
            .map(|x| "-".repeat(*x))
            .collect::<Vec<_>>()
            .join("  ")
    );
    rows.iter().for_each(|r| println!("{}", join(r)));
    println!("({} fila{})", rows.len(), plural(rows.len()));
}

fn pad(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n >= w {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(w - n))
    }
}

fn print_schema(t: &TableDef) {
    // En orden LÓGICO de presentación (`logical_order`); el `rowid_alias` y las
    // columnas de índice siguen siendo posiciones físicas.
    let cols: Vec<String> = t
        .logical_order
        .iter()
        .map(|&phys| {
            let c = &t.columns[phys];
            let mut s = format!("{} {}", c.name, c.col_type.name());
            if t.rowid_alias == Some(phys) {
                s.push_str(" PRIMARY KEY");
            } else if c.not_null {
                s.push_str(" NOT NULL");
            }
            s
        })
        .collect();
    println!("CREATE TABLE {} ({});", t.name, cols.join(", "));
    for idx in &t.indexes {
        let names: Vec<&str> = idx
            .columns
            .iter()
            .map(|&i| t.columns[i].name.as_str())
            .collect();
        println!(
            "  CREATE {}INDEX {} ON {} ({});",
            if idx.unique { "UNIQUE " } else { "" },
            idx.name,
            t.name,
            names.join(", ")
        );
    }
}

fn print_diff(d: &Diff) {
    if d.is_empty() {
        return println!("(sin cambios)");
    }
    for s in &d.schema {
        println!("  {} tabla {}", mark(s.kind), s.table);
    }
    for r in &d.rows {
        println!(
            "  {} fila  tabla#{} rowid {}",
            mark(r.kind),
            r.table_id,
            r.rowid
        );
    }
    println!("— {} de esquema, {} de filas", d.schema.len(), d.rows.len());
}

fn mark(k: ChangeKind) -> char {
    match k {
        ChangeKind::Added => '+',
        ChangeKind::Removed => '-',
        ChangeKind::Modified => '~',
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Quita un comentario de línea `-- …` (hasta el fin de línea) respetando tanto
/// las cadenas entre comillas simples (`'…'`) como los identificadores entre
/// comillas dobles (`"…"`), para que el buffering/enrutado por `;` no se confunda
/// con un comentario al final de una sentencia o en su propia línea. Un `--`
/// dentro de cualquiera de los dos no es comentario. (Aproximación de línea: no
/// modela los escapes `''`/`""`, como el resto del enrutado del REPL.)
fn strip_comment(line: &str) -> &str {
    let b = line.as_bytes();
    let mut in_str = false;
    let mut in_ident = false;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\'' if !in_ident => in_str = !in_str,
            b'"' if !in_str => in_ident = !in_ident,
            b'-' if !in_str && !in_ident && b.get(i + 1) == Some(&b'-') => return &line[..i],
            _ => {}
        }
        i += 1;
    }
    line
}

fn epoch(t: std::time::SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn hex8(h: &[u8; 32]) -> String {
    h[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// 64 dígitos hex → 32 bytes (clave o hash de ancla).
fn parse_hash(h: &str) -> Option<[u8; 32]> {
    if h.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = u8::from_str_radix(h.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(bytes)
}

fn parse_key(h: &str) -> Option<Key> {
    parse_hash(h).map(Key::new)
}

fn hex_full(h: &[u8; 32]) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

fn usage() {
    eprintln!(
        "ark — shell de Arkeion\n\
         \n\
         uso: ark <archivo.arkeion> [opciones]\n\
         opciones:\n\
         \x20 --compress       activa la compresión de página (al crear)\n\
         \x20 --ecc <n>        Reed-Solomon con n símbolos de paridad (al crear)\n\
         \x20 --key <64-hex>   cifrado AES-256-GCM con clave de 32 bytes en hex\n\
         \x20 --cache-mb <n>   tamaño de la caché de páginas en MiB (def. 64)\n\
         \x20 --no-create      no crear el archivo si no existe\n\
         \n\
         dentro: SQL terminado en ';', o meta-comandos (.help)."
    );
}

fn help() {
    println!(
        "Meta-comandos:\n\
         \x20 .tables                 lista las tablas\n\
         \x20 .schema [tabla]         muestra el esquema (CREATE …)\n\
         \x20 .version                versión (commit) actual\n\
         \x20 .history                línea temporal de versiones (git log de datos)\n\
         \x20 .changes <v>            qué cambió la versión v\n\
         \x20 .diff <v1> <v2>         diferencias entre dos versiones\n\
         \x20 .asof <v>               vista de solo lectura AS OF la versión v\n\
         \x20 .live                   vuelve a la cabeza (en vivo)\n\
         \x20 .branches               lista las ramas\n\
         \x20 .branch <n> [v]         crea la rama n (desde v o head)\n\
         \x20 .checkout <rama>        cambia a una rama\n\
         \x20 .merge <orig> <dest>    fusiona orig en dest\n\
         \x20 .verify                 verifica la cadena de auditoría (la historia presente)\n\
         \x20 .anchor                 captura un ancla (versión+hash) para detectar rollbacks luego\n\
         \x20 .verify <v> <hash>      verifica contra un ancla previa (delata truncado/reescritura)\n\
         \x20 .scrub                  diagnóstico de integridad (detecta bit-rot; reparar = vacuum/restaurar)\n\
         \x20 .vacuum [n]             compacta (conserva n versiones, o todas)\n\
         \x20 .help   .quit\n\
         SQL: cualquier sentencia terminada en ';'."
    );
}

fn fail(msg: &str) -> ! {
    eprintln!("ark: {msg}");
    std::process::exit(2);
}

#[cfg(test)]
mod tests {
    use super::strip_comment;

    #[test]
    fn strip_comment_respects_quotes() {
        // Comentario real al final / en su línea.
        assert_eq!(strip_comment("SELECT 1; -- nota"), "SELECT 1; ");
        assert_eq!(strip_comment("-- toda la línea"), "");
        // `--` dentro de una cadena entre comillas simples no es comentario.
        assert_eq!(strip_comment("SELECT 'a--b';"), "SELECT 'a--b';");
        // `--` dentro de un identificador entre comillas dobles tampoco (regresión:
        // antes se truncaba y la sentencia se perdía en silencio).
        assert_eq!(
            strip_comment(r#"SELECT 1 AS "a--b";"#),
            r#"SELECT 1 AS "a--b";"#
        );
        // Comillas dobles tras cerrar una cadena simple: el `--` posterior sí corta.
        assert_eq!(strip_comment(r#"SELECT "x"; -- c"#), r#"SELECT "x"; "#);
    }
}
