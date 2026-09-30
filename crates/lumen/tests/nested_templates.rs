//! OpenClaw's actual remote bin probe nests shell quoting templates three levels deep.
use lumen::{bytecode::Tier, Completion, Engine};

#[test]
fn nested_substitutions_preserve_backticks_quotes_and_regexes() {
    let source = r#"
        const bins = ["bash", "can't"];
        const probe = `for b in ${bins.map((bin) => `'${bin.replace(/'/g, `'\\''`)}'`).join(" ")}; done`;
        if (probe !== "for b in 'bash' 'can'\\''t'; done") throw new Error(probe);
        const nested = `outer ${`inner ${`deep ${'`'}`}`} end`;
        if (nested !== 'outer inner deep ` end') throw new Error(nested);
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
