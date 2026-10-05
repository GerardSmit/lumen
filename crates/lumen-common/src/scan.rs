//! Bounded byte/UTF-16 scans shared by parsers and string adapters.

/// Whether text is empty or consists only of WHATWG ASCII whitespace.
/// Non-ASCII spaces and vertical tab are not included. This borrows the input
/// and shares the exact byte classifier used by forgiving decoding and URLs.
pub fn is_ascii_whitespace_only(text: &str) -> bool {
    text.bytes().all(crate::codec::is_ascii_whitespace)
}

/// Strip and collapse WHATWG ASCII whitespace across borrowed text chunks.
/// Adjacent chunks remain adjacent unless the input contained whitespace.
/// Non-ASCII spaces and vertical tab are preserved. Only the result is stored.
pub fn strip_and_collapse_ascii_whitespace<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    let mut result = String::new();
    let mut pending_space = false;
    for part in parts {
        for character in part.chars() {
            if matches!(character, '\t' | '\n' | '\u{000c}' | '\r' | ' ') {
                pending_space = !result.is_empty();
            } else {
                if pending_space {
                    result.push(' ');
                    pending_space = false;
                }
                result.push(character);
            }
        }
    }
    result
}

/// Append the leading BMP scalars as UTF-16; return the consumed UTF-8 bytes.
/// Supplementary scalars are left to the caller's string representation policy.
pub fn utf8_bmp_prefix(text: &str, out: &mut Vec<u16>) -> usize {
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            use core::arch::x86_64::*;
            if bytes.len() - at >= 16 {
                let block = _mm_loadu_si128(bytes.as_ptr().add(at).cast());
                let zero = _mm_setzero_si128();
                if _mm_movemask_epi8(block) == 0 {
                    let mut units = [0; 16];
                    _mm_storeu_si128(units.as_mut_ptr().cast(), _mm_unpacklo_epi8(block, zero));
                    _mm_storeu_si128(
                        units.as_mut_ptr().add(8).cast(),
                        _mm_unpackhi_epi8(block, zero),
                    );
                    out.extend_from_slice(&units);
                    at += 16;
                    continue;
                }
                // Validated UTF-8 plus this lane pattern proves eight two-byte scalars.
                let pattern = _mm_and_si128(block, _mm_set1_epi16(0xc0e0u16 as i16));
                if _mm_movemask_epi8(_mm_cmpeq_epi16(pattern, _mm_set1_epi16(0x80c0u16 as i16)))
                    == 0xffff
                {
                    let units = _mm_or_si128(
                        _mm_slli_epi16::<6>(_mm_and_si128(block, _mm_set1_epi16(31))),
                        _mm_and_si128(_mm_srli_epi16::<8>(block), _mm_set1_epi16(63)),
                    );
                    let mut lanes = [0; 8];
                    _mm_storeu_si128(lanes.as_mut_ptr().cast(), units);
                    out.extend_from_slice(&lanes);
                    at += 16;
                    continue;
                }
            }
        }
        #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
        unsafe {
            use core::arch::aarch64::*;
            if bytes.len() - at >= 16 {
                let block = vld1q_u8(bytes.as_ptr().add(at));
                if vmaxvq_u8(block) < 128 {
                    let mut units = [0; 16];
                    vst1q_u16(units.as_mut_ptr(), vmovl_u8(vget_low_u8(block)));
                    vst1q_u16(units.as_mut_ptr().add(8), vmovl_high_u8(block));
                    out.extend_from_slice(&units);
                    at += 16;
                    continue;
                }
                let pair = vld2_u8(bytes.as_ptr().add(at));
                if vminv_u8(pair.0) >= 0xc2
                    && vmaxv_u8(pair.0) <= 0xdf
                    && vminv_u8(vceq_u8(vand_u8(pair.1, vdup_n_u8(0xc0)), vdup_n_u8(0x80))) != 0
                {
                    let units = vorrq_u16(
                        vshlq_n_u16::<6>(vmovl_u8(vand_u8(pair.0, vdup_n_u8(31)))),
                        vmovl_u8(vand_u8(pair.1, vdup_n_u8(63))),
                    );
                    let mut block = [0; 8];
                    vst1q_u16(block.as_mut_ptr(), units);
                    out.extend_from_slice(&block);
                    at += 16;
                    continue;
                }
            }
            if bytes.len() - at >= 24 {
                let triple = vld3_u8(bytes.as_ptr().add(at));
                // `text` is validated UTF-8. The lane pattern ensures these are eight
                // complete three-byte scalars, so overlongs/surrogates are impossible.
                let continuations = vand_u8(triple.1, triple.2);
                let high = vorr_u8(triple.1, triple.2);
                if vminv_u8(triple.0) >= 0xe0
                    && vmaxv_u8(triple.0) <= 0xef
                    && vminv_u8(vceq_u8(
                        vand_u8(continuations, vdup_n_u8(0xc0)),
                        vdup_n_u8(0x80),
                    )) != 0
                    && vmaxv_u8(vand_u8(high, vdup_n_u8(0x40))) == 0
                {
                    let units = vorrq_u16(
                        vshlq_n_u16::<12>(vmovl_u8(vand_u8(triple.0, vdup_n_u8(15)))),
                        vorrq_u16(
                            vshlq_n_u16::<6>(vmovl_u8(vand_u8(triple.1, vdup_n_u8(63)))),
                            vmovl_u8(vand_u8(triple.2, vdup_n_u8(63))),
                        ),
                    );
                    let mut block = [0; 8];
                    vst1q_u16(block.as_mut_ptr(), units);
                    out.extend_from_slice(&block);
                    at += 24;
                    continue;
                }
            }
        }
        let c = text[at..].chars().next().unwrap();
        if c as u32 >= 0x10000 {
            break;
        }
        out.push(c as u16);
        at += c.len_utf8();
    }
    at
}

