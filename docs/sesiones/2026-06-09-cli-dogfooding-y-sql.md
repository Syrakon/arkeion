# Sesión 2026-06-09 — CLI `ark`, dogfooding, y superficie SQL

Resumen de la sesión: se construyó un **CLI para ejercitar Arkeion a mano**, se hizo
un **dogfooding de caja negra** que destapó bugs reales (3 HIGH), **se arreglaron
todos**, y se **ampliaron las funciones/features SQL** (alias, funciones escalares,
`IN`). Todo commiteado y pusheado por `torii`. Suite final: **237 verde**, clippy +
fmt limpios.

Commits de la sesión (más reciente arriba):

```
522bbb0 feat(sql): IN / NOT IN (...) set membership
43e2c7d feat(sql): scalar built-in functions — UPPER/LOWER/LENGTH/ABS/ROUND/TRIM/COALESCE/…
94b1ed3 feat(sql): column aliases — SELECT expr AS name
efb7e67 feat(cli): anchor-based rollback detection — .anchor + .verify <v> <hash>
d155ddf fix(lexer): scientific-notation REAL literals + compact REAL rendering in CLI
05dc6bc fix(time-travel): scope AS OF to the branch ancestry — no cross-branch version leak
95c3326 fix: dogfooding round 1 — parser depth guard, positional INSERT, .changes parent, CLI
19c256a feat(cli): `ark` — interactive shell for hands-on dogfooding
```

---

## 1. El CLI `ark` (nuevo) — `src/bin/ark.rs`

Shell de línea de comandos al estilo `sqlite3`, **sin dependencias nuevas** (D8): lee
stdin a mano y detecta el terminal con `std::io::IsTerminal`; en modo pipe ejecuta el
guion y sale.

```bash
cargo build --bin ark
./target/debug/ark mibase.arkeion              # REPL interactivo
./target/debug/ark cifrada.arkeion --key <64-hex>
echo "CREATE TABLE t(id INTEGER PRIMARY KEY); .tables" | ./target/debug/ark t.arkeion
```

- **SQL**: `SELECT`/`VALUES` → tabla impresa (vía `Row::get::<Value>`); el resto →
  `execute` con conteo de filas afectadas.
- **Meta-comandos** que exponen TODO lo distintivo del motor:
  `.tables` · `.schema` · `.version` · `.history`/`.log` · `.changes <v>` ·
  `.diff <a> <b>` · `.asof <v>`/`.live` · `.branches`/`.branch`/`.checkout`/`.merge` ·
  `.verify` · **`.anchor`** · **`.verify <v> <hash>`** · `.scrub` · `.vacuum [n]` ·
  `.help` · `.quit`.
- Tolera un `;` final de más; ignora comentarios `-- …` (con `strip_comment`
  consciente de cadenas).

**Apoyo en la lib** (para `.tables`/`.schema`): `catalog::list_tables` +
`Snapshot::tables` + `Connection::tables`.

---

## 2. Dogfooding de caja negra (método repetible)

Un **Workflow** lanzó 6 agentes en paralelo conduciendo el `ark` por pipe
(`printf … | ./target/debug/ark /tmp/único.arkeion`), cada uno martilleando un área
—tipos/SQL, índices, time-travel, ramas/merge, auditoría/durabilidad, robustez— y
comparando con la semántica SQL esperada; más un agente sintetizador que **reprodujo y
filtró** bugs reales vs comportamiento defendible. **134 escenarios.**

**Veredicto:** el plano de DATOS es sólido —cero bugs de corrección en
queries/índices/aislamiento de ramas/merge/`AS OF` en vivo/persistencia—. Las grietas
estaban en tres capas de alrededor: **auditoría/durabilidad, robustez del parser, y
alcance de `AS OF`**. Todas cerradas.

> Para repetirlo en el futuro: construir `ark`, lanzar N agentes que lo conduzcan por
> pipe sobre archivos `/tmp` únicos, sintetizar al final. No usar `cargo run` en
> agentes paralelos (lock de build); usar el binario ya construido.

---

## 3. Bugs encontrados y ARREGLADOS

### 🔴 HIGH

1. **Parser sin tope de profundidad → SIGABRT** (`95c3326`). SQL muy anidado
   (p. ej. 50 000 paréntesis) desbordaba la pila y **abortaba el proceso** —
   inaceptable en un motor embebido que prohíbe `unsafe`. Fix: cap de 256 niveles en
   `Parser::expr()` → error limpio «expresión demasiado anidada».
2. **`AS OF` cruzaba ramas** (`05dc6bc`). Resolvía la versión contra el índice meta
   **global**, así que desde una rama se podían leer datos de otra línea temporal —
   violación del modelo git-for-data. Fix: `Store::snapshot_at_on(branch, at)` solo
   resuelve versiones en la **ascendencia** de la rama (`is_ancestor`, caminando los
   padres registrados); `Connection::snapshot` y el `AS OF` inline pasan
   `&self.branch`. `AS OF TIMESTAMP` también camina la ascendencia.
