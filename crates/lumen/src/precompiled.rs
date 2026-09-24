//! Ahead-of-time compiled JavaScript: a versioned, sectioned blob an embedder links into its
//! binary in place of the JS source (see the `lumen-aot` crate's `include_js!`), loaded with
//! [`Engine::load_precompiled`](crate::Engine::load_precompiled).
//!
//! ## What is in a blob
//! Parser output plus bytecode. Each unit (a script, one ES module, or one CommonJS module of a
//! bundle) is its AST in the *split* form of [`snapshot::encode_split`]: what a load decodes at
//! once — every function's header (name, parameters, flags, body facts) and the unit's
//! top-level statements — and, apart from it, every function *body*. No function body is a
//! byte range into source text, and by default no function keeps its source text, so
//! `Function.prototype.toString` of a precompiled function renders the spec's NativeFunction
//! form (`function f() { [native code] }`). String literals, template strings, regex literals,
//! identifiers and property names are of course still in it — they are program data, not
//! source text.
//!
//! ### Deferred bodies
//! A function body is decoded the first time something needs it — the tree-walker running the
//! function, a bytecode compile at run time, a `static` block — straight from the blob (see
//! [`AotBody`]); a function that runs on its precompiled bytecode never decodes its body at
//! all. The collector may release a decoded body that went cold, like a lazily parsed one: it
//! decodes again on the next use.
//!
//! ### Kept function text
//! Code that ships functions somewhere as text (`fn.toString()` sent to a browser — every
//! Puppeteer `page.evaluate` callback) needs that text. A unit compiled with *keep source*
//! carries the exact source slices of its outermost functions and classes, concatenated
//! (nested functions are sub-slices; text *between* top-level functions — module statements,
//! comments outside functions — is not kept). Its functions' `toString` ranges point into that
//! text, which stays compressed until the first `toString` that needs it (see [`KeptRef`]).
//! The kept text is in the binary (compressed, but trivially recoverable): keep source only for
//! the modules that need it.
//!
//! ### Bytecode
//! Next to its AST, a unit normally carries bytecode: every function of the unit run through
//! the bytecode compiler at build time, each resulting chunk serialized (see
//! `bytecode::serialize` for the chunk format) and keyed by the function's *function index* —
//! the order the AST codec enters functions, which its decoder reproduces. Loading registers
//! each chunk with its function, and a function with a chunk tiers up on its *first* call (no
//! tree-walked warm-up that would need its body): the chunk is decoded then — no bytecode
//! compile at run time. A function the compiler refused is recorded too (it stays on the
//! tree-walker without a retry). `LUMEN_AOT_EAGER=1` decodes every chunk at load instead.
//!
//! ### Stores
//! Function bodies, bytecode and kept text are each concatenated over the whole blob into a
//! *store* section. A unit refers to its slices by offset. The bytecode store is *raw*: chunks
//! decode straight from the blob's static bytes (a load registers every chunk of a unit, so a
//! compressed store would have to be decompressed whole and held). The others are compressed
//! ([`crate::lzh`]) in independent blocks of [`STORE_BLOCK`] bytes: kept text (read only by
//! `toString`) decompresses a block the first time a slice in it is needed and keeps it for the
//! life of the process; a function body is decoded through a small cache of recently
//! decompressed blocks and copied out, so a program's decoded bodies never pin whole blocks.
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
//! Section kinds: [`SEC_MANIFEST`] (exactly one — the unit list), [`SEC_AST`] (one per unit:
//! its load-time AST), [`SEC_STORE`] (the body, bytecode and kept-text stores). Readers skip
//! kinds they do not know, so later tiers (native code) are added as new section kinds
//! referenced from new manifest fields. [`SEC_LINES`] (at most one) holds the units' line
//! tables for stack traces.
//!
//! A store: `block_size`, `raw_len`, `block_count` (LEB128), `block_count` compressed block
//! lengths (LEB128), then the blocks, each a [`crate::lzh`] frame of `block_size` bytes (the
//! last one shorter).
//!
//! The manifest: the section indices of the body, bytecode and kept-text stores (LEB128, 0 =
//! none), `unit_count` (LEB128), then per unit `kind` (u8: 0 script, 1 module, 2 CommonJS),
//! `key` (LEB128 length + UTF-8), `ast_section` (LEB128 index into the section table), its
//! bodies (`offset`, `len` into the body store), its bytecode and its kept text (each
//! `len + 1` then `offset`, or a single 0 = none), then its links: `link_count` (LEB128) x
//! (`specifier` string, `target` LEB128 unit index) — every import/require specifier the
//! bundler resolved to another unit of the blob; then `entry` (LEB128: unit index + 1, 0 =
//! none).
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
pub const FORMAT_VERSION: u32 = 6;
/// The AST codec version units are encoded with.
pub const AST_VERSION: u32 = snapshot::VERSION;

