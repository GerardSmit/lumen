//! Ahead-of-time compiled JavaScript: a versioned, sectioned blob an embedder links into its
//! binary in place of the JS source (see the `lumen-aot` crate's `include_js!`), loaded with
//! [`Engine::load_precompiled`](crate::Engine::load_precompiled).
//!
//! ## What is in a blob
//! Parser output plus bytecode. Each unit (a script, one ES module, or one CommonJS module of a
//! bundle) is its AST encoded by [`snapshot::encode_stripped`]: every function body is written
//! out in full (nothing is a lazy byte range into the source) and, by default, no function keeps
//! its source text, so the blob decodes against the *empty* source.
//! `Function.prototype.toString` of a precompiled function renders the spec's NativeFunction
//! form (`function f() { [native code] }`). String literals, template strings, regex literals,
//! identifiers and property names are of course still in it — they are program data, not
//! source text.
//!
//! ### Kept function text
//! Code that ships functions somewhere as text (`fn.toString()` sent to a browser — every
//! Puppeteer `page.evaluate` callback) needs that text. A unit compiled with *keep source*
//! carries a source section: the exact source slices of its outermost functions and classes,
//! concatenated (nested functions are sub-slices; text *between* top-level functions — module
//! statements, comments outside functions — is not kept), LZ-compressed. Its AST's function
//! ranges point into that text, so `toString` returns exactly what the source said. The kept
//! text is in the binary (compressed, but trivially recoverable): keep source only for the
//! modules that need it.
//!
//! Next to its AST, a unit normally carries a bytecode section: every function of the unit
//! run through the bytecode compiler at build time, each resulting chunk serialized (see
//! `bytecode::serialize` for the chunk format) and keyed by the function's *function index* —
//! the order the AST codec enters functions, which its decoder reproduces. Loading attaches
//! each chunk to its function: when the function tiers up (the same point a source-loaded
//! function would compile) the chunk is decoded instead of compiled — no bytecode compile at
//! run time — and a function the compiler refused is recorded too (it stays on the
//! tree-walker without a retry). Decoding is on demand because a large bundle calls few of its
//! functions at load (typescript.js: 20k chunks, ~40 tier-ups); `LUMEN_AOT_EAGER=1` decodes
//! every chunk at load instead, so even a function's first call runs on the VM. The AST is
//! still decoded eagerly in full — the VM needs the `Function` nodes (params, flags, inner
//! function templates) and the interpreter still consults the body (hoisting, the tree-walker
//! tier, bodies the compiler refused), so function bodies are not skipped when their bytecode
//! is present.
//!
//! ## Layout (all integers little-endian)
//! ```text
//! 0   magic          8   b"LUMENAOT"
//! 8   format         u32 FORMAT_VERSION — the container layout below
//! 12  flags          u32 reserved, 0
//! 16  ast_version    u32 the AST codec's version (snapshot::VERSION)
//! 20  section_count  u32
//! 24  layout_fp      u64 0 = no layout-dependent sections; else the fingerprint of the
//!                        encodings they use, which must equal the loader's
//!                        [`LAYOUT_FINGERPRINT`] (today: the bytecode codec — op table, operand
//!                        enums, chunk fields; native sections will fold in target, pointer
//!                        width and VM frame/value layout)
//! 32  lumen_version  16  the building lumen's CARGO_PKG_VERSION, NUL-padded
//! 48  section table  section_count x 24: kind u32, flags u32, offset u64, len u64
//!                        (offset is from the start of the blob)
//! ..  section payloads
//! ```
//! Section kinds: [`SEC_MANIFEST`] (exactly one — the unit list), [`SEC_AST`] (one per unit),
//! [`SEC_BYTECODE`] (at most one per unit), [`SEC_SOURCE`] (at most one per unit: kept function
//! text; section flag bit 0 = LZ-compressed, payload then `uncompressed_len` LEB128 + data).
//! Readers skip kinds they do not know, so later tiers (native code) are added as new section
//! kinds referenced from new manifest fields.
//!
//! The manifest: `unit_count` (LEB128), then per unit `kind` (u8: 0 script, 1 module,
//! 2 CommonJS), `key` (LEB128 length + UTF-8), `ast_section` (LEB128 index into the section
//! table), `bytecode_section` (LEB128 index, 0 = none), `source_section` (LEB128 index, 0 =
//! none), then its links: `link_count` (LEB128) x (`specifier` string, `target` LEB128 unit
//! index) — every import/require specifier the bundler resolved to another unit of the blob;
//! then `entry` (LEB128: unit index + 1, 0 = none).
//!
//! ## Modules
//! A module unit's key is `aot:/<path>` (a path relative to the bundle root, `/`-separated).
//! Loading a blob registers every module unit with the realm; a specifier imported from an
//! `aot:/` module that the bundler linked (relative, or a bare package specifier it resolved
//! through `node_modules`), a relative specifier naming a registered unit, or an `aot:/`
//! specifier anywhere resolves inside the bundle and decodes that unit's AST on first import —
//! no host loader, no filesystem. Anything else (`node:` builtins, packages that were not
//! bundled, attribute imports) goes to the host's module loader as usual, with the `aot:/` key
//! as the referrer.
//!
//! ## CommonJS
//! A CommonJS unit (key `aot:/<path>` too, in its own namespace) is the file's body wrapped
//! like Node's module wrapper: its AST is the one expression
//! `(function (exports, require, module, __filename, __dirname) { … })`. Loading a blob with
//! CommonJS units defines the non-enumerable global `__lumenAot`, which a CommonJS loader (the
//! runtime's `require`) uses: `resolve(specifier, parentKey)` → a unit key or `undefined`,
//! `kind(key)` → `"commonjs"` / `"module"` / `undefined`, `load(key)` → the wrapper function.
//! An ES module importing a CommonJS file is linked to a synthesized module unit with the same
//! key that re-exports `require(key)` (default export plus the statically detected names).

use std::collections::HashMap;

use crate::ast::{Expr, Stmt};
use crate::interpreter::Interp;
use crate::snapshot;
use crate::value::Value;

/// The container format version (header + section table + manifest). Bump on any change.
pub const FORMAT_VERSION: u32 = 3;
/// The AST codec version units are encoded with.
pub const AST_VERSION: u32 = snapshot::VERSION;

const MAGIC: &[u8; 8] = b"LUMENAOT";
const HEADER_LEN: usize = 48;
const SECTION_ENTRY_LEN: usize = 24;

