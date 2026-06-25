# 12 — Full-text search (FTS): `MATCH` + índice invertido (diseño + plan)

> **Estado: COMPLETO — F1–F6 hechas y testeadas.** En
> `feat/fts`, testeado: F1 tokenizer, F2 índice invertido + stats BM25 +
> `CREATE/DROP FULLTEXT INDEX` con mantenimiento en insert/update/delete/bulk,
> F3 operador `MATCH` + parser del mini-lenguaje, F4 evaluación (`fts_search`
> contra el índice + eval per-fila → `MATCH` en cualquier posición del WHERE) **y
> narrowing** (el planner usa el índice en SELECT/UPDATE/DELETE, sin full scan),
> F5 **ranking `bm25(col,q)`** (`ORDER BY bm25(…) DESC`, IDF+tf+avgdl; stats
> precomputadas en `run_select` y colgadas del `QuerySchema`) **+ `snippet()` /
> `highlight()`** (extractos con términos resaltados, mismo tokenizer del índice).
> `SELECT id, snippet(body,'a'), bm25(body,'a') FROM t WHERE body MATCH 'a' ORDER
> BY bm25(body,'a') DESC` funciona. **F6 bordes verificados: `MATCH … AS OF
> VERSION n` busca en el pasado y el índice sobrevive al `vacuum`** —ambos *gratis*
> porque vive en el árbol versionado copy-on-write, sin código extra
> (`tests/fts.rs`). Paridad funcional con FTS5 de SQLite, pero **nativo** (no
> virtual table) y **versionado/auditable**. Pendiente (opcional): tokenizer
> email-aware. (FTS implementado en `feat/fts`; F5/F6 y vectores conviven en
> `feat/vectors`, que sale de `feat/fts`.)
>
> **Perf (en `main`):** diccionario de términos + una celda por `(term, doc)` con
> posiciones delta-varint + ranking BM25 *index-only* con `LIMIT` + backfill en
> bloque ⇒ índice **2.4×** menor, build **~8×** más rápido, queries hasta **9×**
> (ver «Rendimiento» y D-FTS6–8).

Primer consumidor real: **papaya** (correo del usuario) para la búsqueda de
mensajes. La migración del store de papaya a Arkeion **no** depende de esto; FTS
es el incremento de búsqueda posterior.

## Por qué nativo (y no un índice externo)

Un índice de búsqueda separado (SQLite FTS5 / tantivy al lado) obliga a mantener
**dos stores** sincronizados: cada escritura actualiza ambos y un crash entre
medias los desincroniza. Nativo = **una transacción, un `fsync`**: el índice
invertido nunca puede divergir del dato, hereda el cifrado, el backup, el
time-travel y la cadena de auditoría sin coste extra. Para un producto de correo
centrado en privacidad esto es decisivo (un solo fichero cifrado, sin un segundo
índice en claro con el contenido de los emails).

## Modelo de almacenamiento

Un índice FTS es un caso especial de índice secundario: misma maquinaria
(b-tree versionado, mantenimiento en cada insert/update/delete, contador de
`index_id`), distinto **grano** — una entrada por *token* en vez de una por valor
de columna. Para no interferir con el escaneo de índices ordinarios (keyspace
`0x02`) se reserva un keyspace propio **`0x03`** con sub-tipo:

```text
[0x03, fts_id BE(4), 0x00, term_id BE(4), rowid BE(8)] → posiciones por field (valor)  (posting)
[0x03, fts_id BE(4), 0x01, rowid BE(8)]                → len doc (varint, nº tokens)
[0x03, fts_id BE(4), 0x02]                             → {N docs, Σ tokens} (avgdl)
[0x03, fts_id BE(4), 0x03, term escapado 0x00 0x00]    → df (nº docs con el término)
[0x03, fts_id BE(4), 0x04, term escapado 0x00 0x00]    → term_id BE(4) (diccionario)
[0x03, fts_id BE(4), 0x05]                             → próximo term_id (u32 LE) (contador)
```

- **Diccionario de términos** (`0x04`/`0x05`): el término (memcomparable, escapado
  `0x00 → 0x00 0xFF`, terminador `0x00 0x00`) se guarda **una sola vez** y mapea a un
  `term_id` de 4 bytes; los postings se indexan por ese `term_id`, no repitiendo la
  palabra en cada celda. El prefijo `term*` se resuelve con un range-scan del
  diccionario → conjunto de `term_id`.
- **Postings** (`0x00`): **una celda por `(term, doc)`**, clave
  `[…0x00, term_id, rowid]`; el **valor** lleva, por cada field, sus posiciones en
  **delta-varint** (`field, count, pos0, Δpos…`). Un prefix-scan sobre
  `[…0x00, term_id]` da todos los `(rowid, posiciones)` del término. (Antes: una celda
  *vacía* por cada `(term, doc, pos)`, con la palabra entera repetida en la clave.)
