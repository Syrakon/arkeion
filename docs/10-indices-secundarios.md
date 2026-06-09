# 10 — Índices secundarios (diseño + plan)

El último grande del frente "SQL completo": `WHERE` sobre columna no-PK pasa de
**O(filas)** (full scan) a **O(log n)** (descenso por un b-tree de índice). El
formato lo reserva desde el día 0 (keyspace `0x02`, ver `docs/02`).

## Modelo

- **Entrada de índice** en el keyspace `0x02`:
  ```text
  [0x02, index_id BE(4), valor_memcomparable, rowid BE*(8)] → vacío
  ```
  El b-tree global queda ordenado por `(index_id, valor, rowid)`. Para
  `col = V`: escanear el prefijo `[0x02, index_id, enc(V)]` → extraer los rowids
  del sufijo → `get_row(rowid)` por cada uno. El rowid en la clave permite varias
  filas con el mismo valor (índice no único) y un orden total estable.
- **Definición de índice**: `IndexDef { name, index_id, columns: Vec<usize>,
  unique }` se guarda **dentro del esquema de la tabla** (`[0x00,0x01,name]`), así
  `table()` la trae y el mantenimiento (insert/update/delete) ve los índices sin
  una consulta extra. Contador de `index_id` propio (como `table_id`).

## Codificación memcomparable (la pieza fina)

La clave de índice debe ordenar **en bytes** igual que el valor ordena en SQL, y
ser **self-delimitada** (el sufijo rowid no se mezcla con un texto de longitud
variable). Por valor: byte de presencia (`0x00` NULL —ordena primero—, `0x01`
no-null) + codificación:
- **Integer**: 8 bytes BE con bit de signo invertido (reusa `record::rowid_be`).
- **Real**: f64 transformado (si negativo, invertir todos los bits; si no, solo el
  de signo) → BE. Ordena correctamente incluido ±0.
- **Bool**: 1 byte.
- **Text/Blob**: bytes **escapados** (`0x00 → 0x00 0xFF`) + terminador `0x00 0x00`.
  Ordena lexicográfico con prefijos correctos (`"ab" < "abc"`).
Dentro de un índice el tipo de columna es fijo (las filas se coercionan al tipo),
así que no hace falta un tag de tipo en la clave; el byte de presencia basta.

## Slicing

- **1a — Codificador memcomparable** (`keyenc`): `encode_index_value(value, out)`
  order-preserving y self-delimitado. Autocontenido y **property-tested**
  (`enc(a) < enc(b) ⇔ a < b` para pares del mismo tipo; NULL primero). Como
  LZSS/RS: núcleo algorítmico primero.
- **1b — Feature de extremo a extremo**: `IndexDef` + `TableDef.indexes` +
  serialización del esquema + contador `index_id`; `CREATE INDEX` / `DROP INDEX`
  (parser + exec) con **backfill** (escanea filas, inserta entradas); mantenimiento
  en `insert_row`/`update_row`/`delete_row`; **planner** para `WHERE col = const`
  (igualdad) → index scan → `get_row`. Tests: full-scan e index-scan coinciden;
  insert/update/delete mantienen el índice; `EXPLAIN`-equivalente opcional.
- **2 — Refinamientos**: rangos (`col > V`, `BETWEEN`) sobre el índice ordenado;
  índices **UNIQUE** (constraint en insert/update); multi-columna.
- **3 — Array de punteros a celda (perf, cambio de formato de nodo)**: hoy los
  nodos se escanean **linealmente** (`inner_child`/`leaf_seek`); un array de offsets
  ordenado da **búsqueda binaria** in-page → O(log celdas). Es el win para los
  inserts/lookups **aleatorios** que generan los índices secundarios (el cursor de
  append de M10-perf solo cubre el caso secuencial). Cambia el formato de nodo: se
  hace una vez, **tras** tener la feature funcionando.

## Hecho cuando

- `CREATE INDEX i ON t(c)`; `SELECT … WHERE c = ?` devuelve **exactamente** lo
  mismo que el full scan pero descendiendo el índice; `UPDATE`/`DELETE` con `WHERE
  c = ?` también.
- insert/update/delete mantienen el índice: tras cualquier secuencia, escanear el
  índice reproduce el mapeo `valor → rowids` real (test contra la tabla).
- `DROP INDEX` borra definición y entradas; `DROP TABLE` arrastra sus índices.
- Property test de orden del codificador en verde para todos los tipos.
