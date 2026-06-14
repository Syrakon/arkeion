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

## Arquitectura — open-core, **tres repos**

El límite open-core se traza a nivel de **repo** (la lib abierta queda pura; el
servidor copyleft no la ensucia):

| Repo | Contenido | Licencia |
|---|---|---|
| **`arkeion`** | la lib embebida + **`arkeion-proto`** (workspace) | MIT/Apache |
| **`arkeion-client`** | el SDK Rust (driver, en repo propio como en Postgres) | MIT/Apache |
| **`arkeion-server`** | el daemon `arkeiond` | **EUPL-1.2** |

- **`arkeion-proto`** (en `arkeion`) — el protocolo de cable, **a mano, sin serde**
  (reusa el `varint` y la codificación de `Value` del core). Framing
  `[u32 LE len][payload]`; `payload = [u8 tag][campos]`, tope 64 MiB. Mensajes:
  `Hello`/`Welcome`, `UseBranch`, `Execute`→`Affected`, `Query{AS OF}`→`Rows`,
  `Verify`→`Audit{…, chain_hash}`, `Error`. **Hecho** (slice 1).
- **`arkeion-client`** — `Client` síncrono: `use_branch`, `execute`, `query`,
  `query_as_of`, `verify`. **Hecho** (slice 2). Abierto = adopción.
- **`arkeion-server`** — `arkeiond` **thread-por-conexión, sin tokio** (casa con el
  escritor único y mantiene la supply-chain mínima). **Hecho** (slice 2). EUPL para
  monetizar el cloud sin tocar la apertura del repo principal.

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

## Slicing (cada slice: compila, suite verde)

1. **`arkeion-proto`** — framing + mensajes + codec, round-trip testeado. **Hecho.**
2. **Servidor + cliente + e2e sobre socket real** — `arkeiond` (thread por conexión)
   + `Client`; CRUD, **aislamiento de rama**, **`AS OF`** y **`verify`** por el cable.
   **Hecho** (incluye `verify`, que estaba previsto aquí).
3. **`diff`/`merge` por la red** — los RPCs de branching que rematan la semántica
   git en la conexión. `Client::diff`/`merge` espejan `Database::diff`/`merge`;
   conflictos → `Error`. e2e: rama, `diff` (fila Added), `merge`, comprobar fusión.
   **Hecho.**
4. **Transacciones multi-sentencia** (con timeout del escritor).
5. **TLS + auth + permisos por rama.**
6. *(Opcional)* capa de compatibilidad **pgwire** para CRUD genérico.
