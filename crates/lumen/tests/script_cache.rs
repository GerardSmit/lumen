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

const URL: &str = "https://example.test/bodies.js";
const BODIES: &str = r##"
function outer(n) {
  function inner(k) { return k * 2; }
  var deep = function (m) { return inner(m) + 1; };
  return deep(n);
}
function thrower() {
  var x = 1;
  throw new Error("boom " + x);
}
class Box { #v = 3; get() { return (function () { return this.#v; }).call(this); } }
function describe() {
  var lines = [];
  try { thrower(); } catch (e) { lines.push(e.stack.split("\n").slice(0, 3).join("|")); }
  return [outer(5), String(outer), String(thrower), new Box().get(), lines[0]].join("#");
}
globalThis.out = describe();
"##;

fn context() -> Rc<ClassicScriptContext> {
    Rc::new(ClassicScriptContext { base_url: URL.into(), ..Default::default() })
}

fn run_bodies(source: &str, cached: Option<&[u8]>) -> (String, Option<Vec<u8>>, Option<Vec<u8>>) {
    let mut engine = Engine::new();
    let (result, fresh) = engine.ctx().run_classic_script_cached(source, context(), cached, true);
    assert!(result.is_ok());
    let updated = engine.ctx().finish_script_cache(URL);
    match engine.eval("out", false).unwrap() {
        Completion::Value(v) => (v, fresh, updated),
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

fn bodies_section(blob: &[u8]) -> usize {
    let top_len = u64::from_le_bytes(blob[21..29].try_into().unwrap()) as usize;
    29 + top_len
}

#[test]
fn updated_blob_runs_the_same_program() {
    let (parsed, fresh, updated) = run_bodies(BODIES, None);
    assert!(parsed.contains("11#function outer(n)"), "{parsed}");
    assert!(parsed.contains("bodies.js:9"), "{parsed}");
    let fresh = fresh.expect("a miss records a blob");
    let updated = updated.expect("the bodies parsed by the run are recorded");
    assert!(updated.len() > fresh.len());
    let mut blob = updated;
    for _ in 0..3 {
        let (decoded, fresh, next) = run_bodies(BODIES, Some(&blob));
        assert_eq!(decoded, parsed);
        assert!(fresh.is_none());
        if let Some(next) = next {
            assert!(next.len() >= blob.len());
            blob = next;
        }
    }
}

#[test]
fn corrupted_bodies_fall_back_to_parsing() {
    let (parsed, _, updated) = run_bodies(BODIES, None);
    let blob = updated.unwrap();

    let truncated = &blob[..blob.len() - 5];
    let (out, fresh, _) = run_bodies(BODIES, Some(truncated));
    assert_eq!(out, parsed);
    assert!(fresh.is_some(), "a malformed blob is replaced");

    let mut garbled = blob.clone();
    let entry = bodies_section(&garbled) + 4;
    let len = u32::from_le_bytes(garbled[entry + 8..entry + 12].try_into().unwrap()) as usize;
    garbled[entry + 12..entry + 12 + len].fill(0xff);
    let (out, fresh, repaired) = run_bodies(BODIES, Some(&garbled));
    assert_eq!(out, parsed);
    assert!(fresh.is_none());
    let repaired = repaired.expect("bodies that fail to decode are recorded again");
    let (out, _, _) = run_bodies(BODIES, Some(&repaired));
    assert_eq!(out, parsed);
}

#[test]
fn early_errors_do_not_depend_on_the_cache() {
    const BAD: &str = "function a() { function b() { function c() { let q = 1; let q = 2; } } }\nglobalThis.out = 1;";
    let (_, _, updated) = run_bodies(BODIES, None);
    let blob = updated.unwrap();
    let message = |engine: &mut Engine, result: Result<_, _>| match result {
        Ok(_) => String::from("no error"),
        Err(abrupt) => {
            let thrown = lumen::embed::abrupt_value(abrupt);
            match engine.ctx().to_string(&thrown) {
                Ok(text) => text.to_string(),
                Err(_) => panic!("thrown value has no text"),
            }
        }
    };
    let mut engine = Engine::new();
    let plain = engine.ctx().run_classic_script(BAD, context());
    let plain = message(&mut engine, plain);
    assert!(plain.starts_with("SyntaxError"), "{plain}");
    let mut engine = Engine::new();
    let (cached, fresh) = engine.ctx().run_classic_script_cached(BAD, context(), Some(&blob), true);
    assert!(fresh.is_none());
    assert_eq!(message(&mut engine, cached), plain);
}

#[test]
fn finished_cache_is_closed() {
    let mut engine = Engine::new();
    let (result, _) = engine.ctx().run_classic_script_cached(BODIES, context(), None, true);
    assert!(result.is_ok());
    assert!(engine.ctx().finish_script_cache(URL).is_some());
    assert!(engine.ctx().finish_script_cache(URL).is_none());
    match engine.eval("out", false).unwrap() {
        Completion::Value(v) => assert!(v.contains("11#function outer(n)"), "{v}"),
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
    assert!(engine.ctx().finish_script_cache(URL).is_none());
}
