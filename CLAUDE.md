# Lumen project rules

Lumen hosts more than one language (the JS engine in `crates/lumen`, Python in `crates/lumen-py`)
on shared infrastructure. These rules apply to every session and every subagent; pass them on
verbatim when delegating.

## Reuse before writing

- Before implementing anything, check what Lumen already has (engine, `lumen-common`, `lumen-os`,
  `lumen-host`, `lumen-node`, `lumen-tls`, `lumen-web`, `lumen-runtime`). If it exists, use it.
- If the existing code is tied to one language (JS `Value`s, `Ctx`, JS strings), split it: move the
  language-neutral core into a shared crate and keep a thin adapter per language. Never copy it.
- Language-neutral algorithms go in `lumen-common` (keep it free of OS calls so it can become
  `no_std + alloc`); OS-facing code goes in `lumen-os`.
- Only write new code when nothing in Lumen covers it.

## Duplication review before every merge

Before merging any branch or commit (including merges from `main`), review the incoming changes:

1. Does this add code that duplicates something elsewhere in Lumen (another crate, the other
   language's engine, a shared crate)?
2. Should it be made generic and shared instead?

Report the findings and resolve them (or schedule a pass to resolve them) as part of the merge.

## Safety

- Never run `rm -rf`, `rm -r`, `rm -f`, `find -delete` or `find -exec rm` with a variable, glob or
  command substitution in the path — not in commands and not in scripts. Remove only single files
  by explicit literal path, or use `git rm`/`git mv` of literal paths. Scripts that refresh
  generated output overwrite files in place; they never delete directories.
- Run `python3` with `PYTHONDONTWRITEBYTECODE=1`, so no `__pycache__` directories are created.

## Python (`crates/lumen-py`)

- Target semantics: CPython 3.12. Vendored CPython modules live unmodified in
  `crates/lumen-py/lib/` (see `VENDORED.md`); fix the engine, never patch vendored files.
- Native modules that are C in CPython are written in Rust on top of Lumen's shared code.
- Correctness is checked against CPython: the corpus (`cargo test -p lumen-py --test corpus`),
  CPython's own test suite (`crates/cpython-test-runner`, checkout via `scripts/cpython-fetch.sh`),
  and differential runs against a local `python3`.

## Native bindings (`crates/lumen-bind`)

- Every native, in every language, is a typed Rust fn/struct declared once with `lumen_bind`
  attributes; each host (JS, Python) exposes every declaration and derives its own names, arity,
  `__text_signature__` / `length` and argument errors. Never hand-write argument parsing or
  per-language registration tables for new natives. Porting guide: `crates/lumen-bind/src/lib.rs`.
- `only(..)` / `skip(..)` / `rename(..)` are rare opt-outs; per-host extras go in `hint(py(..))`.

```rust
#[lumen_bind::module(name = "geometry")]
pub mod geometry {
    use super::*;

    /// `clamp(x, /, lo=0.0, *, hi)` in Python, `geometry.clamp(x, lo, hi)` in JS.
    #[op]
    pub fn clamp(x: f64, #[kw] #[default(0.0)] lo: f64, #[kwonly] hi: f64) -> NativeResult<f64> {
        if lo > hi { return Err(NativeError::value_error("lo > hi")); }
        Ok(x.max(lo).min(hi))
    }
}
```
