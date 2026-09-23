//! A generator closed before its first `next()` — by `return()`, `throw()`, or by being dropped —
//! never runs its body, but the worker thread that would have run it still holds the body's
//! captures. Those must be released while the driver is parked, into the driver's object
//! registry: released after the handoff they race the driver, and released under the worker's own
//! registry they leave the driver's slots dangling for the next collection to walk.
use lumen::{Completion, Engine};

#[test]
fn generators_closed_before_their_first_next_leave_a_sound_heap() {
    let mut engine = Engine::new();
    let completion = engine
        .eval(
            r#"
            function* g(o) { yield o; }
            let n = 0;
            for (let i = 0; i < 400; i++) {
              const thrown = g({ x: i });
              try { thrown.throw(new Error("x")); } catch (e) { n++; }
              g({ y: i }).return(1);
              g({ z: i });
              if (i % 20 === 0) $262.gc();
            }
            $262.gc();
            n;
            "#,
            false,
        )
        .expect("parses");
    match completion {
        Completion::Value(v) => assert_eq!(v, "400"),
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}
