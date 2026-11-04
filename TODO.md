# TODO

## Pass the LeakSanitizer suppressions in `fuzz.yml`

The fuzz targets `json_parse`, `regexp_compile` and `wasm_decode`, and the `asan` job, fail on
LeakSanitizer false positives. Property slots NaN-box string, symbol and BigInt handles, so
LeakSanitizer can't see them (see `fuzz/lsan.supp`). The suppressions are in `fuzz/lsan.supp`,
but the workflow doesn't pass them yet. This session's token can't push changes to
`.github/workflows/`, so a maintainer has to apply this change:

```diff
--- a/.github/workflows/fuzz.yml
+++ b/.github/workflows/fuzz.yml
@@
 env:
   CARGO_TERM_COLOR: always
+  # NaN-boxed string/symbol/BigInt handles are invisible to LeakSanitizer; see fuzz/lsan.supp.
+  LSAN_OPTIONS: suppressions=${{ github.workspace }}/fuzz/lsan.supp:print_suppressions=0
```

Verified locally with this setting:
- `json_parse`, `js_parse`, `regexp_compile` and `wasm_decode` fuzz for 240 s each with no
  findings.
- The ASan job's `cargo test -Zbuild-std --target x86_64-unknown-linux-gnu -p lumen-web wasm`
  step passes.

Once the change is in, remove this entry.
