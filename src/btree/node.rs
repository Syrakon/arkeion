//! Codificación de nodos B-tree y páginas overflow (docs/02-formato-archivo.md).
//!
//! v1 decodifica nodos completos a vectores de celdas y los reencodifica al
//! mutar: O(página) por operación, correcto y simple. El array de punteros de
//! celda (búsqueda binaria in-page) queda como optimización futura.

use crate::error::{Error, Result};
use crate::format::{BODY_SIZE, PageId};

pub const TYPE_LEAF: u8 = 0x01;
pub const TYPE_INNER: u8 = 0x02;
pub const TYPE_OVERFLOW: u8 = 0x03;

/// Longitud máxima de clave: los nodos internos deben alojar varias por página.
pub const MAX_KEY_LEN: usize = 1024;
/// Una celda de hoja mayor que esto manda el valor a overflow (≥3 celdas/página).
pub const MAX_INLINE_CELL: usize = 1280;

const LEAF_HDR: usize = 4; // type, flags, ncells u16
const INNER_HDR: usize = 12; // type, flags, ncells u16, rightmost u64
pub const OVERFLOW_HDR: usize = 12; // type, flags, len u16, next u64
/// Bytes de datos por página overflow.
pub const OVERFLOW_DATA: usize = BODY_SIZE - OVERFLOW_HDR;

const CELL_OVERFLOW_FLAG: u8 = 0x01;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Payload {
    Inline(Vec<u8>),
    Overflow { total_len: u64, first: PageId },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeafCell {
    pub key: Vec<u8>,
    pub payload: Payload,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InnerCell {
    /// Cota superior **exclusiva** de las claves de `child`.
    pub key: Vec<u8>,
    pub child: PageId,
}

pub fn node_type(body: &[u8]) -> u8 {
    body[0]
}

fn corrupt(page: u64, reason: &'static str) -> Error {
    Error::Corrupt { page, reason }
}

// --- varint (compartido en format) con error contextualizado ---

use crate::format::{put_varint, varint_len};

fn get_varint(page: u64, buf: &[u8], pos: &mut usize) -> Result<u64> {
    crate::format::take_varint(buf, pos).ok_or(corrupt(page, "varint inválido"))
}

fn get_slice<'a>(page: u64, buf: &'a [u8], pos: &mut usize, len: usize) -> Result<&'a [u8]> {
    let s = buf
        .get(*pos..*pos + len)
        .ok_or(corrupt(page, "celda truncada"))?;
    *pos += len;
    Ok(s)
}

fn get_u64(page: u64, buf: &[u8], pos: &mut usize) -> Result<u64> {
    let s = get_slice(page, buf, pos, 8)?;
    Ok(u64::from_le_bytes(
        s.try_into().expect("rango fijo de 8 bytes"),
    ))
}

// --- tamaños (para decidir splits sin codificar) ---

pub fn leaf_cell_size(c: &LeafCell) -> usize {
    let payload = match &c.payload {
        Payload::Inline(v) => varint_len(v.len() as u64) + v.len(),
        Payload::Overflow { total_len, .. } => varint_len(*total_len) + 8,
    };
    1 + varint_len(c.key.len() as u64) + c.key.len() + payload
}

pub fn inner_cell_size(c: &InnerCell) -> usize {
    varint_len(c.key.len() as u64) + c.key.len() + 8
}

/// Tamaño que tendría una celda de hoja con el valor inline: decide overflow.
pub fn inline_cell_size(key: &[u8], value_len: usize) -> usize {
    1 + varint_len(key.len() as u64) + key.len() + varint_len(value_len as u64) + value_len
}

// --- hoja ---

