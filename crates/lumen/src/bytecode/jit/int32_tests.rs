use super::{build, Kind, Widen};
use crate::{bytecode::Tier, Completion, Engine, JitMode};
use lumen_codegen::{CheckedOp, InstData};

#[test]
fn vector_loop_shapes_match_compiler_output() {
    use crate::bytecode::{CmpKind, Op, UpdKind};
    for source in [
        "function copy(out,a,n){for(var i=0;i<n;i++)out[i]=a[i];}",
        "function add(out,a,b,n){for(var i=0;i<n;i++)out[i]=a[i]+b[i];}",
        "function mul(out,a,b,n){for(var i=0;i<n;i++)out[i]=a[i]*b[i];}",
    ] {
        let program = crate::parser::parse_script(source, false).unwrap();
        let crate::ast::Stmt::FuncDecl(function) = &program[0] else {
            unreachable!()
        };
        let chunk = crate::bytecode::compile(function).unwrap();
        let ops = chunk.jit_ops();
        let (header, index) = ops
            .iter()
            .enumerate()
            .find_map(|(pc, op)| match op {
                Op::JumpIfNotCmpLL(CmpKind::Lt, index, _, _) => Some((pc, *index)),
                _ => None,
            })
            .expect("counted loop header");
        assert!(
            matches!(ops[header+1], Op::LoadLocal(s) if s==index),
            "{ops:?}"
        );
        assert!(
            matches!(ops[header+2], Op::LoadLocal(s) if s==index),
            "{ops:?}"
        );
        assert!(matches!(ops[header + 3], Op::GetElemLocal(_)), "{ops:?}");
        let store = ops
            .iter()
            .position(|op| matches!(op, Op::SetElemLocalDrop(_)))
            .unwrap();
        assert!(
            matches!(ops[store+1], Op::UpdateLocal(s,UpdKind::IncDiscard) if s==index),
            "{ops:?}"
        );
        assert!(
            matches!(ops[store+2], Op::Jump(pc) if pc as usize==header),
            "{ops:?}"
        );
    }
}

#[test]
fn jit_code_map_reports_native_lifetimes() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static LOADED: AtomicUsize = AtomicUsize::new(0);
    static RETIRED: AtomicUsize = AtomicUsize::new(0);
    crate::set_jit_code_hook(|event| {
        if event.name.contains("code_map_probe") {
            assert_ne!(event.start, 0);
            assert_ne!(event.length, 0);
            if event.loaded {
                LOADED.fetch_add(1, Ordering::SeqCst);
            } else {
                RETIRED.fetch_add(1, Ordering::SeqCst);
            }
        }
    })
    .unwrap();
    let (_, stats) = evaluate("function code_map_probe(n){var x=0;for(var i=0;i<n;i++)x+=i;return x;}code_map_probe(2048)",
        Tier::Bytecode, JitMode::Eager);
    assert!(stats.executed_entries > 0);
    assert!(LOADED.load(Ordering::SeqCst) > 0);
    assert!(RETIRED.load(Ordering::SeqCst) > 0);
}

