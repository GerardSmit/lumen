//! The bundler shared by `include_js!` (compiled into the proc macro via `#[path]`) and
//! [`crate::build`]: read the named files, walk relative ES module imports from the entry,
//! compile every unit with lumen's parser and assemble the blob.

use std::collections::VecDeque;
use std::path::{Component, Path, PathBuf};

use lumen::precompiled::{CompiledUnit, PrecompileBundle};
use lumen::SourceKind;

/// What to precompile. Relative paths resolve against the `base` handed to [`bundle`] (the
/// invoking crate's `CARGO_MANIFEST_DIR` for the macro).
#[derive(Clone, Debug, Default)]
pub struct Spec {
    /// Classic scripts, run in this order before the entry module.
    pub scripts: Vec<PathBuf>,
    /// The entry ES module (evaluated by `load_precompiled`).
    pub entry: Option<PathBuf>,
    /// More ES modules to include (e.g. targets of dynamic `import()`, which the walk does not
    /// see). Registered, not evaluated.
    pub modules: Vec<PathBuf>,
    /// Follow relative static imports / re-exports from the entry and `modules` (default on).
    pub walk: bool,
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
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
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

/// A relative specifier imported from `from`, as a file: exact, then `.js`, then `/index.js`
/// (the runtime resolver in lumen tries the same, in the same order).
fn resolve_file(from: &Path, spec: &str) -> Option<PathBuf> {
    let dir = from.parent()?;
    let exact = clean(&dir.join(spec));
    [
        exact.clone(),
        PathBuf::from(format!("{}.js", exact.display())),
        exact.join("index.js"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

fn is_relative(spec: &str) -> bool {
    spec.starts_with("./") || spec.starts_with("../")
}

pub fn bundle(base: &Path, spec: &Spec) -> Result<Bundle, String> {
    let abs = |p: &PathBuf| clean(&base.join(p));
    let mut inputs = Vec::new();
    let mut out = PrecompileBundle::new();

    for (i, script) in spec.scripts.iter().enumerate() {
        let path = abs(script);
        let src = read(&path)?;
        let unit = CompiledUnit::compile_with(&src, SourceKind::Script, !spec.no_bytecode)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let label = format!(
            "script{i}:{}",
            script.file_name().unwrap_or_default().to_string_lossy()
        );
        out.add_compiled(&label, unit)?;
        inputs.push(path);
    }

    // Breadth-first over the module graph; compile each file once, name it once the root is
    // known (a module's AST does not depend on its key).
    let mut modules: Vec<(PathBuf, CompiledUnit)> = Vec::new();
    let mut queue: VecDeque<(PathBuf, Option<PathBuf>)> = VecDeque::new();
    if let Some(e) = &spec.entry {
        queue.push_back((abs(e), None));
    }
    for m in &spec.modules {
        queue.push_back((abs(m), None));
    }
    while let Some((path, importer)) = queue.pop_front() {
        if modules.iter().any(|(p, _)| *p == path) {
            continue;
        }
        let src = read(&path).map_err(|e| match &importer {
            Some(from) => format!("{e} (imported from {})", from.display()),
            None => e,
        })?;
        let unit = CompiledUnit::compile_with(&src, SourceKind::Module, !spec.no_bytecode)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if spec.walk {
            for s in unit.imports().iter().filter(|s| is_relative(s)) {
                let dep = resolve_file(&path, s)
                    .ok_or_else(|| format!("{}: cannot resolve import {s:?}", path.display()))?;
                queue.push_back((dep, Some(path.clone())));
            }
        }
        inputs.push(path.clone());
        modules.push((path, unit));
    }

    let root = match &spec.root {
        Some(r) => abs(r),
        None => {
            let mut root: Option<PathBuf> = None;
            for (p, _) in &modules {
                let dir = p.parent().unwrap_or(Path::new("")).to_path_buf();
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
    let mut entry_rel = None;
    for (path, unit) in modules {
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
        if spec.entry.as_ref().is_some_and(|e| abs(e) == path) {
            entry_rel = Some(rel.clone());
        }
        out.add_compiled(&rel, unit)?;
    }
    if let Some(e) = entry_rel {
        out.set_entry(&e)?;
    }
    Ok(Bundle {
        blob: out.finish(),
        inputs,
    })
}
