//! The ES-module loader for the runtime: resolves an `import` specifier to a canonical key +
//! source, which the engine's `eval_module` consults for every dependency. The engine already
//! runs the module graph (linking, top-level await); this is just resolution + the
//! CommonJS/builtin interop bridge.
//!
//! Resolution:
//! - `node:x` or a bare builtin name -> a synthetic re-export module (precomputed in JS, so
//!   named imports like `import { readFileSync } from "node:fs"` work).
//! - relative/absolute -> a file on disk (`.mjs`/`.js`/`.json`/`.cjs`, directory index,
//!   `package.json` `main`). `.js`/`.mjs` load as real ESM; `.json` and `.cjs` get a synthetic
//!   default-export wrapper (`.cjs` bridges through the global CommonJS `require`).
//! - bare package -> the `node_modules` walk; an ESM entry (`.mjs`, or `package.json`
//!   `type:module` / `exports` import condition / `module`) loads as real ESM, else it's
//!   default-only CJS interop via `require`. A bare subpath (`hono/logger`) resolves through
//!   its package's `exports` map first (`"./logger"` -> its `import`/`default` target); a
//!   literal file is considered only when there is no map.
//!
//! - `aot:/…` referrer (a module of a precompiled blob) -> the engine resolves the blob's own
//!   units and bundled packages first; only what the blob lacks arrives here, and a bare
//!   package then resolves from the current directory's `node_modules`.
//!
//! Deferred (documented, not silently wrong): named imports from a CommonJS *package* (Node
//! uses additional source static-analysis patterns), import maps.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lumen_host::sysfs::PathExt as _;

/// The named exports of each builtin (`"node:fs"` -> `"appendFile appendFileSync …"`, from the
/// node glue's `esm_exports.js`). A builtin's synthetic ESM source is built from its list when it
/// is first imported ([`builtin_source`]), not for every builtin at startup.
pub struct BuiltinModules(pub HashMap<String, String>);

/// The synthetic ESM form of a builtin: the module object as the default export, and each listed
/// name that is a plain identifier as a named export read from it at import time.
pub fn builtin_source(name: &str, exports: &str) -> String {
    // Builtin names (`fs/promises`) and identifiers need no escaping inside a string literal.
    let mut src =
        format!("const __m = globalThis.__esmBuiltin(\"{name}\");\nexport default __m;\n");
    for k in exports.split(' ') {
        let mut chars = k.chars();
        let ident = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
        if ident && k != "default" {
            src.push_str(&format!("export const {k} = __m[\"{k}\"];\n"));
        }
    }
    src
}

const EXTENSIONS: [&str; 8] = [
    ".mjs", ".js", ".jsx", ".json", ".cjs", ".ts", ".mts", ".cts",
];

/// Build the loader closure `eval_module` wants. It owns everything (`'static`); the engine
/// caches results by the canonical key we return, so returning a stable realpath per file is
/// what dedupes shared dependencies.
pub fn make_loader(
    builtins: BuiltinModules,
) -> impl Fn(&str, &str, Option<&str>) -> Option<(String, String)> {
    make_cached_loader(builtins).0
}

/// [`make_loader`], plus a handle on its [`LoaderCache`] so the caller can drop the cached
/// sources once the module graph they were fetched for has loaded.
pub fn make_cached_loader(
    builtins: BuiltinModules,
) -> (
    impl Fn(&str, &str, Option<&str>) -> Option<(String, String)>,
    std::rc::Rc<LoaderCache>,
) {
    let cache = std::rc::Rc::new(LoaderCache::default());
    let handle = std::rc::Rc::clone(&cache);
    let loader = move |specifier: &str, referrer: &str, attr_type: Option<&str>| {
        cache.resolve(specifier, referrer, attr_type, |s, r, a| {
            resolve(s, r, &builtins, a)
        })
    };
    (loader, handle)
}

/// Memoized resolution for one loader. The engine asks the loader for every import clause of
/// every module it parses — also for dependencies it has already loaded, whose source it then
/// ignores — so a module graph repeats the same resolutions many times (Puppeteer: ~630 loads for
/// ~170 modules), each one filesystem probes, `package.json` reads and a file read. Resolutions
/// are cached by (specifier, referrer directory, attribute). Redundant source copies are bounded
/// by bytes/entries; evaluated modules and their exports remain cached by the engine. The file
/// system is assumed not to change under a loading module graph
/// (Node's resolver caches the same way). A failed resolution is not cached.
#[derive(Default)]
pub struct LoaderCache {
    /// (specifier, referrer directory, attribute type) -> resolved key.
    keys: std::cell::RefCell<HashMap<(String, String, Option<String>), String>>,
    /// (resolved key, attribute type) -> source.
    sources: std::cell::RefCell<SourceCache>,
    /// Package types and canonical directories, for the resolver's helpers.
    memo: std::cell::RefCell<FsMemo>,
    /// Numeric-only opt-in memory checkpoints while a graph is still loading.
    diagnostic_lookups: std::cell::Cell<usize>,
}

const SOURCE_CACHE_BYTES: usize = 8 * 1024 * 1024;
const SOURCE_CACHE_ENTRIES: usize = 512;
type SourceKey = (String, Option<String>);

/// Dynamic imports can keep a loader alive for an entire account lifetime. Cache only
/// 8 MiB of redundant source/key bytes plus at most 512 small map records, not the graph.
#[derive(Default)]
struct SourceCache {
    entries: HashMap<SourceKey, (String, u64)>,
    bytes: usize,
    clock: u64,
}
impl SourceCache {
    fn weight(key: &SourceKey, source: &str) -> usize {
        key.0
            .len()
            .saturating_add(key.1.as_ref().map_or(0, String::len))
            .saturating_add(source.len())
    }
    fn get(&mut self, key: &SourceKey) -> Option<String> {
        let entry = self.entries.get_mut(key)?;
        self.clock = self.clock.saturating_add(1);
        entry.1 = self.clock;
        Some(entry.0.clone())
    }
    fn insert(&mut self, key: SourceKey, source: &str) {
        if let Some((old, _)) = self.entries.remove(&key) {
            self.bytes -= Self::weight(&key, &old);
        }
        let weight = Self::weight(&key, source);
        if weight > SOURCE_CACHE_BYTES {
            return;
        }
        while self.entries.len() >= SOURCE_CACHE_ENTRIES
            || self.bytes.saturating_add(weight) > SOURCE_CACHE_BYTES
        {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, (_, time))| *time)
                .map(|(key, _)| key.clone());
            let Some(oldest) = oldest else {
                break;
            };
            let (old, _) = self.entries.remove(&oldest).expect("cached source");
            self.bytes -= Self::weight(&oldest, &old);
        }
        self.clock = self.clock.saturating_add(1);
        self.bytes += weight;
        self.entries.insert(key, (source.to_owned(), self.clock));
    }
    fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }
}

/// Filesystem facts the resolver re-derives for nearly every module: the `"type"` of each
/// directory's `package.json` (found by walking up from every resolved file) and each
/// directory's canonical path. A [`LoaderCache`] keeps one and installs it in [`MEMO`] while it
/// resolves, since the resolver's helpers are free functions; without one (no loader resolving)
/// they query the filesystem directly.
#[derive(Default)]
struct FsMemo {
    /// Directory -> `None` when it has no package.json, else that file's `"type"` field.
    pkg_type: HashMap<PathBuf, Option<Option<String>>>,
    /// Directory -> its canonical path.
    canon_dirs: HashMap<PathBuf, PathBuf>,
}

