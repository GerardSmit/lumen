//! A large method (an HTTP parser's state machine) must give the same results on every tier,
//! and its native code must compile in reasonable time: the parse loop runs hot enough for its
//! loops and whole-function code to be compiled, recompiled, and entered with dictionary-mode
//! receivers.
use lumen::{bytecode::Tier, Completion, Engine};

const PARSER: &str = include_str!("fixtures/http_parser.js");

const EXPECTED: &str = "400 400 400 \
    1,1,Date|Thu, 24 Sep 2026 07:26:57 GMT|Connection|keep-alive|Keep-Alive|timeout=5|Content-Length|2,,,200,OK,false,true;\
    1,1,Host|localhost:12420|Connection|keep-alive,1,/,,,false,true;";

#[test]
fn http_parser_state_machine_matches_on_every_tier() {
    for tier in [Tier::Interp, Tier::Bytecode] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        let started = std::time::Instant::now();
        match engine.eval(PARSER, false).unwrap() {
            Completion::Value(value) => assert_eq!(value, EXPECTED, "{tier:?}"),
            Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
        }
        // Generous (debug builds, loaded machines): the regression took tens of seconds.
        let limit = if cfg!(debug_assertions) { 120 } else { 20 };
        assert!(
            started.elapsed().as_secs() < limit,
            "{tier:?} took {:?}",
            started.elapsed()
        );
    }
}
