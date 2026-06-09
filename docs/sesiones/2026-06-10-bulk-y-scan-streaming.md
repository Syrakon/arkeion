# Sesión 2026-06-10 (2) — Bulk-load API y full scan en streaming

Dos piezas, el mismo objetivo: superar a SQLite donde quedaba margen real.
**Bulk-load**: `Connection::bulk_insert` hace **2.5–2.6M filas/s estables**
(SQLite plano: 1.2–2.2M ruidoso; ganó en todas las corridas, 1.17–2.05×).
**Full scan**: **4.5M → 8.07M filas/s (+81 %, 0.26× → 0.48×)** con streaming y
decode perezoso. Suite: **259 verde** (+6 bulk, +4 streaming), clippy/fmt
limpios. Todo por `torii`.

---

## 1. Bulk-load API (`Connection::bulk_insert`)

Carga masiva en una transacción (1 fsync) sin pasar por el executor SQL por
fila: esquema resuelto una vez, contador de rowid en un local, y entradas de
índice **diferidas** — ordenadas e insertadas en bloque al final, con el
dup-check UNIQUE intra-lote (prefijos adyacentes tras ordenar) y contra lo
existente (una sonda por prefijo distinto) **antes de escribir ninguna**.

**Solo autocommit**: o se confirma el lote entero o no queda nada. Esa
atomicidad es la que hace seguro diferir los índices — un fallo a mitad jamás
expone filas sin entradas (dentro de un `BEGIN` del usuario sí podría, por eso
ahí devuelve error y se usa el INSERT normal). Piezas: `catalog::put_row_data`
(fila sin índices, camino sin clones), `resolved_index_entry` (clave + flag de
NULL desde los valores crudos), `flush_index_entries` (sort + UNIQUE + bloque);
`keyenc::encode_index_value_ref` deja un solo sitio con el formato
memcomparable.

## 2. Full scan en streaming con decode perezoso

El diagnóstico: `run_select` materializaba TODAS las filas (`Vec<Vec<Value>>`),
decodificando TODAS las columnas, y proyectaba clonando — y el framing del
README («inherente al formato auto-descriptivo») era incorrecto: el formato de
fila de SQLite también es auto-descriptivo; su ventaja es que **no
materializa** y decodifica solo lo pedido. Eso es exactamente lo que hace ahora
la vía rápida:

- **`btree::CursorState`**: el estado del cursor sin el préstamo de la fuente
  (la fuente se pasa en cada paso). `Cursor` queda como envoltorio prestado.
  `advance_view` entrega clave y valor **prestados de la página sostenida**
  (Arc del pager): cero copias por celda.
- **`record::decode_cols_sorted`**: una pasada que decodifica solo las columnas
  pedidas y salta los payloads del resto (corta tras la última pedida).
  `decode_payload`/`skip_payload` son ahora la única fuente de la forma de cada
  payload (`decode_values` delega).
- **`catalog::ScanState` + `ScanProjection`**: scan sin préstamo con semántica
  idéntica a `finish_row` (alias del rowid de la clave, columnas ausentes →
  DEFAULT), proyección precompilada (columnas únicas ordenadas + slots con
  move-en-último-uso).
- **`api::Rows`** es ahora un enum: `Buffered` (executor clásico) o `Stream`
  (posee su `Snapshot` —que es `Clone` barato— y decodifica al iterar).
  Elegibilidad en `exec::stream_select`: proyección de columnas o `*`, sin
  WHERE/JOIN/GROUP BY/HAVING/ORDER BY/agregados; LIMIT/OFFSET sí. Cualquier
  duda (columna desconocida, calificador ajeno) → camino normal, que produce
  los errores: ambos caminos son indistinguibles salvo en coste.

**Oráculo de equivalencia** (`tests/stream.rs`): la misma consulta dentro de
`BEGIN` va por el executor clásico, así que comparar dentro vs fuera detecta
cualquier divergencia — tipos mixtos con NULLs, rowid negativo, alias, columna
repetida, calificada, LIMIT/OFFSET en los bordes, ALTER ADD COLUMN con DEFAULT,
AS OF/snapshot fijado, y estabilidad del snapshot con escrituras en vuelo.

## Números (disco real, mediana, 50k)

| operación | antes | después | ratio |
|---|--:|--:|:--:|
| insert lote (bulk API) | — | **2.59M** | **1.17–2.05×** |
| full scan | 4.45M (0.26×) | **8.07M** | **0.48×** |
| insert lote (SQL) | 1.76M | 1.99M | 1.02× |

Lo que queda del gap del scan (0.48× → 1×): el paso por celda del b-tree CoW y
el `Vec<Value>` por fila que exige la API de `Row` — no la materialización, que
ya no existe. `COUNT(*)`/agregados siguen materializando (candidato futuro).
