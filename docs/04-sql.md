# 04 — Subconjunto SQL v1

Lexer y parser **descendente recursivo escritos a mano** (`sql/lexer.rs`, `sql/parser.rs`):
cero dependencias, errores con posición exacta, y el parser es en sí mismo la documentación
de la gramática. Sin optimizador: el planificador (`exec/planner.rs`) elige entre *full scan*
y *point lookup* por rowid; nada más en v1.

## Tipos

| Tipo SQL | `Value` | Notas |
|---|---|---|
| `INTEGER` | `Integer(i64)` | `INTEGER PRIMARY KEY` ⇒ alias del rowid (estilo SQLite) |
| `REAL` | `Real(f64)` | |
| `TEXT` | `Text(String)` | UTF-8 validado |
| `BLOB` | `Blob(Vec<u8>)` | literales `x'…'` |
| `BOOLEAN` | `Bool(bool)` | `TRUE` / `FALSE` |
| — | `Null` | |

Restricciones v1: `PRIMARY KEY`, `NOT NULL`, `DEFAULT <literal>`. `UNIQUE` y `FOREIGN KEY`
se **aceptan sintácticamente** y quedan registradas en el esquema, pero no se aplican en v1
(documentado en el informe de `CREATE TABLE`; aplicación llega con índices secundarios).

## Sentencias v1

```sql
-- DDL
CREATE TABLE [IF NOT EXISTS] tabla (col TIPO [restricciones], …);
  -- restricciones: PRIMARY KEY | NOT NULL | DEFAULT v | REFERENCES padre [ON DELETE acción]
DROP TABLE [IF EXISTS] tabla;
ALTER TABLE tabla ADD [COLUMN] col TIPO [DEFAULT v] [NOT NULL];  -- al final; no reescribe filas
ALTER TABLE tabla MOVE COLUMN col {FIRST | BEFORE x | AFTER x};  -- reorden lógico (presentación)
ALTER TABLE tabla REORDER COLUMNS (col, …);                     -- fija el orden lógico completo
ALTER TABLE tabla RENAME [COLUMN] old TO new;                   -- solo metadato (nombre)
ALTER TABLE tabla DROP [COLUMN] col;                            -- DROP lógico (tombstone), no reescribe filas
CREATE VIEW [IF NOT EXISTS] vista AS <select>;                  -- SELECT con nombre (no recursiva)
DROP VIEW [IF EXISTS] vista;
CREATE TRIGGER [IF NOT EXISTS] t {BEFORE|AFTER} {INSERT|UPDATE|DELETE} ON tabla
  [FOR EACH ROW] BEGIN <dml>; … END;                            -- row-level; cuerpo con OLD./NEW.
DROP TRIGGER [IF EXISTS] t;

-- DML
INSERT INTO tabla [(cols)] VALUES (expr, …)[, …]
  [ON CONFLICT [(cols)] DO {NOTHING | UPDATE SET col = expr [, …] [WHERE expr]}]
  [RETURNING lista | *];
UPDATE tabla SET col = expr [, …] [WHERE expr] [RETURNING lista | *];
DELETE FROM tabla [WHERE expr] [RETURNING lista | *];

-- Consulta
[WITH cte AS (SELECT …) [, …]]              -- CTEs (tablas con nombre, no recursivas)
SELECT [DISTINCT] lista | *                  -- FROM opcional: sin él, evalúa
  [FROM tabla                               --   expresiones constantes (SELECT 1+1)
  [INNER|LEFT JOIN tabla2 ON expr]]         -- M4, nested-loop
  [WHERE expr]
  [GROUP BY e1, … [HAVING cond]]
  [UNION [ALL] SELECT …]                    -- une (deduplica) / conserva duplicados
  [ORDER BY col [ASC|DESC] [, …]]           -- en UNION, por columna de salida
  [LIMIT n [OFFSET m]]
  [AS OF VERSION n | AS OF TIMESTAMP 'rfc3339'];   -- extensión Arkeion

-- Transacciones
BEGIN; COMMIT; ROLLBACK;
```

