# 01 — Arquitectura

## La decisión central: el archivo es el WAL

La especificación pide «B-tree + WAL append-only». Arkeion lo satisface con un diseño más
fuerte que la pareja clásica B-tree in-place + WAL separado: un **B-tree copy-on-write (CoW)
en un archivo append-only**. Nunca se sobrescribe una página de datos; cada transacción de
escritura añade al final del archivo las páginas nuevas/modificadas más una **página de commit**
que apunta a la nueva raíz.

Esta única decisión hace que los cuatro diferenciadores se deriven del mismo mecanismo en
lugar de ser features pegadas encima:

| Diferenciador | Cómo se deriva |
|---|---|
| **Time-travel** | Las páginas antiguas son inmutables. Leer la versión N = resolver el commit N y leer desde su raíz. Coste O(log n), sin replay. |
| **Branching** | Una rama es una *ref* con nombre apuntando a un commit, como en Git. Dos ramas comparten físicamente todas las páginas que no divergen. |
| **Hash chain** | Cada página de commit incluye `SHA-256` del contenido y el hash encadenado del commit anterior. La cadena se escribe desde el día 0, no se añade después. |
| **Recuperación ACID** | Un commit es válido solo si su página de commit es íntegra. Tras un crash, la cola rota del archivo simplemente se ignora. El «replay del WAL» es un escaneo de cola. |
| **Lecturas sin lock** | Un lector fija un commit (snapshot) y lee páginas inmutables. Cero coordinación con el escritor. |

El precio: el archivo crece con la historia. Se paga con `vacuum` (compactación con política de
retención, ver [05-decisiones](05-decisiones.md#d10) y [02-formato](02-formato-archivo.md)).

## Capas y módulos

Dirección de dependencia estricta: de arriba hacia abajo. Ninguna capa inferior conoce a una superior.

```
┌─────────────────────────────────────────────────────────┐
│  api      Database · Connection · Transaction · Rows    │  API pública
├─────────────────────────────────────────────────────────┤
│  sql      lexer → parser → AST                          │
│  exec     planificador trivial + ejecutor               │  Motor de queries
├─────────────────────────────────────────────────────────┤
│  branch   diff O(cambios) · merge 3-way                 │
│  catalog  tablas · esquema · refs · índice histórico    │  Capa lógica
│  record   codificación de filas y claves                │
├─────────────────────────────────────────────────────────┤
│  tx       snapshots de lectura · escritor único         │
│  commit   construcción de commits · hash chain ·        │  Capa transaccional
│           recuperación                                  │
│  btree    B-tree copy-on-write direccionado por PageId  │
├─────────────────────────────────────────────────────────┤
│  pager    caché de páginas · meta slots · integridad    │
│  crypto   trait CryptoProvider (AES-256-GCM | claro)    │  Capa física
│  io       read_at / write_at portable                   │
│  format   constantes y layouts binarios                 │
└─────────────────────────────────────────────────────────┘
```

### Responsabilidades por módulo

| Módulo | Responsabilidad | Tipos clave |
|---|---|---|
| `format` | Constantes, magics, offsets de los layouts. Sin lógica. | `PAGE_SIZE`, `PageId`, `PageType` |
| `io` | Lectura/escritura posicional portable (Unix `FileExt::read_at`, Windows `seek_read`). | `DbFile` |
| `crypto` | `trait CryptoProvider { seal(page), open(page) }`. Implementaciones: `Aes256GcmProvider`, `PlainProvider` (integridad por SHA-256 truncado). | `CryptoProvider`, `Key` |
| `pager` | Append de páginas, lectura cacheada (`Arc<PageBuf>` inmutables), meta slots A/B, validación de integridad. | `Pager`, `PageBuf` |
| `btree` | B-tree CoW: `get/insert/delete/scan` sobre claves byte. Opera sobre `PageId`, nunca sobre referencias entre nodos. Overflow para valores grandes. | `Tree`, `Cursor` |
| `commit` | Construye la página de commit (raíces, hashes, contador de nonces), protocolo de fsync, escaneo de recuperación, verificación de cadena. | `CommitHeader`, `ChainVerifier` |
| `tx` | `Snapshot` (lectura, fija un commit), `WriteTx` (única, serializada por `Mutex`). | `Snapshot`, `WriteTx` |
| `record` | Codificación memcomparable de claves y formato compacto de filas. | `Value`, `RowCodec`, `KeyCodec` |
| `catalog` | Esquema de tablas en el árbol de datos (ramifica con los datos); refs e índice histórico en el árbol meta (global). | `Catalog`, `TableDef` |
| `sql` | Lexer y parser descendente recursivo escritos a mano. Cero dependencias. | `Token`, `Stmt`, `Expr` |
| `exec` | Planificador trivial (full scan + filtro; sin optimizador) y ejecutor iterador-por-fila. | `Plan`, `Executor` |
| `branch` | Diff entre ramas saltando subárboles físicamente compartidos; merge 3-way a nivel de fila. | `Diff`, `MergeReport` |
| `api` | Fachada pública ergonómica estilo rusqlite. Único módulo re-exportado. | `Database`, `Connection` |

## Los dos árboles

Cada commit referencia **dos raíces** (ver justificación en [05-decisiones, D3](05-decisiones.md#d3)):

- **Árbol de datos** (`data_root`) — catálogo + filas. **Ramifica**: sigue la ascendencia de su rama.
- **Árbol meta** (`meta_root`) — refs de ramas + índice histórico (versión → commit, timestamp → versión).
  **Global y lineal**: cada commit, de cualquier rama, parte del árbol meta del commit global anterior.
  Así las refs nunca divergen entre ramas.

## Modelo de concurrencia

- **Lectores**: `Connection::snapshot()` fija una página de commit; todas las lecturas van contra
  páginas inmutables vía caché compartida. N lectores concurrentes, cero locks compartidos con
  escritura. Aislamiento: **snapshot isolation**.
- **Escritor**: exactamente uno, serializado con `Mutex<Writer>` a nivel de `Database`. Las
  escrituras son por tanto **serializables** por construcción. Throughput de escritura limitado a
  un hilo: aceptado en v1 (ver [07-riesgos](07-riesgos.md)); *group commit* como evolución.
- **Multiproceso**: lock de archivo (advisory) en `open()`; un solo proceso escritor. v1 no
  arbitra escritores entre procesos.

## Flujo de una escritura

```
execute("UPDATE …")
  → sql::parse → exec::plan
  → WriteTx: btree CoW produce páginas nuevas en memoria
  → pager: append páginas al archivo            ── fdatasync
  → commit: página de commit (raíces + hashes)  ── fdatasync   ← punto de durabilidad
  → meta slot A/B actualizado (lazy, no en el camino crítico)
```

## Flujo de una lectura con time-travel

```
query("SELECT … AS OF VERSION 42")
  → árbol meta: hist[42] → PageId del commit 42
  → Snapshot{commit 42} → btree::scan desde data_root(42)
```
