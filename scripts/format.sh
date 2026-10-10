#!/usr/bin/env bash
# Auto-format code and apply clippy's machine-applicable fixes. The Grok agent runs this
# after each change, before testing, so trivial lint errors never cost an attempt.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --all
cargo clippy --workspace --all-targets --fix --allow-dirty --allow-no-vcs --quiet -- -D warnings >/dev/null 2>&1 || true
cargo fmt --all
