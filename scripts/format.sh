#!/usr/bin/env bash
# Auto-format code. The Grok agent runs this after each change, before testing.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --all