`MOVE COLUMN` / `REORDER COLUMNS` reordenan columnas de forma **lógica** (solo el
orden de presentación: la expansión de `*` y el `INSERT` posicional). La posición
**física** y los bytes de las filas no se mueven nunca, así que es O(1), no reescribe
filas y el time-travel queda intacto: un `AS OF` anterior al reorden ve el orden de su
época (el orden de columnas se versiona en el mismo b-tree que los datos). El acceso
por nombre, los índices y el `rowid_alias` son independientes del orden lógico. Es el
modelo de `attlognum` que Postgres planeó y nunca envió; aquí sale gratis porque el
catálogo ya es versionado.

`RENAME COLUMN` solo cambia el nombre (metadato). `DROP COLUMN` es **lógico**
(tombstone, estilo `attisdropped` de Postgres): marca la columna como borrada —deja
de verse en `*` y de resolverse por nombre— pero **congela su posición física** y no
reescribe filas, así que el time-travel queda intacto (un `AS OF` anterior la sigue
viendo) y los bytes muertos los reclama el vacuum. No se puede borrar la PK, la última
columna visible, ni una columna que esté en un índice o FK.

Agregados: `COUNT(*)`, `COUNT(col)`, `SUM`, `AVG`, `MIN`, `MAX`, `GROUP_CONCAT(x[, sep])`.
Admiten `DISTINCT` (`COUNT(DISTINCT x)`, `SUM(DISTINCT x)`, …). `SELECT DISTINCT`
deduplica las filas ya proyectadas.

`MIN`/`MAX` tienen además **forma escalar** con ≥2 argumentos (`MIN(a, b, …)` = el
menor; con 1 argumento son agregados, como en SQLite).

**Funciones escalares** (insensibles a mayúsculas; NULL propaga salvo donde se diga):
- Texto: `UPPER`, `LOWER`, `LENGTH`/`CHAR_LENGTH`, `TRIM`/`LTRIM`/`RTRIM`,
  `SUBSTR`/`SUBSTRING`, `REPLACE`, `INSTR`, `REVERSE`, `HEX`, `CONCAT`,
  `CONCAT_WS(sep, …)`, `LPAD`/`RPAD(s, n[, pad])`, `UNICODE`, `CHAR(…)`, `QUOTE`,
  `PRINTF`/`FORMAT(fmt, …)` (subconjunto C: flags `-`/`0`, anchura, `.precisión`,
  conversiones `d i s f x X o %`), `GLOB(patrón, texto)` (comodines `*`/`?`/`[…]`).
- Numéricas: `ABS`, `ROUND`, `CEIL`/`CEILING`, `FLOOR`, `TRUNC`, `SQRT`, `POW`/`POWER`,
  `MOD`, `SIGN`, `EXP`, `LN`, `LOG`/`LOG10`/`LOG2`/`LOG(base, x)`, `SIN`/`COS`/`TAN`,
  `ASIN`/`ACOS`/`ATAN`/`ATAN2(y, x)`, `PI()`, `RADIANS`/`DEGREES`, `RANDOM`
  (no determinista).
- Condicionales/NULL: `COALESCE`, `IFNULL`, `NULLIF`, `TYPEOF`, `IIF(c, a, b)`
  (azúcar de `CASE WHEN c THEN a ELSE b END`).
- Fecha/hora: `NOW`, `DATE`, `TIME`, `DATETIME`, `STRFTIME(fmt, ms)`,
  `JULIANDAY(ms)`, `UNIXEPOCH(ms)` (→ segundos). El entero de tiempo es **epoch en
  milisegundos** UTC (igual que los timestamps de auditoría), no el día juliano de SQLite.
