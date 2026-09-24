//! The bundler shared by `include_js!` (compiled into the proc macro via `#[path]`) and
//! [`crate::build`]: read the named files, walk the module graph from the entry (static
//! imports, literal dynamic `import()`s, literal `require()`s, and — with `node_modules` —
//! bare package specifiers), compile every unit with lumen's parser and assemble the blob.

use std::collections::{HashMap, VecDeque};
use std::path::{Component, Path, PathBuf};

use lumen::precompiled::{CompileOptions, CompiledUnit, PrecompileBundle};
use lumen::SourceKind;

/// What to precompile. Relative paths resolve against the `base` handed to [`bundle`] (the
/// invoking crate's `CARGO_MANIFEST_DIR` for the macro).
#[derive(Clone, Debug, Default)]
pub struct Spec {
    /// Classic scripts, run in this order before the entry module.
    pub scripts: Vec<PathBuf>,
    /// The entry ES module (evaluated by `load_precompiled`).
    pub entry: Option<PathBuf>,
    /// More ES modules to include (e.g. targets of computed dynamic `import()`s, which the walk
    /// cannot see). Registered, not evaluated.
    pub modules: Vec<PathBuf>,
    /// Follow imports / re-exports, literal `import("…")`s and literal `require("…")`s from
    /// the entry and `modules` (default on).
    pub walk: bool,
    /// Also bundle bare package specifiers (`ws`, `@scope/pkg/sub`) resolved through
    /// `node_modules` (package.json `exports` / `main`), ES modules and CommonJS alike. Node
    /// builtins (`node:*`, `fs`, …) are never bundled. Off: bare specifiers are left to the
    /// host's module loader.
    pub node_modules: bool,
    /// Keep the source text of functions and classes (for `Function.prototype.toString`) in
    /// the files matching any of these globs (`**` = any number of path segments, `*` / `?`
    /// within one; matched against the path relative to the base directory, at any directory
    /// boundary — `"puppeteer-core/**"` matches `node_modules/puppeteer-core/lib/x.js`). `**`
    /// alone keeps every file's function text.
    pub keep_source: Vec<String>,
    /// Files never bundled, as globs (same syntax): an import of one is left to the host's
    /// module loader at run time.
    pub exclude: Vec<String>,
    /// The directory module keys are relative to (`aot:/<path from root>`). Default: the
    /// deepest directory containing every module of the bundle.
    pub root: Option<PathBuf>,
    /// Leave out the precompiled bytecode (AST only; functions compile at run time).
    pub no_bytecode: bool,
}

/// The finished blob and every file read to build it (for rebuild tracking).
pub struct Bundle {
    pub blob: Vec<u8>,
    pub inputs: Vec<PathBuf>,
}

fn read(path: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let text = match text.strip_prefix('\u{feff}') {
        Some(t) => t.to_string(),
        None => text,
    };
    // TypeScript: the engine's strip-only erasure (what it runs for a `.ts` file), every
    // offset kept, so the precompiled bytecode is plain JavaScript.
    if is_ts(path) {
        return lumen::typescript::strip_types(&text)
            .map_err(|e| format!("{}:{}:{}: {e}", path.display(), e.line, e.column));
    }
    Ok(text)
}

fn is_ts(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("ts" | "mts" | "cts")
    )
}

/// `.`/`..` collapsed without touching the filesystem (so a missing file is reported by the
/// read, with the path the user wrote).
fn clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            c => out.push(c.as_os_str()),
        }
    }
    out
}

fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

fn is_relative(spec: &str) -> bool {
    spec.starts_with("./") || spec.starts_with("../") || spec == "." || spec == ".."
}