- **Stats** (`0x01`/`0x02`/`0x03`) son lo que pide Okapi BM25: longitud del doc,
  longitud media (`Σtokens/N`) y frecuencia documental por término (`df`). El
  incremental (`apply_fts_row`) las mantiene celda a celda; el backfill las computa en
  memoria y las escribe ordenadas (ver Rendimiento).

> **Cambio de formato on-disk:** el diccionario + el valor de posting cambian el
> layout del keyspace `0x03` (no `encode_def`, así que el esquema de catálogo sigue en
> v9). **Los índices FTS creados con versiones anteriores deben reconstruirse**
> (`DROP` + `CREATE FULLTEXT INDEX`).

`FtsIndexDef { name, fts_id, columns: Vec<usize>, tokenizer: String, options }`
se guarda **dentro del esquema de la tabla** (como `IndexDef`), así el
mantenimiento ve el índice sin consulta extra. Esquema de catálogo: **v7 → v8**.

Un "documento" = una fila; las columnas indexadas se tokenizan de forma
independiente (las posiciones se reinician por columna y se prefijan con un
field-id para soportar `col:term`).

## Tokenizer (Fase 1 — esta entrega)

Convierte texto en términos buscables. Es **determinista, sin modelo, sin IA**
(no confundir con un tokenizer de LLM): la misma fila produce siempre los mismos
términos, por eso encaja con la auditabilidad. Regla dura del repo: **cero
dependencias de runtime** y `#![forbid(unsafe_code)]` ⇒ todo en `std`.

```rust
pub struct Token { text: String, position: u32, byte_start: usize, byte_end: usize }
pub trait Tokenizer: Send + Sync {
    fn tokenize(&self, text: &str, out: &mut Vec<Token>);   // reusa buffer del llamador
    fn name(&self) -> &str;
}
```

- `byte_start/byte_end` apuntan al texto **original** (no al normalizado): los
  necesitan `snippet()`/`highlight()` para subrayar el match.
- **`unicode`** (default): token = run maximal de `char::is_alphanumeric()`
  (tablas Unicode de `std`); normaliza con `to_lowercase()` + **plegado de
  diacríticos** Latin-1/Extended-A hecho a mano (`café→cafe`, `ñ→n`, `ß→ss`,
  `œ→oe`…, ~lenguas europeas). Toggle `fold_diacritics` (on) y `max_token_len`.
- **`ascii`**: solo alfanuméricos ASCII + lowercase, sin folding (rápido).
- El trait es pluggable: un `email` tokenizer (que de `bob@x.com` emita `bob`,
  `x.com` y la dirección entera) y stemmers Snowball por idioma entran después
  **sin tocar el default**. Stemming queda **fuera de v1**.
- Limitación conocida v1: se asume entrada **NFC**; secuencias descompuestas
  (marca combinante separada) pueden tokenizar imperfecto (no hay normalizador
  Unicode sin dependencia). Aceptable para correo.

## Operador `MATCH` + mini-lenguaje de consulta (Fase 3)

`Expr::Match { col, query, negated }` (calcado de `Expr::Like`; `Kw::Match` en el
lexer, misma precedencia que comparación). La *query string* tiene su **propio
parser** (gramática FTS5): términos, `AND`/`OR`/`NOT`, frase `"..."`, prefijo
`term*`, `NEAR(a b, k)`, filtro por columna `col:term`.

## Planner + ejecución (Fase 4)

Detector de `col MATCH 'q'` en el `WHERE` (análogo a `collect_equalities`,
`exec.rs`), localiza el `FtsIndexDef` de esa columna y enruta a un escaneo FTS:
por término un range-scan de postings, se combinan según el AST (intersección +
chequeo de posiciones para frase/`NEAR`, unión para `OR`, resta para `NOT`) → set
de rowids **+ match-state por fila** (términos, frecuencias, posiciones).

## Ranking + funciones auxiliares (Fase 5 — el punto duro)

`call_function` (`exec.rs`) hoy solo recibe `&[Value]`, sin contexto. Hay que
cablear un `FtsMatchState` por consulta (indexado por rowid) hasta el evaluador:
- `bm25()` / `rank` (Okapi BM25 desde match-state + stats `0x01`–`0x03`),
  `ORDER BY rank`.
- `snippet(col,…)` / `highlight(col,…)`: re-tokenizan el texto de la fila para
  localizar offsets de byte y reconstruyen con marcadores (evita guardar offsets
  en cada posting).

## Slicing