thread_local! {
    static MEMO: std::cell::RefCell<Option<FsMemo>> = const { std::cell::RefCell::new(None) };
}

/// The `package.json` in `dir`: `None` when there is none, else its `"type"` field.
fn dir_package_type(dir: &Path) -> Option<Option<String>> {
    let lookup = || {
        let pkg = dir.join("package.json");
        if !pkg.fs_is_file() {
            return None;
        }
        Some(
            lumen_host::sysfs::read_to_string(pkg)
                .ok()
                .and_then(|t| crate::package_type_from_json(&t)),
        )
    };
    MEMO.with(|m| match m.borrow_mut().as_mut() {
        Some(memo) => memo
            .pkg_type
            .entry(dir.to_path_buf())
            .or_insert_with(lookup)
            .clone(),
        None => lookup(),
    })
}

/// A resolved file's module key: its canonical path. Under a [`FsMemo`] that is the file's
/// directory canonicalized once per directory, plus its name — unless the file itself is a
/// symlink, which is resolved in full like everything else without a memo.
fn canonical_key(file: &Path) -> String {
    let memoized = MEMO.with(|m| m.borrow().is_some());
    let via_dir = || -> Option<PathBuf> {
        let (dir, name) = (file.parent()?, file.file_name()?);
        if dir.as_os_str().is_empty()
            || !lumen_host::sysfs::exists_not_symlink(file)
        {
            return None;
        }
        MEMO.with(|m| {
            let mut m = m.borrow_mut();
            let memo = m.as_mut()?;
            if let Some(c) = memo.canon_dirs.get(dir) {
                return Some(c.join(name));
            }
            let c = lumen_host::canonicalize(dir).ok()?;
            memo.canon_dirs.insert(dir.to_path_buf(), c.clone());
            Some(c.join(name))
        })
    };
    memoized
        .then(via_dir)
        .flatten()
        .or_else(|| lumen_host::canonicalize(file).ok())
        .unwrap_or_else(|| file.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

impl LoaderCache {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        attr_type: Option<&str>,
        uncached: impl FnOnce(&str, &str, Option<&str>) -> Option<(String, String)>,
    ) -> Option<(String, String)> {
        if lumen::memstats::enabled() {
            let lookups = self.diagnostic_lookups.get().wrapping_add(1);
            self.diagnostic_lookups.set(lookups);
            if lookups % 4096 == 0 {
                let keys = self.keys.borrow();
                let sources = self.sources.borrow();
                let memo = self.memo.borrow();
                let key_bytes: usize = keys
                    .iter()
                    .map(|((specifier, base, attr), resolved)| {
                        specifier.len()
                            + base.len()
                            + attr.as_ref().map_or(0, String::len)
                            + resolved.len()
                    })
                    .sum();
                let package_bytes: usize = memo
                    .pkg_type
                    .iter()
                    .map(|(path, kind)| {
                        path.as_os_str().as_encoded_bytes().len()
                            + kind
                                .as_ref()
                                .and_then(Option::as_ref)
                                .map_or(0, String::len)
                    })
                    .sum();
                let canonical_bytes: usize = memo
                    .canon_dirs
                    .iter()
                    .map(|(path, canonical)| {
                        path.as_os_str().as_encoded_bytes().len()
                            + canonical.as_os_str().as_encoded_bytes().len()
                    })
                    .sum();
                eprintln!(
                    "[loader-memory] lookups={lookups} resolutions={} key_payload={key_bytes} sources={} source_payload={} packages={} package_payload={package_bytes} canonical_dirs={} canonical_payload={canonical_bytes}",
                    keys.len(),
                    sources.entries.len(),
                    sources.bytes,
                    memo.pkg_type.len(),
                    memo.canon_dirs.len()
                );
            }
        }
        // An `aot:` referrer resolves against the current directory, not its own location.
        let base = if referrer.starts_with("aot:") {
            None
        } else {
            let r = strip_file_scheme(referrer);
            Some(
                Path::new(r.as_ref())
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            )
        };
        let attr = attr_type.map(str::to_owned);
        if let Some(base) = &base {
            let key = (specifier.to_owned(), base.clone(), attr.clone());
            if let Some(resolved) = self.keys.borrow().get(&key) {
                let sk = (resolved.clone(), attr.clone());
                if let Some(src) = self.sources.borrow_mut().get(&sk) {
                    return Some((resolved.clone(), src));
                }
            }
        }
        let memo = std::mem::take(&mut *self.memo.borrow_mut());
        let outer = MEMO.with(|m| m.replace(Some(memo)));
        let result = uncached(specifier, referrer, attr_type);
        let memo = MEMO.with(|m| m.replace(outer)).unwrap_or_default();
        *self.memo.borrow_mut() = memo;
        let (resolved, src) = result?;
        if let Some(base) = base {
            self.keys
                .borrow_mut()
                .insert((specifier.to_owned(), base, attr.clone()), resolved.clone());
            self.sources
                .borrow_mut()
                .insert((resolved.clone(), attr), &src);
        }
        Some((resolved, src))
    }

    /// Drop the cached sources (resolutions are kept; a later import of a forgotten module
    /// reads its file again).
    pub fn forget_sources(&self) {
        self.sources.borrow_mut().clear();
    }
}

fn resolve(
    specifier: &str,
    referrer: &str,
    builtins: &BuiltinModules,
    attr_type: Option<&str>,
) -> Option<(String, String)> {
    // Builtins: `node:fs` or a bare `fs`/`path`/… name.
    let bare = specifier.strip_prefix("node:").unwrap_or(specifier);
    let key = format!("node:{bare}");
    if let Some(exports) = builtins.0.get(&key) {
        let src = builtin_source(bare, exports);
        return Some((key, src));
    }

    // A module of an ahead-of-time blob (`aot:/…`, see `Runtime::run_precompiled`). The engine
    // already resolved everything the blob holds (its own units, the packages it bundled)
    // before asking here, so what is left is not in the blob: a relative path has nothing on
    // disk to be relative to, and a bare package is looked up from the current directory's
    // `node_modules`, as a program started there would.
    if referrer.starts_with("aot:") {
        if attr_type.is_some()
            || specifier.starts_with("./")
            || specifier.starts_with("../")
            || specifier.starts_with("aot:")
        {
            return None;
        }
        if Path::new(specifier).is_absolute() || specifier.starts_with("file://") {
            return resolve(specifier, "", builtins, attr_type);
        }
        let cwd = lumen_host::sysfs::current_dir().ok()?;
        let (file, is_esm_pkg) = resolve_node_modules(specifier, &cwd)?;
        return load_as_module(&file, !is_esm_pkg);
    }

    // A dynamic import's referrer is `import.meta.url`, a `file://` URL — reduce it (and a
    // `file://` specifier) to a plain path for the filesystem resolver.
    let referrer_path = strip_file_scheme(referrer);
    let specifier_path = strip_file_scheme(specifier);
    let referrer: &str = &referrer_path;
    let specifier: &str = &specifier_path;

    // Package-private imports belong to the nearest package scope, not node_modules.
    if specifier.starts_with('#') {
        let from = Path::new(referrer).parent()?;
        let file = resolve_package_import(specifier, from)?;
        return load_as_module(&file, !file_is_esm(&file));
    }

    // An absolute filesystem path (`C:\x\y.js` on Windows — which `starts_with('/')` misses and
    // the bare-package walk would reject — or `/x/y.js`) names the file directly; the referrer
    // plays no part. `require(esm)` hands its already-resolved filename in this form.
    if attr_type.is_none() && Path::new(specifier).is_absolute() {
        let file = resolve_file_or_dir(&normalize(Path::new(specifier)))?;
        return load_as_module(&file, !file_is_esm(&file));
    }

    // A `with { type: "json" | "text" | "bytes" }` import wants the file's RAW contents — the
    // engine synthesizes the wrapper module itself. No CJS/ESM classification, no JSX transform:
    // the attribute defines the module type, whatever the extension says (importing a `.js` file
    // as text is a spec-tested case).
    if matches!(attr_type, Some("json" | "text" | "bytes")) {
        let file = if specifier.starts_with("./")
            || specifier.starts_with("../")
            || specifier.starts_with('/')
        {
            let base = Path::new(referrer).parent()?.join(specifier);
            resolve_file_or_dir(&normalize(&base))?
        } else {
            resolve_node_modules(specifier, Path::new(referrer).parent()?)?.0
        };
        let key = canonical_key(&file);
        return Some((key, read_raw(&file, attr_type)?));
    }

    if specifier.starts_with("./") || specifier.starts_with("../") || specifier.starts_with('/') {
        let base = Path::new(referrer).parent()?.join(specifier);
        let file = resolve_file_or_dir(&normalize(&base))?;
        // A relative `.js` file is CommonJS unless its nearest package.json is `type: module` —
        // Node's own rule. (A misresolved-as-ESM CJS file would expose no named exports.)
        let cjs = !file_is_esm(&file);
        return load_as_module(&file, cjs);
    }

    // Bare package name.
    let from = Path::new(referrer).parent()?;
    let (file, is_esm_pkg) = resolve_node_modules(specifier, from)?;
    load_as_module(&file, !is_esm_pkg)
}

/// Reduce a `file://` URL to a filesystem path: `file:///C:/x` -> `C:/x` on Windows (the drive
/// letter follows the URL path's leading slash), `file:///x` -> `/x` elsewhere. `%XX` escapes are
/// decoded (a path with a space arrives as `%20`). Anything else is returned unchanged.
fn strip_file_scheme(s: &str) -> std::borrow::Cow<'_, str> {
    let Some(rest) = s.strip_prefix("file://") else {
        return std::borrow::Cow::Borrowed(s);
    };
    // `file://localhost/x` is the same as `file:///x`.
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let bytes = rest.as_bytes();
    let rest = if cfg!(windows)
        && bytes.len() >= 3
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':'
    {
        &rest[1..]
    } else {
        rest
    };
    if !rest.contains('%') {
        return std::borrow::Cow::Owned(rest.to_string());
    }
    let mut out = Vec::with_capacity(rest.len());
    let b = rest.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    std::borrow::Cow::Owned(String::from_utf8_lossy(&out).into_owned())
}

