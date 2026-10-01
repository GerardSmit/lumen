use super::*;

fn module(sections: &[(u8, &[u8])]) -> Vec<u8> {
    let mut bytes = b"\0asm\x01\0\0\0".to_vec();
    for (id, payload) in sections {
        assert!(payload.len() < 128);
        bytes.extend([*id, payload.len() as u8]);
        bytes.extend_from_slice(payload);
    }
    bytes
}

#[test]
fn data_count_precedes_code_and_data_with_custom_sections_anywhere() {
    // One passive segment, no functions; custom name and uninterpreted payload.
    let bytes = module(&[
        (0, &[1, b'a', 99]),
        (12, &[1]),
        (0, &[1, b'b']),
        (10, &[0]),
        (0, &[1, b'c', 42]),
        (11, &[1, 1, 1, 7]),
    ]);
    let parsed = decode(&bytes).unwrap();
    assert_eq!(parsed.data.len(), 1);
    assert_eq!(parsed.data[0].bytes, [7]);
    assert!(parsed.data[0].active.is_none());
}

#[test]
fn reordered_duplicate_and_mismatched_sections_are_rejected() {
    for sections in [
        vec![(10, &[0][..]), (12, &[0][..])],
        vec![(12, &[0][..]), (12, &[0][..])],
        vec![(11, &[0][..]), (10, &[0][..])],
        vec![(12, &[1][..]), (11, &[0][..])],
        vec![(12, &[1][..])],
        vec![(1, &[0][..]), (1, &[0][..])],
    ] {
        assert!(decode(&module(&sections)).is_err(), "{sections:?}");
    }
}

#[test]
fn section_decoders_cannot_borrow_next_section_bytes() {
    // Type vector declares a function but has no function body/type in its section.
    assert!(decode(&module(&[(1, &[1]), (0, &[0, 0x60, 0, 0])])).is_err());
    // Custom section declares a name longer than its own bounded payload.
    assert!(decode(&module(&[(0, &[2, b'a']), (1, &[0])])).is_err());
    // Custom name itself must use a well-formed UTF-8 encoding.
    assert!(decode(&module(&[(0, &[1, 0xff])])).is_err());
}

mod hardening {
    use super::*;
    use crate::wasm::test_util::{self as t, I32};

    fn with_code(code_section: Vec<u8>) -> Vec<u8> {
        t::module(&[
            (1, t::vec(&[t::func_type(&[], &[])])),
            (3, vec![1, 0]),
            (10, code_section),
        ])
    }

    #[test]
    fn locals_cannot_run_past_a_short_code_body() {
        // Body size 0, then bytes that would parse as a locals declaration.
        assert!(decode(&with_code(vec![1, 0, 1, 5, I32, 0x0b])).is_err());
        // Body size 2 holds only the locals declaration: no room for `end`.
        assert!(decode(&with_code(vec![1, 2, 0, 0x0b, 0x0b])).is_err());
        // The locals declaration itself is cut off by the body size.
        assert!(decode(&with_code(vec![1, 2, 1, 5, I32, 0x0b])).is_err());
        assert!(decode(&with_code(vec![1, 2, 0, 0x0b])).is_ok());
    }

    #[test]
    fn local_counts_are_capped_before_expansion() {
        let locals = |groups: &[u32]| {
            let mut b = Vec::new();
            t::uleb(groups.len() as u64, &mut b);
            for &n in groups {
                t::uleb(n as u64, &mut b);
                b.push(I32);
            }
            b.push(0x0b);
            let mut code = vec![1];
            t::uleb(b.len() as u64, &mut code);
            code.extend(b);
            with_code(code)
        };
        assert!(decode(&locals(&[u32::MAX])).is_err());
        assert!(decode(&locals(&[u32::MAX, u32::MAX])).is_err());
        assert!(decode(&locals(&[30_000, 20_001])).is_err());
        let ok = decode(&locals(&[30_000, 20_000])).unwrap();
        assert_eq!(ok.code[0].locals.len(), 50_000);
    }

    #[test]
    fn leb128_must_be_minimal_width() {
        let types = |count: &[u8]| {
            let mut p = count.to_vec();
            p.extend(t::func_type(&[], &[]));
            t::module(&[(1, p)])
        };
        assert!(decode(&types(&[0x81, 0x80, 0x80, 0x80, 0x00])).is_ok());
        // Six bytes for a u32.
        assert!(decode(&types(&[0x81, 0x80, 0x80, 0x80, 0x80, 0x00])).is_err());
        // Five bytes whose unused high bits are set.
        assert!(decode(&types(&[0x81, 0x80, 0x80, 0x80, 0x10])).is_err());

        let global = |init: &[u8]| {
            let mut g = vec![1, I32, 0];
            g.extend_from_slice(init);
            g.push(0x0b);
            t::module(&[(6, g)])
        };
        assert!(decode(&global(&[0x41, 0xff, 0xff, 0xff, 0xff, 0x7f])).is_ok()); // -1
        assert!(decode(&global(&[0x41, 0x80, 0x80, 0x80, 0x80, 0x70])).is_err());
        assert!(decode(&global(&[0x41, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00])).is_err());
    }

    #[test]
    fn imported_function_type_index_is_checked() {
        let import = |ty: u8| {
            let i = vec![1, 1, b'm', 1, b'f', 0x00, ty];
            t::module(&[(1, t::vec(&[t::func_type(&[], &[])])), (2, i)])
        };
        assert!(decode(&import(0)).is_ok());
        assert!(decode(&import(1)).is_err());
    }

    #[test]
    fn limits_are_bounded() {
        let mem = |l: &[u8]| t::module(&[(5, [&[1u8][..], l].concat())]);
        assert!(decode(&mem(&[1, 0, 0x80, 0x80, 0x04])).is_ok()); // max 65536 pages
        assert!(decode(&mem(&[0, 0x81, 0x80, 0x04])).is_err()); // min 65537 pages
        assert!(decode(&mem(&[1, 0, 0x81, 0x80, 0x04])).is_err()); // max 65537 pages
        assert!(decode(&mem(&[1, 2, 1])).is_err()); // max below min
        assert!(decode(&mem(&[3, 1, 1])).is_err()); // shared memory
        let table = |l: &[u8]| t::module(&[(4, [&[1u8, 0x70][..], l].concat())]);
        let mut big = vec![0];
        t::uleb(10_000_001, &mut big);
        assert!(decode(&table(&big)).is_err());
        let mut max = vec![0];
        t::uleb(10_000_000, &mut max);
        assert!(decode(&table(&max)).is_ok());
        assert!(decode(&table(&[1, 5, 4])).is_err());
    }

    #[test]
    fn declared_functions_need_bodies() {
        let m = t::module(&[(1, t::vec(&[t::func_type(&[], &[])])), (3, vec![1, 0])]);
        assert!(decode(&m).is_err());
    }
}
