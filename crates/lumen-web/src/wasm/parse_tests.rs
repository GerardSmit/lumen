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