const MAGIC: &[u8; 8] = b"LUMENAOT";
const HEADER_LEN: usize = 48;
const SECTION_ENTRY_LEN: usize = 24;

/// The unit list (kind, key, AST section of every unit, and the entry).
pub const SEC_MANIFEST: u32 = 1;
/// One unit's load-time AST (function headers and top-level statements; see "What is in a
/// blob" above).
pub const SEC_AST: u32 = 2;
// 3 and 5 were per-unit bytecode / kept-text sections (format 3); 4 is reserved for native
// code (guarded by `layout_fp`).
/// A compressed store: every unit's function bodies, bytecode or kept text (see "Stores").
pub const SEC_STORE: u32 = 6;
/// Every unit's source line table, for stack-trace lines and columns of code whose text is not
/// in the blob (format 5): per unit in manifest order, `len` (LEB128) + the table
/// (`stack_trace::LineTable::encode`; empty = none).
pub const SEC_LINES: u32 = 7;

/// The uncompressed size of a store block: the unit of decompression.
pub const STORE_BLOCK: usize = 64 * 1024;

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
    bodies: Vec<u8>,
    bytecode: Option<Vec<u8>>,
    kept: Option<String>,
    stats: SectionStats,
    imports: Vec<String>,
    deps: snapshot::Deps,
    /// The source's line table (stack-trace positions; see [`SEC_LINES`]).
    lines: Vec<u8>,
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
        let unit = snapshot::encode_split(&body, opts.keep_source.then_some((text, skip)));
        let (bytecode, stats) = if opts.bytecode {
            let (bc, stats) = crate::bytecode::serialize::encode_unit(&unit.funcs);
            (Some(bc), stats)
        } else {
            (None, SectionStats::default())
        };
        Ok(CompiledUnit {
            kind,
            ast: unit.ast,
            bodies: unit.bodies,
            bytecode,
            kept: opts.keep_source.then_some(unit.kept),
            stats,
            imports,
            deps: unit.deps,
            lines: crate::interpreter::stack_trace::LineTable::build(text).encode(),
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

    /// The encoded AST's size in bytes, load-time part plus function bodies, uncompressed (the
    /// bytecode is [`CompiledUnit::bytecode_len`]).
    pub fn len(&self) -> usize {
        self.ast.len() + self.bodies.len()
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
    bodies: Vec<u8>,
    bytecode: Option<Vec<u8>>,
    kept: Option<String>,
    links: Vec<(String, usize)>,
    lines: Vec<u8>,
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

    /// Whether kept function text is compressed in the blob (default on).
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
            bodies: unit.bodies,
            bytecode: unit.bytecode,
            kept: unit.kept,
            links: Vec::new(),
            lines: unit.lines,
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
        // Stores: every unit's bodies, bytecode and kept text concatenated, (offset, len) each.
        let mut bodies = Vec::new();
        let mut bytecode = Vec::new();
        let mut kept = Vec::new();
        let at = |store: &mut Vec<u8>, data: &[u8]| {
            let off = store.len();
            store.extend_from_slice(data);
            (off, data.len())
        };
        let spans: Vec<_> = self
            .units
            .iter()
            .map(|u| {
                (
                    at(&mut bodies, &u.bodies),
                    u.bytecode.as_deref().map(|b| at(&mut bytecode, b)),
                    u.kept.as_deref().map(|k| at(&mut kept, k.as_bytes())),
                )
            })
            .collect();
        let has_bytecode = self.units.iter().any(|u| u.bytecode.is_some());
        let has_kept = self.units.iter().any(|u| u.kept.is_some());
        let stores = [
            store_section(&bodies, true),
            raw_store_section(&bytecode),
            store_section(&kept, self.compress),
        ];
        // Section order: the manifest, every unit's AST, then the stores (those in use).
        let n = self.units.len();
        let store_used = [true, has_bytecode, has_kept];
        let mut store_index = [0u64; 3];
        let mut next = 1 + n as u64;
        for k in 0..3 {
            if store_used[k] {
                store_index[k] = next;
                next += 1;
            }
        }
        let mut manifest = Vec::new();
        for i in store_index {
            uv(&mut manifest, i);
        }
        uv(&mut manifest, n as u64);
        let opt_span = |m: &mut Vec<u8>, s: Option<(usize, usize)>| match s {
            Some((off, len)) => {
                uv(m, len as u64 + 1);
                uv(m, off as u64);
            }
            None => uv(m, 0),
        };
        for (i, u) in self.units.iter().enumerate() {
            manifest.push(match u.kind {
                SourceKind::Script => 0,
                SourceKind::Module => 1,
                SourceKind::CommonJs => 2,
            });
            uv(&mut manifest, u.key.len() as u64);
            manifest.extend_from_slice(u.key.as_bytes());
            uv(&mut manifest, i as u64 + 1); // section 0 is the manifest
            let (b, bc, k) = spans[i];
            uv(&mut manifest, b.0 as u64);
            uv(&mut manifest, b.1 as u64);
            opt_span(&mut manifest, bc);
            opt_span(&mut manifest, k);
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
        for k in 0..3 {
            if store_used[k] {
                sections.push((SEC_STORE, 0, &stores[k]));
            }
        }
        let mut lines = Vec::new();
        for u in &self.units {
            uv(&mut lines, u.lines.len() as u64);
            lines.extend_from_slice(&u.lines);
        }
        if self.units.iter().any(|u| !u.lines.is_empty()) {
            sections.push((SEC_LINES, 0, &lines));
        }
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

/// A store section over `data`: blocks of [`STORE_BLOCK`] bytes, each compressed (or stored).
fn store_section(data: &[u8], compress: bool) -> Vec<u8> {
    let frames: Vec<Vec<u8>> = data
        .chunks(STORE_BLOCK)
        .map(|block| {
            if compress {
                crate::lzh::compress(block)
            } else {
                crate::lzh::stored(block)
            }
        })
        .collect();
    let mut out = Vec::new();
    uv(&mut out, STORE_BLOCK as u64);
    uv(&mut out, data.len() as u64);
    uv(&mut out, frames.len() as u64);
    for f in &frames {
        uv(&mut out, f.len() as u64);
    }
    for f in &frames {
        out.extend_from_slice(f);
    }
    out
}

/// A raw store section over `data`: a zero block size, the length, then the bytes as they are.
fn raw_store_section(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 12);
    uv(&mut out, 0);
    uv(&mut out, data.len() as u64);
    out.extend_from_slice(data);
    out
}

/// Precompile an extension's JS glue (a script) for `lumen_host::Extension::js_init_snapshot`:
/// bytecode plus the glue's function text, kept compressed so `Function.prototype.toString` of
/// a JS-implemented builtin shows its source (as Node's do, and as `util.inspect` relies on)
/// without the plain text in the binary.
pub fn precompile_glue(src: &str, label: &str) -> Result<Vec<u8>, String> {
    let unit = CompiledUnit::compile_with_options(
        src,
        SourceKind::Script,
        CompileOptions {
            bytecode: true,
            keep_source: true,
        },
    )?;
    let mut b = PrecompileBundle::new();
    b.add_compiled(label, unit)?;
    Ok(b.finish())
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

// ---- stores ------------------------------------------------------------------------------------

/// Slices of store sections, decompressed on first use and cached for the process (see
/// "Stores" in the module docs). Keyed by the section's address: a blob is static data.
pub(crate) mod store {
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct Index {
        block_size: usize,
        raw_len: usize,
        /// Each block's compressed frame, as a range of the section.
        frames: Vec<std::ops::Range<usize>>,
    }

    #[derive(Default)]
    struct Cache {
        index: HashMap<usize, &'static Index>,
        blocks: HashMap<(usize, usize), &'static [u8]>,
        /// Slices that span blocks, joined.
        joined: HashMap<(usize, usize, usize), &'static [u8]>,
    }

    static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

    /// `(decompressed blocks, their bytes, joined cross-block slices, their bytes)` held by the
    /// process-wide cache (for the `LUMEN_MEM_STATS` report).
    pub(crate) fn cached_bytes() -> (usize, usize, usize, usize) {
        let guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().map_or((0, 0, 0, 0), |c| {
            (
                c.blocks.len(),
                c.blocks.values().map(|b| b.len()).sum(),
                c.joined.len(),
                c.joined.values().map(|b| b.len()).sum(),
            )
        })
    }

    fn uv(b: &[u8], pos: &mut usize) -> Result<usize, String> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let byte = *b.get(*pos).ok_or("precompiled: truncated store")?;
            *pos += 1;
            v |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return usize::try_from(v).map_err(|_| "precompiled: bad store".to_string());
            }
            shift += 7;
            if shift >= 64 {
                return Err("precompiled: bad store".into());
            }
        }
    }

    fn index(section: &'static [u8]) -> Result<Index, String> {
        let mut pos = 0;
        let block_size = uv(section, &mut pos)?;
        let raw_len = uv(section, &mut pos)?;
        let count = uv(section, &mut pos)?;
        if block_size == 0 || count != raw_len.div_ceil(block_size) {
            return Err("precompiled: bad store header".into());
        }
        let mut lens = Vec::with_capacity(count);
        for _ in 0..count {
            lens.push(uv(section, &mut pos)?);
        }
        let mut frames = Vec::with_capacity(count);
        for len in lens {
            let end = pos
                .checked_add(len)
                .filter(|&e| e <= section.len())
                .ok_or("precompiled: store block out of bounds")?;
            frames.push(pos..end);
            pos = end;
        }
        Ok(Index {
            block_size,
            raw_len,
            frames,
        })
    }

    /// A raw store's bytes (see `raw_store_section`), or `None` for a compressed store.
    fn raw(section: &'static [u8]) -> Result<Option<&'static [u8]>, String> {
        if section.first() != Some(&0) {
            return Ok(None);
        }
        let mut pos = 1;
        let len = uv(section, &mut pos)?;
        section
            .get(pos..)
            .filter(|b| b.len() == len)
            .map(Some)
            .ok_or_else(|| "precompiled: bad raw store".to_string())
    }

    /// Recently decompressed blocks for [`bytes`], most recent last: `(section, block, data)`.
    const RECENT: usize = 4;
    static RECENT_BLOCKS: Mutex<Vec<(usize, usize, Box<[u8]>)>> = Mutex::new(Vec::new());

    /// `len` bytes at `off` of the store `section`, for a reader that only needs them for a
    /// moment (a function body being decoded): a block that is not already held is
    /// decompressed into a small most-recently-used cache rather than kept for the process's
    /// life, and the slice is copied out of it.
    pub(crate) fn bytes(
        section: &'static [u8],
        off: usize,
        len: usize,
    ) -> Result<std::borrow::Cow<'static, [u8]>, String> {
        use std::borrow::Cow;
        if len == 0 {
            return Ok(Cow::Borrowed(&[]));
        }
        if let Some(raw) = raw(section)? {
            return off
                .checked_add(len)
                .and_then(|end| raw.get(off..end))
                .map(Cow::Borrowed)
                .ok_or_else(|| "precompiled: store slice out of bounds".into());
        }
        let _mem = crate::memstats::enter(crate::memstats::Cat::AotStore);
        let key = section.as_ptr() as usize;
        let idx: &'static Index = {
            let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
            let cache = guard.get_or_insert_with(Cache::default);
            match cache.index.get(&key) {
                Some(i) => i,
                None => {
                    let i: &'static Index = Box::leak(Box::new(index(section)?));
                    cache.index.insert(key, i);
                    i
                }
            }
        };
        let end = off
            .checked_add(len)
            .filter(|&e| e <= idx.raw_len)
            .ok_or("precompiled: store slice out of bounds")?;
        let (first, last) = (off / idx.block_size, (end - 1) / idx.block_size);
        let mut out = Vec::with_capacity(len);
        let mut recent = RECENT_BLOCKS.lock().unwrap_or_else(|e| e.into_inner());
        for b in first..=last {
            let at = match recent.iter().position(|(k, n, _)| *k == key && *n == b) {
                Some(at) => at,
                None => {
                    let d = crate::lzh::decompress(&section[idx.frames[b].clone()])?;
                    if d.len() != idx.block_size.min(idx.raw_len - b * idx.block_size) {
                        return Err("precompiled: bad store block".into());
                    }
                    if recent.len() == RECENT {
                        recent.remove(0);
                    }
                    recent.push((key, b, d.into_boxed_slice()));
                    recent.len() - 1
                }
            };
            let entry = recent.remove(at);
            let lo = if b == first { off - first * idx.block_size } else { 0 };
            let hi = (lo + (len - out.len())).min(entry.2.len());
            out.extend_from_slice(&entry.2[lo..hi]);
            recent.push(entry);
        }
        Ok(Cow::Owned(out))
    }

    /// Bytes held by the recently-decompressed-block cache (for the `LUMEN_MEM_STATS` report).
    pub(crate) fn recent_bytes() -> usize {
        RECENT_BLOCKS
            .lock()
            .map(|r| r.iter().map(|b| b.2.len()).sum())
            .unwrap_or(0)
    }

    /// `len` bytes at `off` of the store `section`, uncompressed.
    pub(crate) fn range(
        section: &'static [u8],
        off: usize,
        len: usize,
    ) -> Result<&'static [u8], String> {
        if len == 0 {
            return Ok(&[]);
        }
        if let Some(raw) = raw(section)? {
            return off
                .checked_add(len)
                .and_then(|end| raw.get(off..end))
                .ok_or_else(|| "precompiled: store slice out of bounds".into());
        }
        let _mem = crate::memstats::enter(crate::memstats::Cat::AotStore);
        let key = section.as_ptr() as usize;
        let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        let cache = guard.get_or_insert_with(Cache::default);
        if let Some(j) = cache.joined.get(&(key, off, len)) {
            return Ok(j);
        }
        let idx: &'static Index = match cache.index.get(&key) {
            Some(i) => i,
            None => {
                let i: &'static Index = Box::leak(Box::new(index(section)?));
                cache.index.insert(key, i);
                i
            }
        };
        let end = off
            .checked_add(len)
            .filter(|&e| e <= idx.raw_len)
            .ok_or("precompiled: store slice out of bounds")?;
        let (first, last) = (off / idx.block_size, (end - 1) / idx.block_size);
        let decode = |b: usize| -> Result<Vec<u8>, String> {
            let d = crate::lzh::decompress(&section[idx.frames[b].clone()])?;
            if d.len() != idx.block_size.min(idx.raw_len - b * idx.block_size) {
                return Err("precompiled: bad store block".into());
            }
            Ok(d)
        };
        let start = off - first * idx.block_size;
        if first == last {
            let block: &'static [u8] = match cache.blocks.get(&(key, first)) {
                Some(d) => d,
                None => {
                    let d: &'static [u8] = Box::leak(decode(first)?.into_boxed_slice());
                    cache.blocks.insert((key, first), d);
                    d
                }
            };
            return Ok(&block[start..start + len]);
        }
        // A slice across blocks is joined (and cached as such); its blocks are cached on their
        // own only if something else already needed them.
        let mut joined = Vec::with_capacity(len);
        for b in first..=last {
            let lo = if b == first { start } else { 0 };
            let want = len - joined.len();
            match cache.blocks.get(&(key, b)) {
                Some(d) => joined.extend_from_slice(&d[lo..(lo + want).min(d.len())]),
                None => {
                    let d = decode(b)?;
                    joined.extend_from_slice(&d[lo..(lo + want).min(d.len())]);
                }
            }
        }
        let joined: &'static [u8] = Box::leak(joined.into_boxed_slice());
        cache.joined.insert((key, off, len), joined);
        Ok(joined)
    }
}