pub fn parse_leaf(page: u64, body: &[u8]) -> Result<Vec<LeafCell>> {
    if body[0] != TYPE_LEAF {
        return Err(corrupt(page, "se esperaba una hoja"));
    }
    let ncells = u16::from_le_bytes([body[2], body[3]]) as usize;
    let mut pos = LEAF_HDR;
    let mut cells: Vec<LeafCell> = Vec::with_capacity(ncells);
    for _ in 0..ncells {
        let flags = *body.get(pos).ok_or(corrupt(page, "celda truncada"))?;
        pos += 1;
        let klen = get_varint(page, body, &mut pos)? as usize;
        let key = get_slice(page, body, &mut pos, klen)?.to_vec();
        // Invariantes del árbol como defensa en profundidad: claves no
        // vacías y estrictamente crecientes (una cola de ceros no decodifica).
        if key.is_empty() || cells.last().is_some_and(|p| p.key >= key) {
            return Err(corrupt(page, "claves de hoja desordenadas o vacías"));
        }
        let payload = if flags & CELL_OVERFLOW_FLAG != 0 {
            let total_len = get_varint(page, body, &mut pos)?;
            let first = PageId(get_u64(page, body, &mut pos)?);
            Payload::Overflow { total_len, first }
        } else {
            let vlen = get_varint(page, body, &mut pos)? as usize;
            Payload::Inline(get_slice(page, body, &mut pos, vlen)?.to_vec())
        };
        cells.push(LeafCell { key, payload });
    }
    Ok(cells)
}

/// `false` si las celdas no caben (el llamador hace split). Si caben, escribe
/// el nodo completo y rellena el resto con ceros (contenido determinista).
pub fn encode_leaf(cells: &[LeafCell], body: &mut [u8]) -> bool {
    let mut out = Vec::with_capacity(BODY_SIZE);
    out.extend_from_slice(&[TYPE_LEAF, 0]);
    out.extend_from_slice(&(cells.len() as u16).to_le_bytes());
    for c in cells {
        match &c.payload {
            Payload::Inline(v) => {
                out.push(0);
                put_varint(&mut out, c.key.len() as u64);
                out.extend_from_slice(&c.key);
                put_varint(&mut out, v.len() as u64);
                out.extend_from_slice(v);
            }
            Payload::Overflow { total_len, first } => {
                out.push(CELL_OVERFLOW_FLAG);
                put_varint(&mut out, c.key.len() as u64);
                out.extend_from_slice(&c.key);
                put_varint(&mut out, *total_len);
                out.extend_from_slice(&first.0.to_le_bytes());
            }
        }
    }
    if out.len() > body.len() {
        return false;
    }
    body[..out.len()].copy_from_slice(&out);
    body[out.len()..].fill(0);
    true
}

// --- nodo interno ---

pub fn parse_inner(page: u64, body: &[u8]) -> Result<(Vec<InnerCell>, PageId)> {
    if body[0] != TYPE_INNER {
        return Err(corrupt(page, "se esperaba un nodo interno"));
    }
    let ncells = u16::from_le_bytes([body[2], body[3]]) as usize;
    let rightmost = PageId(u64::from_le_bytes(
        body[4..12].try_into().expect("rango fijo de 8 bytes"),
    ));
    let mut pos = INNER_HDR;
    let mut cells: Vec<InnerCell> = Vec::with_capacity(ncells);
    for _ in 0..ncells {
        let klen = get_varint(page, body, &mut pos)? as usize;
        let key = get_slice(page, body, &mut pos, klen)?.to_vec();
        if key.is_empty() || cells.last().is_some_and(|p| p.key >= key) {
            return Err(corrupt(page, "separadores desordenados o vacíos"));
        }
        let child = PageId(get_u64(page, body, &mut pos)?);
        cells.push(InnerCell { key, child });
    }
    Ok((cells, rightmost))
}

pub fn encode_inner(cells: &[InnerCell], rightmost: PageId, body: &mut [u8]) -> bool {
    let mut out = Vec::with_capacity(BODY_SIZE);
    out.extend_from_slice(&[TYPE_INNER, 0]);
    out.extend_from_slice(&(cells.len() as u16).to_le_bytes());
    out.extend_from_slice(&rightmost.0.to_le_bytes());
    for c in cells {
        put_varint(&mut out, c.key.len() as u64);
        out.extend_from_slice(&c.key);
        out.extend_from_slice(&c.child.0.to_le_bytes());
    }
    if out.len() > body.len() {
        return false;
    }
    body[..out.len()].copy_from_slice(&out);
    body[out.len()..].fill(0);
    true
}

