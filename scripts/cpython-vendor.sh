#!/usr/bin/env bash
#
# Copy the modules listed in crates/lumen-py/lib/MODULES.txt from the ./cpython checkout
# (scripts/cpython-fetch.sh) into crates/lumen-py/lib, unmodified, and refresh LICENSE and
# VENDORED.md. Files are overwritten in place; nothing is ever deleted.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="$ROOT/cpython"
DEST="$ROOT/crates/lumen-py/lib"
LIST="$DEST/MODULES.txt"
GIT=/usr/bin/git

[ -d "$SRC/Lib" ] || { echo "run scripts/cpython-fetch.sh first" >&2; exit 1; }

TAG="$($GIT -C "$SRC" describe --tags --exact-match HEAD)"
COMMIT="$($GIT -C "$SRC" rev-parse HEAD)"
DATE="$($GIT -C "$SRC" log -1 --format=%cs HEAD)"

FILES=()
while IFS= read -r line; do
  line="${line%%#*}"
  line="$(echo "$line" | tr -d '[:space:]')"
  if [ -n "$line" ]; then FILES+=("$line"); fi
done < "$LIST"

for f in "${FILES[@]}"; do
  [ -f "$SRC/Lib/$f" ] || { echo "missing in CPython: Lib/$f" >&2; exit 1; }
  mkdir -p "$DEST/$(dirname "$f")"
  cp "$SRC/Lib/$f" "$DEST/$f"
done
cp "$SRC/LICENSE" "$DEST/LICENSE"

{
  echo "# Vendored CPython standard library"
  echo
  echo "Unmodified copies of files from CPython's \`Lib/\`, kept in CPython's package layout."
  echo "Regenerate with \`scripts/cpython-fetch.sh && scripts/cpython-vendor.sh\`; the list lives in"
  echo "\`MODULES.txt\`. Never edit these files; fix the engine instead."
  echo
  echo "- Tag: \`$TAG\`"
  echo "- Commit: \`$COMMIT\`"
  echo "- Commit date: $DATE"
  echo "- License: \`LICENSE\` (PSF)"
  echo
  echo "## Files"
  echo
  for f in "${FILES[@]}"; do echo "- \`$f\`"; done
} > "$DEST/VENDORED.md"

echo "vendored ${#FILES[@]} files from $TAG ($COMMIT)"
