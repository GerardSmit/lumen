use crate::interp::{run, BufferEnv};
use crate::*;

fn env() -> BufferEnv {
    BufferEnv { mem: vec![0; 256] }
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
    let after = f.layout.iter().map(|b| f.blocks[b.index()].insts.len()).sum::<usize>();
    assert!(after < before, "GVN should merge the duplicate constant and multiply\n{f}");
    let mut e = env();
    for n in [0u64, 7, 100] {
        let want = (0..n).map(|i| 6 * i).sum::<u64>();
        assert_eq!(run(&f, &mut e, &[n]).unwrap(), vec![want]);
    }
}

#[test]
fn trivial_parameters_are_removed() {
    // x is defined once before a diamond; the join's parameter for it must vanish.
    let mut f = Function::new("d", Signature::new(vec![Type::I64, Type::I32], vec![Type::I64]));
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
    assert!(f64::from_bits(binary(BinaryOp::Fmin, Type::F64, f64::NAN.to_bits(), pz).unwrap()).is_nan());
    assert_eq!(binary(BinaryOp::Srem, Type::I32, i32::MIN as u32 as u64, u32::MAX as u64), Some(0));
    assert_eq!(binary(BinaryOp::Sdiv, Type::I32, i32::MIN as u32 as u64, u32::MAX as u64), None);
    assert_eq!(binary(BinaryOp::Ishl, Type::I32, 1, 33), Some(2));
    assert_eq!(convert(ConvOp::ToSint, Type::F64, Type::I32, 2147483648.0f64.to_bits()), None);
    assert_eq!(convert(ConvOp::ToSint, Type::F64, Type::I32, (-2147483648.9f64).to_bits()), Some(0x8000_0000));
    assert_eq!(convert(ConvOp::ToUintSat, Type::F64, Type::I32, (-5.0f64).to_bits()), Some(0));
    assert_eq!(convert(ConvOp::ToUint, Type::F32, Type::I32, (-0.9f32).to_bits() as u64), Some(0));
    assert_eq!(unary(UnaryOp::Nearest, Type::F64, 2.5f64.to_bits()), 2.0f64.to_bits());
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
    let (header, body, side, latch, exit) =
        (b.create_block(), b.create_block(), b.create_block(), b.create_block(), b.create_block());
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
            assert_eq!(f64::from_bits(got[0]), reference(x, n), "n = {n}, x = {x}\n{f}");
        }
    }
}
