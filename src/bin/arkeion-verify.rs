//! `arkeion-verify <archivo.arkeion>` — auditoría de consola de la hash chain
//! (M6). Sale 0 si la cadena es íntegra, 1 si está rota, 2 ante un error de uso
//! o de apertura. Abre en solo lectura y **no** crea el archivo.

#![forbid(unsafe_code)]

use std::process::ExitCode;

use arkeion::{Database, Options};

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("uso: arkeion-verify <archivo.arkeion>");
        return ExitCode::from(2);
    };

    let db = match Database::open(&path, Options::default().create_if_missing(false)) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("no se pudo abrir {path}: {e}");
            return ExitCode::from(2);
        }
    };

    match db.verify() {
        Ok(r) => {
            println!(
                "OK: cadena íntegra — head {}, {} commits",
                r.head, r.commits
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("FALLO: {e}");
            ExitCode::FAILURE
        }
    }
}
