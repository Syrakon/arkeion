# M10 — Compresión de página (diseño, sin compromiso de fecha)

Reducir el tamaño en disco hacia/por debajo de SQLite **manteniendo todas las
propiedades** del motor: acceso aleatorio O(log n) a *cualquier* versión,
time-travel, auditoría tamper-evident y cifrado en reposo.

## El stack de footprint (contexto)

| paso | qué | estado |
|---|---|---|
| 1 | Empaquetado de página (split de hoja append-optimizado) | ✅ hecho (78→40 B/fila, 2.8×→1.43× SQLite) |
| 2 | Prefix-compression de claves en la hoja | pendiente (~1.35×; cambio de formato de nodo) |
| **3** | **Compresión de página (este doc)** | **pendiente — el milestone** |
| 4 | Tier columnar para frío/histórico | pendiente (5-20×; otro modelo de almacenamiento) |

Suelo absoluto = **entropía** de los datos. Los pasos 3-4 acercan a ese suelo.

## El muro arquitectónico

`PageId::byte_offset() = page_id * 4096`: las páginas viven en **slots fijos de
4 KiB**. Comprimir una página a 1 KiB **no ahorra nada** si su slot sigue ocupando
4 KiB. La compresión real exige **páginas de tamaño variable**, y eso obliga a una
capa de indirección: un **directorio de páginas** que mapee `page_id → (offset,
longitud)`. Es el corazón del cambio (y por eso es un milestone, no un slice).

## Diseño propuesto

- **`page_id` pasa a ser lógico**; un **directorio de páginas** (versionado y
  append-only como el resto) lo traduce a `(offset, len)` físico.
- Páginas comprimidas de **tamaño variable** escritas a EOF.
- Orden de transformación: **comprimir → cifrar → sellar** (el tag cubre los bytes
  *finales almacenados*; los datos cifrados son incompresibles, así que la
  compresión va antes del AES-GCM).
- Compresión **tras un `trait`** (como `CryptoProvider`, D8): algoritmo
  sustituible, idealmente **pure-Rust/auditable**, y **opcional** (off por defecto
  para no engordar el core mínimo).
- **Bump de versión de formato.** DBs viejas siguen legibles; las nuevas comprimen.

Potencial medido (per-page, `examples/dbsize` + zlib): ~**4× con slots de 1 KiB**,
~7× cruda → llevaría **por debajo de SQLite** en datos comprimibles.

## ⛔ PRINCIPIO NO-NEGOCIABLE: estabilidad de los datos

Comprimir = quitar redundancia = **aumentar la fragilidad** (cerca de la entropía,
un bit corrupto puede reventar un bloque entero). Esto **no se diseña después**:
es requisito de entrada de M10. Reglas obligatorias:

1. **Detección, intacta.** El tag de integridad se calcula sobre los bytes
   *finales* (comprimidos+cifrados), así que cualquier corrupción se detecta
   **antes** de descomprimir → nunca se alimenta basura al descompresor → **jamás
   dato plausible-pero-mal**. Esto ya lo garantiza el orden comprimir→cifrar→sellar.
2. **Bloques independientes.** Comprimir **cada página por separado** (nunca un
   stream encadenado): un bit malo pierde **una página**, no todo lo posterior.
   Acota el radio del daño.
3. **Presupuesto de ECC por página.** Código corrector (Reed-Solomon/LDPC) sobre
   la página comprimida → poder **corregir** N bytes corruptos, no solo
   detectarlos. Se gasta una **fracción** del espacio ahorrado; el neto es *más
   pequeño que hoy Y más robusto que hoy*.
4. **Replicar lo crítico.** Directorio de páginas, raíces y cadena de commit con
   copias extra (como ya hay 2 meta slots): nunca se pierde la estructura.
5. **Scrubbing.** `verify()` periódico en background → pillar el bit-rot temprano,
   mientras el ECC aún puede arreglarlo.

**Activo de estabilidad que ya tiene Arkeion:** el CoW + historia es un *backup
integrado* — las versiones viejas viven en páginas distintas, así que una
corrupción en la versión actual no destruye el pasado (detectas la rotura con
`verify` y vuelves a la última versión buena). SQLite no tiene esto.

**Regla de oro:** *detección* es innegociable y ya está (tag + cadena + `verify`/
`verify_anchor`); *recuperación* es un **presupuesto** (ECC + réplicas + historia)
que el operador decide. M10 no se considera hecho si comprimido es menos estable
que el formato sin comprimir de hoy.

## Soberanía

