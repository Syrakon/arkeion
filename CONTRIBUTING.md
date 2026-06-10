# Contribuir a Arkeion

Gracias por el interés. Arkeion está en desarrollo activo **pre-1.0**: el formato en disco y la
API pueden cambiar entre versiones sin garantía de compatibilidad.

## Antes de abrir un PR

- Para cualquier cambio no trivial, abre primero un **issue** y cuéntanos qué quieres hacer —
  así evitamos trabajo descartado.
- El proyecto es Rust estable, sin dependencias en runtime y con `#![forbid(unsafe_code)]`.
  Eso no se negocia.

## Requisitos de un PR

1. `cargo test` — la suite completa en verde.
2. `cargo clippy --all-targets -- -D warnings` — limpio.
3. `cargo fmt` — aplicado.
4. Commits en formato [Conventional Commits](https://www.conventionalcommits.org/)
   (`feat:`, `fix:`, `perf:`, `docs:`, …).
5. Si el cambio toca rendimiento, acompáñalo de números: `benches/crud.rs` con la metodología
   descrita en el README (disco real, no tmpfs; mediana de varias corridas).

## Cómo está organizado

La arquitectura por capas está en [`docs/01-arquitectura.md`](docs/01-arquitectura.md) y cada
decisión relevante (D1–D8…) tiene su porqué escrito en
[`docs/05-decisiones.md`](docs/05-decisiones.md); si tu propuesta contradice una, argumenta
contra el porqué, no contra la decisión.

## Licencia de las contribuciones

Al enviar una contribución aceptas el
[Developer Certificate of Origin 1.1](https://developercertificate.org/) y que tu contribución
se licencie según la sección 6 de la [LICENSE](LICENSE).
