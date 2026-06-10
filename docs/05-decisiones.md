# 05 — Decisiones de diseño (ADRs)

Formato corto: **Decisión · Alternativa rechazada · Por qué**.

## D1 — B-tree copy-on-write append-only: el archivo es el WAL {#d1}

- **Decisión**: nunca sobrescribir páginas de datos; cada commit añade páginas nuevas + una página de commit. No existe WAL separado: el archivo entero es el log.
- **Rechazado**: B-tree in-place + WAL separado (modelo SQLite).
- **Por qué**: time-travel, branching, hash chain y recuperación salen del mismo mecanismo (ver [01-arquitectura](01-arquitectura.md)). Con in-place, cada diferenciador exigiría su propia maquinaria (snapshots, page versioning, log paralelo) y el WAL separado rompería la promesa single-file (el `-wal` de SQLite es un segundo archivo). Coste asumido: crecimiento del archivo → D10.

## D2 — Página de 4096 B con reserva criptográfica uniforme {#d2}

- **Decisión**: páginas de 4 KiB; los primeros 28 B de cada página en disco (nonce 12 + tag 16) se reservan siempre, haya cifrado o no.
- **Rechazado**: 8/16 KiB (más amplificación de escritura en CoW: cada commit reescribe el camino raíz→hoja); layout distinto por modo (dos formatos que testear, B-tree consciente del cifrado).
- **Por qué**: 4 KiB = página del SO y sector lógico común → torn writes mínimos; la reserva uniforme cuesta 0,7 % de espacio y compra un B-tree 100 % ignorante del cifrado y verificación de integridad en ambos modos.

## D3 — Dos raíces por commit: árbol de datos (ramifica) + árbol meta (global) {#d3}

- **Decisión**: `data_root` sigue la ascendencia de la rama; `meta_root` (refs + índice histórico) es lineal y global, cada commit lo hereda del commit global anterior.
- **Rechazado**: una sola raíz con las refs dentro del árbol de datos.
- **Por qué**: si las refs viven en el árbol que ramifica, cada rama acaba con una copia divergente y estale de las refs de las demás — inconsistencia estructural. Con la raíz meta global (posible porque el escritor es único, D9) las refs tienen una historia lineal única. Bonus: el índice histórico (versión → commit) se actualiza dentro del propio commit, O(log n) amortizado.

## D4 — Hash chain global en la página de commit, sobre el plaintext {#d4}

- **Decisión**: la cadena cubre **todos** los commits de todas las ramas en orden global; `content_hash` se calcula sobre los bodies en claro.
- **Rechazado**: una cadena por rama (deja huecos auditables al borrar ramas); hash del ciphertext (la rotación de clave rompería la cadena, y un mismo contenido daría hashes distintos).
- **Por qué**: una sola cadena lineal = una sola pregunta de auditoría («¿está intacta?»). Verificar requiere la clave: correcto — el auditor legítimo la tiene, y nadie sin clave puede siquiera enumerar qué se escribió. La firma asimétrica (Ed25519) queda como capa opcional v1.x sobre `chain_hash`, no en el núcleo: una dependencia menos y la propiedad clave (detección de manipulación) ya está.

## D5 — Branching solo por API; `AS OF` sí en SQL {#d5}

- **Decisión**: `create_branch`/`merge`/`diff` son métodos de `Database`; no hay `CREATE BRANCH` en SQL. `AS OF` sí es SQL.
- **Rechazado**: dialecto SQL propio para ramas.
- **Por qué**: `AS OF` es una operación de *consulta* — pertenece al lenguaje de consulta (precedente: SQL:2011 temporal). El branching es una operación de *gestión del ciclo de vida* que ejecutan herramientas y migradores, no queries de aplicación; meterlo en SQL inventa dialecto sin necesidad y complica parser y permisos. Menos superficie inventada = más auditable.

## D6 — AES-256-GCM por página con nonces de contador persistido {#d6}

- **Decisión**: cifrado a nivel de página; nonce de 96 bits = contador `u64` monótono (+ padding), persistido en cada commit (`nonce_counter`) y retomado tras crash desde el último commit válido + margen.
- **Rechazado**: nonces aleatorios por página (riesgo de colisión silenciosa con miles de millones de páginas y, peor, tras restaurar un backup); cifrado a nivel de archivo o de valor.
- **Por qué**: la reutilización de nonce en GCM es catastrófica (recuperación de la clave de autenticación), así que debe ser **estructuralmente imposible**, no improbable: un contador con persistencia transaccional lo garantiza y es auditable. La página es la unidad correcta: alinea cifrado, integridad y E/S.

## D7 — Clave cruda de 32 B; KDF fuera del motor (v1) {#d7}

- **Decisión**: `Options::key: Option<[u8; 32]>`. Derivar de passphrase (Argon2id) es responsabilidad del llamador. `kdf_salt` ya reservado en la cabecera.
- **Rechazado**: Argon2 embebido en v1.
- **Por qué**: el patrón objetivo (multi-tenant con keystore propio de la aplicación, una clave por tenant) no necesita KDF en el motor. Una dependencia menos hoy; la puerta queda abierta sin romper formato.

## D8 — Dependencias: 4 crates de runtime, tras un trait propio {#d8}

