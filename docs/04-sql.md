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
DROP TABLE [IF EXISTS] tabla;
ALTER TABLE tabla ADD [COLUMN] col TIPO [DEFAULT v] [NOT NULL];  -- al final; no reescribe filas
ALTER TABLE tabla MOVE COLUMN col {FIRST | BEFORE x | AFTER x};  -- reorden lógico (presentación)
ALTER TABLE tabla REORDER COLUMNS (col, …);                     -- fija el orden lógico completo

-- DML
INSERT INTO tabla [(cols)] VALUES (expr, …)[, (expr, …)…];
UPDATE tabla SET col = expr [, …] [WHERE expr];
DELETE FROM tabla [WHERE expr];

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
catálogo ya es versionado. (DROP/RENAME COLUMN siguen fuera de v1.)

Agregados: `COUNT(*)`, `COUNT(col)`, `SUM`, `AVG`, `MIN`, `MAX`, `GROUP_CONCAT(x[, sep])`.
Admiten `DISTINCT` (`COUNT(DISTINCT x)`, `SUM(DISTINCT x)`, …). `SELECT DISTINCT`
deduplica las filas ya proyectadas.

**Funciones escalares** (insensibles a mayúsculas; NULL propaga salvo donde se diga):
- Texto: `UPPER`, `LOWER`, `LENGTH`/`CHAR_LENGTH`, `TRIM`/`LTRIM`/`RTRIM`,
  `SUBSTR`/`SUBSTRING`, `REPLACE`, `INSTR`, `REVERSE`, `HEX`.
- Numéricas: `ABS`, `ROUND`, `CEIL`/`CEILING`, `FLOOR`, `SQRT`, `POW`/`POWER`, `MOD`,
  `SIGN`, `RANDOM` (no determinista).
- Condicionales/NULL: `COALESCE`, `IFNULL`, `NULLIF`, `TYPEOF`.
- Fecha/hora: `NOW`, `DATE`, `TIME`, `DATETIME`, `STRFTIME(fmt, ms)`. El entero de
  tiempo es **epoch en milisegundos** UTC (igual que los timestamps de auditoría),
  no el día juliano de SQLite.

**Operadores/expresiones**: `||` (concat de texto), `CAST(x AS tipo)` (válvula del
tipado estricto), `CASE WHEN … THEN … [ELSE …] END` (buscada y simple),
`x [NOT] BETWEEN a AND b`.

**Subconsultas** (no correlacionadas): escalar `(SELECT …)`, `x [NOT] IN (SELECT …)`,
`[NOT] EXISTS (SELECT …)`. Se ejecutan una vez y se sustituyen por su valor antes de
evaluar la consulta exterior; una subconsulta escalar con >1 fila es error, con 0 → NULL.

**CTEs** (`WITH n AS (SELECT …)`): tablas con nombre materializadas, visibles en el
SELECT que sigue; cada una ve las anteriores y tapa a una tabla real homónima. No
recursivas.

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

## Fuera de v1 (deliberadamente)

| Excluido | Cuándo |
|---|---|
| Subconsultas **correlacionadas** (las no correlacionadas sí: escalar/`IN`/`EXISTS`) | v1.x |
| CTEs **recursivas** (las no recursivas, `WITH`, sí) | v1.x |
| Derivadas en `FROM` (`FROM (SELECT …)`) | v1.x |
| `INTERSECT` / `EXCEPT` (`UNION [ALL]` sí) | v1.x |
| Índices secundarios (`CREATE INDEX`) | hecho (v1.1) — espacio de claves `0x02` ya reservado en el formato |
| `ALTER TABLE` salvo `ADD COLUMN` / `MOVE COLUMN` / `REORDER COLUMNS` (DROP/RENAME COLUMN) | v1.x |
| Optimizador de queries | fuera de alcance declarado de v1 |
| Triggers, vistas, FK enforcement | sin fecha |

## Gramática y codegen (`.gate`)

El catálogo es serializable a una representación estable (JSON) vía `Database::schema()` →
los generadores externos de SQL (codegen) consumen eso, no el SQL: el contrato
de codegen es el catálogo, no el texto de las sentencias.
