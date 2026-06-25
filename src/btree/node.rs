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

/// Bit de `body[1]` (flags de nodo) que marca una hoja con **prefijo común
/// comprimido**: tras la cabecera de 4 B va `[plen varint][prefijo]`, y cada celda
/// guarda solo el **sufijo** de su clave (`key[plen..]`). Backward-compatible: las
/// hojas escritas antes llevan el bit a 0 y se leen igual (prefijo vacío). Solo se
/// comprime al re-encodar (split/delete/insert general); el cursor de append escribe
/// hojas sin comprimir, así que la rightmost activa nunca está comprimida.
const LEAF_PREFIX_FLAG: u8 = 0x01;

pub fn leaf_is_compressed(body: &[u8]) -> bool {
    body[1] & LEAF_PREFIX_FLAG != 0
}

/// `(prefijo común, offset del primer contenido de celda)`. `&[]` y `LEAF_HDR` si la
/// hoja no está comprimida; para comprimidas lee `[plen varint][prefijo]`.
fn leaf_prefix_and_start(page: u64, body: &[u8]) -> Result<(&[u8], usize)> {
    if !leaf_is_compressed(body) {
        return Ok((&[][..], LEAF_HDR));
    }
    let mut pos = LEAF_HDR;
    let plen = get_varint(page, body, &mut pos)? as usize;
    let prefix = get_slice(page, body, &mut pos, plen)?;
    Ok((prefix, pos))
}

/// Relación de un `target` de búsqueda con el prefijo común de la hoja, para
/// comparar contra las claves `prefijo ++ sufijo` sin reconstruirlas (zero-copy):
/// si `target` empieza por el prefijo, basta comparar su resto con los sufijos.
enum PrefixCmp<'a> {
    Below,            // target < TODAS las claves (no empieza por el prefijo, y es menor)
    Above,            // target > TODAS las claves
    Within(&'a [u8]), // target = prefijo ++ resto; comparar `resto` con los sufijos
}

fn cmp_prefix<'a>(prefix: &[u8], target: &'a [u8]) -> PrefixCmp<'a> {
    let n = prefix.len().min(target.len());
    match target[..n].cmp(&prefix[..n]) {
        std::cmp::Ordering::Less => PrefixCmp::Below,
        std::cmp::Ordering::Greater => PrefixCmp::Above,
        std::cmp::Ordering::Equal if target.len() < prefix.len() => PrefixCmp::Below,
        std::cmp::Ordering::Equal => PrefixCmp::Within(&target[prefix.len()..]),
    }
}

/// Longitud del prefijo común de TODAS las claves. Como las celdas van ordenadas,
/// el LCP de la primera y la última es el LCP de todas.
fn keys_lcp(cells: &[LeafCell]) -> usize {
    let first = &cells[0].key;
    let last = &cells[cells.len() - 1].key;
    let n = first.len().min(last.len());
    let mut i = 0;
    while i < n && first[i] == last[i] {
        i += 1;
    }
    i
}

/// Tamaño de una celda de hoja guardando solo el sufijo `key[plen..]`.
fn leaf_suffix_cell_size(c: &LeafCell, plen: usize) -> usize {
    let slen = c.key.len() - plen;
    let payload = match &c.payload {
        Payload::Inline(v) => varint_len(v.len() as u64) + v.len(),
        Payload::Overflow { total_len, .. } => varint_len(*total_len) + 8,
    };
    1 + varint_len(slen as u64) + slen + payload
}

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

// --- array de punteros a celda (v3): búsqueda binaria in-page ---
//
// Las celdas se almacenan en orden de clave desde la cabecera (como antes), y un
// **array de punteros** ordenado por clave crece desde el FINAL del nodo hacia el
// contenido: `ptr[i]` (el offset de la celda i) ocupa los 2 bytes en
// `body.len() - 2*(i+1)`. Así `inner_child`/`leaf_seek` hacen búsqueda binaria
// O(log celdas) en vez de escaneo lineal, y anexar la clave máxima sigue siendo
// O(1) (celda al final del contenido + puntero en el borde del array, ambos al
// hueco central). Coste: 2 bytes por celda.