/// Node's builtin modules (bare or `node:`-prefixed): never bundled.
const BUILTINS: &[&str] = &[
    "assert",
    "assert/strict",
    "async_hooks",
    "buffer",
    "child_process",
    "cluster",
    "console",
    "constants",
    "crypto",
    "dgram",
    "diagnostics_channel",
    "dns",
    "dns/promises",
    "domain",
    "events",
    "fs",
    "fs/promises",
    "http",
    "http2",
    "https",
    "inspector",
    "inspector/promises",
    "module",
    "net",
    "os",
    "path",
    "path/posix",
    "path/win32",
    "perf_hooks",
    "process",
    "punycode",
    "querystring",
    "readline",
    "readline/promises",
    "repl",
    "stream",
    "stream/consumers",
    "stream/promises",
    "stream/web",
    "string_decoder",
    "sys",
    "timers",
    "timers/promises",
    "tls",
    "trace_events",
    "tty",
    "url",
    "util",
    "util/types",
    "v8",
    "vm",
    "wasi",
    "worker_threads",
    "zlib",
];

fn is_builtin(spec: &str) -> bool {
    spec.starts_with("node:") || BUILTINS.contains(&spec)
}

/// A bare package specifier (not relative/absolute, no URL scheme, not a builtin).
fn is_bare_package(spec: &str) -> bool {
    !(spec.is_empty()
        || is_relative(spec)
        || spec.starts_with('/')
        || spec.starts_with('\\')
        || spec.contains(':')
        || Path::new(spec).is_absolute()
        || is_builtin(spec))
}

// ---- globs ------------------------------------------------------------------------------------

/// `*`/`?` within one segment.
fn seg_match(pat: &[u8], s: &[u8]) -> bool {
    match (pat.first(), s.first()) {
        (None, None) => true,
        (Some(b'*'), _) => seg_match(&pat[1..], s) || (!s.is_empty() && seg_match(pat, &s[1..])),
        (Some(b'?'), Some(_)) => seg_match(&pat[1..], &s[1..]),
        (Some(p), Some(c)) if p == c => seg_match(&pat[1..], &s[1..]),
        _ => false,
    }
}

fn segs_match(pat: &[&str], path: &[&str]) -> bool {
    match pat.first() {
        None => path.is_empty(),
        Some(&"**") => (0..=path.len()).any(|i| segs_match(&pat[1..], &path[i..])),
        Some(p) => {
            !path.is_empty()
                && seg_match(p.as_bytes(), path[0].as_bytes())
                && segs_match(&pat[1..], &path[1..])
        }
    }
}

/// Whether `glob` matches `rel` (a `/`-separated relative path) at any directory boundary; a
/// glob starting with `/` or `./` is anchored at the start.
fn glob_match(glob: &str, rel: &str) -> bool {
    let glob = glob.replace('\\', "/");
    let (anchored, g) = match glob.strip_prefix("./").or_else(|| glob.strip_prefix('/')) {
        Some(g) => (true, g.to_string()),
        None => (false, glob.clone()),
    };
    let pat: Vec<&str> = g.split('/').filter(|s| !s.is_empty()).collect();
    let path: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    if anchored {
        return segs_match(&pat, &path);
    }
    (0..path.len()).any(|i| segs_match(&pat, &path[i..]))
}

// ---- a minimal JSON reader (package.json) -----------------------------------------------------

#[derive(Clone, Debug)]
enum Json {
    Null,
    Bool,
    Num,
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(kv) => kv.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    fn str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
}