// --- búsqueda in-page sin materializar celdas (camino caliente) ---
//
// El formato es idéntico al de `parse_*`; estas funciones recorren las celdas
// serializadas comparando claves **sin asignar** un `Vec` por celda. Es la
// ganancia del camino de lectura (`get`/`contains`) y del descenso de escritura.
// Los chequeos de orden de `parse_*` (defensa en profundidad) los cubre aquí el
// tag de integridad de la página: un body con tag válido es exactamente el que
// se escribió, con claves ordenadas por construcción.

/// Hijo a seguir en un nodo interno para `key`. Equivale a
/// `parse_inner(..).partition_point(c.key <= key) → child|rightmost`.
pub fn inner_child(page: u64, body: &[u8], key: &[u8]) -> Result<PageId> {
    if body[0] != TYPE_INNER {
        return Err(corrupt(page, "se esperaba un nodo interno"));
    }
    let ncells = u16::from_le_bytes([body[2], body[3]]) as usize;
    let rightmost = PageId(u64::from_le_bytes(
        body[4..12].try_into().expect("rango fijo de 8 bytes"),
    ));
    let mut pos = INNER_HDR;
    for _ in 0..ncells {
        let klen = get_varint(page, body, &mut pos)? as usize;
        let ckey = get_slice(page, body, &mut pos, klen)?;
        let child = PageId(get_u64(page, body, &mut pos)?);
        // Primer separador con clave > `key` (cota superior exclusiva): su hijo
        // cubre `key`. Si ninguno, va al rightmost.
        if ckey > key {
            return Ok(child);
        }
    }
    Ok(rightmost)
}

/// `Some((pos_payload, flags))` con `pos` al inicio del payload de la celda de
/// `key`, o `None` si no está. Escanea sin asignar.
fn leaf_seek(page: u64, body: &[u8], key: &[u8]) -> Result<Option<(usize, u8)>> {
    if body[0] != TYPE_LEAF {
        return Err(corrupt(page, "se esperaba una hoja"));
    }
    let ncells = u16::from_le_bytes([body[2], body[3]]) as usize;
    let mut pos = LEAF_HDR;
    for _ in 0..ncells {
        let flags = *body.get(pos).ok_or(corrupt(page, "celda truncada"))?;
        pos += 1;
        let klen = get_varint(page, body, &mut pos)? as usize;
        let ckey = get_slice(page, body, &mut pos, klen)?;
        match ckey.cmp(key) {
            std::cmp::Ordering::Equal => return Ok(Some((pos, flags))),
            std::cmp::Ordering::Greater => return Ok(None), // ordenadas: ya pasamos el punto
            std::cmp::Ordering::Less => skip_leaf_payload(page, body, &mut pos, flags)?,
        }
    }
    Ok(None)
}

fn skip_leaf_payload(page: u64, body: &[u8], pos: &mut usize, flags: u8) -> Result<()> {
    if flags & CELL_OVERFLOW_FLAG != 0 {
        let _ = get_varint(page, body, pos)?; // total_len
        let _ = get_u64(page, body, pos)?; // first
    } else {
        let vlen = get_varint(page, body, pos)? as usize;
        let _ = get_slice(page, body, pos, vlen)?;
    }
    Ok(())
}

fn read_leaf_payload(page: u64, body: &[u8], pos: &mut usize, flags: u8) -> Result<Payload> {
    if flags & CELL_OVERFLOW_FLAG != 0 {
        let total_len = get_varint(page, body, pos)?;
        let first = PageId(get_u64(page, body, pos)?);
        Ok(Payload::Overflow { total_len, first })
    } else {
        let vlen = get_varint(page, body, pos)? as usize;
        Ok(Payload::Inline(get_slice(page, body, pos, vlen)?.to_vec()))
    }
}

