//! Backend tests. Every test builds IR and compiles it on any host; on a 32-bit ARM host the
//! code also runs and is compared against the IR interpreter ([`crate::interp`]), the oracle for
//! the other backends too.

use super::*;
use crate::interp::{run, BufferEnv};
use crate::machinst::RegClass;
use crate::*;
use lower::ArgLoc;

fn build(
    params: &[Type],
    results: &[Type],
    body: impl FnOnce(&mut FunctionBuilder, &[Value]) -> Vec<Value>,
) -> Function {
    let mut f = Function::new("t", Signature::new(params.to_vec(), results.to_vec()));
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let args = b.block_params(entry).to_vec();
    let rets = body(&mut b, &args);
    b.ret(&rets);
    b.finish();
    verify::verify(&f).unwrap();
    f
}

fn host() -> Config {
    Config::host(None)
}

/// Every instruction set the processor can execute (all of them on a non-ARM host, which only
/// compiles): ARMv7 with and without `sdiv`/`udiv`, and ARMv6.
fn configs() -> Vec<Config> {
    let h = host();
    let v6 = Config {
        v7: false,
        idiv: false,
        ..h
    };
    let mut out = Vec::new();
    if h.v7 && h.idiv {
        out.push(h);
    }
    if h.v7 {
        out.push(Config { idiv: false, ..h });
    }
    out.push(v6);
    out
}

const I32S: [u32; 18] = [
    0,
    1,
    2,
    3,
    7,
    31,
    32,
    33,
    255,
    256,
    0x7fff_ffff,
    0x8000_0000,
    0x8000_0001,
    0xffff_ffff,
    0x1234_5678,
    0xdead_beef,
    100_000,
    0xffff_0000,
];

fn i64s() -> Vec<u64> {
    let mut out = Vec::new();
    for &hi in &[0u32, 1, 0x7fff_ffff, 0x8000_0000, 0xffff_ffff, 0x1234_5678] {
        for &lo in &[0u32, 1, 0x8000_0000, 0xffff_ffff, 31, 0xdead_beef, 32, 33] {
            out.push((hi as u64) << 32 | lo as u64);
        }
    }
    out.extend([63, 64, 65, 40, 100]);
    out
}

fn f64s() -> Vec<u64> {
    [
        0.0f64,
        -0.0,
        1.0,
        -1.0,
        0.5,
        -0.5,
        1.5,
        2.5,
        -2.5,
        3.0,
        1e10,
        -1e10,
        2147483647.0,
        2147483648.0,
        -2147483648.0,
        -2147483649.0,
        4294967295.0,
        4294967296.0,
        4294967297.5,
        -4294967297.5,
        1e19,
        -1e19,
        9007199254740993.0,
        1e300,
        f64::MIN_POSITIVE,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NAN,
    ]
    .iter()
    .map(|v| v.to_bits())
    .collect()
}

#[cfg(target_arch = "arm")]
mod native {
    use super::*;

    pub struct Code {
        _mem: crate::jitmem::ExecMemory,
        pub addr: usize,
    }

    pub fn load(f: &Function, resolve: impl Fn(u32) -> Option<u64>) -> Code {
        load_for(f, &host(), resolve)
    }

    pub fn load_for(f: &Function, cfg: &Config, resolve: impl Fn(u32) -> Option<u64>) -> Code {
        let compiled = compile(f, cfg).unwrap_or_else(|e| panic!("{e}\n{f}"));
        let (mem, addrs) = super::super::load(&[compiled], |id, _| resolve(id)).unwrap();
        Code {
            addr: addrs[0] as usize,
            _mem: mem,
        }
    }

    /// Call with raw-bit arguments laid out by the AAPCS-VFP rules; the result as raw bits.
    pub fn call(code: &Code, sig: &Signature, args: &[u64]) -> u64 {
        let (locs, _) = lower::arg_locs(&regs::aapcs_vfp(), &sig.params).unwrap();
        let mut ints = [0u32; 20];
        let mut floats = [0f64; 4];
        for ((&t, locs), &a) in sig.params.iter().zip(&locs).zip(args) {
            let words = [a as u32, (a >> 32) as u32];
            for (k, loc) in locs.iter().enumerate() {
                match *loc {
                    ArgLoc::Reg(r) if t == Type::F64 => floats[r.hw as usize] = f64::from_bits(a),
                    ArgLoc::Reg(r) => ints[r.hw as usize] = words[k],
                    ArgLoc::Stack(off, _) => ints[4 + off as usize / 4] = words[k],
                }
            }
        }
        type IntRet = extern "C" fn(
            f64,
            f64,
            f64,
            f64,
            u32,
            u32,
            u32,
            u32,
            u32,
            u32,
            u32,
            u32,
            u32,
            u32,
            u32,
            u32,
            u32,
            u32,
            u32,
            u32,
        ) -> u64;
        type FloatRet = extern "C" fn(
            f64,
            f64,
            f64,
            f64,
            u32,
            u32,
            u32,
            u32,
            u32,
            u32,
            u32,
            u32,
            u32,
            u32,
            u32,
            u32,
            u32,
            u32,
            u32,
            u32,
        ) -> f64;
        let i = &ints;
        let d = &floats;
        // SAFETY: the code was generated for `sig`, whose arguments are in the right places.
        unsafe {
            if sig.results.first().is_some_and(|t| t.is_float()) {
                let f: FloatRet = std::mem::transmute(code.addr);
                f(
                    d[0], d[1], d[2], d[3], i[0], i[1], i[2], i[3], i[4], i[5], i[6], i[7], i[8],
                    i[9], i[10], i[11], i[12], i[13], i[14], i[15],
                )
                .to_bits()
            } else {
                let f: IntRet = std::mem::transmute(code.addr);
                let r = f(
                    d[0], d[1], d[2], d[3], i[0], i[1], i[2], i[3], i[4], i[5], i[6], i[7], i[8],
                    i[9], i[10], i[11], i[12], i[13], i[14], i[15],
                );
                match sig.results.first() {
                    Some(Type::I32) => r & 0xffff_ffff,
                    _ => r,
                }
            }
        }
    }
}

fn same(ty: Type, a: u64, b: u64) -> bool {
    a == b
        || (ty == Type::F64 && f64::from_bits(a).is_nan() && f64::from_bits(b).is_nan())
        || (ty == Type::F32
            && f32::from_bits(a as u32).is_nan()
            && f32::from_bits(b as u32).is_nan())
}

/// Compile `f` (always) and, on ARM, run it on each argument vector, comparing with the
/// interpreter. `f` returns at most one value.
fn check(f: &Function, cases: &[Vec<u64>]) {
    check_with(f, cases, |_| None)
}

