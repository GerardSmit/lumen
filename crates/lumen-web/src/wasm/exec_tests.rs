use std::collections::HashMap;
use std::rc::Rc;

use super::super::parse::{FuncType, ValType, decode};
use super::*;
use crate::wasm::test_util::{self as t, I32};

struct NoHost;
impl Host for NoHost {
    fn call_host(
        &mut self,
        _: usize,
        _: &[Val],
        _: &[ValType],
        _: &mut [MemEntity],
    ) -> Result<Vec<Val>, String> {
        Err("no imports".into())
    }
}

fn run(bytes: &[u8], name: &str, args: Vec<Val>) -> Result<Vec<Val>, String> {
    let mut store = Store::default();
    let inst = store.instantiate(decode(bytes)?, Imports::default())?;
    let (_, addr) = store.export_addr(inst, name).ok_or("no export")?;
    store.invoke(addr, args, &mut NoHost, 0)
}

#[test]
fn memory_copy_and_fill_lengths_are_unsigned() {
    // memory.copy(dst = 0, src = 0, n = -1) and memory.fill(0, 0, -1): n is 4 GiB - 1, not -1.
    let copy = &[0x41, 0, 0x41, 0, 0x41, 0x7f, 0xfc, 10, 0, 0];
    let fill = &[0x41, 0, 0x41, 0, 0x41, 0x7f, 0xfc, 11, 0];
    let m = t::funcs(&[(&[], &[], copy), (&[], &[], fill)], true);
    assert!(run(&m, "f0", vec![]).unwrap_err().contains("out of bounds"));
    assert!(run(&m, "f1", vec![]).unwrap_err().contains("out of bounds"));
    // dst near the end of memory; dst + n must not wrap.
    let copy_high = &[0x41, 0x7f, 0x41, 0, 0x41, 2, 0xfc, 10, 0, 0];
    let m = t::funcs(&[(&[], &[], copy_high)], true);
    assert!(run(&m, "f0", vec![]).is_err());
}

#[test]
fn loads_with_large_offsets_trap() {
    // i32.load offset=0xffffffff from address 0xffffffff.
    let load = &[0x41, 0x7f, 0x28, 2, 0xff, 0xff, 0xff, 0xff, 0x0f];
    let m = t::funcs(&[(&[], &[I32], load)], true);
    assert!(run(&m, "f0", vec![]).unwrap_err().contains("out of bounds"));
}

#[test]
fn negative_element_segment_offset_is_out_of_bounds() {
    let m = t::module(&[
        (1, t::vec(&[t::func_type(&[], &[])])),
        (3, vec![1, 0]),
        (4, vec![1, 0x70, 0, 4]),
        (9, vec![1, 0, 0x41, 0x7f, 0x0b, 2, 0, 0]), // offset -1, two entries
        (10, t::vec(&[t::body(&[])])),
    ]);
    let mut store = Store::default();
    let err = store
        .instantiate(decode(&m).unwrap(), Imports::default())
        .err()
        .unwrap();
    assert!(err.contains("out of table bounds"), "{err}");
}

#[test]
fn entity_allocation_is_bounded() {
    let mut store = Store::default();
    assert!(store.alloc_memory(65537, None).is_err());
    assert!(store.alloc_memory(1, Some(65537)).is_err());
    assert!(store.alloc_memory(2, Some(1)).is_err());
    assert!(store.alloc_memory(1, Some(2)).is_ok());
    assert!(store.alloc_table(10_000_001, None).is_err());
    assert!(store.alloc_table(2, Some(1)).is_err());
    assert_eq!(
        store
            .alloc_table(3, None)
            .map(|a| store.tables[a].elems.len()),
        Ok(3)
    );
}

#[test]
fn imported_entity_addresses_are_checked() {
    let m = t::module(&[(2, vec![1, 1, b'm', 1, b'm', 0x02, 0, 1])]);
    let mut store = Store::default();
    let imports = Imports {
        mem_addr: Some(7),
        ..Imports::default()
    };
    assert!(store.instantiate(decode(&m).unwrap(), imports).is_err());
}

#[test]
fn label_scan_rejects_malformed_bodies() {
    assert!(scan_labels(&[0x02, 0x40]).is_err()); // unclosed block
    assert!(scan_labels(&[0x0b]).is_err()); // stray end
    assert!(scan_labels(&[0x05]).is_err()); // stray else
    let mut long = vec![0x0c];
    long.extend([0x80; 12]);
    long.push(0);
    assert!(scan_labels(&long).is_err()); // over-long LEB
    assert!(scan_labels(&[0x02, 0x40, 0x0b]).is_ok());
}