/// The unit list (kind, key, AST section of every unit, and the entry).
pub const SEC_MANIFEST: u32 = 1;
/// One unit's source-free AST (a stripped snapshot).
pub const SEC_AST: u32 = 2;
/// One unit's precompiled bytecode chunks, keyed by function index.
pub const SEC_BYTECODE: u32 = 3;
// Reserved for later tiers: 4 = native code (guarded by `layout_fp`).
/// One unit's kept function text (see "Kept function text" above).
pub const SEC_SOURCE: u32 = 5;

/// Section flag: the payload is LZ-compressed.
const FLAG_LZ: u32 = 1;

/// The layout fingerprint a blob with bytecode must carry (see the header's `layout_fp`).
pub const LAYOUT_FINGERPRINT: u64 = crate::bytecode::serialize::FINGERPRINT;

pub use crate::bytecode::serialize::SectionStats;

/// The lumen version a blob must be built by (checked on load).
pub const LUMEN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The key prefix of a precompiled module.
pub const KEY_PREFIX: &str = "aot:/";

/// The parameter list of the CommonJS module wrapper (Node's).
pub const CJS_WRAPPER_HEAD: &str = "(function (exports, require, module, __filename, __dirname) {";

/// A precompiled blob linked into the binary, as `include_js!` expands to:
/// `static APP: lumen::Precompiled = lumen_aot::include_js!("app.js");`
#[derive(Clone, Copy)]
pub struct Precompiled {
    bytes: &'static [u8],
}

impl Precompiled {
    /// Wrap blob bytes (from `include_js!`, or `include_bytes!` of a blob a build script wrote
    /// with `lumen_aot::build`). Validated when loaded.
    pub const fn from_static(bytes: &'static [u8]) -> Precompiled {
        Precompiled { bytes }
    }

    pub fn as_bytes(&self) -> &'static [u8] {
        self.bytes
    }
}

impl std::fmt::Debug for Precompiled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Precompiled")
            .field("len", &self.bytes.len())
            .finish()
    }
}

/// What a unit is: a classic script, an ES module, or a CommonJS module (the body of a
/// `require`d file, run inside Node's module wrapper).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SourceKind {
    Script,
    Module,
    CommonJs,
}

// ---- building ---------------------------------------------------------------------------------

/// How [`CompiledUnit::compile_with_options`] compiles.
#[derive(Clone, Copy, Debug)]
pub struct CompileOptions {
    /// Precompile every function to bytecode (default on).
    pub bytecode: bool,
    /// Keep the source text of the unit's functions and classes for `toString` (default off;
    /// see "Kept function text" in the module docs).
    pub keep_source: bool,
}

impl Default for CompileOptions {
    fn default() -> Self {
        CompileOptions {
            bytecode: true,
            keep_source: false,
        }
    }
}

/// One unit compiled on its own, before it is named in a bundle (a module's AST does not
/// depend on its key, so a bundler can compile first and choose the bundle root afterwards).
pub struct CompiledUnit {
    kind: SourceKind,
    ast: Vec<u8>,
    bytecode: Option<Vec<u8>>,
    kept: Option<String>,
    stats: SectionStats,
    imports: Vec<String>,
    deps: snapshot::Deps,
}

impl CompiledUnit {
    /// Parse `src` eagerly (every early error surfaces now, at build time), encode it without
    /// its source text and precompile its functions to bytecode. `Err` is a `SyntaxError`
    /// message with its line.
    pub fn compile(src: &str, kind: SourceKind) -> Result<CompiledUnit, String> {
        CompiledUnit::compile_with(src, kind, true)
    }

    /// [`CompiledUnit::compile`], with or without the bytecode section (without it, every
    /// function compiles at run time as if the program had been loaded from source).
    pub fn compile_with(
        src: &str,
        kind: SourceKind,
        bytecode: bool,
    ) -> Result<CompiledUnit, String> {
        CompiledUnit::compile_with_options(
            src,
            kind,
            CompileOptions {
                bytecode,
                keep_source: false,
            },
        )
    }

    /// Compile with explicit [`CompileOptions`]. A [`SourceKind::CommonJs`] `src` is the
    /// file's text as Node would read it (a leading `#!` line is dropped); it is wrapped in
    /// [`CJS_WRAPPER_HEAD`] … `\n})` before parsing.
    pub fn compile_with_options(
        src: &str,
        kind: SourceKind,
        opts: CompileOptions,
    ) -> Result<CompiledUnit, String> {
        let fmt =
            |e: crate::parser::ParseError| format!("SyntaxError: {} (line {})", e.message, e.line);
        let wrapped;
        let text = match kind {
            SourceKind::CommonJs => {
                let body = if src.starts_with("#!") {
                    &src[src.find('\n').unwrap_or(src.len())..]
                } else {
                    src
                };
                wrapped = format!("{CJS_WRAPPER_HEAD}{body}\n}})");
                wrapped.as_str()
            }
            _ => src,
        };
        let body = crate::parser::with_eager_bodies(|| match kind {
            SourceKind::Script | SourceKind::CommonJs => crate::parser::parse_script(text, false),
            SourceKind::Module => crate::parser::parse_module(text),
        })
        .map_err(fmt)?;
        let mut imports = Vec::new();
        if kind == SourceKind::Module {
            for stmt in &body {
                let spec = match stmt {
                    // An attribute import (JSON/text/bytes) is data the host loader supplies.
                    Stmt::Import(d) if d.attr_type.is_none() => &d.source,
                    Stmt::ExportNamed {
                        source: Some(s), ..
                    }
                    | Stmt::ExportAll { source: s, .. } => s,
                    _ => continue,
                };
                if !imports.iter().any(|d: &String| **d == **spec) {
                    imports.push(spec.to_string());
                }
            }
        }
        // The CommonJS wrapper function's own text is the whole file: never keep it.
        let skip = if kind == SourceKind::CommonJs {
            cjs_wrapper_range(&body)
        } else {
            None
        };
        let unit = snapshot::encode_stripped_keep(&body, opts.keep_source.then_some((text, skip)));
        let (bytecode, stats) = if opts.bytecode {
            let (bc, stats) = crate::bytecode::serialize::encode_unit(&unit.funcs);
            (Some(bc), stats)
        } else {
            (None, SectionStats::default())
        };
        Ok(CompiledUnit {
            kind,
            ast: unit.ast,
            bytecode,
            kept: opts.keep_source.then_some(unit.kept),
            stats,
            imports,
            deps: unit.deps,
        })
    }

    /// What the bytecode precompile did (all zero when built without bytecode).
    pub fn bytecode_stats(&self) -> SectionStats {
        self.stats
    }

    /// The encoded bytecode section's size in bytes (0 without one).
    pub fn bytecode_len(&self) -> usize {
        self.bytecode.as_ref().map_or(0, Vec::len)
    }

    /// The kept function text's size in bytes, uncompressed (0 when not kept).
    pub fn kept_source_len(&self) -> usize {
        self.kept.as_ref().map_or(0, String::len)
    }