/// Escape `text` as a JavaScript double-quoted string literal (for synthesized module source).
fn js_string_literal(text: &str) -> String {
    let mut lit = String::with_capacity(text.len() + 2);
    lit.push('"');
    for c in text.chars() {
        match c {
            '"' => lit.push_str("\\\""),
            '\\' => lit.push_str("\\\\"),
            '\n' => lit.push_str("\\n"),
            '\r' => lit.push_str("\\r"),
            '\u{2028}' => lit.push_str("\\u2028"),
            '\u{2029}' => lit.push_str("\\u2029"),
            c if (c as u32) < 0x20 => {
                lit.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => lit.push(c),
        }
    }
    lit.push('"');
    lit
}

/// Read a file for an attribute import. `text`/`json` decode as UTF-8 the way the web platform's
/// "UTF-8 decode" does — invalid sequences become U+FFFD and a leading BOM is stripped. `bytes`
/// must round-trip exactly, so non-UTF-8 content is latin-1-decoded (one char per byte; the
/// engine re-extracts the original bytes when it builds the `Uint8Array`).
fn read_raw(file: &Path, attr_type: Option<&str>) -> Option<String> {
    let bytes = lumen_host::sysfs::read(file).ok()?;
    Some(match attr_type {
        Some("bytes") => match String::from_utf8(bytes) {
            Ok(t) => t,
            Err(e) => e.into_bytes().iter().map(|&b| b as char).collect(),
        },
        _ => {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            text.strip_prefix('\u{feff}')
                .map(str::to_owned)
                .unwrap_or(text)
        }
    })
}

/// A resolved file (existing path, extension probe, or directory index/main). `None` if
/// nothing matches.
fn resolve_file_or_dir(base: &Path) -> Option<PathBuf> {
    if let Some(f) = resolve_file(base) {
        return Some(f);
    }
    if base.fs_is_dir() {
        return resolve_directory(base);
    }
    None
}

fn resolve_file(base: &Path) -> Option<PathBuf> {
    if base.fs_is_file() {
        return Some(base.to_path_buf());
    }
    for ext in EXTENSIONS {
        let mut s = base.as_os_str().to_os_string();
        s.push(ext);
        let candidate = PathBuf::from(s);
        if candidate.fs_is_file() {
            return Some(candidate);
        }
    }
    None
}

fn resolve_directory(dir: &Path) -> Option<PathBuf> {
    let pkg = dir.join("package.json");
    if pkg.fs_is_file() {
        if let Ok(text) = lumen_host::sysfs::read_to_string(&pkg) {
            if let Some(entry) = pkg_entry(&text) {
                let target = normalize(&dir.join(entry));
                if let Some(f) =
                    resolve_file(&target).or_else(|| resolve_file(&target.join("index")))
                {
                    return Some(f);
                }
            }
            // An explicit exports map owns the entry even when blocked, unmatched or absent
            // on disk. Legacy index fallback must not reopen that package root.
            if crate::tsconfig::parse_jsonc(&text)
                .ok()
                .is_some_and(|json| json.get("exports").is_some())
            {
                return None;
            }
        }
    }
    resolve_file(&dir.join("index"))
}

/// The `node_modules` walk. Returns the resolved file and whether the package is ESM (so the
/// caller loads real ESM vs. a CJS default-export wrapper).
fn resolve_node_modules(name: &str, start: &Path) -> Option<(PathBuf, bool)> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if d.file_name().is_some_and(|n| n == "node_modules") {
            dir = d.parent();
            continue;
        }
        // An exports map owns every package subpath, including one with a legacy file
        // of the same name. Check it before probing physical files/directories.
        if let Some(subpath) = package_subpath(name) {
            let pkg_dir = package_dir(d, name);
            if let Ok(text) = lumen_host::sysfs::read_to_string(pkg_dir.join("package.json")) {
                if crate::tsconfig::parse_jsonc(&text).ok().is_some_and(|json| json.get("exports").is_some()) {
                    let entry = exports_subpath(&text, &subpath)?;
                    let mapped = normalize(&pkg_dir.join(entry));
                    if !mapped.fs_is_file() { return None; }
                    let esm = file_is_esm(&mapped);
                    return Some((mapped, esm));
                }
            }
        }
        let target = d.join("node_modules").join(name);
        if target.fs_is_dir() {
            if let Some(f) = resolve_directory(&target) {
                // The resolved file's own nearest package.json decides ESM-ness — a package can
                // ship an ESM build under `dist/es/` with its own `{"type":"module"}`.
                let esm = file_is_esm(&f);
                return Some((f, esm));
            }
            if package_subpath(name).is_none() && target.join("package.json").fs_is_file() {
                return None;
            }
        }
        // A bare specifier can also point straight at a file (`pkg/sub.js`).
        if let Some(f) = resolve_file(&target) {
            let esm = file_is_esm(&f);
            return Some((f, esm));
        }
        dir = d.parent();
    }
    None
}

