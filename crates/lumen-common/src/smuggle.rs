//! Lone UTF-16 surrogates carried inside Rust strings.
//!
//! A JavaScript string is a sequence of UTF-16 code units that may hold unpaired surrogates, which
//! a Rust `str` cannot. They are *smuggled* as the plane-16 private-use scalars
//! `U+10F800 + (unit - 0xD800)`, so any unit sequence round-trips through a valid `str`.

/// First smuggled scalar: encodes the lone surrogate U+D800.
pub const SMUGGLE_BASE: u32 = 0x10F800;

/// If `c` is a smuggled lone surrogate, the surrogate code unit it encodes.
#[inline]
pub fn smuggled(c: char) -> Option<u16> {
    let v = c as u32;
    if (SMUGGLE_BASE..SMUGGLE_BASE + 0x800).contains(&v) {
        Some((v - SMUGGLE_BASE + 0xD800) as u16)
    } else {
        None
    }
}

/// Smuggle a surrogate code unit (0xD800..=0xDFFF) into its private-use scalar.
#[inline]
pub fn smuggle(unit: u16) -> char {
    debug_assert!((0xD800..0xE000).contains(&(unit as u32)));
    char::from_u32(SMUGGLE_BASE + (unit as u32 - 0xD800)).unwrap()
}

#[inline]
pub fn smuggled_high(c: char) -> Option<u16> {
    smuggled(c).filter(|u| (0xD800..0xDC00).contains(&(*u as u32)))
}

#[inline]
pub fn smuggled_low(c: char) -> Option<u16> {
    smuggled(c).filter(|u| (0xDC00..0xE000).contains(&(*u as u32)))
}

/// If `a` and `b` are a smuggled high+low pair, the real character they encode.
pub fn paired_char(a: char, b: char) -> Option<char> {
    let hi = smuggled_high(a)?;
    let lo = smuggled_low(b)?;
    char::from_u32(0x10000 + ((hi as u32 - 0xD800) << 10) + (lo as u32 - 0xDC00))
}