- JSON (estilo SQLite JSON1; parser en Rust puro, sin dependencias): `JSON(x)`
  (valida y minifica), `JSON_VALID(x)`, `JSON_TYPE(json[, ruta])`,
  `JSON_EXTRACT(json, ruta…)` (escalares desenvueltos; con varias rutas → array;
  rutas `$.a.b[0]`), `JSON_ARRAY_LENGTH(json[, ruta])`, `JSON_OBJECT(k, v, …)`,
  `JSON_ARRAY(v, …)`, `JSON_QUOTE(x)`.

**Operadores/expresiones**: `||` (concat de texto), `CAST(x AS tipo)` (válvula del
tipado estricto), `CASE WHEN … THEN … [ELSE …] END` (buscada y simple),
`x [NOT] BETWEEN a AND b`.

**Subconsultas** (no correlacionadas): escalar `(SELECT …)`, `x [NOT] IN (SELECT …)`,
`[NOT] EXISTS (SELECT …)`. Se ejecutan una vez y se sustituyen por su valor antes de
evaluar la consulta exterior; una subconsulta escalar con >1 fila es error, con 0 → NULL.

**CTEs** (`WITH n AS (SELECT …)`): tablas con nombre materializadas, visibles en el
SELECT que sigue; cada una ve las anteriores y tapa a una tabla real homónima. No
recursivas.

**Vistas** (`CREATE VIEW v AS SELECT …`): como una CTE pero **persistente** — su
SELECT se guarda como texto en el catálogo y se materializa al consultarla (refleja
los datos actuales). Una vista sobre otra funciona; los nombres no colisionan con
tablas. No recursivas.

**Claves foráneas**: `col REFERENCES padre[(col)]` (en columna) o `FOREIGN KEY (a, b)
REFERENCES padre (x, y)` (a nivel de tabla, **compuestas**), con `[ON DELETE acción]
[ON UPDATE acción]` y acción `{RESTRICT|CASCADE|SET NULL}`. Las columnas referenciadas
deben ser la **PK** del padre (referencia por rowid) o estar cubiertas por un índice
`UNIQUE`. Se comprueban en INSERT/UPDATE (el padre debe existir; FK `NULL` permitido);
el DELETE/UPDATE del padre aplica la acción (RESTRICT por defecto: en `ON UPDATE` se
comprueba antes de escribir, y CASCADE/SET NULL después). Auto-referencia (árboles) ok.

**Triggers** (`CREATE TRIGGER … {BEFORE|AFTER|INSTEAD OF} {INSERT|UPDATE|DELETE} ON t
[FOR EACH {ROW|STATEMENT}] BEGIN … END`). El cuerpo son sentencias `INSERT`/`UPDATE`/
`DELETE` (re-parseadas al disparar).
- **`FOR EACH ROW`** (por defecto): una vez por fila afectada, con `OLD.col`/`NEW.col`
  ligadas a los valores de la fila (`NEW` en INSERT/UPDATE, `OLD` en UPDATE/DELETE; en
  `AFTER INSERT`, `NEW.id` es el rowid asignado).
- **`FOR EACH STATEMENT`**: una sola vez por sentencia, aunque afecte a 0 o N filas
  (sin `OLD`/`NEW`).
- **`INSTEAD OF`** (solo en **vistas**, row-level): hace la vista **escribible** — la
  escritura se reemplaza por el cuerpo, con `OLD`/`NEW` = filas de la vista, que el
  cuerpo traduce a la(s) tabla(s) base.

Hay guarda de recursión (un trigger que dispara otra escritura). Buenos para
**bitácoras de auditoría**.

**`RETURNING`** (`INSERT`/`UPDATE`/`DELETE … RETURNING lista | *`): la escritura
**devuelve filas** en vez de solo el recuento — las insertadas, las actualizadas (con
los valores **NEW**) o las borradas (sus valores). La lista admite expresiones y alias,
y `*` se expande a las columnas visibles. Hay que ejecutarla por `query` (devuelve
filas); por `execute` la escritura se hace igual y se ignoran las filas. No se admite al
escribir en una vista (vía INSTEAD OF).