    /// The kept function text (`None` when the unit was compiled without keep source).
    pub fn kept_source(&self) -> Option<&str> {
        self.kept.as_deref()
    }

    pub fn kind(&self) -> SourceKind {
        self.kind
    }

    /// A module's static import / re-export specifiers, in source order, deduplicated
    /// (attribute imports excluded). Empty for a script.
    pub fn imports(&self) -> &[String] {
        &self.imports
    }

    /// The string-literal specifiers of the unit's dynamic `import("x")` calls (without import
    /// attributes), deduplicated.
    pub fn dynamic_imports(&self) -> &[String] {
        &self.deps.dynamic_imports
    }

    /// The string-literal specifiers of the unit's `require("x")` calls, deduplicated.
    pub fn requires(&self) -> &[String] {
        &self.deps.requires
    }

    /// The encoded AST's size in bytes (the bytecode section is [`CompiledUnit::bytecode_len`]).
    pub fn len(&self) -> usize {
        self.ast.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ast.is_empty()
    }
}

/// The source range of the CommonJS wrapper function (the unit's single expression).
fn cjs_wrapper_range(body: &[Stmt]) -> Option<(u32, u32)> {
    let Some(Stmt::Expr(mut e)) = body.first().cloned() else {
        return None;
    };
    while let Expr::Paren(inner) = e {
        e = *inner;
    }
    let Expr::Func(f) = e else { return None };
    match &f.source {
        crate::ast::FnSource::Range { start, end, .. } => Some((*start, *end)),
        _ => None,
    }
}

/// Builds a multi-unit blob: any number of scripts, ES modules and CommonJS modules plus an
/// optional entry module. This is what `include_js!` and `lumen_aot::build` drive; embedders
/// with their own bundling needs can use it directly (e.g. from a `build.rs`).
pub struct PrecompileBundle {
    units: Vec<BundleUnit>,
    entry: Option<usize>,
    no_bytecode: bool,
    compress: bool,
}

impl Default for PrecompileBundle {
    fn default() -> Self {
        PrecompileBundle {
            units: Vec::new(),
            entry: None,
            no_bytecode: false,
            compress: true,
        }
    }
}

struct BundleUnit {
    kind: SourceKind,
    key: String,
    ast: Vec<u8>,
    bytecode: Option<Vec<u8>>,
    kept: Option<String>,
    links: Vec<(String, usize)>,
}

impl PrecompileBundle {
    pub fn new() -> PrecompileBundle {
        PrecompileBundle::default()
    }

    /// Whether [`PrecompileBundle::add`] precompiles bytecode (default on). Units added with
    /// [`PrecompileBundle::add_compiled`] keep whatever they were compiled with.
    pub fn set_bytecode(&mut self, on: bool) {
        self.no_bytecode = !on;
    }

    /// Whether kept function text is LZ-compressed in the blob (default on).
    pub fn set_compress_source(&mut self, on: bool) {
        self.compress = on;
    }

    /// Compile and add one unit (see [`PrecompileBundle::add_compiled`]); returns the module's
    /// static import specifiers (relative ones are the caller's to resolve and add).
    pub fn add(&mut self, path: &str, src: &str, kind: SourceKind) -> Result<Vec<String>, String> {
        let unit = CompiledUnit::compile_with(src, kind, !self.no_bytecode)
            .map_err(|e| format!("{path}: {e}"))?;
        let imports = unit.imports.clone();
        self.add_compiled(path, unit)?;
        Ok(imports)
    }

    /// Add a compiled unit and return its index (for [`PrecompileBundle::link`]). `path` names
    /// it: for a module or CommonJS unit, its bundle-relative `/`-separated path (`lib/a.js`,
    /// keyed `aot:/lib/a.js`); for a script, a label. Scripts run in the order they are added.
    pub fn add_compiled(&mut self, path: &str, unit: CompiledUnit) -> Result<usize, String> {
        let key = match unit.kind {
            SourceKind::Module | SourceKind::CommonJs => module_key(path),
            SourceKind::Script => path.to_string(),
        };
        if self
            .units
            .iter()
            .any(|u| u.kind == unit.kind && u.key == key)
        {
            return Err(format!("{key}: added twice"));
        }
        self.units.push(BundleUnit {
            kind: unit.kind,
            key,
            ast: unit.ast,
            bytecode: unit.bytecode,
            kept: unit.kept,
            links: Vec::new(),
        });
        Ok(self.units.len() - 1)
    }

    /// Record that `specifier`, imported or required by unit `from`, is unit `to` — so it
    /// resolves inside the blob at run time whatever it looks like (a bare package name, a
    /// directory, a path without extension).
    pub fn link(&mut self, from: usize, specifier: &str, to: usize) -> Result<(), String> {
        if to >= self.units.len() {
            return Err(format!("link {specifier:?}: no unit {to}"));
        }
        let u = self
            .units
            .get_mut(from)
            .ok_or_else(|| format!("link {specifier:?}: no unit {from}"))?;
        if !u.links.iter().any(|(s, _)| s == specifier) {
            u.links.push((specifier.to_string(), to));
        }
        Ok(())
    }

    /// Whether a module with this bundle-relative path has been added.
    pub fn has_module(&self, path: &str) -> bool {
        let key = module_key(path);
        self.units
            .iter()
            .any(|u| u.kind == SourceKind::Module && u.key == key)
    }

    /// Make the (already added) module at `path` the entry `load_precompiled` evaluates.
    pub fn set_entry(&mut self, path: &str) -> Result<(), String> {
        let key = module_key(path);
        let i = self
            .units
            .iter()
            .position(|u| u.kind == SourceKind::Module && u.key == key)
            .ok_or_else(|| format!("entry {key} is not a module of the bundle"))?;
        self.entry = Some(i);
        Ok(())
    }