#[test]
fn int32_guards_and_widening_keep_number_semantics() {
    for n in [0.0, 1.0, -1.0, i32::MIN as f64, i32::MAX as f64] {
        assert!(Kind::Int32.accepts(&crate::value::Value::Num(n)));
    }
    for n in [
        -0.0,
        0.5,
        i32::MAX as f64 + 1.0,
        i32::MIN as f64 - 1.0,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ] {
        assert!(!Kind::Int32.accepts(&crate::value::Value::Num(n)));
        assert!(Kind::Num.accepts(&crate::value::Value::Num(n)));
    }
    let mut interp = crate::interpreter::Interp::new();
    let env = interp.global_env.clone();
    let program = crate::parser::parse_script(
        "function f(x,y) { x += y; x *= y; x -= y; x++; return x; }",
        false,
    )
    .unwrap();
    let crate::ast::Stmt::FuncDecl(function) = &program[0] else {
        panic!("function")
    };
    let chunk = crate::bytecode::compile(function).unwrap();
    let mut slots = vec![crate::value::Value::Undefined; chunk.n_slots];
    slots[0] = crate::value::Value::Num(2.0);
    slots[1] = crate::value::Value::Num(3.0);
    let compiled = build::build_fn(
        &mut interp,
        &env,
        &chunk,
        &slots,
        &[],
        &[],
        true,
        &crate::value::Value::Undefined,
    )
    .unwrap();
    assert_eq!(compiled.kinds[0], Kind::Int32);
    assert_eq!(compiled.kinds[1], Kind::Int32);
    for want in [CheckedOp::IaddOv, CheckedOp::IsubOv, CheckedOp::ImulOv] {
        assert!(
            compiled.func.insts.iter().any(|i| matches!(i,
            InstData::CheckedBinary { op, .. } if *op == want)),
            "missing {want:?}"
        );
    }
    let compiled = build::build_fn(
        &mut interp,
        &env,
        &chunk,
        &slots,
        &[Widen::Number, Widen::Number],
        &[],
        true,
        &crate::value::Value::Undefined,
    )
    .unwrap();
    assert_eq!(compiled.kinds[0], Kind::Num);
    assert_eq!(compiled.kinds[1], Kind::Num);
}

fn evaluate(src: &str, tier: Tier, mode: JitMode) -> (String, crate::JitStats) {
    let mut engine = Engine::new();
    engine.set_tier(tier);
    engine.set_tier_threshold(0);
    engine.set_jit_mode(mode);
    let Completion::Value(v) = engine.eval(src, false).unwrap() else {
        panic!("threw")
    };
    (v, engine.jit_stats())
}

#[test]
fn inline_atomics_keep_identity_coercion_unsigned_and_exchange_semantics() {
    let source = r#"
        var a = new Int32Array(new SharedArrayBuffer(16));
        function add(a,x) { return Atomics.add(a,0,x); }
        function cas(a,x,y) { return Atomics.compareExchange(a,0,x,y); }
        for(var k=0;k<2048;k++) { add(a,1); cas(a,-1,7); }
        var u = new Uint32Array(a.buffer);
        var rows=[a[0],cas(a,2048,-1),add(u,0),cas(u,-1,9),u[0]];
        var n=0; rows.push(add(a,{valueOf:function(){n++;return 2}}),n,a[0]);
        var old=Atomics.add; Atomics.add=function(){return 99};
        rows.push(add(a,1),a[0]); Atomics.add=old;
        JSON.stringify(rows);
    "#;
    let expected = evaluate(source, Tier::Bytecode, JitMode::Disabled).0;
    for mode in [JitMode::Hot, JitMode::Eager] {
        let (actual, stats) = evaluate(source, Tier::Bytecode, mode);
        assert_eq!(actual, expected);
        assert!(stats.executed_entries > 0);
    }
    let mut interp = crate::interpreter::Interp::new();
    let env = interp.global_env.clone();
    let program =
        crate::parser::parse_script("function f(a,x) { return Atomics.add(a,0,x); }", false)
            .unwrap();
    let crate::ast::Stmt::FuncDecl(function) = &program[0] else {
        panic!("function")
    };
    let chunk = crate::bytecode::compile(function).unwrap();
    let mut slots = vec![crate::value::Value::Undefined; chunk.n_slots];
    slots[1] = crate::value::Value::Num(1.0);
    let built = build::build_fn(
        &mut interp,
        &env,
        &chunk,
        &slots,
        &[],
        &[],
        true,
        &crate::value::Value::Undefined,
    )
    .unwrap();
    assert!(built
        .func
        .funcs
        .iter()
        .any(|f| f.id == lumen_codegen::atomics::ADD32));
}