/// Payload de `key` en una hoja, sin materializar las demás celdas.
pub fn leaf_find(page: u64, body: &[u8], key: &[u8]) -> Result<Option<Payload>> {
    match leaf_seek(page, body, key)? {
        Some((mut pos, flags)) => Ok(Some(read_leaf_payload(page, body, &mut pos, flags)?)),
        None => Ok(None),
    }
}

/// Existencia de `key` en una hoja, sin copiar el valor (para dup-checks).
pub fn leaf_contains(page: u64, body: &[u8], key: &[u8]) -> Result<bool> {
    Ok(leaf_seek(page, body, key)?.is_some())
}

/// Codifica **una** celda de hoja, byte a byte igual que `encode_leaf` por celda.
fn encode_leaf_cell(key: &[u8], payload: &Payload) -> Vec<u8> {
    let mut out = Vec::new();
    match payload {
        Payload::Inline(v) => {
            out.push(0);
            put_varint(&mut out, key.len() as u64);
            out.extend_from_slice(key);
            put_varint(&mut out, v.len() as u64);
            out.extend_from_slice(v);
        }
        Payload::Overflow { total_len, first } => {
            out.push(CELL_OVERFLOW_FLAG);
            put_varint(&mut out, key.len() as u64);
            out.extend_from_slice(key);
            put_varint(&mut out, *total_len);
            out.extend_from_slice(&first.0.to_le_bytes());
        }
    }
    out
}

/// Anexa una celda al final de la hoja **in situ**, solo si `key` va después de
/// la última celda (append puro) y cabe en el hueco. El resultado es **idéntico
/// byte a byte** a re-encodar la hoja con la celda añadida (mismas celdas en
/// orden + mismo relleno a cero, que ya estaba ahí), pero sin parsear ni
/// reescribir las celdas existentes. `Ok(Some(fin))` (offset libre tras la celda
/// nueva) si anexó; `Ok(None)` si hay que ir por el camino general (clave no
/// final, o no cabe ⇒ split). Es la optimización de los inserts secuenciales
/// (imports, rowid creciente). Escanea las celdas para hallar el final: el camino
/// O(1) sin escaneo es [`append_inline_at`] (cursor de append).
pub fn leaf_append(
    page: u64,
    body: &mut [u8],
    key: &[u8],
    payload: &Payload,
) -> Result<Option<usize>> {
    if body[0] != TYPE_LEAF {
        return Err(corrupt(page, "se esperaba una hoja"));
    }
    let ncells = u16::from_le_bytes([body[2], body[3]]) as usize;

    // Recorre hasta el final, recordando el rango de la última clave.
    let mut pos = LEAF_HDR;
    let mut last_key: Option<(usize, usize)> = None;
    for _ in 0..ncells {
        let flags = *body.get(pos).ok_or(corrupt(page, "celda truncada"))?;
        pos += 1;
        let klen = get_varint(page, body, &mut pos)? as usize;
        let kstart = pos;
        let _ = get_slice(page, body, &mut pos, klen)?;
        last_key = Some((kstart, kstart + klen));
        skip_leaf_payload(page, body, &mut pos, flags)?;
    }
    let end = pos;

    // Solo es append si la clave es estrictamente mayor que la última.
    if let Some((ks, ke)) = last_key
        && key <= &body[ks..ke]
    {
        return Ok(None);
    }

    let cell = encode_leaf_cell(key, payload);
    if end + cell.len() > body.len() {
        return Ok(None); // no cabe ⇒ camino general (hará split)
    }
    body[end..end + cell.len()].copy_from_slice(&cell);
    body[2..4].copy_from_slice(&((ncells as u16) + 1).to_le_bytes());
    Ok(Some(end + cell.len()))
}