fn resolve_package_import(name: &str, start: &Path) -> Option<PathBuf> {
    use crate::tsconfig::parse_jsonc;
    if name == "#" || name.starts_with("#/") {
        return None;
    }
    let mut dir = Some(start);
    while let Some(scope) = dir {
        let manifest = scope.join("package.json");
        if manifest.fs_is_file() {
            let json = parse_jsonc(&lumen_host::sysfs::read_to_string(manifest).ok()?).ok()?;
            let entry = json.get("imports")?.get(name)?;
            let entry = package_esm_target(entry).target()?;
            // Relative exact targets cover package-private ESM dependencies such as Chalk.
            // Do not reinterpret unmapped names as arbitrary filesystem paths.
            if !entry.starts_with("./")
                || entry
                    .split('/')
                    .any(|part| part == ".." || part == "node_modules")
            {
                return None;
            }
            return resolve_file(&scope.join(entry));
        }
        if scope.file_name().is_some_and(|name| name == "node_modules") {
            return None;
        }
        dir = scope.parent();
    }
    None
}

/// The package root for a bare specifier under `<parent>/node_modules`: the first path segment,
/// or the first two for a scoped `@scope/name`.
fn package_dir(parent: &Path, name: &str) -> PathBuf {
    let mut parts = name.split('/');
    let mut pkg = String::new();
    if let Some(first) = parts.next() {
        pkg.push_str(first);
        if first.starts_with('@') {
            if let Some(scope_name) = parts.next() {
                pkg.push('/');
                pkg.push_str(scope_name);
            }
        }
    }
    parent.join("node_modules").join(pkg)
}

/// Turn a resolved file into `(canonical_key, source)`. `.mjs`/`.js` are real ESM; `.json`
/// and (when `cjs_default`) `.cjs`/CJS packages get a synthetic default-export wrapper.
fn load_as_module(file: &Path, cjs_default: bool) -> Option<(String, String)> {
    let key = canonical_key(file);
    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "json" => {
            // Route through JSON.parse rather than embedding the text as an expression: an
            // object literal would give `"__proto__"` keys prototype-SETTING semantics (a
            // pollution vector), where JSON.parse creates a plain own data property — the same
            // semantics as a `with { type: "json" }` import.
            let text = lumen_host::sysfs::read_to_string(file).ok()?;
            Some((
                key,
                format!("export default JSON.parse({});", js_string_literal(&text)),
            ))
        }
        // `.mjs` is always ESM regardless of package type; `.cjs` is always CommonJS.
        "mjs" => Some((key.clone(), lumen_host::sysfs::read_to_string(file).ok()?)),
        // `.jsx` is JSX-over-ESM: transpile to plain JS, then load as a module.
        "jsx" => {
            let text = lumen_host::sysfs::read_to_string(file).ok()?;
            let js = crate::jsx::transform(&text).unwrap_or(text);
            Some((key, js))
        }
        "cjs" => Some((key.clone(), cjs_wrapper(&key, source_of(file)))),
        // TypeScript: the engine parses it itself by the key's extension (Node's strip-only
        // semantics, every offset kept; `require` of a `.cts`/CJS `.ts` goes through
        // node:module, which compiles it the same way). Syntax it cannot run throws Node's
        // SyntaxError when the module is parsed.
        "mts" => Some((key, lumen_host::sysfs::read_to_string(file).ok()?)),
        "cts" => Some((key.clone(), cjs_wrapper(&key, source_of(file)))),
        _ if cjs_default => {
            let text = source_of(file);
            // Node's syntax detection: a `.js`/`.ts` file outside any package "type" that only
            // parses as a module (it has `import`/`export`) is ESM after all.
            if (ext == "js" || ext == "ts")
                && package_type_of(file).is_none()
                && detect_module_syntax(&text, ext == "ts")
            {
                return Some((key, text));
            }
            Some((key.clone(), cjs_wrapper(&key, text)))
        }
        _ => {
            let text = lumen_host::sysfs::read_to_string(file).ok()?;
            Some((key, text))
        }
    }
}

/// Whether a resolved file loads as ESM: `.mjs`/`.mts` always, `.cjs`/`.cts` never, and
/// `.js`/`.ts` per the nearest enclosing `package.json` `"type"` (absent ⇒ CommonJS, Node's
/// default).
pub(crate) fn file_is_esm(file: &Path) -> bool {
    match file.extension().and_then(|e| e.to_str()) {
        Some("mjs" | "mts") => true,
        Some("cjs" | "cts") => false,
        _ => {
            let mut dir = file.parent();
            while let Some(d) = dir {
                if let Some(ty) = dir_package_type(d) {
                    return ty.as_deref() == Some("module");
                }
                dir = d.parent();
            }
            false
        }
    }
}

/// The nearest enclosing package.json's `"type"` field (`None` when absent — or no package).
fn package_type_of(file: &Path) -> Option<String> {
    let mut dir = file.parent();
    while let Some(d) = dir {
        if let Some(ty) = dir_package_type(d) {
            return ty;
        }
        dir = d.parent();
    }
    None
}

/// Whether `src` is ESM by Node's detection rule: it has module syntax and does not parse as a
/// CommonJS body. The textual pre-check — a line that starts with an `import`/`export`
/// declaration or uses `import.meta` — keeps ordinary CommonJS to no extra parse at all; only a
/// candidate is test-parsed as a script. (A file that is neither is left to the module parser,
/// which reports its syntax error.)
fn detect_module_syntax(src: &str, ts: bool) -> bool {
    let candidate = src.lines().any(|line| {
        let l = line.trim_start();
        let after = |kw: &str| l.strip_prefix(kw).and_then(|r| r.chars().next());
        matches!(
            after("import"),
            Some(' ' | '\t' | '{' | '*' | '"' | '\'' | '.')
        ) || matches!(after("export"), Some(' ' | '\t' | '{' | '*'))
    });
    candidate
        && if ts {
            !lumen::typescript::parses_as_commonjs(src)
        } else {
            lumen::compile_snapshot(src).is_err()
        }
}

/// Read a file's source (empty on failure — the wrapper still yields a working default export).
fn source_of(file: &Path) -> String {
    lumen_host::sysfs::read_to_string(file).unwrap_or_default()
}

