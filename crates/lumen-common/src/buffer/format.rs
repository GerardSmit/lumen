//! The element-format table and the one pack/unpack implementation: typed reads and writes of a
//! scalar at a byte slice with an explicit byte order. JS typed arrays / DataView and Python's
//! `memoryview` / `_struct` all encode and decode elements through these functions.

use crate::float16::{f16_bits_to_f64, f64_to_f16_bits};

/// The scalar type of one buffer element.
///
/// The first twelve kinds are the JS typed-array element types (in `TypedArray` table order);
/// `Bool` and `Char` exist for Python's `?` and `c` struct codes. Python's size-dependent codes
/// (`l`, `n`, `P`, ...) resolve to a fixed-width kind through [`struct_code`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[repr(u8)]
pub enum ElemKind {
    I8,
    U8,
    /// `Uint8ClampedArray`: stores clamp and round half to even instead of wrapping.
    U8Clamped,
    I16,
    U16,
    I32,
    U32,
    F16,
    F32,
    F64,
    I64,
    U64,
    /// One byte, nonzero = true (`?`).
    Bool,
    /// One raw byte, surfaced as a length-1 byte string (`c`).
    Char,
}

impl ElemKind {
    pub const ALL: [ElemKind; 14] = [
        ElemKind::I8,
        ElemKind::U8,
        ElemKind::U8Clamped,
        ElemKind::I16,
        ElemKind::U16,
        ElemKind::I32,
        ElemKind::U32,
        ElemKind::F16,
        ElemKind::F32,
        ElemKind::F64,
        ElemKind::I64,
        ElemKind::U64,
        ElemKind::Bool,
        ElemKind::Char,
    ];

    /// Size of one element in bytes.
    #[inline(always)]
    pub const fn size(self) -> usize {
        match self {
            ElemKind::I8 | ElemKind::U8 | ElemKind::U8Clamped | ElemKind::Bool | ElemKind::Char => {
                1
            }
            ElemKind::I16 | ElemKind::U16 | ElemKind::F16 => 2,
            ElemKind::I32 | ElemKind::U32 | ElemKind::F32 => 4,
            ElemKind::F64 | ElemKind::I64 | ElemKind::U64 => 8,
        }
    }

    #[inline]
    pub const fn is_float(self) -> bool {
        matches!(self, ElemKind::F16 | ElemKind::F32 | ElemKind::F64)
    }

    /// An integer kind (`Bool` and `Char` are not).
    #[inline]
    pub const fn is_int(self) -> bool {
        !self.is_float() && !matches!(self, ElemKind::Bool | ElemKind::Char)
    }

    #[inline]
    pub const fn is_signed(self) -> bool {
        matches!(
            self,
            ElemKind::I8
                | ElemKind::I16
                | ElemKind::I32
                | ElemKind::I64
                | ElemKind::F16
                | ElemKind::F32
                | ElemKind::F64
        )
    }

    /// A 64-bit integer kind (a BigInt element in JS).
    #[inline(always)]
    pub const fn is_64bit_int(self) -> bool {
        matches!(self, ElemKind::I64 | ElemKind::U64)
    }

    /// The inclusive value range of an integer kind.
    pub const fn int_range(self) -> Option<(i128, i128)> {
        Some(match self {
            ElemKind::I8 => (i8::MIN as i128, i8::MAX as i128),
            ElemKind::U8 | ElemKind::U8Clamped => (0, u8::MAX as i128),
            ElemKind::I16 => (i16::MIN as i128, i16::MAX as i128),
            ElemKind::U16 => (0, u16::MAX as i128),
            ElemKind::I32 => (i32::MIN as i128, i32::MAX as i128),
            ElemKind::U32 => (0, u32::MAX as i128),
            ElemKind::I64 => (i64::MIN as i128, i64::MAX as i128),
            ElemKind::U64 => (0, u64::MAX as i128),
            _ => return None,
        })
    }