    /// Serialize the blob.
    pub fn finish(self) -> Vec<u8> {
        // Section order: the manifest, every unit's AST, then the bytecode sections, then the
        // kept-source sections.
        let n = self.units.len();
        let n_bc = self.units.iter().filter(|u| u.bytecode.is_some()).count();
        let mut next_bc = 1 + n;
        let mut next_src = 1 + n + n_bc;
        let sources: Vec<Option<(u32, Vec<u8>)>> = self
            .units
            .iter()
            .map(|u| {
                u.kept.as_ref().map(|k| {
                    if self.compress {
                        let mut out = Vec::new();
                        uv(&mut out, k.len() as u64);
                        out.extend_from_slice(&lz_compress(k.as_bytes()));
                        (FLAG_LZ, out)
                    } else {
                        (0, k.as_bytes().to_vec())
                    }
                })
            })
            .collect();
        let mut manifest = Vec::new();
        uv(&mut manifest, n as u64);
        for (i, u) in self.units.iter().enumerate() {
            manifest.push(match u.kind {
                SourceKind::Script => 0,
                SourceKind::Module => 1,
                SourceKind::CommonJs => 2,
            });
            uv(&mut manifest, u.key.len() as u64);
            manifest.extend_from_slice(u.key.as_bytes());
            uv(&mut manifest, i as u64 + 1); // section 0 is the manifest
            match &u.bytecode {
                Some(_) => {
                    uv(&mut manifest, next_bc as u64);
                    next_bc += 1;
                }
                None => uv(&mut manifest, 0),
            }
            match &sources[i] {
                Some(_) => {
                    uv(&mut manifest, next_src as u64);
                    next_src += 1;
                }
                None => uv(&mut manifest, 0),
            }
            uv(&mut manifest, u.links.len() as u64);
            for (spec, to) in &u.links {
                uv(&mut manifest, spec.len() as u64);
                manifest.extend_from_slice(spec.as_bytes());
                uv(&mut manifest, *to as u64);
            }
        }
        uv(&mut manifest, self.entry.map_or(0, |e| e as u64 + 1));

        let mut sections: Vec<(u32, u32, &[u8])> = vec![(SEC_MANIFEST, 0, &manifest)];
        for u in &self.units {
            sections.push((SEC_AST, 0, &u.ast));
        }
        for u in &self.units {
            if let Some(bc) = &u.bytecode {
                sections.push((SEC_BYTECODE, 0, bc));
            }
        }
        for (flags, data) in sources.iter().flatten() {
            sections.push((SEC_SOURCE, *flags, data));
        }
        let has_bytecode = sections.iter().any(|(k, _, _)| *k == SEC_BYTECODE);
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&AST_VERSION.to_le_bytes());
        out.extend_from_slice(&(sections.len() as u32).to_le_bytes());
        let layout_fp = if has_bytecode { LAYOUT_FINGERPRINT } else { 0 };
        out.extend_from_slice(&layout_fp.to_le_bytes());
        let mut ver = [0u8; 16];
        let v = env!("CARGO_PKG_VERSION").as_bytes();
        ver[..v.len().min(16)].copy_from_slice(&v[..v.len().min(16)]);
        out.extend_from_slice(&ver);
        let mut offset = (HEADER_LEN + sections.len() * SECTION_ENTRY_LEN) as u64;
        for (kind, flags, data) in &sections {
            out.extend_from_slice(&kind.to_le_bytes());
            out.extend_from_slice(&flags.to_le_bytes());
            out.extend_from_slice(&offset.to_le_bytes());
            out.extend_from_slice(&(data.len() as u64).to_le_bytes());
            offset += data.len() as u64;
        }
        for (_, _, data) in &sections {
            out.extend_from_slice(data);
        }
        out
    }
}

/// Precompile a single script or module (a module is keyed `aot:/main.js` and is the entry).
pub fn precompile(src: &str, kind: SourceKind) -> Result<Vec<u8>, String> {
    let mut b = PrecompileBundle::new();
    b.add("main.js", src, kind)?;
    if kind == SourceKind::Module {
        b.set_entry("main.js")?;
    }
    Ok(b.finish())
}

fn module_key(path: &str) -> String {
    let p = path.replace('\\', "/");
    format!(
        "{KEY_PREFIX}{}",
        p.trim_start_matches("./").trim_start_matches('/')
    )
}

fn uv(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

// ---- LZ compression of kept text --------------------------------------------------------------
//
// A small LZ77 (std-only): a sequence of `literal_len` (LEB128), the literals, then — unless the
// output is complete — `match_len - MIN_MATCH` (LEB128) and `offset` (LEB128, >= 1). Matches
// are found through hash chains over 4-byte windows. JS source compresses ~3-4x.

const MIN_MATCH: usize = 4;

fn lz_compress(input: &[u8]) -> Vec<u8> {
    const HASH_BITS: u32 = 16;
    const MAX_CHAIN: usize = 48;
    let n = input.len();
    let mut out = Vec::with_capacity(n / 3 + 16);
    let mut head = vec![u32::MAX; 1 << HASH_BITS];
    let mut prev = vec![u32::MAX; n];
    let hash = |i: usize| -> usize {
        let v = u32::from_le_bytes([input[i], input[i + 1], input[i + 2], input[i + 3]]);
        (v.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize
    };
    let insert = |i: usize, head: &mut Vec<u32>, prev: &mut Vec<u32>| {
        if i + MIN_MATCH <= n {
            let h = hash(i);
            prev[i] = head[h];
            head[h] = i as u32;
        }
    };
    let mut lit_start = 0;
    let mut i = 0;
    while i + MIN_MATCH <= n {
        let h = hash(i);
        let mut cand = head[h];
        let (mut best_len, mut best_off) = (0usize, 0usize);
        let mut chain = 0;
        while cand != u32::MAX && chain < MAX_CHAIN {
            let c = cand as usize;
            let max = n - i;
            let mut l = 0;
            while l < max && input[c + l] == input[i + l] {
                l += 1;
            }
            if l > best_len {
                best_len = l;
                best_off = i - c;
                if l >= 258 {
                    break;
                }
            }
            cand = prev[c];
            chain += 1;
        }
        if best_len >= MIN_MATCH {
            uv(&mut out, (i - lit_start) as u64);
            out.extend_from_slice(&input[lit_start..i]);
            uv(&mut out, (best_len - MIN_MATCH) as u64);
            uv(&mut out, best_off as u64);
            for j in i..i + best_len {
                insert(j, &mut head, &mut prev);
            }
            i += best_len;
            lit_start = i;
        } else {
            insert(i, &mut head, &mut prev);
            i += 1;
        }
    }
    uv(&mut out, (n - lit_start) as u64);
    out.extend_from_slice(&input[lit_start..]);
    out
}

fn lz_decompress(data: &[u8], len: usize) -> Result<Vec<u8>, String> {
    let bad = || "precompiled: corrupt source section".to_string();
    let mut out = Vec::with_capacity(len);
    let mut pos = 0;
    let read = |pos: &mut usize| -> Result<usize, String> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let b = *data.get(*pos).ok_or_else(bad)?;
            *pos += 1;
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return usize::try_from(v).map_err(|_| bad());
            }
            shift += 7;
            if shift >= 64 {
                return Err(bad());
            }
        }
    };
    loop {
        let lits = read(&mut pos)?;
        let end = pos.checked_add(lits).ok_or_else(bad)?;
        out.extend_from_slice(data.get(pos..end).ok_or_else(bad)?);
        pos = end;
        if out.len() >= len {
            break;
        }
        let m = read(&mut pos)?.checked_add(MIN_MATCH).ok_or_else(bad)?;
        let off = read(&mut pos)?;
        if off == 0 || off > out.len() || out.len() + m > len {
            return Err(bad());
        }
        let from = out.len() - off;
        for k in 0..m {
            out.push(out[from + k]);
        }
    }
    if out.len() != len || pos != data.len() {
        return Err(bad());
    }
    Ok(out)
}

