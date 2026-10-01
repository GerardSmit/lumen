//! Precompile the web platform's JS glue to an AST snapshot at build time, so the runtime
//! *decodes* it on boot instead of re-lexing/parsing it — parsing the static glue is the
//! dominant cold-start cost. This assembles the one IIFE that used to live as a `concat!` in
//! `lib.rs` and writes both the assembled source (`web_glue.js`) and its snapshot blob
//! (`web_glue.snap`) to `OUT_DIR`; `lib.rs` `include_*!`s both. Assembling here makes this the
//! single source of truth — the source and its snapshot can't drift.
//!
//! `lumen` is a build-dependency purely for `compile_snapshot` (parse + encode). If the glue
//! ever fails to parse, the build fails here with a clear message.

use std::path::PathBuf;

/// Order matters: `preamble` captures and deletes the raw `__*` namespaces before the standard
/// classes are defined over them. Keep in sync with what each file expects.
///
/// A group of files that share top-level bindings (or patch each other's classes) is one unit;
/// a `lazy` unit's code runs the first time one of the globals it publishes is touched (see
/// `__lazyWeb` in preamble.js), so a program pays only for the web APIs it uses. Eager units
/// are the tiny ones other glue reads at startup.
struct Unit {
    files: &'static [&'static str],
    lazy: bool,
    /// Globals published by `Object.defineProperty` rather than `globalThis.X = …`: name and
    /// whether it is enumerable.
    defined: &'static [(&'static str, bool)],
}

const UNITS: &[Unit] = &[
    Unit { files: &["preamble.js"], lazy: false, defined: &[] },
    Unit { files: &["events.js", "messaging.js"], lazy: true, defined: &[("__cloneTransferableSignal", false), ("__eventTargetInternals", false)] },
    Unit { files: &["encoding.js", "serialize.js"], lazy: true, defined: &[] },
    Unit { files: &["url.js"], lazy: true, defined: &[("URL", false), ("URLSearchParams", false)] },
    Unit { files: &["urlpattern.js"], lazy: true, defined: &[] },
    Unit { files: &["streams.js"], lazy: true, defined: &[] },
    Unit { files: &["writable.js"], lazy: true, defined: &[] },
    Unit { files: &["compression.js"], lazy: true, defined: &[] },
    Unit { files: &["blob.js", "fetch.js"], lazy: true, defined: &[] },
    Unit { files: &["websocket.js", "eventsource.js"], lazy: true, defined: &[] },
    Unit { files: &["server.js"], lazy: false, defined: &[] },
    Unit { files: &["crypto.js"], lazy: false, defined: &[] },
    Unit { files: &["platform.js"], lazy: false, defined: &[] },
    Unit { files: &["wasm.js"], lazy: true, defined: &[] },
];

/// The globals a lazy unit publishes: its column-0 `globalThis.X = …` statements, then `defined`.
fn published(body: &str, defined: &[(&str, bool)]) -> Vec<(String, bool)> {
    let mut names: Vec<(String, bool)> = Vec::new();
    for line in body.lines() {
        let Some(rest) = line.strip_prefix("globalThis.") else {
            continue;
        };
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
            .unwrap_or(rest.len());
        let is_assign = rest[end..]
            .trim_start()
            .strip_prefix('=')
            .is_some_and(|r| !r.starts_with('='));
        if end > 0 && is_assign && !names.iter().any(|(n, _)| n == &rest[..end]) {
            names.push((rest[..end].to_string(), true));
        }
    }
    for (name, enumerable) in defined {
        if !names.iter().any(|(n, _)| n == name) {
            names.push((name.to_string(), *enumerable));
        }
    }
    assert!(!names.is_empty(), "a lazy web glue unit must publish a global");
    names
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let src_dir = manifest.join("src/js");

    let mut glue = String::from("(() => {\n");
    for unit in UNITS {
        let mut body = String::new();
        for file in unit.files {
            let path = src_dir.join(file);
            println!("cargo:rerun-if-changed={}", path.display());
            body.push_str(
                &std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("read {}: {e}", path.display())),
            );
            body.push('\n');
        }
        if unit.lazy {
            let names = published(&body, unit.defined);
            let list: Vec<String> = names
                .iter()
                .map(|(n, e)| format!("{n}{}", if *e { "" } else { "!" }))
                .collect();
            glue.push_str(&format!(
                "__lazyWeb({:?}, () => {{\n\"lumen:run-once\";\n{body}}});\n",
                list.join(" ")
            ));
        } else {
            glue.push_str(&body);
        }
    }
    glue.push_str("\n})();");

    // An ahead-of-time blob: AST, bytecode and the compressed function text (for `toString`).
    let blob = lumen::precompiled::precompile_glue(&glue, "web-glue")
        .unwrap_or_else(|e| panic!("web glue failed to precompile: {e}"));

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    // web_glue.js is for reference only (not linked).
    std::fs::write(out.join("web_glue.js"), &glue).unwrap();
    std::fs::write(out.join("web_glue.aot"), &blob).unwrap();
    println!("cargo:rerun-if-changed=build.rs");
}
