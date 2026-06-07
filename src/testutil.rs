//! Utilidades compartidas de test (solo compila con `cfg(test)`).

use std::collections::HashMap;

use crate::btree::{Body, NodeSource, NodeStore};
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