    /// The fixed-size struct code for this kind (`Bool` = `?`, `Char` = `c`; `U8Clamped` has
    /// none and reports `B`).
    pub const fn struct_char(self) -> u8 {
        match self {
            ElemKind::I8 => b'b',
            ElemKind::U8 | ElemKind::U8Clamped => b'B',
            ElemKind::I16 => b'h',
            ElemKind::U16 => b'H',
            ElemKind::I32 => b'i',
            ElemKind::U32 => b'I',
            ElemKind::F16 => b'e',
            ElemKind::F32 => b'f',
            ElemKind::F64 => b'd',
            ElemKind::I64 => b'q',
            ElemKind::U64 => b'Q',
            ElemKind::Bool => b'?',
            ElemKind::Char => b'c',
        }
    }
}

/// Byte order of a multi-byte element.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum ByteOrder {
    Little,
    Big,
}

impl ByteOrder {
    pub const NATIVE: ByteOrder = if cfg!(target_endian = "big") {
        ByteOrder::Big
    } else {
        ByteOrder::Little
    };

    #[inline(always)]
    pub const fn little_if(little: bool) -> ByteOrder {
        if little {
            ByteOrder::Little
        } else {
            ByteOrder::Big
        }
    }
}

/// A Python `struct` format prefix: byte order plus native (`@`: native sizes and alignment) or
/// standard (`=`, `<`, `>`, `!`: fixed sizes, no alignment) layout.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum StructMode {
    /// `@` (also the default): native order, sizes and alignment.
    Native,
    /// `=`: native order, standard sizes.
    NativeOrder,
    /// `<`
    Little,
    /// `>` or `!`
    Big,
}

impl StructMode {
    pub const fn from_prefix(c: u8) -> Option<StructMode> {
        Some(match c {
            b'@' => StructMode::Native,
            b'=' => StructMode::NativeOrder,
            b'<' => StructMode::Little,
            b'>' | b'!' => StructMode::Big,
            _ => return None,
        })
    }

    pub const fn order(self) -> ByteOrder {
        match self {
            StructMode::Native | StructMode::NativeOrder => ByteOrder::NATIVE,
            StructMode::Little => ByteOrder::Little,
            StructMode::Big => ByteOrder::Big,
        }
    }

    /// Native sizes and alignment (`@`).
    pub const fn is_native(self) -> bool {
        matches!(self, StructMode::Native)
    }
}

/// One `struct` format character resolved for a [`StructMode`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StructCode {
    /// The element kind; `None` for the non-scalar codes `x` (pad byte), `s` and `p` (strings).
    pub kind: Option<ElemKind>,
    /// Bytes per item.
    pub size: usize,
    /// Alignment in native mode (1 in standard modes).
    pub align: usize,
}

const C_LONG: usize = if cfg!(windows) {
    4
} else {
    core::mem::size_of::<usize>()
};

/// The integer kind of `size` bytes (8 for any larger size).
pub const fn int_kind(size: usize, signed: bool) -> ElemKind {
    match (size, signed) {
        (1, true) => ElemKind::I8,
        (1, false) => ElemKind::U8,
        (2, true) => ElemKind::I16,
        (2, false) => ElemKind::U16,
        (4, true) => ElemKind::I32,
        (4, false) => ElemKind::U32,
        (_, true) => ElemKind::I64,
        (_, false) => ElemKind::U64,
    }
}

/// Resolve a `struct` format character (`x c b B ? h H i I l L q Q n N P e f d s p`). The
/// native-only codes `n`, `N` and `P` return `None` in standard modes, as in CPython.
pub const fn struct_code(c: u8, mode: StructMode) -> Option<StructCode> {
    let native = mode.is_native();
    let (kind, size) = match c {
        b'x' | b's' | b'p' => (None, 1),
        b'c' => (Some(ElemKind::Char), 1),
        b'?' => (Some(ElemKind::Bool), 1),
        b'b' => (Some(ElemKind::I8), 1),
        b'B' => (Some(ElemKind::U8), 1),
        b'h' => (Some(ElemKind::I16), 2),
        b'H' => (Some(ElemKind::U16), 2),
        b'i' => (Some(ElemKind::I32), 4),
        b'I' => (Some(ElemKind::U32), 4),
        b'l' | b'L' => {
            let size = if native { C_LONG } else { 4 };
            (Some(int_kind(size, c == b'l')), size)
        }
        b'q' => (Some(ElemKind::I64), 8),
        b'Q' => (Some(ElemKind::U64), 8),
        b'n' | b'N' | b'P' if native => {
            let size = core::mem::size_of::<usize>();
            (Some(int_kind(size, c == b'n')), size)
        }
        b'e' => (Some(ElemKind::F16), 2),
        b'f' => (Some(ElemKind::F32), 4),
        b'd' => (Some(ElemKind::F64), 8),
        _ => return None,
    };
    Some(StructCode {
        kind,
        size,
        align: if native { size } else { 1 },
    })
}

