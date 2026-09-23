#!/usr/bin/env bash
#
# Run Node.js's own test suite (test/parallel + test/sequential) against lumen-cli and print a
# per-module score. Exits 1 if a test listed in crates/node-compat-runner/passing.txt fails.
#
# Usage:
#   scripts/run-node-compat.sh                         # everything
#   scripts/run-node-compat.sh test-buffer test-path-  # filters: substrings of the test path
#   scripts/run-node-compat.sh --update                # rewrite passing.txt from the results
#   scripts/run-node-compat.sh --verbose parallel/test-events
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [ ! -d "$ROOT/node-test/test/common" ]; then
  "$ROOT/scripts/node-compat-clone.sh"
fi

cd "$ROOT"
cargo build --release -q -p lumen-cli -p node-compat-runner
cargo run --release -q -p node-compat-runner -- "$@"