fn parse_json(text: &str) -> Option<Json> {
    struct P<'a> {
        b: &'a [u8],
        i: usize,
    }
    impl P<'_> {
        fn ws(&mut self) {
            while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
                self.i += 1;
            }
        }
        fn value(&mut self) -> Option<Json> {
            self.ws();
            match *self.b.get(self.i)? {
                b'{' => {
                    self.i += 1;
                    let mut kv = Vec::new();
                    self.ws();
                    if self.b.get(self.i) == Some(&b'}') {
                        self.i += 1;
                        return Some(Json::Obj(kv));
                    }
                    loop {
                        self.ws();
                        let k = self.string()?;
                        self.ws();
                        if self.b.get(self.i) != Some(&b':') {
                            return None;
                        }
                        self.i += 1;
                        let v = self.value()?;
                        kv.push((k, v));
                        self.ws();
                        match self.b.get(self.i)? {
                            b',' => self.i += 1,
                            b'}' => {
                                self.i += 1;
                                return Some(Json::Obj(kv));
                            }
                            _ => return None,
                        }
                    }
                }
                b'[' => {
                    self.i += 1;
                    let mut items = Vec::new();
                    self.ws();
                    if self.b.get(self.i) == Some(&b']') {
                        self.i += 1;
                        return Some(Json::Arr(items));
                    }
                    loop {
                        items.push(self.value()?);
                        self.ws();
                        match self.b.get(self.i)? {
                            b',' => self.i += 1,
                            b']' => {
                                self.i += 1;
                                return Some(Json::Arr(items));
                            }
                            _ => return None,
                        }
                    }
                }
                b'"' => self.string().map(Json::Str),
                b't' if self.b[self.i..].starts_with(b"true") => {
                    self.i += 4;
                    Some(Json::Bool)
                }
                b'f' if self.b[self.i..].starts_with(b"false") => {
                    self.i += 5;
                    Some(Json::Bool)
                }
                b'n' if self.b[self.i..].starts_with(b"null") => {
                    self.i += 4;
                    Some(Json::Null)
                }
                b'-' | b'0'..=b'9' => {
                    while self.i < self.b.len()
                        && matches!(self.b[self.i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
                    {
                        self.i += 1;
                    }
                    Some(Json::Num)
                }
                _ => None,
            }
        }
        fn string(&mut self) -> Option<String> {
            if self.b.get(self.i) != Some(&b'"') {
                return None;
            }
            self.i += 1;
            let mut out: Vec<u8> = Vec::new();
            loop {
                let c = *self.b.get(self.i)?;
                self.i += 1;
                match c {
                    b'"' => return String::from_utf8(out).ok(),
                    b'\\' => {
                        let e = *self.b.get(self.i)?;
                        self.i += 1;
                        match e {
                            b'n' => out.push(b'\n'),
                            b't' => out.push(b'\t'),
                            b'r' => out.push(b'\r'),
                            b'b' => out.push(8),
                            b'f' => out.push(12),
                            b'u' => {
                                let hex = std::str::from_utf8(self.b.get(self.i..self.i + 4)?)
                                    .ok()?;
                                self.i += 4;
                                let cp = u32::from_str_radix(hex, 16).ok()?;
                                let ch = char::from_u32(cp).unwrap_or('\u{fffd}');
                                let mut buf = [0u8; 4];
                                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                            }
                            other => out.push(other),
                        }
                    }
                    c => out.push(c),
                }
            }
        }
    }
    let mut p = P {
        b: text.as_bytes(),
        i: 0,
    };
    p.value()
}

// ---- resolution -------------------------------------------------------------------------------

/// How a file loads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FileKind {
    Esm,
    Cjs,
    Json,
}

/// Which conditions an `exports` lookup uses.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Import,
    Require,
}

struct Resolver {
    pkg_cache: HashMap<PathBuf, Option<Json>>,
    /// package.json files read (rebuild tracking).
    pkg_inputs: Vec<PathBuf>,
}

impl Resolver {
    fn package_json(&mut self, dir: &Path) -> Option<Json> {
        if let Some(j) = self.pkg_cache.get(dir) {
            return j.clone();
        }
        let file = dir.join("package.json");
        let j = if file.is_file() {
            self.pkg_inputs.push(file.clone());
            std::fs::read_to_string(&file)
                .ok()
                .and_then(|t| parse_json(&t))
        } else {
            None
        };
        self.pkg_cache.insert(dir.to_path_buf(), j.clone());
        j
    }

    /// The nearest package.json's `type` for `file` (`None`: no package.json or no field).
    fn package_type(&mut self, file: &Path) -> Option<String> {
        let mut dir = file.parent();
        while let Some(d) = dir {
            if d.join("package.json").is_file() {
                return self
                    .package_json(d)
                    .and_then(|j| j.get("type").and_then(Json::str).map(str::to_string));
            }
            dir = d.parent();
        }
        None
    }

