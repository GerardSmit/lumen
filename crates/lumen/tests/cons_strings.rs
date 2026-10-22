//! Long concatenations are lazy: every read flattens them correctly (deep chains, surrogate seams,
//! views, keys). The script is also valid in node, which produced its expectations.
use lumen::{bytecode::Tier, Completion, Engine};

const SCRIPT: &str = include_str!("fixtures/cons_strings.js");

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
fn lazy_concatenation_interpreter() {
    run(Tier::Interp);
}

#[test]
fn lazy_concatenation_bytecode() {
    run(Tier::Bytecode);
}
