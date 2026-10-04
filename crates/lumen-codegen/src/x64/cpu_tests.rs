use super::*;
use crate::{BinaryOp, CheckedOp, ConvOp, FunctionBuilder, MemKind, VectorOp};

const OPS: [VectorOp; 15] = [
    VectorOp::I32x4Add,
    VectorOp::I32x4Mul,
    VectorOp::I32x4Min,
    VectorOp::I32x4Max,
    VectorOp::I32x4Eq,
    VectorOp::I32x4Lt,
    VectorOp::F64x2Add,
    VectorOp::F64x2Mul,
    VectorOp::F64x2Min,
    VectorOp::F64x2Max,
    VectorOp::F64x2Eq,
    VectorOp::F64x2Lt,
    VectorOp::And,
    VectorOp::AndNot,
    VectorOp::Or,
];

unsafe fn invoke(code: *const u8, abi: Abi, args: [u64; 3]) -> u64 {
    if abi.win64 {
        let f: unsafe extern "win64" fn(u64, u64, u64) -> u64 =
            unsafe { std::mem::transmute(code) };
        unsafe { f(args[0], args[1], args[2]) }
    } else {
        let f: unsafe extern "sysv64" fn(u64, u64, u64) -> u64 =
            unsafe { std::mem::transmute(code) };
        unsafe { f(args[0], args[1], args[2]) }
    }
}

#[test]
fn vectors_match_ir_on_both_abis_with_baseline_and_host_features() {
    let pairs = [
        (
            0x80000000_7fffffff_ffffffff_12345678u128,
            0xffffffff_00000001_87654321_7fffffffu128,
        ),
        (0, u128::MAX),
        (u128::MAX, u128::MAX),
        (
            (-0.0f64).to_bits() as u128 | (0.0f64.to_bits() as u128) << 64,
            0.0f64.to_bits() as u128 | ((-0.0f64).to_bits() as u128) << 64,
        ),
        (
            (f64::NAN.to_bits() as u128) << 64 | 1.25f64.to_bits() as u128,
            2.0f64.to_bits() as u128 | (f64::INFINITY.to_bits() as u128) << 64,
        ),
        (
            (f64::NEG_INFINITY.to_bits() as u128) << 64 | f64::MIN_POSITIVE.to_bits() as u128,
            f64::INFINITY.to_bits() as u128 | (1u128) << 64,
        ),
        (
            0x7ff0000000000001u128 | (3.0f64.to_bits() as u128) << 64,
            4.0f64.to_bits() as u128 | (0x7ff8000000000001u128) << 64,
        ),
    ];
    for abi in [regs::win64(), regs::sysv()] {
        for features in [Features::default(), Features::host()] {
            for op in OPS {
                let mut f = Function::new(
                    "vector",
                    Signature::new(vec![Type::I64; 3], vec![Type::I64]),
                );
                let mut b = FunctionBuilder::new(&mut f);
                let entry = b.create_entry_block();
                let p = b.block_params(entry).to_vec();
                let a = b.load(MemKind::V128, p[0], 0);
                let c = b.load(MemKind::V128, p[1], 0);
                let r = b.vector_binary(op, a, c);
                b.store(MemKind::V128, p[2], r, 0);
                let zero = b.iconst(Type::I64, 0);
                b.ret(&[zero]);
                b.finish();
                crate::verify::verify(&f).unwrap();
                let compiled = compile(
                    &f,
                    &Config {
                        abi,
                        features,
                        traps: None,
                    },
                )
                .unwrap();
                let memory = crate::jitmem::ExecMemory::new(&compiled.code).unwrap();
                for (a, c) in pairs {
                    for offset in 0..16 {
                        let mut left = [0; 32];
                        let mut right = [0; 32];
                        let mut out = [0xa5; 32];
                        left[offset..offset + 16].copy_from_slice(&a.to_le_bytes());
                        right[offset..offset + 16].copy_from_slice(&c.to_le_bytes());
                        unsafe {
                            invoke(
                                memory.as_ptr(),
                                abi,
                                [
                                    left.as_ptr().add(offset) as u64,
                                    right.as_ptr().add(offset) as u64,
                                    out.as_mut_ptr().add(offset) as u64,
                                ],
                            );
                        }
                        let actual =
                            u128::from_le_bytes(out[offset..offset + 16].try_into().unwrap());
                        let expected = crate::eval::vector(op, a, c);
                        if matches!(
                            op,
                            VectorOp::F64x2Add
                                | VectorOp::F64x2Mul
                                | VectorOp::F64x2Min
                                | VectorOp::F64x2Max
                        ) {
                            for shift in [0, 64] {
                                let x = (actual >> shift) as u64;
                                let y = (expected >> shift) as u64;
                                assert!(
                                    x == y
                                        || (f64::from_bits(x).is_nan()
                                            && f64::from_bits(y).is_nan()),
                                    "{op:?}: {x:x} != {y:x}"
                                );
                            }
                        } else {
                            assert_eq!(actual, expected, "{op:?}");
                        }
                        assert!(out[..offset]
                            .iter()
                            .chain(&out[offset + 16..])
                            .all(|x| *x == 0xa5));
                    }
                }
            }
        }
    }
}