- **Decisión**: `aes-gcm`, `sha2` (RustCrypto), `getrandom`, `zeroize`. Dev-only: `tempfile`. Todo lo demás (parser, B-tree, encodings, varint, caché) a mano. La criptografía se consume solo a través de `trait CryptoProvider`.
- **Rechazado**: criptografía artesanal (inaceptable: side-channels, madurez); `ring` (C/asm, opaco a auditoría); `blake3` (SIMD/unsafe y menos familiaridad regulatoria que SHA-256/FIPS 180-4 — y, **medido**, ni siquiera más rápido al tamaño que importa: ver abajo).
- **BLAKE3 reevaluado (perf, 2026-06)**: el tag de integridad hashea una página de **~4 KB**, no flujos grandes. En este hardware (Ryzen 3700X, Zen 2 con `sha_ni`) `sha2 0.10` ya usa **SHA-NI por defecto** (cpufeatures detecta `sha` en runtime; sin flags): **2.04 GB/s** sobre 4 KB. BLAKE3 **con** SIMD/AVX2 da **1.46 GB/s** ahí (0.72×) y pure-Rust 1.44 GB/s (0.71×); solo adelanta a ≥64 KB (2.26×), tamaños que un tag por página nunca ve (su ventaja es el paralelismo de árbol sobre entradas grandes). Adoptarlo **regresaría** el hash ~28 %, costaría un bump de formato y rompería la línea pura-Rust de D8. **Conclusión: se mantiene SHA-256+SHA-NI.** El hash (~2 µs/página) tampoco es el cuello de botella (lo es el `decode` por fila en scans y el fsync/IO en durables).
- **Por qué**: supply chain de 4 crates puras de Rust, auditadas (aes-gcm: NCC Group 2020), con licencia MIT/Apache-2.0. El trait hace el backend **sustituible** sin tocar el motor — relevante para soberanía ([08-soberania](08-soberania.md)). Política: `cargo vet` + vendoring del árbol de dependencias.

## D9 — Escritor único serializado; lectores por snapshot {#d9}

- **Decisión**: un `Mutex<Writer>` por base de datos; lectores ilimitados sin locks contra páginas inmutables. Aislamiento: snapshot isolation para lecturas; las escrituras, al ser una a una, son serializables por construcción.
- **Rechazado**: MVCC multi-escritor (fuera de alcance declarado v1).
- **Por qué**: el requisito es «lecturas concurrentes sin lock global» — cumplido con cero complejidad. El perfil objetivo (multi-tenant, un archivo por tenant) reparte la escritura entre archivos de forma natural. Evolución no rupturista: *group commit*.

## D10 — Vacuum con commit checkpoint que preserva la cadena {#d10}

- **Decisión**: `vacuum(retention)` reescribe a un archivo temporal + `rename` atómico, **reusando la maquinaria de commit** (no copia páginas a mano): un **checkpoint** materializa el estado completo de la frontera `K` (`flags.checkpoint=1`, `prev_chain` sembrado en el génesis del archivo nuevo, versión numerada desde K) y luego se **replayan** los deltas K+1..head con `btree::diff` (O(cambios)). `commit::verify` siembra desde el checkpoint; `recover` lo adopta como arranque de cadena.
- **Publicación atómica e in-vivo (M9)**: tras el `rename`, el `Store` intercambia el par `(pager, head)` bajo un único `Mutex` (`DbState`). Las lecturas nuevas ven el archivo compactado; los snapshots ya en vuelo conservan su `Arc<Pager>` (el inodo viejo, ya sin nombre) y siguen válidos hasta soltarse. No hay que reabrir el handle.
- **Nonce con la misma clave (D6)**: al conservar la clave, el archivo nuevo **continúa** el contador de nonce del viejo en vez de reiniciarlo a 0 — reiniciar reutilizaría pares (clave, nonce) ya usados (catastrófico en GCM). La rotación de clave (`vacuum_rekey`) sí parte de una clave nueva.
- **Linealización (limitación v1, honesta)**: el replay reproduce con fidelidad el estado de **datos** de cada versión retenida (su `AS OF` sigue exacto), pero conserva solo la ref `main` (→ head) y un `parent_version` lineal. Para no cambiar en silencio lo que ve `main` (el head global podría ser de otra rama), `vacuum` **se niega** si existe alguna rama distinta de `main` (`InvalidInput`): fusiónalas o bórralas antes.
- **Rechazado**: GC in-place de páginas muertas (reintroduce escritura in-place y rompe D1); retención infinita obligatoria (el disco no es infinito); reescribir las páginas a mano (duplica la lógica de CoW del b-tree).
- **Por qué**: la tensión historia-infinita ↔ disco-finito se resuelve con política explícita del operador. La cadena sigue verificable de punta a punta y sirve además como rotación de clave (D6) y desfragmentación.

## D11 — Rowid `i64` como clave primaria física {#d11}

- **Decisión**: toda fila se almacena bajo `(table_id, rowid)`; `INTEGER PRIMARY KEY` es alias del rowid; contador `next_rowid` como entrada del árbol de datos (ramifica con la rama, transaccional gratis).
- **Rechazado**: claves primarias arbitrarias como clave física (necesita encoding memcomparable general ya en v1); UUIDs (16 B por referencia, sin orden de inserción).
- **Por qué**: modelo probado (SQLite), point-lookup O(log n) sin índices, y el merge 3-way de ramas tiene identidad de fila natural: `(table_id, rowid)`.

## D12 — La versión es la autoridad; el timestamp, informativo {#d12}

- **Decisión**: el orden lo define `version` (u64 monótono). `AS OF TIMESTAMP` resuelve a la mayor versión con ts ≤ t.
- **Rechazado**: el reloj de pared como autoridad.
- **Por qué**: los relojes retroceden (NTP, VMs); una BD auditable no puede tener una historia cuya ordenación dependa de ellos. Mismo principio que Git: el commit manda, la fecha acompaña.

## D13 — Sin índices secundarios ni optimizador en v1 {#d13}

- **Decisión**: full scan + filtro, point lookup por rowid. Espacio de claves `0x02` reservado para índices.
- **Por qué**: corrección antes que velocidad; el perfil objetivo son archivos de decenas de MB, no TB. Los índices llegan en v1.1 sobre un formato que ya los contempla — sin migración.