// ---- reading ----------------------------------------------------------------------------------

/// A unit's kept-source section, as stored.
#[derive(Clone, Copy)]
pub(crate) struct KeptSection {
    data: &'static [u8],
    compressed: bool,
}

impl KeptSection {
    fn text(&self) -> Result<String, String> {
        let bytes = if self.compressed {
            let mut pos = 0;
            let mut len = 0u64;
            let mut shift = 0;
            loop {
                let b = *self
                    .data
                    .get(pos)
                    .ok_or("precompiled: corrupt source section")?;
                pos += 1;
                len |= ((b & 0x7f) as u64) << shift;
                if b & 0x80 == 0 {
                    break;
                }
                shift += 7;
                if shift >= 64 {
                    return Err("precompiled: corrupt source section".into());
                }
            }
            lz_decompress(&self.data[pos..], len as usize)?
        } else {
            self.data.to_vec()
        };
        String::from_utf8(bytes).map_err(|_| "precompiled: source section is not UTF-8".into())
    }
}

/// One unit of a parsed blob: its kind, key, AST (and bytecode / kept source) section bytes and
/// resolved links.
pub(crate) struct Unit {
    pub kind: SourceKind,
    pub key: String,
    pub ast: &'static [u8],
    pub bytecode: Option<&'static [u8]>,
    pub kept: Option<KeptSection>,
    pub links: Vec<(String, usize)>,
}

impl Unit {
    /// Decode the unit's AST and attach its precompiled bytecode.
    pub fn decode(&self) -> Result<Vec<Stmt>, String> {
        decode_unit(self.ast, self.bytecode, self.kept)
    }
}

pub(crate) struct Parsed {
    pub units: Vec<Unit>,
    pub entry: Option<usize>,
}

fn read_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn read_u64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// Validate a blob's header and read its manifest. Section payloads are not decoded here.
pub(crate) fn parse(bytes: &'static [u8]) -> Result<Parsed, String> {
    let err = |m: &str| Err(format!("precompiled: {m}"));
    if bytes.len() < HEADER_LEN || &bytes[..8] != MAGIC {
        return err("not a lumen precompiled blob");
    }
    if read_u32(bytes, 8) != FORMAT_VERSION {
        return err("container format version mismatch (rebuild with this lumen)");
    }
    if read_u32(bytes, 16) != AST_VERSION {
        return err("AST version mismatch (rebuild with this lumen)");
    }
    let built = &bytes[32..48];
    let built = &built[..built.iter().position(|&b| b == 0).unwrap_or(16)];
    if built != env!("CARGO_PKG_VERSION").as_bytes() {
        return Err(format!(
            "precompiled: built by lumen {} but loaded by {}",
            String::from_utf8_lossy(built),
            env!("CARGO_PKG_VERSION")
        ));
    }
    let layout_fp = read_u64(bytes, 24);
    if layout_fp != 0 && layout_fp != LAYOUT_FINGERPRINT {
        return err("bytecode layout fingerprint mismatch (rebuild with this lumen)");
    }
    let count = read_u32(bytes, 20) as usize;
    let table_end = HEADER_LEN + count * SECTION_ENTRY_LEN;
    if bytes.len() < table_end {
        return err("truncated section table");
    }
    let mut sections = Vec::with_capacity(count);
    for i in 0..count {
        let at = HEADER_LEN + i * SECTION_ENTRY_LEN;
        let (kind, flags, off, len) = (
            read_u32(bytes, at),
            read_u32(bytes, at + 4),
            read_u64(bytes, at + 8),
            read_u64(bytes, at + 16),
        );
        let end = off.checked_add(len).filter(|&e| e <= bytes.len() as u64);
        let Some(end) = end else {
            return err("section out of bounds");
        };
        sections.push((kind, flags, &bytes[off as usize..end as usize]));
    }
    let manifest = sections
        .iter()
        .find(|(k, _, _)| *k == SEC_MANIFEST)
        .map(|(_, _, d)| *d)
        .ok_or("precompiled: no manifest")?;
    read_manifest(manifest, &sections)
}

fn read_manifest(m: &[u8], sections: &[(u32, u32, &'static [u8])]) -> Result<Parsed, String> {
    struct Cur<'a> {
        b: &'a [u8],
        pos: usize,
    }
    impl Cur<'_> {
        fn uv(&mut self) -> Result<u64, String> {
            let mut v = 0u64;
            let mut shift = 0;
            loop {
                let b = *self
                    .b
                    .get(self.pos)
                    .ok_or("precompiled: truncated manifest")?;
                self.pos += 1;
                v |= ((b & 0x7f) as u64) << shift;
                if b & 0x80 == 0 {
                    return Ok(v);
                }
                shift += 7;
                if shift >= 64 {
                    return Err("precompiled: bad manifest varint".into());
                }
            }
        }
        fn u8(&mut self) -> Result<u8, String> {
            let b = *self
                .b
                .get(self.pos)
                .ok_or("precompiled: truncated manifest")?;
            self.pos += 1;
            Ok(b)
        }
        fn str(&mut self) -> Result<String, String> {
            let len = self.uv()? as usize;
            let end = self.pos.checked_add(len).ok_or("precompiled: bad length")?;
            let s = self
                .b
                .get(self.pos..end)
                .ok_or("precompiled: truncated manifest")?;
            self.pos = end;
            String::from_utf8(s.to_vec()).map_err(|_| "precompiled: bad utf8 key".into())
        }
    }
    let mut c = Cur { b: m, pos: 0 };
    let n = c.uv()? as usize;
    let mut units = Vec::with_capacity(n.min(1 << 16));
    for _ in 0..n {
        let kind = match c.u8()? {
            0 => SourceKind::Script,
            1 => SourceKind::Module,
            2 => SourceKind::CommonJs,
            k => return Err(format!("precompiled: bad unit kind {k}")),
        };
        let key = c.str()?;
        let sec = c.uv()? as usize;
        let ast = match sections.get(sec) {
            Some((SEC_AST, _, d)) => *d,
            _ => return Err(format!("precompiled: {key}: bad AST section {sec}")),
        };
        let bytecode = match c.uv()? as usize {
            0 => None,
            sec => match sections.get(sec) {
                Some((SEC_BYTECODE, _, d)) => Some(*d),
                _ => return Err(format!("precompiled: {key}: bad bytecode section {sec}")),
            },
        };
        let kept = match c.uv()? as usize {
            0 => None,
            sec => match sections.get(sec) {
                Some((SEC_SOURCE, flags, d)) => Some(KeptSection {
                    data: d,
                    compressed: flags & FLAG_LZ != 0,
                }),
                _ => return Err(format!("precompiled: {key}: bad source section {sec}")),
            },
        };
        let n_links = c.uv()? as usize;
        let mut links = Vec::with_capacity(n_links.min(1 << 12));
        for _ in 0..n_links {
            let spec = c.str()?;
            let to = c.uv()? as usize;
            if to >= n {
                return Err(format!("precompiled: {key}: bad link target {to}"));
            }
            links.push((spec, to));
        }
        units.push(Unit {
            kind,
            key,
            ast,
            bytecode,
            kept,
            links,
        });
    }
    let entry = match c.uv()? {
        0 => None,
        e => {
            let i = e as usize - 1;
            if units.get(i).is_none_or(|u| u.kind != SourceKind::Module) {
                return Err("precompiled: bad entry".into());
            }
            Some(i)
        }
    };
    Ok(Parsed { units, entry })
}

