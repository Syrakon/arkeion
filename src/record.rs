//! Valores tipados y codificación de registros (docs/02-formato-archivo.md).
//!
//! ```text
//! registro: [ncols varint][tag u8 × ncols][payloads en orden]
//! tags:     0 NULL · 1 FALSE · 2 TRUE · 3 INTEGER (varint zigzag)
//!           4 REAL (f64 LE, 8 B) · 5 TEXT (varint len + UTF-8) · 6 BLOB (varint len + bytes)
//! ```
//!
//! Columnas ausentes al final = NULL (deja sitio a `ALTER TABLE ADD COLUMN`
//! sin reescribir filas, v1.1).

use crate::error::{Error, Result};
use crate::format::{put_varint, take_varint};

/// Valor de una celda. Será el `Value` de la API pública en M3.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "NULL",
            Value::Bool(_) => "BOOLEAN",
            Value::Integer(_) => "INTEGER",
            Value::Real(_) => "REAL",
            Value::Text(_) => "TEXT",
            Value::Blob(_) => "BLOB",
        }
    }
}

/// Vista **prestada** de un [`Value`] ya resuelto (validación, defaults y
/// promoción aplicados): escalares por valor, payloads de texto/blob por
/// referencia. Permite codificar un registro sin materializarlo — ni un clone
/// de `String`/`Vec<u8>` por fila en el camino caliente (M10-perf, fase 2).
#[derive(Clone, Copy, Debug)]
pub enum ValueRef<'a> {
    Null,
    Bool(bool),
    Integer(i64),
    Real(f64),
    Text(&'a str),
    Blob(&'a [u8]),
}

impl<'a> ValueRef<'a> {
    pub fn of(v: &'a Value) -> ValueRef<'a> {
        match v {
            Value::Null => ValueRef::Null,
            Value::Bool(b) => ValueRef::Bool(*b),
            Value::Integer(n) => ValueRef::Integer(*n),
            Value::Real(f) => ValueRef::Real(*f),
            Value::Text(s) => ValueRef::Text(s),
            Value::Blob(b) => ValueRef::Blob(b),
        }
    }

    /// Materializa el valor (clona los payloads prestados).
    pub fn to_value(self) -> Value {
        match self {
            ValueRef::Null => Value::Null,
            ValueRef::Bool(b) => Value::Bool(b),
            ValueRef::Integer(n) => Value::Integer(n),
            ValueRef::Real(f) => Value::Real(f),
            ValueRef::Text(s) => Value::Text(s.to_owned()),
            ValueRef::Blob(b) => Value::Blob(b.to_vec()),
        }
    }
}

const TAG_NULL: u8 = 0;
const TAG_FALSE: u8 = 1;
const TAG_TRUE: u8 = 2;
const TAG_INT: u8 = 3;
const TAG_REAL: u8 = 4;
const TAG_TEXT: u8 = 5;
const TAG_BLOB: u8 = 6;

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

pub fn encode_values(values: &[Value]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + values.len() * 8);
    encode_values_into(values, &mut out);
    out
}

/// Como [`encode_values`] pero en un buffer **reutilizado** (lo limpia antes):
/// el camino caliente de `insert_row` evita asignar un `Vec` por fila (M10-perf).
pub fn encode_values_into(values: &[Value], out: &mut Vec<u8>) {
    encode_resolved_into(values.len(), |i| Ok(ValueRef::of(&values[i])), out)
        .expect("el cierre es infalible");
}

/// Como [`encode_values_into`] pero **sin materializar** el registro: `col(i)`
/// resuelve la columna `i` y puede fallar (validación inline). Dos pasadas
/// sobre el resolutor — tags y payloads — porque el formato pone todos los
/// tags por delante; resolver dos veces es O(1) por columna y evita el `Vec`
/// intermedio. Si `col` falla, `out` queda a medias (cada uso lo limpia antes).
pub fn encode_resolved_into<'a, F>(ncols: usize, mut col: F, out: &mut Vec<u8>) -> Result<()>
where
    F: FnMut(usize) -> Result<ValueRef<'a>>,
{
    out.clear();
    put_varint(out, ncols as u64);
    for i in 0..ncols {
        out.push(match col(i)? {
            ValueRef::Null => TAG_NULL,
            ValueRef::Bool(false) => TAG_FALSE,
            ValueRef::Bool(true) => TAG_TRUE,
            ValueRef::Integer(_) => TAG_INT,
            ValueRef::Real(_) => TAG_REAL,
            ValueRef::Text(_) => TAG_TEXT,
            ValueRef::Blob(_) => TAG_BLOB,
        });
    }
    for i in 0..ncols {
        match col(i)? {
            ValueRef::Null | ValueRef::Bool(_) => {}
            ValueRef::Integer(n) => put_varint(out, zigzag(n)),
            ValueRef::Real(f) => out.extend_from_slice(&f.to_le_bytes()),
            ValueRef::Text(s) => {
                put_varint(out, s.len() as u64);
                out.extend_from_slice(s.as_bytes());
            }
            ValueRef::Blob(b) => {
                put_varint(out, b.len() as u64);
                out.extend_from_slice(b);
            }
        }
    }
    Ok(())
}

pub fn decode_values(buf: &[u8]) -> Result<Vec<Value>> {
    let bad = |reason: &'static str| Error::CorruptRecord(reason);
    let mut pos = 0usize;
    let ncols = take_varint(buf, &mut pos).ok_or(bad("cabecera de registro truncada"))? as usize;
    // Los tags se prestan de `buf` (lectura inmutable); el resto del registro se
    // lee por `pos`, también inmutable, así que no hace falta copiarlos a un Vec.
    let tags = buf.get(pos..pos + ncols).ok_or(bad("tags truncados"))?;
    pos += ncols;

    let mut values = Vec::with_capacity(ncols);
    for &tag in tags {
        values.push(decode_payload(buf, &mut pos, tag)?);
    }
    if pos != buf.len() {
        return Err(bad("bytes sobrantes tras el registro"));
    }
    Ok(values)
}

/// Decodifica el payload del tag `tag` en `pos` (avanzándolo). Único sitio que
/// conoce la forma de cada payload junto a [`skip_payload`].
fn decode_payload(buf: &[u8], pos: &mut usize, tag: u8) -> Result<Value> {
    let bad = |reason: &'static str| Error::CorruptRecord(reason);
    Ok(match tag {
        TAG_NULL => Value::Null,
        TAG_FALSE => Value::Bool(false),
        TAG_TRUE => Value::Bool(true),
        TAG_INT => Value::Integer(unzigzag(
            take_varint(buf, pos).ok_or(bad("entero truncado"))?,
        )),
        TAG_REAL => {
            let bytes = buf.get(*pos..*pos + 8).ok_or(bad("real truncado"))?;
            *pos += 8;
            Value::Real(f64::from_le_bytes(
                bytes.try_into().expect("rango fijo de 8 bytes"),
            ))
        }
        TAG_TEXT => {
            let len = take_varint(buf, pos).ok_or(bad("longitud de texto truncada"))?;
            let bytes = buf
                .get(*pos..*pos + len as usize)
                .ok_or(bad("texto truncado"))?;
            *pos += len as usize;
            Value::Text(String::from_utf8(bytes.to_vec()).map_err(|_| bad("texto no UTF-8"))?)
        }
        TAG_BLOB => {
            let len = take_varint(buf, pos).ok_or(bad("longitud de blob truncada"))?;
            let bytes = buf
                .get(*pos..*pos + len as usize)
                .ok_or(bad("blob truncado"))?;
            *pos += len as usize;
            Value::Blob(bytes.to_vec())
        }
        _ => return Err(bad("tag de valor desconocido")),
    })
}

/// Salta el payload del tag `tag` sin materializarlo (solo avanza `pos`).
fn skip_payload(buf: &[u8], pos: &mut usize, tag: u8) -> Result<()> {
    let bad = |reason: &'static str| Error::CorruptRecord(reason);
    match tag {
        TAG_NULL | TAG_FALSE | TAG_TRUE => {}
        TAG_INT => {
            take_varint(buf, pos).ok_or(bad("entero truncado"))?;
        }
        TAG_REAL => {
            if buf.len() < *pos + 8 {
                return Err(bad("real truncado"));
            }
            *pos += 8;
        }
        TAG_TEXT | TAG_BLOB => {
            let len = take_varint(buf, pos).ok_or(bad("longitud truncada"))? as usize;
            if buf.len() < *pos + len {
                return Err(bad("payload truncado"));
            }
            *pos += len;
        }
        _ => return Err(bad("tag de valor desconocido")),
    }
    Ok(())
}

/// Bytes **prestados** del valor de la columna `col` si es un BLOB (sin asignar),
/// o `None` si es NULL / no-blob / más allá del registro. El camino caliente del
/// KNN: leer el vector empaquetado sin materializar un `Vec` por fila.
pub fn col_blob_bytes(buf: &[u8], col: usize) -> Result<Option<&[u8]>> {
    let bad = |r: &'static str| Error::CorruptRecord(r);
    let mut pos = 0usize;
    let ncols = take_varint(buf, &mut pos).ok_or(bad("cabecera de registro truncada"))? as usize;
    if col >= ncols {
        return Ok(None);
    }
    let tags = buf.get(pos..pos + ncols).ok_or(bad("tags truncados"))?;
    let col_tag = tags[col];
    pos += ncols;
    for &tag in &tags[..col] {
        skip_payload(buf, &mut pos, tag)?;
    }
    if col_tag != TAG_BLOB {
        return Ok(None);
    }
    let len = take_varint(buf, &mut pos).ok_or(bad("longitud de blob truncada"))? as usize;
    Ok(Some(buf.get(pos..pos + len).ok_or(bad("blob truncado"))?))
}

/// Decodifica SOLO las columnas `wanted` (índices **estrictamente crecientes**)
/// de un registro, en una pasada: los payloads no pedidos se saltan sin
/// materializar y la pasada corta tras la última pedida. Las columnas más allá
/// del registro (añadidas por `ALTER TABLE` tras escribir la fila) salen como
/// `None` — el llamador aplica su DEFAULT, como `finish_row`. El camino
/// caliente del full scan en streaming.
pub fn decode_cols_sorted(
    buf: &[u8],
    wanted: &[usize],
    out: &mut Vec<Option<Value>>,
) -> Result<()> {
    let bad = |reason: &'static str| Error::CorruptRecord(reason);
    out.clear();
    let mut pos = 0usize;
    let ncols = take_varint(buf, &mut pos).ok_or(bad("cabecera de registro truncada"))? as usize;
    let tags = buf.get(pos..pos + ncols).ok_or(bad("tags truncados"))?;
    pos += ncols;
    let mut w = 0;
    for (i, &tag) in tags.iter().enumerate() {
        if w == wanted.len() {
            break; // todo lo pedido ya está: el resto del registro no se toca
        }
        if wanted[w] == i {
            out.push(Some(decode_payload(buf, &mut pos, tag)?));
            w += 1;
        } else {
            skip_payload(buf, &mut pos, tag)?;
        }
    }
    while w < wanted.len() {
        out.push(None); // columna posterior al registro: DEFAULT del llamador
        w += 1;
    }
    Ok(())
}

// --- rowid memcomparable ---

const SIGN: u64 = 1 << 63;

/// `i64` con el bit de signo invertido, en big-endian: el orden de bytes
/// coincide con el orden numérico (los negativos van antes).
pub fn rowid_be(rowid: i64) -> [u8; 8] {
    ((rowid as u64) ^ SIGN).to_be_bytes()
}

pub fn rowid_from_be(bytes: &[u8]) -> Option<i64> {
    let arr: [u8; 8] = bytes.try_into().ok()?;
    Some((u64::from_be_bytes(arr) ^ SIGN) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all_types_and_limits() {
        let values = vec![
            Value::Null,
            Value::Bool(false),
            Value::Bool(true),
            Value::Integer(0),
            Value::Integer(-1),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Real(0.0),
            Value::Real(-0.0),
            Value::Real(f64::INFINITY),
            Value::Real(f64::NEG_INFINITY),
            Value::Text(String::new()),
            Value::Text("con\0nul y ñ y 🐢".to_owned()),
            Value::Blob(Vec::new()),
            Value::Blob((0..10_000u32).map(|i| (i % 251) as u8).collect()),
        ];
        let decoded = decode_values(&encode_values(&values)).unwrap();
        assert_eq!(decoded, values);

        // NaN: la igualdad f64 no sirve; comparar el patrón de bits.
        let nan = encode_values(&[Value::Real(f64::NAN)]);
        match &decode_values(&nan).unwrap()[0] {
            Value::Real(f) => assert_eq!(f.to_bits(), f64::NAN.to_bits()),
            other => panic!("se esperaba Real, llegó {other:?}"),
        }
    }

    #[test]
    fn small_negative_integers_encode_short() {
        // zigzag: enteros pequeños (también negativos) en 1 byte de payload.
        let one = encode_values(&[Value::Integer(-1)]);
        assert_eq!(one.len(), 1 + 1 + 1); // ncols + tag + payload
    }

    #[test]
    fn empty_record() {
        assert_eq!(decode_values(&encode_values(&[])).unwrap(), vec![]);
    }

    #[test]
    fn corrupt_records_are_rejected() {
        assert!(decode_values(&[5]).is_err()); // promete 5 columnas, no hay tags
        assert!(decode_values(&[1, TAG_TEXT, 200]).is_err()); // texto truncado
        assert!(decode_values(&[1, 99]).is_err()); // tag desconocido
        let mut extra = encode_values(&[Value::Null]);
        extra.push(0xAB); // bytes sobrantes
        assert!(decode_values(&extra).is_err());
        assert!(decode_values(&encode_values(&[Value::Text("ok".into())])[..3]).is_err());
    }

    #[test]
    fn rowid_order_is_memcomparable() {
        let ids = [i64::MIN, -1_000_000, -5, -1, 0, 1, 5, 1_000_000, i64::MAX];
        let encoded: Vec<[u8; 8]> = ids.iter().map(|&i| rowid_be(i)).collect();
        let mut sorted = encoded.clone();
        sorted.sort();
        assert_eq!(
            encoded, sorted,
            "el orden de bytes debe seguir el orden numérico"
        );
        for &i in &ids {
            assert_eq!(rowid_from_be(&rowid_be(i)), Some(i));
        }
    }
}
