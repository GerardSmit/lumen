//! Pathologically deep or long source constructs must fail with a catchable error (SyntaxError
//! from the parser, RangeError from later phases) on a default 2 MiB thread, never abort the
//! process; and nesting must parse in linear time.
use lumen::{bytecode::Tier, Completion, Engine};
use std::time::{Duration, Instant};

const SMALL_STACK: usize = 2 * 1024 * 1024;

const PRELUDE: &str = "globalThis.A = function () {}; globalThis.B = function () {}; \
    globalThis.a = function () { return a; }; globalThis.b = 1;";

/// Run `(0, eval)(<expr>)` on a fresh engine on a 2 MiB thread; the result is `ok` or the
/// thrown error's name.
fn probe(expr: &str, tier: Tier) -> String {
    let src = format!(
        "{PRELUDE} try {{ (0, eval)({expr}); 'ok' }} catch (e) {{ e && e.name || String(e) }}"
    );
    std::thread::Builder::new()
        .stack_size(SMALL_STACK)
        .spawn(move || {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            match engine.eval(&src, false) {
                Ok(Completion::Value(v)) => v,
                Ok(Completion::Throw { name, message }) => format!("uncaught {name}: {message}"),
                Err(e) => format!("ParseError: {}", e.message),
            }
        })
        .unwrap()
        .join()
        .unwrap()
}

fn check(expr: &str, allowed: &[&str]) {
    for tier in [Tier::Interp, Tier::Bytecode] {
        let r = probe(expr, tier);
        assert!(
            allowed.contains(&r.as_str()),
            "{expr} ({tier:?}): got {r}, expected one of {allowed:?}"
        );
    }
}

const ERR: &[&str] = &["SyntaxError", "RangeError"];
const ANY: &[&str] = &["ok", "SyntaxError", "RangeError", "TypeError"];

macro_rules! probes {
    ($($name:ident: $expr:expr => $allowed:expr;)*) => {
        $(#[test] fn $name() { check($expr, $allowed); })*
    };
}

probes! {
    arrow_chain: "'x=>'.repeat(1e6) + '1'" => ERR;
    async_arrow_chain: "'async x=>'.repeat(1e6) + '1'" => ERR;
    yield_chain: "'function*g(){' + 'yield '.repeat(1e6) + '1}'" => ERR;
    call_chain: "'a' + '()'.repeat(1e6)" => ANY;
    optional_chain: "'a' + '?.b'.repeat(1e6)" => ANY;
    tagged_template_chain: "'a' + '`x`'.repeat(1e6)" => ANY;
    new_chain: "'new '.repeat(1e6) + 'A'" => ERR;
    new_call_chain: "'new a' + '()'.repeat(1e6)" => ANY;
    nullish_chain: "'a??'.repeat(1e6) + 'b'" => ANY;
    logical_or_chain: "'b||'.repeat(1e6) + 'b'" => ANY;
    plus_chain: "'b+'.repeat(1e6) + 'b'" => ANY;
    comma_chain: "'b,'.repeat(1e6) + 'b'" => ANY;
    member_chain: "'a' + '.b'.repeat(1e6)" => ANY;
    index_chain: "'a' + '[0]'.repeat(1e6)" => ANY;
    array_pattern_arrow: "'(' + '['.repeat(1e6) + 'a' + ']'.repeat(1e6) + ')=>1'" => ERR;
    object_pattern_arrow: "'(' + '{a:'.repeat(1e6) + 'a' + '}'.repeat(1e6) + ')=>1'" => ERR;
    class_heritage_expr: "'x=' + 'class extends '.repeat(3e5) + 'Object' + '{}'.repeat(3e5)" => ERR;
    class_heritage_decl: "'class A extends '.repeat(1e5) + 'B{}'.repeat(1e5)" => ERR;
    template_nesting: "'`${'.repeat(1e6) + 'b' + '}`'.repeat(1e6)" => ERR;
    paren_nesting: "'('.repeat(1e6) + 'b' + ')'.repeat(1e6)" => ERR;
    cond_paren_nesting: "'(1?'.repeat(1e6) + '1' + ':1)'.repeat(1e6)" => ERR;
    assign_pattern_nesting: "'({b}='.repeat(1e6) + '{}' + ')'.repeat(1e6)" => ERR;
    // Near the parser's chain budget: later phases must throw rather than overflow.
    call_chain_near_budget: "'a' + '()'.repeat(300)" => ANY;
    logical_chain_near_budget: "'b||'.repeat(300) + 'b'" => ANY;
    tagged_chain_near_budget: "'a' + '`x`'.repeat(300)" => ANY;
    template_parts_near_budget: "'`' + '${b}'.repeat(150) + '`'" => ANY;
}

/// Parse times of `f(n)` and `f(4n)`: linear is ~4x, quadratic ~16x.
fn assert_linear(f: impl Fn(usize) -> String, n: usize) {
    let time = |src: String| {
        std::thread::Builder::new()
            .stack_size(256 << 20)
            .spawn(move || {
                let mut engine = Engine::new();
                let src = format!("if (0) {src};");
                let t = Instant::now();
                let r = engine.eval(&src, false);
                assert!(
                    matches!(r, Ok(Completion::Value(_))),
                    "{:?}",
                    r.err().map(|e| e.message)
                );
                t.elapsed()
            })
            .unwrap()
            .join()
            .unwrap()
    };
    let best = |n: usize| (0..3).map(|_| time(f(n))).min().unwrap();
    let small = best(n);
    let large = best(n * 4);
    assert!(
        large < small * 7 + Duration::from_millis(2),
        "parse time not linear: {small:?} at {n}, {large:?} at {}",
        n * 4
    );
}

#[test]
fn template_nesting_parses_in_linear_time() {
    assert_linear(|n| format!("{}b{}", "`${".repeat(n), "}`".repeat(n)), 200);
}

#[test]
fn cond_paren_nesting_parses_in_linear_time() {
    assert_linear(|n| format!("{}1{}", "(1?".repeat(n), ":1)".repeat(n)), 250);
}

#[test]
fn paren_nesting_parses_in_linear_time() {
    assert_linear(|n| format!("{}b{}", "(".repeat(n), ")".repeat(n)), 250);
}

#[test]
fn assign_pattern_nesting_parses_in_linear_time() {
    assert_linear(
        |n| format!("{}{{}}{}", "({b}=".repeat(n), ")".repeat(n)),
        250,
    );
}
