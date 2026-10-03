use super::{CODE_CACHE, FS_DEAD};
use crate::{Completion, Engine, JitMode, JitStats, bytecode::Tier};

fn engine(mode: JitMode) -> Engine {
    let mut engine = Engine::new();
    engine.set_tier(Tier::Bytecode);
    engine.set_tier_threshold(0);
    engine.set_jit_mode(mode);
    engine
}

fn run(engine: &mut Engine, src: &str) -> String {
    match engine.eval(src, false).unwrap() {
        Completion::Value(v) => v,
        _ => panic!("threw"),
    }
}

fn functions(count: usize) -> String {
    (0..count)
        .map(|k| format!("function f{k}(x){{var s=0;for(var j=0;j<16;j++){{s+=x*j+{k};}}return s;}}\n"))
        .collect()
}

fn call_all(count: usize, rounds: usize) -> (String, i64) {
    let body: String = (0..rounds)
        .flat_map(|r| (0..count).map(move |k| format!("t+=f{k}({r});")))
        .collect();
    let want = (0..rounds as i64)
        .flat_map(|r| (0..count as i64).map(move |k| r * 120 + 16 * k))
        .sum();
    (format!("var t=0;{body}String(t)"), want)
}

fn dead_chunks() -> usize {
    CODE_CACHE.with(|cache| {
        cache
            .borrow()
            .iter()
            .filter_map(std::rc::Weak::upgrade)
            .filter(|chunk| chunk.jit.fstate.get() == FS_DEAD)
            .count()
    })
}

fn unit_bytes() -> usize {
    let mut engine = engine(JitMode::Eager);
    run(&mut engine, &functions(1));
    let (call, _) = call_all(1, 2);
    run(&mut engine, &call);
    let stats = engine.jit_stats();
    assert!(stats.compiled_units > 0);
    stats.code_bytes / stats.compiled_units as usize
}

struct Limit;

impl Limit {
    fn set(bytes: usize) -> Limit {
        lumen_os::jitmem::set_thread_exec_limit(Some(bytes));
        Limit
    }
}

impl Drop for Limit {
    fn drop(&mut self) {
        lumen_os::jitmem::set_thread_exec_limit(None);
    }
}

const COUNT: usize = 24;

#[test]
fn full_arena_evicts_least_recently_used_code_and_keeps_compiling() {
    let unit = unit_bytes();
    let _limit = Limit::set(unit * 4);
    let mut engine = engine(JitMode::Eager);
    run(&mut engine, &functions(COUNT));
    let (call, want) = call_all(COUNT, 3);
    assert_eq!(run(&mut engine, &call), want.to_string());
    let stats: JitStats = engine.jit_stats();
    assert!(stats.evictions > 0, "{stats:?}");
    assert!(stats.evicted_bytes > 0, "{stats:?}");
    assert!(stats.compiled_units as usize >= COUNT, "{stats:?}");
    assert_eq!(stats.allocation_failures, 0, "{stats:?}");
    assert_eq!(dead_chunks(), 0);
    assert!(stats.code_bytes - stats.evicted_bytes <= unit * 4 + unit, "{stats:?}");
}

#[test]
fn evicted_function_recompiles_when_hot_again() {
    let unit = unit_bytes();
    let _limit = Limit::set(unit * 3);
    let mut engine = engine(JitMode::Eager);
    run(&mut engine, &functions(COUNT));
    let (call, want) = call_all(COUNT, 1);
    assert_eq!(run(&mut engine, &call), want.to_string());
    let before = engine.jit_stats();
    assert!(before.evictions > 0, "{before:?}");
    let (again, want) = call_all(COUNT, 2);
    assert_eq!(run(&mut engine, &again), want.to_string());
    let after = engine.jit_stats();
    assert!(after.compiled_units > before.compiled_units, "{before:?} {after:?}");
    assert!(after.evictions > before.evictions, "{before:?} {after:?}");
    assert_eq!(dead_chunks(), 0);
}

#[test]
fn hot_mode_functions_are_not_made_permanently_dead_by_a_full_arena() {
    let unit = unit_bytes();
    let _limit = Limit::set(unit * 3);
    let mut engine = engine(JitMode::Hot);
    run(&mut engine, &functions(COUNT));
    let rounds = 3000;
    let body: String = (0..COUNT).map(|k| format!("t+=f{k}(r);")).collect();
    let src = format!("var t=0;for(var r=0;r<{rounds};r++){{{body}}}String(t)");
    let want: i64 = (0..rounds as i64)
        .map(|r| (0..COUNT as i64).map(|k| r * 120 + 16 * k).sum::<i64>())
        .sum();
    assert_eq!(run(&mut engine, &src), want.to_string());
    let stats = engine.jit_stats();
    assert!(stats.compiled_units > 0, "{stats:?}");
    assert_eq!(dead_chunks(), 0, "{stats:?}");
}

#[test]
fn code_on_the_stack_is_never_evicted() {
    let unit = unit_bytes();
    let _limit = Limit::set(unit * 3);
    let mut engine = engine(JitMode::Eager);
    run(&mut engine, &functions(COUNT));
    let calls: String = (0..COUNT).map(|k| format!("s+=f{k}(i&7);")).collect();
    let src = format!(
        "function outer(n){{var s=0;for(var i=0;i<n;i++){{{calls}}}return s;}}
         function rec(d){{if(d==0)return 0;var s=0;{}return s+rec(d-1);}}
         String(outer(40)+':'+rec(30))",
        (0..COUNT).map(|k| format!("s+=f{k}(d);")).collect::<String>()
    );
    let outer: i64 = (0..40i64)
        .map(|i| (0..COUNT as i64).map(|k| (i & 7) * 120 + 16 * k).sum::<i64>())
        .sum();
    let rec: i64 = (1..=30i64)
        .map(|d| (0..COUNT as i64).map(|k| d * 120 + 16 * k).sum::<i64>())
        .sum();
    assert_eq!(run(&mut engine, &src), format!("{}:{}", outer, rec));
    let stats = engine.jit_stats();
    assert!(stats.evictions > 0, "{stats:?}");
    assert_eq!(dead_chunks(), 0);
}

#[test]
fn pressure_generation_makes_an_idle_engine_trim_cold_code() {
    let mut engine = engine(JitMode::Eager);
    run(&mut engine, &functions(8));
    run(&mut engine, "function g(x){return x+1;}");
    let (call, want) = call_all(8, 2);
    assert_eq!(run(&mut engine, &call), want.to_string());
    let before = engine.jit_stats();
    let spin = "var u=0;for(var i=0;i<300000;i++){u=g(u);}String(u)";
    for _ in 0..3 {
        crate::request_jit_trim();
        assert_eq!(run(&mut engine, spin), "300000");
    }
    let after = engine.jit_stats();
    assert!(after.evictions >= 8, "{before:?} {after:?}");
    assert!(after.evicted_bytes > 0, "{after:?}");
    assert_eq!(run(&mut engine, &call), want.to_string());
}