/// Anexa una celda **inline** al final de la hoja en `end` (offset libre ya
/// conocido) sin escanear las celdas existentes: el camino O(1) del **cursor de
/// append** (M10-perf). El llamador garantiza que `key` va después de la última
/// celda y que la hoja es la rightmost (el cursor lo asegura). Devuelve el nuevo
/// offset libre, o `None` si no cabe (⇒ split por el camino general). No asigna.
pub fn append_inline_at(body: &mut [u8], end: usize, key: &[u8], value: &[u8]) -> Option<usize> {
    let cell_len =
        1 + varint_len(key.len() as u64) + key.len() + varint_len(value.len() as u64) + value.len();
    if end + cell_len > body.len() {
        return None;
    }
    let mut p = end;
    body[p] = 0; // flag inline
    p += 1;
    p = put_varint_at(body, p, key.len() as u64);
    body[p..p + key.len()].copy_from_slice(key);
    p += key.len();
    p = put_varint_at(body, p, value.len() as u64);
    body[p..p + value.len()].copy_from_slice(value);
    p += value.len();
    let ncells = u16::from_le_bytes([body[2], body[3]]);
    body[2..4].copy_from_slice(&(ncells + 1).to_le_bytes());
    Some(p)
}

/// Escribe un varint LEB128 en `body[pos..]` y devuelve la posición siguiente
/// (versión sobre slice de [`crate::format::put_varint`], sin asignar).
fn put_varint_at(body: &mut [u8], mut pos: usize, mut v: u64) -> usize {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            body[pos] = b;
            return pos + 1;
        }
        body[pos] = b | 0x80;
        pos += 1;
    }
}

/// Hijo rightmost de un nodo interno (puntero en `body[4..12]`). El llamador ya
/// sabe que es un nodo interno (camino caliente del cursor de append).
pub fn inner_rightmost(body: &[u8]) -> PageId {
    PageId(u64::from_le_bytes(
        body[4..12].try_into().expect("rango fijo de 8 bytes"),
    ))
}

// --- overflow ---

pub fn encode_overflow(chunk: &[u8], next: PageId, body: &mut [u8]) {
    debug_assert!(chunk.len() <= OVERFLOW_DATA);
    body[0] = TYPE_OVERFLOW;
    body[1] = 0;
    body[2..4].copy_from_slice(&(chunk.len() as u16).to_le_bytes());
    body[4..12].copy_from_slice(&next.0.to_le_bytes());
    body[OVERFLOW_HDR..OVERFLOW_HDR + chunk.len()].copy_from_slice(chunk);
    body[OVERFLOW_HDR + chunk.len()..].fill(0);
}

/// (datos del trozo, página siguiente).
pub fn parse_overflow(page: u64, body: &[u8]) -> Result<(&[u8], PageId)> {
    if body[0] != TYPE_OVERFLOW {
        return Err(corrupt(page, "se esperaba una página overflow"));
    }
    let len = u16::from_le_bytes([body[2], body[3]]) as usize;
    let next = PageId(u64::from_le_bytes(
        body[4..12].try_into().expect("rango fijo de 8 bytes"),
    ));
    let data = body
        .get(OVERFLOW_HDR..OVERFLOW_HDR + len)
        .ok_or(corrupt(page, "overflow truncado"))?;
    Ok((data, next))
}