/// A unit's kept function text: a slice of the blob's kept-text store (`FnSource::Kept` ranges
/// point into it). Nothing is decompressed until a `toString` reads a function's text, and then
/// only the store blocks that function's text spans.
pub struct KeptRef {
    store: &'static [u8],
    off: usize,
    len: usize,
}

impl KeptRef {
    /// The whole text (`None`: the store is corrupt).
    pub fn text(&self) -> Option<&'static str> {
        self.slice(0, self.len as u32)
    }

    /// The text at `start..end` (`None`: out of range or corrupt — `toString` then renders the
    /// native form).
    pub fn slice(&self, start: u32, end: u32) -> Option<&'static str> {
        let (start, end) = (start as usize, end as usize);
        if start > end || end > self.len {
            return None;
        }
        let bytes = store::range(self.store, self.off + start, end - start).ok()?;
        std::str::from_utf8(bytes).ok()
    }
}

/// An ahead-of-time function's deferred body (held in its `LazyBody`): its unit and function
/// index (see [`snapshot::SplitUnit`]).
pub struct AotBody {
    pub(crate) unit: std::rc::Rc<snapshot::SplitUnit>,
    pub(crate) idx: usize,
}

/// Decode a deferred body (see [`crate::ast::Function::ensure_body`]).
pub(crate) fn decode_body(aot: &AotBody) -> Result<Vec<Stmt>, String> {
    aot.unit.decode_body(aot.idx)
}

