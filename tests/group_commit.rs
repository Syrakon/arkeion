//! Group commit: muchos committers concurrentes, cada uno con su propia
//! transacción durable, comparten fsync sin perder ni duplicar nada. El escritor
//! único serializa la write-phase (de ahí el reintento ante `Busy`); el fsync se
//! agrupa fuera del lock. Se verifica corrección bajo carga **y** durabilidad tras
//! reabrir el archivo.

use std::thread;

use arkeion::{Database, Error, Options, params};

#[test]
fn concurrent_durable_commits_all_persist_and_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gc.arkeion");
    let db = Database::open(&path, Options::default()).unwrap();
    db.connect()
        .unwrap()
        .execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, who INTEGER NOT NULL)",
            &[],
        )
        .unwrap();

    const THREADS: i64 = 8;
    const PER: i64 = 250;

    let mut handles = Vec::new();
    for who in 0..THREADS {
        let db = db.clone();
        handles.push(thread::spawn(move || {
            let conn = db.connect().unwrap();
            for seq in 0..PER {
                let id = who * PER + seq + 1; // único en todos los hilos
                // El escritor único es exclusivo durante la write-phase: si está
                // ocupado, `Busy`; reintentar (la ventana es de microsegundos
                // porque el fsync ya no se hace con el lock retenido).
                loop {
                    match conn.execute("INSERT INTO t (id, who) VALUES (?1, ?2)", &params![id, who]) {
                        Ok(_) => break,
                        Err(Error::Busy) => {
                            std::hint::spin_loop();
                            continue;
                        }
                        Err(e) => panic!("insert ({who},{seq}) falló: {e}"),
                    }
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let total = THREADS * PER;

    // Todas las filas presentes, sin duplicados, una por id.
    let conn = db.connect().unwrap();
    let count: i64 = first_i64(&conn, "SELECT COUNT(*) FROM t");
    assert_eq!(count, total, "faltan o sobran filas en caliente");
    let distinct: i64 = first_i64(&conn, "SELECT COUNT(DISTINCT id) FROM t");
    assert_eq!(distinct, total, "hay ids duplicados");

    // Versión = nº de commits: 1 (CREATE) + un commit por INSERT. El escritor único
    // serializa la asignación de versión, así que no hay huecos ni colisiones.
    assert_eq!(conn.version(), (total + 1) as u64);
    drop(conn);

    // Durabilidad real: cerrar y reabrir el archivo; todo sigue ahí.
    drop(db);
    let db2 = Database::open(&path, Options::default()).unwrap();
    let conn2 = db2.connect().unwrap();
    assert_eq!(
        first_i64(&conn2, "SELECT COUNT(*) FROM t"),
        total,
        "se perdieron commits tras reabrir"
    );
    assert_eq!(conn2.version(), (total + 1) as u64);
    // La cadena de auditoría recorre los `total + 1` commits sin romperse.
    let audit = db2.verify().unwrap();
    assert!(audit.chain_ok);
    assert_eq!(audit.head, (total + 1) as u64);
}

/// El autocommit hace cola (bloquea) por el escritor, pero una transacción
/// **explícita** conserva la semántica `Busy` (no bloquea): retiene el escritor un
/// tiempo indefinido y no debe colgar a otros. Determinista: con una tx abierta el
/// escritor está retenido.
#[test]
fn explicit_begin_still_returns_busy_while_a_transaction_holds_the_writer() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("busy.arkeion"), Options::default()).unwrap();
    let a = db.connect().unwrap();
    a.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", &[])
        .unwrap();

    let tx = a.begin().unwrap(); // retiene el escritor único hasta commit/rollback

    // Otra conexión: un BEGIN explícito NO bloquea, devuelve Busy de inmediato.
    let b = db.connect().unwrap();
    assert!(matches!(b.begin().err(), Some(Error::Busy)));

    // Al soltar la tx, el escritor queda libre y ya se puede abrir otra.
    drop(tx);
    assert!(b.begin().is_ok());
}

fn first_i64(conn: &arkeion::Connection, sql: &str) -> i64 {
    match conn.query(sql, &[]).unwrap().next().unwrap().unwrap().get(0) {
        Ok(v) => v,
        Err(e) => panic!("query {sql}: {e}"),
    }
}
