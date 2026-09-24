//! Macro-generated wrappers vs hand-written `NativeFn`s.
//!
//! * `clamp(x, lo, hi)`: pure argument-conversion overhead.
//! * `sum(bytes)` over a 1 MiB Uint8Array: the macro borrows the backing store (`&[u8]`), the
//!   hand-written baseline uses the pre-existing public path `Ctx::typed_array_bytes` (a copy).
//! * `blen(bytes)` at 16 B vs 1 MiB: a zero-copy borrow costs the same at any size.
//!
//! Each op is timed twice: called directly from Rust through its `NativeFn` pointer (isolates the
//! wrapper), and from a JS loop (what a script sees; includes the engine's call overhead).

use lumen::embed::{Ctx, NativeFn, Value};
use lumen::Engine;
use lumen_ext_demo::{blen, clamp, install, sum};
use std::hint::black_box;
use std::time::Instant;

fn clamp_hand(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let num = |ctx: &mut Ctx, i: usize| match args.get(i) {
        Some(Value::Num(n)) => Ok(*n),
        _ => Err(ctx.make_error("TypeError", "clamp: expected a number")),
    };
    let x = num(ctx, 0)?;
    let lo = num(ctx, 1)?;
    let hi = num(ctx, 2)?;
    Ok(Value::Num(x.max(lo).min(hi)))
}

fn sum_hand(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let bytes = args
        .first()
        .and_then(|v| ctx.typed_array_bytes(v))
        .ok_or_else(|| ctx.make_error("TypeError", "sum: expected a Uint8Array"))?;
    Ok(Value::Num(
        bytes.iter().map(|&b| b as u64).sum::<u64>() as f64,
    ))
}

fn blen_hand(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let bytes = args
        .first()
        .and_then(|v| ctx.typed_array_bytes(v))
        .ok_or_else(|| ctx.make_error("TypeError", "blen: expected a Uint8Array"))?;
    Ok(Value::Num(bytes.len() as f64))
}

/// ns per call of `f(args)` invoked straight from Rust.
fn direct(e: &mut Engine, f: NativeFn, args: &[Value], iters: u32) -> f64 {
    let ctx = e.ctx();
    for _ in 0..iters / 10 {
        black_box(f(ctx, Value::Undefined, black_box(args)).ok());
    }
    let t = Instant::now();
    for _ in 0..iters {
        black_box(f(ctx, Value::Undefined, black_box(args)).ok());
    }
    t.elapsed().as_nanos() as f64 / iters as f64
}

/// ns per iteration of a JS loop around `call`.
fn from_js(e: &mut Engine, setup: &str, call: &str, iters: u32) -> f64 {
    let src = format!(
        "(() => {{ {setup}; let acc = 0; for (let i = 0; i < {iters}; i++) {{ acc += {call}; }} return acc; }})()"
    );
    // Warm up (tiering), then time.
    let _ = e.eval_value(&src).unwrap();
    let t = Instant::now();
    let r = e.eval_value(&src).unwrap();
    let dt = t.elapsed().as_nanos() as f64 / iters as f64;
    assert!(r.is_ok(), "benchmark script threw");
    dt
}

fn row(name: &str, hand: f64, mac: f64) {
    println!("{name:<44} {hand:>12.1} {mac:>12.1} {:>9.2}x", hand / mac);
}

fn main() {
    let mut e = Engine::new();
    install(&mut e);
    e.define_namespace("hand", &[("clamp", 3, clamp_hand), ("sum", 1, sum_hand), ("blen", 1, blen_hand)]);

    // Direct Rust calls through the NativeFn pointers.
    let one_mib: Vec<u8> = (0..1 << 20).map(|i| (i * 7) as u8).collect();
    let big = e.eval_value("new Uint8Array(1 << 20)").unwrap().ok().unwrap();
    assert!(e.ctx().typed_array_set_bytes(&big, &one_mib));
    let small = e.eval_value("new Uint8Array(16)").unwrap().ok().unwrap();

    println!("{:<44} {:>12} {:>12} {:>10}", "ns/call", "hand-written", "#[op]", "speedup");
    let nums = [Value::Num(5.0), Value::Num(0.0), Value::Num(3.0)];
    row(
        "clamp, direct Rust call",
        direct(&mut e, clamp_hand, &nums, 20_000_000),
        direct(&mut e, clamp::DESC.native, &nums, 20_000_000),
    );
    let bigs = [big.clone()];
    row(
        "sum(1 MiB), direct Rust call",
        direct(&mut e, sum_hand, &bigs, 400),
        direct(&mut e, sum::DESC.native, &bigs, 400),
    );
    let smalls = [small.clone()];
    row(
        "blen(16 B), direct Rust call",
        direct(&mut e, blen_hand, &smalls, 5_000_000),
        direct(&mut e, blen::DESC.native, &smalls, 5_000_000),
    );
    row(
        "blen(1 MiB), direct Rust call",
        direct(&mut e, blen_hand, &bigs, 2_000),
        direct(&mut e, blen::DESC.native, &bigs, 5_000_000),
    );

    // From JS.
    // The 1 MiB fill runs once, outside the timed region.
    e.eval_value("globalThis.__big = new Uint8Array(1 << 20); for (let k = 0; k < __big.length; k++) __big[k] = k * 7")
        .unwrap()
        .ok()
        .unwrap();
    let setup_big = "const b = globalThis.__big";
    row(
        "clamp, JS loop",
        from_js(&mut e, "", "hand.clamp(i, 10, 1000)", 5_000_000),
        from_js(&mut e, "", "ext.clamp(i, 10, 1000)", 5_000_000),
    );
    row(
        "sum(1 MiB), JS loop",
        from_js(&mut e, setup_big, "hand.sum(b)", 300),
        from_js(&mut e, setup_big, "ext.sum(b)", 300),
    );
    row(
        "blen(16 B), JS loop",
        from_js(&mut e, "const b = new Uint8Array(16)", "hand.blen(b)", 2_000_000),
        from_js(&mut e, "const b = new Uint8Array(16)", "ext.blen(b)", 2_000_000),
    );
    row(
        "blen(1 MiB), JS loop",
        from_js(&mut e, "const b = new Uint8Array(1 << 20)", "hand.blen(b)", 2_000),
        from_js(&mut e, "const b = new Uint8Array(1 << 20)", "ext.blen(b)", 2_000_000),
    );
    row(
        "empty JS loop (i|0) for reference",
        from_js(&mut e, "", "(i | 0)", 5_000_000),
        from_js(&mut e, "", "(i | 0)", 5_000_000),
    );
    let g = e.global_this();
    let ext = e.ctx().get_member(&g, "ext").ok().unwrap();
    let clamp_fn = e.ctx().get_member(&ext, "clamp").ok().unwrap();
    let fast = e.ctx().fast_op_of(&clamp_fn);
    let f: extern "C" fn(f64, f64, f64) -> f64 = unsafe { std::mem::transmute(fast.unwrap().entry.0) };
    let t = Instant::now();
    let mut acc = 0.0;
    for i in 0..20_000_000 {
        acc += f(black_box(i as f64), 10.0, 1000.0);
    }
    black_box(acc);
    println!(
        "{:<44} {:>12} {:>12.2}",
        "clamp, fast extern \"C\" entry (what a JIT calls)",
        "-",
        t.elapsed().as_nanos() as f64 / 20_000_000.0
    );
}
