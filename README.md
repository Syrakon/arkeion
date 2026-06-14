# Arkeion

> Del griego ἀρχεῖον (*arkheîon*): la casa de los arcontes de Atenas, donde residían los
> registros oficiales de la ciudad — la raíz directa de *archivum* → **archivo**.
> El lugar donde viven los registros con autoridad. — [arkeion.tech](https://arkeion.tech)

**Arkeion** es un motor de base de datos **embebido, auditable y versionado**, escrito en Rust puro.
La analogía guía: *como si SQLite y Git tuvieran un hijo… y hubiera nacido en Europa*.

Infraestructura de datos **soberana europea**: diseñada, escrita y gobernada en Europa (holding
**Syrakon**), sin fork ni herencia del formato de SQLite, con cadena de suministro mínima y
auditable (4 dependencias de runtime, todas pure-Rust salvo las primitivas FIPS).

## Características

| | |
|---|---|
| **Modelo** | Relacional, subconjunto de SQL (`JOIN`, `GROUP BY`/`HAVING`, agregados, `ALTER TABLE`) |
| **Índices** | Secundarios B-tree: `CREATE [UNIQUE] INDEX`; el planner los usa para igualdad, rangos y multi-columna |
| **Empaquetado** | Un único archivo por tenant — backup = copiar el archivo |
| **Storage** | B-tree *copy-on-write* append-only: el archivo **es** el WAL; nodos con array de punteros (búsqueda binaria) |
| **Carga masiva** | `bulk_insert`: el lote entero en una transacción, sin executor por fila, índices en bloque — 2.5M filas/s |
| **Lecturas** | SELECT simple en **streaming** (decode perezoso: solo las columnas proyectadas, directo de página) |
| **Durabilidad** | ACID, recuperación por escaneo de cola, un solo `fsync` por commit |
| **Time-travel** | `SELECT … AS OF <versión/timestamp>`; `history()` / `diff_versions()` / `changes()` |
| **Branching** | Ramas de datos con *diff* y *merge* 3-way, modelo conceptual Git |
| **Auditoría** | Cada commit encadenado por hash SHA-256 — alterar el pasado es detectable; `verify()` + anclas |
| **Cifrado** | AES-256-GCM por página (opcional); la PII nunca toca el disco en claro |
| **Compresión** | LZSS pure-Rust por página (opcional); ≤ SQLite en datos comprimibles |
| **Robustez** | Reed-Solomon por página (opcional): corrige bit-rot, no solo lo detecta; `scrub()` |
| **Mantenimiento** | `vacuum` con retención y rotación de clave; rename atómico |
| **Concurrencia** | Lecturas concurrentes sin lock global (snapshots inmutables) |
| **Seguridad** | `#![forbid(unsafe_code)]` |

## Uso

```rust
use arkeion::{Database, Options, Value, params};

let db = Database::open("tenant.arkeion", Options::default())?;
let conn = db.connect()?;

conn.execute("CREATE TABLE facturas (id INTEGER PRIMARY KEY, total REAL, estado TEXT)", &[])?;
conn.execute(
    "INSERT INTO facturas (total, estado) VALUES (?1, ?2)",
    &params![120.0, "borrador"],
)?;
let v1 = conn.version();

// Carga masiva: el lote entero en UNA transacción (1 fsync), sin executor por fila.
conn.bulk_insert(
    "facturas",
    (1..=1_000).map(|i| [Value::Null, Value::Real(i as f64), Value::Text("emitida".into())]),
)?;

// Las lecturas son snapshots inmutables; un SELECT simple se sirve en streaming.
for row in conn.query("SELECT id, total FROM facturas LIMIT 10", &[])? {
    let row = row?;
    println!("{}: {}", row.get::<i64>("id")?, row.get::<f64>("total")?);
}

// Time-travel: la consulta ve el pasado tal y como fue (y `verify()` lo demuestra).
let mut antes = conn.query(&format!("SELECT COUNT(*) FROM facturas AS OF VERSION {v1}"), &[])?;
assert_eq!(antes.next().unwrap()?.get::<i64>(0)?, 1); // solo la factura previa al lote
```

## Estado

**Motor funcional — hitos M0 a M10 + índices secundarios + bulk-load y lecturas en streaming,
implementados y testeados** (259 tests, `clippy -D warnings`, `#![forbid(unsafe_code)]`). **Pre-1.0**:
el formato puede cambiar y no hay release de producción todavía. La versión de
[crates.io](https://crates.io/crates/arkeion) arranca en **0.10.x** porque refleja los hitos
completados (M0–M10); `1.0.0` significará una sola cosa: **formato en disco congelado**. La especificación completa vive en
[`docs/`](docs/):

| Doc | Contenido |
|---|---|
| [01-arquitectura](docs/01-arquitectura.md) | Capas, módulos y la decisión central CoW |
| [02-formato-archivo](docs/02-formato-archivo.md) | Layout binario: cabecera, páginas, commits |
| [03-api](docs/03-api.md) | API pública de Rust |
| [04-sql](docs/04-sql.md) | Subconjunto SQL v1 y extensión `AS OF` |
| [05-decisiones](docs/05-decisiones.md) | Decisiones de diseño justificadas (ADRs) |
| [06-hitos](docs/06-hitos.md) | Plan incremental M0 → M10 |
| [07-riesgos](docs/07-riesgos.md) | Riesgos técnicos y puntos calientes del borrow checker |
| [08-soberania](docs/08-soberania.md) | Posicionamiento: por qué es europeo de verdad y no un fork |
| [09-m10-compresion](docs/09-m10-compresion.md) | Compresión de página + estabilidad de datos (ECC) |
| [10-indices-secundarios](docs/10-indices-secundarios.md) | Índices secundarios: codificación memcomparable, plan, formato de nodo v3 |

## Benchmarks

**Máquina**: AMD Ryzen 7 3700X (8c/16t), 32 GiB, **ext4 sobre SSD SATA**, Arch Linux (kernel 7.0.11),
`rustc` 1.95.0, SQLite 3.50.2 (bundled vía rusqlite). Monohilo.

**Metodología** (léela antes de citar un número): **mediana de N** repeticiones (lecturas 5, durables 3
con DB fresca por repetición); **ambos motores calentados**; **ambos con sentencias preparadas** — el
lexado/parseo queda excluido en los dos lados, la diferencia es de ejecución (llamada nativa de Rust +
búsqueda binaria in-page vs el VM de bytecode de SQLite; y arkeion ni siquiera cachea el plan, lo
re-deriva en cada lookup, así que esa asimetría juega *en su contra*). **fsync a disco real**
(`ARKEION_BENCH_DIR` en el SSD; el tempdir por defecto puede ser `tmpfs`/RAM y entonces los fsync no
tocan disco). Durabilidad: arkeion **1** `fdatasync`/commit; SQLite `synchronous=FULL` + journal de
rollback = **2** fsync/commit.

```sh
ARKEION_BENCH_DIR=/ruta/en/disco/real cargo bench --features bench-sqlite   # CRUD vs SQLite
cargo run --release --example dbsize --features bench-sqlite                 # footprint
```

**Honestidad ante todo**: en disco real, arkeion **gana las escrituras durables** (~2×, su caso de uso
central), **gana los point-lookups** (por PK y por índice secundario) mientras el working set quepa en
la caché de páginas (64 MB por defecto), y en **inserción por lotes** está a la par del SQLite plano por
SQL (0.9–1.0×) y lo **supera con la bulk-load API** (`bulk_insert`, ~2.5M filas/s estables) y **con
garantías equivalentes** (~2.2×); el *full scan* sigue siendo de SQLite, pero el streaming con decode
perezoso lo dejó en ~0.5× (antes 0.26×): lo que queda es el paso por celda del b-tree CoW y el `Vec`
por fila de la API, no materializar el resultado (eso ya no pasa).
*Lección aprendida: una corrida previa en `tmpfs` (RAM) escondía la mayor fortaleza de arkeion — con el
fsync gratis SQLite ganaba las escrituras durables; en disco real se invierte.*

### CRUD monohilo — 50k filas, disco real (operaciones/segundo, mediana)

| operación | arkeion | SQLite | ratio |
|---|--:|--:|:--:|
| **insert 1 fila/commit (durable)** | **656** | 271 | **2.42×** |
| insert lote (1 commit) | 1.99M | 1.94M | 1.02× |
| **insert lote (bulk API)** | **2.59M** | 2.21M | **1.17×** |
| **select por PK** | **567.9k** | 181.5k | **3.13×** |
| **update por PK (durable)** | **572** | 278 | **2.06×** |
| full scan (filas/s) | 8.07M | 16.94M | 0.48× |
| **delete por PK (durable)** | **657** | 263 | **2.50×** |
| **select por índice 2º** | **358.7k** | 181.6k | **1.98×** |

> `ratio = arkeion / SQLite` (> 1 ⇒ arkeion más rápido). El *insert por lotes por SQL* hace ~1.8–2.0M
> filas/s (antes 1.1M — perf fase 2: dup-check de PK explícita sin descenso, `Arc` en la caché de
> esquema, codificación sin clones ni `Vec` por fila), **a la par** del SQLite plano. La **bulk-load
> API** (`Connection::bulk_insert`: una transacción, sin executor por fila, índices en bloque) hace
> **2.5–2.6M estables** y ganó en todas las corridas observadas (1.17–2.05×; el lote de SQLite plano
> es ruidoso, 1.2–2.2M). El número **estable** es la comparación justa (mismas garantías): arkeion
> **gana ~2.2×** (abajo). El *full scan* sirve en **streaming con decode perezoso** (8M filas/s,
> antes 4.5M): solo decodifica las columnas proyectadas, directo de la página, sin materializar el
> resultado.

**Las escrituras durables son la victoria robusta**: en disco real el fsync domina y el commit
append-only de arkeion (1 `fdatasync`) bate a los 2 fsync de SQLite (FULL + journal). No depende del
tamaño del dataset (lo limita el disco, no la caché) y es lo que paga **cada transacción confirmada** —
justo lo que un motor auditado hace constantemente.

### Cómo escala — la caché de páginas

Los lookups dependen de que el working set quepa en la **caché de páginas** (CLOCK, **64 MB** por
defecto): en cada fallo de caché, arkeion **re-verifica el tag de integridad** de la página, mientras
que SQLite no, así que cuando la tabla supera la caché arkeion paga ese coste y la ventaja se erosiona.
Antes la caché eran 16 MB y a 1M filas (~40 MB) el índice secundario llegaba a **invertirse** (0.57×,
arkeion más lento). Con la caché a 64 MB el working set vuelve a caber y los lookups se mantienen
**casi independientes del tamaño**:

| a disco real | 50k filas | 1M filas (~40 MB) |
|---|--:|--:|
| select por PK | 3.13× | 2.86× |
| select por índice 2º | 1.98× | 1.58× |
| insert durable | 2.42× | ~2.1× (independiente del tamaño) |

Más allá de la caché la ventaja vuelve a estrecharse (es inherente a re-verificar páginas frías); el
arreglo es dimensionar la caché al working set con `Options::cache_bytes` (un primitivo de integridad
más rápido NO lo es: BLAKE3 se midió y se rechazó — SHA-256 con SHA-NI ya hace ~2 GB/s y BLAKE3 es
*más lento* sobre páginas de 4 KiB; D8 en [docs/05](docs/05-decisiones.md)). Las escrituras durables
ganan a cualquier tamaño. (Las cifras también dependen de la config: `ARKEION_BENCH_SQLITE_WAL=1`
pone SQLite en WAL —1 sync— y mueve los ratios durables y de PK.)

### Comparación justa — mismas garantías

A SQLite se le da historia completa (tabla `t_log`) + cadena de hash por escritura: **versionado +
tamper-evidence como arkeion**, en una transacción (1 fsync). El foso inatacable:

| insert (50k, disco real) | arkeion | SQLite + auditoría | ratio |
|---|--:|--:|:--:|
| **durable (1/commit)** | **656** | 261 | **2.51×** |
| **lote (1 commit)** | **1.99M** | 906.1k | **2.19×** |

> Con garantías equivalentes, arkeion **gana las dos** — durables (2.51×) **y** lotes (2.19×) — *y* con
> `AS OF`, `verify()` y branching de regalo. (Esta emulación de SQLite ni reproduce el `content_hash`
> por commit ni un snapshot consultable, así que los ratios son **cota inferior** del sobrecoste real de
> igualar a arkeion.)

### Footprint en disco — 100k filas `(id PK, a INTEGER, b TEXT)`

| | tamaño |
|---|--:|
| arkeion — insert (1 commit) | 3.5 MB |
| arkeion — insert **comprimido** (opt-in) | 0.8 MB |
| arkeion — +1 update de todo (historia CoW retenida) | 7.0 MB |
| arkeion — tras `vacuum KeepLast(1)` | 3.5 MB |
| SQLite — insert (1 commit) | 2.8 MB |
| SQLite — +1 update de todo (in-place, sin historia) | 2.8 MB |

**Honestidad**: la fila de 0.8 MB compara el modo **comprimido (opcional, off por defecto)** de arkeion
contra SQLite **sin** comprimir, y sobre un dataset deliberadamente muy comprimible (la columna TEXT es
una constante). **Motor-a-motor en modo por defecto: 3.5 MB (arkeion) vs 2.8 MB (SQLite)** — arkeion
~1.25× **mayor**. La **clave de fila es de longitud variable** (v4: `[0x01][enc_oint(table_id)][enc_oint(rowid)]`,
~6 B típicos en vez de los 13 fijos de antes — entero order-preserving que mantiene el orden del b-tree);
el resto del gap es el byte de flags y la longitud de clave de la celda genérica (un único b-tree para
tablas/índices/catálogo) más el registro auto-descriptivo. El array de punteros v3 (2 B/celda) y la
reserva cripto por página (28 B) son marginales; las hojas ya van casi llenas (split sesgado a la derecha
en inserción secuencial, como SQLite). La compresión opcional usa **Densa** (LZSS + un codificador de
rango adaptativo, pure-Rust, sin deps); SQLite **también** admite compresión (sqlite-zstd, ZIPVFS/CEROD),
tampoco por defecto. Lo que arkeion sí conserva y SQLite no:
**historia CoW** (coste explícito, recuperable con `vacuum`), `verify()` y `AS OF`, manteniendo la
compresión y, opcional, corrección Reed-Solomon.

## Licencia

Doble licencia, a tu elección — la convención del ecosistema Rust:

- [Licencia MIT](LICENSE-MIT)
- [Licencia Apache 2.0](LICENSE-APACHE)

Salvo indicación explícita en contra, toda contribución enviada para su inclusión queda bajo
esa misma doble licencia, sin términos ni condiciones adicionales.

## Contribuir

Issues y PRs bienvenidos — lee [CONTRIBUTING.md](CONTRIBUTING.md) (suite en verde, clippy limpio,
Conventional Commits, y números de bench si tocas rendimiento).
