//! A classic script's persistent parse cache: a miss parses and hands back a blob, a hit decodes
//! it and runs the same program (strictness and lazily parsed bodies included), and a blob from
//! another source is ignored.
#![cfg(feature = "embed")]
use std::rc::Rc;

use lumen::{ClassicScriptContext, Completion, Engine};

const SOURCE: &str = r#""use strict";
function lazy(n) { var s = 0; for (var i = 0; i < n; i++) s += i; return s; }
var sloppy; try { undeclared = 1; sloppy = true; } catch (e) { sloppy = false; }
globalThis.out = lazy(10) + ":" + sloppy + ":" + (function () { return this; })();
"#;

fn run(source: &str, cached: Option<&[u8]>) -> (String, Option<Vec<u8>>) {
    let mut engine = Engine::new();
    let context = Rc::new(ClassicScriptContext { base_url: "https://example.test/app.js".into(), ..Default::default() });
    let (result, fresh) = engine.ctx().run_classic_script_cached(source, context, cached, true);
    assert!(result.is_ok());
    match engine.eval("out", false).unwrap() {
        Completion::Value(v) => (v, fresh),
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

#[test]
fn hit_runs_the_same_program_as_a_parse() {
    let (parsed, blob) = run(SOURCE, None);
    assert_eq!(parsed, "45:false:undefined");
    let blob = blob.expect("a miss records a blob");
    let (decoded, fresh) = run(SOURCE, Some(&blob));
    assert!(fresh.is_none(), "the blob should have been used");
    assert_eq!(decoded, parsed);
}

#[test]
fn blob_for_another_source_is_ignored() {
    let (_, blob) = run(SOURCE, None);
    let other = SOURCE.replace("lazy(10)", "lazy(11)");
    let (out, fresh) = run(&other, blob.as_deref());
    assert_eq!(out, "55:false:undefined");
    assert!(fresh.is_some());
    let (out, fresh) = run(SOURCE, Some(b"garbage"));
    assert_eq!(out, "45:false:undefined");
    assert!(fresh.is_some());
}
