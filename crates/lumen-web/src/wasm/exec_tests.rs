use std::collections::HashMap;
use std::rc::Rc;

use super::super::parse::{decode, FuncType, ValType};
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