/// A synthetic ESM module bridging a CommonJS file: `require(path)`'s result is the default
/// export, and each export name statically discovered in the source becomes a live named export.
/// This is the cjs-module-lexer interop that lets `import { x } from './cjs-file'` link.
fn cjs_wrapper(abs_path: &str, source: String) -> String {
    let mut out = format!(
        "const __m = globalThis.require({});\nexport default __m;\n",
        js_string(abs_path)
    );
    let mut names = Vec::new();
    let mut seen = std::collections::HashSet::new();
    collect_cjs_exports(&source, Path::new(abs_path), 0, &mut names, &mut seen);
    for name in names {
        // `export const NAME = __m["NAME"];` — a live binding onto the CJS export (undefined if
        // the static scan over-approximated, which is harmless).
        out.push_str(&format!(
            "export const {name} = __m[{}];\n",
            js_string(&name)
        ));
    }
    out
}

/// Discover a CJS module's export names, following `module.exports = require('./x')` /
/// `Object.assign(module.exports, require('./x'))` re-exports transitively (bounded depth) — the
/// pattern that indirection files like `react-dom/server.js` use. `file` is the module being
/// scanned, so relative re-export targets can be resolved and read.
fn collect_cjs_exports(
    src: &str,
    file: &Path,
    depth: u32,
    names: &mut Vec<String>,
    seen: &mut std::collections::HashSet<String>,
) {
    for name in scan_cjs_exports(src) {
        add_export(names, seen, &name);
    }
    if depth >= 4 {
        return; // guard against cycles / pathological chains
    }
    for spec in reexport_requires(src) {
        if !spec.starts_with('.') {
            continue; // only follow relative re-exports (a bare package is its own module)
        }
        if let Some(target) = file
            .parent()
            .and_then(|d| resolve_relative_cjs(&d.join(&spec)))
        {
            if let Ok(sub) = lumen_host::sysfs::read_to_string(&target) {
                collect_cjs_exports(&sub, &target, depth + 1, names, seen);
            }
        }
    }
}

/// Resolve a relative `require` target to a file (exact, then `.js`/`.cjs`/`.json` *appended*,
/// then `index.js`), for re-export following. Extensions are appended, not substituted, so
/// `require('./server.node')` resolves to `server.node.js` rather than `server.js`.
fn resolve_relative_cjs(base: &Path) -> Option<PathBuf> {
    let base = normalize(base);
    if base.fs_is_file() {
        return Some(base);
    }
    for ext in [".js", ".cjs", ".json"] {
        let mut s = base.as_os_str().to_os_string();
        s.push(ext);
        let cand = PathBuf::from(s);
        if cand.fs_is_file() {
            return Some(cand);
        }
    }
    let index = base.join("index.js");
    index.fs_is_file().then_some(index)
}

/// The relative specifiers a module re-exports wholesale: `module.exports = require('X')` and
/// `Object.assign(module.exports, require('X'))`.
fn reexport_requires(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, _) in src.match_indices("require(") {
        // Look back for a `module.exports =` or `Object.assign(module.exports,` just before.
        let before = src[..i].trim_end();
        let is_reexport = before.ends_with("module.exports =")
            || before.ends_with("module.exports=")
            || before.ends_with("Object.assign(module.exports,")
            || before.ends_with("Object.assign(exports,");
        if !is_reexport {
            continue;
        }
        let after = &src[i + "require(".len()..];
        if let Some(spec) = leading_string_literal(after.trim_start()) {
            out.push(spec);
        }
    }
    out
}

/// Statically discover a CommonJS module's export names — the cjs-module-lexer heuristic. Handles
/// the two dominant shapes: direct assignment (`exports.X =` / `module.exports.X =`) and the
/// `Object.defineProperty(exports, "X", …)` that transpiled ESM emits. A miss just means a name
/// isn't re-exported (same as before); a false positive is a harmless `undefined` export.
fn scan_cjs_exports(src: &str) -> Vec<String> {
    let bytes = src.as_bytes();
    let mut names: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // `exports.NAME =` and `module.exports.NAME =` (the `module.` prefix is subsumed).
    for (i, _) in src.match_indices("exports.") {
        let start = i + "exports.".len();
        let name = read_ident(bytes, start);
        if !name.is_empty() && next_is_assignment(bytes, start + name.len()) {
            add_export(&mut names, &mut seen, &name);
        }
    }

    // `Object.defineProperty(exports, "NAME", …)` — the transpiler pattern.
    for (i, _) in src.match_indices("defineProperty(") {
        let rest = &src[i + "defineProperty(".len()..];
        let rest = rest.trim_start();
        let rest = rest
            .strip_prefix("module.exports")
            .or_else(|| rest.strip_prefix("exports"));
        if let Some(rest) = rest {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix(',') {
                if let Some(name) = leading_string_literal(rest.trim_start()) {
                    add_export(&mut names, &mut seen, &name);
                }
            }
        }
    }

    // `module.exports = { a, b: … }` — including the `0 && (module.exports = { … })` hint that
    // bundlers (esbuild, tsc) emit specifically for CJS export lexers.
    for (i, _) in src.match_indices("module.exports") {
        let rest = src[i + "module.exports".len()..].trim_start();
        if let Some(rest) = rest.strip_prefix('=') {
            if let Some(rest) = rest.trim_start().strip_prefix('{') {
                for key in object_key_list(rest) {
                    add_export(&mut names, &mut seen, &key);
                }
            }
        }
    }

    // `__export(target, { name: () => name, … })` — the esbuild/tsc re-export helper.
    for (i, _) in src.match_indices("__export(") {
        let rest = &src[i + "__export(".len()..];
        if let Some(comma) = rest.find(',') {
            if let Some(obj) = rest[comma + 1..].trim_start().strip_prefix('{') {
                for key in object_key_list(obj) {
                    add_export(&mut names, &mut seen, &key);
                }
            }
        }
    }

    names
}

/// The top-level keys of an object literal, given the text just past its opening `{`. Handles
/// shorthand (`{ a, b }`), `key:` pairs, string keys, and nested braces/brackets/parens/strings.
fn object_key_list(after_brace: &str) -> Vec<String> {
    let b = after_brace.as_bytes();
    let mut keys = Vec::new();
    let mut depth = 1usize; // already inside the outer `{`
    let mut expect_key = true;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'{' | b'[' | b'(' => {
                depth += 1;
                expect_key = false;
                i += 1;
            }
            b'}' | b']' | b')' => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    break;
                }
            }
            b'"' | b'\'' | b'`' => {
                let quote = c;
                let start = i + 1;
                let mut j = start;
                while j < b.len() && b[j] != quote {
                    if b[j] == b'\\' {
                        j += 1;
                    }
                    j += 1;
                }
                if expect_key && depth == 1 {
                    keys.push(String::from_utf8_lossy(&b[start..j.min(b.len())]).into_owned());
                    expect_key = false;
                }
                i = j + 1;
            }
            b',' if depth == 1 => {
                expect_key = true;
                i += 1;
            }
            b':' if depth == 1 => {
                expect_key = false;
                i += 1;
            }
            _ => {
                if expect_key && depth == 1 && (c.is_ascii_alphabetic() || c == b'_' || c == b'$') {
                    let name = read_ident(b, i);
                    i += name.len();
                    keys.push(name);
                    expect_key = false;
                } else {
                    i += 1;
                }
            }
        }
    }
    keys
}

fn add_export(names: &mut Vec<String>, seen: &mut std::collections::HashSet<String>, name: &str) {
    if is_export_ident(name) && seen.insert(name.to_string()) {
        names.push(name.to_string());
    }
}