El **tamaño no afecta la soberanía** (que va de *dónde* vive el dato, *quién* lo
controla y la *supply-chain*). Reducirlo incluso la refuerza (auto-alojarse en
hardware modesto/edge). Cuidados al implementar la compresión:
- Tras un `trait` sustituible; preferir **pure-Rust/auditable** (ojo: `zstd`/
  `brotli` son C de Meta/Google; `LZ4` es de origen francés pero también C).
  Mantener la supply-chain mínima de D8.
- **Nota CRIME:** comprimir-luego-cifrar filtra información gruesa por el tamaño de
  la página. Para un producto soberano/auditable: documentarlo y considerar la
  compresión **off por defecto cuando hay cifrado**, o un modo de relleno.

## Hecho cuando

- El archivo de un dataset comprimible queda **≤ SQLite** manteniendo `verify()` OK
  y `AS OF` a cualquier versión retenida.
- Un bit corrupto en una página comprimida se **detecta siempre** y, dentro del
  presupuesto de ECC, se **corrige**; fuera del presupuesto, falla limpio (nunca
  dato silenciosamente malo) y se puede volver a una versión íntegra.
- Compresión tras `trait`, opcional, con backend pure-Rust por defecto.

---

# Plan de implementación

## El muro concreto: la identidad de stride fijo

Hoy `offset_físico(page_id) = page_id · 4096` no es un detalle del pager: es una
identidad **acoplada en cuatro sitios** que hay que romper a la vez.

1. `pager.read_page(id)` / `write_reserved_page(id)` leen/escriben exactamente
   `PAGE_SIZE` bytes en `id.byte_offset()`.
2. **Recuperación** (`commit::recover`): salta al head por el meta slot y escanea
   la cola con **stride fijo** (página a página por offset).
3. **`verify`**: recomputa `content_hash` sobre el rango **físicamente contiguo**
   `[commit_page − (pages_written−1), commit_page)` — asume páginas de datos
   contiguas y en orden de id.
4. `n_pages = byte_len / PAGE_SIZE` y `head.n_pages = commit_page + 1`.

Tamaño variable rompe (1) y por tanto (2)–(4). La clave que el diseño de arriba no
aterriza es **cómo camina la recuperación sin stride fijo**.

## Lo que cae solo de los invariantes del motor

Dos garantías que Arkeion **ya** tiene hacen el cambio mucho menor de lo que
parece:

- **Páginas inmutables + `page_id` append-only** (R2): una entrada del directorio
  `page_id → (offset, len)` nunca cambia una vez escrita ⇒ el **directorio es
  append-only**, igual que el resto del archivo.
- **Las páginas de datos de un commit se escriben en orden de id, contiguas,
  seguidas de la página de commit** (`publish_commit`). Si se mantiene ese orden
  físico, el layout sigue siendo *en orden de id* (solo con stride variable):
  `verify` sigue válido (contiguas físicamente) y la recuperación puede **caminar
  el log** si cada registro es **auto-delimitado**.

## Layout físico v2

- Páginas estructurales en **slots fijos de 4 KiB** (sin cambios): header (0),
  meta A (1), meta B (2).
- Zona append = **log de registros de longitud variable**:

  ```text
  [u32 LE stored_len][payload sellado: stored_len bytes]
  payload = nonce(12) ‖ tag(16) ‖ ciphertext(stored_len − 28)
  ```

  El `ciphertext` cubre el body **en claro recortado** (Slice A: trim de ceros
  finales; Slice B: comprimido). El logical body sigue siendo `BODY_SIZE`; se
  rellena con ceros al leer, así que el round-trip y el `content_hash` son
  idénticos byte-a-byte.
- **Bump de `FORMAT_VERSION` a 2.** Las DBs v1 (slots fijos) se siguen abriendo por
  el camino antiguo; las v2 usan el directorio. La cabecera (página 0) lo dice.

### Detección, intacta (principio NO-NEGOCIABLE #1)

El `stored_len` corrupto **ya** lo atrapa el tag: el tag se calcula sobre los
bytes finales exactos, así que un `len` alterado (mayor o menor) hace leer un
ciphertext distinto → fallo de autenticación → `Corrupt`, **antes** de
descomprimir. AAD = `page_id` (como hoy); no hace falta más para la detección. El
sellado va sobre los bytes **finales** (comprimir → cifrar → sellar): nunca se
alimenta basura al descompresor.

## Directorio y recuperación (Slice A: barrido al abrir)