/// Append leading non-surrogate UTF-16 units as UTF-8; return consumed units.
/// Surrogates are left to the caller's pairing/replacement policy.
pub fn utf16_bmp_prefix(units: &[u16], out: &mut String) -> usize {
    let mut at = 0;
    while at < units.len() {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            use core::arch::x86_64::*;
            if units.len() - at >= 8 {
                let block = _mm_loadu_si128(units.as_ptr().add(at).cast());
                let zero = _mm_setzero_si128();
                let mut bytes = [0; 16];
                let ascii =
                    _mm_cmpeq_epi16(_mm_and_si128(block, _mm_set1_epi16(0xff80u16 as i16)), zero);
                let count = if _mm_movemask_epi8(ascii) == 0xffff {
                    _mm_storel_epi64(bytes.as_mut_ptr().cast(), _mm_packus_epi16(block, zero));
                    8
                } else {
                    let high = _mm_and_si128(block, _mm_set1_epi16(0xf800u16 as i16));
                    let below = _mm_cmpeq_epi16(high, zero);
                    let above = _mm_cmpgt_epi16(block, _mm_set1_epi16(127));
                    if _mm_movemask_epi8(_mm_and_si128(below, above)) == 0xffff {
                        let lead = _mm_or_si128(_mm_srli_epi16::<6>(block), _mm_set1_epi16(0xc0));
                        let tail = _mm_or_si128(
                            _mm_slli_epi16::<8>(_mm_and_si128(block, _mm_set1_epi16(63))),
                            _mm_set1_epi16(0x8000u16 as i16),
                        );
                        _mm_storeu_si128(bytes.as_mut_ptr().cast(), _mm_or_si128(lead, tail));
                        16
                    } else {
                        0
                    }
                };
                if count != 0 {
                    out.push_str(core::str::from_utf8_unchecked(&bytes[..count]));
                    at += 8;
                    continue;
                }
            }
        }
        #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
        unsafe {
            use core::arch::aarch64::*;
            if units.len() - at >= 8 {
                let block = vld1q_u16(units.as_ptr().add(at));
                let min = vminvq_u16(block);
                let max = vmaxvq_u16(block);
                let mut bytes = [0; 24];
                let count = if max < 128 {
                    vst1_u8(bytes.as_mut_ptr(), vmovn_u16(block));
                    8
                } else if min >= 128 && max < 0x800 {
                    vst2_u8(
                        bytes.as_mut_ptr(),
                        uint8x8x2_t(
                            vorr_u8(vmovn_u16(vshrq_n_u16::<6>(block)), vdup_n_u8(0xc0)),
                            vorr_u8(vand_u8(vmovn_u16(block), vdup_n_u8(63)), vdup_n_u8(0x80)),
                        ),
                    );
                    16
                } else if min >= 0x800
                    && vmaxvq_u16(vceqq_u16(
                        vandq_u16(block, vdupq_n_u16(0xf800)),
                        vdupq_n_u16(0xd800),
                    )) == 0
                {
                    vst3_u8(
                        bytes.as_mut_ptr(),
                        uint8x8x3_t(
                            vorr_u8(vmovn_u16(vshrq_n_u16::<12>(block)), vdup_n_u8(0xe0)),
                            vorr_u8(
                                vand_u8(vmovn_u16(vshrq_n_u16::<6>(block)), vdup_n_u8(63)),
                                vdup_n_u8(0x80),
                            ),
                            vorr_u8(vand_u8(vmovn_u16(block), vdup_n_u8(63)), vdup_n_u8(0x80)),
                        ),
                    );
                    24
                } else {
                    0
                };
                if count != 0 {
                    // Lane ranges above produce only shortest, non-surrogate UTF-8.
                    out.push_str(core::str::from_utf8_unchecked(&bytes[..count]));
                    at += 8;
                    continue;
                }
            }
        }
        let Some(c) = char::from_u32(units[at] as u32) else {
            break;
        };
        out.push(c);
        at += 1;
    }
    at
}

