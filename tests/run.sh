#!/bin/sh
# Test runner: Rust agent tests, Lua unit specs, e2e scenarios.
# NVIM overrides which nvim to use (default: nvim on PATH).
set -e
cd "$(dirname "$0")/.."

NVIM="${NVIM:-nvim}"

echo "== cargo test =="
cargo test --workspace

echo "== build agent =="
cargo build -p rnvim-agent

echo "== lua unit tests =="
"$NVIM" --headless --clean --cmd "set rtp+=." -l tests/unit_spec.lua

for spec in tests/e2e/*_spec.lua; do
  echo "== e2e: $spec =="
  E2E_HOME=$(mktemp -d)
  HOME="$E2E_HOME" RNVIM_AGENT_BIN="$PWD/target/debug/rnvim-agent" \
    "$NVIM" --headless --clean --cmd "set rtp+=." -l "$spec"
  rm -rf "$E2E_HOME"
done

echo "all tests passed"
