#!/usr/bin/env bash
# Builds the runtime flavour of lumen-wasm and generates the JS glue.
#   ./build.sh            web target  -> example/pkg       (the browser page)
#   ./build.sh node       nodejs target -> example/pkg-node (smoke-node.mjs)
# Needs the wasm32-unknown-unknown target and a wasm-bindgen CLI matching the pinned
# `wasm-bindgen` crate version (0.2.127). Run from anywhere; the cargo wrapper in the monorepo
# root is used when present.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
lumen="$(cd "$here/../.." && pwd)"
profile="${PROFILE:-fast}"
mode="${1:-web}"

if [ -f "$lumen/../../scripts/cargo-bounded.py" ]; then
  root="$(cd "$lumen/../.." && pwd)"
  (cd "$root" && python3 scripts/cargo-bounded.py build --profile "$profile" \
    --manifest-path external/lumen/Cargo.toml -p lumen-wasm --features runtime \
    --target wasm32-unknown-unknown)
  target_dir="$root/target"
else
  (cd "$lumen" && cargo build --profile "$profile" -p lumen-wasm --features runtime \
    --target wasm32-unknown-unknown)
  target_dir="$lumen/target"
fi

wasm="$target_dir/wasm32-unknown-unknown/$profile/lumen_wasm.wasm"
if [ "$mode" = node ]; then
  wasm-bindgen --target nodejs --out-dir "$here/example/pkg-node" "$wasm"
else
  wasm-bindgen --target web --out-dir "$here/example/pkg" "$wasm"
fi
