# 02 — Formato de archivo

Formato propio, diseñado desde cero. **Explícitamente independiente de SQLite**: ni el layout de
páginas, ni el encoding de registros, ni el modelo de journal derivan de él.

Convenciones: enteros de cabecera en **little-endian**; claves de B-tree en encoding
**memcomparable big-endian**. Extensión de archivo recomendada `.arkeion` (no impuesta:
el motor acepta cualquier ruta — *zero-config*).

## Layout global

```
página 0      Cabecera de archivo        (inmutable tras la creación)
página 1      Meta slot A                (reescritura alternada, estilo LMDB)
página 2      Meta slot B
página 3…     Zona append-only: páginas de datos y páginas de commit
```

Las **únicas** escrituras in-place del formato son los dos meta slots, que solo contienen
punteros de arranque (caché del último commit). Todo lo demás es append-only.

## Página en disco — 4096 bytes

`PAGE_SIZE = 4096` (== página de SO y sector lógico común; minimiza torn writes y alinea la
unidad de cifrado). `PageId = u64` (offset en bytes = id × 4096).

Layout uniforme, con o sin cifrado (la reserva criptográfica se paga siempre: 28 B = 0,7 %,
a cambio de un B-tree totalmente ignorante del cifrado — D2/D6 en [05-decisiones](05-decisiones.md)):

```
0..12    nonce        (cifrado: contador GCM de 96 bits | claro: ceros)
12..28   tag          (cifrado: tag GCM con AAD = page_id | claro: SHA-256(LE(page_id) ‖ body)[0..16])
28..4096 body         (4068 B; cifrado: ciphertext del body)
```

El tag liga el contenido a su `page_id`: una página recolocada en otro offset es corrupción
detectada, no datos válidos en el sitio equivocado. Sin cifrado, el nonce debe ser cero
(se verifica): los 4096 bytes de la página quedan cubiertos en ambos modos.

Integridad **siempre** verificada al leer, en ambos modos. Con cifrado activo, todas las páginas
van cifradas salvo la cabecera y los meta slots (que no contienen datos de usuario: solo magics,
punteros y versiones — ni siquiera los nombres de tabla quedan en claro).

### Identificación de páginas

Las páginas de **datos** llevan su tipo en el primer byte del body:

| Tipo | Valor | Contenido |
|---|---|---|
| Hoja B-tree | 0x01 | Celdas clave→valor |
| Interna B-tree | 0x02 | Celdas clave→hijo |
| Overflow | 0x03 | Continuación de valores grandes |

Las páginas **estructurales** se identifican por su magic de 8 bytes en `body[0..8]`
(sin colisión posible: ningún tipo de datos vale 0x41 = `'A'`):

| Página | Magic | Posición |
|---|---|---|
| Cabecera | `"ARKEION1"` | página 0 |
| Meta slot | `"ARKMETA1"` | páginas 1–2 |
| Commit | `"ARKCMT01"` | zona append |

## Cabecera de archivo (body de la página 0)

```
0..8     magic            "ARKEION1"
8..12    format_version   u32 = 1
12..16   page_size        u32 = 4096
16..20   flags            u32   bit0 = cifrado activo
20..36   file_id          [16]  aleatorio en la creación (semilla de la cadena génesis)
36..52   kdf_salt         [16]  reservado (v1: clave cruda de 32 B, sin KDF)
52..     reserva          ceros
```

## Meta slot (body de páginas 1 y 2)

```
0..8     magic              "ARKMETA1"
8..16    version            u64   versión del último commit reflejado
16..24   last_commit_page   u64
24..32   n_pages            u64   longitud del archivo (en páginas) en ese commit
```

El escritor alterna A/B. Al abrir: se toma el slot válido (integridad del tag) con mayor
`version` y se **escanea hacia delante** desde `n_pages` por si hay commits posteriores al
slot (la actualización del slot es lazy, fuera del camino crítico de durabilidad).

## Página de commit (body, tipo 0x04)

```
0..8      magic           "ARKCMT01"
8..12     flags           u32   bit0 = checkpoint de compactación (vacuum)
12..16    reservado       u32
16..24    version         u64   monótona global, arranca en 1
24..32    parent_page     u64   commit padre EN LA RAMA (0 = génesis)
32..40    prev_page       u64   commit anterior GLOBAL (0 = génesis)
40..48    timestamp_ms    u64   reloj de pared, informativo (la versión es la autoridad)
48..56    data_root       u64   raíz del árbol de datos de la rama
56..64    meta_root       u64   raíz del árbol meta global
64..72    nonce_counter   u64   próximo contador GCM tras este commit
72..80    pages_written   u64   páginas añadidas por este commit
80..144   branch          [64]  nombre de rama UTF-8, padding con ceros (máx. 64 B)
144..176  content_hash    [32]  SHA-256 de los bodies EN CLARO escritos por el commit, en orden
176..208  prev_chain      [32]  chain_hash del commit global anterior
208..240  chain_hash      [32]  ver fórmula
240..     reserva         ceros
```