/// Índice de inicio de la mitad derecha de un split: ambas mitades no vacías
/// y de tamaño codificado similar.
pub fn split_point<T>(cells: &[T], size: impl Fn(&T) -> usize) -> usize {
    debug_assert!(cells.len() >= 2);
    let total: usize = cells.iter().map(&size).sum();
    let mut acc = 0;
    for (i, c) in cells.iter().enumerate() {
        acc += size(c);
        if acc * 2 >= total {
            return (i + 1).min(cells.len() - 1);
        }
    }
    cells.len() - 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(key: &[u8], val: &[u8]) -> LeafCell {
        LeafCell {
            key: key.to_vec(),
            payload: Payload::Inline(val.to_vec()),
        }
    }

    #[test]
    fn leaf_roundtrip() {
        let cells = vec![
            cell(b"a", b""),
            cell(b"clave", b"valor"),
            LeafCell {
                key: b"grande".to_vec(),
                payload: Payload::Overflow {
                    total_len: 99_999,
                    first: PageId(42),
                },
            },
        ];
        let mut body = vec![0u8; BODY_SIZE];
        assert!(encode_leaf(&cells, &mut body));
        assert_eq!(parse_leaf(0, &body).unwrap(), cells);
        assert_eq!(node_type(&body), TYPE_LEAF);
    }

    #[test]
    fn leaf_rejects_when_full() {
        let cells: Vec<LeafCell> = (0..10)
            .map(|i| cell(format!("k{i:04}").as_bytes(), &[7u8; 500]))
            .collect();
        let mut body = vec![0u8; BODY_SIZE];
        assert!(!encode_leaf(&cells, &mut body));
        // La mitad sí cabe.
        assert!(encode_leaf(&cells[..5], &mut body));
    }

    #[test]
    fn inner_roundtrip() {
        let cells = vec![
            InnerCell {
                key: b"m".to_vec(),
                child: PageId(7),
            },
            InnerCell {
                key: b"t".to_vec(),
                child: PageId(9),
            },
        ];
        let mut body = vec![0u8; BODY_SIZE];
        assert!(encode_inner(&cells, PageId(11), &mut body));
        let (parsed, rightmost) = parse_inner(0, &body).unwrap();
        assert_eq!(parsed, cells);
        assert_eq!(rightmost, PageId(11));
    }

    /// La búsqueda in-page del descenso debe coincidir con `parse_inner` +
    /// `partition_point` para **toda** clave (el camino caliente no puede
    /// divergir del de referencia).
    #[test]
    fn inner_child_matches_parse_inner() {
        let cells: Vec<InnerCell> = ["d", "h", "m", "s"]
            .iter()
            .enumerate()
            .map(|(i, k)| InnerCell {
                key: k.as_bytes().to_vec(),
                child: PageId(10 + i as u64),
            })
            .collect();
        let mut body = vec![0u8; BODY_SIZE];
        assert!(encode_inner(&cells, PageId(99), &mut body));

        for probe in [
            &b""[..],
            b"a",
            b"d",
            b"dd",
            b"h",
            b"m",
            b"mz",
            b"s",
            b"sz",
            b"zzz",
        ] {
            let (parsed, rightmost) = parse_inner(0, &body).unwrap();
            let idx = parsed.partition_point(|c| c.key.as_slice() <= probe);
            let expected = if idx < parsed.len() {
                parsed[idx].child
            } else {
                rightmost
            };
            assert_eq!(
                inner_child(0, &body, probe).unwrap(),
                expected,
                "descenso diverge para {probe:?}"
            );
        }
    }

    /// `leaf_append` solo anexa cuando la clave va al final y cabe, y produce
    /// **exactamente** los mismos bytes que re-encodar la hoja con la celda.
    #[test]
    fn leaf_append_is_byte_identical_and_rejects_non_append() {
        let base = vec![cell(b"a", b"1"), cell(b"m", b"22"), cell(b"t", b"333")];
        let mut body = vec![0u8; BODY_SIZE];
        assert!(encode_leaf(&base, &mut body));

        // Append (clave > última): byte-idéntico a re-encodar con la celda.
        let mut appended = body.clone();
        assert!(
            leaf_append(0, &mut appended, b"z", &Payload::Inline(b"9".to_vec()))
                .unwrap()
                .is_some()
        );
        let mut expected = base.clone();
        expected.push(cell(b"z", b"9"));
        let mut expected_body = vec![0u8; BODY_SIZE];
        assert!(encode_leaf(&expected, &mut expected_body));
        assert_eq!(
            appended, expected_body,
            "append no es byte-idéntico al re-encode"
        );

        // Append de un payload overflow al final: igual de byte-idéntico.
        let mut over = body.clone();
        let payload = Payload::Overflow {
            total_len: 70_000,
            first: PageId(123),
        };
        assert!(
            leaf_append(0, &mut over, b"zz", &payload)
                .unwrap()
                .is_some()
        );
        let mut expected2 = base.clone();
        expected2.push(LeafCell {
            key: b"zz".to_vec(),
            payload,
        });
        let mut expected2_body = vec![0u8; BODY_SIZE];
        assert!(encode_leaf(&expected2, &mut expected2_body));
        assert_eq!(over, expected2_body);

        // No-append: clave en medio, igual a una existente, o menor que la 1ª.
        for k in [&b"b"[..], b"m", b"0"] {
            let mut b2 = body.clone();
            assert!(
                leaf_append(0, &mut b2, k, &Payload::Inline(b"x".to_vec()))
                    .unwrap()
                    .is_none()
            );
            assert_eq!(b2, body, "no-append no debe tocar el body");
        }

        // Clave final pero que no cabe ⇒ false, body intacto (lo parte el general).
        let mut b3 = body.clone();
        assert!(
            leaf_append(0, &mut b3, b"zzz", &Payload::Inline(vec![7u8; BODY_SIZE]))
                .unwrap()
                .is_none()
        );
        assert_eq!(b3, body);
    }

    /// `leaf_find`/`leaf_contains` deben coincidir con `parse_leaf` +
    /// `binary_search` para claves presentes y ausentes, con payload inline y
    /// overflow.
    #[test]
    fn leaf_find_matches_parse_leaf() {
        let cells = vec![
            cell(b"alfa", b"1"),
            cell(b"beta", b""),
            LeafCell {
                key: b"gamma".to_vec(),
                payload: Payload::Overflow {
                    total_len: 5000,
                    first: PageId(77),
                },
            },
            cell(b"delta", b"4444"),
        ];
        // Las celdas deben ir ordenadas por clave al codificar.
        let mut sorted = cells.clone();
        sorted.sort_by(|a, b| a.key.cmp(&b.key));
        let mut body = vec![0u8; BODY_SIZE];
        assert!(encode_leaf(&sorted, &mut body));

        for probe in [
            &b"aa"[..],
            b"alfa",
            b"beta",
            b"betaa",
            b"delta",
            b"gamma",
            b"gammaz",
            b"zzz",
        ] {
            let parsed = parse_leaf(0, &body).unwrap();
            let expected = parsed
                .binary_search_by(|c| c.key.as_slice().cmp(probe))
                .ok()
                .map(|i| parsed[i].payload.clone());
            assert_eq!(
                leaf_find(0, &body, probe).unwrap(),
                expected,
                "leaf_find diverge para {probe:?}"
            );
            assert_eq!(
                leaf_contains(0, &body, probe).unwrap(),
                expected.is_some(),
                "leaf_contains diverge para {probe:?}"
            );
        }
    }

    #[test]
    fn overflow_roundtrip() {
        let data = vec![3u8; OVERFLOW_DATA];
        let mut body = vec![0u8; BODY_SIZE];
        encode_overflow(&data, PageId(5), &mut body);
        let (chunk, next) = parse_overflow(0, &body).unwrap();
        assert_eq!(chunk, &data[..]);
        assert_eq!(next, PageId(5));
    }

    #[test]
    fn split_point_balances() {
        let cells: Vec<LeafCell> = (0..10).map(|i| cell(&[i], &[0u8; 100])).collect();
        let sp = split_point(&cells, leaf_cell_size);
        assert!(sp >= 1 && sp < cells.len());
        assert!(sp.abs_diff(5) <= 1);
    }

    #[test]
    fn truncated_cells_are_corrupt() {
        let cells = vec![cell(b"clave", b"valor")];
        let mut body = vec![0u8; BODY_SIZE];
        assert!(encode_leaf(&cells, &mut body));
        body[2..4].copy_from_slice(&500u16.to_le_bytes()); // ncells mentiroso
        assert!(parse_leaf(0, &body).is_err());
    }
}
