# 06 — Plan de implementación por hitos

Orden estrictamente incremental: cada hito deja la crate **compilando, testeada y usable** hasta
donde llega. La hash chain se escribe desde M1 (está en el formato), aunque su verificación
pública llegue en M6. Los diferenciadores no esperan a un «big bang»: el formato los soporta
desde el día 0.

```
M0 → M1 → M2 → M3 → M4 ══ MVP ══→ M5 → M6 → M7 → M8 → M9
fundación  KV ACID  relacional  SQL  DML+JOIN   tiempo  audit  cifrado ramas  vacuum
```

## M0 — Fundación: formato, io, pager

**Módulos**: `format`, `io`, `crypto` (solo `PlainProvider`), `pager`.

- Crate `arkeion` con `#![forbid(unsafe_code)]`, CI local (`fmt` + `clippy -D warnings` + `test`).
- Crear/abrir archivo: cabecera, meta slots A/B, append de páginas, lectura con validación de tag.
- **Hecho cuando**: crear → escribir N páginas → reabrir → leer verifica; corrupción de un byte
  en cualquier página se detecta; meta slots alternan y el válido más nuevo gana.

## M1 — B-tree CoW transaccional (motor KV)

**Módulos**: `btree`, `commit`, `tx`.

- Insert/get/delete/scan por rango; splits; overflow; raíz nueva por commit.
- Página de commit completa (hashes incluidos), protocolo de fsync, recuperación por escaneo.
- **Hecho cuando**: *property testing* contra `BTreeMap` de referencia (operaciones aleatorias,
  estados idénticos); test de crash determinista: truncar el archivo en **cada** offset posible
  tras una carga de commits → reabrir nunca pierde un commit confirmado ni resucita uno a medias.
- Entregable intermedio real: **almacén KV embebido ACID**.

## M2 — Capa relacional sin SQL

**Módulos**: `record`, `catalog`.

- Encoding de claves memcomparable y de filas; catálogo (esquema en árbol de datos);
  `next_rowid`; API interna tipada: `create_table/insert/get/scan/delete` con `Value`.
- **Hecho cuando**: round-trip de todos los tipos (incl. límites: `i64::MIN`, NaN, cadenas con
  NUL, blobs > 1 página); orden de scan == orden de rowid; rowids negativos ordenan bien.

## M3 — SQL mínimo + API pública

**Módulos**: `sql`, `exec`, `api`.

- Lexer/parser/AST; `CREATE TABLE`, `INSERT`, `SELECT` (proyección, `WHERE`, `ORDER BY`,
  `LIMIT/OFFSET`); planificador full-scan/point-lookup; `Database/Connection/Rows` + `params!`.
- **Hecho cuando**: suite SQL de aceptación (entrada SQL → salida esperada, archivos de test
  legibles); errores de sintaxis con posición; los ejemplos del [03-api](03-api.md) compilan como doctests.

## M4 — DML completo: el MVP

**Módulos**: ampliaciones de `sql`/`exec`.

- `UPDATE`, `DELETE`, `BEGIN/COMMIT/ROLLBACK`, `Transaction`, prepared statements,
  `INNER/LEFT JOIN` (nested-loop), agregados sin `GROUP BY`, `DEFAULT`/`NOT NULL` aplicados.
- **Hecho cuando**: una app CRUD realista (fixture de gestión: clientes/facturas/líneas)
  corre entera contra Arkeion; rollback restaura estado byte-idéntico; lectores concurrentes
  durante escritura sostenida no observan estados intermedios (test con hilos).

**═══ MVP: aquí Arkeion ya sustituye a SQLite para CRUD básico. ═══**

## M5 — Time-travel

- Índice histórico en árbol meta (se escribe desde M1; aquí se consulta); `AS OF` en parser y
  ejecutor; `Connection::snapshot(AsOf)`; `version()`.
- **Hecho cuando**: tras K commits, `AS OF VERSION i` reproduce exactamente el estado i para
  todo i (test exhaustivo con historial grabado); `AS OF TIMESTAMP` resuelve fronteras
  correctamente (ts exacto, anterior al primero, posterior al último).

## M6 — Auditoría verificable

- `Database::verify()`: recorrido completo de la cadena, `AuditReport`; binario opcional
  `arkeion-verify` (mismo repo) para verificación desde consola.
- **Hecho cuando**: manipular **cualquier** byte de **cualquier** página histórica (fuzz de
  tampering) ⇒ `ChainBroken` con la versión exacta donde rompe; rendimiento: verificar 100k
  commits en segundos, no minutos.

## M7 — Cifrado en reposo

- `Aes256GcmProvider`, gestión del contador de nonces (incl. recuperación post-crash),
  `Options::key`, `WrongKey`.
