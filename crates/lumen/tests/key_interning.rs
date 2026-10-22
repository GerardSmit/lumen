//! Property keys are interned: equal keys from computed, static, JSON, for-in and reflection
//! paths behave identically. The script is also valid in node, which produced its expectations.
use lumen::{bytecode::Tier, Completion, Engine};

const SCRIPT: &str = include_str!("fixtures/key_interning.js");

fn run(tier: Tier) {
    let mut e = Engine::new();
    e.set_tier(tier);
    e.set_tier_threshold(0);
    match e.eval(&format!("{SCRIPT}\n'passed'"), false).unwrap() {
        Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
        Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
    }
}

#[test]
fn key_identity_across_paths_interpreter() {
    run(Tier::Interp);
}

#[test]
fn key_identity_across_paths_bytecode() {
    run(Tier::Bytecode);
}
