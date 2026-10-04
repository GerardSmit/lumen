//! One module, declared once with `lumen_bind`, bound into both the JS engine and Python.

use lumen::embed::Value;
use lumen::Engine;
use lumen_py::vm::Interp;

#[lumen_bind::module(name = "demo")]
pub mod demo {
    use lumen_bind::{NativeError, NativeResult};

    /// Clamps `x` into `[lo, hi]`.
    #[op]
    pub fn clamp(
        #[kw] x: f64,
        #[kw]
        #[default(0.0)]
        lo: f64,
        #[kw]
        #[default(1.0)]
        hi: f64,
    ) -> NativeResult<f64> {
        if lo > hi {
            return Err(NativeError::value_error("lo must not exceed hi"));
        }
        Ok(x.clamp(lo, hi))
    }

    #[op]
    pub fn hypot2(a: f64, b: f64) -> f64 {
        a * a + b * b
    }

    #[op]
    pub fn repeat_str(s: &str, n: u32) -> String {
        s.repeat(n as usize)
    }

    /// Sums the bytes of any byte buffer, without copying it.
    #[op]
    pub fn byte_sum(data: &[u8]) -> u64 {
        data.iter().map(|&b| b as u64).sum()
    }

    #[op]
    pub fn total(#[varargs] xs: Vec<f64>) -> f64 {
        xs.iter().sum()
    }

    #[constant(name = "VERSION")]
    const VERSION: i32 = 2;

    #[class]
    pub struct Counter {
        n: i64,
        step: i64,
    }

    #[methods]
    impl Counter {
        #[constructor]
        fn new(
            #[default(0)] start: i64,
            #[kwonly]
            #[default(1)]
            step: i64,
        ) -> Counter {
            Counter { n: start, step }
        }

        #[getter]
        fn value(&self) -> i64 {
            self.n
        }

        fn bump(&mut self) -> i64 {
            self.n += self.step;
            self.n
        }

        #[proto(repr)]
        fn repr(&self) -> String {
            format!("Counter({})", self.n)
        }
    }
}

fn js_true(e: &mut Engine, src: &str) {
    let v = e.eval_value(src).unwrap().ok().unwrap();
    assert!(matches!(v, Value::Bool(true)), "{src}");
}

#[test]
fn js_side() {
    let mut e = Engine::new();
    assert!(e.define_module::<demo::Module>().is_ok());
    js_true(&mut e, "demo.clamp(5, 0, 2) === 2");
    js_true(&mut e, "demo.clamp(-1) === 0");
    js_true(
        &mut e,
        "demo.clamp.length === 1 && demo.hypot2.length === 2",
    );
    js_true(
        &mut e,
        "try { demo.clamp(1, 3, 2); false } catch (err) { err instanceof RangeError && err.message === 'lo must not exceed hi' }",
    );
    js_true(&mut e, "demo.hypot2(3, 4) === 25");
    js_true(&mut e, "demo.repeat_str('ab', 3) === 'ababab'");
    js_true(&mut e, "demo.byte_sum(new Uint8Array([1, 2, 250])) === 253");
    js_true(&mut e, "demo.total(1, 2, 3.5) === 6.5");
    js_true(&mut e, "demo.VERSION === 2");
    js_true(
        &mut e,
        "const c = new demo.Counter(5); c.bump(); c.bump() === 7 && c.value === 7",
    );
}

#[test]
fn python_side() {
    let mut it = Interp::new();
    assert!(lumen_py::bind::module_object::<demo::Module>(&mut it).is_ok());
    let src = r#"
import demo
from demo import clamp
assert clamp(5, 0, 2) == 2
assert clamp(-1) == 0
assert clamp(0.5, hi=0.25) == 0.25
try:
    clamp(1, 3, 2)
except ValueError as e:
    assert str(e) == "lo must not exceed hi"
else:
    raise AssertionError
try:
    clamp()
except TypeError as e:
    assert str(e) == "clamp() missing required argument 'x' (pos 1)", e
try:
    clamp(1, lo=0, bogus=1)
except TypeError as e:
    assert str(e) == "'bogus' is an invalid keyword argument for clamp()", e
assert clamp.__text_signature__ == "($module, /, x, lo=0.0, hi=1.0)", clamp.__text_signature__
assert demo.hypot2.__text_signature__ == "($module, a, b, /)", demo.hypot2.__text_signature__
assert demo.hypot2(3, 4) == 25
assert demo.repeat_str("ab", 3) == "ababab"
try:
    demo.hypot2(3)
except TypeError as e:
    assert str(e) == "hypot2 expected 2 arguments, got 1", e
assert demo.byte_sum(b"\x01\x02\xfa") == 253
assert demo.byte_sum(bytearray(b"\x01\x02")) == 3
assert demo.total(1, 2, 3.5) == 6.5
assert demo.VERSION == 2
c = demo.Counter(5)
c.bump()
assert c.bump() == 7 and c.value == 7, c.value
assert repr(c) == "Counter(7)", repr(c)
assert demo.Counter(step=3).bump() == 3
assert type(c).__name__ == "Counter" and type(c).__module__ == "demo", type(c).__module__
"#;
    assert_eq!(it.run_source(src, "<dual>"), 0);
}