#[test]
fn int32_native_arithmetic_matches_interpreter_on_guard_exits() {
    for op in ["+", "-", "*", "/", "%", "&", "|", "^", "<<", ">>", ">>>"] {
        let src = format!(
            r#"
            function f(x,y) {{ x = x {op} y; return x; }}
            for (var k=0; k<2048; k++) f(12,3);
            var cases = [[2147483647,1],[-2147483648,-1],[0,-1],[-4,2],
                [7,2],[7,0],[0,0],[50000,50000],[-2147483648,0],
                [3.5,2],[-0,1],[NaN,1],[Infinity,2],['7',2]];
            cases.map(function(v) {{ var r=f(v[0],v[1]);
                return Object.is(r,-0) ? '-0' : String(r); }}).join(':')
        "#
        );
        let expected = evaluate(&src, Tier::Interp, JitMode::Disabled).0;
        for mode in [JitMode::Disabled, JitMode::Hot, JitMode::Eager] {
            let (actual, stats) = evaluate(&src, Tier::Bytecode, mode);
            assert_eq!(actual, expected, "{op} {mode:?}");
            if mode != JitMode::Disabled
                && cfg!(any(target_arch = "x86_64", target_arch = "aarch64"))
            {
                assert!(
                    stats.executed_entries > 0,
                    "no native execution: {op} {mode:?}"
                );
                assert_eq!(stats.failed_compilations, 0, "{op} {mode:?}");
            }
        }
    }
}

#[test]
fn int32_updates_stores_and_coercion_exit_once() {
    let src = r#"
        function inc(x) { var old=x++; return old+':'+x; }
        function dec(x) { var old=x--; return old+':'+x; }
        function store(x,y) { x=y; return Object.is(x,-0) ? '-0' : String(x); }
        function add(x,y) { x+=y; return x; }
        for(var k=0;k<2048;k++) { inc(k); dec(k); store(k,k); add(k,1); }
        var calls=0, obj={valueOf:function(){ calls++; return 0.5; }};
        [inc(2147483647), dec(-2147483648), store(1,-0), store(1,0.5),
         add(1,obj), calls, add(1,'x')].join('|')
    "#;
    let expected = evaluate(src, Tier::Interp, JitMode::Disabled).0;
    for mode in [JitMode::Disabled, JitMode::Hot, JitMode::Eager] {
        assert_eq!(evaluate(src, Tier::Bytecode, mode).0, expected, "{mode:?}");
    }
}

#[test]
fn int32_loop_locals_and_tail_call_guards_preserve_state() {
    let src = r#"
        function sum(n) { var s=0; for(var i=0;i<n;i++) s+=i; return s; }
        function mix(n) { var x=0; for(var i=0;i<n;i++) { x+=i; if(i===1500) x=0.5; } return x; }
        function recurse(n,x) { if(n===0) return x; return recurse(n-1,x+1); }
        function unset(n) { var x; if(n) x=0; x++; return String(x); }
        for(var k=0;k<2048;k++) { sum(4); recurse(3,1); unset(1); }
        [sum(4000),mix(4000),recurse(4,2147483646),unset(0)].join('|')
    "#;
    let expected = evaluate(src, Tier::Interp, JitMode::Disabled).0;
    for mode in [JitMode::Disabled, JitMode::Hot, JitMode::Eager] {
        assert_eq!(evaluate(src, Tier::Bytecode, mode).0, expected, "{mode:?}");
    }
    let mut interp = crate::interpreter::Interp::new();
    let env = interp.global_env.clone();
    let program = crate::parser::parse_script(
        "function sum(n) { var s=0; for(var i=0;i<n;i++) s+=i; return s; }",
        false,
    )
    .unwrap();
    let crate::ast::Stmt::FuncDecl(function) = &program[0] else {
        panic!("function")
    };
    let chunk = crate::bytecode::compile(function).unwrap();
    let mut slots = vec![crate::value::Value::Undefined; chunk.n_slots];
    slots[0] = crate::value::Value::Num(4.0);
    let compiled = build::build_fn(
        &mut interp,
        &env,
        &chunk,
        &slots,
        &[],
        &[],
        true,
        &crate::value::Value::Undefined,
    )
    .unwrap();
    assert!(
        compiled
            .kinds
            .iter()
            .skip(1)
            .filter(|k| **k == Kind::Int32)
            .count()
            >= 2
    );
    lumen_codegen::verify::verify(&compiled.func).unwrap();
}