/// Decode one unit's AST and attach its bytecode section to the decoded functions: refusals
/// at once, chunks on demand (decoded when the function first tiers up) unless
/// `LUMEN_AOT_EAGER` is set, which decodes every chunk now so first calls run on the VM.
pub(crate) fn decode_unit(
    ast: &[u8],
    bytecode: Option<&'static [u8]>,
    kept: Option<KeptSection>,
) -> Result<Vec<Stmt>, String> {
    let text = match kept {
        Some(k) => k.text()?,
        None => String::new(),
    };
    let Some(bc) = bytecode else {
        return snapshot::decode(ast, &text);
    };
    static EAGER: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let eager = *EAGER.get_or_init(|| std::env::var_os("LUMEN_AOT_EAGER").is_some());
    let (body, funcs) = snapshot::decode_with_functions(ast, &text)?;
    crate::bytecode::serialize::attach_unit(bc, &funcs, eager)?;
    Ok(body)
}

/// Every unit's kept function text, by key (for tooling and tests: e.g. to check what a
/// keep-source bundle actually exposes). Units without kept text are omitted.
#[doc(hidden)]
pub fn kept_sources(blob: &'static [u8]) -> Result<Vec<(String, String)>, String> {
    let parsed = parse(blob)?;
    let mut out = Vec::new();
    for u in &parsed.units {
        if let Some(k) = u.kept {
            out.push((u.key.clone(), k.text()?));
        }
    }
    Ok(out)
}

/// A blob's units as `(kind, key)` (tooling/tests).
#[doc(hidden)]
pub fn list_units(blob: &'static [u8]) -> Result<Vec<(SourceKind, String)>, String> {
    Ok(parse(blob)?
        .units
        .into_iter()
        .map(|u| (u.kind, u.key))
        .collect())
}

/// Bytecode-tier counters, process-wide (see [`stats`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PrecompiledStats {
    /// Bytecode compiles (at run time: functions tiering up without a precompiled chunk).
    pub compiles: u64,
    /// Precompiled chunks registered at load, to decode when their function tiers up.
    pub chunks_registered: u64,
    /// Precompiled chunks decoded into a function's code cache (on tier-up, or at load with
    /// `LUMEN_AOT_EAGER`) — each one a compile the load did not do.
    pub chunks_attached: u64,
    /// Precompiled compiler refusals attached (functions that stay on the tree-walker).
    pub refusals_attached: u64,
}

/// The counters since the last [`reset_stats`] — e.g. to check that a precompiled
/// program ran without compiling anything at run time.
pub fn stats() -> PrecompiledStats {
    let (compiles, chunks_registered, chunks_attached, refusals_attached) =
        crate::bytecode::serialize::counters();
    PrecompiledStats {
        compiles,
        chunks_registered,
        chunks_attached,
        refusals_attached,
    }
}

pub fn reset_stats() {
    crate::bytecode::serialize::reset_counters();
}

/// What [`verify_bytecode`] checked, and how long each step took.
#[derive(Clone, Debug, Default)]
pub struct VerifyReport {
    pub units: usize,
    pub functions: usize,
    /// Attached chunks found identical to a fresh compile of the decoded function.
    pub chunks: usize,
    /// Attached refusals confirmed by a fresh compile.
    pub refusals: usize,
    /// Decoding every unit's AST.
    pub decode_ast: std::time::Duration,
    /// Decoding + attaching every bytecode section.
    pub attach: std::time::Duration,
    /// Compiling every attached function afresh (what the load saved).
    pub recompile: std::time::Duration,
}

/// Round-trip check of a blob's bytecode (for tests and tooling): decode every unit, attach its
/// chunks, then recompile each function of the decoded AST and require the result to equal the
/// attached chunk (or refusal) exactly.
#[doc(hidden)]
pub fn verify_bytecode(blob: &'static [u8]) -> Result<VerifyReport, String> {
    use std::time::Instant;
    let parsed = parse(blob)?;
    let mut rep = VerifyReport::default();
    for u in &parsed.units {
        rep.units += 1;
        let t = Instant::now();
        let text = match u.kept {
            Some(k) => k.text()?,
            None => String::new(),
        };
        let (_body, funcs) = snapshot::decode_with_functions(u.ast, &text)?;
        rep.decode_ast += t.elapsed();
        rep.functions += funcs.len();
        let Some(bc) = u.bytecode else { continue };
        let t = Instant::now();
        crate::bytecode::serialize::attach_unit(bc, &funcs, true)
            .map_err(|e| format!("{}: {e}", u.key))?;
        rep.attach += t.elapsed();
        let t = Instant::now();
        let (chunks, refusals) = crate::bytecode::serialize::verify_unit(&funcs)
            .map_err(|e| format!("{}: {e}", u.key))?;
        rep.recompile += t.elapsed();
        rep.chunks += chunks;
        rep.refusals += refusals;
    }
    Ok(rep)
}

// ---- the realm's unit table -------------------------------------------------------------------

/// One registered unit (module or CommonJS) of a loaded blob.
struct RegUnit {
    kind: SourceKind,
    key: String,
    ast: &'static [u8],
    bytecode: Option<&'static [u8]>,
    kept: Option<KeptSection>,
    /// Specifier -> registered unit id.
    links: HashMap<String, usize>,
}

