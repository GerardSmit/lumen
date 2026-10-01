#!/usr/bin/env bash
#
# Fetch the latest CPython 3.12.x release into ./cpython (gitignored) as a shallow, blobless,
# sparse clone containing only Lib/ (standard library and its test suite) and LICENSE.
# Idempotent: an existing checkout is fetched and moved to the newest tag in place.
# Set CPYTHON_TAG to pin a tag instead of the newest v3.12.*.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="$ROOT/cpython"
REPO="https://github.com/python/cpython"
GIT=/usr/bin/git

TAG="${CPYTHON_TAG:-}"
if [ -z "$TAG" ]; then
  TAG="$($GIT ls-remote --tags --refs "$REPO" 'v3.12.*' \
    | sed 's|.*refs/tags/||' \
    | grep -E '^v3\.12\.[0-9]+$' \
    | sort -t. -k3 -n \
    | tail -n 1)"
fi
[ -n "$TAG" ] || { echo "could not determine the latest v3.12 tag" >&2; exit 1; }

if [ ! -d "$DEST/.git" ]; then
  mkdir -p "$DEST"
  $GIT -C "$DEST" init -q
  $GIT -C "$DEST" remote add origin "$REPO"
  $GIT -C "$DEST" config core.autocrlf false
  $GIT -C "$DEST" config extensions.partialClone origin
fi
$GIT -C "$DEST" sparse-checkout init --no-cone
$GIT -C "$DEST" sparse-checkout set '/Lib/' '/LICENSE'
$GIT -C "$DEST" fetch -q --depth 1 --filter=blob:none origin "refs/tags/$TAG:refs/tags/$TAG"
$GIT -C "$DEST" checkout -q "$TAG"

echo "tag:    $TAG"
echo "commit: $($GIT -C "$DEST" rev-parse HEAD)"