// ---- reading ----------------------------------------------------------------------------------

/// Where one unit's parts are: its load-time AST section and its slices of the stores.
#[derive(Clone, Copy)]
pub(crate) struct UnitData {
    pub ast: &'static [u8],
    bodies: (&'static [u8], usize),
    bytecode: Option<(&'static [u8], usize, usize)>,
    kept: Option<(&'static [u8], usize, usize)>,
    /// The unit's line table (see [`SEC_LINES`]).
    lines: Option<&'static [u8]>,
}

impl UnitData {
    fn kept_ref(&self) -> Option<std::rc::Rc<KeptRef>> {
        self.kept.map(|(store, off, len)| {
            std::rc::Rc::new(KeptRef { store, off, len })
        })
    }

    fn bytecode(&self) -> Result<Option<&'static [u8]>, String> {
        self.bytecode
            .map(|(store, off, len)| store::range(store, off, len))
            .transpose()
    }

    /// The unit's AST, decoded on demand (see [`snapshot::SplitUnit`]). The unit's source
    /// marker and line table are noted for the caller that runs it
    /// (`stack_trace::take_parsed_source`).
    fn split_unit(&self) -> Result<std::rc::Rc<snapshot::SplitUnit>, String> {
        let kept = self.kept_ref();
        let src: std::rc::Rc<str> = std::rc::Rc::from("");
        let (store, base) = self.bodies;
        let unit = snapshot::SplitUnit::new(
            self.ast,
            src.clone(),
            kept,
            Box::new(move |start, len| store::bytes(store, base + start, len)),
        )?;
        let table = self
            .lines
            .map(|b| std::rc::Rc::new(crate::interpreter::stack_trace::LineTable::lazy(b)));
        crate::interpreter::stack_trace::note_parsed_source(src, table);
        Ok(unit)
    }
}

/// One unit of a parsed blob: its kind, key, parts and resolved links.
pub(crate) struct Unit {
    pub kind: SourceKind,
    pub key: String,
    pub data: UnitData,
    pub links: Vec<(String, usize)>,
}

impl Unit {
    /// Decode the unit's AST and attach its precompiled bytecode.
    pub fn decode(&self) -> Result<Vec<Stmt>, String> {
        decode_unit(&self.data)
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
    let mut parsed = read_manifest(manifest, &sections)?;
    if let Some((_, _, mut d)) = sections.iter().find(|(k, _, _)| *k == SEC_LINES).copied() {
        for u in &mut parsed.units {
            let (mut at, mut len, mut shift) = (0usize, 0usize, 0);
            while let Some(&b) = d.get(at) {
                at += 1;
                len |= ((b & 0x7f) as usize) << shift;
                shift += 7;
                if b & 0x80 == 0 || shift > 56 {
                    break;
                }
            }
            let Some(table) = at.checked_add(len).and_then(|end| d.get(at..end)) else {
                return err("truncated line tables");
            };
            u.data.lines = (!table.is_empty()).then_some(table);
            d = &d[at + len..];
        }
    }
    Ok(parsed)
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
        fn usize(&mut self) -> Result<usize, String> {
            usize::try_from(self.uv()?).map_err(|_| "precompiled: bad manifest value".into())
        }
    }
    let mut c = Cur { b: m, pos: 0 };
    let mut stores: [Option<&'static [u8]>; 3] = [None; 3];
    for s in &mut stores {
        *s = match c.usize()? {
            0 => None,
            i => match sections.get(i) {
                Some((SEC_STORE, _, d)) => Some(*d),
                _ => return Err(format!("precompiled: bad store section {i}")),
            },
        };
    }
    let [body_store, bc_store, kept_store] = stores;
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
        let sec = c.usize()?;
        let ast = match sections.get(sec) {
            Some((SEC_AST, _, d)) => *d,
            _ => return Err(format!("precompiled: {key}: bad AST section {sec}")),
        };
        let bodies_off = c.usize()?;
        let bodies_len = c.usize()?;
        let bodies = match body_store {
            Some(store) => (store, bodies_off),
            None if bodies_len == 0 => (&[][..], 0),
            None => return Err(format!("precompiled: {key}: no body store")),
        };
        let mut span = |store: Option<&'static [u8]>, what: &str| match c.usize()? {
            0 => Ok(None),
            len => {
                let off = c.usize()?;
                let store =
                    store.ok_or_else(|| format!("precompiled: {key}: no {what} store"))?;
                Ok::<_, String>(Some((store, off, len - 1)))
            }
        };
        let bytecode = span(bc_store, "bytecode")?;
        let kept = span(kept_store, "kept-text")?;
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
            data: UnitData {
                ast,
                bodies,
                bytecode,
                kept,
                lines: None,
            },
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

/// Decode one unit's AST and attach its bytecode: refusals at once, chunks on demand (decoded
/// when the function first runs) unless `LUMEN_AOT_EAGER` is set, which decodes every chunk
/// now.
pub(crate) fn decode_unit(u: &UnitData) -> Result<Vec<Stmt>, String> {
    let unit = u.split_unit()?;
    if let Some(bc) = u.bytecode()? {
        static EAGER: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let eager = *EAGER.get_or_init(|| std::env::var_os("LUMEN_AOT_EAGER").is_some());
        crate::bytecode::serialize::attach_unit(bc, &unit)?;
        if eager {
            // Every function and chunk now: the unit's functions stay alive through the
            // top-level AST and the chunks that reference them.
            let body = unit.decode_top()?;
            for f in unit.all_functions()? {
                crate::bytecode::serialize::attach_now(&f);
            }
            return Ok(body);
        }
    }
    unit.decode_top()
}

/// Every unit's kept function text, by key (for tooling and tests: e.g. to check what a
/// keep-source bundle actually exposes). Units without kept text are omitted.
#[doc(hidden)]
pub fn kept_sources(blob: &'static [u8]) -> Result<Vec<(String, String)>, String> {
    let parsed = parse(blob)?;
    let mut out = Vec::new();
    for u in &parsed.units {
        if let Some(k) = u.data.kept_ref() {
            let text = k.text().ok_or("precompiled: corrupt kept text")?;
            out.push((u.key.clone(), text.to_string()));
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
        let unit = u.data.split_unit()?;
        let funcs = unit.all_functions()?;
        rep.decode_ast += t.elapsed();
        rep.functions += funcs.len();
        let Some(bc) = u.data.bytecode()? else {
            continue;
        };
        let t = Instant::now();
        crate::bytecode::serialize::attach_unit(bc, &unit).map_err(|e| format!("{}: {e}", u.key))?;
        for f in &funcs {
            crate::bytecode::serialize::attach_now(f);
        }
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
    data: UnitData,
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
            data: u.data,
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
    Some(decode_unit(&u.data).map_err(|e| format!("{key}: {e}")))
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
        .map(|u| u.data);
    let Some(data) = unit else {
        return Err(interp.make_error("Error", format!("precompiled: no CommonJS unit {key}")));
    };
    let t_decode = std::time::Instant::now();
    let body = decode_unit(&data)
        .map_err(|e| interp.make_error("SyntaxError", format!("{key}: {e}")))?;
    crate::modules::load_stats::add(&crate::modules::load_stats::PARSE, t_decode, data.ast.len());
    // The unit's own text starts after the wrapper header, on its first line.
    let src = interp.adopt_parsed_source(Some(&key), CJS_WRAPPER_HEAD.len() as u32);
    let saved = interp.strict;
    interp.strict = false;
    let result = interp.run_program(&body);
    interp.strict = saved;
    // The wrapper function: its frame is V8's `Object.<anonymous>`.
    if let (Some(src), Ok(Value::Obj(f))) = (&src, &result) {
        let ptr = crate::value::Gc::as_ptr(f) as usize;
        interp.register_source(src, None, None, ptr, None);
    }
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

    /// Bodies stay in the blob until used: a precompiled function that runs on its chunk never
    /// decodes its body, one that runs on the tree-walker decodes it on its first call.
    #[test]
    fn bodies_are_deferred_and_decoded_on_demand() {
        let src = "function hot(n) { let s = 0; for (let i = 0; i < n; i++) s += i; return s; }\n\
                   function* gen() { yield 1; yield 2; }\n\
                   class A { static { globalThis.__sb = 7; } m(x = () => 3) { return x(); } }\n\
                   globalThis.__r = [hot(10), [...gen()].join(), new A().m(), __sb].join('|');\n";
        let blob: &'static [u8] =
            Box::leak(precompile(src, SourceKind::Script).unwrap().into_boxed_slice());
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
        assert_eq!(got, "45|1,2|3|7");
        let parsed = parse(blob).unwrap();
        let funcs = parsed.units[0].data.split_unit().unwrap().all_functions().unwrap();
        assert!(funcs.iter().all(|f| f.parsed_body().is_none()));
        assert!(funcs[0].body().len() == 3, "hot's body decodes on demand");
    }

    /// Stack traces of precompiled code keep their source positions: the blob carries each
    /// unit's line table (the text itself is not in it), on the tree-walker and the precompiled
    /// bytecode alike.
    #[test]
    fn precompiled_frames_keep_lines_and_columns() {
        let src = "function leaf() { return new Error('x').stack; }\n\
                   function mid() { return leaf(); }\n\
                   let s; for (let i = 0; i < 3; i++) s = mid();\n\
                   globalThis.__r = s;\n";
        let mut b = PrecompileBundle::new();
        b.add("main.js", src, SourceKind::Script).unwrap();
        let blob: &'static [u8] = Box::leak(b.finish().into_boxed_slice());
        for tier in [crate::bytecode::Tier::Interp, crate::bytecode::Tier::Bytecode] {
            let mut e = crate::Engine::new();
            e.set_tier(tier);
            e.set_tier_threshold(0);
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
                "Error: x\n    at leaf (main.js:1:26)\n    at mid (main.js:2:25)\n    \
                 at main.js:3:40",
                "{tier:?}"
            );
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
