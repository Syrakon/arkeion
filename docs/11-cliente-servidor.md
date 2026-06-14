# M11 — Módulo de conexión: cliente-servidor nativo

Arkeion deja de ser solo librería embebida y gana un **servidor**. Pero no «otro
servidor SQL»: el diferenciador es que **la semántica git y de auditoría son
ciudadanos de primera en el protocolo** — rama por sesión, time-travel (`AS OF`),
`verify` criptográfico por la red, y (después) `diff`/`merge` como RPCs. Un cloud
**verificable**, no «confía en nosotros»: encaja con la soberanía (docs/08).

## Decisión: protocolo **nativo**, no pgwire

Se evaluó pgwire (ecosistema instantáneo) vs nativo. Se eligió **nativo primero**
porque lo que diferencia a arkeion —conectar a una rama, migrar, `diff`, `merge`,
`verify` por la conexión— **no cabe en pgwire** sin violentarlo (variables de
sesión, sintaxis ad-hoc). Una capa de compatibilidad pgwire para CRUD genérico
queda como posible añadido posterior. Lo que vende un servidor arkeion es
justo lo que pgwire no sabe expresar.

## Arquitectura

Workspace, para no contaminar el core (D8, 4 deps) y dejar sitio al límite
**open-core** (la lib sigue MIT/Apache; el servidor podrá llevar otra licencia):

- **`arkeion`** (raíz) — la lib embebida, intacta.
- **`arkeion-proto`** — el protocolo de cable, **a mano, sin serde** (reusa el
  `varint` y la codificación de `Value` del core). Framing `[u32 LE len][payload]`;
  `payload = [u8 tag][campos]`. Mensajes: `Hello`/`Welcome`, `UseBranch`,
  `Execute`→`Affected`, `Query{AS OF}`→`Rows`, `Verify`→`Audit{…, chain_hash}`,
  `Error`. Acota el frame a 64 MiB antes de asignar (nunca confiar en el otro
  extremo). **Hecho** (slice 1).
- **`arkeion-server`** — el binario `arkeiond` + lógica, **thread-por-conexión**
  (sin tokio: casa con el motor síncrono y mantiene la supply-chain mínima).
- **`arkeion-client`** — cliente Rust nativo.

## Modelo de sesión

Una conexión TCP = una sesión = una **vista sobre una rama** (por defecto `main`,
cambiable con `UseBranch`). El **escritor único** del motor serializa las
escrituras de todos los clientes; las lecturas (snapshots, también `AS OF`) van en
paralelo y nunca bloquean. Consecuencias a vigilar:

- El techo de escritura del servidor = el del escritor único → el *group commit*
  (backlog) pasa a merecer la pena (amortizar fsync entre clientes).
- Una transacción abierta por red retiene el lock de escritura mientras dura →
  hacen falta timeouts / cancelación para que un cliente colgado no bloquee al
  resto. (Transacciones multi-sentencia llegan en un slice posterior.)

## Seguridad

- **Cifrado en tránsito (TLS)** complementa el cifrado en reposo del motor.
  (Slice posterior; el primer corte es texto claro en `localhost`.)
- El módulo de conexión es el hogar de **auth, roles y permisos por rama** (p. ej.
  `main` read-only para unos, una feature-branch escribible para otros).

## Slicing (cada slice: workspace compilando, suite verde)

1. **`arkeion-proto`** — framing + mensajes + codec, con round-trip testeado.
   **Hecho.**
2. **Servidor + cliente mínimos + test e2e** — `arkeiond` escucha, thread por
   conexión; el cliente conecta y hace `CREATE`/`INSERT`/`SELECT`, más una rama y
   un `AS OF` (los diferenciadores), sobre un socket real.
3. **`verify` + `diff`/`merge` por la red.**
4. **Transacciones multi-sentencia** (con timeout del escritor).
5. **TLS + auth + permisos por rama.**
6. *(Opcional)* capa de compatibilidad **pgwire** para CRUD genérico.
