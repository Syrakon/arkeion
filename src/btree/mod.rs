//! B-tree copy-on-write direccionado por `PageId`.
//!
//! Los nodos nunca se enlazan por referencia: leer un nodo = pedir su body a
//! un `NodeSource`; «mutar» = copiarlo a una página sucia de la transacción
//! (`NodeStore::make_dirty`) y reescribir el camino hoja→raíz. No hay grafo
//! de referencias mutables que pelee con el borrow checker (R1).
//!
//! Simplificación deliberada de v1: `delete` no rebalancea nodos infrallenos,
//! solo elimina nodos vacíos (los reequilibra `vacuum` al compactar, M9).

pub mod node;

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::format::{PageBuf, PageId};
use node::{InnerCell, LeafCell, Payload};

/// Sentinela de árbol vacío (la página 0 es la cabecera del archivo: inequívoco).
pub const NO_ROOT: PageId = PageId(0);

/// Body de una página, venga de la caché compartida o del estado de una tx.
pub enum Body<'a> {
    Shared(Arc<PageBuf>),
    Local(&'a [u8]),
}

impl Body<'_> {
    pub fn bytes(&self) -> &[u8] {
        match self {
            Body::Shared(p) => p.body(),
            Body::Local(b) => b,
        }
    }
}

/// Lectura de páginas (snapshot o transacción de escritura).
pub trait NodeSource {
    fn body(&self, id: PageId) -> Result<Body<'_>>;
}

/// Escritura CoW de páginas (solo la transacción de escritura).
pub trait NodeStore: NodeSource {
    /// Página sucia nueva, zeroed.
    fn alloc(&mut self) -> Result<PageId>;
    /// CoW: si `id` ya es sucia la devuelve; si es durable, copia su contenido
    /// a una página sucia nueva y devuelve el nuevo id.
    fn make_dirty(&mut self, id: PageId) -> Result<PageId>;
    /// Body mutable de una página sucia.
    fn body_mut(&mut self, id: PageId) -> &mut [u8];
    /// Libera una página sucia (reutilizable en esta tx). No-op si es durable:
    /// las páginas durables son historia, no se liberan (D1).
    fn free(&mut self, id: PageId);
    fn is_dirty(&self, id: PageId) -> bool;

    /// Cursor de append cacheado (M10-perf): saca el cursor de la hoja rightmost
    /// para el camino rápido de inserts secuenciales. Por defecto `None` (sin
    /// caché): el store hace siempre el insert completo, correcto y sin coste.
    fn take_append_cursor(&mut self) -> Option<AppendCursor> {
        None
    }
    /// Guarda (o limpia) el cursor de append. Por defecto no-op.
    fn set_append_cursor(&mut self, _cursor: Option<AppendCursor>) {}
}

/// Cursor de append a la hoja **rightmost** de un árbol (M10-perf). Cachea la
/// hoja, su offset libre y su última clave para que un insert secuencial
/// (rowid creciente, imports) anexe en **O(1)** sin re-descender ni re-escanear.
/// Solo es válido si `key > last_key` (clave nueva = máximo global, va a la
/// rightmost) y `root` sigue siendo la raíz vigente; cualquier otra cosa cae al
/// camino completo, que lo reestablece. Anexar a la rightmost no toca separadores
/// (el hijo rightmost no tiene cota superior), por eso es correcto.
#[derive(Clone, Debug)]
pub struct AppendCursor {
    /// Raíz del árbol para la que el cursor es válido.
    pub root: PageId,
    /// Hoja rightmost (sucia) donde se anexa.
    pub leaf: PageId,
    /// Offset libre en el body de la hoja (dónde va la próxima celda).
    pub end: usize,
    /// Última clave de la hoja (cota para aceptar el append: `key > last_key`).
    pub last_key: Vec<u8>,
}

// --- lectura ---

pub fn get<S: NodeSource>(src: &S, root: PageId, key: &[u8]) -> Result<Option<Vec<u8>>> {
    if root == NO_ROOT {
        return Ok(None);
    }
    let mut id = root;
    loop {
        let body = src.body(id)?;
        match node::node_type(body.bytes()) {
            node::TYPE_INNER => {
                let child = node::inner_child(id.0, body.bytes(), key)?;
                drop(body);
                id = child;
            }
            node::TYPE_LEAF => {
                let found = node::leaf_find(id.0, body.bytes(), key)?;
                drop(body);
                return match found {
                    // Inline ya copió el valor en `leaf_find`: no lo re-clones.
                    Some(Payload::Inline(v)) => Ok(Some(v)),
                    Some(payload) => Ok(Some(read_value(src, &payload)?)),
                    None => Ok(None),
                };
            }
            _ => {
                return Err(Error::Corrupt {
                    page: id.0,
                    reason: "tipo de nodo inesperado",
                });
            }
        }
    }
}

/// `true` si la clave existe, sin materializar su valor (dup-checks baratos).
pub fn contains<S: NodeSource>(src: &S, root: PageId, key: &[u8]) -> Result<bool> {
    if root == NO_ROOT {
        return Ok(false);
    }
    let mut id = root;
    loop {
        let body = src.body(id)?;
        match node::node_type(body.bytes()) {
            node::TYPE_INNER => {
                let child = node::inner_child(id.0, body.bytes(), key)?;
                drop(body);
                id = child;
            }
            node::TYPE_LEAF => {
                let found = node::leaf_contains(id.0, body.bytes(), key)?;
                drop(body);
                return Ok(found);
            }
            _ => {
                return Err(Error::Corrupt {
                    page: id.0,
                    reason: "tipo de nodo inesperado",
                });
            }
        }
    }
}

/// Como [`read_value`] pero **consume** el payload: un `Inline` se mueve sin
/// re-clonar (el camino caliente del scan en streaming ya tiene el valor leído
/// de la página una vez; clonarlo otra vez sería el doble-copia que evitamos).
fn read_value_owned<S: NodeSource>(src: &S, payload: Payload) -> Result<Vec<u8>> {
    match payload {
        Payload::Inline(v) => Ok(v),
        overflow => read_value(src, &overflow),
    }
}

fn read_value<S: NodeSource>(src: &S, payload: &Payload) -> Result<Vec<u8>> {
    match payload {
        Payload::Inline(v) => Ok(v.clone()),
        Payload::Overflow { total_len, first } => {
            let mut out = Vec::with_capacity(*total_len as usize);
            let mut id = *first;
            while id != NO_ROOT {
                let body = src.body(id)?;
                let (chunk, next) = node::parse_overflow(id.0, body.bytes())?;
                out.extend_from_slice(chunk);
                drop(body);
                id = next;
            }
            if out.len() as u64 != *total_len {
                return Err(Error::Corrupt {
                    page: first.0,
                    reason: "cadena overflow incompleta",
                });
            }
            Ok(out)
        }
    }
}

// --- escritura ---

struct InsertOutcome {
    /// Id del nodo tras la operación (puede haber cambiado por CoW).
    id: PageId,
    /// Split: (separador, nodo derecho). El separador es la cota inferior
    /// inclusiva del derecho.
    split: Option<(Vec<u8>, PageId)>,
    /// Si el insert anexó al final de la hoja rightmost **global** (todo el
    /// descenso fue al hijo rightmost), la info para (re)establecer el cursor de
    /// append. `None` en cualquier otro caso (overwrite, insert en medio, etc.).
    tail: Option<Tail>,
}

/// Datos para (re)establecer el [`AppendCursor`] desde un insert que anexó.
struct Tail {
    leaf: PageId,
    end: usize,
    last_key: Vec<u8>,
}

