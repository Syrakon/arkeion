# 12 — Full-text search (FTS): `MATCH` + índice invertido (diseño + plan)

> **Estado: EN CURSO — `MATCH` ya BUSCA (F1–F4 hechas).** En `feat/fts`,
> testeado: F1 tokenizer, F2 índice invertido + stats BM25 + `CREATE/DROP
> FULLTEXT INDEX` con mantenimiento en insert/update/delete/bulk, F3 operador
> `MATCH` + parser del mini-lenguaje de consulta, F4 evaluación (`fts_search`
> contra el índice **+** eval per-fila para que `MATCH` funcione en cualquier
> posición del WHERE —OR/NOT/combinado—). `SELECT … WHERE col MATCH 'a AND b'`
> **devuelve filas**. Falta: **narrowing** (que el planner use el índice para no
> hacer full scan), F5 ranking `bm25` + `snippet`/`highlight`, F6 bordes
> (`AS OF`, vacuum). Paridad funcional con FTS5 de SQLite, pero **nativo** (no
> virtual table — Arkeion no tiene vtabs) y **versionado/auditable**: el índice
> vive en el mismo árbol copy-on-write ⇒ `… MATCH 'x' AS OF VERSION n` busca en
> el pasado.

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
[0x03, 0x00, fts_id BE(4), term escapado 0x00 0x00, rowid BE(8), pos varint] → vacío   (posting)
[0x03, 0x01, fts_id BE(4), rowid BE(8)]                                       → len doc (varint, nº tokens)
[0x03, 0x02, fts_id BE(4)]                                                    → {N docs, Σ tokens} (avgdl)
[0x03, 0x03, fts_id BE(4), term escapado 0x00 0x00]                           → df (nº docs con el término)
```

- **Postings** ordenados por `(fts_id, term, rowid, pos)`: un prefix-scan sobre
  `[0x03,0x00,fts_id,term]` da todos los `(rowid, pos)` del término — exactamente
  como `index_lookup`. El `term` se escapa igual que el texto memcomparable
  (`0x00 → 0x00 0xFF`, terminador `0x00 0x00`) para ser self-delimitado frente al
  `rowid` que le sigue.
- **Stats** (`0x01`/`0x02`/`0x03`) son lo que pide Okapi BM25: longitud del doc,
  longitud media (`Σtokens/N`) y frecuencia documental por término (`df`). Se
  mantienen incrementalmente: al añadir el primer posting de un `(term, doc)` se
  incrementa `df`; al borrar el último, se decrementa.

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

## Decisiones

- **D-FTS1 — Nativo, no virtual table.** Arkeion no tiene vtabs y la
  consistencia/cifrado/time-travel de un store único lo justifican. `MATCH` es un
  predicado especial sobre un índice físico.
- **D-FTS2 — Keyspace `0x03` propio.** Disjunto del `0x02` de índices ordinarios:
  el escaneo de índices normales no ve nunca postings FTS y viceversa.
- **D-FTS3 — Postings term-major, una celda por `(term,doc,pos)`.** Menos compacto
  que un posting-list comprimido en payload, pero los update/delete son
  inserciones/borrados limpios — ideal para copy-on-write y MVCC.
- **D-FTS4 — Tokenizer en `std`, plegado de diacríticos a mano.** Cero deps;
  stemming fuera de v1 pero pluggable por trait.
- **D-FTS5 — Stats BM25 materializadas e incrementales** (`df`, len doc, avgdl) en
  vez de recalcular por consulta.
