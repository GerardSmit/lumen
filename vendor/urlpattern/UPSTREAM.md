# Upstream source and local changes

This directory vendors the Rust crate `urlpattern` version `0.6.0` from
[crates.io](https://crates.io/crates/urlpattern/0.6.0), the Deno authors' implementation of the
URLPattern standard, MIT licensed (see [LICENSE](LICENSE)). Upstream:
<https://github.com/denoland/rust-urlpattern>.

Lumen already has a WHATWG URL parser and an ECMAScript regular-expression engine
(`lumen-common`), and URLPattern custom groups are ECMAScript syntax, so the crate's own
dependencies are replaced instead of pulled in (`url`, `idna`, `regex`, `serde`, `icu_properties`).
The tokenizer, constructor-string parser, pattern parser, component compiler and matcher are
unchanged.

Local changes:

- `src/url.rs` (new): the part of `rust-url` the crate used (`Url::parse`, component getters, the
  setters the canonicalization callbacks drive, `quirks::*`) over `lumen_common::url`. Setting an
  opaque path percent-encodes with the C0 control set, as the spec's "canonicalize an opaque
  pathname" requires.
- `src/regexp.rs`: the `regex::Regex` implementation is replaced by `EcmaRegExp`, an ECMAScript
  `u` / `ui` expression run by `lumen_common::regex::js`. It compiles lazily unless forced, so
  components that match through the literal and single-capture fast paths never compile one.
  `RegExp::syntax` is gone: ECMAScript is the only syntax, and component expressions are always
  validated when they contain a custom group.
- `src/parser.rs`: `RegexSyntax` and the `serde` derives are removed. The regexp escape list
  includes `/` as in the spec, and a segment wildcard with no delimiter is the spec's `[^]+?`
  rather than `.+?`.
- `src/tokenizer.rs`: name code points use `lumen_common::regex::js::regex_ident_start` /
  `regex_ident_part` (ID_Start / ID_Continue plus `$`, `_`, ZWNJ, ZWJ) instead of `icu_properties`.
- `src/lib.rs`, `src/component.rs`: match results keep group order (`Vec<(String, Option<String>)>`
  instead of a `HashMap`), `UrlPatternOptions` has no `regex_syntax`, `UrlPattern` defaults to
  `EcmaRegExp`, and the test module (which needs `regex`, `serde_json` and the WPT data files) is
  removed. Lumen's tests live in `lumen-host` and `lumen-runtime`.
- `src/quirks.rs`: reduced to `parse_match_input`; the serde bridge types for Deno's JavaScript
  runtime are removed.
- `Cargo.toml`: depends only on `lumen-common`; benches and dev-dependencies are dropped.