/// Module and CommonJS units registered with a realm (kept in its host state).
#[derive(Default)]
pub(crate) struct PrecompiledModules {
    units: Vec<RegUnit>,
    /// Module key -> unit id.
    modules: HashMap<String, usize>,
    /// CommonJS key -> unit id.
    cjs: HashMap<String, usize>,
    /// Every bare specifier the bundler linked, blob-wide (first link wins): what a computed
    /// `import(name)` / `require(name)` from inside the bundle finds when the importing unit
    /// has no link of its own for it.
    bare: HashMap<String, usize>,
}

impl PrecompiledModules {
    fn unit_of(&self, key: &str) -> Option<&RegUnit> {
        let id = self.cjs.get(key).or_else(|| self.modules.get(key))?;
        self.units.get(*id)
    }

    /// The registered unit id `specifier` names from `referrer` (a unit key): the referrer's
    /// own link, an `aot:/` key, a relative path among the registered units (exact, `.js`,
    /// `.json`, `.cjs`, `/index.js`), or a bundled bare package.
    fn lookup(&self, specifier: &str, referrer: &str, want: SourceKind) -> Option<usize> {
        let from_aot = referrer.starts_with(KEY_PREFIX);
        if from_aot {
            let linked = self
                .cjs
                .get(referrer)
                .and_then(|&id| self.units[id].links.get(specifier))
                .or_else(|| {
                    self.modules
                        .get(referrer)
                        .and_then(|&id| self.units[id].links.get(specifier))
                });
            if let Some(&id) = linked {
                return Some(id);
            }
        }
        let table = |k: &str| -> Option<usize> {
            match want {
                SourceKind::Module => self.modules.get(k).copied(),
                _ => self
                    .cjs
                    .get(k)
                    .or_else(|| self.modules.get(k))
                    .copied(),
            }
        };
        let path = if let Some(path) = specifier.strip_prefix(KEY_PREFIX) {
            normalize(path)?
        } else if (specifier.starts_with("./") || specifier.starts_with("../")) && from_aot {
            let dir = match referrer.rfind('/') {
                Some(i) if i >= KEY_PREFIX.len() => &referrer[KEY_PREFIX.len()..=i],
                _ => "",
            };
            normalize(&format!("{dir}{specifier}"))?
        } else if from_aot && is_bare(specifier) {
            return self.bare.get(specifier).copied();
        } else {
            return None;
        };
        // Exact, then the extensions — the order the bundler resolved in.
        let key = format!("{KEY_PREFIX}{path}");
        [
            key.clone(),
            format!("{key}.js"),
            format!("{key}.json"),
            format!("{key}.cjs"),
            format!("{key}/index.js"),
        ]
        .into_iter()
        .find_map(|k| table(&k))
    }
}

/// A bare package specifier (`ws`, `@scope/pkg/sub`), not a URL/scheme, path or builtin form.
fn is_bare(s: &str) -> bool {
    !(s.is_empty()
        || s.starts_with('.')
        || s.starts_with('/')
        || s.starts_with('\\')
        || s.starts_with("node:")
        || s.contains(':'))
}

pub(crate) fn register_modules(interp: &mut Interp, parsed: &Parsed) {
    if !interp.host_state.has::<PrecompiledModules>() {
        interp.host_state.put(PrecompiledModules::default());
    }
    let has_cjs = parsed.units.iter().any(|u| u.kind == SourceKind::CommonJs);
    let table = interp.host_state.get_mut::<PrecompiledModules>().unwrap();
    // Registered ids for this blob's unit indices (scripts are not registered).
    let base = table.units.len();
    let mut ids = vec![usize::MAX; parsed.units.len()];
    let mut next = base;
    for (i, u) in parsed.units.iter().enumerate() {
        if u.kind != SourceKind::Script {
            ids[i] = next;
            next += 1;
        }
    }
    for (i, u) in parsed.units.iter().enumerate() {
        if u.kind == SourceKind::Script {
            continue;
        }
        let links: HashMap<String, usize> = u
            .links
            .iter()
            .filter(|(_, to)| ids[*to] != usize::MAX)
            .map(|(s, to)| (s.clone(), ids[*to]))
            .collect();
        for (s, &id) in &links {
            if is_bare(s) {
                table.bare.entry(s.clone()).or_insert(id);
            }
        }
        let id = ids[i];
        match u.kind {
            SourceKind::Module => table.modules.insert(u.key.clone(), id),
            _ => table.cjs.insert(u.key.clone(), id),
        };
        table.units.push(RegUnit {
            kind: u.kind,
            key: u.key.clone(),
            ast: u.ast,
            bytecode: u.bytecode,
            kept: u.kept,
            links,
        });
    }
    if has_cjs {
        install_cjs_api(interp);
    }
}

/// The decoded AST of `key` if it is a registered precompiled module (`None`: not one).
pub(crate) fn module_body(interp: &Interp, key: &str) -> Option<Result<Vec<Stmt>, String>> {
    if !key.starts_with(KEY_PREFIX) {
        return None;
    }
    let table = interp.host_state.get::<PrecompiledModules>()?;
    let u = &table.units[*table.modules.get(key)?];
    Some(decode_unit(u.ast, u.bytecode, u.kept).map_err(|e| format!("{key}: {e}")))
}

/// Resolve `specifier` (imported from `referrer`) to a registered precompiled module's key.
pub(crate) fn resolve(
    interp: &Interp,
    specifier: &str,
    referrer: &str,
    attr_type: Option<&str>,
) -> Option<String> {
    if attr_type.is_some() {
        return None;
    }
    let table = interp.host_state.get::<PrecompiledModules>()?;
    let id = table.lookup(specifier, referrer, SourceKind::Module)?;
    let key = &table.units[id].key;
    // A link may name a CommonJS unit (a dynamic import from CommonJS code): ESM sees it
    // through the synthesized module unit of the same key, when the bundler made one.
    table.modules.contains_key(key).then(|| key.clone())
}

// ---- the CommonJS side: `__lumenAot` ----------------------------------------------------------

fn install_cjs_api(interp: &mut Interp) {
    let global = interp.global.clone();
    if global.borrow().props.get("__lumenAot").is_some() {
        return;
    }
    let ns = interp.new_object();
    interp.def_method(&ns, "resolve", 2, aot_resolve);
    interp.def_method(&ns, "kind", 1, aot_kind);
    interp.def_method(&ns, "load", 1, aot_load);
    global.borrow_mut().props.insert(
        "__lumenAot",
        crate::value::Property::data(Value::Obj(ns), true, false, true),
    );
}

fn str_arg(args: &[Value], i: usize) -> Option<String> {
    match args.get(i) {
        Some(Value::Str(s)) => Some(s.to_string()),
        _ => None,
    }
}