pub fn insert<S: NodeStore>(s: &mut S, root: PageId, key: &[u8], value: &[u8]) -> Result<PageId> {
    if key.is_empty() || key.len() > node::MAX_KEY_LEN {
        return Err(Error::InvalidInput("clave vacía o de más de 1024 bytes"));
    }
    // Camino rápido: cursor de append (inserts secuenciales: rowid creciente,
    // imports). Anexa O(1) a la hoja rightmost sin re-descender, sin parsear y
    // **sin asignar** (reusa el Vec de `last_key`). Solo para valores inline y
    // claves estrictamente mayores que la última (nuevo máximo global).
    // `take` siempre saca el cursor; si el camino rápido no aplica (otra raíz,
    // clave no-máxima, overflow o sin hueco), el camino completo lo reestablece
    // desde su `tail`, así que no se pierde.
    if let Some(mut cur) = s.take_append_cursor()
        && cur.root == root
        && key > cur.last_key.as_slice()
        && s.is_dirty(cur.leaf)
        && node::inline_cell_size(key, value.len()) <= node::MAX_INLINE_CELL
        && let Some(new_end) = node::append_inline_at(s.body_mut(cur.leaf), cur.end, key, value)
    {
        cur.end = new_end;
        cur.last_key.clear();
        cur.last_key.extend_from_slice(key);
        s.set_append_cursor(Some(cur));
        return Ok(root);
    }

    let payload = if node::inline_cell_size(key, value.len()) <= node::MAX_INLINE_CELL {
        Payload::Inline(value.to_vec())
    } else {
        Payload::Overflow {
            total_len: value.len() as u64,
            first: write_chain(s, value)?,
        }
    };

    if root == NO_ROOT {
        let id = s.alloc()?;
        let cells = [LeafCell {
            key: key.to_vec(),
            payload,
        }];
        let ok = node::encode_leaf(&cells, s.body_mut(id));
        debug_assert!(ok, "una celda siempre cabe en una hoja vacía");
        // El segundo insert (root ≠ NO_ROOT) ya establece el cursor por su tail.
        s.set_append_cursor(None);
        return Ok(id);
    }

    let out = insert_rec(s, root, key, payload)?;
    let new_root = match out.split {
        None => out.id,
        Some((sep, right)) => {
            let new_root = s.alloc()?;
            let cells = [InnerCell {
                key: sep,
                child: out.id,
            }];
            let ok = node::encode_inner(&cells, right, s.body_mut(new_root));
            debug_assert!(ok, "una raíz con una celda siempre cabe");
            new_root
        }
    };
    // (Re)establece el cursor: solo si el insert anexó a la hoja rightmost global.
    s.set_append_cursor(out.tail.map(|t| AppendCursor {
        root: new_root,
        leaf: t.leaf,
        end: t.end,
        last_key: t.last_key,
    }));
    Ok(new_root)
}

fn insert_rec<S: NodeStore>(
    s: &mut S,
    id: PageId,
    key: &[u8],
    payload: Payload,
) -> Result<InsertOutcome> {
    let body = s.body(id)?;
    match node::node_type(body.bytes()) {
        node::TYPE_LEAF => {
            drop(body);
            let id = s.make_dirty(id)?;
            // Fast-path: una clave que va al final de la hoja (inserts
            // secuenciales: rowid creciente, imports) se anexa in situ sin
            // parsear ni re-encodar las celdas existentes. Bytes idénticos al
            // re-encode. Devuelve el `tail` para (re)establecer el cursor.
            if let Some(end) = node::leaf_append(id.0, s.body_mut(id), key, &payload)? {
                return Ok(InsertOutcome {
                    id,
                    split: None,
                    tail: Some(Tail {
                        leaf: id,
                        end,
                        last_key: key.to_vec(),
                    }),
                });
            }

            // General: parsear (la copia ya sucia), insertar/sobrescribir, encodar.
            let body = s.body(id)?;
            let mut cells = node::parse_leaf(id.0, body.bytes())?;
            drop(body);
            let appended = match cells.binary_search_by(|c| c.key.as_slice().cmp(key)) {
                Ok(i) => {
                    free_payload(s, &cells[i].payload);
                    cells[i].payload = payload;
                    false
                }
                Err(i) => {
                    cells.insert(
                        i,
                        LeafCell {
                            key: key.to_vec(),
                            payload,
                        },
                    );
                    i + 1 == cells.len() // la celda nueva quedó la última
                }
            };
            if node::encode_leaf(&cells, s.body_mut(id)) {
                // No es un append a la rightmost (overwrite o insert en medio):
                // sin tail; el cursor se reestablecerá en el próximo append.
                return Ok(InsertOutcome {
                    id,
                    split: None,
                    tail: None,
                });
            }
            // Split de hoja: el separador es la primera clave de la derecha. En un
            // **append al final** (inserts secuenciales: rowid creciente, imports)
            // partimos 100/0 —la celda nueva sola a la derecha— para dejar la hoja
            // izquierda LLENA en vez de a la mitad. Casi duplica el llenado en
            // cargas append (como hace SQLite); en otro caso, balance por tamaño.
            let sp = if appended {
                cells.len() - 1
            } else {
                node::split_point(&cells, node::leaf_cell_size)
            };
            let right_cells = cells.split_off(sp);
            let sep = right_cells[0].key.clone();
            let right = s.alloc()?;
            let ok = node::encode_leaf(&right_cells, s.body_mut(right))
                && node::encode_leaf(&cells, s.body_mut(id));
            debug_assert!(ok, "cada mitad de un split cabe por construcción");
            // Split: el cursor se reestablecerá con el próximo append (que irá a
            // la mitad derecha, la nueva rightmost).
            Ok(InsertOutcome {
                id,
                split: Some((sep, right)),
                tail: None,
            })
        }
        node::TYPE_INNER => {
            // Descenso sin materializar el nodo (camino caliente).
            let child = node::inner_child(id.0, body.bytes(), key)?;
            // ¿Bajamos al hijo rightmost? Solo entonces la hoja del tail es la
            // rightmost **global** y el cursor puede apuntarla.
            let is_rightmost = child == node::inner_rightmost(body.bytes());
            drop(body);
            let InsertOutcome {
                id: cid,
                split: csplit,
                tail: ctail,
            } = insert_rec(s, child, key, payload)?;
            let tail = if is_rightmost { ctail } else { None };

            // Atajo CoW: si el hijo no cambió de id (ya estaba sucio en esta tx)
            // y no hubo split, este nodo interno —forzosamente ya sucio, pues un
            // padre limpio solo apunta a hijos limpios— sigue siendo válido tal
            // cual. Evita re-parsear y re-encodar todo el camino en cada insert.
            if csplit.is_none() && cid == child {
                return Ok(InsertOutcome {
                    id,
                    split: None,
                    tail,
                });
            }

            // Algo cambió: ahora sí materializar para mutar y re-encodar.
            let body = s.body(id)?;
            let (mut cells, mut rightmost) = node::parse_inner(id.0, body.bytes())?;
            drop(body);
            let idx = cells.partition_point(|c| c.key.as_slice() <= key);
            if idx < cells.len() {
                cells[idx].child = cid;
            } else {
                rightmost = cid;
            }
            if let Some((sep, right)) = csplit {
                // cid cubre < sep; right cubre [sep, cota original del slot).
                cells.insert(
                    idx,
                    InnerCell {
                        key: sep,
                        child: cid,
                    },
                );
                if idx + 1 < cells.len() {
                    cells[idx + 1].child = right;
                } else {
                    rightmost = right;
                }
            }

            let id = s.make_dirty(id)?;
            if node::encode_inner(&cells, rightmost, s.body_mut(id)) {
                return Ok(InsertOutcome {
                    id,
                    split: None,
                    tail,
                });
            }
            // Split interno: la clave de cells[sp] SUBE; su hijo pasa a ser el
            // rightmost de la mitad izquierda.
            let sp = node::split_point(&cells, node::inner_cell_size);
            let sep_up = cells[sp].key.clone();
            let left_rightmost = cells[sp].child;
            let right_cells: Vec<InnerCell> = cells.drain(sp + 1..).collect();
            cells.pop(); // la celda sp sube
            let right = s.alloc()?;
            let ok = node::encode_inner(&right_cells, rightmost, s.body_mut(right))
                && node::encode_inner(&cells, left_rightmost, s.body_mut(id));
            debug_assert!(ok, "cada mitad de un split cabe por construcción");
            // La mitad derecha hereda el rightmost original ⇒ la hoja del `tail`
            // sigue siendo la rightmost global; el cursor la apuntará tras el
            // nuevo nodo raíz.
            Ok(InsertOutcome {
                id,
                split: Some((sep_up, right)),
                tail,
            })
        }
        _ => Err(Error::Corrupt {
            page: id.0,
            reason: "tipo de nodo inesperado",
        }),
    }
}