/// A decoded element, for consumers that need the exact value of any kind (Python).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Scalar {
    Int(i128),
    Float(f64),
    Bool(bool),
    Char(u8),
}

/// Why a checked store refused a value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PackError {
    /// An integer outside the kind's range (inclusive bounds).
    OutOfRange { lo: i128, hi: i128 },
    /// A finite float too large for `F16` / `F32`.
    FloatOverflow,
    /// The scalar's type does not fit the kind (a float for an integer kind, ...).
    WrongType,
}

macro_rules! get {
    ($t:ty, $b:expr, $order:expr) => {{
        const N: usize = core::mem::size_of::<$t>();
        let a: [u8; N] = $b[..N].try_into().unwrap();
        match $order {
            ByteOrder::Little => <$t>::from_le_bytes(a),
            ByteOrder::Big => <$t>::from_be_bytes(a),
        }
    }};
}

macro_rules! put {
    ($v:expr, $out:expr, $order:expr) => {{
        let v = $v;
        let a = match $order {
            ByteOrder::Little => v.to_le_bytes(),
            ByteOrder::Big => v.to_be_bytes(),
        };
        $out[..a.len()].copy_from_slice(&a);
    }};
}

/// The raw bits of the first `size` (1..=8) bytes of `b`, zero-extended.
#[inline]
pub fn load_bits(b: &[u8], size: usize, order: ByteOrder) -> u64 {
    let b = &b[..size];
    match order {
        ByteOrder::Little => b.iter().rev().fold(0u64, |a, &x| (a << 8) | x as u64),
        ByteOrder::Big => b.iter().fold(0u64, |a, &x| (a << 8) | x as u64),
    }
}

/// Store the low `size` (1..=8) bytes of `bits` into `out`.
#[inline]
pub fn store_bits(out: &mut [u8], size: usize, bits: u64, order: ByteOrder) {
    let out = &mut out[..size];
    for (k, o) in out.iter_mut().enumerate() {
        let shift = match order {
            ByteOrder::Little => 8 * k,
            ByteOrder::Big => 8 * (size - 1 - k),
        };
        *o = (bits >> shift) as u8;
    }
}

/// An element as a Number (`f64`): exact for every kind but 64-bit integers beyond 2^53, which
/// round to nearest. `Bool` reads 0/1, `Char` its byte.
#[inline(always)]
pub fn load_f64(kind: ElemKind, b: &[u8], order: ByteOrder) -> f64 {
    match kind {
        ElemKind::I8 => b[0] as i8 as f64,
        ElemKind::U8 | ElemKind::U8Clamped | ElemKind::Char => b[0] as f64,
        ElemKind::Bool => (b[0] != 0) as u8 as f64,
        ElemKind::I16 => get!(i16, b, order) as f64,
        ElemKind::U16 => get!(u16, b, order) as f64,
        ElemKind::I32 => get!(i32, b, order) as f64,
        ElemKind::U32 => get!(u32, b, order) as f64,
        ElemKind::F16 => f16_bits_to_f64(get!(u16, b, order)),
        ElemKind::F32 => get!(f32, b, order) as f64,
        ElemKind::F64 => get!(f64, b, order),
        ElemKind::I64 => get!(i64, b, order) as f64,
        ElemKind::U64 => get!(u64, b, order) as f64,
    }
}

