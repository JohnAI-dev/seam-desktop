#!/usr/bin/env bash
# The single test entry point used by CI and by the Grok agent.
# Ends by launching the real app in a virtual display: it must open its window,
# render the UI, call into Rust, and report back, or the test fails.
set -euo pipefail
cd "$(dirname "$0")/.."
step() { echo; echo "== $*"; }

step "format"
cargo fmt --all -- --check

step "lint"
cargo clippy --workspace --all-targets -- -D warnings

step "unit tests"
cargo test --workspace

step "build app"
cargo build -p seam-desktop

step "launch app (self-test)"
WEBKIT_DISABLE_DMABUF_RENDERER=1 WEBKIT_DISABLE_COMPOSITING_MODE=1 \
  timeout 120 xvfb-run -a target/debug/seam-desktop --self-test
