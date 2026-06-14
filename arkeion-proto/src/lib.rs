//! Protocolo de cable **nativo** de arkeion (M11, cliente-servidor).
//!
//! Hecho a mano, sin serde ni deps de terceros (fiel a D8): reusa el `varint` y
//! la codificación de `Value` del core. Diseño deliberado: la **semántica git y
//! de auditoría son ciudadanos de primera** —rama por sesión, `AS OF`, `verify`—,
//! no añadidos forzados sobre un protocolo ajeno.
//!
//! Framing: cada mensaje es `[u32 LE len][payload]`; el `payload` es
//! `[u8 tag][campos]`. El primer byte discrimina la variante; los campos van con
//! `varint`, cadenas `varint(len)‖utf8` y listas de `Value` con la codificación
//! de registro del core (`varint(len)‖encode_values`).

use std::io::{self, Read, Write};
use std::time::{Duration, UNIX_EPOCH};

use arkeion::AsOf;
use arkeion::Value;
use arkeion::format::{put_varint, take_varint};
use arkeion::record::{decode_values, encode_values};

/// Versión del protocolo que habla esta build. El `Hello`/`Welcome` la negocian;
/// una incompatibilidad se rechaza limpia en vez de malinterpretar bytes.
pub const PROTO_VERSION: u32 = 1;

/// Tope de tamaño de un frame (64 MiB): acota la asignación ante una longitud
/// hostil antes de leer el cuerpo (R7-style: nunca confiar en el otro extremo).
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

// --- tags de mensaje ---
const REQ_HELLO: u8 = 1;
const REQ_USE_BRANCH: u8 = 2;
const REQ_EXECUTE: u8 = 3;
const REQ_QUERY: u8 = 4;
const REQ_VERIFY: u8 = 5;

const RES_WELCOME: u8 = 1;
const RES_AFFECTED: u8 = 2;
const RES_ROWS: u8 = 3;
const RES_AUDIT: u8 = 4;
const RES_ERROR: u8 = 5;

/// Error de protocolo: marco/cuerpo mal formado o E/S. Nunca se entrega un
/// mensaje a medio decodificar.
#[derive(Debug)]
pub enum ProtoError {
    Io(io::Error),
    /// El cuerpo se agotó antes de completar el mensaje.
    Truncated,
    /// Tag de mensaje (o sub-tag) desconocido.
    BadTag(u8),
    /// Una cadena no era UTF-8 o una lista de `Value` no decodificó.
    BadValue,
    /// El frame anunciaba más de [`MAX_FRAME`] bytes.
    FrameTooLarge(usize),
}

impl std::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtoError::Io(e) => write!(f, "E/S de protocolo: {e}"),
            ProtoError::Truncated => write!(f, "mensaje truncado"),
            ProtoError::BadTag(t) => write!(f, "tag de mensaje desconocido: {t}"),
            ProtoError::BadValue => write!(f, "valor o cadena mal formada"),
            ProtoError::FrameTooLarge(n) => write!(f, "frame de {n} B excede el tope"),
        }
    }
}

impl std::error::Error for ProtoError {}

impl From<io::Error> for ProtoError {
    fn from(e: io::Error) -> ProtoError {
        ProtoError::Io(e)
    }
}

// --- framing ---

