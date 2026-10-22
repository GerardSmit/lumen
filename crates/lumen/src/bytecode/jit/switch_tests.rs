use crate::{bytecode::Tier, Completion, Engine, JitMode};

fn evaluate(src: &str, tier: Tier, mode: JitMode) -> (String, crate::JitStats) {
    let mut engine = Engine::new();
    engine.set_tier(tier);
    engine.set_tier_threshold(0);
    engine.set_jit_mode(mode);
    match engine.eval(src, false).expect("parse") {
        Completion::Value(v) => (v, engine.jit_stats()),
        Completion::Throw { name, message } => panic!("threw {name}: {message}"),
    }
}

fn agree_on_every_tier(src: &str, expected: &str, all_compile: bool) {
    for (tier, mode) in [
        (Tier::Interp, JitMode::Disabled),
        (Tier::Bytecode, JitMode::Disabled),
        (Tier::Bytecode, JitMode::Hot),
    ] {
        assert_eq!(evaluate(src, tier, mode).0, expected, "{tier:?} {mode:?}");
    }
    let (got, stats) = evaluate(src, Tier::Bytecode, JitMode::Eager);
    assert_eq!(got, expected, "eager JIT");
    assert!(stats.compiled_units > 0, "nothing was compiled");
    if all_compile {
        assert_eq!(stats.failed_compilations, 0);
    }
}

#[test]
fn switch_dispatch_matches_strict_equality() {
    agree_on_every_tier(
        include_str!("switch_tests/sw1.js"),
        include_str!("switch_tests/sw1.expected"),
        true,
    );
}

#[test]
fn switch_in_compiled_loops_keeps_control_flow() {
    agree_on_every_tier(
        include_str!("switch_tests/sw2.js"),
        include_str!("switch_tests/sw2.expected"),
        true,
    );
}

#[test]
fn typed_array_numeric_keys_follow_integer_indexed_semantics() {
    agree_on_every_tier(
        include_str!("switch_tests/ta1.js"),
        include_str!("switch_tests/ta1.expected"),
        false,
    );
}

#[test]
fn loop_with_a_switch_table_compiles_whole() {
    let src = "function f(n){var m=0,a=0;for(var i=0;i<n;i++){switch(m){case 0:a+=1;m=1;break;case 1:a+=2;m=2;break;case 2:a+=3;m=3;break;case 3:a+=4;m=0;break;default:a=-1}}return a}f(5000)";
    let program = crate::parser::parse_script(src, false).unwrap();
    let crate::ast::Stmt::FuncDecl(function) = &program[0] else {
        panic!("function")
    };
    let chunk = crate::bytecode::compile(function).unwrap();
    assert!(chunk
        .jit_ops()
        .iter()
        .any(|op| matches!(op, crate::bytecode::Op::SwitchLK(..))));
    let (got, stats) = evaluate(src, Tier::Bytecode, JitMode::Eager);
    assert_eq!(got, "12500");
    assert!(stats.compiled_units > 0);
    assert_eq!(stats.failed_compilations, 0);
}

#[test]
fn switch_loop_with_hundreds_of_cases_compiles_within_the_frame_limit() {
    let cases: String = (0..150)
        .map(|k| {
            format!(
                "case {k}: acc=(acc*31+{k}+g(i))|0; o.v{m}=(o.v{m}|0)+acc; break;",
                m = k % 7
            )
        })
        .collect();
    let src = format!(
        "(function(){{function g(x){{return x&3}}var acc=1,o={{v0:0,v1:0,v2:0,v3:0,v4:0,v5:0,v6:0}};\
         for(var i=0;i<20000;i++){{switch(i%150){{{cases}default:acc=0}}}}\
         return acc+','+o.v0+','+o.v3+','+o.v6}})()"
    );
    let (want, _) = evaluate(&src, Tier::Bytecode, JitMode::Disabled);
    let (got, stats) = evaluate(&src, Tier::Bytecode, JitMode::Eager);
    assert_eq!(got, want);
    assert!(stats.compiled_units > 0);
    assert_eq!(stats.failed_compilations, 0);
}