/// An integer element exactly (floats truncate toward zero, saturating; NaN reads 0).
#[inline]
pub fn load_int(kind: ElemKind, b: &[u8], order: ByteOrder) -> i128 {
    match kind {
        ElemKind::I8 => b[0] as i8 as i128,
        ElemKind::U8 | ElemKind::U8Clamped | ElemKind::Char => b[0] as i128,
        ElemKind::Bool => (b[0] != 0) as i128,
        ElemKind::I16 => get!(i16, b, order) as i128,
        ElemKind::U16 => get!(u16, b, order) as i128,
        ElemKind::I32 => get!(i32, b, order) as i128,
        ElemKind::U32 => get!(u32, b, order) as i128,
        ElemKind::I64 => get!(i64, b, order) as i128,
        ElemKind::U64 => get!(u64, b, order) as i128,
        ElemKind::F16 | ElemKind::F32 | ElemKind::F64 => load_f64(kind, b, order) as i128,
    }
}

/// An element as its exact typed value.
#[inline]
pub fn load(kind: ElemKind, b: &[u8], order: ByteOrder) -> Scalar {
    match kind {
        ElemKind::Bool => Scalar::Bool(b[0] != 0),
        ElemKind::Char => Scalar::Char(b[0]),
        k if k.is_float() => Scalar::Float(load_f64(k, b, order)),
        k => Scalar::Int(load_int(k, b, order)),
    }
}

/// `n` truncated toward zero and reduced modulo 2^32 (enough low bits for every width up to 32);
/// non-finite values give 0. A plain `as i64` saturates at |n| >= 2^63, so 1e20 would store -1
/// in an 8-bit element instead of 0.
#[inline(always)]
fn wrap_int(n: f64) -> i64 {
    if !n.is_finite() {
        0
    } else if n.abs() < 9223372036854775808.0 {
        n.trunc() as i64
    } else {
        n.trunc().rem_euclid(4294967296.0) as i64
    }
}

/// ToUint8Clamp: NaN and negatives give 0, values above 255 give 255, otherwise round half to
/// even.
#[inline]
pub fn clamp_u8(n: f64) -> u8 {
    if n.is_nan() || n <= 0.0 {
        return 0;
    }
    if n >= 255.0 {
        return 255;
    }
    let f = n.floor();
    let r = if f + 0.5 < n {
        f + 1.0
    } else if n < f + 0.5 {
        f
    } else if (f as i64) % 2 == 1 {
        f + 1.0
    } else {
        f
    };
    r as u8
}

/// Store a Number with C-cast / ECMAScript semantics: integer kinds truncate toward zero and wrap
/// modulo 2^bits (ToInt8 .. ToUint32; non-finite gives 0; 64-bit kinds saturate at the i64 range
/// first), `U8Clamped` clamps (ToUint8Clamp), floats round to nearest even with overflow to
/// infinity, `Bool` stores whether `n` is nonzero (NaN is false).
#[inline(always)]
pub fn store_f64(kind: ElemKind, n: f64, out: &mut [u8], order: ByteOrder) {
    match kind {
        ElemKind::I8 | ElemKind::U8 | ElemKind::Char => out[0] = wrap_int(n) as u8,
        ElemKind::U8Clamped => out[0] = clamp_u8(n),
        ElemKind::Bool => out[0] = (n != 0.0 && !n.is_nan()) as u8,
        ElemKind::I16 | ElemKind::U16 => put!(wrap_int(n) as u16, out, order),
        ElemKind::I32 | ElemKind::U32 => put!(wrap_int(n) as u32, out, order),
        ElemKind::F16 => put!(f64_to_f16_bits_inf(n), out, order),
        ElemKind::F32 => put!(n as f32, out, order),
        ElemKind::F64 => put!(n, out, order),
        ElemKind::I64 | ElemKind::U64 => {
            let i = if n.is_finite() { n.trunc() as i64 } else { 0 };
            put!(i as u64, out, order)
        }
    }
}

