#!/usr/bin/env bash
#
# Clone the tc39/test262 conformance suite into ./test262 (gitignored), the JS-language analogue of
# the ./wpt checkout used by scripts/run-wpt.sh. Idempotent: if ./test262 already exists it just
# reports and exits. Set TEST262_REF to pin a specific tag/commit (default: a shallow main clone).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="$ROOT/test262"
REPO="https://github.com/tc39/test262.git"
REF="${TEST262_REF:-}"

if [ -d "$DEST/test" ] && [ -d "$DEST/harness" ]; then
  echo "test262 already present at $DEST"
  exit 0
fi

echo "Cloning test262 into $DEST ..."
# Byte-exact checkout: tests such as Function.prototype.toString line-terminator normalisation and
# `import ... with { type: "bytes" }` compare against the files' exact bytes, which a Windows
# `core.autocrlf=true` checkout would rewrite to CRLF.
if [ -n "$REF" ]; then
  git clone -c core.autocrlf=false --depth 1 --branch "$REF" "$REPO" "$DEST"
else
  git clone -c core.autocrlf=false --depth 1 "$REPO" "$DEST"
fi
echo "Done. $(find "$DEST/test" -name '*.js' | wc -l | tr -d ' ') test files."