**UPSERT** (`INSERT … ON CONFLICT [(cols)] DO {NOTHING | UPDATE SET …}`): si una fila
choca con la **PK** o un índice **UNIQUE**, en vez de fallar se **omite** (`DO NOTHING`)
o se **actualiza** la fila existente (`DO UPDATE`). En `DO UPDATE`, las expresiones del
`SET`/`WHERE` ven las columnas de la fila existente y `excluded.col` = la fila propuesta;
un `WHERE` falso descarta esa actualización. La lista de columnas objetivo tras
`ON CONFLICT` se acepta pero no restringe (el conflicto se detecta sobre cualquier clave
única). El recuento incluye filas insertadas y actualizadas, no las omitidas.

`GROUP BY` / `HAVING` (post-M9): `SELECT … GROUP BY e1, e2 [HAVING cond]` agrupa por el
valor de las expresiones (normalmente columnas) y emite una fila por grupo, plegando los
agregados de la proyección sobre cada grupo. Una columna fuera de un agregado debe aparecer
en el `GROUP BY` (SQL estándar). `HAVING` filtra los grupos ya agregados; `ORDER BY`/`LIMIT`
actúan sobre la salida (el `ORDER BY` referencia columnas de la proyección).

`SELECT` sin `FROM`: `SELECT 1 + 1`, `SELECT UPPER('hi') AS g` evalúan expresiones
**constantes** contra una única fila implícita (estilo SQLite). Un `WHERE` constante puede
filtrarla (`SELECT 1 WHERE 1 = 0` → vacío). Sin tabla la proyección no admite columnas, `*`
ni agregados, y las cláusulas que necesitan filas (`JOIN`/`GROUP BY`/`HAVING`/`ORDER BY`/`AS OF`)
se rechazan.

## Identificadores

Sin comillas: empiezan por letra o `_` y siguen con letras, dígitos o `_`. Los caracteres
**unicode** cuentan como letra (`café`, `名前`), como en SQLite. Entre **comillas dobles**
(`"…"`, con `""` como escape de una comilla literal) un identificador puede ser una palabra
reservada o llevar espacios/símbolos: `SELECT "select", "mi col" FROM t`. Las comillas
**simples** son siempre cadena de texto, nunca identificador.

## Expresiones (`WHERE`, `SET`, proyección)

```
expr     := or
or       := and ( OR and )*
and      := not ( AND not )*
not      := [NOT] cmp
cmp      := add ( (= | != | <> | < | <= | > | >=| LIKE | IS [NOT] NULL) add )?
add      := mul ( (+ | -) mul )*
mul      := unary ( (* | / | %) unary )*
unary    := [-] primary
primary  := literal | columna | tabla.columna | ?N | :nombre | ( expr ) | función(args)
```

- Parámetros **posicionales** `?1`, `?2`, … (binding `&params![…]`) o **nombrados**
  `:nombre` (binding `&named_params!{ ":nombre" => v }` vía `execute_named`/`query_named`;
  los dos puntos del binding son opcionales y se puede repetir el mismo nombre). No se
  mezclan en una misma sentencia.
- `LIKE` con `%` y `_`, sensible a mayúsculas (como SQLite con `PRAGMA case_sensitive_like`).
- Comparaciones entre tipos distintos: error tipado, **no** coerción silenciosa (filosofía
  human-first: lo sorprendente es un bug). Excepción: `INTEGER` ↔ `REAL` se promociona.
- **División/módulo por cero → `NULL`** (compat SQLite), entero o real; no es error. Un
  resultado real `NaN` (p. ej. `inf - inf`) también se normaliza a `NULL` como en SQLite;
  `±inf` (p. ej. `1e308 * 10`) se conserva. El **desbordamiento de entero** sí es error
  (`+`/`-`/`*` y la división `i64::MIN / -1`, coherente con la filosofía «error antes que
  sorpresa»; SQLite ahí promociona a real). `i64::MIN % -1` da `0` (sin desbordamiento real).