`page_id` es **lógico**; el pager mantiene un directorio en memoria
`Vec<PhysLoc>` indexado por `page_id` (`PhysLoc = { offset: u64, len: u32 }`). En
v1 se reconstruye al abrir con un **barrido secuencial bufferizado** de la zona
append: se leen los prefijos de longitud en orden de id, se asignan offsets
acumulados y, de paso, se localiza el **head** (el último registro de commit
autoconsistente y bien encadenado). La cola rota = primer registro que no enmarca
o no abre; el barrido para ahí (truncado lógico, como hoy).

Esto **unifica** `recover` y la construcción del directorio en un solo recorrido y
conserva las garantías de crash: las páginas de datos preceden a la de commit, así
que un commit a medio escribir nunca se adopta (el barrido para antes de su página
de commit), y uno totalmente durable sí (todos sus registros abren).

Coste de apertura: **O(páginas)** (barrido), peor que el O(1)+cola de hoy. Es la
contrapartida conocida de v1 y se ataca en el **Slice D** con un **directorio
persistido** (page-table radix o b-tree de `(offset,len)` apuntado por el head):
O(log n) para resolver y abrir, carga perezosa. No cambia el formato de *datos*,
solo añade un índice.

## Slicing (cada slice: crate compilando, suite verde, formato versionado)

- **A1 — Sellado de longitud variable en `crypto`.** `seal_bytes`/`open_bytes`
  (payload `nonce‖tag‖ciphertext` de longitud variable, AAD = `page_id`); reexpresar
  el `seal`/`open` de `PageBuf` sobre ellos → byte-idéntico. Sin cambio de formato;
  suite verde. *(Primer incremento, aislado y testeable.)*
- **A2 — Framing + directorio + pager.** `[u32 len][payload]` en la zona append,
  directorio en memoria, `read_page`/`write_reserved_page` sobre offsets con trim
  de ceros. Bump a v2.
- **A3 — Recuperación por barrido + `verify` sobre offsets.** `recover` como barrido
  que construye el directorio y halla el prefijo de commits válido; `content_hash`
  sobre offsets del directorio. Tests de crash/auditoría verdes.
- **B — Compresor tras `trait`, off por defecto.** `comprimir → cifrar → sellar`,
  bloque independiente por página, tag de método por página (nunca inflar), backend
  pure-Rust. El formato ya soporta longitud variable desde A.
- **C — Presupuesto de estabilidad.** ECC por página + réplicas de lo crítico +
  scrubbing en `verify` (el NO-NEGOCIABLE; varios sub-slices).
- **D (si escala mal) — Directorio persistido.** Apertura y resolución O(log n).

`vacuum` no necesita trabajo extra: reescribe replayando por la maquinaria de
commit (`publish_commit`), así que hereda el framing y el directorio en cuanto A
está hecho.

## Etapa de entropía (`Densa`): por qué de rango y no Huffman

El LZSS de Slice B deja los **literales a 1 byte crudo** y los códigos de match
con layout fijo; sus distribuciones siguen sesgadas, así que una segunda pasada
de entropía (como Deflate = LZ77 + Huffman) puede exprimirlas más. Pero **a 4 KiB
por página la cabecera del modelo es decisiva**: un Huffman estático debe
serializar ~256 longitudes de código (decenas de bytes) por página, y sobre una
salida LZSS de unos cientos de bytes eso **se come toda la ganancia** —medido:
gana en 0 de 400 páginas, incluso infla 0.1%—.

La solución es un coder **sin cabecera**: un **codificador de rango adaptativo**
(orden 0, Subbotin carryless). El modelo de frecuencias arranca uniforme y se
adapta símbolo a símbolo, así que no paga tabla por página. `Densa` aplica el coder
sobre la salida LZSS y, por página, elige el menor de **{crudo, LZSS,
LZSS+rango}** (nunca inflar). Resultados medidos:

- Filas con texto **variado** (nombres/emails/ids): **−8.8 %** frente a LZSS pelado,
  gana en 400/400 páginas.
- Dataset del footprint (TEXT constante): comprimido **1.0 → 0.8 MB** (los enteros
  y las claves variables dejan sesgo residual que el coder captura).
- Prosa muy repetitiva / binario aleatorio: neutro (LZSS ya es óptimo /
  incompresible) → cae a LZSS por «nunca inflar», nunca peor.

**Estabilidad:** la compresión **auto-verifica** en caliente (descomprime y
compara antes de adoptar la etapa de rango), así que un bug del coder jamás
corrompe una página —en el peor caso se descarta la entropía y queda el LZSS—.
Sigue siendo pure-Rust, sin dependencias (D8). El tag de método es **por página**
(`METHOD_DENSA`), así que el backend convive con los anteriores sin migrar nada.
