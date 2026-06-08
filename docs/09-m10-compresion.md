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
