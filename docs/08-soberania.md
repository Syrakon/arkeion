# 08 — Nota de posicionamiento: soberanía

Por qué Arkeion puede defenderse como **«europeo de verdad»** ante sector público y regulado,
y dónde están los límites honestos de esa afirmación.

## La afirmación defendible

La soberanía que importa — y la que se evalúa en una licitación o una auditoría — opera en tres
planos, y Arkeion los controla los tres:

| Plano | Situación |
|---|---|
| **El motor** | Escrito desde cero en Rust. Ni una línea, ni el formato de archivo, ni el modelo de journal derivan de SQLite ni de ningún motor estadounidense. El formato está especificado públicamente en [02-formato-archivo](02-formato-archivo.md): un tercero puede implementar un lector independiente. |
| **La entidad** | Propiedad y gobernanza bajo holding europeo (**Syrakon**). Roadmap, licencia y decisiones de seguridad se toman bajo jurisdicción UE. Sin CLA hacia entidad extranjera. |
| **Los datos** | Single-file bajo control físico del operador, cifrado AES-256-GCM, claves del operador. No hay telemetría, no hay cloud obligatorio, no hay «llama a casa». La residencia UE es una propiedad del despliegue que el diseño nunca compromete. |

## Por qué no un fork (la pregunta que siempre cae)

Un fork de SQLite/libSQL habría sido más rápido. Se descartó porque:

1. **Herencia técnica = herencia de gobernanza.** Un fork hereda formato, decisiones y deuda de
   un proyecto cuyo centro de gravedad (SQLite Consortium, Turso) es estadounidense. La
   narrativa «europeo» se reduce a «mantenido en Europa» — indefendible ante escrutinio serio.
2. **Los diferenciadores no caben en SQLite.** Time-travel, branching y hash chain requieren un
   storage CoW append-only ([05-decisiones, D1](05-decisiones.md#d1)). Sobre el formato de SQLite serían parches
   frágiles; aquí son consecuencias del diseño.
3. **Auditabilidad real.** ~15k líneas de Rust diseñadas para ser leídas valen más, ante un
   auditor, que 150k de C heredado con décadas de casos especiales.

## Cumplimiento por diseño (mapeo rápido)

| Requisito regulatorio | Mecanismo de Arkeion |
|---|---|
| GDPR — protección de datos en reposo | Cifrado por página; ni esquema ni datos en claro; claves del operador (D6, D7) |
| GDPR — derecho de supresión | `vacuum` con retención: la supresión es física, no lógica, y verificable (D10) |
| Integridad probatoria (sanidad, AAPP) | Hash chain global: alterar el pasado rompe la cadena de forma detectable (D4) |
| Trazabilidad de cambios | Time-travel nativo: el estado en cualquier versión/instante es consultable (M5) |
| Reversibilidad de migraciones | Branching con diff y merge: ensayar, revisar, fusionar o descartar (M8) |
| Auditoría de la cadena de suministro | 4 dependencias de runtime, puras Rust, vendorizadas, `cargo vet`; resto a mano (D8) |

## Los límites honestos (decirlos antes de que los encuentren)

- **Toolchain**: Rust y LLVM son proyectos globales con fundaciones en EE. UU. No existe
  compilador soberano; tampoco lo tiene nadie más en la industria. Lo que se controla es el
  *código fuente auditable* que entra al compilador.
- **Criptografía**: RustCrypto es una comunidad internacional descentralizada (no una empresa
  US), con auditoría pública. Aun así, el motor la consume tras `trait CryptoProvider`: si
  mañana existe un backend europeo validado (p. ej. con certificación BSI/ANSSI/CCN), se
  enchufa **sin tocar el formato ni el motor** (D8).
- **«Made in Europe»** significa aquí: diseño, implementación, gobernanza, especificación del
  formato y custodia de datos bajo control europeo. No significa que cada transistor o cada
  línea de la cadena de herramientas lo sea — y decirlo así es exactamente lo que distingue una
  postura seria del *sovereignty-washing*.

## Alineamiento con la estrategia UE

- **EU Open Source Strategy**: licencia [TSAL-1.0](../LICENSE) → Apache 2.0 diferido (cada
  versión a los diez años), especificación de formato pública, sin lock-in: los datos siempre
  exportables.
- **Technological Sovereignty Package (jun. 2026)**: Arkeion encaja en la capa de
  infraestructura de datos *made in Europe, for Europe* — candidato natural a programas de
  financiación/adopción pública de OSS europeo.