fn cell_ptr(body: &[u8], i: usize) -> usize {
    let p = body.len() - 2 * (i + 1);
    u16::from_le_bytes([body[p], body[p + 1]]) as usize
}

/// Escribe `ptr[i] = off` en el array del final del nodo.
fn set_cell_ptr(body: &mut [u8], i: usize, off: usize) {
    let p = body.len() - 2 * (i + 1);
    body[p..p + 2].copy_from_slice(&(off as u16).to_le_bytes());
}

/// Clave de una celda interna en `off`: `(clave, pos tras la clave)` (apunta al
/// `child` u64).
fn inner_key_at(page: u64, body: &[u8], off: usize) -> Result<(&[u8], usize)> {
    let mut pos = off;
    let klen = get_varint(page, body, &mut pos)? as usize;
    let key = get_slice(page, body, &mut pos, klen)?;
    Ok((key, pos))
}

/// Flags + clave de una celda de hoja en `off`: `(flags, clave, pos tras la
/// clave)` (apunta al payload).
fn leaf_key_at(page: u64, body: &[u8], off: usize) -> Result<(u8, &[u8], usize)> {
    let mut pos = off;
    let flags = *body.get(pos).ok_or(corrupt(page, "celda truncada"))?;
    pos += 1;
    let klen = get_varint(page, body, &mut pos)? as usize;
    let key = get_slice(page, body, &mut pos, klen)?;
    Ok((flags, key, pos))
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
    // Hojas comprimidas (v3+): `[plen varint][prefijo]` tras la cabecera; cada celda
    // guarda el sufijo y la clave completa = prefijo ++ sufijo.
    let (prefix, mut pos) = leaf_prefix_and_start(page, body)?;
    let prefix = prefix.to_vec();
    let mut cells: Vec<LeafCell> = Vec::with_capacity(ncells);
    for _ in 0..ncells {
        let flags = *body.get(pos).ok_or(corrupt(page, "celda truncada"))?;
        pos += 1;
        let slen = get_varint(page, body, &mut pos)? as usize;
        let suffix = get_slice(page, body, &mut pos, slen)?;
        let mut key = Vec::with_capacity(prefix.len() + slen);
        key.extend_from_slice(&prefix);
        key.extend_from_slice(suffix);
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

/// `false` si las celdas (contenido + array de punteros) no caben (el llamador
/// hace split). Si caben, escribe el nodo completo: cabecera, contenido en orden
/// de clave, hueco central a ceros y el array de punteros al final (v3). Contenido
/// determinista (mismo input ⇒ mismos bytes).
pub fn encode_leaf(cells: &[LeafCell], body: &mut [u8]) -> bool {
    let n = cells.len();
    // Comprime si hay un prefijo común que merezca la pena (≥2 B y ≥2 celdas): el
    // prefijo se guarda UNA vez tras la cabecera y las celdas solo el sufijo.
    let plen = if n >= 2 { keys_lcp(cells) } else { 0 };
    if plen >= 2 {
        let pblock = varint_len(plen as u64) + plen;
        let content_len: usize =
            pblock + cells.iter().map(|c| leaf_suffix_cell_size(c, plen)).sum::<usize>();
        let content_end = LEAF_HDR + content_len;
        if content_end + 2 * n > body.len() {
            return false;
        }
        body[0] = TYPE_LEAF;
        body[1] = LEAF_PREFIX_FLAG;
        body[2..4].copy_from_slice(&(n as u16).to_le_bytes());
        let mut off = put_varint_at(body, LEAF_HDR, plen as u64);
        body[off..off + plen].copy_from_slice(&cells[0].key[..plen]);
        off += plen;
        for (i, c) in cells.iter().enumerate() {
            set_cell_ptr(body, i, off);
            off = write_leaf_cell_at(body, off, &c.key[plen..], &c.payload);
        }
        debug_assert_eq!(off, content_end);
        let ptr_start = body.len() - 2 * n;
        body[content_end..ptr_start].fill(0);
        return true;
    }
    let content_len: usize = cells.iter().map(leaf_cell_size).sum();
    let content_end = LEAF_HDR + content_len;
    if content_end + 2 * n > body.len() {
        return false; // no caben contenido + punteros ⇒ split
    }
    body[0] = TYPE_LEAF;
    body[1] = 0;
    body[2..4].copy_from_slice(&(n as u16).to_le_bytes());
    let mut off = LEAF_HDR;
    for (i, c) in cells.iter().enumerate() {
        set_cell_ptr(body, i, off);
        off = write_leaf_cell_at(body, off, &c.key, &c.payload);
    }
    debug_assert_eq!(off, content_end);
    let ptr_start = body.len() - 2 * n;
    body[content_end..ptr_start].fill(0);
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
    let n = cells.len();
    let content_len: usize = cells.iter().map(inner_cell_size).sum();
    let content_end = INNER_HDR + content_len;
    if content_end + 2 * n > body.len() {
        return false;
    }
    body[0] = TYPE_INNER;
    body[1] = 0;
    body[2..4].copy_from_slice(&(n as u16).to_le_bytes());
    body[4..12].copy_from_slice(&rightmost.0.to_le_bytes());
    let mut off = INNER_HDR;
    for (i, c) in cells.iter().enumerate() {
        set_cell_ptr(body, i, off);
        off = write_inner_cell_at(body, off, &c.key, c.child);
    }
    debug_assert_eq!(off, content_end);
    let ptr_start = body.len() - 2 * n;
    body[content_end..ptr_start].fill(0);
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
    Ok(inner_child_indexed(page, body, key)?.0)
}

/// Como [`inner_child`] pero devuelve también el **índice** del hijo elegido
/// (`= partition_point`, en `0..=ncells`; `ncells` ⇒ rightmost). El índice sirve
/// para retomar por el hermano siguiente en un recorrido (`for_each_prefix`).
pub fn inner_child_indexed(page: u64, body: &[u8], key: &[u8]) -> Result<(PageId, usize)> {
    if body[0] != TYPE_INNER {
        return Err(corrupt(page, "se esperaba un nodo interno"));
    }
    let ncells = u16::from_le_bytes([body[2], body[3]]) as usize;
    // partition_point por búsqueda binaria: primer separador con clave > `key`
    // (cota superior exclusiva). Su hijo cubre `key`; si ninguno, va al rightmost.
    let mut lo = 0;
    let mut hi = ncells;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let (ckey, _) = inner_key_at(page, body, cell_ptr(body, mid))?;
        if ckey > key {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    Ok((inner_child_value_at(page, body, lo)?, lo))
}

/// Número de separadores de un nodo interno.
pub fn inner_ncells(body: &[u8]) -> usize {
    u16::from_le_bytes([body[2], body[3]]) as usize
}

/// Hijo en el índice `idx` (`cells[idx].child`, o rightmost si `idx == ncells`).
pub fn inner_child_value_at(page: u64, body: &[u8], idx: usize) -> Result<PageId> {
    let ncells = u16::from_le_bytes([body[2], body[3]]) as usize;
    if idx < ncells {
        let (_, mut pos) = inner_key_at(page, body, cell_ptr(body, idx))?;
        Ok(PageId(get_u64(page, body, &mut pos)?))
    } else {
        Ok(inner_rightmost(body))
    }
}

/// Llama a `f` con la clave de cada celda de la hoja cuya clave empieza por
/// `prefix`, en orden y **sin asignar** una celda por entrada (binary search a la
/// primera, luego recorrido in-page). Devuelve `true` si la última celda de la
/// hoja casó el prefijo (el bloque puede continuar en la hoja hermana siguiente),
/// `false` si terminó dentro de esta hoja.
pub fn leaf_for_each_prefix(
    page: u64,
    body: &[u8],
    prefix: &[u8],
    f: &mut impl FnMut(&[u8]) -> Result<()>,
) -> Result<bool> {
    if body[0] != TYPE_LEAF {
        return Err(corrupt(page, "se esperaba una hoja"));
    }
    let ncells = u16::from_le_bytes([body[2], body[3]]) as usize;
    let (lprefix, _) = leaf_prefix_and_start(page, body)?;
    let mut keybuf: Vec<u8> = Vec::new();
    // Emite la clave COMPLETA (`lprefix ++ sufijo`) a `f`; zero-copy si no comprimida.
    macro_rules! emit {
        ($suffix:expr) => {{
            if lprefix.is_empty() {
                f($suffix)?;
            } else {
                keybuf.clear();
                keybuf.extend_from_slice(lprefix);
                keybuf.extend_from_slice($suffix);
                f(&keybuf)?;
            }
        }};
    }
    // El parámetro `prefix` es el prefijo BUSCADO; `lprefix` es el común de la hoja.
    let srest: &[u8] = if prefix.len() <= lprefix.len() {
        // buscado ⊆ común: casan TODAS (si el común empieza por el buscado) o NINGUNA.
        if lprefix.starts_with(prefix) {
            for i in 0..ncells {
                let (_f, suffix, _) = leaf_key_at(page, body, cell_ptr(body, i))?;
                emit!(suffix);
            }
            return Ok(true);
        }
        // Ninguna casa. Si el buscado va DESPUÉS de las claves de la hoja (`>`), el
        // bloque empieza en la hermana siguiente ⇒ `true` (sigue); si va antes, `false`.
        return Ok(prefix > lprefix);
    } else {
        if !prefix.starts_with(lprefix) {
            // Ninguna casa (toda clave empieza por lprefix ≠ inicio del buscado); el
            // bloque puede estar en la hermana si el buscado va después.
            return Ok(prefix > lprefix);
        }
        &prefix[lprefix.len()..]
    };
    // lower_bound: primer sufijo >= srest.
    let mut lo = 0;
    let mut hi = ncells;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let (_f, suffix, _) = leaf_key_at(page, body, cell_ptr(body, mid))?;
        if suffix < srest {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let mut i = lo;
    while i < ncells {
        let (_f, suffix, _) = leaf_key_at(page, body, cell_ptr(body, i))?;
        if !suffix.starts_with(srest) {
            return Ok(false); // pasamos el bloque del prefijo dentro de esta hoja
        }
        emit!(suffix);
        i += 1;
    }
    Ok(true) // toda la cola de la hoja casó: puede seguir en la hermana
}

/// Número de celdas de una hoja (los 2 bytes de la cabecera).
pub fn leaf_ncells(body: &[u8]) -> usize {
    u16::from_le_bytes([body[2], body[3]]) as usize
}

/// Índice de la primera celda con clave `>= key` (lower_bound, búsqueda binaria).
/// Lo usa el cursor de scan para arrancar en la posición de `scan_from`.
pub fn leaf_lower_bound(page: u64, body: &[u8], key: &[u8]) -> Result<usize> {
    let ncells = leaf_ncells(body);
    let (prefix, _) = leaf_prefix_and_start(page, body)?;
    let krest = match cmp_prefix(prefix, key) {
        PrefixCmp::Below => return Ok(0),       // `key` < todas ⇒ arranca en 0
        PrefixCmp::Above => return Ok(ncells),  // `key` > todas ⇒ tras la última
        PrefixCmp::Within(r) => r,
    };
    let mut lo = 0;
    let mut hi = ncells;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let (_flags, suffix, _) = leaf_key_at(page, body, cell_ptr(body, mid))?;
        if suffix < krest {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(lo)
}

/// Clave (propia) + payload de la celda `i` de una hoja, leídos **in-page** por el
/// array de punteros. El payload `Inline` trae su valor (una copia); `Overflow`,
/// la referencia a resolver. Para el cursor de scan en streaming: una celda a la
/// vez, sin materializar toda la hoja en un `Vec<LeafCell>`.
pub fn leaf_cell_at(page: u64, body: &[u8], i: usize) -> Result<(Vec<u8>, Payload)> {
    let off = cell_ptr(body, i);
    let (flags, suffix, payload_pos) = leaf_key_at(page, body, off)?;
    let (prefix, _) = leaf_prefix_and_start(page, body)?;
    let mut key = Vec::with_capacity(prefix.len() + suffix.len());
    key.extend_from_slice(prefix); // vacío si no comprimida ⇒ key == sufijo (zero-extra)
    key.extend_from_slice(suffix);
    let mut pos = payload_pos;
    let payload = read_leaf_payload(page, body, &mut pos, flags)?;
    Ok((key, payload))
}

/// Como [`leaf_cell_view`] pero reconstruye la clave COMPLETA (`prefijo ++ sufijo`)
/// en `key_buf` (reutilizable): la vía del cursor de scan para hojas **comprimidas**,
/// donde la clave no es un slice contiguo de la página. El valor sí llega prestado.
pub fn leaf_cell_view_into<'a>(
    page: u64,
    body: &'a [u8],
    i: usize,
    key_buf: &mut Vec<u8>,
) -> Result<PayloadView<'a>> {
    let off = cell_ptr(body, i);
    let (flags, suffix, payload_pos) = leaf_key_at(page, body, off)?;
    let (prefix, _) = leaf_prefix_and_start(page, body)?;
    key_buf.clear();
    key_buf.extend_from_slice(prefix);
    key_buf.extend_from_slice(suffix);
    let mut pos = payload_pos;
    if flags & CELL_OVERFLOW_FLAG != 0 {
        let total_len = get_varint(page, body, &mut pos)?;
        let first = PageId(get_u64(page, body, &mut pos)?);
        Ok(PayloadView::Overflow { total_len, first })
    } else {
        let vlen = get_varint(page, body, &mut pos)? as usize;
        Ok(PayloadView::Inline(get_slice(page, body, &mut pos, vlen)?))
    }
}

/// Payload **prestado** del body (cero copias): la vista del scan en streaming
/// hacia la API, que decodifica las columnas pedidas directo de la página.
#[derive(Debug)]
pub enum PayloadView<'a> {
    Inline(&'a [u8]),
    Overflow { total_len: u64, first: PageId },
}

/// Como [`leaf_cell_at`] pero sin copiar nada: clave y valor inline llegan
/// prestados del body (válidos mientras viva el préstamo de la página).
pub fn leaf_cell_view(page: u64, body: &[u8], i: usize) -> Result<(&[u8], PayloadView<'_>)> {
    let off = cell_ptr(body, i);
    let (flags, key, payload_pos) = leaf_key_at(page, body, off)?;
    let mut pos = payload_pos;
    if flags & CELL_OVERFLOW_FLAG != 0 {
        let total_len = get_varint(page, body, &mut pos)?;
        let first = PageId(get_u64(page, body, &mut pos)?);
        Ok((key, PayloadView::Overflow { total_len, first }))
    } else {
        let vlen = get_varint(page, body, &mut pos)? as usize;
        Ok((
            key,
            PayloadView::Inline(get_slice(page, body, &mut pos, vlen)?),
        ))
    }
}

/// `Some((pos_payload, flags))` con `pos` al inicio del payload de la celda de
/// `key`, o `None` si no está. Escanea sin asignar.
fn leaf_seek(page: u64, body: &[u8], key: &[u8]) -> Result<Option<(usize, u8)>> {
    if body[0] != TYPE_LEAF {
        return Err(corrupt(page, "se esperaba una hoja"));
    }
    let ncells = u16::from_le_bytes([body[2], body[3]]) as usize;
    // Si la hoja está comprimida, `key` se compara contra `prefijo ++ sufijo`: pela
    // el prefijo una vez (si `key` no empieza por él, no está) y compara solo sufijos.
    let (prefix, _) = leaf_prefix_and_start(page, body)?;
    let krest = match cmp_prefix(prefix, key) {
        PrefixCmp::Below | PrefixCmp::Above => return Ok(None),
        PrefixCmp::Within(r) => r,
    };
    // Búsqueda binaria sobre el array de punteros (sufijos en orden).
    let mut lo = 0;
    let mut hi = ncells;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let (flags, suffix, payload_pos) = leaf_key_at(page, body, cell_ptr(body, mid))?;
        match suffix.cmp(krest) {
            std::cmp::Ordering::Equal => return Ok(Some((payload_pos, flags))),
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
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
    // Anexa el SUFIJO (`key` sin el prefijo común; prefijo vacío si la hoja no está
    // comprimida ⇒ sufijo = clave completa). Solo si `key` comparte ese prefijo (mismo
    // bloque); si no, `None` ⇒ camino general, que re-encoda con un prefijo más corto.
    let (prefix, start) = leaf_prefix_and_start(page, body)?;
    let prefix_len = prefix.len();
    if !key.starts_with(prefix) {
        return Ok(None);
    }
    let stored = &key[prefix_len..];

    // Recorre hasta el final, recordando el rango del último sufijo almacenado.
    let mut pos = start;
    let mut last_suffix: Option<(usize, usize)> = None;
    for _ in 0..ncells {
        let flags = *body.get(pos).ok_or(corrupt(page, "celda truncada"))?;
        pos += 1;
        let slen = get_varint(page, body, &mut pos)? as usize;
        let sstart = pos;
        let _ = get_slice(page, body, &mut pos, slen)?;
        last_suffix = Some((sstart, sstart + slen));
        skip_leaf_payload(page, body, &mut pos, flags)?;
    }
    let end = pos;

    // Solo es append si el sufijo nuevo es estrictamente mayor que el último (= clave
    // nueva > última, ya que comparten el prefijo).
    if let Some((ss, se)) = last_suffix
        && stored <= &body[ss..se]
    {
        return Ok(None);
    }

    let cell = encode_leaf_cell(stored, payload);
    // La celda nueva y un puntero más deben caber sobre el array del final.
    if end + cell.len() > body.len() - 2 * (ncells + 1) {
        return Ok(None); // no caben ⇒ camino general (hará split)
    }
    body[end..end + cell.len()].copy_from_slice(&cell);
    set_cell_ptr(body, ncells, end); // puntero de la celda nueva (clave máxima)
    body[2..4].copy_from_slice(&((ncells as u16) + 1).to_le_bytes());
    Ok(Some(end + cell.len()))
}

/// Anexa una celda **inline** al final de la hoja en `end` (offset libre ya
/// conocido) sin escanear las celdas existentes: el camino O(1) del **cursor de
/// append** (M10-perf). El llamador garantiza que `key` va después de la última
/// celda y que la hoja es la rightmost (el cursor lo asegura). Devuelve el nuevo
/// offset libre, o `None` si no cabe (⇒ split por el camino general). No asigna.
pub fn append_inline_at(body: &mut [u8], end: usize, key: &[u8], value: &[u8]) -> Option<usize> {
    let ncells = u16::from_le_bytes([body[2], body[3]]) as usize;
    // Si la hoja está comprimida, se anexa el SUFIJO (`key` sin el prefijo común), y
    // solo si `key` comparte ese prefijo (mismo bloque); si no, `None` ⇒ camino
    // general, que re-encodará con un prefijo más corto. Así la rightmost activa puede
    // estar comprimida (densa) y el append sigue siendo O(1).
    let stored: &[u8] = if leaf_is_compressed(body) {
        let (prefix, _) = leaf_prefix_and_start(0, body).ok()?;
        if !key.starts_with(prefix) {
            return None;
        }
        &key[prefix.len()..]
    } else {
        key
    };
    let cell_len = 1
        + varint_len(stored.len() as u64)
        + stored.len()
        + varint_len(value.len() as u64)
        + value.len();
    // La celda nueva y un puntero más deben caber sobre el array del final.
    if end + cell_len > body.len() - 2 * (ncells + 1) {
        return None;
    }
    let mut p = end;
    body[p] = 0; // flag inline
    p += 1;
    p = put_varint_at(body, p, stored.len() as u64);
    body[p..p + stored.len()].copy_from_slice(stored);
    p += stored.len();
    p = put_varint_at(body, p, value.len() as u64);
    body[p..p + value.len()].copy_from_slice(value);
    p += value.len();
    set_cell_ptr(body, ncells, end); // puntero de la celda nueva (clave máxima)
    body[2..4].copy_from_slice(&((ncells + 1) as u16).to_le_bytes());
    Some(p)
}

/// Escribe una celda de hoja en `body[off..]` **sin asignar** y devuelve el
/// offset siguiente. Mismo formato que `encode_leaf_cell`.
fn write_leaf_cell_at(body: &mut [u8], off: usize, key: &[u8], payload: &Payload) -> usize {
    let mut p = off;
    match payload {
        Payload::Inline(v) => {
            body[p] = 0;
            p += 1;
            p = put_varint_at(body, p, key.len() as u64);
            body[p..p + key.len()].copy_from_slice(key);
            p += key.len();
            p = put_varint_at(body, p, v.len() as u64);
            body[p..p + v.len()].copy_from_slice(v);
            p += v.len();
        }
        Payload::Overflow { total_len, first } => {
            body[p] = CELL_OVERFLOW_FLAG;
            p += 1;
            p = put_varint_at(body, p, key.len() as u64);
            body[p..p + key.len()].copy_from_slice(key);
            p += key.len();
            p = put_varint_at(body, p, *total_len);
            body[p..p + 8].copy_from_slice(&first.0.to_le_bytes());
            p += 8;
        }
    }
    p
}

/// Escribe una celda interna (`[varint klen][key][child u64]`) en `body[off..]`
/// sin asignar y devuelve el offset siguiente.
fn write_inner_cell_at(body: &mut [u8], off: usize, key: &[u8], child: PageId) -> usize {
    let mut p = off;
    p = put_varint_at(body, p, key.len() as u64);
    body[p..p + key.len()].copy_from_slice(key);
    p += key.len();
    body[p..p + 8].copy_from_slice(&child.0.to_le_bytes());
    p + 8
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
    fn compressed_leaf_roundtrip() {
        // Prefijo común largo ⇒ encode_leaf comprime; parse_leaf reconstruye igual.
        let cells = vec![
            cell(b"posting:term42:doc0001", b""),
            cell(b"posting:term42:doc0050", b"x"),
            LeafCell {
                key: b"posting:term42:doc9999".to_vec(),
                payload: Payload::Overflow {
                    total_len: 40_000,
                    first: PageId(13),
                },
            },
        ];
        let mut body = vec![0u8; BODY_SIZE];
        assert!(encode_leaf(&cells, &mut body));
        assert!(
            leaf_is_compressed(&body),
            "un prefijo común largo debe comprimir"
        );
        assert_eq!(parse_leaf(0, &body).unwrap(), cells);
        // Sin prefijo común ⇒ no comprime (formato v1 intacto).
        let plain = vec![cell(b"a", b"1"), cell(b"zzz", b"2")];
        let mut body2 = vec![0u8; BODY_SIZE];
        assert!(encode_leaf(&plain, &mut body2));
        assert!(!leaf_is_compressed(&body2));
        assert_eq!(parse_leaf(0, &body2).unwrap(), plain);
    }

    #[test]
    fn compressed_leaf_find_and_for_each() {
        // Claves con prefijo común ⇒ hoja comprimida; `leaf_find` y
        // `leaf_for_each_prefix` deben coincidir con el oráculo (`parse_leaf`) por el
        // camino comprimido (pelar prefijo + búsqueda binaria de sufijos).
        let keys: Vec<Vec<u8>> = (0..20u32)
            .map(|i| format!("idx:42:{i:08}").into_bytes())
            .collect();
        let cells: Vec<LeafCell> = keys.iter().map(|k| cell(k, b"v")).collect();
        let mut body = vec![0u8; BODY_SIZE];
        assert!(encode_leaf(&cells, &mut body));
        assert!(leaf_is_compressed(&body));
        for probe in [
            &b"idx:42:00000000"[..],
            b"idx:42:00000010",
            b"idx:42:00000019",
            b"idx:42:00000020", // ausente, > todas en el bloque
            b"idx:42:99999999",
            b"idx:41:00000000", // < todas (prefijo distinto)
            b"zzz",
        ] {
            let parsed = parse_leaf(0, &body).unwrap();
            let expected = parsed
                .binary_search_by(|c| c.key.as_slice().cmp(probe))
                .ok()
                .map(|i| parsed[i].payload.clone());
            assert_eq!(leaf_find(0, &body, probe).unwrap(), expected, "find {probe:?}");
        }
        let mut got = Vec::new();
        leaf_for_each_prefix(0, &body, b"idx:42:", &mut |k| {
            got.push(k.to_vec());
            Ok(())
        })
        .unwrap();
        assert_eq!(got, keys, "for_each_prefix sobre el prefijo común");
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
