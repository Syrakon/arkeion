//! Recuperación tras truncado de cola **por la capa SQL**. `tests/kv.rs` ya prueba a
//! fondo el mecanismo de recuperación a nivel Store; aquí se valida que la capa de
//! arriba —catálogo (esquema), tabla e **índice secundario**— recupera a la última
//! commit completa: ni pierde ni resucita filas, y el índice queda coherente con las
//! filas presentes. Mismo principio que el test KV, ejercido vía `Database` + SQL.

use std::fs;

use arkeion::{Database, Options, params};

fn row_ids(db: &Database) -> Vec<i64> {
    let conn = db.connect().unwrap();
    let mut ids: Vec<i64> = conn
        .query("SELECT id FROM t", &[])
        .unwrap()
        .map(|r| r.unwrap().get::<i64>(0).unwrap())
        .collect();
    ids.sort_unstable();
    ids
}

#[test]
fn sql_recovery_truncates_to_last_complete_commit_with_index() {
    const K: i64 = 40;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rec.arkeion");

    // Esquema (tabla + índice secundario sobre `tag`) y K commits, una fila por commit.
    // `tag = id * 10` permite comprobar el índice por igualdad tras recuperar.
    let mut commit_ends: Vec<u64> = Vec::new();
    {
        let db = Database::open(&path, Options::default()).unwrap();
        let conn = db.connect().unwrap();
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, tag INTEGER NOT NULL)", &[])
            .unwrap();
        conn.execute("CREATE INDEX t_tag ON t (tag)", &[]).unwrap();
        for id in 1..=K {
            conn.execute("INSERT INTO t (id, tag) VALUES (?1, ?2)", &params![id, id * 10])
                .unwrap();
            commit_ends.push(fs::metadata(&path).unwrap().len());
        }
    }
    let bytes = fs::read(&path).unwrap();

    // Puntos de corte: cada frontera exacta de commit, y un punto INTERMEDIO entre
    // fronteras consecutivas (cola desgarrada a media commit).
    let mut cuts: Vec<u64> = Vec::new();
    for (i, &end) in commit_ends.iter().enumerate() {
        cuts.push(end); // frontera exacta
        if i + 1 < commit_ends.len() {
            cuts.push((end + commit_ends[i + 1]) / 2); // a media commit siguiente
        }
    }

    for cut in cuts {
        let cut = cut.min(bytes.len() as u64);
        let work = dir.path().join(format!("cut_{cut}.arkeion"));
        fs::write(&work, &bytes[..cut as usize]).unwrap();

        let db = Database::open(&work, Options::default()).unwrap();
        // Recuperado = nº de commits cuya frontera entra entera en el corte.
        let recovered = commit_ends.iter().filter(|&&e| e <= cut).count() as i64;

        // Filas: exactamente el prefijo {1..=recovered}, sin huecos ni resurrecciones.
        let ids = row_ids(&db);
        let expected: Vec<i64> = (1..=recovered).collect();
        assert_eq!(ids, expected, "filas != prefijo esperado (cut={cut})");

        // Cadena auditable tras la recuperación.
        assert!(db.verify().unwrap().chain_ok, "cadena rota tras recuperar (cut={cut})");

        // Índice secundario coherente: cada id presente es localizable por su tag, y un
        // tag de una fila NO recuperada no devuelve nada.
        let conn = db.connect().unwrap();
        for id in 1..=recovered {
            let got: i64 = conn
                .query("SELECT id FROM t WHERE tag = ?1", &params![id * 10])
                .unwrap()
                .next()
                .unwrap_or_else(|| panic!("tag {} ausente del índice (cut={cut})", id * 10))
                .unwrap()
                .get(0)
                .unwrap();
            assert_eq!(got, id, "el índice devolvió id equivocado (cut={cut})");
        }
        if recovered < K {
            let absent = conn
                .query("SELECT id FROM t WHERE tag = ?1", &params![(recovered + 1) * 10])
                .unwrap()
                .next()
                .is_some();
            assert!(!absent, "el índice resucitó una fila no recuperada (cut={cut})");
        }
        drop(db);
        fs::remove_file(&work).unwrap();
    }
}