/// Store an integer wrapping modulo 2^bits (two's complement truncation). Float kinds store
/// the nearest float, `Bool` whether `n` is nonzero, `U8Clamped` clamps.
#[inline]
pub fn store_int_wrapping(kind: ElemKind, n: i128, out: &mut [u8], order: ByteOrder) {
    match kind {
        ElemKind::I8 | ElemKind::U8 | ElemKind::Char => out[0] = n as u8,
        ElemKind::U8Clamped => out[0] = n.clamp(0, 255) as u8,
        ElemKind::Bool => out[0] = (n != 0) as u8,
        ElemKind::I16 | ElemKind::U16 => put!(n as u16, out, order),
        ElemKind::I32 | ElemKind::U32 => put!(n as u32, out, order),
        ElemKind::I64 | ElemKind::U64 => put!(n as u64, out, order),
        ElemKind::F16 | ElemKind::F32 | ElemKind::F64 => store_f64(kind, n as f64, out, order),
    }
}

/// Store an integer, refusing values outside an integer kind's range (Python `struct` /
/// `memoryview` semantics). `Bool` stores whether `n` is nonzero; a float kind is
/// [`PackError::WrongType`].
pub fn store_int_checked(
    kind: ElemKind,
    n: i128,
    out: &mut [u8],
    order: ByteOrder,
) -> Result<(), PackError> {
    if kind == ElemKind::Bool {
        out[0] = (n != 0) as u8;
        return Ok(());
    }
    let Some((lo, hi)) = kind.int_range() else {
        return Err(PackError::WrongType);
    };
    if n < lo || n > hi {
        return Err(PackError::OutOfRange { lo, hi });
    }
    store_int_wrapping(kind, n, out, order);
    Ok(())
}

/// Store a float into a float kind, refusing a finite value that overflows `F16` / `F32`
/// (Python `struct` semantics); non-float kinds are [`PackError::WrongType`].
pub fn store_float_checked(
    kind: ElemKind,
    x: f64,
    out: &mut [u8],
    order: ByteOrder,
) -> Result<(), PackError> {
    match kind {
        ElemKind::F16 => put!(
            f64_to_f16_bits(x).ok_or(PackError::FloatOverflow)?,
            out,
            order
        ),
        ElemKind::F32 => {
            let y = x as f32;
            if y.is_infinite() && x.is_finite() {
                return Err(PackError::FloatOverflow);
            }
            put!(y, out, order)
        }
        ElemKind::F64 => put!(x, out, order),
        _ => return Err(PackError::WrongType),
    }
    Ok(())
}

/// Checked store of a decoded scalar: the inverse of [`load`]. Integers may go to float kinds
/// (converted), floats only to float kinds.
pub fn store(kind: ElemKind, v: Scalar, out: &mut [u8], order: ByteOrder) -> Result<(), PackError> {
    match (kind, v) {
        (ElemKind::Bool, Scalar::Bool(b)) => out[0] = b as u8,
        (ElemKind::Char, Scalar::Char(c)) => out[0] = c,
        (k, Scalar::Int(n)) if k.is_float() => return store_float_checked(k, n as f64, out, order),
        (k, Scalar::Int(n)) if k.is_int() || k == ElemKind::Bool => {
            return store_int_checked(k, n, out, order)
        }
        (k, Scalar::Float(x)) if k.is_float() => return store_float_checked(k, x, out, order),
        _ => return Err(PackError::WrongType),
    }
    Ok(())
}

/// IEEE binary16 bits of `x` rounding once to nearest even, overflowing to infinity
/// (ECMAScript's `Math.f16round` / `Float16Array` conversion).
#[inline]
pub fn f64_to_f16_bits_inf(x: f64) -> u16 {
    match f64_to_f16_bits(x) {
        Some(h) => h,
        None => (((x.to_bits() >> 63) as u16) << 15) | 0x7c00,
    }
}