/// (nueva raíz, existía la clave).
pub fn delete<S: NodeStore>(s: &mut S, root: PageId, key: &[u8]) -> Result<(PageId, bool)> {
    if root == NO_ROOT {
        return Ok((NO_ROOT, false));
    }
    let (new_root, existed) = delete_rec(s, root, key)?;
    Ok((new_root.unwrap_or(NO_ROOT), existed))
}

fn delete_rec<S: NodeStore>(s: &mut S, id: PageId, key: &[u8]) -> Result<(Option<PageId>, bool)> {
    let body = s.body(id)?;
    match node::node_type(body.bytes()) {
        node::TYPE_LEAF => {
            let mut cells = node::parse_leaf(id.0, body.bytes())?;
            drop(body);
            let Ok(i) = cells.binary_search_by(|c| c.key.as_slice().cmp(key)) else {
                return Ok((Some(id), false));
            };
            free_payload(s, &cells[i].payload);
            cells.remove(i);
            if cells.is_empty() {
                s.free(id);
                return Ok((None, true));
            }
            let id = s.make_dirty(id)?;
            let ok = node::encode_leaf(&cells, s.body_mut(id));
            debug_assert!(ok, "quitar una celda nunca crece");
            Ok((Some(id), true))
        }
        node::TYPE_INNER => {
            let child = node::inner_child(id.0, body.bytes(), key)?;
            drop(body);
            let (new_child, existed) = delete_rec(s, child, key)?;

            // Atajo CoW (igual que en insert): hijo intacto ⇒ nada que reescribir.
            if new_child == Some(child) {
                return Ok((Some(id), existed));
            }

            let body = s.body(id)?;
            let (mut cells, mut rightmost) = node::parse_inner(id.0, body.bytes())?;
            drop(body);
            let idx = cells.partition_point(|c| c.key.as_slice() <= key);
            match new_child {
                Some(nc) => {
                    if idx < cells.len() {
                        cells[idx].child = nc;
                    } else {
                        rightmost = nc;
                    }
                }
                None => {
                    // El hijo desapareció: su rango se fusiona con el vecino.
                    if idx < cells.len() {
                        cells.remove(idx);
                    } else if let Some(last) = cells.pop() {
                        rightmost = last.child;
                    } else {
                        // Nodo sin hijos: desaparece también.
                        s.free(id);
                        return Ok((None, existed));
                    }
                    if cells.is_empty() {
                        // Solo queda el rightmost: el nodo colapsa en su hijo.
                        s.free(id);
                        return Ok((Some(rightmost), existed));
                    }
                }
            }
            let id = s.make_dirty(id)?;
            let ok = node::encode_inner(&cells, rightmost, s.body_mut(id));
            debug_assert!(ok, "actualizar o quitar celdas nunca crece");
            Ok((Some(id), existed))
        }
        _ => Err(Error::Corrupt {
            page: id.0,
            reason: "tipo de nodo inesperado",
        }),
    }
}

/// Libera la cadena overflow de un valor sustituido o borrado, solo si fue
/// escrita por esta misma tx (una cadena es sucia o durable en bloque; las
/// durables son historia y no se tocan).
fn free_payload<S: NodeStore>(s: &mut S, payload: &Payload) {
    let Payload::Overflow { first, .. } = payload else {
        return;
    };
    if !s.is_dirty(*first) {
        return;
    }
    let mut id = *first;
    while id != NO_ROOT {
        let next = {
            let Ok(body) = s.body(id) else { return };
            match node::parse_overflow(id.0, body.bytes()) {
                Ok((_, next)) => next,
                Err(_) => return,
            }
        };
        s.free(id);
        id = next;
    }
}

fn write_chain<S: NodeStore>(s: &mut S, data: &[u8]) -> Result<PageId> {
    debug_assert!(!data.is_empty());
    let mut next = NO_ROOT;
    for chunk in data.chunks(node::OVERFLOW_DATA).rev() {
        let id = s.alloc()?;
        node::encode_overflow(chunk, next, s.body_mut(id));
        next = id;
    }
    Ok(next)
}

// --- cursor de rango ---

/// Iterador ascendente por clave. Resuelve overflow al producir cada par.
/// La hoja actual del cursor, sostenida **sin materializar** sus celdas: el `Arc`
/// de la página (pager: sin copia) o una copia de sus bytes (fuentes que prestan,
/// como `MemStore` de tests). Las celdas se leen in-page de una en una al avanzar,
/// evitando el `Vec<LeafCell>` con dos asignaciones por celda del enfoque viejo.
enum HeldLeaf {
    Shared(Arc<PageBuf>),
    Owned(Box<[u8]>),
}

impl HeldLeaf {
    fn bytes(&self) -> &[u8] {
        match self {
            HeldLeaf::Shared(p) => p.body(),
            HeldLeaf::Owned(b) => b,
        }
    }
}

/// Estado de la hoja en curso del cursor de scan en streaming.
struct LeafScan {
    page: u64,
    held: HeldLeaf,
    /// Próxima celda a emitir (en orden de clave, vía el array de punteros).
    next: usize,
    ncells: usize,
}

pub struct Cursor<'s, S: NodeSource> {
    src: &'s S,
    state: CursorState,
}

/// Estado del cursor de scan **sin el préstamo de la fuente**: lo posee quien
/// necesita un scan que viva por sí mismo (el `Rows` en streaming de la API,
/// que es dueño de su `Snapshot`) y pasa la fuente en cada paso. [`Cursor`] es
/// el envoltorio prestado clásico sobre este estado.
pub struct CursorState {
    /// Nodos internos pendientes: (id, índice del próximo hijo a visitar).
    stack: Vec<(PageId, usize)>,
    /// Hoja en curso (en streaming), o `None` si ninguna/agotada.
    leaf: Option<LeafScan>,
    /// Buffer reutilizable para reconstruir la clave completa (`prefijo común ++
    /// sufijo`) de hojas **comprimidas** en `advance_view`; sin uso en las hojas sin
    /// comprimir (clave contigua ⇒ zero-copy).
    key_buf: Vec<u8>,
}

pub fn scan<S: NodeSource>(src: &S, root: PageId) -> Result<Cursor<'_, S>> {
    Ok(Cursor {
        src,
        state: scan_state(src, root, None)?,
    })
}

/// Comienza en la primera clave ≥ `start`.
pub fn scan_from<'s, S: NodeSource>(
    src: &'s S,
    root: PageId,
    start: &[u8],
) -> Result<Cursor<'s, S>> {
    Ok(Cursor {
        src,
        state: scan_state(src, root, Some(start))?,
    })
}