/// Read a JS identifier starting at `pos`.
fn read_ident(bytes: &[u8], pos: usize) -> String {
    let mut end = pos;
    while end < bytes.len() {
        let c = bytes[end];
        let ok = c.is_ascii_alphanumeric() || c == b'_' || c == b'$';
        if !ok {
            break;
        }
        end += 1;
    }
    String::from_utf8_lossy(&bytes[pos..end]).into_owned()
}

/// Whether the next non-space token at `pos` is a plain `=` (assignment) rather than `==`/`=>`.
fn next_is_assignment(bytes: &[u8], mut pos: usize) -> bool {
    while pos < bytes.len() && (bytes[pos] == b' ' || bytes[pos] == b'\t') {
        pos += 1;
    }
    pos < bytes.len()
        && bytes[pos] == b'='
        && bytes.get(pos + 1) != Some(&b'=')
        && bytes.get(pos + 1) != Some(&b'>')
}

/// The contents of a leading `"…"`/`'…'` string literal (no escapes handled — export names are
/// plain identifiers in practice).
fn leading_string_literal(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let quote = *bytes.first()?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let rest = &s[1..];
    let end = rest.find(quote as char)?;
    Some(rest[..end].to_string())
}

/// A valid, non-reserved identifier usable as an `export const` name (excludes `default` and
/// the transpiler marker `__esModule`).
fn is_export_ident(name: &str) -> bool {
    if name.is_empty() || name == "default" || name == "__esModule" {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
        return false;
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
    {
        return false;
    }
    !is_reserved_word(name)
}

/// ES reserved words that cannot be `export const` binding names.
fn is_reserved_word(name: &str) -> bool {
    matches!(
        name,
        "break"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "continue"
            | "debugger"
            | "default"
            | "delete"
            | "do"
            | "else"
            | "enum"
            | "export"
            | "extends"
            | "false"
            | "finally"
            | "for"
            | "function"
            | "if"
            | "import"
            | "in"
            | "instanceof"
            | "new"
            | "null"
            | "return"
            | "super"
            | "switch"
            | "this"
            | "throw"
            | "true"
            | "try"
            | "typeof"
            | "var"
            | "void"
            | "while"
            | "with"
            | "yield"
            | "let"
            | "static"
            | "await"
    )
}

/// Relative targets only; external private targets remain unsupported.
#[derive(Debug, PartialEq)]
enum PackageEsmTarget<'a> {
    Target(&'a str),
    Blocked,
    NoMatch,
    Unsupported,
}

impl<'a> PackageEsmTarget<'a> {
    fn target(self) -> Option<&'a str> {
        match self {
            Self::Target(target) => Some(target),
            _ => None,
        }
    }
}

/// Preserve declaration order; unmatched nested conditions allow later active siblings,
/// while an explicit null blocks the target rather than falling through.
fn package_esm_target(value: &crate::tsconfig::Json) -> PackageEsmTarget<'_> {
    use crate::tsconfig::Json;
    match value {
        Json::Str(value) if relative_package_target(value) => PackageEsmTarget::Target(value),
        Json::Null => PackageEsmTarget::Blocked,
        Json::Arr(targets) => {
            if targets.is_empty() {
                return PackageEsmTarget::Blocked;
            }
            // Node arrays skip unmatched conditions, nulls and invalid targets in order.
            // A null resets the remembered invalid result; without a valid target, the
            // last null/invalid result wins. Missing files do not trigger array fallback.
            let mut last = PackageEsmTarget::NoMatch;
            for target in targets {
                match package_esm_target(target) {
                    resolved @ PackageEsmTarget::Target(_) => return resolved,
                    PackageEsmTarget::NoMatch => {}
                    resolved => last = resolved,
                }
            }
            last
        }
        Json::Obj(conditions) => {
            for (condition, value) in conditions {
                if matches!(condition.as_str(), "node" | "import" | "default") {
                    let resolved = package_esm_target(value);
                    if resolved != PackageEsmTarget::NoMatch {
                        return resolved;
                    }
                }
            }
            PackageEsmTarget::NoMatch
        }
        _ => PackageEsmTarget::Unsupported,
    }
}

/// Legacy root module/main fields apply only when the package has no exports field.
fn pkg_entry(pkg_json: &str) -> Option<String> {
    let json = crate::tsconfig::parse_jsonc(pkg_json).ok()?;
    if let Some(exports) = json.get("exports") {
        return package_esm_target(exports.get(".").unwrap_or(exports))
            .target()
            .map(str::to_string);
    }
    ["module", "main"]
        .into_iter()
        .find_map(|field| match json.get(field)? {
            crate::tsconfig::Json::Str(value) => Some(value.clone()),
            _ => None,
        })
}

/// The subpath of a bare specifier, if any: `hono/logger` -> `logger`, `@sc/pkg/a/b` -> `a/b`,
/// and `hono` / `@sc/pkg` (bare package roots) -> `None`.
fn package_subpath(name: &str) -> Option<String> {
    let mut parts = if name.starts_with('@') {
        name.splitn(3, '/')
    } else {
        name.splitn(2, '/')
    };
    parts.next()?; // package (or scope)
    if name.starts_with('@') {
        parts.next()?; // scoped name
    }
    parts.next().map(str::to_string)
}

/// Exact subpaths take precedence; single-star patterns prefer the longest static prefix,
/// then the longest key (Node's pattern-key precedence). Star captures can contain slashes.
fn exports_subpath(pkg_json: &str, subpath: &str) -> Option<String> {
    use crate::tsconfig::Json;
    let json = crate::tsconfig::parse_jsonc(pkg_json).ok()?;
    let exports = json.get("exports")?;
    let key = format!("./{subpath}");
    if let Some(target) = exports.get(&key) {
        return package_esm_target(target).target().map(str::to_string);
    }
    let Json::Obj(entries) = exports else {
        return None;
    };
    let mut matched = None;
    for (pattern, target) in entries {
        let Some(star) = pattern.find('*') else {
            continue;
        };
        if pattern[star + 1..].contains('*') {
            continue;
        }
        let (prefix, suffix) = (&pattern[..star], &pattern[star + 1..]);
        if key.len() < prefix.len() + suffix.len()
            || !key.starts_with(prefix)
            || !key.ends_with(suffix)
        {
            continue;
        }
        let rank = (prefix.len(), pattern.len());
        if matched.as_ref().is_some_and(|(best, _, _)| *best >= rank) {
            continue;
        }
        matched = Some((rank, target, &key[prefix.len()..key.len() - suffix.len()]));
    }
    let (_, target, capture) = matched?;
    let resolved = package_esm_target(target).target()?.replace('*', capture);
    relative_package_target(&resolved).then_some(resolved)
}

fn relative_package_target(value: &str) -> bool {
    value.starts_with("./")
        && !value.split('/').skip(1).any(|part| {
            let lower = part.to_ascii_lowercase();
            let dots = lower.replace("%2e", ".");
            dots == "."
                || dots == ".."
                || lower == "node_modules"
                || lower.contains("%2f")
                || lower.contains("%5c")
                || lower.contains('\\')
        })
}

