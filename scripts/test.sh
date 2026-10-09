#!/usr/bin/env bash
# The single test entry point used by CI and by the Grok agent.
# Ends by launching the real app in a virtual display: it must open its window,
# render the UI, call into Rust, and find the adb and scrcpy that ship inside it.
set -euo pipefail
cd "$(dirname "$0")/.."
CURRENT="setup"
step() { CURRENT="$*"; echo; echo "== $*"; }
trap 'echo "::error title=Test failed::Step \"$CURRENT\" failed"' ERR

step "fetch built-in tools"
python3 scripts/fetch_tools.py linux-x86_64

step "format"
cargo fmt --all -- --check

step "lint"
cargo clippy --workspace --all-targets -- -D warnings

step "unit tests"
set +e
test_out=$(cargo test --workspace 2>&1)
test_code=$?
set -e
echo "$test_out"
if [ $test_code -ne 0 ]; then
  # Surface failing tests and their panic messages as annotations.
  echo "$test_out" | grep -E "^test .* FAILED$|panicked at|left:|right:|^thread '" | head -20 | while IFS= read -r l; do
    echo "::error title=Unit test failed::$l"
  done
  exit 1
fi

step "build app"
cargo build -p seam-desktop

step "launch app (self-test)"
set +e
out=$(WEBKIT_DISABLE_DMABUF_RENDERER=1 WEBKIT_DISABLE_COMPOSITING_MODE=1 \
  timeout 120 xvfb-run -a target/debug/seam-desktop --self-test 2>&1)
code=$?
set -e
echo "$out"
summary=$(echo "$out" | grep "SELF-TEST" | tail -1)
if [ $code -ne 0 ]; then
  echo "::error title=Self-test failed::exit $code: ${summary:-$(echo "$out" | tail -3 | tr '\n' ' ')}"
  exit 1
fi
for tool in adb scrcpy; do
  if ! echo "$summary" | grep -q "$tool found (built in)"; then
    echo "::error title=Self-test failed::the app did not use its built-in $tool: $summary"
    exit 1
  fi
done
if ! echo "$summary" | grep -q "link listening on"; then
  echo "::error title=Self-test failed::the phone link did not start: $summary"
  exit 1
fi
echo "self-test passed: $summary"
