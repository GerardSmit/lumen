//! The sampling CPU profiler attributes a hot loop to the function running it, with its callers
//! above it, on the tree-walker, the bytecode interpreter and the JIT; it records nothing unless
//! started.
use lumen::{bytecode::Tier, Completion, Engine, JitMode};

const HZ: u32 = 2000;

const SCRIPT: &str = r#"
function inner(n) {
    let s = 0;
    for (let i = 0; i < n; i++) { s = (s + i * 3) % 1000003; }
    return s;
}
function outer() {
    let t = 0;
    const end = Date.now() + 400;
    while (Date.now() < end) { t += inner(20000); }
    return t;
}
outer();
"#;

fn run(engine: &mut Engine) {
    match engine.eval(SCRIPT, false).unwrap() {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

fn names(stack: &str) -> Vec<&str> {
    stack
        .split(';')
        .map(|f| f.split(" (").next().unwrap())
        .collect()
}

/// `(samples with outer;inner as the innermost two frames, samples with outer innermost)`.
fn split(folded: &str) -> (u64, u64) {
    let (mut inner, mut outer) = (0, 0);
    for line in folded.lines() {
        let (stack, count) = line.rsplit_once(' ').unwrap();
        let count: u64 = count.parse().unwrap();
        let frames = names(stack);
        match frames.as_slice() {
            [.., "outer", "inner"] => inner += count,
            [.., "outer"] => outer += count,
            _ => {}
        }
    }
    (inner, outer)
}

fn profiled(tier: Tier, jit: JitMode) -> String {
    let mut engine = Engine::new();
    engine.set_tier(tier);
    engine.set_tier_threshold(0);
    engine.set_jit_mode(jit);
    engine.start_cpu_profile(None, HZ);
    run(&mut engine);
    engine.cpu_profile_folded().unwrap()
}

fn check(tier: Tier, jit: JitMode) {
    let folded = profiled(tier, jit);
    let (inner, outer) = split(&folded);
    assert!(inner > 0, "{tier:?} {jit:?}: no outer;inner stacks in\n{folded}");
    assert!(
        inner > outer,
        "{tier:?} {jit:?}: inner {inner} should dominate outer {outer}\n{folded}"
    );
}

#[test]
fn tree_walker_attributes_the_hot_loop_to_inner() {
    check(Tier::Interp, JitMode::Disabled);
}

#[test]
fn bytecode_interpreter_attributes_the_hot_loop_to_inner() {
    check(Tier::Bytecode, JitMode::Disabled);
}

#[test]
fn default_tier_attributes_the_hot_loop_to_inner() {
    check(Tier::Bytecode, JitMode::Hot);
}

#[test]
fn write_puts_folded_stacks_in_the_file() {
    let path = std::env::temp_dir().join(format!("lumen-cpu-profile-{}.folded", std::process::id()));
    let mut engine = Engine::new();
    engine.start_cpu_profile(Some(path.clone()), HZ);
    run(&mut engine);
    assert_eq!(engine.write_cpu_profile().as_deref(), Some(path.as_path()));
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(split(&text).0 > 0, "{text}");
    let _ = std::fs::remove_file(&path);
    let mut lines = path.into_os_string();
    lines.push(".lines");
    let _ = std::fs::remove_file(std::path::PathBuf::from(lines));
}

#[test]
fn disabled_profiler_produces_nothing() {
    let mut engine = Engine::new();
    run(&mut engine);
    assert!(engine.cpu_profile_folded().is_none());
    assert!(engine.write_cpu_profile().is_none());
}