## La extensión `AS OF`

A **nivel de sentencia** (cierra la sentencia, tras todas las cláusulas): toda la consulta se
evalúa contra un único snapshot histórico — sin mezclas por-tabla en v1.

```sql
SELECT total, estado FROM facturas WHERE id = 7 AS OF VERSION 1042;
SELECT * FROM facturas AS OF TIMESTAMP '2026-05-01T00:00:00Z';
```

- `AS OF VERSION n` — exacto; error `VersionNotFound` si `n` se compactó o no existe.
- `AS OF TIMESTAMP t` — resuelve a la **mayor versión con timestamp ≤ t** (recorriendo el
  índice histórico `META_HIST`, que guarda el timestamp en su valor; el índice temporal aparte
  se quitó en M9-perf). El timestamp es informativo; la versión es la autoridad
  ([05-decisiones, D12](05-decisiones.md#d12)).
- Solo en `SELECT`. Escribir en el pasado no existe: para eso están las ramas.

`AS OF` compone con **todo** el dialecto: el ejecutor es genérico sobre la fuente de datos, así
que fija el snapshot histórico una vez y **todo** se evalúa contra él — funciones escalares,
`CAST`/`CASE`/`||`, agregados (`GROUP_CONCAT(DISTINCT …)`), `UNION`/`INTERSECT`/`EXCEPT`,
subconsultas (incluidas las **correlacionadas**), CTEs (`WITH`) y **vistas** (que se
materializan contra ese snapshot; además su propia definición está versionada, así que un
`AS OF` anterior a `CREATE VIEW` no la ve). El **catálogo** se versiona en el mismo b-tree, de
modo que un `AS OF` ve el esquema de su época (columnas, orden lógico, FKs). La única excepción
es el **reloj**: `now()`/`date('now')` devuelven la hora de **ejecución**, no la histórica —
`AS OF` retrotrae los datos, no el tiempo de la sentencia. Cobertura en `tests/timetravel.rs`.

## Fuera de v1 (deliberadamente)

Las subconsultas **correlacionadas**, las CTEs **recursivas** (`WITH RECURSIVE`), las derivadas
en `FROM` (`FROM (SELECT …)`) y `INTERSECT`/`EXCEPT` ya están **hechas** (v1.x); las FKs
**compuestas**/no-PK/`ON UPDATE` y los triggers `INSTEAD OF`/`FOR EACH STATEMENT`, también. Lo
que queda deliberadamente fuera:

| Excluido | Cuándo |
|---|---|
| `ALTER TABLE` físico (cambio de tipo, DROP físico que reescribe filas) | rompería el time-travel sin epoch por fila; `ADD`/`MOVE`/`REORDER`/`RENAME`/`DROP` (lógico) ✅ hechos |
| `DROP COLUMN` que **recupera el espacio** al instante | el DROP lógico deja bytes muertos; el vacuum los reclama en una reescritura |
| Optimizador de queries (CBO con estadísticas) | fuera de alcance; hay un planificador **determinista por reglas**: índice-vs-scan y *predicate pushdown* en JOINs |
| FK: columna no-PK, composite, ON UPDATE | v1.x (v1: una columna → PK del padre, ON DELETE) |
| Triggers `INSTEAD OF` / statement-level / cuerpo no-DML | v1.x (v1: row-level BEFORE/AFTER, cuerpo DML) |

## Gramática y codegen (`.gate`)

El catálogo es serializable a una representación estable (JSON) vía `Database::schema()` →
los generadores externos de SQL (codegen) consumen eso, no el SQL: el contrato
de codegen es el catálogo, no el texto de las sentencias.
