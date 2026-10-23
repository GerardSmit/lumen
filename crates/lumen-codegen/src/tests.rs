use crate::interp::{run, BufferEnv};
use crate::*;

fn env() -> BufferEnv {
    BufferEnv { mem: vec![0; 256] }
}

#[test]
fn vectors_preserve_high_lanes_through_optimization_and_backends() {
    use VectorOp::*;
    for op in [
        I32x4Add, I32x4Mul, I32x4Min, I32x4Max, I32x4Eq, I32x4Lt, F64x2Add, F64x2Mul, F64x2Min,
        F64x2Max, F64x2Eq, F64x2Lt, And, AndNot, Or,
    ] {
        let mut f = Function::new("vector_memory", Signature::new(vec![Type::I64], vec![]));
        let mut b = FunctionBuilder::new(&mut f);
        let entry = b.create_entry_block();
        let pointer = b.block_params(entry)[0];
        let a = b.load(MemKind::V128, pointer, 0);
        let rhs = b.load(MemKind::V128, pointer, 16);
        let result = b.vector_binary(op, a, rhs);
        b.store(MemKind::V128, pointer, result, 32);
        b.ret(&[]);
        b.finish();
        let original = f.clone();
        opt::optimize(&mut f);
        verify::verify(&f).unwrap();
        aarch64::compile(&f, &aarch64::Config::host(None)).unwrap();
        let module =
            wasm::compile_module(&[f.clone()], &wasm::Config::default(), |_| None).unwrap();
        assert!(!module.is_empty());
        for a in [
            0u128,
            u128::MAX,
            0x8000_0000_7fff_ffff_ffff_ffff_0000_0001,
            0x7ff8_0000_0000_0000_8000_0000_0000_0000,
        ] {
            for rhs in [0u128, u128::MAX, 0x3ff0_0000_0000_0000_bff0_0000_0000_0000] {
                let expected = eval::vector(op, a, rhs).to_le_bytes();
                for function in [&original, &f] {
                    let mut memory = env();
                    memory.mem[..16].copy_from_slice(&a.to_le_bytes());
                    memory.mem[16..32].copy_from_slice(&rhs.to_le_bytes());
                    run(function, &mut memory, &[0]).unwrap();
                    assert_eq!(&memory.mem[32..48], &expected, "{op:?}");
                }
            }
        }
    }
}

#[test]
fn signed_i32_float_roundtrips_fold_without_losing_high_bits() {
    for op in [ConvOp::ToJsInt32, ConvOp::ToSintSat] {
        for to in [Type::I32, Type::I64] {
            if op == ConvOp::ToJsInt32 && to == Type::I64 {
                continue;
            }
            let mut f = Function::new("i32_roundtrip", Signature::new(vec![Type::I32], vec![to]));
            let mut b = FunctionBuilder::new(&mut f);
            let entry = b.create_entry_block();
            let x = b.block_params(entry)[0];
            let float = b.convert(ConvOp::FromSint, Type::F64, x);
            let result = b.convert(op, to, float);
            b.ret(&[result]);
            b.finish();
            let original = f.clone();
            opt::optimize(&mut f);
            assert!(!f
                .layout
                .iter()
                .flat_map(|b| &f.blocks[b.index()].insts)
                .any(|i| matches!(
                    f.inst(*i),
                    InstData::Convert {
                        op: ConvOp::FromSint | ConvOp::ToJsInt32 | ConvOp::ToSintSat,
                        ..
                    }
                )));
            for x in [i32::MIN, -16777217, -1, 0, 1, 16777217, i32::MAX] {
                assert_eq!(
                    run(&f, &mut env(), &[x as u32 as u64]).unwrap(),
                    run(&original, &mut env(), &[x as u32 as u64]).unwrap()
                );
            }
        }
    }
    // I64 -> F64 may round; never erase that round trip.
    let mut f = Function::new(
        "i64_roundtrip",
        Signature::new(vec![Type::I64], vec![Type::I64]),
    );
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let x = b.block_params(entry)[0];
    let float = b.convert(ConvOp::FromSint, Type::F64, x);
    let result = b.convert(ConvOp::ToSintSat, Type::I64, float);
    b.ret(&[result]);
    b.finish();
    opt::optimize(&mut f);
    let x = (1u64 << 53) + 1;
    assert_eq!(run(&f, &mut env(), &[x]).unwrap(), vec![x - 1]);
}

#[test]
fn widened_i32_index_conversions_fold_to_the_narrow_value() {
    // `FromSint(Sext(x))` reads `x` directly; `ToSint(FromSint(x))` to I64 is `Sext(x)`.
    for to in [Type::I32, Type::I64] {
        let mut f = Function::new("index", Signature::new(vec![Type::I32], vec![Type::F64, to]));
        let mut b = FunctionBuilder::new(&mut f);
        let entry = b.create_entry_block();
        let x = b.block_params(entry)[0];
        let wide = b.convert(ConvOp::Sext, Type::I64, x);
        let float = b.convert(ConvOp::FromSint, Type::F64, wide);
        let narrow = b.convert(ConvOp::FromSint, Type::F64, x);
        let back = b.convert(ConvOp::ToSint, to, narrow);
        b.ret(&[float, back]);
        b.finish();
        let original = f.clone();
        opt::optimize(&mut f);
        verify::verify(&f).unwrap();
        let converts = f
            .layout
            .iter()
            .flat_map(|b| &f.blocks[b.index()].insts)
            .filter(|i| matches!(f.inst(**i), InstData::Convert { op: ConvOp::ToSint, .. }))
            .count();
        assert_eq!(converts, 0, "{to:?}");
        for x in [i32::MIN, -16777217, -1, 0, 1, 16777217, i32::MAX] {
            assert_eq!(
                run(&f, &mut env(), &[x as u32 as u64]).unwrap(),
                run(&original, &mut env(), &[x as u32 as u64]).unwrap()
            );
        }
    }
}

#[test]
fn checked_i32_keeps_both_results_through_optimization_and_legalization() {
    for op in [CheckedOp::IaddOv, CheckedOp::IsubOv, CheckedOp::ImulOv] {
        let mut f = Function::new(
            "checked",
            Signature::new(vec![Type::I32; 2], vec![Type::I64]),
        );
        let mut b = FunctionBuilder::new(&mut f);
        let entry = b.create_entry_block();
        let args = b.block_params(entry).to_vec();
        let (value, overflow) = b.checked_binary(op, args[0], args[1]);
        let value = b.convert(ConvOp::Uext, Type::I64, value);
        let flag = b.convert(ConvOp::Uext, Type::I64, overflow);
        let shift = b.iconst(Type::I64, 32);
        let flag = b.binary(BinaryOp::Ishl, flag, shift);
        let result = b.binary(BinaryOp::Bor, value, flag);
        b.ret(&[result]);
        b.finish();
        verify::verify(&f).unwrap();
        let mut optimized = f.clone();
        opt::optimize(&mut optimized);
        let mut portable = optimized.clone();
        legalize::legalize(
            &mut portable,
            legalize::Legal {
                from_u64: true,
                to_uint: true,
                to_int_sat: true,
                srem_min_neg1: true,
                fcopysign: true,
                js_to_i32: true,
                checked_i32: false,
            },
        );
        verify::verify(&portable).unwrap();
        wasm::compile_module(&[optimized.clone()], &wasm::Config::default(), |_| None).unwrap();
        #[cfg(target_arch = "x86_64")]
        let native = {
            let code = x64::compile(&optimized, &x64::Config::host(None)).unwrap();
            jitmem::ExecMemory::new(&code.code).unwrap()
        };
        for a in [i32::MIN, -46341, -1, 0, 1, 46341, i32::MAX] {
            for b in [i32::MIN, -46341, -1, 0, 1, 46341, i32::MAX] {
                let (value, overflow) = match op {
                    CheckedOp::IaddOv => a.overflowing_add(b),
                    CheckedOp::IsubOv => a.overflowing_sub(b),
                    CheckedOp::ImulOv => a.overflowing_mul(b),
                };
                let expected = vec![value as u32 as u64 | ((overflow as u64) << 32)];
                for function in [&f, &optimized, &portable] {
                    assert_eq!(
                        run(function, &mut env(), &[a as u32 as u64, b as u32 as u64]).unwrap(),
                        expected
                    );
                }
                #[cfg(target_arch = "x86_64")]
                unsafe {
                    let call: unsafe extern "C" fn(u32, u32) -> u64 =
                        std::mem::transmute(native.as_ptr());
                    assert_eq!(call(a as u32, b as u32), expected[0]);
                }
            }
        }
    }
}

/// sum = 0; for i in 0..n { sum += i * k } ; return sum   (k is a loop invariant constant)
fn sum_loop() -> Function {
    let mut f = Function::new("sum", Signature::new(vec![Type::I32], vec![Type::I32]));
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let n = b.block_params(entry)[0];
    let (i, sum) = (b.declare_var(Type::I32), b.declare_var(Type::I32));
    let zero = b.iconst(Type::I32, 0);
    b.def_var(i, zero);
    b.def_var(sum, zero);
    let header = b.create_block();
    let body = b.create_block();
    let exit = b.create_block();
    b.jump(header, &[]);

    b.switch_to_block(header);
    let iv = b.use_var(i);
    let c = b.icmp(IntCC::Slt, iv, n);
    b.brif(c, body, &[], exit, &[]);
    b.seal_block(body);
    b.seal_block(exit);

    b.switch_to_block(body);
    let iv = b.use_var(i);
    let k = b.iconst(Type::I32, 3);
    let k2 = b.iconst(Type::I32, 3);
    let t = b.binary(BinaryOp::Imul, iv, k);
    let t2 = b.binary(BinaryOp::Imul, k2, iv); // same value, commuted
    let sv = b.use_var(sum);
    let s = b.binary(BinaryOp::Iadd, sv, t);
    let s = b.binary(BinaryOp::Iadd, s, t2);
    b.def_var(sum, s);
    let one = b.iconst(Type::I32, 1);
    let ni = b.binary(BinaryOp::Iadd, iv, one);
    b.def_var(i, ni);
    b.jump(header, &[]);
    b.seal_block(header);

    b.switch_to_block(exit);
    let r = b.use_var(sum);
    b.ret(&[r]);
    b.finish();
    f
}

#[test]
fn builder_constructs_loop_ssa() {
    let f = sum_loop();
    verify::verify(&f).unwrap();
    // The header gets two parameters (i and sum); exit reads sum through its single predecessor.
    assert_eq!(f.blocks[1].params.len(), 2, "{f}");
    let mut e = env();
    for n in [0u64, 1, 5, 100] {
        let want = (0..n).map(|i| 6 * i).sum::<u64>();
        assert_eq!(run(&f, &mut e, &[n]).unwrap(), vec![want]);
    }
}

