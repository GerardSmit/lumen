//! Actual Babel/Jiti minification puts a regex expression directly after a for header.
use lumen::{bytecode::Tier, Completion, Engine};

#[test]
fn control_body_regex_and_call_division_remain_distinct() {
    let source = r#"
        let hits = 0;
        for(let i=0;i<2;i++) /[^ \t]/.exec("x") && hits++;
        if ((true)) /\w/.test("a") && hits++;
        let left=1; while(left--) /\d/.test("1") && hits++;
        const obj = {if(){return 12}, while(){return 8}};
        const divided = obj.if() / 2 + obj.while() / 4 + (12) / 3;
        if (hits !== 4 || divided !== 12) throw new Error('regex or division changed');
        'passed';
    "#;
    for tier in [Tier::Interp, Tier::Bytecode] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        match engine.eval(source, false).unwrap() {
            Completion::Value(value) => assert_eq!(value, "passed"),
            Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
        }
    }
}