#[test]
fn interpreter_traps_on_an_unvalidated_body() {
    // Bypass validation: a body that underflows its operand stack must trap, not panic.
    let mut store = Store::default();
    let inst = store
        .instantiate(decode(&t::func(&[], &[], &[])).unwrap(), Imports::default())
        .unwrap();
    for code in [
        &[0x6a][..],
        &[0x45],
        &[0x1b],
        &[0x04, 0x40, 0x0b],
        &[0x0d, 0],
        &[0x0e, 0, 0],
        &[0x41, 0, 0x11, 0, 0],
        &[0xfc, 0],
        &[0xfc, 10, 0, 0],
        &[0x43, 0],
        &[0x7f],
        &[0xa8],
        &[0x40, 0],
    ] {
        let labels = scan_labels(code).unwrap_or_else(|_| HashMap::new());
        store.funcs.push(FuncEntity::Wasm {
            compiled: Rc::new(Compiled {
                ty: FuncType {
                    params: vec![],
                    results: vec![ValType::I32],
                },
                locals: vec![],
                code: code.to_vec(),
                labels,
            }),
            instance: inst,
        });
        let addr = store.funcs.len() - 1;
        assert!(
            store.invoke(addr, vec![], &mut NoHost, 0).is_err(),
            "{code:02x?}"
        );
    }
}

#[test]
fn mutual_recursion_through_both_tiers_exhausts_the_call_stack() {
    // f0 calls f1 calls f0; f1 declares a funcref local, which the native tier cannot translate,
    // so with the JIT on every round trip crosses native code -> bridge -> interpreter.
    let m = t::module(&[
        (1, t::vec(&[t::func_type(&[], &[])])),
        (3, vec![2, 0, 0]),
        (7, t::vec(&[t::export("f0", 0, 0)])),
        (
            10,
            t::vec(&[t::body(&[0x10, 1]), vec![6, 1, 1, 0x70, 0x10, 0, 0x0b]]),
        ),
    ]);
    // Debug-build interpreter frames are large; give the 1024-deep recursion room.
    let err = std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(move || run(&m, "f0", vec![]).unwrap_err())
        .unwrap()
        .join()
        .unwrap();
    assert!(err.contains("call stack exhausted"), "{err}");
}

#[test]
fn f32_nan_payload_survives_reinterpret_and_arithmetic_is_not_canonicalised() {
    // f32.const 0x7fa00000 (signalling NaN payload); f32.neg; i32.reinterpret_f32
    let neg = &[0x43, 0, 0, 0xa0, 0x7f, 0x8c, 0xbc];
    // f32.const 0x7fc12345; i32.reinterpret_f32
    let bits = &[0x43, 0x45, 0x23, 0xc1, 0x7f, 0xbc];
    // f32.min(NaN, 1.0) is a NaN with the quiet bit set
    let min = &[0x43, 0x45, 0x23, 0xc1, 0x7f, 0x43, 0, 0, 0x80, 0x3f, 0x96, 0xbc];
    let m = t::funcs(
        &[(&[], &[I32], neg), (&[], &[I32], bits), (&[], &[I32], min)],
        false,
    );
    assert_eq!(run(&m, "f0", vec![]).unwrap()[0].i32() as u32, 0xffa0_0000);
    assert_eq!(run(&m, "f1", vec![]).unwrap()[0].i32() as u32, 0x7fc1_2345);
    let min = run(&m, "f2", vec![]).unwrap()[0].i32() as u32;
    assert_eq!(min & 0x7fc0_0000, 0x7fc0_0000);
}

mod simd {
    use super::*;
    use crate::wasm::test_util::{I64, V128, i32x4, seq, simd};

