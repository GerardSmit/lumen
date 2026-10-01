//! Assembly of the `node:` glue from `src/js/*.js` into one IIFE, shared by `build.rs` (which
//! precompiles the result) and `glue_dev.rs` (which assembles it from disk at startup when
//! `LUMEN_NODE_GLUE_DIR` is set).

pub struct Entry {
    pub name: &'static str,
    /// Put the file's body in its own `{ }` block (see `JS_FILES` in build.rs).
    pub wrap: bool,
    /// Load the file's body on first use (see `LAZY` in build.rs).
    pub lazy: bool,
    /// The cargo feature (`CARGO_FEATURE_<NAME>` spelling) the file belongs to, if optional.
    pub feature: Option<&'static str>,
}

pub fn assemble(
    entries: &[Entry],
    read: impl Fn(&str) -> String,
    feature_on: impl Fn(&str) -> bool,
    mem_marks: bool,
) -> String {
    let mut glue = String::from("(() => {\n");
    for (index, file) in entries.iter().enumerate() {
        if file.feature.is_some_and(|f| !feature_on(f)) {
            continue;
        }
        let body = read(file.name);
        // Node's lib modules (`defineModule(name, function (module, exports, …) {…})`) each run
        // once too: mark them like the lazy files' bodies.
        let body = body.replace(
            "function (module, exports, require, internalBinding, primordials) {",
            "function (module, exports, require, internalBinding, primordials) {\"lumen:run-once\";",
        );
        // A memory checkpoint after each file, for LUMEN_MEM_STATS (see lumen-host).
        let mark = if mem_marks {
            format!(
                "\nif (typeof __lumenMemMark === \"function\") __lumenMemMark({:?});\n",
                file.name
            )
        } else {
            String::new()
        };
        if file.lazy {
            assert!(file.wrap, "{}: a lazy glue file must be wrapped", file.name);
            let [builtins, internals, globals] = lazy_names(file.name, &body);
            // The `"lumen:run-once"` directive (lumen's `serialize::RUN_ONCE`): the body runs
            // once, on the tree-walker, and the collector can release it afterwards.
            glue.push_str(&format!(
                "__lazyGlue({index}, {builtins:?}, {internals:?}, {globals:?}, () => {{\n\"lumen:run-once\";\n"
            ));
            glue.push_str(&body);
            glue.push_str(&mark);
            glue.push_str("\n});\n");
            continue;
        }
        if index > 0 {
            // preamble.js (index 0) declares it.
            glue.push_str(&format!("__glueIndex = {index};\n"));
        }
        if file.wrap {
            glue.push_str("{\n");
            glue.push_str(&body);
            glue.push_str("\n}\n");
        } else {
            glue.push_str(&body);
        }
        glue.push_str(&mark);
    }
    // Registrations made after startup (by code the glue runs later) win over any lazy file's.
    glue.push_str("\n__glueIndex = 1e9;\n})();");
    glue
}

/// The names a lazy glue file registers, as `__lazyGlue` takes them: the `__builtins` names, the
/// `__internals` names, and the globals it defines at (near) top level — space-separated.
fn lazy_names(file: &str, body: &str) -> [String; 3] {
    let literal_args = |call: &str| -> Vec<String> {
        let mut names = Vec::new();
        let mut rest = body;
        while let Some(at) = rest.find(call) {
            rest = &rest[at + call.len()..];
            let arg = rest.trim_start();
            let Some(arg) = arg.strip_prefix('"') else {
                panic!("{file}: {call}…) needs a string-literal name to load lazily (see LAZY)");
            };
            let name = &arg[..arg.find('"').expect("unterminated name")];
            if !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
        names
    };
    let builtins = literal_args("__builtins.set(");
    let internals = literal_args("__internals.set(");
    let mut globals: Vec<String> = Vec::new();
    for line in body.lines() {
        let indent = line.len() - line.trim_start().len();
        let line = line.trim_start();
        if indent > 2 {
            continue;
        }
        let name = if let Some(rest) = line.strip_prefix("Object.defineProperty(globalThis, \"") {
            rest.split('"').next()
        } else if let Some(rest) = line.strip_prefix("globalThis.") {
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
                .unwrap_or(rest.len());
            rest[end..].trim_start().strip_prefix('=').filter(|r| !r.starts_with('=')).map(|_| &rest[..end])
        } else {
            None
        };
        if let Some(name) = name {
            if !globals.iter().any(|g| g == name) {
                globals.push(name.to_string());
            }
        }
    }
    assert!(
        !builtins.is_empty() || !internals.is_empty() || !globals.is_empty(),
        "{file}: a lazy glue file must register something (see LAZY)"
    );
    [builtins.join(" "), internals.join(" "), globals.join(" ")]
}
