# Native URLPattern

`URLPattern` is a native class (`lumen_host::url_pattern`, installed as a lazy global by
`lumen-web`). `urlpattern.js`, a 200-line approximation that only understood `:name`, `*`,
`(regex)` and `{...}` in a single pass, is gone; the class now follows the URLPattern standard
(tokenizer, constructor-string parser, pattern parser, per-component canonicalization, matcher).

## Reuse decision

Per CLAUDE.md, a maintained crate was preferred over a new implementation. The candidate is
[`urlpattern`](https://crates.io/crates/urlpattern) 0.6.0 (Deno authors, MIT, pure Rust, passes
the WPT URLPattern data in Deno). Used as published it would not have kept the spec behaviour or
the "one implementation" rule:

- Its default regular-expression backend is the `regex` crate, whose dialect is not ECMAScript
  (no lookaround or backreferences, different `\d`/`\w`/`\b`, Unicode classes, case folding and
  escape rules). URLPattern custom groups (`(\d+)`, `(?:a|b)`, `(?!x)`) are ECMAScript syntax, so
  patterns that work in a browser or in Lumen's `RegExp` would be rejected or match differently.
  The crate does have a `RegExp` trait (Deno plugs V8 in through it) and a `RegexSyntax::EcmaScript`
  mode, but `regex` stays a hard dependency.
- It canonicalizes through `rust-url`, a second WHATWG URL parser (plus `idna`,
  `percent-encoding`, `form_urlencoded`, ICU data) that would disagree with the native `URL`
  class on edge cases, against "language-neutral algorithms live once" in `lumen-common`.
- `serde` derives, `icu_properties` and a `HashMap` for group results (spec order is lost) come
  with it. None of `url`, `idna`, `regex` or `icu_properties` was in `Cargo.lock`.

Lumen already has both halves: `lumen_common::regex::js` is the ECMAScript engine behind `RegExp`
(language-neutral, with `u`/`i` flags), and `lumen_common::url` is the WHATWG parser behind `URL`.
So the crate is vendored in `vendor/urlpattern` and patched (the way `wuff` and
`rusty_h264-decoder` are), with its backends replaced rather than adding a second parser and
regex dialect. `vendor/urlpattern/UPSTREAM.md` lists every change; the algorithms are untouched.

- `EcmaRegExp` implements the crate's `RegExp` trait on `lumen_common::regex::js` with the flags
  the spec asks for (`u`, plus `i` for `ignoreCase`). Compilation is lazy unless a component has a
  custom group, which is always validated at construction. Components matched by the literal and
  single-capture fast paths (`/books/:id`, `*`, plain text) never compile a regular expression.
- `url.rs` in the crate is a thin shim over `lumen_common::url`, so canonicalizing a pattern
  (`http://EXAMPLE.com` -> `example.com`, percent-encoding, IPv6 hosts, default ports) is the
  same code that parses a `URL`.
- The tokenizer's name rules use `lumen_common::regex::js::regex_ident_start` / `regex_ident_part`
  (the same ID_Start / ID_Continue tables as `(?<name>...)`).

## API and behaviour

`new URLPattern(input, baseURL, options)` and `new URLPattern(input, options)` follow the two WebIDL
overloads: a second argument that is `undefined`, `null` or an object is the options unless a third
argument is passed. `input` is a string (constructor-string syntax, needing a base unless it has a
protocol) or a `URLPatternInit`; `options.ignoreCase` is read with `ToBoolean`. Dictionaries are
read in WebIDL's lexicographic member order, so getters on init objects run in the spec order.

`test` and `exec` take `(input = {}, baseURL)`. A string input resolves against `baseURL` (a parse
failure gives `null`/`false`); an init object together with `baseURL` throws `TypeError`; an init
whose canonicalization fails gives `null`. `exec` returns `{ inputs, protocol, ..., hash }` with
`inputs` the arguments as passed (an init is copied into a plain object) and every component
`{ input, groups }`. `groups` has own data properties in pattern order, `undefined` for a group
that did not take part. The component getters, `hasRegExpGroups`, `@@toStringTag` and
enumerability come from the `webidl` hint.

Every invalid pattern, base URL or argument combination throws `TypeError` (`Failed to construct
'URLPattern': ...`). A match that exceeds the regular-expression backtracking limit reports no
match instead of throwing.

## Behaviour changes from the JS version

- `*` and `:name` no longer treat the whole pattern as one regex: pathname segments stop at `/`,
  hostname segments at `.`, modifiers (`?`, `+`, `*`), `{...}` groups with prefixes/suffixes,
  `\` escapes and regexp groups follow the spec.
- Patterns are canonicalized (`protocol` loses its `:`, hostnames are lowercased and IDNA-encoded,
  a default port is dropped, pathnames are percent-encoded and dot segments resolved), and the
  component getters return the pattern string the spec generates instead of the raw input.
- A string input with a base URL inherits protocol, hostname and port from the base as the spec
  says (the JS version copied protocol and hostname only).
- Invalid patterns throw `TypeError`; the JS version produced a broken `RegExp` or an unrelated
  `SyntaxError`.
- `exec` result `groups` for unnamed groups are keyed `"0"`, `"1"`, ... and unmatched groups are
  `undefined` rather than `""`.

## Known limits

- The crate's literal matcher compares with `str::to_lowercase` for `ignoreCase`, not the regexp
  engine's case folding, so the few characters whose lowercase and case-fold differ can compare
  differently on that fast path.
- The Bitnest kernel does not install `URLPattern` (it never bundled `urlpattern.js`); it can add
  `lumen_host::url_pattern::bindings::Module` to its lazy globals when a program needs it.