fn check_with(f: &Function, cases: &[Vec<u64>], resolve: impl Fn(u32) -> Option<u64>) {
    let _ = &resolve;
    let mut optimized = f.clone();
    opt::optimize(&mut optimized);
    verify::verify(&optimized).unwrap();
    for cfg in configs() {
        for g in [f, &optimized] {
            compile(g, &cfg).unwrap_or_else(|e| panic!("{e}\n{g}"));
        }
    }
    #[cfg(target_arch = "arm")]
    for (cfg, g) in configs()
        .into_iter()
        .flat_map(|c| [(c, f), (c, &optimized)])
    {
        let code = native::load_for(g, &cfg, &resolve);
        for args in cases {
            let want = run(f, &mut BufferEnv { mem: vec![0; 256] }, args).unwrap();
            let got = native::call(&code, &g.sig, args);
            if let Some(&w) = want.first() {
                let ty = f.sig.results[0];
                assert!(
                    same(ty, got, w),
                    "{args:x?} (v7: {}): got {got:#x}, want {w:#x}\n{g}",
                    cfg.v7
                );
            }
        }
    }
    #[cfg(not(target_arch = "arm"))]
    let _ = (cases, run, BufferEnv { mem: Vec::new() });
}

fn pairs(a: &[u64], b: &[u64]) -> Vec<Vec<u64>> {
    a.iter()
        .flat_map(|&x| b.iter().map(move |&y| vec![x, y]))
        .collect()
}

fn i32s() -> Vec<u64> {
    I32S.iter().map(|&x| x as u64).collect()
}

#[test]
fn integer_binary_ops_match_the_interpreter() {
    use BinaryOp::*;
    for ty in [Type::I32, Type::I64] {
        let vals = if ty == Type::I32 { i32s() } else { i64s() };
        for op in [
            Iadd, Isub, Imul, Band, Bor, Bxor, Ishl, Ushr, Sshr, Rotl, Rotr,
        ] {
            let f = build(&[ty, ty], &[ty], |b, a| vec![b.binary(op, a[0], a[1])]);
            check(&f, &pairs(&vals, &vals));
        }
        for op in [Sdiv, Udiv, Srem, Urem] {
            let f = build(&[ty, ty], &[ty], |b, a| vec![b.binary(op, a[0], a[1])]);
            let nonzero: Vec<u64> = vals
                .iter()
                .copied()
                .filter(|&v| {
                    eval::norm(ty, v) != 0 && eval::norm(ty, v) != eval::norm(ty, u64::MAX)
                })
                .collect();
            check(&f, &pairs(&vals, &nonzero));
        }
    }
}

/// 32-bit division and remainder by constants (shifts, multiply-high) and by registers
/// (`sdiv`/`udiv` where available), including zero, ±1, `MIN` and powers of two.
#[test]
fn division_by_constants_matches_the_interpreter() {
    use BinaryOp::*;
    let mut dividends = i32s();
    dividends.extend([5, 6, 9, 10, 999, 1000, 1001, 0xffff_fff9, 0xffff_fc18, 0x8000_0007]);
    let cases: Vec<Vec<u64>> = dividends.iter().map(|&v| vec![v]).collect();
    let mut divisors: Vec<i64> = vec![
        0, 1, -1, 2, -2, 3, -3, 4, 5, 6, 7, -7, 8, 10, -10, 16, 25, 60, 100, 125, 641, 1000,
        1024, -1024, 3600, 65536, 1_000_003, 6700417, 0x4000_0000, 0x7fff_ffff,
    ];
    divisors.extend([-0x8000_0000, 0x8000_0001, 0xffff_fffe, 0xc000_0000]);
    for op in [Sdiv, Udiv, Srem, Urem] {
        for &d in &divisors {
            let f = build(&[Type::I32], &[Type::I32], |b, a| {
                let k = b.iconst(Type::I32, d);
                vec![b.binary(op, a[0], k)]
            });
            let cases: Vec<_> = cases.iter().map(|c| vec![c[0], d as u32 as u64]).collect();
            let ok: Vec<_> = defined(op, &cases).into_iter().map(|c| vec![c[0]]).collect();
            check(&f, &ok);
        }
    }
    // Register divisors, zero and -1 included.
    let regs: Vec<u64> = divisors.iter().map(|&d| d as u32 as u64).collect();
    for op in [Sdiv, Udiv, Srem, Urem] {
        let f = build(&[Type::I32, Type::I32], &[Type::I32], |b, a| {
            vec![b.binary(op, a[0], a[1])]
        });
        check(&f, &defined(op, &pairs(&dividends, &regs)));
    }
}

/// The `[dividend, divisor]` cases the IR defines for `op`: no zero divisor and no signed
/// `MIN / -1` (the interpreter rejects the rest).
fn defined(op: BinaryOp, cases: &[Vec<u64>]) -> Vec<Vec<u64>> {
    cases
        .iter()
        .filter(|c| {
            let (a, d) = (c[0] as u32, c[1] as u32);
            d != 0 && !(op == BinaryOp::Sdiv && a == 0x8000_0000 && d == u32::MAX)
        })
        .cloned()
        .collect()
}

/// With `sdiv`/`udiv`, a register division is one instruction and no helper call; ARMv6
/// divides a register by a helper call, a constant without one.
#[test]
fn division_uses_the_hardware_divider_when_present() {
    let v7 = Config {
        v7: true,
        idiv: true,
        ..host()
    };
    let v6 = Config {
        v7: false,
        idiv: false,
        ..host()
    };
    let words = |f: &Function, cfg: &Config| -> Vec<u32> {
        let c = compile(f, cfg).unwrap();
        c.code
            .chunks(4)
            .map(|w| u32::from_le_bytes(w.try_into().unwrap()))
            .collect()
    };
    let blx = |w: &u32| w & 0x0fff_fff0 == 0x012f_ff30;
    let sdiv = |w: &u32| w & 0x0ff0_f0f0 == 0x0710_f010;
    let by_reg = build(&[Type::I32, Type::I32], &[Type::I32], |b, a| {
        vec![b.binary(BinaryOp::Srem, a[0], a[1])]
    });
    assert!(words(&by_reg, &v7).iter().any(sdiv));
    assert!(!words(&by_reg, &v7).iter().any(blx));
    assert!(words(&by_reg, &v6).iter().any(blx));
    let by_const = build(&[Type::I32], &[Type::I32], |b, a| {
        let k = b.iconst(Type::I32, 1000);
        vec![b.binary(BinaryOp::Srem, a[0], k)]
    });
    for cfg in [v7, v6] {
        let w = words(&by_const, &cfg);
        assert!(!w.iter().any(blx) && !w.iter().any(sdiv), "{w:x?}");
    }
}

