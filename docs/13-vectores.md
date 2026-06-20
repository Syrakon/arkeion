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

## Híbrido (futuro)

Con BM25 (docs/12) + coseno disponibles, el ranking híbrido es combinar scores en
el `ORDER BY` (p. ej. `0.5*bm25(...) + 0.5*(1 - cosine_distance(...))`), o un
re-rank en dos fases. Lo ideal para búsqueda de correo.

## Slicing

- **V1 — KNN exacto** (este corte): `src/vector.rs` (pack/unpack + cosine/l2/dot,
  property-tested) + registro en `call_function` + test de integración KNN por
  SQL. Núcleo algorítmico primero (como `keyenc`/tokenizer).
- **V2 — quantization** (int8): 4× storage y velocidad de la fuerza bruta.
- **V3 — IVF** (ANN opt-in): centroides k-means como **datos versionados**
  (evento discreto, no muta por-insert), postings por cluster (mismo patrón que el
  FTS), `nprobe` clusters escaneados. Compatible con copy-on-write; HNSW NO.

## Decisiones

- **D-VEC1 — KNN exacto antes que ANN.** Determinista/auditable, y es el cimiento
  (re-rank + ground-truth) del ANN. ANN solo cuando la escala lo pida.
- **D-VEC2 — Vector = BLOB de f32, no tipo nuevo.** Máximamente aditivo; un
  `VECTOR(dim)` tipado con validación de dimensión es una mejora futura.
- **D-VEC3 — Embeddings externos.** Arkeion no embebe modelos (cero deps/ML,
  reproducibilidad).
- **D-VEC4 — Si ANN, IVF no HNSW.** Centroides-como-datos + postings por cluster
  respetan el versionado; el grafo mutable de HNSW no.
