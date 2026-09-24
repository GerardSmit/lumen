//! `SyncFn<'call>` op parameters and the sync-callback query (`Ctx::sync_callback_params`).
#![cfg(feature = "macros")]

use lumen::embed::{OpError, SyncFn, Value};
use lumen::Engine;

/// Calls `f(i)` for `i < n` and sums the results.
#[lumen::op]
pub fn sum_by(ctx: &mut lumen::embed::Ctx, n: u32, f: SyncFn<'_>) -> Result<f64, OpError> {
    let mut s = 0.0;
    for k in 0..n {
        if let Value::Num(x) = f.call(ctx, Value::Undefined, &[Value::Num(k as f64)])? {
            s += x;
        }
    }
    Ok(s)
}

#[lumen::op]
pub fn two_cbs(ctx: &mut lumen::embed::Ctx, a: SyncFn<'_>, x: f64, b: SyncFn<'_>) -> Result<Value, OpError> {
    a.call(ctx, Value::Undefined, &[])?;
    b.call(ctx, Value::Undefined, &[Value::Num(x)])
}

#[lumen::op]
pub fn plain(x: f64) -> f64 {
    x
}

fn global(e: &mut Engine, name: &str) -> Value {
    let g = e.global_this();
    e.ctx().get_member(&g, name).ok().unwrap()
}

fn mask(e: &mut Engine, src: &str) -> u32 {
    let v = e.eval_value(src).unwrap().ok().unwrap();
    e.ctx().sync_callback_params(&v)
}

#[test]
fn op_flags_and_calls() {
    assert_eq!(sum_by::DESC.flags >> 8, 0b10);
    assert_eq!(two_cbs::DESC.flags >> 8, 0b101);
    assert_eq!(plain::DESC.flags >> 8, 0);

    let mut e = Engine::new();
    e.define_op(&sum_by::DESC);
    e.define_op(&two_cbs::DESC);
    e.define_op(&plain::DESC);
    let r = e.eval_value("sum_by(4, i => i * 10)").unwrap().ok().unwrap();
    assert!(matches!(r, Value::Num(x) if x == 60.0));
    let r = e.eval_value("try { sum_by(2, 1) } catch (e) { e.message }").unwrap().ok().unwrap();
    assert!(matches!(&r, Value::Str(_)));
    let r = e.eval_value("let t = 0; two_cbs(() => t++, 5, x => x + t)").unwrap().ok().unwrap();
    assert!(matches!(r, Value::Num(x) if x == 6.0));

    let f = global(&mut e, "sum_by");
    assert_eq!(e.ctx().sync_callback_params(&f), 0b10);
    let f = global(&mut e, "two_cbs");
    assert_eq!(e.ctx().sync_callback_params(&f), 0b101);
    let f = global(&mut e, "plain");
    assert_eq!(e.ctx().sync_callback_params(&f), 0);
}

#[test]
fn builtin_marks() {
    let mut e = Engine::new();
    for (src, want) in [
        ("Array.prototype.map", 1),
        ("Array.prototype.reduceRight", 1),
        ("Array.prototype.sort", 1),
        ("Array.from", 2),
        ("Object.getPrototypeOf(Uint8Array).prototype.filter", 1),
        ("Object.getPrototypeOf(Uint8Array).from", 2),
        ("String.prototype.replace", 2),
        ("String.prototype.replaceAll", 2),
        ("Map.prototype.forEach", 1),
        ("Set.prototype.forEach", 1),
        ("JSON.parse", 2),
        ("JSON.stringify", 2),
        ("Array.prototype.push", 0),
        ("Promise.prototype.then", 0),
        ("(x => x)", 0),
    ] {
        assert_eq!(mask(&mut e, src), want, "{src}");
    }
    for &(path, _) in lumen::embed::SYNC_CALLBACK_BUILTINS {
        let src = path.replace("%TypedArray%", "Object.getPrototypeOf(Int8Array)");
        assert_ne!(mask(&mut e, &src), 0, "{path} did not resolve to a plain native");
    }
}
