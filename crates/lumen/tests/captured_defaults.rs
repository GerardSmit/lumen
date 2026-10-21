//! Defaulted parameters captured by closures across every execution tier.
use lumen::{bytecode::Tier, Completion, Engine};

fn check(source: &str) {
    for tier in [Tier::Interp, Tier::Bytecode] {
        let mut e = Engine::new();
        e.set_tier(tier);
        e.set_tier_threshold(0);
        let script = format!(
            "function assert(x) {{ if (!x) throw new Error('assertion'); }}\n{source}\n'passed'"
        );
        match e.eval(&script, false).unwrap() {
            Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
            Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
        }
    }
}

#[test]
fn fresh_defaults_and_explicit_arguments_keep_identity() {
    check(
        r#"
        function object(x={}) { return () => x; }
        function array(x=[]) { return () => x; }
        function number(x=7) { return () => ++x; }
        const supplied={};
        for(let i=0;i<300;i++) {
            const a=object(), b=object(undefined);
            assert(a()===a() && a()!==b() && object(supplied)()===supplied);
            assert(object(null)()===null && object(false)()===false);
            assert(array()()!==array()());
            const n=number(); assert(n()===8 && n()===9);
        }
    "#,
    );
}

#[test]
fn later_defaults_and_body_initializers_observe_initialized_capture() {
    check(
        r#"
        function later(x={}, y=x) { return [() => x,y]; }
        function hoisted(x={}) { function read() { return x; } return read; }
        function overwrite(x={}) { const read=()=>x; x={changed:true}; return read; }
        for(let i=0;i<300;i++) {
            const pair=later(); assert(pair[0]()===pair[1]);
            const obj={}; assert(hoisted(obj)()===obj && typeof hoisted()()==='object');
            assert(overwrite()().changed);
        }
    "#,
    );
}

#[test]
fn hoist_conflicts_and_effectful_defaults_preserve_order() {
    check(
        r#"
        function conflict(x={}) { function x() { return 9; } return () => x; }
        let log='';
        function effect(x=(log+='a',{}), y=(log+='b',x)) { return () => y; }
        for(let i=0;i<100;i++) {
            assert(conflict()()()===9);
            log=''; assert(typeof effect()()==='object' && log==='ab');
        }
    "#,
    );
}

#[test]
fn outer_locals_used_only_in_destructured_parameter_defaults_are_captured() {
    check(
        r#"
        function t1(){var X=7; return function({t=X}={}){return t}}
        function t2(){var X=7; return function({q:t=X}={}){return t}}
        function t3(){var X=7; return function([t=X]=[]){return t}}
        function t4(){var X=7; return ({t=X}={})=>t}
        function t5(){var X=7; return function({a:{b=X}={}}={}){return b}}
        function t6(){var K='k',V=9; return function({[K]:v=V}={}){return v}}
        function t7(){var K='k'; return function({[K]:v}={k:5}){return v}}
        for(let i=0;i<100;i++) {
            assert(t1()()===7 && t1()({})===7 && t1()({t:1})===1);
            assert(t2()()===7 && t2()({})===7);
            assert(t3()()===7 && t3()([])===7);
            assert(t4()()===7 && t4()({})===7);
            assert(t5()()===7 && t5()({a:{}})===7);
            assert(t6()()===9 && t6()({})===9);
            assert(t7()()===5);
        }
    "#,
    );
}