/// Widen Latin-1 bytes to UTF-16 units without interpreting UTF-8.
pub fn latin1_to_utf16(bytes: &[u8]) -> Vec<u16> {
    let mut out = vec![0; bytes.len()];
    #[allow(unused_mut)]
    let mut at = 0;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use core::arch::x86_64::*;
        let zero = _mm_setzero_si128();
        while bytes.len() - at >= 16 {
            let block = _mm_loadu_si128(bytes.as_ptr().add(at).cast());
            _mm_storeu_si128(
                out.as_mut_ptr().add(at).cast(),
                _mm_unpacklo_epi8(block, zero),
            );
            _mm_storeu_si128(
                out.as_mut_ptr().add(at + 8).cast(),
                _mm_unpackhi_epi8(block, zero),
            );
            at += 16;
        }
    }
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    unsafe {
        use core::arch::aarch64::*;
        while bytes.len() - at >= 16 {
            let block = vld1q_u8(bytes.as_ptr().add(at));
            vst1q_u16(out.as_mut_ptr().add(at), vmovl_u8(vget_low_u8(block)));
            vst1q_u16(out.as_mut_ptr().add(at + 8), vmovl_high_u8(block));
            at += 16;
        }
    }
    for k in at..bytes.len() {
        out[k] = bytes[k] as u16;
    }
    out
}

/// Narrow UTF-16 units to ASCII, rejecting all non-ASCII units and surrogates.
pub fn utf16_to_ascii(units: &[u16]) -> Option<String> {
    let mut out = vec![0; units.len()];
    #[allow(unused_mut)]
    let mut at = 0;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use core::arch::x86_64::*;
        while units.len() - at >= 16 {
            let a = _mm_loadu_si128(units.as_ptr().add(at).cast());
            let b = _mm_loadu_si128(units.as_ptr().add(at + 8).cast());
            let high = _mm_and_si128(_mm_or_si128(a, b), _mm_set1_epi16(0xff80u16 as i16));
            if _mm_movemask_epi8(_mm_cmpeq_epi16(high, _mm_setzero_si128())) != 0xffff {
                return None;
            }
            _mm_storeu_si128(out.as_mut_ptr().add(at).cast(), _mm_packus_epi16(a, b));
            at += 16;
        }
    }
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    unsafe {
        use core::arch::aarch64::*;
        while units.len() - at >= 16 {
            let a = vld1q_u16(units.as_ptr().add(at));
            let b = vld1q_u16(units.as_ptr().add(at + 8));
            if vmaxvq_u16(vorrq_u16(a, b)) >= 128 {
                return None;
            }
            vst1q_u8(
                out.as_mut_ptr().add(at),
                vcombine_u8(vmovn_u16(a), vmovn_u16(b)),
            );
            at += 16;
        }
    }
    for k in at..units.len() {
        if units[k] >= 128 {
            return None;
        }
        out[k] = units[k] as u8;
    }
    // Every byte was checked below 128.
    Some(unsafe { String::from_utf8_unchecked(out) })
}

