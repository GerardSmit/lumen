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
    for blk in [inner, inner_then, other] {
        b.seal_block(blk);
    }
    b.switch_to_block(inner);
    let again = b.icmp(IntCC::Eq, x, five);
    b.brif(again, inner_then, &[], other, &[]);
    b.switch_to_block(inner_then);
    let ten = b.iconst(Type::I32, 10);
    b.ret(&[ten]);
    b.switch_to_block(other);
    let twenty = b.iconst(Type::I32, 20);
    b.ret(&[twenty]);
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
    for blk in [hit, miss, never, last] {
        b.seal_block(blk);
    }
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
    for blk in [left, right, merge, yes, no] {
        b.seal_block(blk);
    }
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
    for blk in [first, second, yes, no] {
        b.seal_block(blk);
    }
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