/// Estado de scan posicionado en la primera clave ≥ `start` (o la primera del
/// árbol). El llamador conserva el estado y avanza con [`CursorState::advance`]
/// o [`CursorState::advance_view`], pasando la fuente en cada paso.
pub fn scan_state<S: NodeSource>(
    src: &S,
    root: PageId,
    start: Option<&[u8]>,
) -> Result<CursorState> {
    let mut state = CursorState {
        stack: Vec::new(),
        leaf: None,
        key_buf: Vec::new(),
    };
    if root != NO_ROOT {
        state.descend(src, root, start)?;
    }
    Ok(state)
}

impl CursorState {
    fn descend<S: NodeSource>(
        &mut self,
        src: &S,
        mut id: PageId,
        start: Option<&[u8]>,
    ) -> Result<()> {
        loop {
            let body = src.body(id)?;
            match node::node_type(body.bytes()) {
                node::TYPE_INNER => {
                    let (cells, rightmost) = node::parse_inner(id.0, body.bytes())?;
                    drop(body);
                    let idx = match start {
                        None => 0,
                        Some(k) => cells.partition_point(|c| c.key.as_slice() <= k),
                    };
                    let child = if idx < cells.len() {
                        cells[idx].child
                    } else {
                        rightmost
                    };
                    self.stack.push((id, idx + 1));
                    id = child;
                }
                node::TYPE_LEAF => {
                    let ncells = node::leaf_ncells(body.bytes());
                    // Posición de arranque: lower_bound de `start` (scan_from) o 0.
                    let next = match start {
                        None => 0,
                        Some(k) => node::leaf_lower_bound(id.0, body.bytes(), k)?,
                    };
                    // Sostiene la hoja sin copiar (Arc del pager) o copiándola una
                    // vez (fuentes que prestan); las celdas se leen al avanzar.
                    let held = match body {
                        Body::Shared(p) => HeldLeaf::Shared(p),
                        Body::Local(b) => HeldLeaf::Owned(b.into()),
                    };
                    self.leaf = Some(LeafScan {
                        page: id.0,
                        held,
                        next,
                        ncells,
                    });
                    return Ok(());
                }
                _ => {
                    return Err(Error::Corrupt {
                        page: id.0,
                        reason: "tipo de nodo inesperado",
                    });
                }
            }
        }
    }

    /// Avanza a la siguiente hoja vía la pila de padres. `false` = árbol
    /// agotado. Puede dejar `leaf` en `None` con pila pendiente (un padre
    /// agotado): el llamador reintenta en bucle, como hacía `advance`.
    fn next_leaf<S: NodeSource>(&mut self, src: &S) -> Result<bool> {
        self.leaf = None;
        let Some((id, idx)) = self.stack.last().copied() else {
            return Ok(false);
        };
        let body = src.body(id)?;
        let (cells, rightmost) = node::parse_inner(id.0, body.bytes())?;
        drop(body);
        if idx <= cells.len() {
            self.stack.last_mut().expect("recién consultado").1 += 1;
            let child = if idx < cells.len() {
                cells[idx].child
            } else {
                rightmost
            };
            self.descend(src, child, None)?;
        } else {
            self.stack.pop();
        }
        Ok(true)
    }

    /// Próxima celda con clave y valor **propios** (una copia de cada una).
    pub fn advance<S: NodeSource>(&mut self, src: &S) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        loop {
            // Emite la próxima celda de la hoja en curso, leída in-page (una copia
            // de clave + una de valor, sin materializar el resto de la hoja).
            if let Some(ls) = self.leaf.as_mut()
                && ls.next < ls.ncells
            {
                let i = ls.next;
                ls.next += 1;
                let (key, payload) = node::leaf_cell_at(ls.page, ls.held.bytes(), i)?;
                let value = read_value_owned(src, payload)?;
                return Ok(Some((key, value)));
            }
            if !self.next_leaf(src)? {
                return Ok(None);
            }
        }
    }

    /// Próxima celda **sin copiar**: `f` recibe clave y valor prestados de la
    /// página sostenida (válidos solo durante la llamada). Un valor overflow se
    /// materializa en un buffer temporal (raro en filas: solo celdas grandes).
    /// El camino caliente del full scan en streaming decodifica las columnas
    /// proyectadas directo de la página, sin `Vec` de clave/valor por fila.
    pub fn advance_view<S: NodeSource, T>(
        &mut self,
        src: &S,
        f: impl FnOnce(&[u8], &[u8]) -> Result<T>,
    ) -> Result<Option<T>> {
        loop {
            let ready = self.leaf.as_ref().is_some_and(|ls| ls.next < ls.ncells);
            if ready {
                let (i, compressed) = {
                    let ls = self.leaf.as_mut().expect("comprobado arriba");
                    let i = ls.next;
                    ls.next += 1;
                    (i, node::leaf_is_compressed(ls.held.bytes()))
                };
                if compressed {
                    // Clave reconstruida en `key_buf` (no contigua en la página); el
                    // valor inline sí llega prestado.
                    let view = {
                        let ls = self.leaf.as_ref().expect("comprobado arriba");
                        node::leaf_cell_view_into(ls.page, ls.held.bytes(), i, &mut self.key_buf)?
                    };
                    return match view {
                        node::PayloadView::Inline(value) => f(&self.key_buf, value).map(Some),
                        node::PayloadView::Overflow { total_len, first } => {
                            let owned = read_value(src, &Payload::Overflow { total_len, first })?;
                            f(&self.key_buf, &owned).map(Some)
                        }
                    };
                }
                let ls = self.leaf.as_ref().expect("comprobado arriba");
                let (key, view) = node::leaf_cell_view(ls.page, ls.held.bytes(), i)?;
                return match view {
                    node::PayloadView::Inline(value) => f(key, value).map(Some),
                    node::PayloadView::Overflow { total_len, first } => {
                        let owned = read_value(src, &Payload::Overflow { total_len, first })?;
                        f(key, &owned).map(Some)
                    }
                };
            }
            if !self.next_leaf(src)? {
                return Ok(None);
            }
        }
    }
}

impl<S: NodeSource> Iterator for Cursor<'_, S> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.state.advance(self.src).transpose()
    }
}

/// Llama a `f` con la clave de **cada** entrada cuya clave empieza por `prefix`,
/// en orden. A diferencia de [`scan_from`] + filtro, desciende **in-page**
/// (binary search, sin parsear nodos a `Vec`) y recorre las celdas que casan sin
/// materializar una celda por entrada ni copiar su payload. Es el camino caliente
/// del *lookup* por índice secundario (prefijo `[0x02, index_id, valor]`): muchas
/// entradas contiguas, casi siempre en una sola hoja. `f` recibe la clave prestada
/// (válida solo durante la llamada). Cruza a hojas hermanas vía la pila de padres
/// igual que [`Cursor`], pero sin su coste de materialización.
pub fn for_each_prefix<S: NodeSource>(
    src: &S,
    root: PageId,
    prefix: &[u8],
    mut f: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    if root == NO_ROOT {
        return Ok(());
    }
    let bad = |id: PageId| Error::Corrupt {
        page: id.0,
        reason: "tipo de nodo inesperado",
    };

    // Desciende a la primera hoja candidata, apilando (interno, próximo hijo).
    let mut stack: Vec<(PageId, usize)> = Vec::new();
    let mut id = root;
    loop {
        let body = src.body(id)?;
        match node::node_type(body.bytes()) {
            node::TYPE_INNER => {
                let (child, idx) = node::inner_child_indexed(id.0, body.bytes(), prefix)?;
                drop(body);
                stack.push((id, idx + 1));
                id = child;
            }
            node::TYPE_LEAF => {
                drop(body);
                break;
            }
            _ => return Err(bad(id)),
        }
    }

    // Recorre la hoja; mientras el bloque del prefijo llegue al final de la hoja,
    // sigue por la hoja hermana siguiente (descenso leftmost desde el padre).
    loop {
        let body = src.body(id)?;
        let extends = node::leaf_for_each_prefix(id.0, body.bytes(), prefix, &mut f)?;
        drop(body);
        if !extends {
            return Ok(());
        }
        // Busca la hoja hermana siguiente subiendo por la pila.
        let mut next = None;
        while let Some(&(iid, idx)) = stack.last() {
            let body = src.body(iid)?;
            let nc = node::inner_ncells(body.bytes());
            if idx <= nc {
                stack.last_mut().expect("recién consultado").1 = idx + 1;
                let child = node::inner_child_value_at(iid.0, body.bytes(), idx)?;
                drop(body);
                // Desciende al hijo más a la izquierda de ese subárbol.
                let mut cur = child;
                loop {
                    let b = src.body(cur)?;
                    match node::node_type(b.bytes()) {
                        node::TYPE_INNER => {
                            let first = node::inner_child_value_at(cur.0, b.bytes(), 0)?;
                            drop(b);
                            stack.push((cur, 1));
                            cur = first;
                        }
                        node::TYPE_LEAF => {
                            drop(b);
                            next = Some(cur);
                            break;
                        }
                        _ => return Err(bad(cur)),
                    }
                }
                break;
            }
            drop(body);
            stack.pop();
        }
        match next {
            Some(n) => id = n,
            None => return Ok(()),
        }
    }
}