    fn classify(&mut self, file: &Path) -> FileKind {
        match file.extension().and_then(|e| e.to_str()) {
            Some("mjs" | "mts") => FileKind::Esm,
            Some("cjs" | "cts") => FileKind::Cjs,
            Some("json") => FileKind::Json,
            _ => match self.package_type(file).as_deref() {
                Some("module") => FileKind::Esm,
                _ => FileKind::Cjs,
            },
        }
    }

    fn as_file(&self, p: &Path, mode: Mode) -> Option<PathBuf> {
        if p.is_file() {
            return Some(p.to_path_buf());
        }
        let exts: &[&str] = match mode {
            Mode::Import => &[".js", ".mjs", ".cjs", ".json", ".ts", ".mts", ".cts"],
            Mode::Require => &[".js", ".json", ".cjs", ".mjs", ".ts", ".cts"],
        };
        exts.iter().map(|e| with_suffix(p, e)).find(|c| c.is_file())
    }

    fn as_dir(&mut self, dir: &Path, mode: Mode) -> Option<PathBuf> {
        if !dir.is_dir() {
            return None;
        }
        if let Some(main) = self
            .package_json(dir)
            .and_then(|j| j.get("main").and_then(Json::str).map(str::to_string))
        {
            let target = clean(&dir.join(main));
            if let Some(f) = self
                .as_file(&target, mode)
                .or_else(|| self.as_file(&target.join("index"), mode))
            {
                return Some(f);
            }
        }
        self.as_file(&dir.join("index"), mode)
    }

    fn relative(&mut self, from: &Path, spec: &str, mode: Mode) -> Option<PathBuf> {
        let base = clean(&from.parent()?.join(spec));
        self.as_file(&base, mode).or_else(|| self.as_dir(&base, mode))
    }

    /// A bare specifier through the `node_modules` walk from `from`'s directory.
    fn package(&mut self, from: &Path, spec: &str, mode: Mode) -> Option<PathBuf> {
        let (name, sub) = split_package(spec)?;
        let mut dir = from.parent();
        while let Some(d) = dir {
            if d.file_name().is_some_and(|n| n == "node_modules") {
                dir = d.parent();
                continue;
            }
            let pkg_dir = d.join("node_modules").join(name);
            if pkg_dir.is_dir() {
                return self.in_package(&pkg_dir, sub, mode);
            }
            dir = d.parent();
        }
        None
    }

    fn in_package(&mut self, pkg_dir: &Path, sub: &str, mode: Mode) -> Option<PathBuf> {
        let pkg = self.package_json(pkg_dir);
        if let Some(exports) = pkg.as_ref().and_then(|p| p.get("exports")) {
            let subpath = if sub.is_empty() {
                ".".to_string()
            } else {
                format!("./{sub}")
            };
            let target = resolve_exports(exports, &subpath, mode)?;
            let f = clean(&pkg_dir.join(target));
            return f.is_file().then_some(f);
        }
        if sub.is_empty() {
            return self.as_dir(pkg_dir, mode);
        }
        let base = pkg_dir.join(sub);
        self.as_file(&base, mode).or_else(|| self.as_dir(&base, mode))
    }
}

/// `@scope/name/sub` -> (`@scope/name`, `sub`); `name/sub` -> (`name`, `sub`).
fn split_package(spec: &str) -> Option<(&str, &str)> {
    let mut idx = spec.find('/');
    if spec.starts_with('@') {
        let first = idx?;
        idx = spec[first + 1..].find('/').map(|i| first + 1 + i);
    }
    Some(match idx {
        Some(i) => (&spec[..i], &spec[i + 1..]),
        None => (spec, ""),
    })
}