/// Change ASCII letters while preserving all other UTF-8 bytes.
pub fn ascii_case(text: &str, upper: bool) -> String {
    let mut out = text.as_bytes().to_vec();
    #[allow(unused_mut)]
    let mut at = 0;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use core::arch::x86_64::*;
        let first = _mm_set1_epi8(if upper { b'a' as i8 } else { b'A' as i8 });
        while out.len() - at >= 16 {
            let block = _mm_loadu_si128(out.as_ptr().add(at).cast());
            let letters = _mm_cmpeq_epi8(
                _mm_subs_epu8(_mm_sub_epi8(block, first), _mm_set1_epi8(25)),
                _mm_setzero_si128(),
            );
            let result = _mm_xor_si128(block, _mm_and_si128(letters, _mm_set1_epi8(32)));
            _mm_storeu_si128(out.as_mut_ptr().add(at).cast(), result);
            at += 16;
        }
    }
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    unsafe {
        use core::arch::aarch64::*;
        let first = vdupq_n_u8(if upper { b'a' } else { b'A' });
        while out.len() - at >= 16 {
            let block = vld1q_u8(out.as_ptr().add(at));
            let letters = vcleq_u8(vsubq_u8(block, first), vdupq_n_u8(25));
            vst1q_u8(
                out.as_mut_ptr().add(at),
                veorq_u8(block, vandq_u8(letters, vdupq_n_u8(32))),
            );
            at += 16;
        }
    }
    if upper {
        out[at..].make_ascii_uppercase();
    } else {
        out[at..].make_ascii_lowercase();
    }
    // ASCII case changes cannot alter UTF-8 validity.
    unsafe { String::from_utf8_unchecked(out) }
}

/// Repeat an encoded element using bounded bulk copies.
#[allow(clippy::manual_is_multiple_of)] // is_multiple_of needs Rust 1.87; MSRV is 1.82.
pub fn fill_pattern(bytes: &mut [u8], pattern: &[u8]) {
    assert!(!pattern.is_empty() && bytes.len() % pattern.len() == 0);
    if bytes.is_empty() {
        return;
    }
    if pattern.len() == 1 {
        bytes.fill(pattern[0]);
        return;
    }
    bytes[..pattern.len()].copy_from_slice(pattern);
    let mut filled = pattern.len();
    while filled < bytes.len() {
        let count = filled.min(bytes.len() - filled);
        bytes.copy_within(..count, filled);
        filled += count;
    }
}