#[test]
fn optimize_preserves_semantics_and_merges_values() {
    let mut f = sum_loop();
    let before = f.blocks.iter().map(|b| b.insts.len()).sum::<usize>();
    opt::optimize(&mut f);
    verify::verify(&f).unwrap();
    let after = f
        .layout
        .iter()
        .map(|b| f.blocks[b.index()].insts.len())
        .sum::<usize>();
    assert!(
        after < before,
        "GVN should merge the duplicate constant and multiply\n{f}"
    );
    let mut e = env();
    for n in [0u64, 7, 100] {
        let want = (0..n).map(|i| 6 * i).sum::<u64>();
        assert_eq!(run(&f, &mut e, &[n]).unwrap(), vec![want]);
    }
}

#[test]
fn trivial_parameters_are_removed() {
    // x is defined once before a diamond; the join's parameter for it must vanish.
    let mut f = Function::new(
        "d",
        Signature::new(vec![Type::I64, Type::I32], vec![Type::I64]),
    );
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let (a, c) = (b.block_params(entry)[0], b.block_params(entry)[1]);
    let x = b.declare_var(Type::I64);
    let y = b.declare_var(Type::I64);
    b.def_var(x, a);
    b.def_var(y, a);
    let (l, r, j) = (b.create_block(), b.create_block(), b.create_block());
    b.brif(c, l, &[], r, &[]);
    b.seal_block(l);
    b.seal_block(r);
    b.switch_to_block(l);
    let two = b.iconst(Type::I64, 2);
    let yv = b.use_var(y);
    let m = b.binary(BinaryOp::Imul, yv, two);
    b.def_var(y, m);
    b.jump(j, &[]);
    b.switch_to_block(r);
    b.jump(j, &[]);
    b.seal_block(j);
    b.switch_to_block(j);
    let xv = b.use_var(x);
    let yv = b.use_var(y);
    let s = b.binary(BinaryOp::Iadd, xv, yv);
    b.ret(&[s]);
    b.finish();
    opt::optimize(&mut f);
    verify::verify(&f).unwrap();
    assert_eq!(f.blocks[j.index()].params.len(), 1, "{f}");
    let mut e = env();
    assert_eq!(run(&f, &mut e, &[5, 1]).unwrap(), vec![15]);
    assert_eq!(run(&f, &mut e, &[5, 0]).unwrap(), vec![10]);
}

#[test]
fn constant_branches_fold_away() {
    let mut f = Function::new("c", Signature::new(vec![], vec![Type::I32]));
    let mut b = FunctionBuilder::new(&mut f);
    b.create_entry_block();
    let x = b.iconst(Type::I32, 6);
    let y = b.iconst(Type::I32, 7);
    let p = b.binary(BinaryOp::Imul, x, y);
    let c = b.icmp(IntCC::Eq, p, x);
    let (t, e, j) = (b.create_block(), b.create_block(), b.create_block());
    let v = b.append_block_param(j, Type::I32);
    b.brif(c, t, &[], e, &[]);
    b.seal_block(t);
    b.seal_block(e);
    b.switch_to_block(t);
    b.jump(j, &[x]);
    b.switch_to_block(e);
    b.jump(j, &[p]);
    b.seal_block(j);
    b.switch_to_block(j);
    b.ret(&[v]);
    b.finish();
    opt::optimize(&mut f);
    verify::verify(&f).unwrap();
    assert_eq!(f.layout.len(), 3, "the then-block is unreachable\n{f}");
    assert!(f.blocks[j.index()].params.is_empty(), "{f}");
    assert_eq!(run(&f, &mut env(), &[]).unwrap(), vec![42]);
}

#[test]
fn memory_and_traps() {
    // store n at [8], load it back sign-extended from a byte, trap if it's zero.
    let mut f = Function::new("m", Signature::new(vec![Type::I32], vec![Type::I64]));
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let n = b.block_params(entry)[0];
    let base = b.iconst(Type::I64, 0);
    b.store(MemKind::I32, base, n, 8);
    let v = b.load(MemKind::I64S8, base, 8);
    let z = b.unary(UnaryOp::Eqz, v);
    b.trap_if(z, 7);
    b.ret(&[v]);
    b.finish();
    verify::verify(&f).unwrap();
    let mut e = env();
    assert_eq!(run(&f, &mut e, &[0xff]).unwrap(), vec![u64::MAX]);
    assert_eq!(run(&f, &mut e, &[0x100]), Err(7));
}

#[test]
fn eval_follows_wasm_rules() {
    use crate::eval::*;
    let nz = (-0.0f64).to_bits();
    let pz = 0.0f64.to_bits();
    assert_eq!(binary(BinaryOp::Fmin, Type::F64, pz, nz), Some(nz));
    assert_eq!(binary(BinaryOp::Fmax, Type::F64, nz, pz), Some(pz));
    assert!(
        f64::from_bits(binary(BinaryOp::Fmin, Type::F64, f64::NAN.to_bits(), pz).unwrap()).is_nan()
    );
    assert_eq!(
        binary(
            BinaryOp::Srem,
            Type::I32,
            i32::MIN as u32 as u64,
            u32::MAX as u64
        ),
        Some(0)
    );
    assert_eq!(
        binary(
            BinaryOp::Sdiv,
            Type::I32,
            i32::MIN as u32 as u64,
            u32::MAX as u64
        ),
        None
    );
    assert_eq!(binary(BinaryOp::Ishl, Type::I32, 1, 33), Some(2));
    assert_eq!(
        convert(
            ConvOp::ToSint,
            Type::F64,
            Type::I32,
            2147483648.0f64.to_bits()
        ),
        None
    );
    assert_eq!(
        convert(
            ConvOp::ToSint,
            Type::F64,
            Type::I32,
            (-2147483648.9f64).to_bits()
        ),
        Some(0x8000_0000)
    );
    assert_eq!(
        convert(ConvOp::ToUintSat, Type::F64, Type::I32, (-5.0f64).to_bits()),
        Some(0)
    );
    assert_eq!(
        convert(
            ConvOp::ToUint,
            Type::F32,
            Type::I32,
            (-0.9f32).to_bits() as u64
        ),
        Some(0)
    );
    assert_eq!(
        unary(UnaryOp::Nearest, Type::F64, 2.5f64.to_bits()),
        2.0f64.to_bits()
    );
}

/// `s = x; for (i = 0.0; i < n; i += 1.0) { s = s * 2 + i; if s > 1000 { return -s } }; return s`
/// with `n` a constant: F64 induction, a side exit in the body, a value live after the loop.
fn const_trip_loop(n: f64) -> Function {
    let mut f = Function::new("u", Signature::new(vec![Type::F64], vec![Type::F64]));
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let x = b.block_params(entry)[0];
    let (i, s) = (b.declare_var(Type::F64), b.declare_var(Type::F64));
    let zero = b.f64const(0.0);
    b.def_var(i, zero);
    b.def_var(s, x);
    let (header, body, side, latch, exit) = (
        b.create_block(),
        b.create_block(),
        b.create_block(),
        b.create_block(),
        b.create_block(),
    );
    b.jump(header, &[]);
    b.switch_to_block(header);
    let iv = b.use_var(i);
    let lim = b.f64const(n);
    let c = b.fcmp(FloatCC::Lt, iv, lim);
    b.brif(c, body, &[], exit, &[]);
    b.seal_block(body);
    b.seal_block(exit);
    b.switch_to_block(body);
    let sv = b.use_var(s);
    let two = b.f64const(2.0);
    let t = b.binary(BinaryOp::Fmul, sv, two);
    let iv = b.use_var(i);
    let t = b.binary(BinaryOp::Fadd, t, iv);
    b.def_var(s, t);
    let big = b.f64const(1000.0);
    let over = b.fcmp(FloatCC::Gt, t, big);
    b.brif(over, side, &[], latch, &[]);
    b.seal_block(side);
    b.seal_block(latch);
    b.switch_to_block(side);
    let neg = b.unary(UnaryOp::Fneg, t);
    b.ret(&[neg]);
    b.switch_to_block(latch);
    let one = b.f64const(1.0);
    let iv = b.use_var(i);
    let ni = b.binary(BinaryOp::Fadd, iv, one);
    b.def_var(i, ni);
    b.jump(header, &[]);
    b.seal_block(header);
    b.switch_to_block(exit);
    let r = b.use_var(s);
    b.ret(&[r]);
    b.finish();
    f
}

fn has_back_edge(f: &Function) -> bool {
    let cfg = crate::cfg::Cfg::new(f);
    cfg.rpo
        .iter()
        .any(|&h| cfg.preds[h.index()].iter().any(|&p| cfg.dominates(h, p)))
}

#[test]
fn small_constant_loops_unroll() {
    let reference = |x: f64, n: f64| {
        let mut s = x;
        let mut i = 0.0;
        while i < n {
            s = s * 2.0 + i;
            if s > 1000.0 {
                return -s;
            }
            i += 1.0;
        }
        s
    };
    for n in [1.0, 3.0, 4.0, 8.0, 9.0, 0.5] {
        let mut f = const_trip_loop(n);
        opt::optimize(&mut f);
        verify::verify(&f).unwrap();
        // Up to 8 trips unroll completely; 9 stays a loop.
        assert_eq!(has_back_edge(&f), n > 8.0, "n = {n}\n{f}");
        let mut e = env();
        for x in [0.0f64, 1.5, 100.0, 600.0, -3.0] {
            let got = run(&f, &mut e, &[x.to_bits()]).unwrap();
            assert_eq!(
                f64::from_bits(got[0]),
                reference(x, n),
                "n = {n}, x = {x}\n{f}"
            );
        }
    }
}

