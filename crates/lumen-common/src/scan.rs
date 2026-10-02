//! Bounded byte/UTF-16 scans shared by parsers and string adapters.

/// Bytes before a JSON quote, escape, or forbidden control character.
pub fn json_string_prefix(bytes: &[u8]) -> usize {
    json_prefix(bytes, false)
}

/// Printable ASCII bytes that can be copied directly into a JSON string.
pub fn json_ascii_prefix(bytes: &[u8]) -> usize {
    json_prefix(bytes, true)
}

fn json_prefix(bytes: &[u8], ascii: bool) -> usize {
    #[allow(unused_mut)]
    let mut at = 0;
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    unsafe {
        use core::arch::aarch64::*;
        while bytes.len() - at >= 16 {
            let block = vld1q_u8(bytes.as_ptr().add(at));
            let mut stop = vorrq_u8(vcltq_u8(block, vdupq_n_u8(32)),
                vorrq_u8(vceqq_u8(block, vdupq_n_u8(b'"')), vceqq_u8(block, vdupq_n_u8(b'\\'))));
            if ascii { stop = vorrq_u8(stop, vcgeq_u8(block, vdupq_n_u8(128))); }
            if vmaxvq_u8(stop) != 0 { break; }
            at += 16;
        }
    }
    at + bytes[at..].iter().position(|&byte| byte < 32 || byte == b'"' || byte == b'\\' || (ascii && byte >= 128))
        .unwrap_or(bytes.len() - at)
}

/// Equal leading bytes, stopping before either slice ends.
pub fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    let length = a.len().min(b.len());
    #[allow(unused_mut)]
    let mut at = 0;
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    unsafe {
        use core::arch::aarch64::*;
        while length - at >= 16 {
            let equal = vceqq_u8(vld1q_u8(a.as_ptr().add(at)), vld1q_u8(b.as_ptr().add(at)));
            if vminvq_u8(equal) == 0 { break; }
            at += 16;
        }
    }
    at + a[at..length].iter().zip(&b[at..length]).position(|(a, b)| a != b).unwrap_or(length - at)
}

/// UTF-16 substring search, including lone surrogate units.
pub fn utf16_find(haystack: &[u16], needle: &[u16]) -> Option<usize> {
    if needle.is_empty() { return Some(0); }
    let candidates = haystack.len().checked_sub(needle.len())? + 1;
    #[allow(unused_mut)]
    let mut at = 0;
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    unsafe {
        use core::arch::aarch64::*;
        while candidates - at >= 8 {
            let matches = vceqq_u16(vld1q_u16(haystack.as_ptr().add(at)), vdupq_n_u16(needle[0]));
            if vmaxvq_u16(matches) != 0 {
                for k in at..at+8 {
                    if haystack[k..k+needle.len()] == *needle { return Some(k); }
                }
            }
            at += 8;
        }
    }
    (at..candidates).find(|&k| haystack[k..k+needle.len()] == *needle)
}

/// Length of the leading JSON whitespace run (exactly SP, TAB, LF, CR).
pub fn json_whitespace_prefix(bytes: &[u8]) -> usize {
    #[allow(unused_mut)]
    let mut at = 0;
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    unsafe {
        use core::arch::aarch64::*;
        while bytes.len() - at >= 16 {
            let block = vld1q_u8(bytes.as_ptr().add(at));
            let space = vorrq_u8(vceqq_u8(block, vdupq_n_u8(b' ')),
                vorrq_u8(vceqq_u8(block, vdupq_n_u8(b'\t')),
                    vorrq_u8(vceqq_u8(block, vdupq_n_u8(b'\n')), vceqq_u8(block, vdupq_n_u8(b'\r')))));
            if vminvq_u8(space) == 0 { break; }
            at += 16;
        }
    }
    at + bytes[at..].iter().position(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
        .unwrap_or(bytes.len() - at)
}

/// Whether UTF-16 units can be represented as ASCII without surrogate handling.
pub fn utf16_is_ascii(units: &[u16]) -> bool {
    #[allow(unused_mut)]
    let mut at = 0;
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    unsafe {
        use core::arch::aarch64::*;
        while units.len() - at >= 8 {
            if vmaxvq_u16(vld1q_u16(units.as_ptr().add(at))) >= 128 { return false; }
            at += 8;
        }
    }
    units[at..].iter().all(|&unit| unit < 128)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scans_match_scalar_at_every_length_alignment_and_stop() {
        for offset in 0..16 {
            for length in 0..80 {
                for stop in 0..=length {
                    for byte in [0, 31, b'"', b'\\', 0x80, 0xff] {
                        let mut bytes = vec![b'a'; offset + length];
                        if stop < length { bytes[offset + stop] = byte; }
                        let bytes = &bytes[offset..];
                        assert_eq!(json_string_prefix(bytes), bytes.iter()
                            .position(|&byte| byte < 32 || byte == b'"' || byte == b'\\').unwrap_or(length));
                    }
                    let mut spaces: Vec<u8> = (0..offset+length).map(|index| b" \t\n\r"[index%4]).collect();
                    if stop < length { spaces[offset + stop] = 11; }
                    assert_eq!(json_whitespace_prefix(&spaces[offset..]), stop);
                    let mut units = vec![127; offset + length];
                    assert!(utf16_is_ascii(&units[offset..]));
                    if stop < length {
                        units[offset + stop] = 128;
                        assert!(!utf16_is_ascii(&units[offset..]));
                    }
                }
            }
        }
    }
}