/// Find a Number in little-endian encoded buffer elements (optionally equating NaNs).
#[allow(clippy::manual_is_multiple_of)] // is_multiple_of needs Rust 1.87; MSRV is 1.82.
pub fn numeric_find(
    bytes: &[u8],
    kind: crate::buffer::ElemKind,
    needle: f64,
    nan_equal: bool,
) -> Option<usize> {
    use crate::buffer::{load_f64, ByteOrder};
    let width = kind.size();
    assert!(bytes.len() % width == 0 && !kind.is_64bit_int());
    #[allow(unused_mut)]
    let mut at = 0;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use crate::buffer::ElemKind;
        use core::arch::x86_64::*;
        match kind {
            ElemKind::U8 | ElemKind::U8Clamped => {
                if needle != needle as u8 as f64 {
                    return None;
                }
                let value = _mm_set1_epi8(needle as u8 as i8);
                while bytes.len() - at >= 16 {
                    let block = _mm_loadu_si128(bytes.as_ptr().add(at).cast());
                    let matches = _mm_movemask_epi8(_mm_cmpeq_epi8(block, value)) as u32;
                    if matches != 0 {
                        return Some(at + matches.trailing_zeros() as usize);
                    }
                    at += 16;
                }
            }
            ElemKind::I32 => {
                if needle != needle as i32 as f64 {
                    return None;
                }
                let value = _mm_set1_epi32(needle as i32);
                while bytes.len() - at >= 16 {
                    let block = _mm_loadu_si128(bytes.as_ptr().add(at).cast());
                    let matches = _mm_movemask_epi8(_mm_cmpeq_epi32(block, value)) as u32;
                    if matches != 0 {
                        return Some(at / 4 + matches.trailing_zeros() as usize / 4);
                    }
                    at += 16;
                }
            }
            ElemKind::F64 => {
                let value = _mm_set1_pd(needle);
                while bytes.len() - at >= 16 {
                    let block = _mm_loadu_pd(bytes.as_ptr().add(at).cast());
                    let mask = if nan_equal && needle.is_nan() {
                        _mm_cmpunord_pd(block, block)
                    } else {
                        _mm_cmpeq_pd(block, value)
                    };
                    let matches = _mm_movemask_pd(mask) as u32;
                    if matches != 0 {
                        return Some(at / 8 + matches.trailing_zeros() as usize);
                    }
                    at += 16;
                }
            }
            _ => {}
        }
    }
    #[cfg(all(
        target_arch = "aarch64",
        target_feature = "neon",
        target_endian = "little"
    ))]
    unsafe {
        use crate::buffer::ElemKind;
        use core::arch::aarch64::*;
        match kind {
            ElemKind::U8 | ElemKind::U8Clamped => {
                if needle != needle as u8 as f64 {
                    return None;
                }
                while bytes.len() - at >= 16 {
                    if vmaxvq_u8(vceqq_u8(
                        vld1q_u8(bytes.as_ptr().add(at)),
                        vdupq_n_u8(needle as u8),
                    )) != 0
                    {
                        break;
                    }
                    at += 16;
                }
            }
            ElemKind::I32 => {
                if needle != needle as i32 as f64 {
                    return None;
                }
                while bytes.len() - at >= 16 {
                    if vmaxvq_u32(vceqq_s32(
                        vld1q_s32(bytes.as_ptr().add(at).cast()),
                        vdupq_n_s32(needle as i32),
                    )) != 0
                    {
                        break;
                    }
                    at += 16;
                }
            }
            ElemKind::F64 => {
                while bytes.len() - at >= 16 {
                    let block = vld1q_f64(bytes.as_ptr().add(at).cast());
                    let matches = if nan_equal && needle.is_nan() {
                        veorq_u64(vceqq_f64(block, block), vdupq_n_u64(u64::MAX))
                    } else {
                        vceqq_f64(block, vdupq_n_f64(needle))
                    };
                    if vgetq_lane_u64(matches, 0) | vgetq_lane_u64(matches, 1) != 0 {
                        break;
                    }
                    at += 16;
                }
            }
            _ => {}
        }
    }
    bytes[at..]
        .chunks_exact(width)
        .position(|element| {
            let value = load_f64(kind, element, ByteOrder::Little);
            value == needle || (nan_equal && needle.is_nan() && value.is_nan())
        })
        .map(|index| at / width + index)
}

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
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use core::arch::x86_64::*;
        while bytes.len() - at >= 16 {
            let block = _mm_loadu_si128(bytes.as_ptr().add(at).cast());
            let control =
                _mm_cmpeq_epi8(_mm_subs_epu8(block, _mm_set1_epi8(31)), _mm_setzero_si128());
            let stop = _mm_or_si128(
                control,
                _mm_or_si128(
                    _mm_cmpeq_epi8(block, _mm_set1_epi8(b'"' as i8)),
                    _mm_cmpeq_epi8(block, _mm_set1_epi8(b'\\' as i8)),
                ),
            );
            let mask =
                (_mm_movemask_epi8(stop) | if ascii { _mm_movemask_epi8(block) } else { 0 }) as u32;
            if mask != 0 {
                return at + mask.trailing_zeros() as usize;
            }
            at += 16;
        }
    }
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    unsafe {
        use core::arch::aarch64::*;
        while bytes.len() - at >= 16 {
            let block = vld1q_u8(bytes.as_ptr().add(at));
            let mut stop = vorrq_u8(
                vcltq_u8(block, vdupq_n_u8(32)),
                vorrq_u8(
                    vceqq_u8(block, vdupq_n_u8(b'"')),
                    vceqq_u8(block, vdupq_n_u8(b'\\')),
                ),
            );
            if ascii {
                stop = vorrq_u8(stop, vcgeq_u8(block, vdupq_n_u8(128)));
            }
            if vmaxvq_u8(stop) != 0 {
                break;
            }
            at += 16;
        }
    }
    at + bytes[at..]
        .iter()
        .position(|&byte| byte < 32 || byte == b'"' || byte == b'\\' || (ascii && byte >= 128))
        .unwrap_or(bytes.len() - at)
}