/// package.json `exports` for `subpath` (`.` or `./x`): exact keys, then `*` patterns (the
/// longest matching prefix wins); conditions `node`, `import`/`require`, `default`.
fn resolve_exports(exports: &Json, subpath: &str, mode: Mode) -> Option<String> {
    let is_map = matches!(exports, Json::Obj(kv) if kv.iter().any(|(k, _)| k.starts_with('.')));
    if !is_map {
        return (subpath == ".").then(|| resolve_target(exports, None, mode))?;
    }
    let Json::Obj(kv) = exports else { return None };
    if let Some((_, v)) = kv.iter().find(|(k, _)| k == subpath) {
        return resolve_target(v, None, mode);
    }
    let mut best: Option<(usize, &Json, String)> = None;
    for (k, v) in kv {
        let Some(star) = k.find('*') else { continue };
        let (pre, post) = (&k[..star], &k[star + 1..]);
        if subpath.len() >= pre.len() + post.len()
            && subpath.starts_with(pre)
            && subpath.ends_with(post)
            && best.as_ref().is_none_or(|b| pre.len() > b.0)
        {
            let m = subpath[pre.len()..subpath.len() - post.len()].to_string();
            best = Some((pre.len(), v, m));
        }
    }
    let (_, v, m) = best?;
    resolve_target(v, Some(&m), mode)
}

fn resolve_target(t: &Json, star: Option<&str>, mode: Mode) -> Option<String> {
    match t {
        Json::Str(s) => Some(match star {
            Some(m) => s.replace('*', m),
            None => s.clone(),
        }),
        Json::Arr(items) => items.iter().find_map(|i| resolve_target(i, star, mode)),
        Json::Obj(kv) => {
            let want = match mode {
                Mode::Import => "import",
                Mode::Require => "require",
            };
            kv.iter()
                .filter(|(k, _)| k == want || k == "node" || k == "default")
                .find_map(|(_, v)| resolve_target(v, star, mode))
        }
        _ => None,
    }
}

// ---- synthesized sources ----------------------------------------------------------------------

fn js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn is_ident(s: &str) -> bool {
    let mut cs = s.chars();
    matches!(cs.next(), Some(c) if c == '_' || c == '$' || c.is_ascii_alphabetic())
        && cs.all(|c| c == '_' || c == '$' || c.is_ascii_alphanumeric())
}

/// Export names a CommonJS source assigns statically (`exports.x =`, `module.exports.x =`,
/// `Object.defineProperty(exports, "x"`) — a text scan in the spirit of cjs-module-lexer. Over-
/// approximation is harmless (the name reads `undefined`).
fn cjs_export_names(src: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut push = |n: &str| {
        if is_ident(n) && n != "default" && n != "__esModule" && !names.iter().any(|x| x == n) {
            names.push(n.to_string());
        }
    };
    let b = src.as_bytes();
    let mut i = 0;
    while let Some(off) = src[i..].find("exports.") {
        let at = i + off;
        let prev = if at == 0 { b' ' } else { b[at - 1] };
        let preceded_ok = !(prev.is_ascii_alphanumeric() || matches!(prev, b'_' | b'$' | b'.'))
            || src[..at].ends_with("module.");
        let start = at + "exports.".len();
        let end = start
            + src[start..]
                .find(|c: char| !(c == '_' || c == '$' || c.is_ascii_alphanumeric()))
                .unwrap_or(src.len() - start);
        let rest = src[end..].trim_start();
        if preceded_ok && rest.starts_with('=') && !rest.starts_with("==") {
            push(&src[start..end]);
        }
        i = end.max(at + 1);
    }
    let mut i = 0;
    while let Some(off) = src[i..].find("defineProperty(exports,") {
        let start = i + off + "defineProperty(exports,".len();
        let rest = src[start..].trim_start();
        if let Some(q) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') {
            if let Some(close) = rest[1..].find(q) {
                push(&rest[1..1 + close]);
            }
        }
        i = start;
    }
    names
}

/// Whether a `.js` file with no package "type" is an ES module by Node's detection: it has
/// module syntax (a line starting with an `import`/`export` declaration — the cheap pre-check)
/// and does not parse as a CommonJS body.
fn is_module_syntax(src: &str) -> bool {
    let candidate = src.lines().any(|line| {
        let l = line.trim_start();
        let after = |kw: &str| l.strip_prefix(kw).and_then(|r| r.chars().next());
        matches!(after("import"), Some(' ' | '\t' | '{' | '*' | '"' | '\'' | '.'))
            || matches!(after("export"), Some(' ' | '\t' | '{' | '*'))
    });
    candidate
        && CompiledUnit::compile_with_options(
            src,
            SourceKind::CommonJs,
            CompileOptions {
                bytecode: false,
                keep_source: false,
            },
        )
        .is_err()
}