    fn i32_const(mut v: i32) -> Vec<u8> {
        let mut out = vec![0x41];
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if (v == 0 && b & 0x40 == 0) || (v == -1 && b & 0x40 != 0) {
                out.push(b);
                return out;
            }
            out.push(b | 0x80);
        }
    }

    fn i32_of(m: &[u8]) -> i32 {
        run(m, "f0", vec![]).unwrap()[0].i32()
    }

    fn i64_of(m: &[u8]) -> i64 {
        run(m, "f0", vec![]).unwrap()[0].i64()
    }

    #[test]
    fn const_and_extract_lane() {
        let body = seq(&[i32x4([1, 2, 3, 4]), simd(0x1b, &[2])]);
        assert_eq!(i32_of(&t::func(&[], &[I32], &body)), 3);
        let signed = seq(&[i32x4([0x80, 0, 0, 0]), simd(0x15, &[0])]);
        assert_eq!(i32_of(&t::func(&[], &[I32], &signed)), -128);
        let unsigned = seq(&[i32x4([0x80, 0, 0, 0]), simd(0x16, &[0])]);
        assert_eq!(i32_of(&t::func(&[], &[I32], &unsigned)), 128);
    }

    #[test]
    fn shuffle_picks_bytes_from_both_operands() {
        let a: Vec<u8> = (0..16).collect();
        let b: Vec<u8> = (0..16).map(|i| 0x80 + i).collect();
        let mut sel = [0u8; 16];
        sel[..4].copy_from_slice(&[31, 0, 16, 15]);
        let body = seq(&[simd(0x0c, &a), simd(0x0c, &b), simd(0x0d, &sel), simd(0x1b, &[0])]);
        assert_eq!(i32_of(&t::func(&[], &[I32], &body)), 0x0f80_008f);
    }

    #[test]
    fn i32x4_add_wraps_per_lane() {
        let body = seq(&[
            i32x4([1, 2, 3, 0x7fff_ffff]),
            i32x4([10, 20, 30, 1]),
            simd(0xae, &[]),
            simd(0x1b, &[3]),
        ]);
        assert_eq!(i32_of(&t::func(&[], &[I32], &body)), i32::MIN);
    }

    #[test]
    fn f32x4_min_propagates_nan_but_pmin_is_a_comparison() {
        let nan = 0x7fc1_2345;
        let one = 1.0f32.to_bits();
        let lane0 = |op: u32| {
            seq(&[
                i32x4([nan, one, one, one]),
                i32x4([one; 4]),
                simd(op, &[]),
                simd(0x1b, &[0]),
            ])
        };
        assert_eq!(i32_of(&t::func(&[], &[I32], &lane0(0xe8))) as u32, 0x7fc0_0000);
        assert_eq!(i32_of(&t::func(&[], &[I32], &lane0(0xea))) as u32, nan);
        let zeros = seq(&[
            i32x4([0x8000_0000, 0, 0, 0]),
            i32x4([0, 0, 0, 0]),
            simd(0xe8, &[]),
            simd(0x1b, &[0]),
        ]);
        assert_eq!(i32_of(&t::func(&[], &[I32], &zeros)) as u32, 0x8000_0000);
    }

    #[test]
    fn narrow_saturates_signed_lanes() {
        let body = seq(&[
            i32x4([70000, (-70000i32) as u32, 5, (-5i32) as u32]),
            i32x4([1, 2, 3, 4]),
            simd(0x85, &[]),
            simd(0x1d, &[0]),
        ]);
        assert_eq!(i64_of(&t::func(&[], &[I64], &body)) as u64, 0xfffb_0005_8000_7fff);
    }

    #[test]
    fn load_lane_replaces_one_lane_from_memory() {
        let body = seq(&[
            vec![0x41, 8],
            i32_const(0x1122_3344),
            vec![0x36, 2, 0], // i32.store
            vec![0x41, 8],
            i32x4([9, 9, 9, 9]),
            simd(0x56, &[2, 0, 2]), // v128.load32_lane lane 2
            simd(0x1b, &[2]),
        ]);
        let m = t::funcs(&[(&[], &[I32], &body)], true);
        assert_eq!(i32_of(&m), 0x1122_3344);
        let other = seq(&[
            vec![0x41, 8],
            i32x4([9, 8, 7, 6]),
            simd(0x56, &[2, 0, 2]),
            simd(0x1b, &[3]),
        ]);
        assert_eq!(i32_of(&t::funcs(&[(&[], &[I32], &other)], true)), 6);
    }

    #[test]
    fn out_of_bounds_vector_access_traps() {
        let body = seq(&[vec![0x41], vec![0xf1, 0xff, 0x03], simd(0x00, &[4, 0])]); // address 65521
        let m = t::funcs(&[(&[], &[V128], &body)], true);
        assert!(run(&m, "f0", vec![]).unwrap_err().contains("out of bounds"));
    }

    #[test]
    fn vector_values_pass_through_calls() {
        let m = t::funcs(
            &[
                (&[V128, V128], &[V128], &[0x20, 0, 0x20, 1, 0xfd, 0xae, 0x01]),
                (
                    &[],
                    &[I32],
                    &seq(&[
                        i32x4([1, 2, 3, 4]),
                        i32x4([5, 6, 7, 8]),
                        vec![0x10, 0],
                        simd(0x1b, &[3]),
                    ]),
                ),
            ],
            false,
        );
        assert_eq!(run(&m, "f1", vec![]).unwrap()[0].i32(), 12);
    }
}
