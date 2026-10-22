use crate::{Engine, JitMode};

fn leaked_objects(setup: &str, call: &str) -> i64 {
    let mut engine = Engine::new();
    engine.set_jit_mode(JitMode::Eager);
    engine.eval(setup, false).unwrap();
    let warm = format!("for (var i = 0; i < 20; i++) {{ var o = mk(); {call}; }}");
    engine.eval(&warm, false).unwrap();
    engine.interp.gc_collect();
    engine.interp.gc_collect();
    let before = engine.interp.live_object_count();
    let run = format!("for (var i = 0; i < 400; i++) {{ var o = mk(); {call}; }}");
    engine.eval(&run, false).unwrap();
    for _ in 0..3 {
        engine.interp.gc_collect();
    }
    engine.interp.live_object_count() - before
}

const SETUP: &str = "
function mk() { var s = { t: 0, n: 3, u: null }; s.self = s; return s; }
function appendProp(o) { o.t += 1; return 1; }
function appendSibling(o) { var n = o.n; o.t += n; return n; }
function readProp(o) { return o.t; }
function dupProp(o) { o.t++; --o.t; return 1; }
function readElem(o) { return o['t']; }
function compoundElem(o) { o['t'] += 1; return o['t']++; }
";

#[test]
fn fused_property_updates_release_the_receiver() {
    for call in [
        "appendProp(o)",
        "appendSibling(o)",
        "readProp(o)",
        "dupProp(o)",
        "readElem(o)",
        "compoundElem(o)",
    ] {
        let leaked = leaked_objects(SETUP, call);
        assert!(leaked <= 8, "{call} left {leaked} objects alive");
    }
}
