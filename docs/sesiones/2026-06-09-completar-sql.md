# Sesión 2026-06-09 — Completar SQL

Cierre de los gaps SQL que dejó el dogfooding: **`SELECT <expr>` sin `FROM`**,
**identificadores entre comillas dobles + unicode**, y **división/módulo por cero →
`NULL`** (compat SQLite). Implementado, sometido a una **revisión adversarial
multi-agente**, y arreglados los 5 hallazgos LOW que destapó. Suite final: **244
verde**, clippy + fmt limpios. Todo por `torii`.

---

## 1. Las tres features

### `SELECT <expr>` sin `FROM`
`SelectStmt.from` pasa a `Option<TableRef>` (`sql/ast.rs`). El parser hace el `FROM`
opcional y omite el bucle de joins sin tabla (`sql/parser.rs`). En exec, un nuevo
`run_select_no_from` evalúa expresiones **constantes** contra una única fila implícita
(estilo SQLite): `SELECT 1 + 1`, `SELECT UPPER('hi') AS g`. Admite un `WHERE` y
`LIMIT/OFFSET` constantes, y **rechaza** lo que necesita filas (`*`, columnas,
agregados, `JOIN`/`GROUP BY`/`HAVING`/`ORDER BY`/`AS OF`).

### Identificadores entre comillas dobles + unicode
En el lexer (`sql/lexer.rs`): `"…"` con escape `""` se emite como `Tok::Ident`, así que
una palabra reservada o un nombre con espacios pueden ser identificadores
(`SELECT "select", "mi col" FROM t`). Y los identificadores **sin comillas** aceptan
bytes ≥ 0x80 (cuerpo de un carácter UTF-8), de modo que `café`/`名前` son válidos, como
en SQLite. La comilla simple sigue siendo cadena, nunca identificador.

### División/módulo por cero → `NULL`
En `arith` (`exec.rs`): divisor cero (entero o real) → `NULL`, no error ni `±inf`/`NaN`
— compat SQLite.

---

## 2. Revisión adversarial (método)

Un **Workflow** con 3 revisores en paralelo (una dimensión cada uno: lexer,
parser+exec del no-FROM, aritmética) → cada hallazgo pasa por un verificador
adversarial que intenta **refutarlo** antes de aceptarlo. 6 hallazgos brutos → **5 LOW
reales** (1 descartado). Todos arreglados:

1. **[CLI] `strip_comment` ciego a las comillas dobles** (`bin/ark.rs`). Un `--` dentro
   de un identificador `"a--b"` se trataba como comentario → la línea se truncaba y la
   sentencia se **perdía en silencio** (y arrastraba la siguiente al buffer). Fix: flag
   `in_ident` que se conmuta con `"`; el `--` solo corta fuera de ambas comillas.
2. **[no-FROM] proyección antes de filtrar.** `SELECT 'a' + 1 WHERE 1 = 0` **erraba** en
   vez de devolver 0 filas; el camino con FROM filtra y recorta **antes** de proyectar.
   Fix: `run_select_no_from` decide la supervivencia de la fila (WHERE + `LIMIT/OFFSET`)
   y solo entonces materializa la proyección. Los nombres de columna se computan siempre.
3. **[no-FROM] `AS OF` daba un error confuso** («se esperaba un alias de columna tras
   AS»). Fix: `select_item` mira adelante `AS OF` (como `table_ref`) y no lo trata como
   alias; ahora parsea con `from: None` y exec da «AS OF requiere FROM».
4. **[arith] `i64::MIN % -1`** daba un error falso de desbordamiento (quirk de CPU en
   `checked_rem`); el resultado real es **0**. Fix: `unwrap_or(0)`. La **división**
   `i64::MIN / -1` sigue siendo error de desbordamiento, coherente con `+`/`-`/`*` y con
   la filosofía «error antes que sorpresa» (SQLite ahí promociona a real — divergencia
   documentada).
5. **[arith] `NaN` real escapaba** (`inf - inf`, `inf / inf`) y se podía almacenar,
   rompiendo `IS NULL`/orden/igualdad. Fix: un resultado real `NaN` se normaliza a
   `NULL` (como SQLite); `±inf` se conserva (totalmente ordenado). El comentario del
   guard, que sobre-prometía, se corrigió.

---

## 3. Estado al cierre

- **Suite 244 verde**, `clippy --all-targets` y `fmt` limpios.
- Los 5 arreglos verificados **end-to-end** con el binario `ark` (caja negra por pipe).
- Tests: `tests/sql_features.rs` (`select_without_from`, `quoted_and_unicode_identifiers`,
  `division_by_zero_is_null`) + unit en `exec.rs` (`integer_overflow…`,
  `real_nan_is_null_but_inf_is_kept`), `lexer.rs` (`quoted_and_unicode_identifiers`),
  `parser.rs` (`select_without_from`), `bin/ark.rs` (`strip_comment_respects_quotes`).
  `tests/sql/03_types_errors.sqltest` actualizado a la nueva semántica de `/0`.
- Docs: `docs/04-sql.md` (FROM opcional, sección de identificadores, semántica de
  división/NaN).
- VCS = **torii** (`torii save -am`, `torii sync`); git vetado.

## 4. Pendiente (baja prioridad / defendible)

- `i64::MIN` no escribible como literal (quirk compartido con SQLite); meta-comandos
  case-sensitive; sin `.drop-branch` en el CLI.
- Roadmap previo: caché configurable vía `Options`; BLAKE3 para working sets grandes;
  dirección cliente-servidor. Ver `docs/06-hitos.md` y memoria del proyecto.