/// Inline `floor`/`ceil`/`trunc`/`nearest` at the edges of the 2^52 trick: signed zeros, values
/// just below one half, odd and even ties, the 2^52 boundary, subnormals, and F32.
#[test]
fn rounding_matches_the_interpreter_at_the_edges() {
    use UnaryOp::*;
    const TWO52: f64 = 4503599627370496.0;
    let mut xs = vec![
        0.3, 0.7, 0.49999999999999994, 0.5000000000000001, 1.5, 2.5, 3.5, 4.5, 1e-310, 5e-324,
        TWO52 - 0.5, TWO52 - 1.5, TWO52 - 0.75, TWO52, TWO52 + 1.0, TWO52 * 2.0 - 1.0,
        123456.789, 1e15 + 0.5, 0.9999999999999999, 1.0000000000000002,
    ];
    xs.extend(xs.clone().into_iter().map(|x| -x));
    let mut cases: Vec<Vec<u64>> = xs.iter().map(|x| vec![x.to_bits()]).collect();
    cases.extend(f64s().into_iter().map(|x| vec![x]));
    cases.push(vec![(-f64::NAN).to_bits()]);
    for op in [Ceil, Floor, Trunc, Nearest] {
        let f = build(&[Type::F64], &[Type::F64], |b, a| vec![b.unary(op, a[0])]);
        check(&f, &cases);
        // F32 arguments are unsupported, so the F32 form goes through a demotion.
        let f = build(&[Type::F64], &[Type::F64], |b, a| {
            let n = b.convert(ConvOp::Demote, Type::F32, a[0]);
            let r = b.unary(op, n);
            vec![b.convert(ConvOp::Promote, Type::F64, r)]
        });
        check(&f, &cases);
    }
}

#[test]
fn integer_binary_ops_with_a_constant_operand_match_the_interpreter() {
    use BinaryOp::*;
    for ty in [Type::I32, Type::I64] {
        let vals = if ty == Type::I32 { i32s() } else { i64s() };
        let consts: Vec<i64> = vec![
            0,
            1,
            -1,
            4,
            31,
            32,
            63,
            255,
            256,
            0xff00,
            0xffff,
            0x1_0000,
            0x1234_5678,
            -255,
            -256,
            0x7fff_ffff,
            0x8000_0000,
            0xf000_000f,
            0x3_0000_0001,
            -0x1_0000_0000,
        ];
        for op in [
            Iadd, Isub, Imul, Band, Bor, Bxor, Ishl, Ushr, Sshr, Rotl, Rotr,
        ] {
            for &c in &consts {
                for const_first in [false, true] {
                    if const_first && matches!(op, Ishl | Ushr | Sshr | Rotl | Rotr) {
                        continue;
                    }
                    let f = build(&[ty], &[ty], |b, a| {
                        let k = b.iconst(ty, c);
                        vec![if const_first {
                            b.binary(op, k, a[0])
                        } else {
                            b.binary(op, a[0], k)
                        }]
                    });
                    let cases: Vec<Vec<u64>> = vals.iter().map(|&v| vec![v]).collect();
                    check(&f, &cases);
                }
            }
        }
    }
}

#[test]
fn comparisons_and_selects_match_the_interpreter() {
    use IntCC::*;
    let ccs = [Eq, Ne, Slt, Sle, Sgt, Sge, Ult, Ule, Ugt, Uge];
    for ty in [Type::I32, Type::I64] {
        let vals = if ty == Type::I32 { i32s() } else { i64s() };
        for cc in ccs {
            // As a value.
            let f = build(&[ty, ty], &[Type::I32], |b, a| vec![b.icmp(cc, a[0], a[1])]);
            check(&f, &pairs(&vals, &vals));
            // Fused with select.
            let f = build(&[ty, ty], &[ty], |b, a| {
                let c = b.icmp(cc, a[0], a[1]);
                vec![b.select(c, a[0], a[1])]
            });
            check(&f, &pairs(&vals, &vals));
            // Fused with a branch, with a constant operand on either side.
            for c in [0i64, 1, -1, 255, 0x1234_5678, 0x1_0000_0000] {
                for first in [false, true] {
                    let f = build(&[ty], &[Type::I32], |b, a| {
                        let k = b.iconst(ty, c);
                        let cond = if first {
                            b.icmp(cc, k, a[0])
                        } else {
                            b.icmp(cc, a[0], k)
                        };
                        let (yes, no) = (b.create_block(), b.create_block());
                        b.brif(cond, yes, &[], no, &[]);
                        b.seal_block(yes);
                        b.seal_block(no);
                        b.switch_to_block(no);
                        let z = b.iconst(Type::I32, 10);
                        b.ret(&[z]);
                        b.switch_to_block(yes);
                        vec![b.iconst(Type::I32, 20)]
                    });
                    let cases: Vec<Vec<u64>> = vals.iter().map(|&v| vec![v]).collect();
                    check(&f, &cases);
                }
            }
        }
    }
    let fccs = [
        FloatCC::Eq,
        FloatCC::Ne,
        FloatCC::Lt,
        FloatCC::Le,
        FloatCC::Gt,
        FloatCC::Ge,
    ];
    for cc in fccs {
        let f = build(&[Type::F64, Type::F64], &[Type::I32], |b, a| {
            vec![b.fcmp(cc, a[0], a[1])]
        });
        check(&f, &pairs(&f64s(), &f64s()));
        let f = build(&[Type::F64, Type::F64], &[Type::F64], |b, a| {
            let c = b.fcmp(cc, a[0], a[1]);
            vec![b.select(c, a[0], a[1])]
        });
        check(&f, &pairs(&f64s(), &f64s()));
    }
}

#[test]
fn unary_ops_match_the_interpreter() {
    use UnaryOp::*;
    for ty in [Type::I32, Type::I64] {
        let vals = if ty == Type::I32 { i32s() } else { i64s() };
        let cases: Vec<Vec<u64>> = vals.iter().map(|&v| vec![v]).collect();
        let mut ops = vec![Clz, Ctz, Popcnt, Sext8, Sext16];
        if ty == Type::I64 {
            ops.push(Sext32);
        }
        for op in ops {
            let f = build(&[ty], &[ty], |b, a| vec![b.unary(op, a[0])]);
            check(&f, &cases);
        }
        let f = build(&[ty], &[Type::I32], |b, a| vec![b.unary(Eqz, a[0])]);
        check(&f, &cases);
    }
    let cases: Vec<Vec<u64>> = f64s().iter().map(|&v| vec![v]).collect();
    for op in [Fneg, Fabs, Sqrt, Ceil, Floor, Trunc, Nearest] {
        let f = build(&[Type::F64], &[Type::F64], |b, a| vec![b.unary(op, a[0])]);
        check(&f, &cases);
    }
}

