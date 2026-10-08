# parse-srcset

[![All Contributors](https://img.shields.io/badge/all_contributors-1-orange.svg?style=flat-square)](#contributors-)

[![Crates.io](https://img.shields.io/crates/v/parse-srcset.svg)](https://crates.io/crates/parse-srcset)
[![Documentation](https://docs.rs/parse-srcset/badge.svg)](https://docs.rs/parse-srcset)
[![CI](https://github.com/trananhtung/parse-srcset/actions/workflows/ci.yml/badge.svg)](https://github.com/trananhtung/parse-srcset/actions/workflows/ci.yml)
[![License](https://img.shields.io/crates/l/parse-srcset.svg)](#license)

**Parse an HTML `srcset` attribute** into its image candidates — each a URL plus an
optional density (`x`), width (`w`), or height (`h`) descriptor. A faithful Rust
port of the [`parse-srcset`](https://www.npmjs.com/package/parse-srcset) npm package,
which implements the
[WHATWG algorithm](https://html.spec.whatwg.org/multipage/images.html#parsing-a-srcset-attribute).
Zero dependencies and `#![no_std]`.

```rust
use parse_srcset::parse_srcset;

let candidates = parse_srcset("small.jpg 480w, large.jpg 800w, fallback.jpg");
assert_eq!(candidates[0].url, "small.jpg");
assert_eq!(candidates[0].width, Some(480));
assert_eq!(candidates[2].width, None); // bare fallback URL

let dpr = parse_srcset("img.png 1x, img@2x.png 2x");
assert_eq!(dpr[1].density, Some(2.0));
```

## Why parse-srcset?

Responsive-image tooling — build steps, SSR, image CDNs, linters — needs to read
the candidates out of a `srcset` string. The grammar has corners (commas inside
data URLs, descriptor validation, the `w`/`x`/`h` rules) that the canonical JS
parser already nails; this ports it faithfully so Rust tooling matches the browser.

```toml
[dependencies]
parse-srcset = "0.1"
```

## API

| Item | Purpose |
| --- | --- |
| `parse_srcset(input)` | `Vec<ImageCandidate>` |
| `ImageCandidate { url, density, width, height }` | A URL and its optional `x`/`w`/`h` descriptor |

## Behavior

- Candidates are separated by commas; surrounding whitespace is ignored and empty
  candidates are dropped.
- A URL may contain commas (e.g. a `data:` URI); a trailing comma just ends the URL.
- Exactly one of density/width is allowed per candidate (plus an optional height);
  a candidate with conflicting or malformed descriptors is **skipped**, matching the
  reference implementation.
- A `0x` density and `0w`/`0h` are treated as the reference does (density `0` is
  dropped; `0w`/`0h` are errors).

## Contributors ✨

This project follows the [all-contributors](https://github.com/all-contributors/all-contributors) specification. Contributions of any kind are welcome — code, docs, bug reports, ideas, reviews! See the [emoji key](https://allcontributors.org/docs/en/emoji-key) for how each contribution is recognized, and open a PR or issue to get involved.

Thanks goes to these wonderful people:

<!-- ALL-CONTRIBUTORS-LIST:START - Do not remove or modify this section -->
<!-- prettier-ignore-start -->
<!-- markdownlint-disable -->
<table>
  <tbody>
    <tr>
      <td align="center" valign="top" width="14.28%"><a href="https://github.com/trananhtung"><img src="https://avatars.githubusercontent.com/u/30992229?v=4?s=100" width="100px;" alt="Tung Tran"/><br /><sub><b>Tung Tran</b></sub></a><br /><a href="https://github.com/trananhtung/parse-srcset/commits?author=trananhtung" title="Code">💻</a> <a href="#maintenance-trananhtung" title="Maintenance">🚧</a></td>
    </tr>
  </tbody>
</table>

<!-- markdownlint-restore -->
<!-- prettier-ignore-end -->

<!-- ALL-CONTRIBUTORS-LIST:END -->

## License

Licensed under either of [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at
your option.
