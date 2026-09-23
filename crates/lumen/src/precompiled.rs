//! Ahead-of-time compiled JavaScript: a versioned, sectioned blob an embedder links into its
//! binary in place of the JS source (see the `lumen-aot` crate's `include_js!`), loaded with
//! [`Engine::load_precompiled`](crate::Engine::load_precompiled).
//!
//! ## What is in a blob
//! Parser output plus bytecode. Each unit (a script, or one ES module of a bundle) is its AST
//! encoded by [`snapshot::encode_stripped`]: every function body is written out in full
//! (nothing is a lazy byte range into the source) and no function keeps its source text, so
//! the blob decodes against the *empty* source. `Function.prototype.toString` of a
//! precompiled function renders the spec's NativeFunction form (`function f() { [native
//! code] }`). String literals, template strings, regex literals, identifiers and property
//! names are of course still in it — they are program data, not source text.
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
//! every chunk at load instead, so even a function's first call runs on the VM. The AST is still decoded eagerly in full — the VM needs the
//! `Function` nodes (params, flags, inner function templates) and the interpreter still
//! consults the body (hoisting, the tree-walker tier, bodies the compiler refused), so function
//! bodies are not skipped when their bytecode is present.
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
//! [`SEC_BYTECODE`] (at most one per unit). Readers skip kinds they do not know, so later tiers
//! (native code) are added as new section kinds referenced from new manifest fields.
//!
//! The manifest: `unit_count` (LEB128), then per unit `kind` (u8: 0 script, 1 module), `key`
//! (LEB128 length + UTF-8), `ast_section` (LEB128 index into the section table),
//! `bytecode_section` (LEB128 index, 0 = none); then `entry` (LEB128: unit index + 1, 0 =
//! none).
//!
//! ## Modules
//! A module unit's key is `aot:/<path>` (a path relative to the bundle root, `/`-separated).
//! Loading a blob registers every module unit with the realm; a relative specifier imported
//! from an `aot:/` module (or an `aot:/` specifier anywhere) that names a registered unit
//! resolves inside the bundle and decodes that unit's AST on first import — no host loader, no
//! filesystem. Anything else (bare specifiers, `node:` builtins, attribute imports) goes to the
//! host's module loader as usual, with the `aot:/` key as the referrer.

use std::collections::HashMap;

use crate::ast::Stmt;
use crate::interpreter::Interp;
use crate::snapshot;

/// The container format version (header + section table + manifest). Bump on any change.
pub const FORMAT_VERSION: u32 = 2;
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

/// The layout fingerprint a blob with bytecode must carry (see the header's `layout_fp`).
pub const LAYOUT_FINGERPRINT: u64 = crate::bytecode::serialize::FINGERPRINT;

pub use crate::bytecode::serialize::SectionStats;

/// The lumen version a blob must be built by (checked on load).
pub const LUMEN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The key prefix of a precompiled module.
pub const KEY_PREFIX: &str = "aot:/";

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

/// Whether a unit is a classic script or an ES module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Script,
    Module,
}

// ---- building ---------------------------------------------------------------------------------

