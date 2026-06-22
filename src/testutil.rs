//! Utilidades compartidas de test (solo compila con `cfg(test)`).

use std::collections::HashMap;

use crate::btree::{AppendCursor, Body, NodeSource, NodeStore};
use crate::error::{Error, Result};
use crate::format::{BODY_SIZE, PageId};

/// Almacén en memoria para probar árbol y catálogo sin pager: todas las
/// páginas son «sucias» y `make_dirty` es identidad.
pub(crate) struct MemStore {
    pub(crate) pages: HashMap<PageId, Vec<u8>>,
    next: u64,
}

impl MemStore {
    pub(crate) fn new() -> MemStore {
        MemStore {
            pages: HashMap::new(),
            next: 3,
        }
    }
}

impl NodeSource for MemStore {
    fn body(&self, id: PageId) -> Result<Body<'_>> {
        self.pages
            .get(&id)
            .map(|p| Body::Local(p.as_slice()))
            .ok_or(Error::Corrupt {
                page: id.0,
                reason: "página inexistente (test)",
            })
    }
}

impl NodeStore for MemStore {
    fn alloc(&mut self) -> Result<PageId> {
        let id = PageId(self.next);
        self.next += 1;
        self.pages.insert(id, vec![0u8; BODY_SIZE]);
        Ok(id)
    }

    fn make_dirty(&mut self, id: PageId) -> Result<PageId> {
        Ok(id)
    }

    fn body_mut(&mut self, id: PageId) -> &mut [u8] {
        self.pages.get_mut(&id).expect("página sucia inexistente")
    }

    fn free(&mut self, id: PageId) {
        self.pages.remove(&id);
    }

    fn is_dirty(&self, _id: PageId) -> bool {
        true
    }
}

/// Como `MemStore` pero con **CoW real**: tras `freeze`, modificar una página
/// anterior la copia a una nueva, dejando intactas las raíces ya construidas.
/// Necesario para probar el diff de árboles que comparten estructura (M8): con
/// `MemStore` (mutación in-place) las dos raíces colapsarían en la misma.
pub(crate) struct CowMemStore {
    pages: HashMap<PageId, Vec<u8>>,
    next: u64,
    frozen_below: u64,
}

impl CowMemStore {
    pub(crate) fn new() -> CowMemStore {
        CowMemStore {
            pages: HashMap::new(),
            next: 3,
            frozen_below: 3,
        }
    }

    /// Congela las páginas actuales: a partir de aquí, mutarlas las copia (CoW).
    pub(crate) fn freeze(&mut self) {
        self.frozen_below = self.next;
    }
}

impl NodeSource for CowMemStore {
    fn body(&self, id: PageId) -> Result<Body<'_>> {
        self.pages
            .get(&id)
            .map(|p| Body::Local(p.as_slice()))
            .ok_or(Error::Corrupt {
                page: id.0,
                reason: "página inexistente (test)",
            })
    }
}

impl NodeStore for CowMemStore {
    fn alloc(&mut self) -> Result<PageId> {
        let id = PageId(self.next);
        self.next += 1;
        self.pages.insert(id, vec![0u8; BODY_SIZE]);
        Ok(id)
    }

    fn make_dirty(&mut self, id: PageId) -> Result<PageId> {
        if id.0 >= self.frozen_below {
            return Ok(id); // de esta generación: mutable in-place
        }
        let content = self.pages.get(&id).expect("página congelada").clone();
        let new = self.alloc()?;
        self.pages.insert(new, content);
        Ok(new)
    }

    fn body_mut(&mut self, id: PageId) -> &mut [u8] {
        self.pages.get_mut(&id).expect("página sucia inexistente")
    }

    fn free(&mut self, id: PageId) {
        // Las páginas congeladas son historia compartida: no se liberan.
        if id.0 >= self.frozen_below {
            self.pages.remove(&id);
        }
    }

    fn is_dirty(&self, id: PageId) -> bool {
        id.0 >= self.frozen_below
    }
}

/// Como `CowMemStore` pero ADEMÁS con el **cursor de append** activo, igual que
/// `TxStore` en producción. Ningún otro store de test implementa el cursor (los
/// demás usan el default no-op), así que el camino O(1) de append solo se ejercita
/// aquí — y en producción.
pub(crate) struct CursorStore {
    pages: HashMap<PageId, Vec<u8>>,
    next: u64,
    frozen_below: u64,
    cursor: Option<AppendCursor>,
}

impl CursorStore {
    pub(crate) fn new() -> CursorStore {
        CursorStore {
            pages: HashMap::new(),
            next: 3,
            frozen_below: 3,
            cursor: None,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn freeze(&mut self) {
        self.frozen_below = self.next;
    }
}

impl NodeSource for CursorStore {
    fn body(&self, id: PageId) -> Result<Body<'_>> {
        self.pages
            .get(&id)
            .map(|p| Body::Local(p.as_slice()))
            .ok_or(Error::Corrupt {
                page: id.0,
                reason: "página inexistente (test)",
            })
    }
}

impl NodeStore for CursorStore {
    fn alloc(&mut self) -> Result<PageId> {
        let id = PageId(self.next);
        self.next += 1;
        self.pages.insert(id, vec![0u8; BODY_SIZE]);
        Ok(id)
    }

    fn make_dirty(&mut self, id: PageId) -> Result<PageId> {
        if id.0 >= self.frozen_below {
            return Ok(id);
        }
        let content = self.pages.get(&id).expect("página congelada").clone();
        let new = self.alloc()?;
        self.pages.insert(new, content);
        Ok(new)
    }

    fn body_mut(&mut self, id: PageId) -> &mut [u8] {
        // Como `TxStore`: `body_mut` solo sobre página sucia (de esta generación).
        // Mutar una congelada corromper­ía historia compartida; el b-tree siempre
        // hace CoW antes (`make_dirty`), así que esta aserción de fidelidad no debe
        // saltar nunca.
        assert!(
            id.0 >= self.frozen_below,
            "body_mut sobre página CONGELADA {id:?} (frozen_below={})",
            self.frozen_below
        );
        self.pages.get_mut(&id).expect("página sucia inexistente")
    }

    fn free(&mut self, id: PageId) {
        if id.0 >= self.frozen_below {
            self.pages.remove(&id);
        }
    }

    fn is_dirty(&self, id: PageId) -> bool {
        id.0 >= self.frozen_below
    }

    fn take_append_cursor(&mut self) -> Option<AppendCursor> {
        self.cursor.take()
    }

    fn set_append_cursor(&mut self, cursor: Option<AppendCursor>) {
        self.cursor = cursor;
    }
}