- **Hecho cuando**: misma suite completa de M0–M6 en verde con cifrado activo (matriz de CI
  claro/cifrado); un volcado hex del archivo no revela ni nombres de tabla ni datos (test de
  no-aparición de plaintext conocido); clave errónea ⇒ `WrongKey`, jamás datos corruptos.

## M8 — Branching, diff y merge

**Módulo**: `branch`.

- `create/drop/connect_branch`, refs en árbol meta; `diff` saltando subárboles con el mismo
  `PageId` (coste O(cambios)); merge 3-way por `(table_id, rowid)` con ancestro común;
  `MergePolicy::FailOnConflict` y reporte de conflictos detallado.
- **Hecho cuando**: rama de migración del ejemplo del [03-api](03-api.md) completa
  (branch → migrar → diff → merge); conflictos detectados: misma fila modificada en ambas ramas,
  esquema divergente; merge limpio aplica exactamente el diff y nada más; `diff` de dos ramas de
  1M filas que difieren en 10 termina en milisegundos.

## M9 — Vacuum, robustez y números

- **Núcleo (entregado)**: `vacuum(retention)` con checkpoint de cadena, replay de deltas y
  rename atómico (`Retention::{KeepAll,KeepLast,KeepSince}`, `VacuumReport`); rotación de clave
  (`vacuum_rekey`); tope y eviction de caché de páginas (`PageCache`, LRU aproximada). Ver D10.
- **Diferido a v1.x**: benchmarks reproducibles vs SQLite (criterio honesto: mismo orden de
  magnitud en CRUD monohilo); fuzzing del parser SQL y del decoder de páginas (dev-only,
  `cargo-fuzz`); guía de operación (backup, retención, claves).
- **Hecho cuando**: tras vacuum, `verify()` OK y las versiones retenidas responden `AS OF`
  (las compactadas → `VersionNotFound`) ✅; vacuum interrumpido (kill en cualquier punto) deja
  el archivo original intacto — garantizado por el rename atómico (publica un inodo nuevo; el
  original nunca se muta in situ) ✅; números de benchmark publicados en `docs/` ⏳ (diferido).

## Después de v1 (registrado, sin compromiso)

**Foco activo (post-M9): completar la base de datos por dos frentes.**
- **(1) SQL más completo**: índices secundarios (`0x02`, el grande: WHERE no-PK pasa de O(filas) a
  O(log n)), `GROUP BY`/`HAVING`, `ALTER TABLE ADD COLUMN`, parámetros nombrados.
- **(2) Superpoderes de primera clase** (lo que nos diferencia de SQLite): API de *time-travel* y
  "git para datos" — línea temporal (`history()`), operaciones de rama y diff/merge ergonómicas,
  auditoría como API. La sintaxis SQL es solo una "cara"; el motor es agnóstico al lenguaje, así
  que un *query builder* tipado en Rust es opcional y barato (no un segundo lenguaje de texto).

**M10 — footprint en disco (compresión de página): ✅ HECHO** (formato v2). Stack para bajar
a/por debajo de SQLite, cumpliendo los tres criterios "Hecho cuando":
- **Slice A** (directorio de páginas + log de registros de tamaño variable + recuperación por
  barrido): `page_id` lógico, ruptura limpia v1→v2.
- **Slice B** (compresor tras `trait`, off por defecto, backend LZSS pure-Rust): **insert 100k filas
  4.0→0.8 MB = 3.5× por debajo de SQLite** (2.8 MB) en datos comprimibles.
- **Slice C** (estabilidad NO-NEGOCIABLE): **Reed-Solomon por página** (corrige N bytes corruptos
  dentro del presupuesto, falla limpio fuera) + **scrubbing** (`scrub()` delata la degradación que
  el ECC corrige en silencio). Réplicas: meta slots 2× + ECC + historia CoW como backup estructural.
- **Pendiente (backlog):** Slice D (directorio persistido → apertura O(log n) en vez del barrido
  O(páginas) de v1, y réplica del directorio), prefix-compression de claves, tier columnar.
Diseño completo en [`09-m10-compresion.md`](09-m10-compresion.md).

**Backlog — modo cliente-servidor (producto "en red"):** exponer Arkeion por un protocolo de red
(p. ej. wire de Postgres o uno propio) con multi-cliente, autenticación y, llegado el caso,
hosting. Es **otro producto/otra liga** (compite con Neon/Postgres, no con SQLite) y se construye
**encima** del motor embebido, sin tocar su núcleo. Interés confirmado: tenerlo en funcionamiento.

**Otros (v1.x):** firma Ed25519 de la cadena, `MergePolicy` adicionales, group commit, export de
historia pre-vacuum, KDF integrado (Argon2id), benchmarks publicados.