/// Escribe `payload` enmarcado (`[u32 LE len][payload]`) y hace flush.
pub fn write_frame<W: Write>(w: &mut W, payload: &[u8]) -> io::Result<()> {
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

/// Lee un frame completo. Acota la longitud a [`MAX_FRAME`] **antes** de asignar.
pub fn read_frame<R: Read>(r: &mut R) -> Result<Vec<u8>, ProtoError> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(ProtoError::FrameTooLarge(n));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

// --- helpers de codificación ---

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_varint(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

fn put_vals(out: &mut Vec<u8>, vals: &[Value]) {
    let enc = encode_values(vals);
    put_varint(out, enc.len() as u64);
    out.extend_from_slice(&enc);
}

fn put_asof(out: &mut Vec<u8>, a: &AsOf) {
    match a {
        AsOf::Head => out.push(0),
        AsOf::Version(v) => {
            out.push(1);
            put_varint(out, *v);
        }
        AsOf::Timestamp(t) => {
            out.push(2);
            let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
            put_varint(out, d.as_secs());
            put_varint(out, d.subsec_nanos() as u64);
        }
    }
}

fn take_u8(buf: &[u8], pos: &mut usize) -> Result<u8, ProtoError> {
    let b = *buf.get(*pos).ok_or(ProtoError::Truncated)?;
    *pos += 1;
    Ok(b)
}

fn take_u32(buf: &[u8], pos: &mut usize) -> Result<u32, ProtoError> {
    let s = buf.get(*pos..*pos + 4).ok_or(ProtoError::Truncated)?;
    *pos += 4;
    Ok(u32::from_le_bytes(s.try_into().expect("rango fijo de 4")))
}

fn take_varint_e(buf: &[u8], pos: &mut usize) -> Result<u64, ProtoError> {
    take_varint(buf, pos).ok_or(ProtoError::Truncated)
}

fn take_str(buf: &[u8], pos: &mut usize) -> Result<String, ProtoError> {
    let n = take_varint_e(buf, pos)? as usize;
    let bytes = buf.get(*pos..*pos + n).ok_or(ProtoError::Truncated)?;
    *pos += n;
    String::from_utf8(bytes.to_vec()).map_err(|_| ProtoError::BadValue)
}

fn take_vals(buf: &[u8], pos: &mut usize) -> Result<Vec<Value>, ProtoError> {
    let n = take_varint_e(buf, pos)? as usize;
    let bytes = buf.get(*pos..*pos + n).ok_or(ProtoError::Truncated)?;
    *pos += n;
    decode_values(bytes).map_err(|_| ProtoError::BadValue)
}

fn take_asof(buf: &[u8], pos: &mut usize) -> Result<AsOf, ProtoError> {
    match take_u8(buf, pos)? {
        0 => Ok(AsOf::Head),
        1 => Ok(AsOf::Version(take_varint_e(buf, pos)?)),
        2 => {
            let secs = take_varint_e(buf, pos)?;
            let nanos = take_varint_e(buf, pos)? as u32;
            Ok(AsOf::Timestamp(UNIX_EPOCH + Duration::new(secs, nanos)))
        }
        other => Err(ProtoError::BadTag(other)),
    }
}

// --- mensajes cliente → servidor ---

/// Petición del cliente. `Query` lleva su `AS OF` —el time-travel es de primera,
/// no una variable de sesión escondida.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    /// Handshake: la versión de protocolo del cliente.
    Hello { version: u32 },
    /// Fija la rama de la sesión (por defecto `main`).
    UseBranch(String),
    /// DDL/DML; responde [`Response::Affected`].
    Execute { sql: String, params: Vec<Value> },
    /// Consulta de lectura en `as_of`; responde [`Response::Rows`].
    Query {
        sql: String,
        params: Vec<Value>,
        as_of: AsOf,
    },
    /// Recorre la cadena de hash; responde [`Response::Audit`].
    Verify,
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        match self {
            Request::Hello { version } => {
                o.push(REQ_HELLO);
                o.extend_from_slice(&version.to_le_bytes());
            }
            Request::UseBranch(name) => {
                o.push(REQ_USE_BRANCH);
                put_str(&mut o, name);
            }
            Request::Execute { sql, params } => {
                o.push(REQ_EXECUTE);
                put_str(&mut o, sql);
                put_vals(&mut o, params);
            }
            Request::Query { sql, params, as_of } => {
                o.push(REQ_QUERY);
                put_str(&mut o, sql);
                put_vals(&mut o, params);
                put_asof(&mut o, as_of);
            }
            Request::Verify => o.push(REQ_VERIFY),
        }
        o
    }

    pub fn decode(buf: &[u8]) -> Result<Request, ProtoError> {
        let mut pos = 0;
        let req = match take_u8(buf, &mut pos)? {
            REQ_HELLO => Request::Hello {
                version: take_u32(buf, &mut pos)?,
            },
            REQ_USE_BRANCH => Request::UseBranch(take_str(buf, &mut pos)?),
            REQ_EXECUTE => Request::Execute {
                sql: take_str(buf, &mut pos)?,
                params: take_vals(buf, &mut pos)?,
            },
            REQ_QUERY => Request::Query {
                sql: take_str(buf, &mut pos)?,
                params: take_vals(buf, &mut pos)?,
                as_of: take_asof(buf, &mut pos)?,
            },
            REQ_VERIFY => Request::Verify,
            other => return Err(ProtoError::BadTag(other)),
        };
        Ok(req)
    }
}