#[test]
fn vector_c_abi_is_rejected() {
    let mut f = Function::new(
        "invalid",
        Signature::new(vec![Type::V128], vec![Type::V128]),
    );
    let mut b = FunctionBuilder::new(&mut f);
    let e = b.create_entry_block();
    let v = b.block_params(e)[0];
    b.ret(&[v]);
    b.finish();
    assert!(compile(&f, &Config::host(None))
        .err()
        .unwrap()
        .contains("internal values"));
    assert!(trampoline(&f.sig, &Config::host(None))
        .unwrap_err()
        .contains("internal values"));
}

#[test]
fn aligned_loops_and_prefetch_execute_with_the_same_results() {
    use crate::IntCC;
    let mut f = Function::new(
        "aligned",
        Signature::new(vec![Type::I64; 3], vec![Type::I64]),
    );
    let mut b = FunctionBuilder::new(&mut f);
    let e = b.create_entry_block();
    let p = b.block_params(e).to_vec();
    let head = b.create_block();
    let body = b.create_block();
    let done = b.create_block();
    let i = b.append_block_param(head, Type::I64);
    let z = b.iconst(Type::I64, 0);
    b.jump(head, &[z]);
    b.switch_to_block(head);
    let test = b.icmp(IntCC::Ult, i, p[1]);
    b.brif(test, body, &[], done, &[]);
    b.switch_to_block(body);
    b.prefetch(p[0], 0);
    let one = b.iconst(Type::I64, 1);
    let next = b.binary(BinaryOp::Iadd, i, one);
    b.jump(head, &[next]);
    b.switch_to_block(done);
    b.ret(&[i]);
    b.finish();
    for abi in [regs::win64(), regs::sysv()] {
        let cfg = Config {
            abi,
            features: Features::default(),
            traps: None,
        };
        let c = compile_owned_aligned(f.clone(), &cfg, 64).unwrap();
        assert!(c.code.windows(2).any(|p| p == [0x0f, 0x18]));
        let (mem, addrs) = load_aligned(&[c], 64, |_, _| None).unwrap();
        assert_eq!(addrs[0] % 64, 0);
        let input = [0u8; 16];
        assert_eq!(
            unsafe { invoke(mem.as_ptr(), abi, [input.as_ptr() as u64, 2048, 0]) },
            2048
        );
    }
}

unsafe fn clobber_callers() {
    unsafe {
        core::arch::asm!("pxor xmm0,xmm0", "pxor xmm1,xmm1", "pxor xmm2,xmm2",
        "pxor xmm3,xmm3", "pxor xmm4,xmm4", "pxor xmm5,xmm5", out("xmm0") _,out("xmm1") _,out("xmm2") _,
        out("xmm3") _,out("xmm4") _,out("xmm5") _, options(nostack));
    }
}
extern "win64" fn win_helper() {
    unsafe {
        clobber_callers();
    }
}
extern "sysv64" fn sysv_helper() {
    unsafe {
        clobber_callers();
        core::arch::asm!("pxor xmm6,xmm6", "pxor xmm7,xmm7",
        "pxor xmm8,xmm8", "pxor xmm9,xmm9", "pxor xmm10,xmm10", "pxor xmm11,xmm11",
        "pxor xmm12,xmm12", "pxor xmm13,xmm13", "pxor xmm14,xmm14", "pxor xmm15,xmm15",
        out("xmm6") _,out("xmm7") _,
        out("xmm8") _,out("xmm9") _,out("xmm10") _,out("xmm11") _,out("xmm12") _,
        out("xmm13") _,out("xmm14") _,out("xmm15") _, options(nostack));
    }
}