// --- diff de dos árboles CoW (M8) ---

/// Cambio en una clave entre dos árboles (`from` → `to`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyChange {
    /// Clave nueva en `to`.
    Added(Vec<u8>),
    /// Clave que estaba en `from` y desapareció.
    Removed(Vec<u8>),
    /// Clave en ambos con valor distinto: `(valor_from, valor_to)`.
    Modified(Vec<u8>, Vec<u8>),
}

/// Una clave que difiere entre dos árboles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyDiff {
    pub key: Vec<u8>,
    pub change: KeyChange,
}

/// Diferencias clave a clave entre dos árboles `from` y `to`, en orden
/// ascendente. **Salta los subárboles con el mismo `PageId`** (inmutables por
/// CoW): el coste es O(cambios), no O(datos) — el corazón del diff de ramas
/// barato (M8).
pub fn diff<S: NodeSource>(src: &S, from: PageId, to: PageId) -> Result<Vec<KeyDiff>> {
    let mut out = Vec::new();
    diff_range(src, from, to, None, None, &mut out)?;
    Ok(out)
}

/// Diff acotado a `[lo, hi)` (extremos abiertos = `None`).
fn diff_range<S: NodeSource>(
    src: &S,
    from: PageId,
    to: PageId,
    lo: Option<&[u8]>,
    hi: Option<&[u8]>,
    out: &mut Vec<KeyDiff>,
) -> Result<()> {
    if from == to {
        return Ok(()); // subárbol idéntico: el atajo que da O(cambios)
    }
    if from == NO_ROOT {
        for (key, val) in collect_range(src, to, lo, hi)? {
            out.push(KeyDiff {
                key,
                change: KeyChange::Added(val),
            });
        }
        return Ok(());
    }
    if to == NO_ROOT {
        for (key, val) in collect_range(src, from, lo, hi)? {
            out.push(KeyDiff {
                key,
                change: KeyChange::Removed(val),
            });
        }
        return Ok(());
    }
    let from_inner = node::node_type(src.body(from)?.bytes()) == node::TYPE_INNER;
    let to_inner = node::node_type(src.body(to)?.bytes()) == node::TYPE_INNER;
    if from_inner && to_inner {
        diff_inner(src, from, to, lo, hi, out)
    } else {
        // Alturas distintas o ambas hojas: comparar las entradas del rango.
        diff_flat(src, from, to, lo, hi, out)
    }
}

/// Dos nodos internos: parte el espacio de claves por las cotas de ambos y
/// recurre en cada par de hijos que se solapan (saltando los compartidos).
fn diff_inner<S: NodeSource>(
    src: &S,
    from: PageId,
    to: PageId,
    lo: Option<&[u8]>,
    hi: Option<&[u8]>,
    out: &mut Vec<KeyDiff>,
) -> Result<()> {
    let a = child_intervals(src, from)?;
    let b = child_intervals(src, to)?;
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        let (alo, aup) = (a[i].0.as_deref(), a[i].1.as_deref());
        let (blo, bup) = (b[j].0.as_deref(), b[j].1.as_deref());
        let seg_lo = max_lower(max_lower(alo, blo), lo);
        let seg_hi = min_upper(min_upper(aup, bup), hi);
        if range_valid(seg_lo, seg_hi) {
            diff_range(src, a[i].2, b[j].2, seg_lo, seg_hi, out)?;
        }
        // Avanzar el hijo con cota superior menor (`None` = +inf).
        match (aup, bup) {
            (None, None) => {
                i += 1;
                j += 1;
            }
            (None, Some(_)) => j += 1,
            (Some(_), None) => i += 1,
            (Some(x), Some(y)) => match x.cmp(y) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    i += 1;
                    j += 1;
                }
            },
        }
    }
    Ok(())
}

/// Compara dos subárboles materializando sus entradas en `[lo, hi)`. Solo se
/// usa cuando las alturas difieren o se llega a hojas: los subárboles son
/// pequeños (la inmensa mayoría se saltó por `PageId` antes de llegar aquí).
fn diff_flat<S: NodeSource>(
    src: &S,
    from: PageId,
    to: PageId,
    lo: Option<&[u8]>,
    hi: Option<&[u8]>,
    out: &mut Vec<KeyDiff>,
) -> Result<()> {
    let a = collect_range(src, from, lo, hi)?;
    let b = collect_range(src, to, lo, hi)?;
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].0.cmp(&b[j].0) {
            std::cmp::Ordering::Less => {
                out.push(KeyDiff {
                    key: a[i].0.clone(),
                    change: KeyChange::Removed(a[i].1.clone()),
                });
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(KeyDiff {
                    key: b[j].0.clone(),
                    change: KeyChange::Added(b[j].1.clone()),
                });
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                if a[i].1 != b[j].1 {
                    out.push(KeyDiff {
                        key: a[i].0.clone(),
                        change: KeyChange::Modified(a[i].1.clone(), b[j].1.clone()),
                    });
                }
                i += 1;
                j += 1;
            }
        }
    }
    for (key, val) in &a[i..] {
        out.push(KeyDiff {
            key: key.clone(),
            change: KeyChange::Removed(val.clone()),
        });
    }
    for (key, val) in &b[j..] {
        out.push(KeyDiff {
            key: key.clone(),
            change: KeyChange::Added(val.clone()),
        });
    }
    Ok(())
}

/// Hijos de un nodo interno como `(lower, upper, child)`, contiguos y cubriendo
/// `(-inf, +inf)`. `cell[i].key` es la cota superior exclusiva de su hijo.
#[allow(clippy::type_complexity)]
fn child_intervals<S: NodeSource>(
    src: &S,
    id: PageId,
) -> Result<Vec<(Option<Vec<u8>>, Option<Vec<u8>>, PageId)>> {
    let body = src.body(id)?;
    let (cells, rightmost) = node::parse_inner(id.0, body.bytes())?;
    drop(body);
    let mut out = Vec::with_capacity(cells.len() + 1);
    let mut prev: Option<Vec<u8>> = None;
    for c in cells {
        out.push((prev.clone(), Some(c.key.clone()), c.child));
        prev = Some(c.key);
    }
    out.push((prev, None, rightmost));
    Ok(out)
}