// --- mensajes servidor → cliente ---

/// Respuesta del servidor.
#[derive(Clone, Debug, PartialEq)]
pub enum Response {
    /// Handshake aceptado: versión del servidor y rama activa.
    Welcome { version: u32, branch: String },
    /// Filas afectadas por un `Execute`.
    Affected(u64),
    /// Resultado de un `Query`.
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
    },
    /// Informe de auditoría: cabeza, nº de commits, integridad de la cadena y su
    /// hash —el cliente puede comprobar que el servidor no ha manipulado nada—.
    Audit {
        head: u64,
        commits: u64,
        chain_ok: bool,
        chain_hash: [u8; 32],
    },
    /// Error tipado (el `Display` del error del motor, ya legible).
    Error(String),
}

impl Response {
    pub fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        match self {
            Response::Welcome { version, branch } => {
                o.push(RES_WELCOME);
                o.extend_from_slice(&version.to_le_bytes());
                put_str(&mut o, branch);
            }
            Response::Affected(n) => {
                o.push(RES_AFFECTED);
                put_varint(&mut o, *n);
            }
            Response::Rows { columns, rows } => {
                o.push(RES_ROWS);
                put_varint(&mut o, columns.len() as u64);
                for c in columns {
                    put_str(&mut o, c);
                }
                put_varint(&mut o, rows.len() as u64);
                for r in rows {
                    put_vals(&mut o, r);
                }
            }
            Response::Audit {
                head,
                commits,
                chain_ok,
                chain_hash,
            } => {
                o.push(RES_AUDIT);
                put_varint(&mut o, *head);
                put_varint(&mut o, *commits);
                o.push(u8::from(*chain_ok));
                o.extend_from_slice(chain_hash);
            }
            Response::Error(msg) => {
                o.push(RES_ERROR);
                put_str(&mut o, msg);
            }
        }
        o
    }

    pub fn decode(buf: &[u8]) -> Result<Response, ProtoError> {
        let mut pos = 0;
        let res = match take_u8(buf, &mut pos)? {
            RES_WELCOME => Response::Welcome {
                version: take_u32(buf, &mut pos)?,
                branch: take_str(buf, &mut pos)?,
            },
            RES_AFFECTED => Response::Affected(take_varint_e(buf, &mut pos)?),
            RES_ROWS => {
                let ncols = take_varint_e(buf, &mut pos)? as usize;
                let mut columns = Vec::with_capacity(ncols);
                for _ in 0..ncols {
                    columns.push(take_str(buf, &mut pos)?);
                }
                let nrows = take_varint_e(buf, &mut pos)? as usize;
                let mut rows = Vec::with_capacity(nrows);
                for _ in 0..nrows {
                    rows.push(take_vals(buf, &mut pos)?);
                }
                Response::Rows { columns, rows }
            }
            RES_AUDIT => {
                let head = take_varint_e(buf, &mut pos)?;
                let commits = take_varint_e(buf, &mut pos)?;
                let chain_ok = take_u8(buf, &mut pos)? != 0;
                let bytes = buf.get(pos..pos + 32).ok_or(ProtoError::Truncated)?;
                let mut chain_hash = [0u8; 32];
                chain_hash.copy_from_slice(bytes);
                // `chain_hash` es el último campo; no hace falta avanzar `pos`.
                Response::Audit {
                    head,
                    commits,
                    chain_ok,
                    chain_hash,
                }
            }
            RES_ERROR => Response::Error(take_str(buf, &mut pos)?),
            other => return Err(ProtoError::BadTag(other)),
        };
        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req_roundtrip(r: Request) {
        assert_eq!(Request::decode(&r.encode()).unwrap(), r);
    }
    fn res_roundtrip(r: Response) {
        assert_eq!(Response::decode(&r.encode()).unwrap(), r);
    }

    #[test]
    fn requests_roundtrip() {
        req_roundtrip(Request::Hello { version: 1 });
        req_roundtrip(Request::UseBranch("migracion-iva".into()));
        req_roundtrip(Request::Execute {
            sql: "INSERT INTO t VALUES (?1)".into(),
            params: vec![Value::Integer(42), Value::Text("ñ 🐢".into()), Value::Null],
        });
        req_roundtrip(Request::Query {
            sql: "SELECT * FROM t".into(),
            params: vec![],
            as_of: AsOf::Head,
        });
        req_roundtrip(Request::Query {
            sql: "SELECT 1".into(),
            params: vec![Value::Real(-1.5), Value::Bool(true)],
            as_of: AsOf::Version(7),
        });
        req_roundtrip(Request::Query {
            sql: "SELECT 1".into(),
            params: vec![],
            as_of: AsOf::Timestamp(UNIX_EPOCH + Duration::new(1_718_000_000, 123)),
        });
        req_roundtrip(Request::Verify);
    }

    #[test]
    fn responses_roundtrip() {
        res_roundtrip(Response::Welcome {
            version: 1,
            branch: "main".into(),
        });
        res_roundtrip(Response::Affected(3));
        res_roundtrip(Response::Rows {
            columns: vec!["id".into(), "nombre".into()],
            rows: vec![
                vec![Value::Integer(1), Value::Text("Acme".into())],
                vec![Value::Integer(2), Value::Null],
            ],
        });
        res_roundtrip(Response::Audit {
            head: 10,
            commits: 11,
            chain_ok: true,
            chain_hash: [7u8; 32],
        });
        res_roundtrip(Response::Error("tabla desconocida".into()));
    }

    #[test]
    fn empty_rows_and_params_roundtrip() {
        res_roundtrip(Response::Rows {
            columns: vec![],
            rows: vec![],
        });
        req_roundtrip(Request::Execute {
            sql: "BEGIN".into(),
            params: vec![],
        });
    }

    #[test]
    fn malformed_is_error_not_panic() {
        assert!(matches!(Request::decode(&[]), Err(ProtoError::Truncated)));
        assert!(matches!(
            Request::decode(&[99]),
            Err(ProtoError::BadTag(99))
        ));
        // Execute que promete una cadena más larga que el cuerpo.
        assert!(Request::decode(&[REQ_EXECUTE, 200]).is_err());
        assert!(matches!(Response::decode(&[]), Err(ProtoError::Truncated)));
    }

    #[test]
    fn frame_roundtrip_and_bound() {
        let payload = Request::Verify.encode();
        let mut buf = Vec::new();
        write_frame(&mut buf, &payload).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        assert_eq!(read_frame(&mut cursor).unwrap(), payload);

        // Una longitud hostil se rechaza antes de asignar.
        let mut huge = (MAX_FRAME as u32 + 1).to_le_bytes().to_vec();
        huge.extend_from_slice(&[0, 0]);
        let mut c = std::io::Cursor::new(huge);
        assert!(matches!(
            read_frame(&mut c),
            Err(ProtoError::FrameTooLarge(_))
        ));
    }
}