```
chain_hash = SHA-256( prev_chain ‖ content_hash ‖ LE(version) ‖ LE(timestamp_ms)
                      ‖ LE(data_root) ‖ LE(meta_root) ‖ branch )
génesis:     prev_chain = SHA-256( "ARKEION1" ‖ file_id )
```

La cadena es **lineal sobre el orden global de commits** (todas las ramas): manipular cualquier
escritura pasada, en cualquier rama, rompe la cadena. `content_hash` cubre el plaintext: la
verificación de auditoría requiere la clave (coherente con que un auditor legítimo la tenga, y
sobrevive a rotaciones de clave vía vacuum).

Un commit con `flags.checkpoint = 1` lo escribe `vacuum`: su `prev_chain` transporta el
`chain_hash` cabeza de la historia truncada, preservando la continuidad verificable de la cadena
aunque las páginas antiguas ya no existan.

## Páginas B-tree

```
hoja (0x01):     [type u8][flags u8][ncells u16] · celdas secuenciales
                 celda: [flags u8][klen varint][key][vlen varint][val]
                 flags bit0 = overflow: [flags][klen][key][len total varint][primera overflow u64]
                 (celda inline > 1280 B ⇒ el valor va a overflow; clave máx. 1024 B)
interna (0x02):  [type][flags][ncells u16][rightmost u64] · celdas: [klen varint][key][hijo u64]
                 cell.key = cota superior exclusiva de su hijo
overflow (0x03): [type][flags][len u16][next u64][bytes…]
```

Invariantes verificados al decodificar: claves no vacías y estrictamente crecientes dentro del
nodo. v1 decodifica nodos completos (sin array de punteros de celda in-page; reservado como
optimización futura). `delete` no rebalancea nodos infrallenos — solo elimina nodos vacíos;
la compactación de `vacuum` (M9) reequilibra al reescribir.

## Espacios de claves

### Árbol de datos (`data_root` — ramifica con la rama)

```
[0x00, 0x01, nombre_tabla UTF-8]            → {table_id u32, schema}     catálogo
[0x00, 0x02, table_id BE]                   → next_rowid u64             contador rowid
[0x01, table_id u32 BE, rowid u64 BE*]      → registro (fila)
[0x02, …]                                     reservado: índices secundarios
```
\* rowid `i64` con el bit de signo invertido → orden memcomparable correcto para negativos.

El esquema vive en el árbol de datos **a propósito**: una migración en una rama cambia el
esquema solo en esa rama.

### Árbol meta (`meta_root` — global, lineal, nunca ramifica)

```
[0x01, nombre_rama]                → {head_version u64, head_page u64}     refs
[0x02, version BE]                 → {commit_page u64, ts_ms u64, branch}  índice histórico
[0x03, ts_ms BE, version BE]       → ∅                                     AS OF TIMESTAMP
```

## Registro (fila)

```
[ncols varint][tag u8 × ncols][payloads en orden]

tags:  0 NULL · 1 FALSE · 2 TRUE · 3 INTEGER (varint zigzag)
       4 REAL (f64 LE, 8 B) · 5 TEXT (varint len + UTF-8) · 6 BLOB (varint len + bytes)
```

Columnas ausentes al final = NULL (deja la puerta abierta a `ALTER TABLE ADD COLUMN` sin
reescribir filas).

## Protocolo de commit y recuperación

Escritura (escritor único):

```
1. páginas nuevas (CoW) → append            2. fdatasync
3. página de commit → append                4. fdatasync     ← punto de durabilidad
5. meta slot alternado → write in-place         (lazy; fsync periódico)
```

Apertura / recuperación:

```
1. Validar cabecera (magic, versión de formato, flags).
2. Leer meta A y B → candidato = válido con mayor version. Ambos corruptos → escaneo completo.
3. Validar la página de commit candidata (tag + magic + chain opcional).
4. Escanear hacia delante desde n_pages: cada página de commit válida posterior avanza el head.
5. Cola rota (páginas sin commit válido que las cubra) → ignorada. Eso ES el «replay del WAL»:
   no hay redo ni undo, solo descartar lo no confirmado.
```

## Vacuum (compactación)

`vacuum(retention)` reescribe en un archivo temporal las versiones vivas según la política
(`KeepAll | KeepVersions(n) | KeepSince(ts)`), escribe un commit checkpoint que encadena con la
historia truncada, hace `fsync` y `rename` atómico sobre el original. Es también el mecanismo de
**rotación de clave** (se reescribe todo con clave nueva).