/// One unit compiled on its own, before it is named in a bundle (a module's AST does not
/// depend on its key, so a bundler can compile first and choose the bundle root afterwards).
pub struct CompiledUnit {
    kind: SourceKind,
    ast: Vec<u8>,
    bytecode: Option<Vec<u8>>,
    stats: SectionStats,
    imports: Vec<String>,
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
        let fmt =
            |e: crate::parser::ParseError| format!("SyntaxError: {} (line {})", e.message, e.line);
        let body = crate::parser::with_eager_bodies(|| match kind {
            SourceKind::Script => crate::parser::parse_script(src, false),
            SourceKind::Module => crate::parser::parse_module(src),
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
        let (ast, funcs) = snapshot::encode_stripped_with_functions(&body);
        let (bytecode, stats) = if bytecode {
            let (bc, stats) = crate::bytecode::serialize::encode_unit(&funcs);
            (Some(bc), stats)
        } else {
            (None, SectionStats::default())
        };
        Ok(CompiledUnit {
            kind,
            ast,
            bytecode,
            stats,
            imports,
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

    pub fn kind(&self) -> SourceKind {
        self.kind
    }

    /// A module's static import / re-export specifiers, in source order, deduplicated
    /// (attribute imports excluded). Empty for a script.
    pub fn imports(&self) -> &[String] {
        &self.imports
    }

    /// The encoded AST's size in bytes (the bytecode section is [`CompiledUnit::bytecode_len`]).
    pub fn len(&self) -> usize {
        self.ast.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ast.is_empty()
    }
}

/// Builds a multi-unit blob: any number of scripts and ES modules plus an optional entry
/// module. This is what `include_js!` and `lumen_aot::build` drive; embedders with their own
/// bundling needs can use it directly (e.g. from a `build.rs`).
#[derive(Default)]
pub struct PrecompileBundle {
    units: Vec<BundleUnit>,
    entry: Option<usize>,
    no_bytecode: bool,
}

struct BundleUnit {
    kind: SourceKind,
    key: String,
    ast: Vec<u8>,
    bytecode: Option<Vec<u8>>,
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

    /// Compile and add one unit (see [`PrecompileBundle::add_compiled`]); returns the module's
    /// static import specifiers (relative ones are the caller's to resolve and add).
    pub fn add(&mut self, path: &str, src: &str, kind: SourceKind) -> Result<Vec<String>, String> {
        let unit = CompiledUnit::compile_with(src, kind, !self.no_bytecode)
            .map_err(|e| format!("{path}: {e}"))?;
        let imports = unit.imports.clone();
        self.add_compiled(path, unit)?;
        Ok(imports)
    }

    /// Add a compiled unit. `path` names it: for a module, its bundle-relative `/`-separated
    /// path (`lib/a.js`, keyed `aot:/lib/a.js`); for a script, a label. Scripts run in the
    /// order they are added.
    pub fn add_compiled(&mut self, path: &str, unit: CompiledUnit) -> Result<(), String> {
        let key = match unit.kind {
            SourceKind::Module => module_key(path),
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
        });
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
        // Section order: the manifest, every unit's AST, then the bytecode sections.
        let n = self.units.len();
        let mut next_bc = 1 + n;
        let mut manifest = Vec::new();
        uv(&mut manifest, n as u64);
        for (i, u) in self.units.iter().enumerate() {
            manifest.push(match u.kind {
                SourceKind::Script => 0,
                SourceKind::Module => 1,
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
        }
        uv(&mut manifest, self.entry.map_or(0, |e| e as u64 + 1));

        let mut sections: Vec<(u32, &[u8])> = vec![(SEC_MANIFEST, &manifest)];
        for u in &self.units {
            sections.push((SEC_AST, &u.ast));
        }
        for u in &self.units {
            if let Some(bc) = &u.bytecode {
                sections.push((SEC_BYTECODE, bc));
            }
        }
        let has_bytecode = sections.iter().any(|(k, _)| *k == SEC_BYTECODE);
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
        for (kind, data) in &sections {
            out.extend_from_slice(&kind.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&offset.to_le_bytes());
            out.extend_from_slice(&(data.len() as u64).to_le_bytes());
            offset += data.len() as u64;
        }
        for (_, data) in &sections {
            out.extend_from_slice(data);
        }
        out
    }
}

/// Precompile a single script or module (a module is keyed `aot:/main.js` and is the entry).
pub fn precompile(src: &str, kind: SourceKind) -> Result<Vec<u8>, String> {
    let mut b = PrecompileBundle::new();
    match kind {
        SourceKind::Script => {
            b.add("main.js", src, kind)?;
        }
        SourceKind::Module => {
            b.add("main.js", src, kind)?;
            b.set_entry("main.js")?;
        }
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

// ---- reading ----------------------------------------------------------------------------------

/// One unit of a parsed blob: its kind, key, and AST (and bytecode) section bytes.
pub(crate) struct Unit {
    pub kind: SourceKind,
    pub key: String,
    pub ast: &'static [u8],
    pub bytecode: Option<&'static [u8]>,
}

impl Unit {
    /// Decode the unit's AST and attach its precompiled bytecode.
    pub fn decode(&self) -> Result<Vec<Stmt>, String> {
        decode_unit(self.ast, self.bytecode)
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
        let (kind, off, len) = (
            read_u32(bytes, at),
            read_u64(bytes, at + 8),
            read_u64(bytes, at + 16),
        );
        let end = off.checked_add(len).filter(|&e| e <= bytes.len() as u64);
        let Some(end) = end else {
            return err("section out of bounds");
        };
        sections.push((kind, &bytes[off as usize..end as usize]));
    }
    let manifest = sections
        .iter()
        .find(|(k, _)| *k == SEC_MANIFEST)
        .map(|(_, d)| *d)
        .ok_or("precompiled: no manifest")?;
    read_manifest(manifest, &sections)
}

fn read_manifest(m: &[u8], sections: &[(u32, &'static [u8])]) -> Result<Parsed, String> {
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
            k => return Err(format!("precompiled: bad unit kind {k}")),
        };
        let key = c.str()?;
        let sec = c.uv()? as usize;
        let ast = match sections.get(sec) {
            Some((SEC_AST, d)) => *d,
            _ => return Err(format!("precompiled: {key}: bad AST section {sec}")),
        };
        let bytecode = match c.uv()? as usize {
            0 => None,
            sec => match sections.get(sec) {
                Some((SEC_BYTECODE, d)) => Some(*d),
                _ => return Err(format!("precompiled: {key}: bad bytecode section {sec}")),
            },
        };
        units.push(Unit {
            kind,
            key,
            ast,
            bytecode,
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
) -> Result<Vec<Stmt>, String> {
    let Some(bc) = bytecode else {
        return snapshot::decode(ast, "");
    };
    static EAGER: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let eager = *EAGER.get_or_init(|| std::env::var_os("LUMEN_AOT_EAGER").is_some());
    let (body, funcs) = snapshot::decode_with_functions(ast, "")?;
    crate::bytecode::serialize::attach_unit(bc, &funcs, eager)?;
    Ok(body)
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
        let (_body, funcs) = snapshot::decode_with_functions(u.ast, "")?;
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

// ---- the realm's module table -----------------------------------------------------------------

/// Module units registered with a realm (kept in its host state), by key.
#[derive(Default)]
pub(crate) struct PrecompiledModules {
    units: HashMap<String, (&'static [u8], Option<&'static [u8]>)>,
}

pub(crate) fn register_modules(interp: &mut Interp, parsed: &Parsed) {
    if !interp.host_state.has::<PrecompiledModules>() {
        interp.host_state.put(PrecompiledModules::default());
    }
    let table = interp.host_state.get_mut::<PrecompiledModules>().unwrap();
    for u in parsed.units.iter().filter(|u| u.kind == SourceKind::Module) {
        table.units.insert(u.key.clone(), (u.ast, u.bytecode));
    }
}

/// The decoded AST of `key` if it is a registered precompiled module (`None`: not one).
pub(crate) fn module_body(interp: &Interp, key: &str) -> Option<Result<Vec<Stmt>, String>> {
    if !key.starts_with(KEY_PREFIX) {
        return None;
    }
    let (ast, bytecode) = *interp
        .host_state
        .get::<PrecompiledModules>()?
        .units
        .get(key)?;
    Some(decode_unit(ast, bytecode).map_err(|e| format!("{key}: {e}")))
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
    let key = if let Some(path) = specifier.strip_prefix(KEY_PREFIX) {
        normalize(path)?
    } else if (specifier.starts_with("./") || specifier.starts_with("../"))
        && referrer.starts_with(KEY_PREFIX)
    {
        let dir = match referrer.rfind('/') {
            Some(i) if i >= KEY_PREFIX.len() => &referrer[KEY_PREFIX.len()..=i],
            _ => "",
        };
        normalize(&format!("{dir}{specifier}"))?
    } else {
        return None;
    };
    // Exact, then with `.js`, then `/index.js` — the order the bundler resolved it in.
    let key = format!("{KEY_PREFIX}{key}");
    [key.clone(), format!("{key}.js"), format!("{key}/index.js")]
        .into_iter()
        .find(|k| table.units.contains_key(k))
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