#[test]
fn values_only_exits_read_are_computed_in_the_exits() {
    let mut f = Function::new("sink", Signature::new(vec![Type::I32], vec![Type::I32]));
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let n = b.block_params(entry)[0];
    let i = b.declare_var(Type::I32);
    let zero = b.iconst(Type::I32, 0);
    b.def_var(i, zero);
    let header = b.create_block();
    let body = b.create_block();
    let cont = b.create_block();
    let found = b.create_block();
    let done = b.create_block();
    b.jump(header, &[]);

    b.switch_to_block(header);
    let iv = b.use_var(i);
    let c = b.icmp(IntCC::Slt, iv, n);
    b.brif(c, body, &[], done, &[]);
    b.seal_block(body);
    b.seal_block(done);

    b.switch_to_block(body);
    let iv = b.use_var(i);
    let wide = b.convert(ConvOp::FromSint, Type::F64, iv);
    let seven = b.iconst(Type::I32, 7);
    let hit = b.icmp(IntCC::Eq, iv, seven);
    b.brif(hit, found, &[], cont, &[]);
    b.seal_block(found);
    b.seal_block(cont);

    b.switch_to_block(found);
    let two = b.f64const(2.0);
    let twice = b.binary(BinaryOp::Fmul, wide, two);
    let back = b.convert(ConvOp::ToSintSat, Type::I32, twice);
    b.ret(&[back]);

    b.switch_to_block(cont);
    let iv = b.use_var(i);
    let one = b.iconst(Type::I32, 1);
    let next = b.binary(BinaryOp::Iadd, iv, one);
    b.def_var(i, next);
    b.jump(header, &[]);
    b.seal_block(header);

    b.switch_to_block(done);
    let minus = b.iconst(Type::I32, -1);
    b.ret(&[minus]);
    b.finish();

    let original = f.clone();
    opt::optimize(&mut f);
    verify::verify(&f).unwrap();
    let converts: Vec<bool> = f
        .layout
        .iter()
        .flat_map(|blk| {
            let returns = f
                .terminator(*blk)
                .is_some_and(|t| matches!(f.inst(t), InstData::Return { .. }));
            f.blocks[blk.index()]
                .insts
                .iter()
                .filter(|i| {
                    matches!(
                        f.inst(**i),
                        InstData::Convert {
                            op: ConvOp::FromSint,
                            ..
                        }
                    )
                })
                .map(move |_| returns)
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(converts, vec![true], "{f}");
    let mut e = env();
    for n in [0u64, 5, 7, 8, 100] {
        let want = run(&original, &mut e, &[n]).unwrap();
        assert_eq!(run(&f, &mut e, &[n]).unwrap(), want, "n={n}");
    }
}

#[test]
fn gvn_merges_only_along_dominance() {
    // a*3 in the entry dominates both arms; a+7 appears in each arm but neither dominates the other.
    let mut f = Function::new(
        "g",
        Signature::new(vec![Type::I64, Type::I32], vec![Type::I64]),
    );
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let (a, c) = (b.block_params(entry)[0], b.block_params(entry)[1]);
    let three = b.iconst(Type::I64, 3);
    let seven = b.iconst(Type::I64, 7);
    let top = b.binary(BinaryOp::Imul, a, three);
    let (l, r) = (b.create_block(), b.create_block());
    b.brif(c, l, &[], r, &[]);
    b.seal_block(l);
    b.seal_block(r);
    b.switch_to_block(l);
    let dup = b.binary(BinaryOp::Imul, a, three);
    let lhs = b.binary(BinaryOp::Iadd, a, seven);
    let lv = b.binary(BinaryOp::Iadd, lhs, dup);
    b.ret(&[lv]);
    b.switch_to_block(r);
    let rhs = b.binary(BinaryOp::Iadd, a, seven);
    let rv = b.binary(BinaryOp::Iadd, rhs, top);
    b.ret(&[rv]);
    b.finish();
    opt::optimize(&mut f);
    verify::verify(&f).unwrap();
    let count = |op: BinaryOp| {
        f.layout
            .iter()
            .flat_map(|blk| f.blocks[blk.index()].insts.iter().copied())
            .filter(|i| matches!(f.inst(*i), InstData::Binary { op: o, .. } if *o == op))
            .count()
    };
    assert_eq!(count(BinaryOp::Imul), 1, "{f}");
    assert_eq!(count(BinaryOp::Iadd), 4, "{f}");
    let mut e = env();
    assert_eq!(run(&f, &mut e, &[5, 1]).unwrap(), vec![27]);
    assert_eq!(run(&f, &mut e, &[5, 0]).unwrap(), vec![27]);
}

fn joined_number_check(other_arm: Option<f64>) -> Function {
    let mut f = Function::new(
        "joined_number",
        Signature::new(vec![Type::I32; 4], vec![Type::I32, Type::I32]),
    );
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let args = b.block_params(entry).to_vec();
    let (cond, x, y, acc) = (args[0], args[1], args[2], args[3]);
    let (left, right, join) = (b.create_block(), b.create_block(), b.create_block());
    let number = b.append_block_param(join, Type::F64);
    b.brif(cond, left, &[], right, &[]);
    b.seal_block(left);
    b.seal_block(right);

    b.switch_to_block(left);
    let from_x = b.convert(ConvOp::FromSint, Type::F64, x);
    b.jump(join, &[from_x]);

    b.switch_to_block(right);
    let other = match other_arm {
        Some(c) => b.f64const(c),
        None => b.convert(ConvOp::FromSint, Type::F64, y),
    };
    b.jump(join, &[other]);
    b.seal_block(join);

    b.switch_to_block(join);
    let int = b.convert(ConvOp::ToSintSat, Type::I32, number);
    let back = b.convert(ConvOp::FromSint, Type::F64, int);
    let differs = b.fcmp(FloatCC::Ne, number, back);
    let bits = b.convert(ConvOp::Bitcast, Type::I64, number);
    let neg_zero = b.iconst(Type::I64, i64::MIN);
    let is_neg_zero = b.icmp(IntCC::Eq, bits, neg_zero);
    let not_int = b.binary(BinaryOp::Bor, differs, is_neg_zero);
    let (sum, overflow) = b.checked_binary(CheckedOp::IaddOv, acc, int);
    let flag = b.binary(BinaryOp::Bor, not_int, overflow);
    b.ret(&[sum, flag]);
    b.finish();
    f
}

fn count_insts(f: &Function, pred: impl Fn(&InstData) -> bool) -> usize {
    f.layout
        .iter()
        .flat_map(|b| f.blocks[b.index()].insts.iter())
        .filter(|i| pred(f.inst(**i)))
        .count()
}

#[test]
fn joined_int32_numbers_stay_integers() {
    let original = joined_number_check(None);
    let mut f = original.clone();
    opt::optimize(&mut f);
    verify::verify(&f).unwrap();
    assert_eq!(
        count_insts(&f, |d| matches!(
            d,
            InstData::Convert {
                op: ConvOp::ToSintSat | ConvOp::Bitcast,
                ..
            } | InstData::FloatCmp { .. }
        )),
        0,
        "{f}"
    );
    assert!(f
        .layout
        .iter()
        .flat_map(|b| &f.blocks[b.index()].params)
        .all(|p| f.value_type(*p) != Type::F64), "{f}");
    let values = [i32::MIN, -1, 0, 1, 12345, i32::MAX];
    for cond in [0u32, 1] {
        for x in values {
            for y in values {
                for acc in values {
                    let input = [cond as u64, x as u32 as u64, y as u32 as u64, acc as u32 as u64];
                    let picked = if cond != 0 { x } else { y };
                    let (sum, overflow) = acc.overflowing_add(picked);
                    let expected = vec![sum as u32 as u64, overflow as u64];
                    assert_eq!(run(&original, &mut env(), &input).unwrap(), expected);
                    assert_eq!(run(&f, &mut env(), &input).unwrap(), expected, "{f}");
                }
            }
        }
    }
}

#[test]
fn joined_numbers_with_a_float_argument_are_not_narrowed() {
    for constant in [0.5f64, -0.0] {
        let original = joined_number_check(Some(constant));
        let mut f = original.clone();
        opt::optimize(&mut f);
        verify::verify(&f).unwrap();
        assert!(
            f.layout
                .iter()
                .flat_map(|b| &f.blocks[b.index()].params)
                .any(|p| f.value_type(*p) == Type::F64),
            "{f}"
        );
        assert!(
            count_insts(&f, |d| matches!(d, InstData::FloatCmp { .. })) > 0,
            "{f}"
        );
        for x in [i32::MIN, -1, 0, 7, i32::MAX] {
            for acc in [i32::MIN, 0, 5, i32::MAX] {
                for cond in [0u64, 1] {
                    let input = [cond, x as u32 as u64, 0, acc as u32 as u64];
                    let expected = run(&original, &mut env(), &input).unwrap();
                    assert_eq!(run(&f, &mut env(), &input).unwrap(), expected, "{f}");
                    if cond == 0 {
                        assert_eq!(expected[1], 1, "constant arm is not an int32");
                    }
                }
            }
        }
    }
}

#[test]
fn loop_carried_int32_numbers_narrow() {
    let mut f = Function::new(
        "loop_number",
        Signature::new(vec![Type::I32, Type::I32], vec![Type::I32]),
    );
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let (n, start) = (b.block_params(entry)[0], b.block_params(entry)[1]);
    let (header, exit) = (b.create_block(), b.create_block());
    let number = b.append_block_param(header, Type::F64);
    let counter = b.append_block_param(header, Type::I32);
    let first = b.convert(ConvOp::FromSint, Type::F64, start);
    let zero = b.iconst(Type::I32, 0);
    b.jump(header, &[first, zero]);
    b.switch_to_block(header);
    let one = b.iconst(Type::I32, 1);
    let next = b.binary(BinaryOp::Iadd, counter, one);
    let more = b.icmp(IntCC::Slt, next, n);
    b.brif(more, header, &[number, next], exit, &[]);
    b.seal_block(header);
    b.seal_block(exit);
    b.switch_to_block(exit);
    let int = b.convert(ConvOp::ToSintSat, Type::I32, number);
    b.ret(&[int]);
    b.finish();
    let original = f.clone();
    opt::optimize(&mut f);
    verify::verify(&f).unwrap();
    assert_eq!(
        count_insts(&f, |d| matches!(
            d,
            InstData::Convert {
                op: ConvOp::ToSintSat,
                ..
            }
        )),
        0,
        "{f}"
    );
    for start in [i32::MIN, -3, 0, i32::MAX] {
        let input = [4, start as u32 as u64];
        assert_eq!(
            run(&f, &mut env(), &input).unwrap(),
            run(&original, &mut env(), &input).unwrap()
        );
    }
}

fn loads(f: &Function) -> usize {
    count_insts(f, |d| matches!(d, InstData::Load { .. }))
}

#[test]
fn stores_forward_to_loads_in_block_and_single_predecessor_successor() {
    let mut f = Function::new(
        "forward",
        Signature::new(vec![Type::I64, Type::I64], vec![Type::I64]),
    );
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let (buf, x) = (b.block_params(entry)[0], b.block_params(entry)[1]);
    let tag = b.iconst(Type::I32, 3);
    b.store(MemKind::I32U8, buf, tag, 0);
    b.store(MemKind::I64, buf, x, 8);
    let reloaded_tag = b.load(MemKind::I32U8, buf, 0);
    let same = b.icmp(IntCC::Eq, reloaded_tag, tag);
    let (hit, miss) = (b.create_block(), b.create_block());
    b.brif(same, hit, &[], miss, &[]);
    b.seal_block(hit);
    b.seal_block(miss);
    b.switch_to_block(hit);
    let payload = b.load(MemKind::I64, buf, 8);
    b.ret(&[payload]);
    b.switch_to_block(miss);
    let zero = b.iconst(Type::I64, 0);
    b.ret(&[zero]);
    b.finish();
    assert_eq!(loads(&f), 2);
    opt::optimize(&mut f);
    verify::verify(&f).unwrap();
    assert_eq!(loads(&f), 0, "{f}");
    assert_eq!(run(&f, &mut env(), &[32, 0x1234_5678_9abc]).unwrap(), vec![0x1234_5678_9abc]);
}

#[test]
fn store_to_another_address_value_blocks_forwarding() {
    let mut f = Function::new(
        "alias",
        Signature::new(vec![Type::I64, Type::I64], vec![Type::I64]),
    );
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let (p, q) = (b.block_params(entry)[0], b.block_params(entry)[1]);
    let one = b.iconst(Type::I64, 1);
    let two = b.iconst(Type::I64, 2);
    b.store(MemKind::I64, p, one, 0);
    b.store(MemKind::I64, q, two, 0);
    let v = b.load(MemKind::I64, p, 0);
    b.ret(&[v]);
    b.finish();
    opt::optimize(&mut f);
    assert_eq!(loads(&f), 1, "{f}");
    assert_eq!(run(&f, &mut env(), &[16, 16]).unwrap(), vec![2]);
    assert_eq!(run(&f, &mut env(), &[16, 32]).unwrap(), vec![1]);
}

struct CallClobbers(BufferEnv);

impl interp::Env for CallClobbers {
    fn load(&mut self, addr: u64, bytes: u32) -> u64 {
        self.0.load(addr, bytes)
    }
    fn store(&mut self, addr: u64, bytes: u32, value: u64) {
        self.0.store(addr, bytes, value)
    }
    fn call(&mut self, _: &ExtFunc, _: &Signature, _: &[u64]) -> Result<Vec<u64>, u32> {
        self.0.store(16, 8, 55);
        Ok(vec![])
    }
    fn call_indirect(&mut self, _: &Signature, _: u64, _: &[u64]) -> Result<Vec<u64>, u32> {
        unreachable!()
    }
}

#[test]
fn call_blocks_forwarding() {
    let mut f = Function::new("call", Signature::new(vec![Type::I64], vec![Type::I64]));
    let callee = f.import_function(Signature::new(vec![], vec![]), 0);
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let p = b.block_params(entry)[0];
    let one = b.iconst(Type::I64, 1);
    b.store(MemKind::I64, p, one, 0);
    b.call_fn(callee, &[]);
    let v = b.load(MemKind::I64, p, 0);
    b.ret(&[v]);
    b.finish();
    opt::optimize(&mut f);
    assert_eq!(loads(&f), 1, "{f}");
    let mut e = CallClobbers(env());
    assert_eq!(run(&f, &mut e, &[16]).unwrap(), vec![55]);
}

#[test]
fn narrow_store_of_unbounded_value_is_not_forwarded() {
    let mut f = Function::new(
        "narrow",
        Signature::new(vec![Type::I64, Type::I32], vec![Type::I32, Type::I32]),
    );
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let (p, x) = (b.block_params(entry)[0], b.block_params(entry)[1]);
    b.store(MemKind::I32U8, p, x, 0);
    let from_arg = b.load(MemKind::I32U8, p, 0);
    let wide = b.iconst(Type::I32, 300);
    b.store(MemKind::I32U8, p, wide, 4);
    let from_const = b.load(MemKind::I32U8, p, 4);
    b.ret(&[from_arg, from_const]);
    b.finish();
    opt::optimize(&mut f);
    assert_eq!(loads(&f), 2, "{f}");
    assert_eq!(run(&f, &mut env(), &[16, 300]).unwrap(), vec![44, 44]);
}

#[test]
fn merge_blocks_do_not_inherit_known_memory() {
    let mut f = Function::new(
        "merge",
        Signature::new(vec![Type::I64, Type::I32, Type::I64], vec![Type::I64]),
    );
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let (p, c, x) = (
        b.block_params(entry)[0],
        b.block_params(entry)[1],
        b.block_params(entry)[2],
    );
    let (left, right, join) = (b.create_block(), b.create_block(), b.create_block());
    b.store(MemKind::I64, p, x, 0);
    b.brif(c, left, &[], right, &[]);
    b.seal_block(left);
    b.seal_block(right);
    b.switch_to_block(left);
    let nine = b.iconst(Type::I64, 9);
    b.store(MemKind::I64, p, nine, 0);
    b.jump(join, &[]);
    b.switch_to_block(right);
    b.jump(join, &[]);
    b.seal_block(join);
    b.switch_to_block(join);
    let v = b.load(MemKind::I64, p, 0);
    b.ret(&[v]);
    b.finish();
    opt::optimize(&mut f);
    verify::verify(&f).unwrap();
    assert_eq!(loads(&f), 1, "{f}");
    assert_eq!(run(&f, &mut env(), &[16, 1, 7]).unwrap(), vec![9]);
    assert_eq!(run(&f, &mut env(), &[16, 0, 7]).unwrap(), vec![7]);
}

fn brif_count(f: &Function) -> usize {
    f.layout
        .iter()
        .filter_map(|b| f.terminator(*b))
        .filter(|t| matches!(f.inst(*t), InstData::Brif { .. }))
        .count()
}

fn i32_function(name: &str, params: usize) -> Function {
    Function::new(name, Signature::new(vec![Type::I32; params], vec![Type::I32]))
}

#[test]
fn gvn_folds_repeated_condition_on_then_edge() {
    let mut f = i32_function("then_edge", 1);
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let x = b.block_params(entry)[0];
    let five = b.iconst(Type::I32, 5);
    let c = b.icmp(IntCC::Eq, x, five);
    let (inner, inner_then, other) = (b.create_block(), b.create_block(), b.create_block());
    b.brif(c, inner, &[], other, &[]);
    b.switch_to_block(inner);
    let again = b.icmp(IntCC::Eq, x, five);
    b.brif(again, inner_then, &[], other, &[]);
    b.switch_to_block(inner_then);
    let ten = b.iconst(Type::I32, 10);
    b.ret(&[ten]);
    b.switch_to_block(other);
    let twenty = b.iconst(Type::I32, 20);
    b.ret(&[twenty]);
    for blk in [inner, inner_then, other] {
        b.seal_block(blk);
    }
    b.finish();
    let original = f.clone();
    opt::optimize(&mut f);
    verify::verify(&f).unwrap();
    assert_eq!(brif_count(&f), 1, "{f}");
    let mut e = env();
    for x in [0u64, 4, 5, 6] {
        let want = run(&original, &mut e, &[x]).unwrap();
        assert_eq!(run(&f, &mut e, &[x]).unwrap(), want, "x={x}");
    }
}

#[test]
fn gvn_folds_repeated_condition_on_else_edge() {
    let mut f = i32_function("else_edge", 1);
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let x = b.block_params(entry)[0];
    let five = b.iconst(Type::I32, 5);
    let c = b.icmp(IntCC::Eq, x, five);
    let (hit, miss, never, last) = (
        b.create_block(),
        b.create_block(),
        b.create_block(),
        b.create_block(),
    );
    b.brif(c, hit, &[], miss, &[]);
    b.switch_to_block(hit);
    let one = b.iconst(Type::I32, 1);
    b.ret(&[one]);
    b.switch_to_block(miss);
    let again = b.icmp(IntCC::Eq, x, five);
    b.brif(again, never, &[], last, &[]);
    b.switch_to_block(never);
    let two = b.iconst(Type::I32, 2);
    b.ret(&[two]);
    b.switch_to_block(last);
    let three = b.iconst(Type::I32, 3);
    b.ret(&[three]);
    for blk in [hit, miss, never, last] {
        b.seal_block(blk);
    }
    b.finish();
    let original = f.clone();
    opt::optimize(&mut f);
    verify::verify(&f).unwrap();
    assert_eq!(brif_count(&f), 1, "{f}");
    let mut e = env();
    for x in [0u64, 4, 5, 6] {
        let want = run(&original, &mut e, &[x]).unwrap();
        assert_eq!(run(&f, &mut e, &[x]).unwrap(), want, "x={x}");
    }
}

#[test]
fn gvn_branch_facts_do_not_reach_merge_blocks() {
    let mut f = i32_function("merge", 1);
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let x = b.block_params(entry)[0];
    let five = b.iconst(Type::I32, 5);
    let c = b.icmp(IntCC::Eq, x, five);
    let (left, right, merge, yes, no) = (
        b.create_block(),
        b.create_block(),
        b.create_block(),
        b.create_block(),
        b.create_block(),
    );
    b.brif(c, left, &[], right, &[]);
    b.switch_to_block(left);
    b.jump(merge, &[]);
    b.switch_to_block(right);
    b.jump(merge, &[]);
    b.switch_to_block(merge);
    let again = b.icmp(IntCC::Eq, x, five);
    b.brif(again, yes, &[], no, &[]);
    b.switch_to_block(yes);
    let one = b.iconst(Type::I32, 1);
    b.ret(&[one]);
    b.switch_to_block(no);
    let zero = b.iconst(Type::I32, 0);
    b.ret(&[zero]);
    for blk in [left, right, merge, yes, no] {
        b.seal_block(blk);
    }
    b.finish();
    let original = f.clone();
    opt::optimize(&mut f);
    verify::verify(&f).unwrap();
    assert_eq!(brif_count(&f), 2, "{f}");
    let mut e = env();
    for x in [0u64, 5, 6] {
        let want = run(&original, &mut e, &[x]).unwrap();
        assert_eq!(run(&f, &mut e, &[x]).unwrap(), want, "x={x}");
    }
}

#[test]
fn gvn_nonzero_condition_is_not_treated_as_one() {
    let mut f = i32_function("nonzero", 1);
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let x = b.block_params(entry)[0];
    let (taken, skipped) = (b.create_block(), b.create_block());
    b.brif(x, taken, &[], skipped, &[]);
    b.seal_block(taken);
    b.seal_block(skipped);
    b.switch_to_block(taken);
    let one = b.iconst(Type::I32, 1);
    let is_one = b.icmp(IntCC::Eq, x, one);
    b.ret(&[is_one]);
    b.switch_to_block(skipped);
    let seven = b.iconst(Type::I32, 7);
    b.ret(&[seven]);
    b.finish();
    let original = f.clone();
    opt::optimize(&mut f);
    verify::verify(&f).unwrap();
    let mut e = env();
    for (x, want) in [(0u64, 7u64), (1, 1), (2, 0), (0xffff_ffff, 0)] {
        assert_eq!(run(&original, &mut e, &[x]).unwrap(), vec![want], "x={x}");
        assert_eq!(run(&f, &mut e, &[x]).unwrap(), vec![want], "x={x}");
    }
}

#[test]
fn gvn_band_condition_makes_both_operands_known() {
    let mut f = i32_function("band", 2);
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let (x, y) = (b.block_params(entry)[0], b.block_params(entry)[1]);
    let five = b.iconst(Type::I32, 5);
    let ten = b.iconst(Type::I32, 10);
    let c1 = b.icmp(IntCC::Eq, x, five);
    let c2 = b.icmp(IntCC::Ult, y, ten);
    let both = b.binary(BinaryOp::Band, c1, c2);
    let (first, second, yes, no) = (
        b.create_block(),
        b.create_block(),
        b.create_block(),
        b.create_block(),
    );
    b.brif(both, first, &[], no, &[]);
    b.switch_to_block(first);
    b.brif(c1, second, &[], no, &[]);
    b.switch_to_block(second);
    b.brif(c2, yes, &[], no, &[]);
    b.switch_to_block(yes);
    let one = b.iconst(Type::I32, 1);
    b.ret(&[one]);
    b.switch_to_block(no);
    let zero = b.iconst(Type::I32, 0);
    b.ret(&[zero]);
    for blk in [first, second, yes, no] {
        b.seal_block(blk);
    }
    b.finish();
    let original = f.clone();
    opt::optimize(&mut f);
    verify::verify(&f).unwrap();
    assert_eq!(brif_count(&f), 1, "{f}");
    let mut e = env();
    for x in [4u64, 5] {
        for y in [3u64, 10, 11] {
            let want = run(&original, &mut e, &[x, y]).unwrap();
            assert_eq!(run(&f, &mut e, &[x, y]).unwrap(), want, "x={x} y={y}");
        }
    }
}

const FLOAT_CCS: [FloatCC; 6] = [
    FloatCC::Eq,
    FloatCC::Ne,
    FloatCC::Lt,
    FloatCC::Le,
    FloatCC::Gt,
    FloatCC::Ge,
];

const EDGE_I32: [i32; 7] = [i32::MIN, -1, 0, 1, 7, 8, i32::MAX];

fn count_float_cmps(f: &Function) -> usize {
    count_insts(f, |d| matches!(d, InstData::FloatCmp { .. }))
}

#[test]
fn float_compare_of_two_int_conversions_becomes_int_compare() {
    for cc in FLOAT_CCS {
        let mut f = i32_function("both_conversions", 2);
        let mut b = FunctionBuilder::new(&mut f);
        let entry = b.create_entry_block();
        let (x, y) = (b.block_params(entry)[0], b.block_params(entry)[1]);
        let fx = b.convert(ConvOp::FromSint, Type::F64, x);
        let fy = b.convert(ConvOp::FromSint, Type::F64, y);
        let c = b.fcmp(cc, fx, fy);
        b.ret(&[c]);
        b.finish();
        let original = f.clone();
        opt::optimize(&mut f);
        verify::verify(&f).unwrap();
        assert_eq!(count_float_cmps(&f), 0, "{cc:?}\n{f}");
        let mut e = env();
        for x in EDGE_I32 {
            for y in EDGE_I32 {
                let args = [x as u32 as u64, y as u32 as u64];
                assert_eq!(
                    run(&f, &mut e, &args).unwrap(),
                    run(&original, &mut e, &args).unwrap(),
                    "{cc:?} x={x} y={y}"
                );
            }
        }
    }
}

#[test]
fn float_compare_against_integral_constant_becomes_int_compare() {
    for cc in FLOAT_CCS {
        for constant_first in [false, true] {
            for k in [0.0, -0.0, 7.0, -1.0, 2147483647.0, -2147483648.0] {
                let mut f = i32_function("conversion_vs_constant", 1);
                let mut b = FunctionBuilder::new(&mut f);
                let entry = b.create_entry_block();
                let x = b.block_params(entry)[0];
                let fx = b.convert(ConvOp::FromSint, Type::F64, x);
                let kc = b.f64const(k);
                let c = if constant_first {
                    b.fcmp(cc, kc, fx)
                } else {
                    b.fcmp(cc, fx, kc)
                };
                b.ret(&[c]);
                b.finish();
                let original = f.clone();
                opt::optimize(&mut f);
                verify::verify(&f).unwrap();
                assert_eq!(count_float_cmps(&f), 0, "{cc:?} k={k} first={constant_first}\n{f}");
                let mut e = env();
                for x in EDGE_I32 {
                    let args = [x as u32 as u64];
                    assert_eq!(
                        run(&f, &mut e, &args).unwrap(),
                        run(&original, &mut e, &args).unwrap(),
                        "{cc:?} k={k} first={constant_first} x={x}"
                    );
                }
            }
        }
    }
}

#[test]
fn float_compare_with_inexact_operands_stays_float() {
    for cc in FLOAT_CCS {
        for constant_first in [false, true] {
            let mut f = i32_function("fractional_constant", 1);
            let mut b = FunctionBuilder::new(&mut f);
            let entry = b.create_entry_block();
            let x = b.block_params(entry)[0];
            let fx = b.convert(ConvOp::FromSint, Type::F64, x);
            let half = b.f64const(0.5);
            let c = if constant_first {
                b.fcmp(cc, half, fx)
            } else {
                b.fcmp(cc, fx, half)
            };
            b.ret(&[c]);
            b.finish();
            let original = f.clone();
            opt::optimize(&mut f);
            verify::verify(&f).unwrap();
            assert_eq!(count_float_cmps(&f), 1, "{cc:?}\n{f}");
            let mut e = env();
            for x in EDGE_I32 {
                let args = [x as u32 as u64];
                assert_eq!(
                    run(&f, &mut e, &args).unwrap(),
                    run(&original, &mut e, &args).unwrap(),
                    "{cc:?} x={x}"
                );
            }
        }
    }

    for k in [3000000000.0, f64::NAN, f64::INFINITY] {
        let mut f = i32_function("out_of_range_constant", 1);
        let mut b = FunctionBuilder::new(&mut f);
        let entry = b.create_entry_block();
        let x = b.block_params(entry)[0];
        let fx = b.convert(ConvOp::FromSint, Type::F64, x);
        let kc = b.f64const(k);
        let c = b.fcmp(FloatCC::Lt, fx, kc);
        b.ret(&[c]);
        b.finish();
        let original = f.clone();
        opt::optimize(&mut f);
        verify::verify(&f).unwrap();
        let mut e = env();
        for x in EDGE_I32 {
            let args = [x as u32 as u64];
            assert_eq!(
                run(&f, &mut e, &args).unwrap(),
                run(&original, &mut e, &args).unwrap(),
                "k={k} x={x}"
            );
        }
    }

    let mut f = Function::new(
        "plain_float_argument",
        Signature::new(vec![Type::I32, Type::F64], vec![Type::I32]),
    );
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let (x, y) = (b.block_params(entry)[0], b.block_params(entry)[1]);
    let fx = b.convert(ConvOp::FromSint, Type::F64, x);
    let c = b.fcmp(FloatCC::Lt, fx, y);
    b.ret(&[c]);
    b.finish();
    let original = f.clone();
    opt::optimize(&mut f);
    verify::verify(&f).unwrap();
    assert_eq!(count_float_cmps(&f), 1, "{f}");
    let mut e = env();
    for x in EDGE_I32 {
        for y in [-1.5f64, 0.0, 7.0, f64::NAN, 3e9] {
            let args = [x as u32 as u64, y.to_bits()];
            assert_eq!(
                run(&f, &mut e, &args).unwrap(),
                run(&original, &mut e, &args).unwrap(),
                "x={x} y={y}"
            );
        }
    }
}

// ----- loop-invariant code motion --------------------------------------------------------

fn licm_sig() -> Signature {
    Signature::new(vec![Type::I64, Type::I32], vec![Type::I64])
}

/// `acc = 0; for i in 0..n { acc += step }; acc` over `(p: I64, n: I32)`. `header` emits code
/// in the loop header ahead of its test; `body` gets `p`, the I64 induction value and the
/// header's values, and returns the step.
fn licm_loop(
    f: &mut Function,
    header: impl FnOnce(&mut FunctionBuilder, Value) -> Vec<Value>,
    body: impl FnOnce(&mut FunctionBuilder, Value, Value, &[Value]) -> Value,
) {
    let mut b = FunctionBuilder::new(f);
    let entry = b.create_entry_block();
    let (p, n) = (b.block_params(entry)[0], b.block_params(entry)[1]);
    let (i, acc) = (b.declare_var(Type::I32), b.declare_var(Type::I64));
    let zero = b.iconst(Type::I32, 0);
    let zero64 = b.iconst(Type::I64, 0);
    b.def_var(i, zero);
    b.def_var(acc, zero64);
    let (h, body_block, exit) = (b.create_block(), b.create_block(), b.create_block());
    b.jump(h, &[]);

    b.switch_to_block(h);
    let values = header(&mut b, p);
    let iv = b.use_var(i);
    let c = b.icmp(IntCC::Slt, iv, n);
    b.brif(c, body_block, &[], exit, &[]);
    b.seal_block(body_block);
    b.seal_block(exit);

    b.switch_to_block(body_block);
    let iv = b.use_var(i);
    let wide = b.convert(ConvOp::Sext, Type::I64, iv);
    let step = body(&mut b, p, wide, &values);
    let a = b.use_var(acc);
    let a = b.binary(BinaryOp::Iadd, a, step);
    b.def_var(acc, a);
    let one = b.iconst(Type::I32, 1);
    let next = b.binary(BinaryOp::Iadd, iv, one);
    b.def_var(i, next);
    b.jump(h, &[]);
    b.seal_block(h);

    b.switch_to_block(exit);
    let r = b.use_var(acc);
    b.ret(&[r]);
    b.finish();
}

/// Instructions matching `pred` inside some loop.
fn in_loops(f: &Function, pred: impl Fn(&InstData) -> bool) -> usize {
    let cfg = cfg::Cfg::with_loops(f);
    f.layout
        .iter()
        .filter(|b| cfg.loop_depth[b.index()] > 0)
        .flat_map(|b| &f.blocks[b.index()].insts)
        .filter(|i| pred(f.inst(**i)))
        .count()
}

fn is_load(d: &InstData) -> bool {
    matches!(d, InstData::Load { .. })
}

/// A buffer whose word at 8 is 7 and at 16 is 3.
fn licm_env() -> BufferEnv {
    let mut e = env();
    e.mem[8] = 7;
    e.mem[16] = 3;
    e
}

/// Optimize a copy of `f`, check it verifies and computes what `f` does, and return it.
fn licm_optimized(f: &Function, mut make_env: impl FnMut() -> Box<dyn interp::Env>) -> Function {
    let mut o = f.clone();
    opt::optimize(&mut o);
    verify::verify(&o).unwrap_or_else(|e| panic!("{e}\n{o}"));
    // Trapping functions need a trampoline's TrapConfig; the interpreter runs cover them.
    if !o.to_string().contains("TrapIf") {
        aarch64::compile(&o, &aarch64::Config::host(None)).unwrap();
    }
    for n in [0u64, 1, 2, 5, 9] {
        let want = run(f, &mut *make_env(), &[0, n]).unwrap();
        assert_eq!(run(&o, &mut *make_env(), &[0, n]).unwrap(), want, "n={n}\n{o}");
    }
    o
}

fn buffer() -> Box<dyn interp::Env> {
    Box::new(licm_env())
}

#[test]
fn licm_hoists_invariant_pure_arithmetic() {
    let mut f = Function::new("licm_pure", licm_sig());
    licm_loop(
        &mut f,
        |_, _| vec![],
        |b, p, i, _| {
            let k = b.iconst(Type::I64, 0x55);
            let q = b.binary(BinaryOp::Bxor, p, k);
            let r = b.binary(BinaryOp::Imul, q, q);
            b.binary(BinaryOp::Iadd, r, i)
        },
    );
    let o = licm_optimized(&f, buffer);
    let mul = |d: &InstData| {
        matches!(
            d,
            InstData::Binary {
                op: BinaryOp::Imul | BinaryOp::Bxor,
                ..
            }
        )
    };
    assert_eq!(in_loops(&o, mul), 0, "{o}");
    assert_eq!(count_insts(&o, mul), 2, "{o}");
}

#[test]
fn licm_hoists_readonly_load_past_stores_and_calls() {
    let mut f = Function::new("licm_readonly", licm_sig());
    let callee = f.import_function(Signature::new(vec![], vec![]), 0);
    licm_loop(
        &mut f,
        |b, p| vec![b.load_with(MemKind::I64, p, 8, MemFlags::new(AliasClass::HEAP).readonly())],
        |b, p, i, h| {
            b.store(MemKind::I64, p, i, 24);
            b.call_fn(callee, &[]);
            b.binary(BinaryOp::Imul, h[0], i)
        },
    );
    let o = licm_optimized(&f, || Box::new(CallClobbers(licm_env())));
    assert_eq!(in_loops(&o, is_load), 0, "{o}");
}

#[test]
fn licm_keeps_loads_whose_memory_the_loop_writes() {
    #[derive(Clone, Copy)]
    enum Writer {
        SameClass,
        UnknownStore,
        ClobberingCall,
    }
    for writer in [Writer::SameClass, Writer::UnknownStore, Writer::ClobberingCall] {
        let mut f = Function::new("licm_written", licm_sig());
        let callee = f.import_function(Signature::new(vec![], vec![]), 0);
        let flags = MemFlags::new(AliasClass::HEAP).notrap();
        // The call writes 55 to 16.
        let offset = if matches!(writer, Writer::ClobberingCall) { 16 } else { 8 };
        licm_loop(
            &mut f,
            |b, p| vec![b.load_with(MemKind::I64, p, offset, flags)],
            |b, p, i, h| {
                match writer {
                    Writer::SameClass => b.store_with(MemKind::I64, p, i, 8, flags),
                    Writer::UnknownStore => b.store(MemKind::I64, p, i, 32),
                    Writer::ClobberingCall => {
                        b.call_fn(callee, &[]);
                    }
                }
                b.binary(BinaryOp::Iadd, h[0], i)
            },
        );
        let o = licm_optimized(&f, || Box::new(CallClobbers(licm_env())));
        assert_eq!(in_loops(&o, is_load), 1, "{o}");
    }
}

/// Calls return their argument plus one and write it to 64 (stack memory in these tests).
struct StackWriter(BufferEnv);

impl interp::Env for StackWriter {
    fn load(&mut self, addr: u64, bytes: u32) -> u64 {
        self.0.load(addr, bytes)
    }
    fn store(&mut self, addr: u64, bytes: u32, value: u64) {
        self.0.store(addr, bytes, value)
    }
    fn call(&mut self, _: &ExtFunc, _: &Signature, args: &[u64]) -> Result<Vec<u64>, u32> {
        self.0.store(64, 8, args[0]);
        Ok(vec![args[0].wrapping_add(1)])
    }
    fn call_indirect(&mut self, _: &Signature, _: u64, _: &[u64]) -> Result<Vec<u64>, u32> {
        unreachable!()
    }
}

/// The loop of [`licm_hoists_load_past_calls_and_stores_of_other_classes`]: a heap load in the
/// header, a stack store and a call writing only the stack in the body.
fn stack_writer_loop() -> Function {
    let mut f = Function::new("licm_other_class", licm_sig());
    let callee = f.import_function_with_effects(
        Signature::new(vec![Type::I64], vec![Type::I64]),
        0,
        CallEffects::writes(AliasSet::of(AliasClass::STACK)),
    );
    licm_loop(
        &mut f,
        |b, p| vec![b.load_with(MemKind::I64, p, 8, MemFlags::new(AliasClass::HEAP))],
        |b, p, i, h| {
            b.store_with(MemKind::I64, p, i, 72, MemFlags::new(AliasClass::STACK));
            let r = b.call_fn(callee, &[i])[0];
            let m = b.binary(BinaryOp::Imul, h[0], i);
            b.binary(BinaryOp::Iadd, m, r)
        },
    );
    f
}

#[test]
fn licm_hoists_load_past_calls_and_stores_of_other_classes() {
    let f = stack_writer_loop();
    let o = licm_optimized(&f, || Box::new(StackWriter(licm_env())));
    assert_eq!(in_loops(&o, is_load), 0, "{o}");
    assert_eq!(count_insts(&o, is_load), 1, "{o}");
}

#[cfg(target_arch = "aarch64")]
#[test]
fn licm_hoisted_load_runs_natively() {
    extern "C" fn plus_one(x: i64) -> i64 {
        x + 1
    }
    let mut f = stack_writer_loop();
    opt::optimize(&mut f);
    assert_eq!(in_loops(&f, is_load), 0, "{f}");
    let code = aarch64::compile(&f, &aarch64::Config::host(None)).unwrap();
    let (mem, addrs) =
        aarch64::load(&[code], |id, _| (id == 0).then_some(plus_one as extern "C" fn(i64) -> i64 as usize as u64)).unwrap();
    let _keep = mem;
    let mut buf = [0u64; 16];
    buf[1] = 7;
    for n in [0u32, 1, 6, 100] {
        let want: u64 = (0..n as u64).map(|i| 7 * i + i + 1).sum();
        let got = unsafe {
            let call: unsafe extern "C" fn(*mut u64, u32) -> u64 =
                std::mem::transmute(addrs[0] as usize);
            call(buf.as_mut_ptr(), n)
        };
        assert_eq!(got, want, "n={n}");
        assert_eq!(buf[9], n.saturating_sub(1) as u64);
    }
}

/// A loop whose body loads `p + 8` only on odd iterations.
fn conditional_load_loop(flags: MemFlags) -> Function {
    let mut f = Function::new("licm_conditional", licm_sig());
    licm_loop(
        &mut f,
        |_, _| vec![],
        |b, p, i, _| {
            let (then, join) = (b.create_block(), b.create_block());
            let v = b.append_block_param(join, Type::I64);
            let one = b.iconst(Type::I64, 1);
            let odd = b.binary(BinaryOp::Band, i, one);
            let odd = b.convert(ConvOp::Wrap, Type::I32, odd);
            let zero = b.iconst(Type::I64, 0);
            b.brif(odd, then, &[], join, &[zero]);
            b.seal_block(then);
            b.switch_to_block(then);
            let x = b.load_with(MemKind::I64, p, 8, flags);
            b.jump(join, &[x]);
            b.seal_block(join);
            b.switch_to_block(join);
            b.binary(BinaryOp::Iadd, v, i)
        },
    );
    f
}

#[test]
fn licm_hoists_conditional_loads_only_when_marked_notrap() {
    let heap = MemFlags::new(AliasClass::HEAP);
    let f = conditional_load_loop(heap);
    let o = licm_optimized(&f, buffer);
    assert_eq!(in_loops(&o, is_load), 1, "{o}");
    // Not hoisted, so a loop that never runs never touches the (out-of-range) address.
    assert_eq!(run(&o, &mut env(), &[100_000, 0]).unwrap(), vec![0]);

    let f = conditional_load_loop(heap.notrap());
    let o = licm_optimized(&f, buffer);
    assert_eq!(in_loops(&o, is_load), 0, "{o}");
}

#[test]
fn licm_hoists_header_loads_that_every_entry_runs() {
    let mut f = Function::new("licm_header", licm_sig());
    licm_loop(
        &mut f,
        |b, p| vec![b.load_with(MemKind::I64, p, 8, MemFlags::new(AliasClass::HEAP))],
        |b, _, i, h| b.binary(BinaryOp::Iadd, h[0], i),
    );
    let o = licm_optimized(&f, buffer);
    assert_eq!(in_loops(&o, is_load), 0, "{o}");
}

#[test]
fn licm_keeps_loads_of_unknown_class() {
    let mut f = Function::new("licm_unknown", licm_sig());
    licm_loop(
        &mut f,
        |b, p| vec![b.load(MemKind::I64, p, 8)],
        |b, _, i, h| b.binary(BinaryOp::Iadd, h[0], i),
    );
    let o = licm_optimized(&f, buffer);
    assert_eq!(in_loops(&o, is_load), 1, "{o}");
}

#[test]
fn licm_keeps_address_adds_that_fold_into_loop_accesses() {
    let mut f = Function::new("licm_address", licm_sig());
    licm_loop(
        &mut f,
        |_, _| vec![],
        |b, p, i, _| {
            let k = b.iconst(Type::I64, 40);
            let slot = b.binary(BinaryOp::Iadd, p, k);
            b.store(MemKind::I64, slot, i, 0);
            i
        },
    );
    let o = licm_optimized(&f, buffer);
    let add_into_store = o.layout.iter().any(|&blk| {
        o.blocks[blk.index()].insts.windows(2).any(|w| {
            matches!(o.inst(w[0]), InstData::Binary { op: BinaryOp::Iadd, .. })
                && matches!(o.inst(w[1]), InstData::Store { addr, .. } if *addr == o.results(w[0])[0])
        })
    });
    assert!(add_into_store, "{o}");
}

#[test]
fn licm_creates_a_preheader_for_a_header_with_parameters_and_two_entries() {
    let mut f = Function::new("licm_preheader", licm_sig());
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let (p, n) = (b.block_params(entry)[0], b.block_params(entry)[1]);
    let (left, right, h, body, exit) = (
        b.create_block(),
        b.create_block(),
        b.create_block(),
        b.create_block(),
        b.create_block(),
    );
    let hi = b.append_block_param(h, Type::I32);
    let hacc = b.append_block_param(h, Type::I64);
    let zero = b.iconst(Type::I32, 0);
    let one32 = b.iconst(Type::I32, 1);
    let odd = b.binary(BinaryOp::Band, n, one32);
    b.brif(odd, left, &[], right, &[]);
    b.seal_block(left);
    b.seal_block(right);
    b.switch_to_block(left);
    let hundred = b.iconst(Type::I64, 100);
    b.jump(h, &[zero, hundred]);
    b.switch_to_block(right);
    let two_hundred = b.iconst(Type::I64, 200);
    b.jump(h, &[zero, two_hundred]);
    b.switch_to_block(h);
    let c = b.icmp(IntCC::Slt, hi, n);
    b.brif(c, body, &[], exit, &[]);
    b.seal_block(body);
    b.seal_block(exit);
    b.switch_to_block(body);
    let sq = b.binary(BinaryOp::Imul, p, p);
    let acc = b.binary(BinaryOp::Iadd, hacc, sq);
    let one = b.iconst(Type::I32, 1);
    let next = b.binary(BinaryOp::Iadd, hi, one);
    b.jump(h, &[next, acc]);
    b.seal_block(h);
    b.switch_to_block(exit);
    b.ret(&[hacc]);
    b.finish();
    verify::verify(&f).unwrap();
    let original = f.clone();

    let mut g = f.clone();
    assert!(licm::licm(&mut g));
    verify::verify(&g).unwrap_or_else(|e| panic!("{e}\n{g}"));
    let cfg = cfg::Cfg::new(&g);
    let entering: Vec<Block> = cfg.preds[h.index()]
        .iter()
        .copied()
        .filter(|&p| !cfg.dominates(h, p))
        .collect();
    assert_eq!(entering.len(), 1, "{g}");
    let ph = entering[0];
    assert_ne!(ph, left);
    assert_ne!(ph, right);
    assert_eq!(g.blocks[ph.index()].params.len(), 2, "{g}");
    let mul = |d: &InstData| matches!(d, InstData::Binary { op: BinaryOp::Imul, .. });
    assert!(g.blocks[ph.index()].insts.iter().any(|&i| mul(g.inst(i))), "{g}");
    assert_eq!(in_loops(&g, mul), 0, "{g}");
    for args in [[3u64, 0], [3, 1], [5, 4], [7, 9]] {
        let want = run(&original, &mut env(), &args).unwrap();
        assert_eq!(run(&g, &mut env(), &args).unwrap(), want, "{args:?}\n{g}");
    }
    let o = licm_optimized(&f, buffer);
    assert_eq!(in_loops(&o, mul), 0, "{o}");
}

#[test]
fn licm_hoists_nested_loop_invariants_to_the_outermost_preheader() {
    let mut f = Function::new("licm_nested", licm_sig());
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let (p, n) = (b.block_params(entry)[0], b.block_params(entry)[1]);
    let (j, i, acc) = (
        b.declare_var(Type::I32),
        b.declare_var(Type::I32),
        b.declare_var(Type::I64),
    );
    let zero = b.iconst(Type::I32, 0);
    let zero64 = b.iconst(Type::I64, 0);
    let one = b.iconst(Type::I32, 1);
    b.def_var(j, zero);
    b.def_var(acc, zero64);
    let (oh, ob, ih, ib, olatch, exit) = (
        b.create_block(),
        b.create_block(),
        b.create_block(),
        b.create_block(),
        b.create_block(),
        b.create_block(),
    );
    b.jump(oh, &[]);
    b.switch_to_block(oh);
    let jv = b.use_var(j);
    let c = b.icmp(IntCC::Slt, jv, n);
    b.brif(c, ob, &[], exit, &[]);
    b.seal_block(ob);
    b.seal_block(exit);
    b.switch_to_block(ob);
    b.def_var(i, zero);
    b.jump(ih, &[]);
    b.switch_to_block(ih);
    let iv = b.use_var(i);
    let c = b.icmp(IntCC::Slt, iv, n);
    b.brif(c, ib, &[], olatch, &[]);
    b.seal_block(ib);
    b.seal_block(olatch);
    b.switch_to_block(ib);
    let sq = b.binary(BinaryOp::Imul, p, p);
    let jv = b.use_var(j);
    let jw = b.convert(ConvOp::Sext, Type::I64, jv);
    let jj = b.binary(BinaryOp::Imul, jw, jw);
    let a = b.use_var(acc);
    let a = b.binary(BinaryOp::Iadd, a, sq);
    let a = b.binary(BinaryOp::Iadd, a, jj);
    b.def_var(acc, a);
    let iv = b.use_var(i);
    let next = b.binary(BinaryOp::Iadd, iv, one);
    b.def_var(i, next);
    b.jump(ih, &[]);
    b.seal_block(ih);
    b.switch_to_block(olatch);
    let jv = b.use_var(j);
    let next = b.binary(BinaryOp::Iadd, jv, one);
    b.def_var(j, next);
    b.jump(oh, &[]);
    b.seal_block(oh);
    b.switch_to_block(exit);
    let r = b.use_var(acc);
    b.ret(&[r]);
    b.finish();

    let o = licm_optimized(&f, buffer);
    let cfg = cfg::Cfg::with_loops(&o);
    let depth_of = |pred: &dyn Fn(Value) -> bool| {
        o.layout
            .iter()
            .flat_map(|blk| o.blocks[blk.index()].insts.iter().map(move |&i| (*blk, i)))
            .filter(|&(_, i)| {
                matches!(o.inst(i), InstData::Binary { op: BinaryOp::Imul, args } if pred(args[0]))
            })
            .map(|(blk, _)| cfg.loop_depth[blk.index()])
            .collect::<Vec<_>>()
    };
    assert_eq!(depth_of(&|a| a == p), vec![0], "p * p leaves both loops\n{o}");
    assert_eq!(depth_of(&|a| a != p), vec![1], "j * j leaves the inner loop only\n{o}");
}

#[test]
fn stores_and_calls_of_other_classes_keep_known_memory() {
    let mut f = Function::new(
        "classes",
        Signature::new(vec![Type::I64, Type::I64], vec![Type::I64]),
    );
    let callee = f.import_function_with_effects(
        Signature::new(vec![Type::I64], vec![Type::I64]),
        0,
        CallEffects::writes(AliasSet::of(AliasClass::STACK)),
    );
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let (p, q) = (b.block_params(entry)[0], b.block_params(entry)[1]);
    let frame = MemFlags::new(AliasClass::FRAME);
    let x = b.iconst(Type::I64, 11);
    let y = b.iconst(Type::I64, 22);
    b.store_with(MemKind::I64, p, x, 0, frame);
    b.store_with(MemKind::I64, q, y, 0, MemFlags::new(AliasClass::HEAP));
    b.call_fn(callee, &[y]);
    let v = b.load_with(MemKind::I64, p, 0, frame);
    b.ret(&[v]);
    b.finish();
    let original = f.clone();
    opt::optimize(&mut f);
    assert_eq!(loads(&f), 0, "{f}");
    let want = run(&original, &mut StackWriter(env()), &[16, 32]).unwrap();
    assert_eq!(want, vec![11]);
    assert_eq!(run(&f, &mut StackWriter(env()), &[16, 32]).unwrap(), want);
}

#[test]
fn readonly_loads_merge_across_clobbering_calls() {
    let mut f = Function::new("readonly", Signature::new(vec![Type::I64], vec![Type::I64]));
    let callee = f.import_function(Signature::new(vec![], vec![]), 0);
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let p = b.block_params(entry)[0];
    let flags = MemFlags::new(AliasClass::FRAME).readonly();
    let a = b.load_with(MemKind::I64, p, 8, flags);
    b.call_fn(callee, &[]);
    let c = b.load_with(MemKind::I64, p, 8, flags);
    let sum = b.binary(BinaryOp::Iadd, a, c);
    b.ret(&[sum]);
    b.finish();
    opt::optimize(&mut f);
    assert_eq!(loads(&f), 1, "{f}");
    assert_eq!(run(&f, &mut CallClobbers(licm_env()), &[0]).unwrap(), vec![14]);
}

#[test]
fn licm_keeps_loads_behind_a_call_in_the_same_block() {
    let mut f = Function::new("licm_after_call", licm_sig());
    let callee =
        f.import_function_with_effects(Signature::new(vec![], vec![]), 0, CallEffects::READNONE);
    licm_loop(
        &mut f,
        |b, p| {
            b.call_fn(callee, &[]);
            vec![b.load_with(MemKind::I64, p, 8, MemFlags::new(AliasClass::HEAP))]
        },
        |b, _, i, h| b.binary(BinaryOp::Iadd, h[0], i),
    );
    let o = licm_optimized(&f, || Box::new(CallClobbers(licm_env())));
    assert_eq!(in_loops(&o, is_load), 1, "{o}");
}

#[test]
fn licm_keeps_loads_behind_a_trap_in_the_same_block() {
    let mut f = Function::new("licm_after_trap", licm_sig());
    licm_loop(
        &mut f,
        |_, _| vec![],
        |b, p, i, _| {
            let limit = b.iconst(Type::I64, 1000);
            let over = b.icmp(IntCC::Sgt, i, limit);
            b.trap_if(over, 1);
            let x = b.load_with(MemKind::I64, p, 8, MemFlags::new(AliasClass::HEAP));
            b.binary(BinaryOp::Iadd, x, i)
        },
    );
    let o = licm_optimized(&f, buffer);
    assert_eq!(in_loops(&o, is_load), 1, "{o}");
}

#[test]
fn licm_skips_a_loop_headed_by_the_entry_block() {
    let mut f = Function::new("licm_entry", licm_sig());
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_block();
    for ty in licm_sig().params {
        b.append_block_param(entry, ty);
    }
    b.switch_to_block(entry);
    let (p, n) = (b.block_params(entry)[0], b.block_params(entry)[1]);
    let exit = b.create_block();
    let sq = b.binary(BinaryOp::Imul, p, p);
    let zero = b.iconst(Type::I32, 0);
    let c = b.icmp(IntCC::Sgt, n, zero);
    b.brif(c, entry, &[sq, n], exit, &[]);
    b.seal_block(entry);
    b.seal_block(exit);
    b.switch_to_block(exit);
    b.ret(&[p]);
    b.finish();
    let before = f.to_string();
    assert!(!licm::licm(&mut f));
    assert_eq!(f.to_string(), before);
}

#[test]
fn licm_hoists_from_a_loop_whose_branch_has_both_edges_to_the_header() {
    let mut f = Function::new("licm_double_edge", licm_sig());
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let (p, n) = (b.block_params(entry)[0], b.block_params(entry)[1]);
    let (i, acc) = (b.declare_var(Type::I32), b.declare_var(Type::I64));
    let zero = b.iconst(Type::I32, 0);
    let zero64 = b.iconst(Type::I64, 0);
    b.def_var(i, zero);
    b.def_var(acc, zero64);
    let (h, body, exit) = (b.create_block(), b.create_block(), b.create_block());
    b.jump(h, &[]);
    b.switch_to_block(h);
    let iv = b.use_var(i);
    let c = b.icmp(IntCC::Slt, iv, n);
    b.brif(c, body, &[], exit, &[]);
    b.seal_block(body);
    b.seal_block(exit);
    b.switch_to_block(body);
    let sq = b.binary(BinaryOp::Imul, p, p);
    let a = b.use_var(acc);
    let a = b.binary(BinaryOp::Iadd, a, sq);
    b.def_var(acc, a);
    let iv = b.use_var(i);
    let one = b.iconst(Type::I32, 1);
    let next = b.binary(BinaryOp::Iadd, iv, one);
    b.def_var(i, next);
    let again = b.icmp(IntCC::Sge, next, zero);
    b.brif(again, h, &[], h, &[]);
    b.seal_block(h);
    b.switch_to_block(exit);
    let r = b.use_var(acc);
    b.ret(&[r]);
    b.finish();
    verify::verify(&f).unwrap();

    let mut g = f.clone();
    assert!(licm::licm(&mut g));
    verify::verify(&g).unwrap_or_else(|e| panic!("{e}\n{g}"));
    let mul = |d: &InstData| matches!(d, InstData::Binary { op: BinaryOp::Imul, .. });
    assert_eq!(in_loops(&g, mul), 0, "{g}");
    for args in [[3u64, 0], [3, 1], [5, 4]] {
        let want = run(&f, &mut env(), &args).unwrap();
        assert_eq!(run(&g, &mut env(), &args).unwrap(), want, "{args:?}\n{g}");
    }
}

#[test]
fn licm_leaves_a_loop_with_nothing_to_hoist_untouched() {
    let mut f = Function::new("licm_nothing", licm_sig());
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let n = b.block_params(entry)[1];
    let (left, right, h, body, exit) = (
        b.create_block(),
        b.create_block(),
        b.create_block(),
        b.create_block(),
        b.create_block(),
    );
    let hi = b.append_block_param(h, Type::I32);
    let hacc = b.append_block_param(h, Type::I64);
    let zero = b.iconst(Type::I32, 0);
    let one32 = b.iconst(Type::I32, 1);
    let odd = b.binary(BinaryOp::Band, n, one32);
    b.brif(odd, left, &[], right, &[]);
    b.seal_block(left);
    b.seal_block(right);
    b.switch_to_block(left);
    let hundred = b.iconst(Type::I64, 100);
    b.jump(h, &[zero, hundred]);
    b.switch_to_block(right);
    let two_hundred = b.iconst(Type::I64, 200);
    b.jump(h, &[zero, two_hundred]);
    b.switch_to_block(h);
    let c = b.icmp(IntCC::Slt, hi, n);
    b.brif(c, body, &[], exit, &[]);
    b.seal_block(body);
    b.seal_block(exit);
    b.switch_to_block(body);
    let wide = b.convert(ConvOp::Sext, Type::I64, hi);
    let acc = b.binary(BinaryOp::Iadd, hacc, wide);
    let one = b.iconst(Type::I32, 1);
    let next = b.binary(BinaryOp::Iadd, hi, one);
    b.jump(h, &[next, acc]);
    b.seal_block(h);
    b.switch_to_block(exit);
    b.ret(&[hacc]);
    b.finish();
    verify::verify(&f).unwrap();

    let before = f.to_string();
    let blocks = f.layout.len();
    assert!(!licm::licm(&mut f));
    assert_eq!(f.layout.len(), blocks);
    assert_eq!(f.to_string(), before);
}

#[test]
fn unroll_copies_alias_metadata() {
    let mut f = Function::new("unroll_meta", licm_sig());
    let flags = MemFlags::new(AliasClass::HEAP).notrap();
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let p = b.block_params(entry)[0];
    let (i, acc) = (b.declare_var(Type::I32), b.declare_var(Type::I64));
    let zero = b.iconst(Type::I32, 0);
    let zero64 = b.iconst(Type::I64, 0);
    b.def_var(i, zero);
    b.def_var(acc, zero64);
    let (h, body, exit) = (b.create_block(), b.create_block(), b.create_block());
    b.jump(h, &[]);
    b.switch_to_block(h);
    let iv = b.use_var(i);
    let three = b.iconst(Type::I32, 3);
    let c = b.icmp(IntCC::Slt, iv, three);
    b.brif(c, body, &[], exit, &[]);
    b.seal_block(body);
    b.seal_block(exit);
    b.switch_to_block(body);
    let x = b.load_with(MemKind::I64, p, 8, flags);
    let a = b.use_var(acc);
    let a = b.binary(BinaryOp::Iadd, a, x);
    b.def_var(acc, a);
    let iv = b.use_var(i);
    let one = b.iconst(Type::I32, 1);
    let next = b.binary(BinaryOp::Iadd, iv, one);
    b.def_var(i, next);
    b.jump(h, &[]);
    b.seal_block(h);
    b.switch_to_block(exit);
    let r = b.use_var(acc);
    b.ret(&[r]);
    b.finish();

    assert!(unroll::unroll(&mut f));
    verify::verify(&f).unwrap_or_else(|e| panic!("{e}\n{f}"));
    let marked = f
        .layout
        .iter()
        .flat_map(|b| f.blocks[b.index()].insts.iter().copied())
        .filter(|&i| is_load(f.inst(i)))
        .map(|i| f.mem_flags(i))
        .collect::<Vec<_>>();
    assert!(marked.len() >= 2, "{f}");
    assert!(marked.iter().all(|&m| m == flags), "{f}");
}

#[test]
fn call_between_unmarked_loads_blocks_forwarding() {
    let mut f = Function::new("unmarked", Signature::new(vec![Type::I64], vec![Type::I64]));
    let callee = f.import_function(Signature::new(vec![], vec![]), 0);
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let p = b.block_params(entry)[0];
    let a = b.load(MemKind::I64, p, 0);
    b.call_fn(callee, &[]);
    let c = b.load(MemKind::I64, p, 0);
    let sum = b.binary(BinaryOp::Iadd, a, c);
    b.ret(&[sum]);
    b.finish();
    let original = f.clone();
    opt::optimize(&mut f);
    assert_eq!(loads(&f), 2, "{f}");
    let want = run(&original, &mut CallClobbers(licm_env()), &[16]).unwrap();
    assert_eq!(want, vec![58]);
    assert_eq!(run(&f, &mut CallClobbers(licm_env()), &[16]).unwrap(), want);
}

// ----- loop versioning ---------------------------------------------------------------------

/// `acc = 0; for i in 0..n { x = [p+16]; if x == 3 { if i == call_at { call() } acc += x + i }
/// else { acc += 1000 + i } }; acc`. The guard's failure side is cold; the call (which writes
/// 55 to 16) sits on a cold block of its own.
fn versioned_loop(call_at: i64, flags: MemFlags) -> Function {
    versioned_loop_with(call_at, flags, false, false)
}

/// [`versioned_loop`] with the guarded load in the header (ahead of the exit test), and a
/// `trap_if` (never taken) ahead of it.
fn versioned_loop_with(call_at: i64, flags: MemFlags, load_in_header: bool, trap_first: bool) -> Function {
    let mut f = Function::new("version", licm_sig());
    let callee = f.import_function(Signature::new(vec![], vec![]), 0);
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let (p, n) = (b.block_params(entry)[0], b.block_params(entry)[1]);
    let (i, acc) = (b.declare_var(Type::I32), b.declare_var(Type::I64));
    let zero = b.iconst(Type::I32, 0);
    let zero64 = b.iconst(Type::I64, 0);
    b.def_var(i, zero);
    b.def_var(acc, zero64);
    let h = b.create_block();
    let (body, hot, slow, call, join, exit) = (
        b.create_block(),
        b.create_block(),
        b.create_block(),
        b.create_block(),
        b.create_block(),
        b.create_block(),
    );
    b.jump(h, &[]);

    b.switch_to_block(h);
    if trap_first {
        let limit = b.iconst(Type::I32, 1000);
        let over = b.icmp(IntCC::Sgt, n, limit);
        b.trap_if(over, 1);
    }
    let early = load_in_header.then(|| b.load_with(MemKind::I64, p, 16, flags));
    let iv = b.use_var(i);
    let c = b.icmp(IntCC::Slt, iv, n);
    b.brif(c, body, &[], exit, &[]);
    b.seal_block(body);
    b.seal_block(exit);

    b.switch_to_block(body);
    let x = early.unwrap_or_else(|| b.load_with(MemKind::I64, p, 16, flags));
    let three = b.iconst(Type::I64, 3);
    let ok = b.icmp(IntCC::Eq, x, three);
    b.brif(ok, hot, &[], slow, &[]);
    b.seal_block(hot);
    b.seal_block(slow);

    b.switch_to_block(slow);
    b.mark_cold();
    let iv = b.use_var(i);
    let wide = b.convert(ConvOp::Sext, Type::I64, iv);
    let k = b.iconst(Type::I64, 1000);
    let s = b.binary(BinaryOp::Iadd, wide, k);
    let a = b.use_var(acc);
    let a = b.binary(BinaryOp::Iadd, a, s);
    b.def_var(acc, a);
    b.jump(join, &[]);

    b.switch_to_block(hot);
    let iv = b.use_var(i);
    let wide = b.convert(ConvOp::Sext, Type::I64, iv);
    let s = b.binary(BinaryOp::Iadd, wide, x);
    let a = b.use_var(acc);
    let a = b.binary(BinaryOp::Iadd, a, s);
    b.def_var(acc, a);
    let at = b.iconst(Type::I64, call_at);
    let now = b.icmp(IntCC::Eq, wide, at);
    b.brif(now, call, &[], join, &[]);
    b.seal_block(call);

    b.switch_to_block(call);
    b.mark_cold();
    b.call_fn(callee, &[]);
    b.jump(join, &[]);
    b.seal_block(join);

    b.switch_to_block(join);
    let iv = b.use_var(i);
    let one = b.iconst(Type::I32, 1);
    let next = b.binary(BinaryOp::Iadd, iv, one);
    b.def_var(i, next);
    b.jump(h, &[]);
    b.seal_block(h);

    b.switch_to_block(exit);
    let r = b.use_var(acc);
    b.ret(&[r]);
    b.finish();
    f
}

/// Loops whose blocks hold no load.
fn load_free_loops(f: &Function) -> usize {
    let cfg = cfg::Cfg::new(f);
    let mut n = 0;
    for b in &f.layout {
        let header = cfg.preds[b.index()]
            .iter()
            .any(|p| cfg.is_reachable(*p) && cfg.dominates(*b, *p));
        if !header || !cfg.is_reachable(*b) {
            continue;
        }
        // The loop's blocks: those reaching a latch of `b` without passing `b`.
        let mut seen = vec![false; f.blocks.len()];
        let mut work: Vec<Block> = cfg.preds[b.index()]
            .iter()
            .copied()
            .filter(|p| cfg.dominates(*b, *p))
            .collect();
        seen[b.index()] = true;
        let mut loads = f.blocks[b.index()].insts.iter().filter(|i| is_load(f.inst(**i))).count();
        while let Some(x) = work.pop() {
            if seen[x.index()] {
                continue;
            }
            seen[x.index()] = true;
            loads += f.blocks[x.index()].insts.iter().filter(|i| is_load(f.inst(**i))).count();
            work.extend(cfg.preds[x.index()].iter().copied());
        }
        n += (loads == 0) as usize;
    }
    n
}

#[test]
fn version_hoists_guarded_load_out_of_fast_loop() {
    for call_at in [-1, 0, 2, 4] {
        let f = versioned_loop(call_at, MemFlags::new(AliasClass::HEAP).notrap());
        let o = licm_optimized(&f, || Box::new(CallClobbers(licm_env())));
        assert_eq!(load_free_loops(&o), 1, "call_at={call_at}\n{o}");
        let f = versioned_loop_with(call_at, MemFlags::new(AliasClass::HEAP), true, false);
        let o = licm_optimized(&f, || Box::new(CallClobbers(licm_env())));
        assert_eq!(load_free_loops(&o), 1, "call_at={call_at}\n{o}");
    }
}

#[test]
fn version_hoists_a_trapping_load_behind_the_exit_test_into_the_checked_dispatcher() {
    for call_at in [-1, 2] {
        let f = versioned_loop(call_at, MemFlags::new(AliasClass::HEAP));
        let o = licm_optimized(&f, || Box::new(CallClobbers(licm_env())));
        assert_eq!(load_free_loops(&o), 1, "call_at={call_at}\n{o}");
    }
}

#[test]
fn version_keeps_a_trapping_load_behind_a_trap() {
    for call_at in [-1, 2] {
        let f = versioned_loop_with(call_at, MemFlags::new(AliasClass::HEAP), true, true);
        let o = licm_optimized(&f, || Box::new(CallClobbers(licm_env())));
        assert_eq!(load_free_loops(&o), 0, "call_at={call_at}\n{o}");
    }
}

#[test]
fn version_skips_loads_the_hot_path_writes() {
    let mut f = Function::new("version_written", licm_sig());
    let flags = MemFlags::new(AliasClass::HEAP).notrap();
    licm_loop(
        &mut f,
        |_, _| vec![],
        |b, p, i, _| {
            let x = b.load_with(MemKind::I64, p, 16, flags);
            b.store_with(MemKind::I64, p, i, 16, flags);
            b.binary(BinaryOp::Iadd, x, i)
        },
    );
    let o = licm_optimized(&f, buffer);
    assert_eq!(load_free_loops(&o), 0, "{o}");
}
