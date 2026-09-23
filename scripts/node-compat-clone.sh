#!/usr/bin/env bash
#
# Clone the test/ directory of nodejs/node into ./node-test (gitignored) for the Node compatibility
# runner. Idempotent: if ./node-test already exists it just reports and exits. The default ref is
# the Node version lumen reports as process.version, so the tests match the API level it claims;
# set NODE_TEST_REF to pin another tag.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="$ROOT/node-test"
REPO="https://github.com/nodejs/node.git"
REF="${NODE_TEST_REF:-v20.11.0}"

if [ -d "$DEST/test/common" ]; then
  echo "Node test suite already present at $DEST ($(git -C "$DEST" describe --tags 2>/dev/null || echo unknown ref))"
  exit 0
fi

echo "Cloning nodejs/node $REF (test/ only) into $DEST ..."
git clone --depth 1 --branch "$REF" --filter=blob:none --sparse "$REPO" "$DEST"
git -C "$DEST" sparse-checkout set test
echo "Done. $(find "$DEST/test/parallel" "$DEST/test/sequential" -name 'test-*.js' | wc -l | tr -d ' ') test files."
