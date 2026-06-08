//! Branching a nivel de aplicación (M8): pone nombre a los cambios de clave
//! cruda del árbol de datos. El diff barato —con skip de subárboles
//! compartidos por `PageId`, coste O(cambios)— vive en `btree`; aquí se
//! decodifica en cambios de fila (`[0x01,table_id,rowid]`) y de esquema
//! (`[0x00,0x01,nombre]`) legibles. El contador interno de rowid no aparece.

use crate::btree::{KeyChange, KeyDiff};
use crate::record;

/// Tipo de cambio sobre una fila o tabla.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Removed,
    Modified,
}

/// Cambio en una fila concreta entre dos ramas.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowChange {
    pub table_id: u32,
    pub rowid: i64,
    pub kind: ChangeKind,
}

/// Cambio de esquema: una tabla creada, borrada o redefinida.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaChange {
    pub table: String,
    pub kind: ChangeKind,
}

/// Diferencias decodificadas entre dos ramas (M8). El contador de rowid y el de
/// table_id (metadato interno) no aparecen.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Diff {
    pub schema: Vec<SchemaChange>,
    pub rows: Vec<RowChange>,
}

impl Diff {
    /// `true` si las dos ramas son idénticas.
    pub fn is_empty(&self) -> bool {
        self.schema.is_empty() && self.rows.is_empty()
    }

    /// Total de cambios (esquema + filas).
    pub fn len(&self) -> usize {
        self.schema.len() + self.rows.len()
    }
}

fn kind_of(change: &KeyChange) -> ChangeKind {
    match change {
        KeyChange::Added(_) => ChangeKind::Added,
        KeyChange::Removed(_) => ChangeKind::Removed,
        KeyChange::Modified(_, _) => ChangeKind::Modified,
    }
}

/// Decodifica los cambios de clave cruda del árbol de datos en un `Diff`.
pub(crate) fn decode(key_diffs: &[KeyDiff]) -> Diff {
    let mut diff = Diff::default();
    for d in key_diffs {
        let key = &d.key;
        match key.first() {
            // Esquema (catálogo): [0x00, 0x01, nombre UTF-8].
            Some(0x00) if key.get(1) == Some(&0x01) => {
                diff.schema.push(SchemaChange {
                    table: String::from_utf8_lossy(&key[2..]).into_owned(),
                    kind: kind_of(&d.change),
                });
            }
            // Otros metadatos del catálogo (contadores): internos, se omiten.
            Some(0x00) => {}
            // Fila: [0x01, table_id BE (4), rowid BE memcomparable (8)].
            Some(0x01) if key.len() >= 13 => {
                let table_id = u32::from_be_bytes(key[1..5].try_into().expect("4 bytes"));
                if let Some(rowid) = record::rowid_from_be(&key[5..13]) {
                    diff.rows.push(RowChange {
                        table_id,
                        rowid,
                        kind: kind_of(&d.change),
                    });
                }
            }
            // Espacios reservados (índices secundarios, v1.1): se ignoran.
            _ => {}
        }
    }
    diff
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_schema_and_row_changes() {
        // Esquema: tabla "t" creada.
        let schema_key = [&[0x00u8, 0x01][..], b"t"].concat();
        // Fila: table_id 1, rowid 42.
        let mut row_key = vec![0x01u8];
        row_key.extend_from_slice(&1u32.to_be_bytes());
        row_key.extend_from_slice(&record::rowid_be(42));
        // Contador interno: debe omitirse.
        let counter_key = vec![0x00u8, 0x02, 0, 0, 0, 1];

        let diffs = vec![
            KeyDiff {
                key: schema_key,
                change: KeyChange::Added(b"esquema".to_vec()),
            },
            KeyDiff {
                key: row_key,
                change: KeyChange::Modified(b"viejo".to_vec(), b"nuevo".to_vec()),
            },
            KeyDiff {
                key: counter_key,
                change: KeyChange::Modified(b"1".to_vec(), b"2".to_vec()),
            },
        ];

        let diff = decode(&diffs);
        assert_eq!(diff.schema.len(), 1);
        assert_eq!(diff.schema[0].table, "t");
        assert_eq!(diff.schema[0].kind, ChangeKind::Added);
        assert_eq!(diff.rows.len(), 1);
        assert_eq!(diff.rows[0].table_id, 1);
        assert_eq!(diff.rows[0].rowid, 42);
        assert_eq!(diff.rows[0].kind, ChangeKind::Modified);
        assert_eq!(diff.len(), 2); // el contador no cuenta
    }
}