3. **Truncar 1 byte revertía el head y `.verify` decía «cadena OK»** (`efb7e67`). Es
   la ambigüedad inherente *cola-rota (crash) vs manipulación* del append-only: la
   cadena más corta resultante es válida, y `verify()` sola no puede distinguirla. La
   **mitigación son las anclas** (`verify_anchor`), que existían pero no eran
   accesibles desde el CLI. Fix: `.anchor` captura un ancla (versión + hash) para
   guardar aparte, y `.verify <v> <hash>` la verifica → **detecta el rollback**
   (verificado de extremo a extremo: 4 commits → `.anchor` → `truncate -1` → reabrir
   → `.verify` pelado dice OK pero `.verify v4 <hash>` delata el commit perdido).

### 🟠 MEDIUM

- **`.changes <v>` usaba `v-1`** en vez del padre registrado (`95c3326`) → en
  merges/puntos de bifurcación inventaba borrados fantasma y ocultaba inserts. Ahora
  usa `read_parent_version(v)`.
- **INSERT posicional con menos valores que columnas** se aceptaba en silencio
  rellenando NULL (`95c3326`) → viola SQL. Ahora exige un valor por columna (la forma
  con lista de columnas sigue nombrando un subconjunto).
- **Literales REAL científicos** (`1e308`, `1.5e3`, `100.`) rechazados por el lexer
  (`d155ddf`) → ahora se aceptan exponente y punto final.

### 🟡 LOW / CLI

- Comentarios `--` mal enrutados en el CLI → `strip_comment` (`95c3326`).
- `.vacuum <arg-no-numérico>` compactaba TODO en silencio (riesgo por typo) → ahora
  valida (`95c3326`).
- `.scrub` sobrevendía «corrige bit-rot»: es **diagnóstico** (el append-only no
  reescribe in situ; reparar = `vacuum`/restaurar). Reformulado (`95c3326`).
- REAL se renderiza con `{:?}` (forma corta round-trip, siempre con `.` o `e`) → un
  REAL `5.0` ya no se ve como `5` (`d155ddf`).

**Regresiones:** `tests/dogfood_fixes.rs` (6 tests).

---

## 4. Features SQL añadidas

> Patrón para añadir una variante a `Expr`: tocar `is_const` / `contains_param` /
> `has_aggregate` (`ast.rs`) + `validate_columns` / `col_outside_agg` /
> `collect_columns` / `fold_aggregates` / `eval` (`exec.rs`). El compilador lista los
> `match` no exhaustivos.

- **Alias de columna** (`94b1ed3`): `SELECT expr AS nombre`. `SelectItem::Expr { expr,
  alias }`; la salida toma el alias en proyección normal, agregados y `GROUP BY`.
- **Funciones escalares** (`43e2c7d`): `Expr::Function`, dispatch por nombre
  (insensible a mayúsculas) en `call_function`. Set: `upper · lower · length/char_length
  · trim/ltrim/rtrim · abs · round(x[,d]) · coalesce · ifnull · typeof ·
  substr/substring(s, inicio[, largo])`. Se anidan (`LENGTH(TRIM(s))`); NULL propaga;
  tipo equivocado = error; función desconocida = error en exec.
- **`IN` / `NOT IN (...)`** (`522bbb0`): `Expr::In`, lógica trivalente SQL (NULL a la
  izquierda → NULL; no hallado con NULL en la lista → desconocido; `NOT IN` invierte
  el booleano, no el NULL). El lexer gana la keyword `IN`.

**Tests:** `tests/sql_features.rs` (3 tests).

---

## 5. Estado al cierre

- **Suite: 237 verde**, `clippy --all-targets` y `fmt` limpios.
- Todo **commiteado y pusheado** por `torii` (working tree limpio).
- VCS del repo = **torii** (`torii save -am`, `torii sync`); git está vetado.

---

## 6. Pendiente (baja prioridad / defendible)

- **`SELECT <expr>` sin `FROM`** (evaluar expresiones constantes sin tabla): único gap
  SQL del dogfood sin hacer; requiere `SelectStmt.from: Option<TableRef>` y un camino
  "sin tabla" en `run_select` (churn moderado).
- Identificadores entre comillas dobles / unicode; `i64::MIN` no escribible como
  literal (quirk compartido con SQLite); división por cero da error (vs NULL de
  SQLite); meta-comandos case-sensitive; sin `.drop-branch` en el CLI (la API
  `Database::drop_branch` existe).
- Roadmap previo (en memoria): caché configurable vía `Options`; BLAKE3 para working
  sets muy grandes (comprobar SHA-NI antes); dirección cliente-servidor.

## 7. Cómo retomar

```bash
cargo build --bin ark && ./target/debug/ark /tmp/prueba.arkeion   # jugar a mano
cargo test                                                         # 237 verde
cargo clippy --all-targets && cargo fmt
torii status                                                       # estado VCS
```

Docs de diseño relacionados: `docs/03-api.md`, `docs/04-sql.md`, `docs/06-hitos.md`.