/// Entradas `(key, val)` de un subárbol en `[lo, hi)`, en orden.
fn collect_range<S: NodeSource>(
    src: &S,
    id: PageId,
    lo: Option<&[u8]>,
    hi: Option<&[u8]>,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    if id == NO_ROOT {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for item in scan_from(src, id, lo.unwrap_or(&[]))? {
        let (key, val) = item?;
        if hi.is_some_and(|h| key.as_slice() >= h) {
            break;
        }
        out.push((key, val));
    }
    Ok(out)
}

/// Mayor de dos cotas inferiores (`None` = -inf).
fn max_lower<'a>(a: Option<&'a [u8]>, b: Option<&'a [u8]>) -> Option<&'a [u8]> {
    match (a, b) {
        (None, x) | (x, None) => x,
        (Some(x), Some(y)) => Some(x.max(y)),
    }
}

/// Menor de dos cotas superiores (`None` = +inf).
fn min_upper<'a>(a: Option<&'a [u8]>, b: Option<&'a [u8]>) -> Option<&'a [u8]> {
    match (a, b) {
        (None, x) | (x, None) => x,
        (Some(x), Some(y)) => Some(x.min(y)),
    }
}

/// `true` si `[lo, hi)` no es vacío (extremos abiertos siempre válidos).
fn range_valid(lo: Option<&[u8]>, hi: Option<&[u8]>) -> bool {
    match (lo, hi) {
        (Some(l), Some(h)) => l < h,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{CowMemStore, CursorStore, MemStore};

    /// Stress que **replica `apply_fts_row` a nivel b-tree** sobre `CursorStore`
    /// (CoW + cursor de append, como `TxStore`): rows `0x01` → freeze → por doc,
    /// postings `0x00` + `df` `0x03` (read-modify-write con varint creciente, en
    /// orden SEMBRADO que imita el `HashSet`) + doclen `0x01` + global `0x02`. Tras
    /// construir, escanea todas las hojas (`parse_leaf` detecta desorden). Ejercita
    /// el camino O(1) del **cursor de append**, que en producción solo toca `TxStore`
    /// (ningún otro test lo cubría). `FTS_STRESS_DOCS` (env) ajusta la escala;
    /// `FTS_STRESS_SEED` el orden; el `chk=` impreso es determinista por seed.
    ///
    /// HISTORIA: un benchmark a 1M docs vio `CREATE FULLTEXT INDEX` corromper el
    /// b-tree. Este stress lo persiguió y demostró que **NO es un bug del código**:
    /// el build es determinista byte-a-byte (mismo `chk=` en ejecuciones repetidas)
    /// y `#![forbid(unsafe_code)]` hace imposible el UB, pero la corrupción aparecía
    /// en páginas ALEATORIAS como **flips de 1 bit** (XOR potencia de 2) → corrupción
    /// de MEMORIA del entorno (RAM no-ECC / sandbox), no del b-tree. En hardware
    /// estable no debe fallar nunca; un fallo aquí = bit-flip de memoria.
    #[test]
    #[ignore = "stress largo; canario de determinismo del b-tree (correr a mano con FTS_STRESS_DOCS)"]
    fn fts_pattern_stress() {
        use crate::format::put_varint;
        use crate::keyenc::encode_index_value_ref;
        use crate::record::{ValueRef, rowid_be};

        const FTS_ID: [u8; 4] = [0, 0, 0, 1];
        let sub = |s: u8| -> Vec<u8> {
            let mut k = vec![0x03u8];
            k.extend_from_slice(&FTS_ID);
            k.push(s);
            k
        };
        let term_enc = |t: &str| -> Vec<u8> {
            let mut k = Vec::new();
            encode_index_value_ref(ValueRef::Text(t), &mut k);
            k
        };
        let posting = |t: &str, rid: i64, pos: u32| -> Vec<u8> {
            let mut k = sub(0x00);
            k.extend_from_slice(&term_enc(t));
            k.extend_from_slice(&rowid_be(rid));
            k.push(0); // field
            put_varint(&mut k, pos as u64);
            k
        };
        let dfk = |t: &str| -> Vec<u8> {
            let mut k = sub(0x03);
            k.extend_from_slice(&term_enc(t));
            k
        };
        let doclenk = |rid: i64| -> Vec<u8> {
            let mut k = sub(0x01);
            k.extend_from_slice(&rowid_be(rid));
            k
        };
        let globalk = sub(0x02);

        let ndocs: i64 = std::env::var("FTS_STRESS_DOCS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(300_000);
        let mut seed: u64 = std::env::var("FTS_STRESS_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0x1234_5678_9ABC_DEF0);
        // Bisección: validar el árbol cada `VALIDATE_EVERY` docs a partir de
        // `VALIDATE_FROM` (escaneo = O(árbol), por eso configurable).
        let validate_every: i64 = std::env::var("FTS_STRESS_VALIDATE_EVERY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(50_000);
        let validate_from: i64 = std::env::var("FTS_STRESS_VALIDATE_FROM")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let mut rng = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        const VOCAB: usize = 20_000;
        const TOKENS: usize = 40;
        // Términos de LONGITUD VARIABLE (como palabras reales): claves con
        // relaciones de prefijo entre sí, que las claves de longitud fija ocultan y
        // que estresan la comparación `key > last_key` del cursor de append.
        let vocab: Vec<String> = (0..VOCAB)
            .map(|i| {
                let len = 1 + ((i * 2654435761usize) >> 8) % 14; // 1..=14 chars, pseudo
                let mut w = String::with_capacity(len);
                let mut x = i as u64 + 1;
                for _ in 0..len {
                    x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                    w.push((b'a' + (x >> 33) as u8 % 26) as char);
                }
                w
            })
            .collect();

        let mut s = CursorStore::new();
        let mut root = NO_ROOT;
        // Tabla: una fila por doc (keyspace 0x01 — a la izquierda del índice FTS).
        for doc in 0..ndocs {
            let mut rowkey = vec![0x01u8];
            rowkey.extend_from_slice(&rowid_be(doc));
            root = insert(&mut s, root, &rowkey, &[b'x'; 80]).unwrap();
        }
        s.freeze(); // la tabla queda «commiteada»; el índice se construye sobre ella (CoW)

        let mut df: std::collections::HashMap<usize, u64> = std::collections::HashMap::new();
        let mut total: u64 = 0;
        for doc in 0..ndocs {
            // tokens sesgados a comunes (Zipf-ish): 1/3 del top-200 (df crece mucho
            // ⇒ varints crecen ⇒ overwrites con payload mayor, como el texto real).
            let mut terms: Vec<usize> = Vec::with_capacity(TOKENS);
            for pos in 0..TOKENS {
                let r = rng() as usize;
                let t = if r.is_multiple_of(3) {
                    r % 200
                } else {
                    r % VOCAB
                };
                root = insert(&mut s, root, &posting(&vocab[t], doc, pos as u32), b"").unwrap();
                terms.push(t);
            }
            // distintos, en orden SEMBRADO-shuffle (imita iteración de HashSet).
            terms.sort_unstable();
            terms.dedup();
            for i in (1..terms.len()).rev() {
                let j = (rng() as usize) % (i + 1);
                terms.swap(i, j);
            }
            for &t in &terms {
                let key = dfk(&vocab[t]);
                let _ = get(&s, root, &key).unwrap(); // read-modify-write como fts_adjust_varint
                let c = df.entry(t).or_insert(0);
                *c += 1;
                let mut val = Vec::new();
                put_varint(&mut val, *c);
                root = insert(&mut s, root, &key, &val).unwrap();
            }
            let mut dl = Vec::new();
            put_varint(&mut dl, TOKENS as u64);
            root = insert(&mut s, root, &doclenk(doc), &dl).unwrap();
            let _ = get(&s, root, &globalk).unwrap();
            total += TOKENS as u64;
            let mut g = Vec::new();
            put_varint(&mut g, doc as u64 + 1);
            put_varint(&mut g, total);
            root = insert(&mut s, root, &globalk, &g).unwrap();

            // Escaneo periódico (es O(árbol)): parsea todas las hojas ⇒ detecta
            // corrupción pronto. Intervalo/arranque configurables para bisecar.
            if doc >= validate_from && (doc + 1) % validate_every == 0 {
                let mut n = 0u64;
                // Escaneo = O(árbol): parsea cada hoja ⇒ detecta corrupción de orden.
                // Además, checksum FNV-1a del árbol entero: con el mismo seed este
                // valor es determinista byte-a-byte (el build no lee estado no
                // determinista). Sirve de canario: si dos ejecuciones del mismo seed
                // dan checksums distintos, hay corrupción de MEMORIA (bit-flips), no
                // un bug del código —el b-tree es puro y `#![forbid(unsafe_code)]`—.
                let mut chk = 0xcbf2_9ce4_8422_2325u64;
                for r in scan(&s, root).unwrap() {
                    let (k, v) = r.unwrap(); // unwrap = panic si parse ve corrupción
                    for &b in &k {
                        chk = (chk ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
                    }
                    chk = chk.rotate_left(7);
                    for &b in &v {
                        chk = (chk ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
                    }
                    n += 1;
                }
                eprintln!("  doc {} / {ndocs} · {n} claves · chk={chk:016x} · OK", doc + 1);
            }
        }
        let mut n = 0u64;
        for r in scan(&s, root).unwrap() {
            r.unwrap();
            n += 1;
        }
        eprintln!("fts_pattern_stress OK: docs={ndocs} claves_escaneadas={n}");
    }

    fn k(i: u32) -> Vec<u8> {
        format!("clave-{i:06}").into_bytes()
    }

    fn v(i: u32) -> Vec<u8> {
        format!("valor-{i}").into_bytes()
    }

    #[test]
    fn empty_tree() {
        let s = MemStore::new();
        assert_eq!(get(&s, NO_ROOT, b"x").unwrap(), None);
        assert_eq!(scan(&s, NO_ROOT).unwrap().count(), 0);
    }

    #[test]
    fn insert_get_update_delete_small() {
        let mut s = MemStore::new();
        let mut root = NO_ROOT;
        root = insert(&mut s, root, b"b", b"2").unwrap();
        root = insert(&mut s, root, b"a", b"1").unwrap();
        root = insert(&mut s, root, b"c", b"3").unwrap();
        assert_eq!(get(&s, root, b"a").unwrap().unwrap(), b"1");
        assert_eq!(get(&s, root, b"zz").unwrap(), None);

        root = insert(&mut s, root, b"a", b"uno").unwrap();
        assert_eq!(get(&s, root, b"a").unwrap().unwrap(), b"uno");

        let (root, existed) = delete(&mut s, root, b"a").unwrap();
        assert!(existed);
        assert_eq!(get(&s, root, b"a").unwrap(), None);
        let (_, existed) = delete(&mut s, root, b"a").unwrap();
        assert!(!existed);
    }

    /// Fuzz de invariantes ESTRUCTURALES del b-tree: secuencias aleatorias de
    /// insert/delete (RNG xorshift sembrado, varios seeds) contra un `BTreeMap` de
    /// referencia. Periódicamente verifica que (1) `get` coincide con el modelo para
    /// TODA clave viva, y (2) el SCAN completo entrega exactamente las mismas claves
    /// del modelo en orden ESTRICTAMENTE ascendente. Esto delata pérdida, duplicado o
    /// desorden por splits/merges — invariantes de estructura que el test de valor por
    /// clave (`tests/kv.rs`) no comprueba directamente. Claves de longitud variable
    /// (4–12 bytes) con prefijos compartidos para forzar splits y casos de frontera.
    #[test]
    fn fuzz_structural_invariants_match_btreemap() {
        use std::collections::BTreeMap;

        // Pool determinista: longitud variable, los 4 primeros bytes distinguen `i`.
        fn key_of(i: u32) -> Vec<u8> {
            let mut k = i.to_be_bytes().to_vec();
            for j in 0..(i % 9) {
                k.push(i.wrapping_add(j) as u8);
            }
            k
        }

        for seed in [0xA11C_E5EDu64, 0xB0CA_0011, 0xC0FF_EE42, 0xD15E_A5E0] {
            let mut s = MemStore::new();
            let mut root = NO_ROOT;
            let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
            let mut st = seed | 1;
            let mut rnd = || {
                st ^= st << 13;
                st ^= st >> 7;
                st ^= st << 17;
                st
            };

            for round in 0..3000u64 {
                let i = (rnd() % 400) as u32;
                let key = key_of(i);
                if rnd() % 100 < 65 {
                    let vlen = (rnd() % 40) as usize;
                    let val: Vec<u8> =
                        (0..vlen).map(|b| (i as u8).wrapping_add(b as u8)).collect();
                    root = insert(&mut s, root, &key, &val).unwrap();
                    model.insert(key, val);
                } else {
                    let had = model.remove(&key).is_some();
                    let (r, removed) = delete(&mut s, root, &key).unwrap();
                    root = r;
                    assert_eq!(
                        removed, had,
                        "delete reportó {removed} pero el modelo tenía {had} (round {round}, seed {seed:x})"
                    );
                }

                if round % 200 == 0 {
                    // (1) get == modelo para toda clave viva.
                    for (mk, mv) in &model {
                        assert_eq!(
                            get(&s, root, mk).unwrap().as_deref(),
                            Some(mv.as_slice()),
                            "get != modelo (round {round}, seed {seed:x})"
                        );
                    }
                    // (2) scan completo == modelo, en orden estricto ascendente.
                    let mut prev: Option<Vec<u8>> = None;
                    let mut n = 0usize;
                    for item in scan_from(&s, root, b"").unwrap() {
                        let (k, v) = item.unwrap();
                        if let Some(p) = &prev {
                            assert!(*p < k, "scan desordenado: {p:?} !< {k:?} (seed {seed:x})");
                        }
                        let mv = model
                            .get(&k)
                            .unwrap_or_else(|| panic!("clave {k:?} en scan ausente del modelo"));
                        assert_eq!(&v, mv, "valor de scan != modelo para {k:?}");
                        prev = Some(k);
                        n += 1;
                    }
                    assert_eq!(
                        n,
                        model.len(),
                        "scan cuenta {n} != modelo {} (round {round}, seed {seed:x})",
                        model.len()
                    );
                }
            }
        }
    }

    #[test]
    fn for_each_prefix_matches_filtered_scan() {
        let mut s = MemStore::new();
        let mut root = NO_ROOT;
        // 10 grupos × 500 claves: cada prefijo de grupo casa 500 entradas que
        // abarcan varias hojas (fuerza la travesía de hermanos de `for_each_prefix`).
        for g in 0..10u32 {
            for i in 0..500u32 {
                let key = format!("p{g:02}-{i:06}").into_bytes();
                root = insert(&mut s, root, &key, b"").unwrap();
            }
        }
        let collect = |prefix: &[u8]| -> Vec<Vec<u8>> {
            let mut out = Vec::new();
            for_each_prefix(&s, root, prefix, |k| {
                out.push(k.to_vec());
                Ok(())
            })
            .unwrap();
            out
        };
        // Oráculo: scan completo filtrado por prefijo (mismo resultado, en orden).
        let truth = |prefix: &[u8]| -> Vec<Vec<u8>> {
            scan(&s, root)
                .unwrap()
                .map(|r| r.unwrap().0)
                .filter(|k| k.starts_with(prefix))
                .collect()
        };
        for g in 0..10u32 {
            let prefix = format!("p{g:02}-").into_bytes();
            assert_eq!(collect(&prefix), truth(&prefix), "grupo {g}");
            assert_eq!(collect(&prefix).len(), 500);
        }
        assert_eq!(collect(b"p99-"), Vec::<Vec<u8>>::new()); // sin coincidencias
        assert_eq!(collect(b"p").len(), 5000); // casa todo
        assert_eq!(collect(b""), truth(b"")); // prefijo vacío = todas
        assert_eq!(collect(b"p05-000042"), vec![b"p05-000042".to_vec()]); // un match
        let mut empty = Vec::new();
        for_each_prefix(&s, NO_ROOT, b"x", |k| {
            empty.push(k.to_vec());
            Ok(())
        })
        .unwrap();
        assert!(empty.is_empty()); // árbol vacío
    }

    #[test]
    fn many_keys_split_scan_and_drain() {
        let mut s = MemStore::new();
        let mut root = NO_ROOT;
        const N: u32 = 5000;
        // Inserción en orden pseudoaleatorio determinista.
        let mut order: Vec<u32> = (0..N).collect();
        let mut seed = 0x9E3779B97F4A7C15u64;
        for i in (1..order.len()).rev() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            order.swap(i, (seed % (i as u64 + 1)) as usize);
        }
        for &i in &order {
            root = insert(&mut s, root, &k(i), &v(i)).unwrap();
        }
        // Scan completo, ordenado y exacto.
        let all: Vec<_> = scan(&s, root).unwrap().map(|r| r.unwrap()).collect();
        assert_eq!(all.len(), N as usize);
        for (i, (key, val)) in all.iter().enumerate() {
            assert_eq!(key, &k(i as u32));
            assert_eq!(val, &v(i as u32));
        }
        // scan_from a mitad.
        let tail: Vec<_> = scan_from(&s, root, &k(N / 2))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(tail.len(), (N / 2) as usize);
        assert_eq!(tail[0].0, k(N / 2));

        // Borrar todo: el árbol queda vacío y sin páginas residentes.
        for i in 0..N {
            let (nr, existed) = delete(&mut s, root, &k(i)).unwrap();
            assert!(existed, "clave {i} debía existir");
            root = nr;
        }
        assert_eq!(root, NO_ROOT);
        assert!(
            s.pages.is_empty(),
            "quedan {} páginas sin liberar",
            s.pages.len()
        );
    }

    #[test]
    fn overflow_values_roundtrip_and_free() {
        let mut s = MemStore::new();
        let mut root = NO_ROOT;
        let big: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
        root = insert(&mut s, root, b"grande", &big).unwrap();
        root = insert(&mut s, root, b"chico", b"x").unwrap();
        assert_eq!(get(&s, root, b"grande").unwrap().unwrap(), big);

        // Sustituir libera la cadena anterior (en MemStore todo es sucio).
        let pages_before = s.pages.len();
        root = insert(&mut s, root, b"grande", b"ya-no").unwrap();
        assert!(s.pages.len() < pages_before);
        assert_eq!(get(&s, root, b"grande").unwrap().unwrap(), b"ya-no");

        let (root, _) = delete(&mut s, root, b"grande").unwrap();
        let (root, _) = delete(&mut s, root, b"chico").unwrap();
        assert_eq!(root, NO_ROOT);
        assert!(s.pages.is_empty());
    }

    #[test]
    fn key_size_limits() {
        let mut s = MemStore::new();
        assert!(matches!(
            insert(&mut s, NO_ROOT, &[0u8; 2000], b"v"),
            Err(Error::InvalidInput(_))
        ));
        assert!(matches!(
            insert(&mut s, NO_ROOT, b"", b"v"),
            Err(Error::InvalidInput(_))
        ));
        // Clave en el límite exacto: válida.
        let root = insert(&mut s, NO_ROOT, &[7u8; node::MAX_KEY_LEN], b"v").unwrap();
        assert_eq!(
            get(&s, root, &[7u8; node::MAX_KEY_LEN]).unwrap().unwrap(),
            b"v"
        );
    }

    // --- diff (M8) ---

    fn change_of<'a>(d: &'a [KeyDiff], key: &[u8]) -> Option<&'a KeyChange> {
        d.iter().find(|e| e.key == key).map(|e| &e.change)
    }

    #[test]
    fn diff_detects_add_remove_modify_in_order() {
        let mut s = CowMemStore::new();
        let mut from = NO_ROOT;
        for i in 0..200 {
            from = insert(&mut s, from, &k(i), &v(i)).unwrap();
        }
        s.freeze(); // congela `from`: las siguientes mutaciones lo dejan intacto
        let mut to = insert(&mut s, from, &k(999), b"nuevo").unwrap(); // added
        to = insert(&mut s, to, &k(100), b"cambiado").unwrap(); // modified
        to = delete(&mut s, to, &k(50)).unwrap().0; // removed

        let d = diff(&s, from, to).unwrap();
        assert_eq!(d.len(), 3, "{d:?}");
        assert_eq!(
            change_of(&d, &k(999)),
            Some(&KeyChange::Added(b"nuevo".to_vec()))
        );
        assert_eq!(
            change_of(&d, &k(100)),
            Some(&KeyChange::Modified(v(100), b"cambiado".to_vec()))
        );
        assert_eq!(change_of(&d, &k(50)), Some(&KeyChange::Removed(v(50))));
        // Salida en orden ascendente de clave.
        assert!(d.windows(2).all(|w| w[0].key < w[1].key));
    }

    #[test]
    fn diff_empty_and_identical() {
        let mut s = MemStore::new();
        let mut t = NO_ROOT;
        for i in 0..30 {
            t = insert(&mut s, t, &k(i), &v(i)).unwrap();
        }
        assert!(diff(&s, t, t).unwrap().is_empty());

        let added = diff(&s, NO_ROOT, t).unwrap();
        assert_eq!(added.len(), 30);
        assert!(
            added
                .iter()
                .all(|e| matches!(e.change, KeyChange::Added(_)))
        );

        let removed = diff(&s, t, NO_ROOT).unwrap();
        assert_eq!(removed.len(), 30);
        assert!(
            removed
                .iter()
                .all(|e| matches!(e.change, KeyChange::Removed(_)))
        );
    }

    #[test]
    fn diff_is_exact_on_a_large_shared_tree() {
        // Árbol multinivel grande; copia CoW con pocos cambios ⇒ diff exacto y
        // pequeño. La corrección sobre miles de claves implica que el skip por
        // `PageId` funciona: los subárboles compartidos no se materializan.
        let mut s = CowMemStore::new();
        let mut from = NO_ROOT;
        for i in 0..5000 {
            from = insert(&mut s, from, &k(i), &v(i)).unwrap();
        }
        s.freeze();
        let mut to = from;
        for i in [3u32, 1234, 2500, 4999] {
            to = insert(&mut s, to, &k(i), b"X").unwrap(); // 4 modificadas
        }
        to = insert(&mut s, to, &k(9999), b"nuevo").unwrap(); // 1 añadida
        to = delete(&mut s, to, &k(2222)).unwrap().0; // 1 borrada

        let d = diff(&s, from, to).unwrap();
        assert_eq!(d.len(), 6, "{} cambios: {:?}", d.len(), d);
        assert_eq!(
            change_of(&d, &k(9999)),
            Some(&KeyChange::Added(b"nuevo".to_vec()))
        );
        assert_eq!(change_of(&d, &k(2222)), Some(&KeyChange::Removed(v(2222))));
        assert!(matches!(
            change_of(&d, &k(2500)),
            Some(KeyChange::Modified(_, _))
        ));
    }
}
