# Contributing to Arkeion

Thanks for your interest. Arkeion is under active **pre-1.0** development: the on-disk format and
the API may change between releases with no compatibility guarantee.

## Before opening a PR

- For any non-trivial change, open an **issue** first and tell us what you want to do — that way
  we avoid throwaway work.
- The project is stable Rust, with no runtime dependencies and `#![forbid(unsafe_code)]`.
  That is non-negotiable.

## PR requirements

1. `cargo test` — the full suite green.
2. `cargo clippy --all-targets -- -D warnings` — clean.
3. `cargo fmt` — applied.
4. Commits in [Conventional Commits](https://www.conventionalcommits.org/) format
   (`feat:`, `fix:`, `perf:`, `docs:`, …).
5. If the change touches performance, back it up with numbers: `benches/crud.rs` following the
   methodology described in the README (real disk, not tmpfs; median of several runs).

## How it is organized

The layered architecture lives in [`docs/01-arquitectura.md`](docs/01-arquitectura.md), and every
relevant decision (D1–D8…) has its rationale written down in
[`docs/05-decisiones.md`](docs/05-decisiones.md); if your proposal contradicts one, argue against
the rationale, not against the decision.

## License of contributions

Unless you explicitly state otherwise, any contribution you submit for inclusion in the project is
licensed under the dual [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE) license, as defined by
the Apache 2.0 license, without any additional terms or conditions.