- **F1 — Tokenizer** (`src/fts`): trait + `unicode`/`ascii` + tabla de plegado,
  autocontenido y **unit/property-tested** (posiciones monótonas, offsets válidos
  y que recortan el texto original, folding idempotente). *Núcleo algorítmico
  primero, como `keyenc`/LZSS/RS.* ← entrega actual.
- **F2 — Índice invertido**: keyspace `0x03`, `FtsIndexDef`, esquema v8, contador,
  `CREATE FULLTEXT INDEX`/`DROP`/`REBUILD` con backfill, mantenimiento en
  insert/update/delete, stats BM25.
- **F3 — `MATCH` + parser de query**.
- **F4 — Planner + ejecución de escaneo FTS**.
- **F5 — `bm25`/`rank` + `snippet`/`highlight`** (match-state plumbing).
- **F6 — Bordes**: `MATCH … AS OF`, interacción con `vacuum`, `tests/fts.rs`.

## Rendimiento

Optimizaciones de tamaño, build y latencia (diccionario, colapso de postings,
ranking index-only, backfill en bloque). Medido en **MS MARCO 50k** passages,
embebido vs SQLite **FTS5** (misma máquina, in-process):

| | formato original | **actual** | SQLite FTS5 |
|---|--:|--:|--:|
| Índice | 146 MB | **62 MB** (2.4× menor) | 27 MB (2.3×) |
| Build | 63 s | **8 s** (7.9×) | 0.6 s (13×) |
| Query término común | 25.6 ms | **2.8 ms** (9×) | 1.1 ms (2.5×) |
| Query prefijo / frase | 9.3 / 9.1 ms | **1.3 / 2.4 ms** | 0.6 / 0.6 ms |

Resumen: el índice quedó **2.4× más pequeño** y el build **~8× más rápido** que el
formato original, y la latencia de queries pesadas bajó hasta **9×**. El hueco que
queda con FTS5 (índice ~2.3×, build ~13×, latencia ~2-3×) es, por diseño,
posting-lists segmentadas (D-FTS3) y block-max WAND. _Números de sandbox — re-medir
en hardware ECC con el kit `reverify/` (ver memoria del repo)._

## Decisiones

- **D-FTS1 — Nativo, no virtual table.** Arkeion no tiene vtabs y la
  consistencia/cifrado/time-travel de un store único lo justifican. `MATCH` es un
  predicado especial sobre un índice físico.
- **D-FTS2 — Keyspace `0x03` propio.** Disjunto del `0x02` de índices ordinarios:
  el escaneo de índices normales no ve nunca postings FTS y viceversa.
- **D-FTS3 — Postings term-major, una celda por `(term, doc)`.** El término se guarda
  una vez en el diccionario (`term_id` de 4 B) y las posiciones van en el valor en
  delta-varint; update/delete siguen siendo inserciones/borrados limpios de una celda
  por `(term, doc)` — ideal para copy-on-write y MVCC. **No** se usan posting-lists por
  término (un blob con todos los docs) a propósito: darían un índice ~3.6× menor
  (paridad FTS5) pero romperían el incremental O(tokens) y obligarían a segmentos +
  merge en background.
- **D-FTS4 — Tokenizer en `std`, plegado de diacríticos a mano.** Cero deps;
  stemming fuera de v1 pero pluggable por trait.
- **D-FTS5 — Stats BM25 materializadas e incrementales** (`df`, len doc, avgdl) en
  vez de recalcular por consulta.
- **D-FTS6 — Diccionario de términos.** `term → term_id` (4 B) evita repetir la cadena
  del término en cada posting; los prefijos `term*` se resuelven contra el diccionario.
- **D-FTS7 — Ranking BM25 *index-only* con `LIMIT`.** `… MATCH 'q' ORDER BY
  bm25(col,'q') DESC LIMIT k` se puntúa **desde el índice** (`tf` del valor del
  posting, `dl` de la clave de doclen, `idf`/`avgdl` de stats) y solo materializa un
  shortlist de filas que `run_select` re-rankea exacto — evita un `get_row` +
  re-tokenización por cada doc que casa. Con otros predicados en el WHERE cae al plan
  general. Fórmula BM25 única (`catalog::bm25_idf`/`bm25_term`), compartida con el
  `bm25()` por-fila para que no diverjan.
- **D-FTS8 — Backfill en bloque.** `CREATE FULLTEXT INDEX` acumula el índice en memoria
  (dict/df/postings/doclen/global como mapas) y lo escribe **ordenado por clave**
  (cursor de append), no con una inserción dispersa por token. Más rápido y, de regalo,
  índice más compacto (páginas llenas en vez de partidas a media ocupación).
