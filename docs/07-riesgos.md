# 07 — Riesgos técnicos y puntos calientes del borrow checker

## Borrow checker: dónde apretará y cómo se desactiva el problema

### R1 — Nodos del B-tree: aliasing en estructuras enlazadas

El clásico: un B-tree con nodos enlazados por referencias/`Box` + punteros padre es una pelea
perdida con el borrow checker (aliasing mutable durante splits y rebalanceos).

**Desactivado por diseño**: en Arkeion los nodos **no se enlazan por referencia sino por
`PageId`** (un `u64`). El árbol opera sobre ids; leer un nodo = pedir `Arc<PageBuf>` inmutable
al pager; «mutar» = construir un buffer nuevo y appendearlo (CoW real). No hay grafo de
referencias mutables: no hay pelea. Los cursores guardan una pila de `(PageId, índice_celda)` —
datos planos, sin lifetimes hacia el árbol.

**Riesgo residual**: corrección lógica de splits/merges. Mitigación: property testing contra
`BTreeMap` (M1) y fuzzing del decoder (M9).

### R2 — Caché de páginas compartida entre snapshots e hilos

Un `HashMap<PageId, Arc<PageBuf>>` tras un lock. Las páginas son inmutables ⇒ no existe
invalidación (la entrada de un id nunca cambia de contenido) — se elimina la fuente clásica de
bugs de caché. Riesgos reales: contención del lock (mitigación: sharding por hash del id si los
benchmarks lo piden) y memoria sin tope (mitigación: eviction LRU/clock en M9; las páginas
seguras de evictar siempre, porque releer verifica integridad).

### R3 — `Transaction<'conn>` y ergonomía de lifetimes

Una transacción que toma `&mut Connection` infecta la API. Decisión: `Transaction<'conn>` toma
`&Connection` y el escritor único se adquiere vía `Mutex` interno; `commit(self)`/`rollback(self)`
consumen por valor y `Drop` sin commit = rollback. Riesgo: doble `begin()` en el mismo hilo ⇒
deadlock. Mitigación: `try_lock` + error `Busy` en lugar de bloquear (documentado).

## Durabilidad y archivo

### R4 — Semántica real de fsync

`fdatasync` no garantiza lo mismo en todo SO/FS (y los discos mienten). Mitigaciones: el diseño
ya tolera cola rota (un commit es válido solo si **todas** sus páginas validan su tag, lo que
hace ilegible una escritura desgarrada); por eso basta **un** fsync por commit (M9-perf) sin
barrera intermedia —la recuperación se detiene en la primera página ilegible y nunca adopta un
commit a medias—; `fsync` también del directorio tras crear/renombrar (rename de vacuum); test
de truncado exhaustivo y de commit-a-medias, y más adelante inyección de errores en la capa `io`.

### R5 — Crecimiento del archivo entre vacuums

Cargas write-heavy inflan el archivo (cada commit reescribe el camino raíz→hoja: ~`log n`
páginas + datos). Mitigaciones: vacuum con retención (M9), métrica `pages_written`/commit
expuesta para observabilidad, y agrupar escrituras en transacciones explícitas (documentar:
1000 inserts en una transacción ≈ mismo coste de camino que 1).

### R6 — Backups en caliente

«Backup = copiar el archivo» es seguro **entre** commits, no durante uno (cola a medias se
descarta al abrir → backup consistente con el último commit completo: aceptable). Pero un
backup restaurado y continuado en paralelo con el original **bifurca el contador de nonces**
(D6) si comparten clave. Mitigación v1: documentar (clave nueva o vacuum al restaurar);
v1.x: `Database::backup()` con snapshot + re-keying.

## Criptografía

### R7 — Reutilización de nonce GCM

El riesgo catastrófico del diseño. Cubierto estructuralmente (contador persistido + margen
post-crash, D6) **más** el escenario de R6 (restauración de backups), que es organizativo, no
estructural — por eso se documenta en mayúsculas y se automatiza en v1.x.

### R8 — Gestión de claves fuera del motor

`Key` se zeroiza en `Drop`, pero el motor no gestiona rotación programada ni custodia: eso es
del llamador. Riesgo de mal uso por terceros → guía de operación en M9 con el patrón
correcto (KDF, almacenamiento, rotación vía vacuum).

## Semántica de versionado

### R9 — Conflictos de merge: definición exacta

3-way por `(table_id, rowid)`: conflicto si ambas ramas modificaron/borraron la misma fila de
forma distinta desde el ancestro, o si el esquema divergió de forma no idéntica. Riesgos:
rowids autoasignados que colisionan entre ramas (dos inserts independientes ⇒ mismo rowid,
filas distintas — es conflicto **semántico** indetectable como tal). Mitigación v1: documentar
el patrón (ramas para migraciones/tests, no para escritura concurrente de larga vida);
el reporte de merge marca inserts con mismo rowid en ambas ramas como conflicto explícito.

### R10 — Tensión hash-chain ↔ vacuum

Compactar borra historia que la cadena cubría. Resuelto por diseño con el commit checkpoint
(D10), pero queda un riesgo de proceso: si el operador necesita la historia probatoria completa
debe **exportarla antes** del vacuum (v1: documentado; v1.x: `export_history()` integrado).

### R11 — Reloj de pared

Cubierto por D12 (la versión manda). Riesgo residual menor: timestamps no monótonos en el
índice `0x03` confunden `AS OF TIMESTAMP` (devuelve el resultado correcto según el índice, que
puede sorprender si el reloj saltó hacia atrás). Documentar; no hay corrección posible sin
inventarse el tiempo.

## Proyecto

### R12 — El parser SQL crece sin control

Los subconjuntos SQL tienden a ampliarse por presión de uso. Mitigación: la lista de
[04-sql](04-sql.md) es contrato de v1; todo lo demás se rechaza con error claro y se anota en
«después de v1». Las aplicaciones que generan su SQL vía codegen mantienen la
superficie usada acotada por construcción.

### R13 — Un solo archivo de verdad

Single-file estricto implica que metadatos de operación (locks multi-proceso) no pueden vivir
en archivos auxiliares persistentes. v1: advisory lock sobre el propio archivo (`flock`-style,
vía API segura de std/SO); sin archivos `-wal`/`-shm` jamás — es parte de la promesa.