/// `__lumenAot.resolve(specifier, parentKey)`: the key of the unit `require(specifier)` from
/// unit `parentKey` loads, or `undefined` when the blob does not have it.
fn aot_resolve(interp: &mut Interp, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let (Some(spec), referrer) = (str_arg(args, 0), str_arg(args, 1).unwrap_or_default()) else {
        return Ok(Value::Undefined);
    };
    let Some(table) = interp.host_state.get::<PrecompiledModules>() else {
        return Ok(Value::Undefined);
    };
    Ok(
        match table.lookup(&spec, &referrer, SourceKind::CommonJs) {
            Some(id) => Value::from_string(table.units[id].key.clone()),
            None => Value::Undefined,
        },
    )
}

/// `__lumenAot.kind(key)`: `"commonjs"` when `key` is a CommonJS unit (what `require` loads —
/// preferred over the ESM facade of the same key), `"module"` for an ES module, else
/// `undefined`.
fn aot_kind(interp: &mut Interp, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let key = str_arg(args, 0).unwrap_or_default();
    let Some(table) = interp.host_state.get::<PrecompiledModules>() else {
        return Ok(Value::Undefined);
    };
    Ok(match table.unit_of(&key).map(|u| u.kind) {
        Some(SourceKind::CommonJs) => Value::from_string("commonjs".to_string()),
        Some(SourceKind::Module) => Value::from_string("module".to_string()),
        _ => Value::Undefined,
    })
}

/// `__lumenAot.load(key)`: the CommonJS unit's module-wrapper function
/// `(exports, require, module, __filename, __dirname)`.
fn aot_load(interp: &mut Interp, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let key = str_arg(args, 0).unwrap_or_default();
    let unit = interp
        .host_state
        .get::<PrecompiledModules>()
        .and_then(|t| t.cjs.get(&key).map(|&id| &t.units[id]))
        .map(|u| (u.ast, u.bytecode, u.kept));
    let Some((ast, bytecode, kept)) = unit else {
        return Err(interp.make_error("Error", format!("precompiled: no CommonJS unit {key}")));
    };
    let body = decode_unit(ast, bytecode, kept)
        .map_err(|e| interp.make_error("SyntaxError", format!("{key}: {e}")))?;
    let saved = interp.strict;
    interp.strict = false;
    let result = interp.run_program(&body);
    interp.strict = saved;
    result
}

/// Collapse `.`/`..` segments of a `/`-separated relative path (`None`: escapes the root).
fn normalize(path: &str) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop()?;
            }
            s => out.push(s),
        }
    }
    Some(out.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lz_round_trips() {
        let samples: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"a".to_vec(),
            b"abcabcabcabcabcabcabcabc".to_vec(),
            "function f(a) { return a + 1; } function g(b) { return b + 1; }"
                .repeat(50)
                .into_bytes(),
            (0..5000u32).map(|i| (i * 7919 % 251) as u8).collect(),
        ];
        for s in samples {
            let c = lz_compress(&s);
            assert_eq!(lz_decompress(&c, s.len()).unwrap(), s);
        }
    }

    #[test]
    fn kept_source_is_function_text_only() {
        let src = "// top comment MARK_TOP\nconst x = 1; /* between MARK_MID */\n\
                   export function f(a) { /* in f MARK_F */ return () => a; }\n\
                   export const g = (b) => b * 2; // after MARK_END\n";
        let unit = CompiledUnit::compile_with_options(
            src,
            SourceKind::Module,
            CompileOptions {
                bytecode: true,
                keep_source: true,
            },
        )
        .unwrap();
        let kept = unit.kept_source().unwrap();
        assert!(kept.contains("MARK_F"));
        assert!(kept.contains("(b) => b * 2"));
        for m in ["MARK_TOP", "MARK_MID", "MARK_END", "const x"] {
            assert!(!kept.contains(m), "{m} leaked into {kept:?}");
        }
    }

    /// `toString` of kept functions from a loaded blob: declarations, arrows, methods, classes,
    /// and functions inside template substitutions (which the parser lexes as separate text).
    #[test]
    fn kept_source_round_trips_through_a_blob() {
        let src = "// HEADER_MARK\n\
                   function f(a) { /* c */ return a + 1; }\n\
                   const g = (x, y) => x * y;\n\
                   class K { m() { return 1; } }\n\
                   const t = `${[1].map(v => v * 2)}`;\n\
                   export function h() { return `${[2].map(q => q)}` + String(w => w); }\n\
                   globalThis.__r = [String(f), String(g), String(K), String(new K().m), \
                   String(() => 0), t, h()].join('|');\n";
        let unit = CompiledUnit::compile_with_options(
            src,
            SourceKind::Module,
            CompileOptions {
                bytecode: true,
                keep_source: true,
            },
        )
        .unwrap();
        assert!(!unit.kept_source().unwrap().contains("HEADER_MARK"));
        let mut b = PrecompileBundle::new();
        b.add_compiled("main.js", unit).unwrap();
        b.set_entry("main.js").unwrap();
        let blob: &'static [u8] = Box::leak(b.finish().into_boxed_slice());
        let mut e = crate::Engine::new();
        match e.load_precompiled(&Precompiled::from_static(blob)) {
            Ok(crate::Completion::Value(_)) => {}
            Ok(crate::Completion::Throw { name, message }) => panic!("{name}: {message}"),
            Err(err) => panic!("load failed: {}", err.message),
        }
        let got = match e.eval("globalThis.__r", false) {
            Ok(crate::Completion::Value(v)) => v,
            _ => panic!("eval failed"),
        };
        assert_eq!(
            got,
            "function f(a) { /* c */ return a + 1; }|(x, y) => x * y|class K { m() { return 1; } }|\
             m() { return 1; }|() => 0|2|2w => w"
        );
    }

    #[test]
    fn cjs_wrapper_text_is_not_kept() {
        let src = "// header MARK_HEAD\nmodule.exports = function add(a, b) { return a + b; };\n";
        let unit = CompiledUnit::compile_with_options(
            src,
            SourceKind::CommonJs,
            CompileOptions {
                bytecode: false,
                keep_source: true,
            },
        )
        .unwrap();
        assert_eq!(
            unit.kept_source().unwrap(),
            "function add(a, b) { return a + b; }"
        );
        assert_eq!(unit.requires(), &[] as &[String]);
    }

    #[test]
    fn collects_literal_dynamic_imports_and_requires() {
        let unit = CompiledUnit::compile(
            "const a = require('ws'); const b = require(x); async function f() { await import('./d.js'); await import(y); }",
            SourceKind::Script,
        )
        .unwrap();
        assert_eq!(unit.requires(), &["ws".to_string()]);
        assert_eq!(unit.dynamic_imports(), &["./d.js".to_string()]);
    }
}
