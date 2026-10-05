# Upstream source and local changes

This directory vendors the Rust crate `wuff` version `0.2.9` from
[crates.io](https://crates.io/crates/wuff/0.2.9). Its upstream repository is
[`nicoburns/wuff`](https://github.com/nicoburns/wuff), pinned to commit
`78e67f52713c0a2c36bd083fea18ba029565c9ec` (the commit recorded by the crate's
`.cargo_vcs_info.json`). The downloaded `.crate` archive SHA-256 is
`200a7b806377b65f1ff9a985874332f15357c87e5dd7855f35f06dade95b8b3d`.

The upstream decoder is MIT licensed; see [LICENSE](LICENSE). Five small font
fixtures are copied from the pinned web-platform-tests checkout at revision
`74ca910926d76710943f2a8798817102f69d6e40`, from
`css/WOFF2/support/`. They remain under the WPT contributors' 3-Clause BSD
license in [LICENSE-WPT.md](LICENSE-WPT.md). The fixture test source names and
SHA-256 digests are:

| Fixture | WPT purpose | SHA-256 |
| --- | --- | --- |
| `valid-001.woff2` | Valid WOFF2 font | `92cd7b916e02cb77a9b21fce3b89f588ba060eda31784c693093438a1a9a8a69` |
| `header-totalsfntsize-001.woff2` | Advisory `totalSfntSize` is too small | `b3b5b7acca6badafeea93966e1ec4c8450e0efeac6d05a30a9033c443fa22001` |
| `header-totalsfntsize-002.woff2` | Advisory `totalSfntSize` is too large | `d89e2c52a5bfe224f4c2cb92ee5759b4b082e419d0fb9dafd8328462956dde93` |
| `tabledata-glyf-origlength-003.woff2` | Transformed `glyf` length differs from `origLength` | `4c8908acaeaa4e3599e5bffa0eb2a78ed99c638b7a885214763a315f55b9f6ba` |
| `blocks-overlap-002.woff2` | Invalid private-data overlap must be rejected | `69ca13a59f7aa8ef10703b0af9cf9521b225072e978e67186e7d750fbe75ffcd` |

The upstream source and `Cargo.toml` are otherwise preserved. Local decoder
changes are deliberately limited to resource bounds and the integration seam:

- `src/limits.rs` centralizes checked, fallible vector capacities, reservations,
  padded lengths, and append limits.
- `decompress_woff2_with_custom_brotli_limited` accepts a caller-provided output
  ceiling and checks it through table-directory parsing, reconstruction, glyph
  and horizontal-metrics scratch, and final SFNT writes. The original API still
  delegates with the upstream 128 MiB default.
- Header `totalSfntSize` and transformed table `origLength` values are not used
  as allocation bounds. The format's decoded table stream and actual
  reconstruction lengths are checked separately.

- `decompress_woff1_with_custom_z` verifies each table's directory checksum
  (`head` with its checkSumAdjustment zeroed) and fails on a mismatch.

The patched upstream files are `src/lib.rs`, `src/decompress_woff1.rs`, `src/decompress_woff2.rs`,
`src/woff/headers.rs`, `src/woff/glyf_decoder.rs`, `src/woff/hmtx_decoder.rs`,
and the `pub(crate)` test callback visibility in `src/brotli.rs`. The tests add
only `src/decompress_woff2.rs` unit cases and the listed WPT fixtures.

Lumen consumes this as an optional local path dependency from
`lumen-common`'s `compress` feature. It supplies the zlib (WOFF1) and Brotli (WOFF2)
callbacks from the existing bounded shared decoders; no generated decoder copy or resolver
override is used.