#[test]
fn float_binary_ops_match_the_interpreter() {
    use BinaryOp::*;
    for op in [Fadd, Fsub, Fmul, Fdiv, Fmin, Fmax, Fcopysign] {
        let f = build(&[Type::F64, Type::F64], &[Type::F64], |b, a| {
            vec![b.binary(op, a[0], a[1])]
        });
        check(&f, &pairs(&f64s(), &f64s()));
    }
}

#[test]
fn conversions_match_the_interpreter() {
    use ConvOp::*;
    let ints = i32s();
    let wide = i64s();
    let floats = f64s();
    let case1 = |v: &[u64]| -> Vec<Vec<u64>> { v.iter().map(|&x| vec![x]).collect() };
    for (op, from, to, vals) in [
        (Wrap, Type::I64, Type::I32, &wide),
        (Sext, Type::I32, Type::I64, &ints),
        (Uext, Type::I32, Type::I64, &ints),
        (FromSint, Type::I32, Type::F64, &ints),
        (FromUint, Type::I32, Type::F64, &ints),
        (FromSint, Type::I64, Type::F64, &wide),
        (FromUint, Type::I64, Type::F64, &wide),
        (FromSint, Type::I32, Type::F32, &ints),
        (FromUint, Type::I32, Type::F32, &ints),
        (FromSint, Type::I64, Type::F32, &wide),
        (FromUint, Type::I64, Type::F32, &wide),
        (ToSintSat, Type::F64, Type::I32, &floats),
        (ToUintSat, Type::F64, Type::I32, &floats),
        (ToSintSat, Type::F64, Type::I64, &floats),
        (ToUintSat, Type::F64, Type::I64, &floats),
        (ToJsInt32, Type::F64, Type::I32, &floats),
        (Bitcast, Type::I64, Type::F64, &wide),
        (Bitcast, Type::F64, Type::I64, &floats),
        (Bitcast, Type::I32, Type::F32, &ints),
    ] {
        // Single-precision values cross the signature as their bits.
        let (pf, pt) = (
            if from == Type::F32 { Type::I32 } else { from },
            if to == Type::F32 { Type::I32 } else { to },
        );
        let f = build(&[pf], &[pt], |b, a| {
            let x = if from == Type::F32 {
                b.convert(Bitcast, Type::F32, a[0])
            } else {
                a[0]
            };
            let y = b.convert(op, to, x);
            vec![if to == Type::F32 {
                b.convert(Bitcast, Type::I32, y)
            } else {
                y
            }]
        });
        check(&f, &case1(vals));
    }
    // Unchecked float -> int conversions in range.
    let in_range: Vec<u64> = [
        0.0f64,
        1.5,
        -1.5,
        100.9,
        -100.9,
        2147483000.0,
        -2147483000.0,
    ]
    .iter()
    .map(|v| v.to_bits())
    .collect();
    for (op, to) in [
        (ToSint, Type::I32),
        (ToSint, Type::I64),
        (ToUint, Type::I64),
    ] {
        let f = build(&[Type::F64], &[to], |b, a| vec![b.convert(op, to, a[0])]);
        let vals: Vec<u64> = if op == ToUint {
            in_range
                .iter()
                .copied()
                .filter(|&v| f64::from_bits(v) >= 0.0)
                .collect()
        } else {
            in_range.clone()
        };
        check(&f, &case1(&vals));
    }
    let f = build(&[Type::F64], &[Type::I32], |b, a| {
        vec![b.convert(ToUint, Type::I32, a[0])]
    });
    let vals: Vec<u64> = [0.0f64, 1.9, 4294967295.0, 3000000000.5]
        .iter()
        .map(|v| v.to_bits())
        .collect();
    check(&f, &case1(&vals));
    // f32 round trips.
    let f = build(&[Type::F64], &[Type::F64], |b, a| {
        let s = b.convert(Demote, Type::F32, a[0]);
        let t = b.binary(BinaryOp::Fadd, s, s);
        vec![b.convert(Promote, Type::F64, t)]
    });
    check(&f, &case1(&floats));
    let f = build(&[Type::F64], &[Type::I32], |b, a| {
        let s = b.convert(Demote, Type::F32, a[0]);
        vec![b.convert(ToSintSat, Type::I32, s)]
    });
    check(&f, &case1(&floats));
}

#[test]
fn checked_arithmetic_matches_the_interpreter() {
    for op in [CheckedOp::IaddOv, CheckedOp::IsubOv, CheckedOp::ImulOv] {
        // Both results as values.
        let f = build(&[Type::I32, Type::I32], &[Type::I64], |b, a| {
            let (v, o) = b.checked_binary(op, a[0], a[1]);
            let v = b.convert(ConvOp::Uext, Type::I64, v);
            let o = b.convert(ConvOp::Uext, Type::I64, o);
            let s = b.iconst(Type::I64, 32);
            let o = b.binary(BinaryOp::Ishl, o, s);
            vec![b.binary(BinaryOp::Bor, v, o)]
        });
        check(&f, &pairs(&i32s(), &i32s()));
        // The flag fused into a branch.
        let f = build(&[Type::I32, Type::I32], &[Type::I32], |b, a| {
            let (v, o) = b.checked_binary(op, a[0], a[1]);
            let (yes, no) = (b.create_block(), b.create_block());
            b.brif(o, yes, &[], no, &[]);
            b.seal_block(yes);
            b.seal_block(no);
            b.switch_to_block(yes);
            let z = b.iconst(Type::I32, 0);
            b.ret(&[z]);
            b.switch_to_block(no);
            vec![v]
        });
        check(&f, &pairs(&i32s(), &i32s()));
    }
}

