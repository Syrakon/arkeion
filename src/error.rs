//! Tipos de error públicos del motor.

use std::fmt;

use crate::tx::AsOf;

/// Resultado estándar de Arkeion.
pub type Result<T> = std::result::Result<T, Error>;

/// Error del motor. Toda lectura valida integridad: la corrupción aflora como
/// error tipado, nunca como datos silenciosamente malos.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// Error de E/S del sistema operativo.
    Io(std::io::Error),
    /// El archivo no es una base de datos Arkeion.
    NotADatabase,
    /// El archivo usa una versión de formato más nueva que esta build.
    UnsupportedFormat { version: u32 },
    /// Tamaño de página distinto del soportado.
    UnsupportedPageSize { page_size: u32 },
    /// El archivo está cifrado y no se proporcionó clave.
    KeyRequired,
    /// Corrupción detectada al validar una página.
    Corrupt { page: u64, reason: &'static str },
    /// Argumento inválido del llamador (p. ej. clave demasiado larga).
    InvalidInput(&'static str),
    /// Registro o esquema almacenado mal formado (la integridad de página ya
    /// validó: esto señala una incompatibilidad o un bug, no un disco roto).
    CorruptRecord(&'static str),
    /// Violación de una restricción relacional (PK duplicada, NOT NULL, tipo).
    Constraint(&'static str),
    /// Error de SQL: sintaxis (con posición en bytes) o semántica (sin ella).
    Sql { msg: String, pos: Option<usize> },
    /// `AS OF` apunta a un punto inalcanzable de la historia: versión futura o
    /// ya compactada por `vacuum` (M9).
    VersionNotFound(AsOf),
    /// Conversión `Row::get::<T>` imposible para el valor presente.
    Conversion {
        expected: &'static str,
        got: &'static str,
    },
    /// Otro proceso (u otro handle) mantiene la base de datos abierta.
    Busy,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "error de E/S: {e}"),
            Error::NotADatabase => write!(f, "el archivo no es una base de datos Arkeion"),
            Error::UnsupportedFormat { version } => {
                write!(
                    f,
                    "versión de formato {version} no soportada por esta build"
                )
            }
            Error::UnsupportedPageSize { page_size } => {
                write!(f, "tamaño de página {page_size} no soportado")
            }
            Error::KeyRequired => write!(f, "el archivo está cifrado: se requiere clave"),
            Error::Corrupt { page, reason } => {
                write!(f, "corrupción en la página {page}: {reason}")
            }
            Error::InvalidInput(reason) => write!(f, "argumento inválido: {reason}"),
            Error::CorruptRecord(reason) => write!(f, "registro mal formado: {reason}"),
            Error::Constraint(reason) => write!(f, "restricción violada: {reason}"),
            Error::Sql { msg, pos: Some(p) } => write!(f, "error SQL en byte {p}: {msg}"),
            Error::Sql { msg, pos: None } => write!(f, "error SQL: {msg}"),
            Error::VersionNotFound(AsOf::Version(v)) => {
                write!(
                    f,
                    "la versión {v} no existe (futura o compactada por vacuum)"
                )
            }
            Error::VersionNotFound(_) => {
                write!(
                    f,
                    "no hay ninguna versión para el punto temporal solicitado"
                )
            }
            Error::Conversion { expected, got } => {
                write!(f, "no se puede convertir {got} a {expected}")
            }
            Error::Busy => write!(f, "la base de datos está en uso por otro proceso"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::Io(e)
    }
}
