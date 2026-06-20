# 13 — Búsqueda vectorial: KNN exacto (diseño + plan)

> **Estado: EN CURSO.** Slice 1 (KNN exacto) en `feat/vectors`. Búsqueda
> semántica **complementaria** al FTS léxico (docs/12): el FTS encuentra palabras
> exactas, los vectores encuentran *significado parecido*. El objetivo final es el
> **híbrido** (BM25 + similitud vectorial). ANN (IVF) queda como acelerador
> opt-in posterior, no por defecto.

## Por qué KNN exacto primero

Un índice **ANN** (HNSW/IVF) es **aproximado** (recall < 100%), su construcción
es **no determinista** y muta con cada insert — choca con la identidad
*versionada/auditable/reproducible* de Arkeion. El **KNN exacto** (fuerza bruta:
distancia a todos, top-K) es **determinista, completo y reproducible**, y al ser
**solo datos** en el árbol versionado hereda el time-travel gratis
(`… ORDER BY cosine_distance(emb, ?) LIMIT k AS OF VERSION n` busca el pasado
semánticamente). Encaja sin fricción. Además **el ANN se construye ENCIMA del
exacto** (re-ranking de candidatos + ground-truth para medir recall), así que no
es trabajo desechable. Para correo personal (≤ ~1M vectores, peor caso ~cientos
de ms; con quantization int8 mucho más) el exacto **basta**.

## Arkeion no genera embeddings

Nada de ML/modelos/deps (regla dura del repo). Los embeddings los produce el
cliente (papaya) con su modelo y se **guardan como datos**; Arkeion solo
**almacena y busca**. Así sigue 100% determinista y auditable: guarda floats
opacos, igual que un BLOB.

## Diseño (máximamente aditivo)

Un vector = **BLOB de `f32` little-endian** (sin tipo nuevo, sin cambio de
formato, sin tocar el parser). La búsqueda son **funciones escalares puras**
(`src/vector.rs`, cero deps) registradas en `call_function`:

- `vector(x, y, …) → BLOB` — empaqueta floats como `f32` LE (constructor).
- `cosine_distance(a, b) → REAL` — `1 − cos_sim`; menor = más parecido (norma 0 ⇒ 1.0).
- `l2_distance(a, b) → REAL` — distancia euclídea; menor = más parecido.
- `dot(a, b) → REAL` — producto interno; mayor = más parecido.

Distancias acumuladas en `f64`; error si las dimensiones no casan. KNN = SQL que
ya existe:

```sql
CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT, emb BLOB);
INSERT INTO docs (body, emb) VALUES ('…', vector(0.1, 0.2, 0.3));  -- o un BLOB del cliente
SELECT id, body
FROM docs
ORDER BY cosine_distance(emb, vector(0.11, 0.19, 0.31))   -- query vector
LIMIT 10;                                                  -- top-K vecinos
```

El `ORDER BY … LIMIT` recorre las filas calculando la distancia (full scan
exacto) y se queda con los K mejores — KNN exacto, sin estructura nueva.

## Híbrido (HECHO — se compone solo)

Con BM25 (docs/12) + coseno disponibles, la búsqueda **híbrida** (léxica +
semántica) **funciona sin código nuevo**: se expresa con SQL ya existente (CTE +
window functions). El método robusto es **RRF (Reciprocal Rank Fusion)** — fusiona
por *rango* en cada ranking, así evita el problema de escalas (BM25 no acotado vs
coseno ∈ [0,2]):

```sql
WITH ranked AS (
  SELECT id,
    ROW_NUMBER() OVER (ORDER BY bm25(body, 'rust') DESC)                       AS lex,
    ROW_NUMBER() OVER (ORDER BY cosine_distance(emb, vector(1.0,0.0,0.0)) ASC) AS sem
  FROM docs
)
SELECT id FROM ranked ORDER BY 1.0/(60+lex) + 1.0/(60+sem) DESC LIMIT 10;
```

El doc bueno en **ambas** señales gana al mejor de cada una por separado — justo
lo que quieres en correo. Único cambio que hizo falta: que `bm25()` se precompute
también cuando aparece dentro de una window function (`collect_bm25` recurre en
`Expr::Window`). Probado en `tests/hybrid.rs`. *(Alternativa más simple pero
sensible a escala: `ORDER BY w1*bm25(…) + w2*(1 - cosine_distance(…))` con scores
normalizados.)*

## Slicing

- **V1 — KNN exacto** (este corte): `src/vector.rs` (pack/unpack + cosine/l2/dot,
  property-tested) + registro en `call_function` + test de integración KNN por
  SQL. Núcleo algorítmico primero (como `keyenc`/tokenizer).
- **V2 — quantization int8 (HECHO):** constructor `vector_i8()`; formato con byte
  de tag (`0x00` f32 / `0x01` int8 = escala + bytes int8); ~4× menos storage. Las
  distancias desempaquetan ambos formatos transparentemente (query f32 vs
  almacenado int8 funciona). Cuantización simétrica por vector (`max|v|/127`).
- **V3 — IVF (ANN) — HECHO y usable por SQL.** `CREATE VECTOR INDEX vi ON docs(emb)
  [USING cosine|l2] [LISTS k] [PROBES p]` entrena k-means (núcleo `src/ivf.rs`),
  persiste centroides + postings por cluster en keyspace `0x04` (esquema catálogo
  v9), mantiene el índice en insert/update/delete/bulk. El planner enruta
  `ORDER BY cosine_distance(col, ?) LIMIT k` (sin WHERE) al índice: escanea
  `nprobe` clusters y el `ORDER BY`/`LIMIT` rankea exacto los candidatos. `PROBES p`
  fija nprobe por índice (recall vs velocidad; por defecto `ceil(lists/10)`, mín 1),
  serializado en el bloque vectorial v9. `REBUILD VECTOR INDEX vi` re-entrena los
  centroides sobre los datos ACTUALES y re-asigna las filas: el hook de insert mete
  cada fila nueva en el centroide viejo más cercano sin re-particionar, así que tras
  muchas altas el clustering se degrada y REBUILD lo refresca (conserva vidx_id,
  lists, metric y nprobe; comparte el núcleo `build_vector_clusters` con CREATE).
  Centroides como datos versionados (evento discreto, no muta por-insert) ⇒
  compatible con copy-on-write; HNSW NO. **Pendiente:** métrica en el plan vs
  híbrido.

## Decisiones

- **D-VEC1 — KNN exacto antes que ANN.** Determinista/auditable, y es el cimiento
  (re-rank + ground-truth) del ANN. ANN solo cuando la escala lo pida.
- **D-VEC2 — Vector = BLOB de f32, no tipo nuevo.** Máximamente aditivo; un
  `VECTOR(dim)` tipado con validación de dimensión es una mejora futura.
- **D-VEC3 — Embeddings externos.** Arkeion no embebe modelos (cero deps/ML,
  reproducibilidad).
- **D-VEC4 — Si ANN, IVF no HNSW.** Centroides-como-datos + postings por cluster
  respetan el versionado; el grafo mutable de HNSW no.