#[test]
fn memory_accesses_match_the_interpreter() {
    // Round trip every kind through memory at several offsets, base-register and folded.
    let kinds = [
        MemKind::I32,
        MemKind::I64,
        MemKind::I32S8,
        MemKind::I32U8,
        MemKind::I32S16,
        MemKind::I32U16,
        MemKind::I64S8,
        MemKind::I64U8,
        MemKind::I64S16,
        MemKind::I64U16,
        MemKind::I64S32,
        MemKind::I64U32,
        MemKind::F64,
        MemKind::F32,
    ];
    for kind in kinds {
        for offset in [
            0i32, 4, 8, 12, 60, 252, 256, 1020, 1024, 4090, 5000, -4, -8, -300, 7, 13,
        ] {
            let ty = kind.ty();
            let wire = if ty == Type::F32 { Type::I32 } else { ty };
            let f = build(&[Type::I32, wire], &[wire], |b, a| {
                let v = if ty == Type::F32 {
                    b.convert(ConvOp::Bitcast, Type::F32, a[1])
                } else {
                    a[1]
                };
                b.store(kind, a[0], v, offset);
                let r = b.load(kind, a[0], offset);
                vec![if ty == Type::F32 {
                    b.convert(ConvOp::Bitcast, Type::I32, r)
                } else {
                    r
                }]
            });
            // Compile everywhere; run on ARM against a real buffer.
            compile(&f, &host()).unwrap();
            #[cfg(target_arch = "arm")]
            {
                let code = native::load(&f, |_| None);
                let mut buf = vec![0xa5u8; 16384];
                let base = buf.as_mut_ptr() as usize + 8192;
                let vals: Vec<u64> = match ty {
                    Type::I32 => i32s(),
                    Type::I64 => i64s(),
                    Type::F64 => f64s(),
                    _ => i32s(),
                };
                for v in vals {
                    for b in buf.iter_mut() {
                        *b = 0xa5;
                    }
                    let got = native::call(&code, &f.sig, &[base as u64, v]);
                    // Reference: the interpreter over a buffer addressed from 0.
                    let mut env = BufferEnv {
                        mem: vec![0xa5; 16384],
                    };
                    let want = run(&f, &mut env, &[8192, v]).unwrap()[0];
                    assert!(
                        same(wire, got, want),
                        "{kind:?}@{offset} {v:#x}: {got:#x} vs {want:#x}"
                    );
                    let lo = (8192 + offset) as usize;
                    let n = kind.bytes() as usize;
                    assert_eq!(&buf[lo..lo + n], &env.mem[lo..lo + n], "{kind:?}@{offset}");
                    assert_eq!(buf[lo - 1], 0xa5);
                    assert_eq!(buf[lo + n], 0xa5);
                }
            }
        }
    }
}

#[test]
fn folded_addresses_match_the_interpreter() {
    // iadd(base, const) and iadd(base, index) feeding loads and stores.
    for kind in [
        MemKind::I32,
        MemKind::I64,
        MemKind::F64,
        MemKind::I32U8,
        MemKind::I32S16,
    ] {
        let ty = kind.ty();
        let f = build(&[Type::I32, Type::I32, ty], &[ty], |b, a| {
            let c = b.iconst(Type::I32, 40);
            let p = b.binary(BinaryOp::Iadd, a[0], c);
            b.store(kind, p, a[2], 4);
            let q = b.binary(BinaryOp::Iadd, a[0], a[1]);
            b.store(kind, q, a[2], 0);
            let big = b.iconst(Type::I32, 5000);
            let r = b.binary(BinaryOp::Iadd, a[0], big);
            let x = b.load(kind, r, 0);
            let y = b.load(kind, p, 4);
            let z = b.load(kind, q, 0);
            let s = b.binary(
                if ty.is_float() {
                    BinaryOp::Fadd
                } else {
                    BinaryOp::Iadd
                },
                x,
                y,
            );
            vec![b.binary(
                if ty.is_float() {
                    BinaryOp::Fadd
                } else {
                    BinaryOp::Iadd
                },
                s,
                z,
            )]
        });
        compile(&f, &host()).unwrap();
        #[cfg(target_arch = "arm")]
        {
            let code = native::load(&f, |_| None);
            let mut buf = vec![0u8; 16384];
            let base = buf.as_mut_ptr() as usize + 2048;
            let mut env = BufferEnv {
                mem: vec![0; 16384],
            };
            let vals = if ty == Type::I64 {
                i64s()
            } else if ty.is_float() {
                f64s()
            } else {
                i32s()
            };
            for v in vals {
                let got = native::call(&code, &f.sig, &[base as u64, 100, v]);
                let want = run(&f, &mut env, &[2048, 100, v]).unwrap()[0];
                assert!(same(ty, got, want), "{kind:?} {v:#x}");
            }
        }
    }
}

/// `acc[i] += (x ^ i) * k` over many live values: forces spills of both register classes.
#[test]
fn register_pressure_loops_match_the_interpreter() {
    for ty in [Type::I32, Type::I64, Type::F64] {
        pressure_loop(ty, 20);
    }
}

/// Enough live values that spill slots lie beyond the reach of `ldr` (4095) and `vldr` (1020).
#[test]
fn large_frames_reach_every_spill_slot() {
    for ty in [Type::I32, Type::I64, Type::F64] {
        pressure_loop(ty, 700);
    }
}

fn pressure_loop(ty: Type, n: usize) {
    {
        let f = build(&[Type::I32, ty], &[ty], |b, a| {
            let accs: Vec<Variable> = (0..n).map(|_| b.declare_var(ty)).collect();
            let i = b.declare_var(Type::I32);
            for (k, &v) in accs.iter().enumerate() {
                let init = match ty {
                    Type::F64 => b.f64const(k as f64 + 0.5),
                    t => b.iconst(t, k as i64 * 3 + 1),
                };
                b.def_var(v, init);
            }
            let zero = b.iconst(Type::I32, 0);
            b.def_var(i, zero);
            let (header, body, exit) = (b.create_block(), b.create_block(), b.create_block());
            b.jump(header, &[]);
            b.switch_to_block(header);
            let iv = b.use_var(i);
            let c = b.icmp(IntCC::Slt, iv, a[0]);
            b.brif(c, body, &[], exit, &[]);
            b.seal_block(body);
            b.seal_block(exit);
            b.switch_to_block(body);
            let (add, mul) = if ty.is_float() {
                (BinaryOp::Fadd, BinaryOp::Fmul)
            } else {
                (BinaryOp::Iadd, BinaryOp::Imul)
            };
            for (k, &v) in accs.iter().enumerate() {
                let old = b.use_var(v);
                let t = b.binary(mul, old, a[1]);
                let t = b.binary(add, t, a[1]);
                let _ = k;
                b.def_var(v, t);
            }
            let one = b.iconst(Type::I32, 1);
            let ni = b.binary(BinaryOp::Iadd, iv, one);
            b.def_var(i, ni);
            b.jump(header, &[]);
            b.seal_block(header);
            b.switch_to_block(exit);
            let mut sum = b.use_var(accs[0]);
            for &v in &accs[1..] {
                let x = b.use_var(v);
                sum = b.binary(add, sum, x);
            }
            vec![sum]
        });
        let vals = match ty {
            Type::I32 => vec![3, 0x1234_5679, 0xffff_ffff],
            Type::I64 => vec![3, 0x1_2345_6789, u64::MAX],
            _ => vec![1.5f64.to_bits(), (-0.25f64).to_bits(), 1.0001f64.to_bits()],
        };
        let cases: Vec<Vec<u64>> = [0u64, 1, 5, 17]
            .iter()
            .flat_map(|&n| vals.iter().map(move |&v| vec![n, v]))
            .collect();
        check(&f, &cases);
    }
}