/// The ES module an `import` of a CommonJS unit links to: it `require`s the unit (same key,
/// found through `import.meta.url`) and re-exports it as the default plus each static name.
fn cjs_facade(names: &[String]) -> String {
    let mut out = String::from(
        "const __m = globalThis.require(import.meta.url);\nexport default __m;\n",
    );
    for (i, n) in names.iter().enumerate() {
        out.push_str(&format!(
            "const __e{i} = __m == null ? undefined : __m[{}];\nexport {{ __e{i} as {n} }};\n",
            js_string(n)
        ));
    }
    out
}

// ---- the walk ---------------------------------------------------------------------------------

/// A unit to build: a file loaded as `kind` (a `Module` of a CommonJS/JSON file is its facade).
type Node = (PathBuf, SourceKind);

struct Built {
    node: Node,
    unit: CompiledUnit,
    /// (specifier, target) — resolved dependencies inside the bundle.
    links: Vec<(String, Node)>,
}

pub fn bundle(base: &Path, spec: &Spec) -> Result<Bundle, String> {
    let abs = |p: &PathBuf| clean(&base.join(p));
    let rel_to_base = |p: &Path| -> String {
        let r = p.strip_prefix(base).unwrap_or(p);
        r.components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/")
    };
    let keep = |p: &Path| -> bool {
        let r = rel_to_base(p);
        spec.keep_source.iter().any(|g| g == "**" || glob_match(g, &r))
    };
    let excluded = |p: &Path| -> bool {
        let r = rel_to_base(p);
        spec.exclude
            .iter()
            .any(|g| glob_match(g, &r))
    };
    let opts = |p: &Path| CompileOptions {
        bytecode: !spec.no_bytecode,
        keep_source: keep(p),
    };
    let mut inputs = Vec::new();
    let mut out = PrecompileBundle::new();
    let mut res = Resolver {
        pkg_cache: HashMap::new(),
        pkg_inputs: Vec::new(),
    };

    for (i, script) in spec.scripts.iter().enumerate() {
        let path = abs(script);
        let src = read(&path)?;
        let unit = CompiledUnit::compile_with_options(&src, SourceKind::Script, opts(&path))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let label = format!(
            "script{i}:{}",
            script.file_name().unwrap_or_default().to_string_lossy()
        );
        out.add_compiled(&label, unit)?;
        inputs.push(path);
    }

    // Breadth-first over the graph; compile each (file, kind) once, name it once the root is
    // known (an AST does not depend on its key).
    let mut built: Vec<Built> = Vec::new();
    let mut seen: HashMap<Node, ()> = HashMap::new();
    let mut queue: VecDeque<(Node, Option<PathBuf>)> = VecDeque::new();
    if let Some(e) = &spec.entry {
        queue.push_back(((abs(e), SourceKind::Module), None));
    }
    for m in &spec.modules {
        queue.push_back(((abs(m), SourceKind::Module), None));
    }
    // Entry and `modules` are ES modules by contract, whatever their extension says.
    let explicit: Vec<PathBuf> = spec
        .entry
        .iter()
        .chain(spec.modules.iter())
        .map(&abs)
        .collect();

    while let Some((node, importer)) = queue.pop_front() {
        if seen.contains_key(&node) {
            continue;
        }
        seen.insert(node.clone(), ());
        let (path, kind) = node.clone();
        let with_importer = |e: String| match &importer {
            Some(from) => format!("{e} (imported from {})", from.display()),
            None => e,
        };
        let file_kind = if explicit.contains(&path) {
            FileKind::Esm
        } else {
            match res.classify(&path) {
                // Node's syntax detection: a `.js`/`.ts` file outside any package "type" is
                // CommonJS unless it only parses as a module.
                FileKind::Cjs
                    if path.extension().is_some_and(|x| x == "js" || x == "ts")
                        && res.package_type(&path).is_none()
                        && read(&path).is_ok_and(|s| is_module_syntax(&s)) =>
                {
                    FileKind::Esm
                }
                k => k,
            }
        };
        let mut links: Vec<(String, Node)> = Vec::new();

        let unit = match (kind, file_kind) {
            // A CommonJS / JSON file imported as a module: the facade, plus the file itself.
            (SourceKind::Module, FileKind::Cjs | FileKind::Json) => {
                let src = read(&path).map_err(with_importer)?;
                let names = if file_kind == FileKind::Cjs {
                    cjs_export_names(&src)
                } else {
                    Vec::new()
                };
                queue.push_back(((path.clone(), SourceKind::CommonJs), importer.clone()));
                CompiledUnit::compile_with_options(
                    &cjs_facade(&names),
                    SourceKind::Module,
                    CompileOptions {
                        bytecode: !spec.no_bytecode,
                        keep_source: false,
                    },
                )
                .map_err(|e| format!("{} (facade): {e}", path.display()))?
            }
            // `require` of an ES module: the module itself (require(esm)).
            (SourceKind::CommonJs, FileKind::Esm) => {
                queue.push_back(((path.clone(), SourceKind::Module), importer.clone()));
                continue;
            }
            (SourceKind::CommonJs, FileKind::Json) => {
                let src = read(&path).map_err(with_importer)?;
                let body = format!("module.exports = JSON.parse({});", js_string(&src));
                CompiledUnit::compile_with_options(
                    &body,
                    SourceKind::CommonJs,
                    CompileOptions {
                        bytecode: !spec.no_bytecode,
                        keep_source: false,
                    },
                )
                .map_err(|e| format!("{}: {e}", path.display()))?
            }
            (SourceKind::CommonJs, FileKind::Cjs) => {
                let src = read(&path).map_err(with_importer)?;
                CompiledUnit::compile_with_options(&src, SourceKind::CommonJs, opts(&path))
                    .map_err(|e| format!("{}: {e}", path.display()))?
            }
            (SourceKind::Module, FileKind::Esm) => {
                let src = read(&path).map_err(with_importer)?;
                CompiledUnit::compile_with_options(&src, SourceKind::Module, opts(&path))
                    .map_err(|e| format!("{}: {e}", path.display()))?
            }
            (SourceKind::Script, _) => unreachable!("scripts are not walked"),
        };

        if spec.walk && !(kind == SourceKind::Module && file_kind != FileKind::Esm) {
            // (specifier, mode, required): a static import must resolve (relative) — the rest
            // are best-effort (optional dependencies, feature-detected requires).
            let mut deps: Vec<(String, Mode, bool)> = Vec::new();
            for s in unit.imports() {
                deps.push((s.clone(), Mode::Import, is_relative(s)));
            }
            for s in unit.dynamic_imports() {
                deps.push((s.clone(), Mode::Import, false));
            }
            if kind == SourceKind::CommonJs {
                for s in unit.requires() {
                    deps.push((s.clone(), Mode::Require, false));
                }
            }
            for (s, mode, required) in deps {
                if is_builtin(&s) {
                    continue;
                }
                let target = if is_relative(&s) {
                    res.relative(&path, &s, mode)
                } else if spec.node_modules && is_bare_package(&s) {
                    res.package(&path, &s, mode)
                } else {
                    continue;
                };
                let Some(target) = target else {
                    if required {
                        return Err(format!("{}: cannot resolve import {s:?}", path.display()));
                    }
                    continue;
                };
                if excluded(&target) {
                    continue;
                }
                let tkind = match mode {
                    Mode::Import => SourceKind::Module,
                    Mode::Require => SourceKind::CommonJs,
                };
                let tnode = (target, tkind);
                links.push((s, tnode.clone()));
                queue.push_back((tnode, Some(path.clone())));
            }
        }
        if !inputs.contains(&path) {
            inputs.push(path.clone());
        }
        built.push(Built { node, unit, links });
    }

    // `require` of an ES module was forwarded to its module unit: point links there.
    let present: HashMap<Node, ()> = built.iter().map(|b| (b.node.clone(), ())).collect();
    let fix = |n: &Node| -> Node {
        if present.contains_key(n) {
            n.clone()
        } else {
            (n.0.clone(), SourceKind::Module)
        }
    };

    let root = match &spec.root {
        Some(r) => abs(r),
        None => {
            let mut root: Option<PathBuf> = None;
            for b in &built {
                let dir = b.node.0.parent().unwrap_or(Path::new("")).to_path_buf();
                root = Some(match root {
                    None => dir,
                    Some(r) => {
                        let mut r = r;
                        while !dir.starts_with(&r) {
                            if !r.pop() {
                                break;
                            }
                        }
                        r
                    }
                });
            }
            root.unwrap_or_default()
        }
    };
    let entry = spec.entry.as_ref().map(&abs);
    let mut entry_rel = None;
    let mut index: HashMap<Node, usize> = HashMap::new();
    let mut pending_links: Vec<(usize, Vec<(String, Node)>)> = Vec::new();
    for b in built {
        let (path, kind) = &b.node;
        let rel = path.strip_prefix(&root).map_err(|_| {
            format!(
                "{} is outside the bundle root {}",
                path.display(),
                root.display()
            )
        })?;
        let rel: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        let rel = rel.join("/");
        if *kind == SourceKind::Module && entry.as_ref() == Some(path) {
            entry_rel = Some(rel.clone());
        }
        let i = out.add_compiled(&rel, b.unit)?;
        index.insert(b.node.clone(), i);
        pending_links.push((i, b.links));
    }
    for (from, links) in pending_links {
        for (s, target) in links {
            if let Some(&to) = index.get(&fix(&target)) {
                out.link(from, &s, to)?;
            }
        }
    }
    if let Some(e) = entry_rel {
        out.set_entry(&e)?;
    }
    inputs.extend(res.pkg_inputs);
    Ok(Bundle {
        blob: out.finish(),
        inputs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob_match("puppeteer-core/**", "node_modules/puppeteer-core/lib/a.js"));
        assert!(glob_match("**", "a/b.js"));
        assert!(glob_match("*.mjs", "js/test.mjs"));
        assert!(glob_match("js/*.mjs", "x/js/test.mjs"));
        assert!(!glob_match("./js/*.mjs", "x/js/test.mjs"));
        assert!(glob_match("./x/js/*.mjs", "x/js/test.mjs"));
        assert!(!glob_match("puppeteer-core/**", "node_modules/ws/index.js"));
        assert!(glob_match("lib/**/bidi/*", "node_modules/p/lib/puppeteer/bidi/x.js"));
    }

    #[test]
    fn exports_resolution() {
        let j = parse_json(
            r#"{".": {"types": "./t.d.ts", "import": "./esm.mjs", "require": "./cjs.js"},
                "./internal/*": {"import": "./lib/*"}, "./*": "./*", "./package.json": "./package.json"}"#,
        )
        .unwrap();
        assert_eq!(resolve_exports(&j, ".", Mode::Import).as_deref(), Some("./esm.mjs"));
        assert_eq!(resolve_exports(&j, ".", Mode::Require).as_deref(), Some("./cjs.js"));
        assert_eq!(
            resolve_exports(&j, "./internal/a.js", Mode::Import).as_deref(),
            Some("./lib/a.js")
        );
        assert_eq!(resolve_exports(&j, "./x/y.js", Mode::Import).as_deref(), Some("./x/y.js"));
        let s = parse_json(r#""./main.js""#).unwrap();
        assert_eq!(resolve_exports(&s, ".", Mode::Import).as_deref(), Some("./main.js"));
        assert_eq!(split_package("@a/b/c/d"), Some(("@a/b", "c/d")));
        assert_eq!(split_package("ws"), Some(("ws", "")));
    }

    #[test]
    fn cjs_names() {
        let n = cjs_export_names(
            "exports.a = 1; module.exports.b = 2; x.exports.c = 3; exports.d == 4;\n\
             Object.defineProperty(exports, \"e\", {}); exports.default = 5;",
        );
        assert_eq!(n, vec!["a", "b", "e"]);
    }
}
