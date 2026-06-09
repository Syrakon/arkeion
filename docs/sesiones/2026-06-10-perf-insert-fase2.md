# Sesión 2026-06-10 — Perf de inserts, fase 2

El insert por lotes pasa de **1.11M → ~1.8M filas/s** (+60 %): de 0.57× a
**0.9–1.0× el SQLite plano** (paridad práctica) y de 1.23× a **2.4×** en la
comparación justa con auditoría. Cuatro costes por fila eliminados del camino
caliente, sin tocar formato en disco ni semántica. Suite: **248 verde**, clippy
limpio. Todo por `torii`.

---

## El diagnóstico (trazado sobre el bench real)

El bench inserta con **PK explícita** (`INSERT INTO t (id, n) VALUES (?1, ?2)`),
y ahí el perfil por fila era:

1. `resolve_rowid` hacía un **`btree::contains` por fila** — un descenso O(log n)
   entero solo para el dup-check del rowid explícito — mientras el insert en sí
   ya era O(1) por el cursor de append. El descenso dominaba.
2. `table_cached` devolvía el `TableDef` **clonado en profundidad por
   sentencia** (Strings de columnas + índices): 4–6 asignaciones por fila en un
   lote de statements preparados.
3. `insert_values` del executor asignaba un **`Vec<Value>` nuevo por fila**.
4. `validate_record_into` **clonaba cada valor** (`String`/`Blob` incluidos)
   solo para codificarlo justo después.

## Los cuatro arreglos

### 1. Dup-check sin descenso (`catalog::resolve_rowid`)

**Invariante del contador**: todo rowid existente es `< next` — cada insert deja
el contador por encima y el merge lo reconcilia por máximo
(`merge_reconciles_rowid_counter_by_max`). El camino **automático ya confiaba en
él** (asigna `next` sin dup-check), así que el explícito puede hacer lo mismo:
`n >= next` ⇒ la fila no puede existir ⇒ el `contains` sobra. Un INSERT con PK
explícita creciente queda O(1) como el automático. Salvedad cubierta: si `next`
saturó en `i64::MAX` (un `saturating_add` previo), ese rowid sí puede existir y
se comprueba.

### 2. `Arc<TableDef>` en la caché de esquema (`tx::table_cached`)

Entregar el def cacheado es ahora un bump de refcount, no un clone profundo.
UPDATE/DELETE materializan con `(*def).clone()` donde `QuerySchema` necesita
propiedad (camino templado).

### 3. Buffer de valores prestado (`exec::insert_values_into`)

El executor evalúa la fila en un `Vec<Value>` que vive en la `WriteTx`
(`take_values_buf`/`put_values_buf`): cero asignaciones por fila también entre
sentencias preparadas re-ejecutadas. Si una fila falla, el buffer se pierde con
el `?` — camino frío.

### 4. Codificación sin clones (`record::ValueRef` + `catalog::resolve_col`)

`ValueRef<'a>` es la vista prestada de un `Value` resuelto (escalares por valor,
`Text`/`Blob` por referencia). `resolve_col` queda como **única fuente** de las
reglas de validación (alias→NULL, defaults, promoción INTEGER→REAL, NOT NULL,
tipos) y `record::encode_resolved_into` codifica en dos pasadas (tags, payloads)
directo de los valores prestados. En tablas **sin índices** (el bulk-load
típico) el insert ya no clona ni materializa el registro; con índices se
mantiene el camino de buffers reutilizados (las entradas necesitan el registro
en memoria). `encode_values_into` delega en el nuevo codificador: un solo sitio
con el formato.

## Números (disco real, mediana de 3, 50k filas)

| insert | antes | después | ratio antes | ratio después |
|---|--:|--:|:--:|:--:|
| lote (1 commit) | 1.11M | **1.76–1.85M** | 0.57× | **0.93–1.03×** |
| lote, comparación justa | 1.23× | | | **2.42–2.59×** |
| durable (1/commit) | 647 | ~600–660 | 2.28× | ~2.2–2.3× (fsync-bound, sin cambio) |

El resto de operaciones quedó dentro de su banda de ruido (el full scan oscila
4.4–4.8M entre builds por alineación de código; no hay regresión de camino).

## Lo que queda en el roadmap de inserts

- **Bulk-load API** (construcción bottom-up del b-tree + índices diferidos):
  el margen restante es menor ahora que el camino normal está a ~1× de SQLite,
  pero seguiría aplicando a cargas masivas ordenadas y a tablas con índices.
- El insert **durable** se queda como está: lo limita el `fdatasync` (~1.7 ms),
  ya gana 2× y acelerarlo exigiría relajar garantías.