#[test]
fn br_table_and_block_arguments_match_the_interpreter() {
    for ty in [Type::I32, Type::I64, Type::F64] {
        let f = build(&[Type::I32, ty], &[ty], |b, a| {
            let join = b.create_block();
            let r = b.append_block_param(join, ty);
            let blocks: Vec<Block> = (0..5).map(|_| b.create_block()).collect();
            let dflt = b.create_block();
            let targets: Vec<(Block, Vec<Value>)> = blocks.iter().map(|&x| (x, vec![])).collect();
            b.br_table(a[0], &targets, (dflt, &[]));
            for (k, &blk) in blocks.iter().chain([&dflt]).enumerate() {
                b.seal_block(blk);
                b.switch_to_block(blk);
                let v = match ty {
                    Type::F64 => {
                        let c = b.f64const(k as f64 * 1.25);
                        b.binary(BinaryOp::Fadd, a[1], c)
                    }
                    t => {
                        let c = b.iconst(t, k as i64 * 1000 + 7);
                        b.binary(BinaryOp::Iadd, a[1], c)
                    }
                };
                b.jump(join, &[v]);
            }
            b.seal_block(join);
            b.switch_to_block(join);
            vec![r]
        });
        let vals = match ty {
            Type::F64 => vec![0.5f64.to_bits(), (-3.0f64).to_bits()],
            Type::I64 => vec![1, u64::MAX],
            _ => vec![1, 0xffff_ffff],
        };
        let cases: Vec<Vec<u64>> = (0u64..8)
            .flat_map(|i| vals.iter().map(move |&v| vec![i, v]))
            .chain([vec![0xffff_ffff, vals[0]], vec![0x8000_0000, vals[0]]])
            .collect();
        check(&f, &cases);
    }
}

#[test]
fn argument_passing_follows_aapcs_vfp() {
    let abi = regs::aapcs_vfp();
    let (locs, stack) = lower::arg_locs(
        &abi,
        &[
            Type::I32,
            Type::I64,
            Type::I32,
            Type::F64,
            Type::I64,
            Type::I32,
        ],
    )
    .unwrap();
    // r0 | (skip r1) r2:r3 | stack 0 | d0 | stack 8..16 | stack 16
    assert!(matches!(locs[0][..], [ArgLoc::Reg(r)] if r.hw == 0));
    assert!(matches!(locs[1][..], [ArgLoc::Reg(a), ArgLoc::Reg(b)] if a.hw == 2 && b.hw == 3));
    assert!(matches!(locs[2][..], [ArgLoc::Stack(0, 4)]));
    assert!(matches!(locs[3][..], [ArgLoc::Reg(r)] if r.hw == 0 && r.class == RegClass::Float));
    assert!(matches!(
        locs[4][..],
        [ArgLoc::Stack(8, 4), ArgLoc::Stack(12, 4)]
    ));
    assert!(matches!(locs[5][..], [ArgLoc::Stack(16, 4)]));
    assert_eq!(stack, 24);
    assert!(lower::arg_locs(&abi, &[Type::F32]).is_err());
    assert!(lower::arg_locs(&abi, &[Type::F64; 7]).is_err());
}

#[test]
fn signatures_with_many_arguments_pass_and_return_them() {
    // Mixed register and stack arguments of every kind, folded into one value.
    let params = [
        Type::I32,
        Type::I64,
        Type::F64,
        Type::I32,
        Type::I64,
        Type::I32,
        Type::F64,
        Type::I64,
        Type::I32,
    ];
    let f = build(&params, &[Type::I64], |b, a| {
        let mut acc = b.iconst(Type::I64, 0);
        for (k, &v) in a.iter().enumerate() {
            let w = match params[k] {
                Type::I32 => b.convert(ConvOp::Uext, Type::I64, v),
                Type::I64 => v,
                _ => b.convert(ConvOp::ToSintSat, Type::I64, v),
            };
            let m = b.iconst(Type::I64, 3 * k as i64 + 1);
            let w = b.binary(BinaryOp::Imul, w, m);
            acc = b.binary(BinaryOp::Iadd, acc, w);
        }
        vec![acc]
    });
    let args: Vec<u64> = vec![
        5,
        0x1_0000_0003,
        7.0f64.to_bits(),
        9,
        0xffff_ffff_0000_0001,
        0x8000_0000,
        (-11.0f64).to_bits(),
        0x7fff_ffff_ffff_ffff,
        13,
    ];
    check(&f, &[args]);
    // A float return.
    let f = build(&[Type::I32, Type::F64, Type::I32], &[Type::F64], |b, a| {
        let x = b.convert(ConvOp::FromSint, Type::F64, a[0]);
        let y = b.convert(ConvOp::FromSint, Type::F64, a[2]);
        let s = b.binary(BinaryOp::Fadd, x, a[1]);
        vec![b.binary(BinaryOp::Fmul, s, y)]
    });
    check(
        &f,
        &[
            vec![3, 1.5f64.to_bits(), 4],
            vec![(-3i32) as u32 as u64, 0.25f64.to_bits(), 9],
        ],
    );
}

#[cfg(target_arch = "arm")]
mod calls {
    use super::*;

    extern "C" fn mix(a: i32, b: i64, c: f64, d: i32, e: i32, g: i64, h: f64, i: i32) -> i64 {
        a as i64 * 3
            + b * 5
            + (c * 7.0) as i64
            + d as i64 * 11
            + e as i64 * 13
            + g * 17
            + (h * 19.0) as i64
            + i as i64 * 23
    }
    extern "C" fn half(x: f64) -> f64 {
        x / 2.0
    }
    extern "C" fn id64(x: u64) -> u64 {
        x ^ 0x1234_5678_9abc_def0
    }
    extern "C" fn add3(a: u32, b: u32, c: u32) -> u32 {
        a.wrapping_add(b).wrapping_add(c)
    }

    fn sig(params: &[Type], results: &[Type]) -> Signature {
        Signature::new(params.to_vec(), results.to_vec())
    }