#[test]
fn full_vectors_survive_spills_calls_and_block_arguments() {
    for abi in [regs::win64(), regs::sysv()] {
        let mut f = Function::new("spill", Signature::new(vec![Type::I64; 3], vec![Type::I64]));
        let mut b = FunctionBuilder::new(&mut f);
        let entry = b.create_entry_block();
        let p = b.block_params(entry).to_vec();
        let values: Vec<_> = (0..40)
            .map(|k| b.load(MemKind::V128, p[0], k * 16))
            .collect();
        let helper = b.func.import_function(Signature::default(), 1);
        b.call_fn(helper, &[]);
        let next = b.create_block();
        let params: Vec<_> = (0..40)
            .map(|_| b.append_block_param(next, Type::V128))
            .collect();
        b.jump(next, &values);
        b.switch_to_block(next);
        for (k, v) in params.into_iter().enumerate() {
            b.store(MemKind::V128, p[1], v, k as i32 * 16);
        }
        let z = b.iconst(Type::I64, 0);
        b.ret(&[z]);
        b.finish();
        let mut compiled = compile(
            &f,
            &Config {
                abi,
                features: Features::default(),
                traps: None,
            },
        )
        .unwrap();
        compiled
            .link(|_| {
                Some(if abi.win64 {
                    win_helper as *const () as u64
                } else {
                    sysv_helper as *const () as u64
                })
            })
            .unwrap();
        let memory = crate::jitmem::ExecMemory::new(&compiled.code).unwrap();
        let input: Vec<u8> = (0..640).map(|i| ((i * 17 + i / 7) ^ 0xa5) as u8).collect();
        let mut out = vec![0; 640];
        unsafe {
            invoke(
                memory.as_ptr(),
                abi,
                [input.as_ptr() as u64, out.as_mut_ptr() as u64, 0],
            );
        }
        assert_eq!(out, input);
    }
}

#[test]
fn checked_i32_uses_native_flags_and_matches_wrapped_results() {
    for abi in [regs::win64(), regs::sysv()] {
        for op in [CheckedOp::IaddOv, CheckedOp::IsubOv, CheckedOp::ImulOv] {
            let mut f = Function::new(
                "checked",
                Signature::new(vec![Type::I32; 3], vec![Type::I64]),
            );
            let mut b = FunctionBuilder::new(&mut f);
            let entry = b.create_entry_block();
            let p = b.block_params(entry).to_vec();
            let (r, o) = b.checked_binary(op, p[0], p[1]);
            let r = b.convert(ConvOp::Uext, Type::I64, r);
            let o = b.convert(ConvOp::Uext, Type::I64, o);
            let n = b.iconst(Type::I64, 32);
            let o = b.binary(BinaryOp::Ishl, o, n);
            let r = b.binary(BinaryOp::Bor, r, o);
            b.ret(&[r]);
            b.finish();
            let compiled = compile(
                &f,
                &Config {
                    abi,
                    features: Features::default(),
                    traps: None,
                },
            )
            .unwrap();
            let memory = crate::jitmem::ExecMemory::new(&compiled.code).unwrap();
            for a in [i32::MIN, i32::MAX, -50000, -1, 0, 1, 50000] {
                for c in [i32::MIN, i32::MAX, -50000, -1, 0, 1, 50000] {
                    let (r, o) = match op {
                        CheckedOp::IaddOv => a.overflowing_add(c),
                        CheckedOp::IsubOv => a.overflowing_sub(c),
                        CheckedOp::ImulOv => a.overflowing_mul(c),
                    };
                    let actual = unsafe {
                        invoke(memory.as_ptr(), abi, [a as u32 as u64, c as u32 as u64, 0])
                    };
                    assert_eq!(actual, r as u32 as u64 | ((o as u64) << 32));
                }
            }
        }
    }
}

#[test]
fn atomics_are_inline_and_sequentially_consistent() {
    use core::sync::atomic::{AtomicU32, Ordering};
    for abi in [regs::win64(), regs::sysv()] {
        for id in [crate::atomics::ADD32, crate::atomics::COMPARE_EXCHANGE32] {
            let mut f = Function::new(
                "atomic",
                Signature::new(vec![Type::I64, Type::I32, Type::I32], vec![Type::I32]),
            );
            let mut b = FunctionBuilder::new(&mut f);
            let entry = b.create_entry_block();
            let p = b.block_params(entry).to_vec();
            let sig = crate::atomics::signature(id).unwrap();
            let count = sig.params.len();
            let intrinsic = b.func.import_function(sig, id);
            let r = b.call_fn(intrinsic, &p[..count]);
            b.ret(&r);
            b.finish();
            let compiled = compile(
                &f,
                &Config {
                    abi,
                    features: Features::default(),
                    traps: None,
                },
            )
            .unwrap();
            assert!(compiled.relocs.is_empty());
            assert!(compiled.code.windows(2).any(|x| x == [0xf0, 0x0f]));
            let memory = crate::jitmem::ExecMemory::new(&compiled.code).unwrap();
            let value = AtomicU32::new(u32::MAX);
            let ptr = &value as *const AtomicU32 as u64;
            let old = unsafe { invoke(memory.as_ptr(), abi, [ptr, 1, 7]) };
            assert_eq!(old, u32::MAX as u64);
            if id == crate::atomics::ADD32 {
                assert_eq!(value.load(Ordering::SeqCst), 0);
            } else {
                assert_eq!(value.load(Ordering::SeqCst), u32::MAX);
                assert_eq!(
                    unsafe { invoke(memory.as_ptr(), abi, [ptr, u32::MAX as u64, 7]) },
                    u32::MAX as u64
                );
                assert_eq!(value.load(Ordering::SeqCst), 7);
            }
        }
    }
}