/// `x` rounded to the nearest binary16 value (`Math.f16round`).
#[inline]
pub fn f16_round(x: f64) -> f64 {
    f16_bits_to_f64(f64_to_f16_bits_inf(x))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ByteOrder::{Big, Little};

    fn enc(kind: ElemKind, n: f64, order: ByteOrder) -> Vec<u8> {
        let mut b = vec![0u8; kind.size()];
        store_f64(kind, n, &mut b, order);
        b
    }

    #[test]
    fn sizes_and_struct_chars() {
        for k in ElemKind::ALL {
            assert_eq!(
                k.size(),
                struct_code(k.struct_char(), StructMode::Little)
                    .unwrap()
                    .size,
                "{k:?}"
            );
            if k != ElemKind::U8Clamped {
                assert_eq!(
                    struct_code(k.struct_char(), StructMode::Big).unwrap().kind,
                    Some(k)
                );
            }
        }
    }

    #[test]
    fn struct_codes_native_vs_standard() {
        let l = struct_code(b'l', StructMode::Little).unwrap();
        assert_eq!((l.kind, l.size, l.align), (Some(ElemKind::I32), 4, 1));
        let ln = struct_code(b'l', StructMode::Native).unwrap();
        assert_eq!(ln.size, C_LONG);
        assert_eq!(ln.align, C_LONG);
        assert!(struct_code(b'n', StructMode::Little).is_none());
        assert!(struct_code(b'P', StructMode::Big).is_none());
        let n = struct_code(b'N', StructMode::Native).unwrap();
        assert_eq!(n.kind, Some(int_kind(core::mem::size_of::<usize>(), false)));
        assert_eq!(struct_code(b'x', StructMode::Native).unwrap().kind, None);
        assert!(struct_code(b'z', StructMode::Native).is_none());
        assert_eq!(StructMode::from_prefix(b'!'), Some(StructMode::Big));
        assert_eq!(
            StructMode::from_prefix(b'@').unwrap().order(),
            ByteOrder::NATIVE
        );
    }

    #[test]
    fn known_encodings() {
        assert_eq!(enc(ElemKind::I16, -2.0, Little), [0xfe, 0xff]);
        assert_eq!(enc(ElemKind::I16, -2.0, Big), [0xff, 0xfe]);
        assert_eq!(enc(ElemKind::U32, 0x01020304 as f64, Big), [1, 2, 3, 4]);
        assert_eq!(enc(ElemKind::U32, 0x01020304 as f64, Little), [4, 3, 2, 1]);
        assert_eq!(enc(ElemKind::F32, 1.0, Big), [0x3f, 0x80, 0, 0]);
        assert_eq!(
            enc(ElemKind::F64, 1.0, Little),
            [0, 0, 0, 0, 0, 0, 0xf0, 0x3f]
        );
        assert_eq!(enc(ElemKind::F16, 1.0, Big), [0x3c, 0x00]);
        assert_eq!(enc(ElemKind::F16, 1e6, Little), [0x00, 0x7c]);
        assert_eq!(enc(ElemKind::F16, -1e6, Little), [0x00, 0xfc]);
        assert_eq!(enc(ElemKind::I64, -1.0, Big), [0xff; 8]);
    }

    #[test]
    fn wrapping_number_stores() {
        assert_eq!(enc(ElemKind::I8, 1e20, Little), [0]);
        assert_eq!(enc(ElemKind::U8, -1.0, Little), [255]);
        assert_eq!(enc(ElemKind::U8, 257.9, Little), [1]);
        assert_eq!(enc(ElemKind::I8, f64::NAN, Little), [0]);
        assert_eq!(enc(ElemKind::U16, f64::INFINITY, Little), [0, 0]);
        assert_eq!(enc(ElemKind::I32, 4294967297.0, Little), [1, 0, 0, 0]);
        assert_eq!(enc(ElemKind::Bool, f64::NAN, Little), [0]);
        assert_eq!(enc(ElemKind::Bool, -0.5, Little), [1]);
    }

    #[test]
    fn clamped_rounds_half_to_even() {
        let c = |n| enc(ElemKind::U8Clamped, n, Little)[0];
        assert_eq!(
            [
                c(0.5),
                c(1.5),
                c(2.5),
                c(2.6),
                c(-3.0),
                c(300.0),
                c(f64::NAN)
            ],
            [0, 2, 2, 3, 0, 255, 0]
        );
    }

    #[test]
    fn round_trips_every_kind_and_order() {
        let samples = [0.0, 1.0, -1.0, 127.0, 200.0, -128.0, 65535.0, 3.5, -0.0];
        for k in ElemKind::ALL {
            for order in [Little, Big] {
                for &x in &samples {
                    let b = enc(k, x, order);
                    let mut again = vec![0u8; k.size()];
                    if k.is_64bit_int() {
                        // Beyond 2^53 a Number is lossy: 64-bit kinds round-trip as integers.
                        store_int_wrapping(k, load_int(k, &b, order), &mut again, order);
                    } else {
                        store_f64(k, load_f64(k, &b, order), &mut again, order);
                    }
                    assert_eq!(b, again, "{k:?} {order:?} {x}");
                }
            }
        }
    }

    #[test]
    fn exact_integer_loads() {
        let b = [0xff; 8];
        assert_eq!(load_int(ElemKind::U64, &b, Little), u64::MAX as i128);
        assert_eq!(load_int(ElemKind::I64, &b, Big), -1);
        assert_eq!(
            load(ElemKind::U32, &b, Little),
            Scalar::Int(u32::MAX as i128)
        );
        assert_eq!(load(ElemKind::Bool, &[2], Little), Scalar::Bool(true));
        assert_eq!(load(ElemKind::Char, b"z", Little), Scalar::Char(b'z'));
        assert_eq!(
            load(ElemKind::F16, &[0x00, 0x3c], Little),
            Scalar::Float(1.0)
        );
        let mut o = [0u8; 8];
        store_int_wrapping(ElemKind::U64, -1, &mut o, Big);
        assert_eq!(o, [0xff; 8]);
        store_int_wrapping(ElemKind::I16, 0x1_2345, &mut o, Big);
        assert_eq!(&o[..2], [0x23, 0x45]);
    }

    #[test]
    fn checked_stores() {
        let mut o = [0u8; 8];
        assert_eq!(
            store_int_checked(ElemKind::I8, 128, &mut o, Little),
            Err(PackError::OutOfRange { lo: -128, hi: 127 })
        );
        assert_eq!(
            store_int_checked(ElemKind::U16, -1, &mut o, Little),
            Err(PackError::OutOfRange { lo: 0, hi: 65535 })
        );
        assert_eq!(
            store_int_checked(ElemKind::U16, 0x1234, &mut o, Big),
            Ok(())
        );
        assert_eq!(&o[..2], [0x12, 0x34]);
        assert_eq!(
            store_int_checked(ElemKind::F64, 1, &mut o, Big),
            Err(PackError::WrongType)
        );
        assert_eq!(
            store_float_checked(ElemKind::F16, 65520.0, &mut o, Little),
            Err(PackError::FloatOverflow)
        );
        assert_eq!(
            store_float_checked(ElemKind::F32, 1e300, &mut o, Little),
            Err(PackError::FloatOverflow)
        );
        assert_eq!(
            store_float_checked(ElemKind::F32, f64::INFINITY, &mut o, Little),
            Ok(())
        );
        assert_eq!(store(ElemKind::F64, Scalar::Int(3), &mut o, Little), Ok(()));
        assert_eq!(load(ElemKind::F64, &o, Little), Scalar::Float(3.0));
        assert_eq!(
            store(ElemKind::I32, Scalar::Float(3.0), &mut o, Little),
            Err(PackError::WrongType)
        );
        assert_eq!(
            store(ElemKind::Char, Scalar::Char(7), &mut o, Little),
            Ok(())
        );
        assert_eq!(o[0], 7);
    }

    #[test]
    fn raw_bits() {
        let mut o = [0u8; 3];
        store_bits(&mut o, 3, 0x0a0b0c, Big);
        assert_eq!(o, [0x0a, 0x0b, 0x0c]);
        assert_eq!(load_bits(&o, 3, Big), 0x0a0b0c);
        assert_eq!(load_bits(&o, 3, Little), 0x0c0b0a);
        store_bits(&mut o, 2, 0xbeef, Little);
        assert_eq!(&o[..2], [0xef, 0xbe]);
    }

    #[test]
    fn f16_helpers() {
        assert_eq!(f16_round(65519.0), 65504.0);
        assert_eq!(f16_round(65520.0), f64::INFINITY);
        assert_eq!(f16_round(-65520.0), f64::NEG_INFINITY);
        assert_eq!(f16_round(2f64.powi(-25) * 1.0000001), 2f64.powi(-24));
        assert!(f16_round(f64::NAN).is_nan());
    }
}