    #[test]
    fn external_and_indirect_calls_preserve_live_values() {
        let mut f = Function::new(
            "calls",
            sig(&[Type::I32, Type::I64, Type::F64], &[Type::I64]),
        );
        let mixsig = sig(
            &[
                Type::I32,
                Type::I64,
                Type::F64,
                Type::I32,
                Type::I32,
                Type::I64,
                Type::F64,
                Type::I32,
            ],
            &[Type::I64],
        );
        let s4 = f.import_signature(sig(&[Type::I32; 3], &[Type::I32]));
        let f1 = f.import_function(mixsig, 1);
        let f2 = f.import_function(sig(&[Type::F64], &[Type::F64]), 2);
        let f3 = f.import_function(sig(&[Type::I64], &[Type::I64]), 3);
        let mut b = FunctionBuilder::new(&mut f);
        let entry = b.create_entry_block();
        let a = b.block_params(entry).to_vec();
        let k = b.iconst(Type::I32, 77);
        let one = b.f64const(1.5);
        let r1 = b.call_fn(f1, &[a[0], a[1], a[2], k, a[0], a[1], one, k])[0];
        let h = b.call_fn(f2, &[a[2]])[0];
        let r2 = b.call_fn(f3, &[r1])[0];
        let callee = b.iconst(
            Type::I32,
            add3 as extern "C" fn(u32, u32, u32) -> u32 as usize as i64,
        );
        let r3 = b.call_indirect(s4, callee, &[a[0], k, a[0]])[0];
        let hi = b.convert(ConvOp::ToSintSat, Type::I64, h);
        let r3 = b.convert(ConvOp::Uext, Type::I64, r3);
        let mut acc = b.binary(BinaryOp::Iadd, r1, r2);
        acc = b.binary(BinaryOp::Iadd, acc, hi);
        acc = b.binary(BinaryOp::Iadd, acc, r3);
        // Values live across all the calls.
        acc = b.binary(BinaryOp::Iadd, acc, a[1]);
        let ai = b.convert(ConvOp::Uext, Type::I64, a[0]);
        acc = b.binary(BinaryOp::Iadd, acc, ai);
        b.ret(&[acc]);
        b.finish();
        verify::verify(&f).unwrap();
        let resolve = |id: u32| {
            Some(match id {
                1 => mix as extern "C" fn(i32, i64, f64, i32, i32, i64, f64, i32) -> i64 as usize,
                2 => half as extern "C" fn(f64) -> f64 as usize,
                3 => id64 as extern "C" fn(u64) -> u64 as usize,
                _ => return None,
            } as u64)
        };
        let code = native::load(&f, resolve);
        for (x, y, z) in [
            (3i32, 0x1_0000_0005i64, 9.5f64),
            (-4, -7, -0.5),
            (0, 0, 0.0),
        ] {
            let args = [x as u32 as u64, y as u64, z.to_bits()];
            let got = native::call(&code, &f.sig, &args);
            let r1 = mix(x, y, z, 77, x, y, 1.5, 77);
            let want = (r1 as u64)
                .wrapping_add(id64(r1 as u64))
                .wrapping_add((z / 2.0) as i64 as u64)
                .wrapping_add(add3(x as u32, 77, x as u32) as u64)
                .wrapping_add(y as u64)
                .wrapping_add(x as u32 as u64);
            assert_eq!(got, want, "{x} {y} {z}");
        }
    }
}

#[test]
fn js_int32_handles_every_double() {
    let f = build(&[Type::F64], &[Type::I32], |b, a| {
        vec![b.convert(ConvOp::ToJsInt32, Type::I32, a[0])]
    });
    let mut vals = f64s();
    vals.extend(
        [
            2147483647.5f64,
            2147483648.0,
            -2147483648.5,
            4294967296.0 * 3.0 + 5.0,
            -4294967296.0 * 3.0 - 5.0,
            1.0e15 + 0.5,
            -1.0e15,
            9.007199254740991e15,
            1.0e21,
            1.0e300,
            2147483646.9,
            -2147483647.9,
        ]
        .map(f64::to_bits),
    );
    let cases: Vec<Vec<u64>> = vals.iter().map(|&v| vec![v]).collect();
    check(&f, &cases);
    // With the result live across other float work (the slow path must keep d0-d5 intact).
    let f = build(&[Type::F64, Type::F64, Type::F64], &[Type::F64], |b, a| {
        let i = b.convert(ConvOp::ToJsInt32, Type::I32, a[0]);
        let x = b.convert(ConvOp::FromSint, Type::F64, i);
        let y = b.binary(BinaryOp::Fmul, a[1], a[2]);
        let z = b.binary(BinaryOp::Fadd, x, y);
        vec![b.binary(BinaryOp::Fsub, z, a[1])]
    });
    let cases: Vec<Vec<u64>> = vals
        .iter()
        .map(|&v| vec![v, 1.25f64.to_bits(), (-8.5f64).to_bits()])
        .collect();
    check(&f, &cases);
}

#[test]
fn unsupported_operations_are_rejected_cleanly() {
    let cfg = host();
    let f = build(&[Type::I32], &[], |b, a| {
        b.trap_if(a[0], 7);
        vec![]
    });
    assert!(compile(&f, &cfg).is_err());
    let f = build(&[Type::I32], &[Type::V128], |b, a| {
        vec![b.load(MemKind::V128, a[0], 0)]
    });
    assert!(compile(&f, &cfg).is_err());
    let f = build(&[Type::F32], &[Type::F32], |_, a| vec![a[0]]);
    assert!(compile(&f, &cfg).is_err(), "f32 arguments");
}