/// Exact byte equality, with bounded SIMD blocks on AArch64.
pub fn bytes_equal(a: &[u8], b: &[u8]) -> bool {
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    {
        a.len() == b.len() && common_prefix(a, b) == a.len()
    }
    #[cfg(not(all(target_arch = "aarch64", target_feature = "neon")))]
    {
        a == b
    }
}

/// Equal leading bytes, stopping before either slice ends.
pub fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    let length = a.len().min(b.len());
    #[allow(unused_mut)]
    let mut at = 0;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use core::arch::x86_64::*;
        while length - at >= 16 {
            let x = _mm_loadu_si128(a.as_ptr().add(at).cast());
            let y = _mm_loadu_si128(b.as_ptr().add(at).cast());
            let mismatch = (!_mm_movemask_epi8(_mm_cmpeq_epi8(x, y)) as u32) & 0xffff;
            if mismatch != 0 {
                return at + mismatch.trailing_zeros() as usize;
            }
            at += 16;
        }
    }
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    unsafe {
        use core::arch::aarch64::*;
        while length - at >= 16 {
            let equal = vceqq_u8(vld1q_u8(a.as_ptr().add(at)), vld1q_u8(b.as_ptr().add(at)));
            if vminvq_u8(equal) == 0 {
                break;
            }
            at += 16;
        }
    }
    at + a[at..length]
        .iter()
        .zip(&b[at..length])
        .position(|(a, b)| a != b)
        .unwrap_or(length - at)
}

/// UTF-16 substring search, including lone surrogate units.
pub fn utf16_find(haystack: &[u16], needle: &[u16]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    let candidates = haystack.len().checked_sub(needle.len())? + 1;
    #[allow(unused_mut)]
    let mut at = 0;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use core::arch::x86_64::*;
        let first = _mm_set1_epi16(needle[0] as i16);
        while candidates - at >= 8 {
            let block = _mm_loadu_si128(haystack.as_ptr().add(at).cast());
            let mut mask = _mm_movemask_epi8(_mm_cmpeq_epi16(block, first)) as u32;
            while mask != 0 {
                let lane = mask.trailing_zeros() as usize / 2;
                let k = at + lane;
                if haystack[k..k + needle.len()] == *needle {
                    return Some(k);
                }
                mask &= !(3 << (lane * 2));
            }
            at += 8;
        }
    }
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    unsafe {
        use core::arch::aarch64::*;
        while candidates - at >= 8 {
            let matches = vceqq_u16(vld1q_u16(haystack.as_ptr().add(at)), vdupq_n_u16(needle[0]));
            if vmaxvq_u16(matches) != 0 {
                for k in at..at + 8 {
                    if haystack[k..k + needle.len()] == *needle {
                        return Some(k);
                    }
                }
            }
            at += 8;
        }
    }
    (at..candidates).find(|&k| haystack[k..k + needle.len()] == *needle)
}

