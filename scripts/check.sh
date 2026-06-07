#!/usr/bin/env bash
# CI local de Arkeion: formato, lints estrictos y tests (hito M0, docs/06-hitos.md).
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --quiet
echo "✓ check completo"