/// Lexically resolve `.`/`..` without touching the filesystem (the target may not exist yet).
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// A minimal JSON scan for a `"field": "value"` string — enough for the few `package.json` keys
/// we read, without a JSON dependency (the workspace is zero-dep). Matches the field as a *key*
/// (a `"field"` followed by `:`), so it skips occurrences of the same text used as a value — e.g.
/// the `"module"` in `"type": "module"` is not mistaken for a `"module"` key.
#[cfg(test)]
fn json_string_field(json: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\"");
    let mut from = 0;
    while let Some(pos) = json[from..].find(&needle) {
        let after = json[from + pos + needle.len()..].trim_start();
        if let Some(value) = after.strip_prefix(':') {
            // A key: return its string value (`None` if the value isn't a string).
            return parse_string(value);
        }
        // Matched the text as a value or substring; keep looking for the real key.
        from += pos + needle.len();
    }
    None
}

/// Parse a JSON string literal from `s` (leading whitespace allowed, then the opening `"`).
#[cfg(test)]
fn parse_string(s: &str) -> Option<String> {
    let rest = s.trim_start().strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => out.push(chars.next()?),
            other => out.push(other),
        }
    }
    None
}

/// A JSON/JS string literal (for embedding an absolute path in generated source).
fn js_string(s: &str) -> String {
    let mut out = String::from('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn source_cache_bounds_bytes_and_keeps_recent_sources() {
        use super::*;
        let mut cache = SourceCache::default();
        let a = ("a".to_owned(), None);
        let b = ("b".to_owned(), None);
        let c = ("c".to_owned(), None);
        let source = "x".repeat(SOURCE_CACHE_BYTES / 2 - 16);
        cache.insert(a.clone(), &source);
        cache.insert(b.clone(), &source);
        assert!(cache.get(&a).is_some());
        cache.insert(c.clone(), &"y".repeat(64));
        assert!(cache.get(&a).is_some());
        assert!(cache.get(&b).is_none());
        assert_eq!(cache.get(&c).as_deref(), Some("y".repeat(64).as_str()));
        assert!(cache.bytes <= SOURCE_CACHE_BYTES);
        cache.insert(a.clone(), &"z".repeat(SOURCE_CACHE_BYTES + 1));
        assert!(
            cache.get(&a).is_none(),
            "oversized replacement must not leave stale source"
        );
        cache.clear();
        assert_eq!(cache.bytes, 0);
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn source_cache_bounds_empty_records_and_separates_attributes() {
        use super::*;
        let mut cache = SourceCache::default();
        for index in 0..SOURCE_CACHE_ENTRIES + 1 {
            cache.insert((index.to_string(), None), "");
        }
        assert_eq!(cache.entries.len(), SOURCE_CACHE_ENTRIES);
        assert!(cache.get(&("0".into(), None)).is_none());
        cache.insert(("file".into(), None), "plain");
        cache.insert(("file".into(), Some("json".into())), "json");
        assert_eq!(cache.get(&("file".into(), None)).as_deref(), Some("plain"));
        assert_eq!(
            cache.get(&("file".into(), Some("json".into()))).as_deref(),
            Some("json")
        );
    }

    #[test]
    fn source_cache_misses_reread_while_hits_preserve_resolution() {
        use super::*;
        let cache = LoaderCache::default();
        let calls = std::cell::Cell::new(0);
        let read = |_: &str, _: &str, _: Option<&str>| {
            calls.set(calls.get() + 1);
            Some(("canonical".to_owned(), calls.get().to_string()))
        };
        assert_eq!(
            cache
                .resolve("./a.mjs", "/root/main.mjs", None, read)
                .unwrap()
                .1,
            "1"
        );
        assert_eq!(
            cache
                .resolve("./a.mjs", "/root/main.mjs", None, read)
                .unwrap()
                .1,
            "1"
        );
        cache.forget_sources();
        assert_eq!(
            cache
                .resolve("./a.mjs", "/root/main.mjs", None, read)
                .unwrap()
                .1,
            "2"
        );
        assert_eq!(calls.get(), 2);
    }
    #[test]
    fn export_patterns_preserve_exact_blocks_specificity_and_boundaries() {
        let manifest = r#"{"exports":{"./*":{"import":"./dist/esm/*","require":"./dist/cjs/*"},"./feature/*":"./feature/*","./feature/*.js":"./specific/*.mjs","./blocked.js":null,"./internal/*":null}}"#;
        assert_eq!(
            super::exports_subpath(manifest, "types.js").as_deref(),
            Some("./dist/esm/types.js")
        );
        assert_eq!(
            super::exports_subpath(manifest, "feature/a.js").as_deref(),
            Some("./specific/a.mjs")
        );
        for subpath in [
            "blocked.js",
            "internal/private.js",
            "../secret.js",
            "%2e%2e/secret.js",
            "node_modules/secret.js",
            "a%2fb.js",
        ] {
            assert_eq!(super::exports_subpath(manifest, subpath), None, "{subpath}");
        }
    }

    #[test]
    fn explicit_root_exports_do_not_fall_back_to_legacy_entries() {
        for exports in ["null", r#"{".":null}"#, r#"{".":{"require":"./cjs.js"}}"#] {
            let manifest =
                format!(r#"{{"exports":{exports},"module":"./legacy.mjs","main":"./legacy.js"}}"#);
            assert_eq!(super::pkg_entry(&manifest), None);
        }
        assert_eq!(
            super::pkg_entry(r#"{"main":"./legacy.js"}"#).as_deref(),
            Some("./legacy.js")
        );
    }

    #[test]
    fn nested_unmatched_condition_continues_but_null_blocks() {
        let manifest =
            r#"{"exports":{"node":{"require":"./wrong.cjs"},"default":"./fallback.mjs"}}"#;
        assert_eq!(
            super::pkg_entry(manifest).as_deref(),
            Some("./fallback.mjs")
        );
        let blocked =
            r#"{"exports":{"node":{"import":null},"default":"./wrong.mjs"},"main":"./legacy.js"}"#;
        assert_eq!(super::pkg_entry(blocked), None);
        let ordered = r#"{"exports":{"default":"./first.mjs","node":{"import":"./second.mjs"}}}"#;
        assert_eq!(super::pkg_entry(ordered).as_deref(), Some("./first.mjs"));
        assert_eq!(
            super::pkg_entry(r#"{"exports":["./array.mjs"],"main":"./legacy.js"}"#),
            Some("./array.mjs".to_string())
        );
    }

    #[test]
    fn exports_arrays_use_ordered_fallback_for_acorn_and_nulls() {
        let acorn = r#"{"exports":{".":[{"import":"./dist/acorn.mjs","require":"./dist/acorn.js","default":"./dist/acorn.js"},"./dist/acorn.js"]}}"#;
        assert_eq!(super::pkg_entry(acorn).as_deref(), Some("./dist/acorn.mjs"));
        let fallback = r#"{"exports":[{"require":"./wrong.cjs"},null,false,"../outside.js",[null,"./first.mjs"],"./second.mjs"]}"#;
        assert_eq!(super::pkg_entry(fallback).as_deref(), Some("./first.mjs"));
        let blocked_branch = r#"{"exports":{"node":[null,{"require":"./unused.cjs"}],"default":"./wrong.mjs"},"main":"./legacy.js"}"#;
        assert_eq!(super::pkg_entry(blocked_branch), None);
        let unmatched =
            r#"{"exports":{"node":[{"require":"./unused.cjs"}],"default":"./fallback.mjs"}}"#;
        assert_eq!(
            super::pkg_entry(unmatched).as_deref(),
            Some("./fallback.mjs")
        );
    }

    #[test]
    fn exports_array_terminal_status_matches_node() {
        use super::{package_esm_target, PackageEsmTarget};
        for (source, expected) in [
            ("[]", PackageEsmTarget::Blocked),
            ("[false,null]", PackageEsmTarget::Blocked),
            ("[null,false]", PackageEsmTarget::Unsupported),
            (r#"[{"require":"./unused.cjs"}]"#, PackageEsmTarget::NoMatch),
        ] {
            let json = crate::tsconfig::parse_jsonc(source).unwrap();
            assert_eq!(package_esm_target(&json), expected, "{source}");
        }
    }

    #[test]
    fn package_root_export_ignores_earlier_subpaths() {
        let manifest = r#"{"repository":{"type":"git"},"type":"module","exports":{
            "./compile":{"import":"./compile.mjs"},
            ".":{"import":"./index.mjs"},
            "./blocked":{"node":null,"default":"./wrong.mjs"}
        }}"#;
        assert_eq!(super::pkg_entry(manifest).as_deref(), Some("./index.mjs"));
        assert_eq!(
            super::exports_subpath(manifest, "compile").as_deref(),
            Some("./compile.mjs")
        );
        assert_eq!(super::exports_subpath(manifest, "blocked"), None);
    }
    use super::*;

    #[test]
    fn json_field_scan() {
        assert_eq!(
            json_string_field(r#"{"type":"module"}"#, "type").as_deref(),
            Some("module")
        );
        assert_eq!(
            json_string_field(r#"{ "main" : "lib/i.js" }"#, "main").as_deref(),
            Some("lib/i.js")
        );
        assert_eq!(json_string_field(r#"{"a":1}"#, "type"), None);
    }

    #[test]
    fn package_type_reads_only_the_root_field() {
        assert_eq!(
            crate::package_type_from_json(r#"{"repository":{"type":"git"},"type":"module"}"#)
                .as_deref(),
            Some("module")
        );
        assert_eq!(
            crate::package_type_from_json(r#"{"repository":{"type":"module"}}"#),
            None
        );
        assert_eq!(crate::package_type_from_json(r#"{"type":null}"#), None);
    }

    #[test]
    fn json_field_matches_key_not_value() {
        // `"module"` appears first as the *value* of `"type"`, then as a real key. The scan must
        // return the key's value, not choke on the value occurrence (the hono package.json shape).
        let pkg = r#"{ "main": "dist/cjs/index.js", "type": "module", "module": "dist/index.js" }"#;
        assert_eq!(
            json_string_field(pkg, "module").as_deref(),
            Some("dist/index.js")
        );
        assert_eq!(json_string_field(pkg, "type").as_deref(), Some("module"));
    }

    #[test]
    fn pkg_entry_prefers_exports_import_condition() {
        // hono-shaped: `main` is CJS, but the ESM entry lives under exports' `import` condition.
        let pkg = r#"{
            "main": "dist/cjs/index.js",
            "type": "module",
            "module": "dist/index.js",
            "exports": { ".": {
                "types": "./dist/types/index.d.ts",
                "import": "./dist/index.js",
                "require": "./dist/cjs/index.js"
            } }
        }"#;
        assert_eq!(pkg_entry(pkg).as_deref(), Some("./dist/index.js"));
    }

    #[test]
    fn pkg_entry_exports_string_then_module_then_main() {
        assert_eq!(
            pkg_entry(r#"{ "exports": "./e.js", "main": "./m.js" }"#).as_deref(),
            Some("./e.js")
        );
        assert_eq!(
            pkg_entry(r#"{ "module": "./mod.js", "main": "./m.js" }"#).as_deref(),
            Some("./mod.js")
        );
        assert_eq!(
            pkg_entry(r#"{ "main": "./m.js" }"#).as_deref(),
            Some("./m.js")
        );
    }

    #[test]
    fn package_subpath_splits_plain_and_scoped() {
        assert_eq!(package_subpath("hono"), None);
        assert_eq!(package_subpath("hono/logger").as_deref(), Some("logger"));
        assert_eq!(
            package_subpath("hono/dist/x.js").as_deref(),
            Some("dist/x.js")
        );
        assert_eq!(package_subpath("@scope/pkg"), None);
        assert_eq!(package_subpath("@scope/pkg/sub").as_deref(), Some("sub"));
    }

    #[test]
    fn exports_subpath_reads_condition_and_string_forms() {
        // hono-shaped middleware subpath: an object with an `import` condition.
        let pkg = r#"{ "exports": {
            ".": { "import": "./dist/index.js" },
            "./logger": {
                "types": "./dist/types/middleware/logger/index.d.ts",
                "import": "./dist/middleware/logger/index.js",
                "require": "./dist/cjs/middleware/logger/index.js"
            }
        } }"#;
        assert_eq!(
            exports_subpath(pkg, "logger").as_deref(),
            Some("./dist/middleware/logger/index.js")
        );
        assert_eq!(exports_subpath(pkg, "cors"), None);
        // Bare-string subpath form.
        assert_eq!(
            exports_subpath(r#"{ "exports": { "./x": "./lib/x.js" } }"#, "x").as_deref(),
            Some("./lib/x.js")
        );
    }

    #[test]
    fn package_dir_handles_plain_and_scoped_subpaths() {
        let root = Path::new("/app");
        assert_eq!(
            package_dir(root, "hono"),
            PathBuf::from("/app/node_modules/hono")
        );
        assert_eq!(
            package_dir(root, "hono/dist/index.js"),
            PathBuf::from("/app/node_modules/hono")
        );
        assert_eq!(
            package_dir(root, "@scope/pkg/sub.js"),
            PathBuf::from("/app/node_modules/@scope/pkg")
        );
    }

    #[test]
    fn normalize_dotdot() {
        assert_eq!(
            normalize(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
    }

    #[test]
    fn js_string_escapes() {
        assert_eq!(js_string(r#"a"b\c"#), r#""a\"b\\c""#);
    }
    #[test]
    fn subpath_exports_own_legacy_files_and_refuse_blocked_disk_fallback() {
        let dir=std::env::temp_dir().join(format!("lumen-exports-shadow-{}-{}",std::process::id(),std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let pkg=dir.join("node_modules/shadow");std::fs::create_dir_all(pkg.join("dist")).unwrap();
        std::fs::write(pkg.join("package.json"),r#"{"type":"module","exports":{"./stream":{"import":"./dist/stream.js","require":"./dist/stream.cjs"},"./cjs":"./dist/stream.cjs","./blocked":null}}"#).unwrap();
        for path in ["stream.js","blocked.js","dist/stream.js","dist/stream.cjs"] {std::fs::write(pkg.join(path),"").unwrap();}
        let (path,esm)=super::resolve_node_modules("shadow/stream",&dir).expect("mapped module");
        assert_eq!(path,pkg.join("dist/stream.js"));assert!(esm);
        let (path,esm)=super::resolve_node_modules("shadow/cjs",&dir).expect("mapped cjs");
        assert_eq!(path,pkg.join("dist/stream.cjs"));assert!(!esm);
        assert!(super::resolve_node_modules("shadow/blocked",&dir).is_none());
        assert!(super::resolve_node_modules("shadow/stream.js",&dir).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

}