/// ARMv6 code never contains an ARMv7-only instruction, even where the ARMv7 code does:
/// constants, external call addresses, `ctz` and the atomics' barriers.
#[test]
fn armv6_code_avoids_armv7_instructions() {
    let v6 = Config {
        v7: false,
        ..host()
    };
    let v7 = Config { v7: true, ..host() };
    let consts = build(&[Type::I32], &[Type::I32], |b, a| {
        let k = b.iconst(Type::I32, 0x1234_5678);
        let c = b.unary(UnaryOp::Ctz, a[0]);
        let s = b.binary(BinaryOp::Iadd, k, c);
        let sig = Signature::new(vec![Type::I32], vec![Type::I32]);
        let callee = b.func.import_function(sig, 9);
        b.call_fn(callee, &[s])
    });
    let atomic = build(&[Type::I64, Type::I32, Type::I32], &[Type::I32], |b, p| {
        let sig = crate::atomics::signature(crate::atomics::COMPARE_EXCHANGE32).unwrap();
        let intrinsic = b
            .func
            .import_function(sig, crate::atomics::COMPARE_EXCHANGE32);
        b.call_fn(intrinsic, &p[..3])
    });
    let v7_only = |w: u32| {
        w & 0x0fb0_0000 == 0x0300_0000 // movw, movt
            || w & 0x0fff_0ff0 == 0x06ff_0f30 // rbit
            || w & 0xffff_fff0 == 0xf57f_f050 // dmb
            || w & 0x0fff_ffff == 0x0320_f000 // nop hint
            || w == 0xf57f_f01f // clrex (ARMv6K)
    };
    for f in [&consts, &atomic] {
        let words = |cfg: &Config| -> Vec<u32> {
            let c = compile(f, cfg).unwrap_or_else(|e| panic!("{e}\n{f}"));
            c.code
                .chunks(4)
                .map(|w| u32::from_le_bytes(w.try_into().unwrap()))
                .collect()
        };
        assert!(words(&v7).into_iter().any(v7_only), "{f}");
        let bad: Vec<_> = words(&v6).into_iter().filter(|&w| v7_only(w)).collect();
        assert!(bad.is_empty(), "{bad:x?}\n{f}");
    }
}

/// `ctz` (the ARMv6 sequence on every host) and an external call through a patched ARMv6
/// address load agree with the interpreter.
#[test]
fn armv6_ctz_and_patched_calls_match_the_interpreter() {
    let f = build(&[Type::I32], &[Type::I32], |b, a| {
        vec![b.unary(UnaryOp::Ctz, a[0])]
    });
    check(&f, &i32s().into_iter().map(|x| vec![x]).collect::<Vec<_>>());
    let mut buf = Vec::new();
    for imm in [0u32, 0xffff_ffff, 0x1234_5678, 0x8000_0001] {
        let mut a = asm::Asm::with_isa(false);
        a.mov_imm_patchable(1, 0xdead_beef);
        buf.clear();
        buf.extend_from_slice(&a.buf);
        asm::patch_mov32(&mut buf, 0, imm);
        let mut b = asm::Asm::with_isa(false);
        b.mov_imm_patchable(1, imm);
        assert_eq!(buf, b.buf, "{imm:#x}");
    }
}

/// `(a, b) = (b, a + b)` per iteration: block arguments that swap registers (parallel-move
/// cycles) for every register class.
#[test]
fn loop_carried_swaps_match_the_interpreter() {
    for ty in [Type::I32, Type::I64, Type::F64] {
        let f = build(&[Type::I32, ty, ty], &[ty], |b, a| {
            let header = b.create_block();
            let (i, x, y) = (
                b.append_block_param(header, Type::I32),
                b.append_block_param(header, ty),
                b.append_block_param(header, ty),
            );
            let zero = b.iconst(Type::I32, 0);
            b.jump(header, &[zero, a[1], a[2]]);
            let (body, exit) = (b.create_block(), b.create_block());
            b.switch_to_block(header);
            let c = b.icmp(IntCC::Slt, i, a[0]);
            b.brif(c, body, &[], exit, &[]);
            b.seal_block(body);
            b.seal_block(exit);
            b.switch_to_block(body);
            let add = if ty.is_float() {
                BinaryOp::Fadd
            } else {
                BinaryOp::Iadd
            };
            let s = b.binary(add, x, y);
            let one = b.iconst(Type::I32, 1);
            let ni = b.binary(BinaryOp::Iadd, i, one);
            b.jump(header, &[ni, y, s]);
            b.seal_block(header);
            b.switch_to_block(exit);
            vec![x]
        });
        let (p, q) = match ty {
            Type::F64 => (1.0f64.to_bits(), 2.5f64.to_bits()),
            _ => (1, 2),
        };
        let cases: Vec<Vec<u64>> = (0..12).map(|n| vec![n, p, q]).collect();
        check(&f, &cases);
    }
}

/// The `atomics` intrinsics are inline `ldrex`/`strex` loops (no call), with the old value as
/// the result and the update applied only on a match (COMPARE_EXCHANGE32).
#[test]
fn atomics_are_inline_and_sequentially_consistent() {
    for id in [crate::atomics::ADD32, crate::atomics::COMPARE_EXCHANGE32] {
        let f = build(&[Type::I64, Type::I32, Type::I32], &[Type::I32], |b, p| {
            let sig = crate::atomics::signature(id).unwrap();
            let count = sig.params.len();
            let intrinsic = b.func.import_function(sig, id);
            b.call_fn(intrinsic, &p[..count])
        });
        let compiled = compile(&f, &host()).unwrap();
        assert!(
            compiled.relocs.is_empty(),
            "the intrinsic must not be called"
        );
        // ldrex rt, [rn]
        assert!(compiled
            .code
            .chunks(4)
            .any(|w| u32::from_le_bytes(w.try_into().unwrap()) & 0x0ff0_0fff == 0x0190_0f9f));
        #[cfg(target_arch = "arm")]
        {
            use core::sync::atomic::{AtomicU32, Ordering};
            let code = native::load(&f, |_| None);
            let value = AtomicU32::new(u32::MAX);
            let ptr = &value as *const AtomicU32 as usize as u64;
            let old = native::call(&code, &f.sig, &[ptr, 1, 7]);
            assert_eq!(old as u32, u32::MAX);
            if id == crate::atomics::ADD32 {
                assert_eq!(value.load(Ordering::SeqCst), 0);
            } else {
                // Expected 1 does not match: unchanged.
                assert_eq!(value.load(Ordering::SeqCst), u32::MAX);
                let old = native::call(&code, &f.sig, &[ptr, u32::MAX as u64, 7]);
                assert_eq!(old as u32, u32::MAX);
                assert_eq!(value.load(Ordering::SeqCst), 7);
            }
        }
    }
}

/// An I64 address (a pointer taken from a 64-bit word) addresses through its low word, the way
/// wasm32 wraps it; the high word is ignored.
#[test]
fn i64_addresses_use_their_low_word() {
    let f = build(&[Type::I64], &[Type::I32], |b, a| {
        vec![b.load(MemKind::I32, a[0], 4)]
    });
    compile(&f, &host()).unwrap();
    #[cfg(target_arch = "arm")]
    {
        let words = [0x1111_1111u32, 0x2222_2222];
        let base = words.as_ptr() as usize as u64;
        let code = native::load(&f, |_| None);
        for high in [0u64, 0xfff9_0000_0000_0000, 0xdead_beef_0000_0000] {
            assert_eq!(
                native::call(&code, &f.sig, &[base | high]) as u32,
                0x2222_2222
            );
        }
    }
}