/// Length of the leading JSON whitespace run (exactly SP, TAB, LF, CR).
pub fn json_whitespace_prefix(bytes: &[u8]) -> usize {
    #[allow(unused_mut)]
    let mut at = 0;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use core::arch::x86_64::*;
        while bytes.len() - at >= 16 {
            let block = _mm_loadu_si128(bytes.as_ptr().add(at).cast());
            let space = _mm_or_si128(
                _mm_cmpeq_epi8(block, _mm_set1_epi8(b' ' as i8)),
                _mm_or_si128(
                    _mm_cmpeq_epi8(block, _mm_set1_epi8(b'\t' as i8)),
                    _mm_or_si128(
                        _mm_cmpeq_epi8(block, _mm_set1_epi8(b'\n' as i8)),
                        _mm_cmpeq_epi8(block, _mm_set1_epi8(b'\r' as i8)),
                    ),
                ),
            );
            let mask = (!_mm_movemask_epi8(space) as u32) & 0xffff;
            if mask != 0 {
                return at + mask.trailing_zeros() as usize;
            }
            at += 16;
        }
    }
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    unsafe {
        use core::arch::aarch64::*;
        while bytes.len() - at >= 16 {
            let block = vld1q_u8(bytes.as_ptr().add(at));
            let space = vorrq_u8(
                vceqq_u8(block, vdupq_n_u8(b' ')),
                vorrq_u8(
                    vceqq_u8(block, vdupq_n_u8(b'\t')),
                    vorrq_u8(
                        vceqq_u8(block, vdupq_n_u8(b'\n')),
                        vceqq_u8(block, vdupq_n_u8(b'\r')),
                    ),
                ),
            );
            if vminvq_u8(space) == 0 {
                break;
            }
            at += 16;
        }
    }
    at + bytes[at..]
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
        .unwrap_or(bytes.len() - at)
}

/// Whether UTF-16 units can be represented as ASCII without surrogate handling.
pub fn utf16_is_ascii(units: &[u16]) -> bool {
    #[allow(unused_mut)]
    let mut at = 0;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use core::arch::x86_64::*;
        while units.len() - at >= 8 {
            let block = _mm_loadu_si128(units.as_ptr().add(at).cast());
            let high = _mm_and_si128(block, _mm_set1_epi16(0xff80u16 as i16));
            if _mm_movemask_epi8(_mm_cmpeq_epi16(high, _mm_setzero_si128())) != 0xffff {
                return false;
            }
            at += 8;
        }
    }
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    unsafe {
        use core::arch::aarch64::*;
        while units.len() - at >= 8 {
            if vmaxvq_u16(vld1q_u16(units.as_ptr().add(at))) >= 128 {
                return false;
            }
            at += 8;
        }
    }
    units[at..].iter().all(|&unit| unit < 128)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn whitespace_only_preserves_non_html_spaces_and_url_text() {
        for text in ["", " ", "\t", "\n", "\u{000c}", "\r", " \t\n\u{000c}\r "] {
            assert!(is_ascii_whitespace_only(text));
        }
        for text in ["\u{000b}", "\u{00a0}", "\u{2003}", "\u{0000}", " a.js "] {
            assert!(!is_ascii_whitespace_only(text));
        }
    }
    #[test]
    fn scans_match_scalar_at_every_length_alignment_and_stop() {
        for offset in 0..16 {
            for length in 0..80 {
                for stop in 0..=length {
                    for byte in [0, 31, b'"', b'\\', 0x80, 0xff] {
                        let mut bytes = vec![b'a'; offset + length];
                        if stop < length {
                            bytes[offset + stop] = byte;
                        }
                        let bytes = &bytes[offset..];
                        assert_eq!(
                            json_string_prefix(bytes),
                            bytes
                                .iter()
                                .position(|&byte| byte < 32 || byte == b'"' || byte == b'\\')
                                .unwrap_or(length)
                        );
                    }
                    let mut spaces: Vec<u8> = (0..offset + length)
                        .map(|index| b" \t\n\r"[index % 4])
                        .collect();
                    if stop < length {
                        spaces[offset + stop] = 11;
                    }
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
