//! Fixed-width SIMD (`v128`, opcode prefix 0xFD): the opcode table that validation and the label
//! scan share, and the interpreter's vector operations. A vector is a `u128` whose lane 0 is the
//! least significant lane; the native tier does not translate vector code, so functions using it
//! run interpreted.

use super::exec::{
    Store, Val, pop, read_uleb, round_ties_even_f32, round_ties_even_f64, sat_i32, sat_u32,
    wasm_max_f32, wasm_max_f64, wasm_min_f32, wasm_min_f64,
};
use super::parse::ValType;

/// The sub-opcode of `v128.const`, the one vector instruction allowed in constant expressions.
pub const V128_CONST: u32 = 12;

#[derive(Clone, Copy)]
pub enum Imm {
    None,
    /// A memarg whose alignment exponent may be at most `align`.
    Mem {
        align: u32,
    },
    /// A memarg followed by a lane index below `lanes`.
    MemLane {
        align: u32,
        lanes: u8,
    },
    Lane(u8),
    Const,
    Shuffle,
}

/// A vector instruction's immediates and stack effect (operands listed bottom to top).
pub struct Info {
    pub imm: Imm,
    pub pops: &'static [ValType],
    pub push: Option<ValType>,
}

/// The shape of sub-opcode `op`, or `None` when it is not a (final-proposal) SIMD instruction.
pub fn info(op: u32) -> Option<Info> {
    use ValType::{F32, F64, I32, I64, V128};
    let mk = |imm: Imm, pops: &'static [ValType], push: Option<ValType>| Info { imm, pops, push };
    Some(match op {
        0x00 => mk(Imm::Mem { align: 4 }, &[I32], Some(V128)),
        0x01..=0x06 | 0x0a | 0x5d => mk(Imm::Mem { align: 3 }, &[I32], Some(V128)),
        0x07 => mk(Imm::Mem { align: 0 }, &[I32], Some(V128)),
        0x08 => mk(Imm::Mem { align: 1 }, &[I32], Some(V128)),
        0x09 | 0x5c => mk(Imm::Mem { align: 2 }, &[I32], Some(V128)),
        0x0b => mk(Imm::Mem { align: 4 }, &[I32, V128], None),
        0x0c => mk(Imm::Const, &[], Some(V128)),
        0x0d => mk(Imm::Shuffle, &[V128, V128], Some(V128)),
        0x0f..=0x11 => mk(Imm::None, &[I32], Some(V128)),
        0x12 => mk(Imm::None, &[I64], Some(V128)),
        0x13 => mk(Imm::None, &[F32], Some(V128)),
        0x14 => mk(Imm::None, &[F64], Some(V128)),
        0x15 | 0x16 => mk(Imm::Lane(16), &[V128], Some(I32)),
        0x17 => mk(Imm::Lane(16), &[V128, I32], Some(V128)),
        0x18 | 0x19 => mk(Imm::Lane(8), &[V128], Some(I32)),
        0x1a => mk(Imm::Lane(8), &[V128, I32], Some(V128)),
        0x1b => mk(Imm::Lane(4), &[V128], Some(I32)),
        0x1c => mk(Imm::Lane(4), &[V128, I32], Some(V128)),
        0x1d => mk(Imm::Lane(2), &[V128], Some(I64)),
        0x1e => mk(Imm::Lane(2), &[V128, I64], Some(V128)),
        0x1f => mk(Imm::Lane(4), &[V128], Some(F32)),
        0x20 => mk(Imm::Lane(4), &[V128, F32], Some(V128)),
        0x21 => mk(Imm::Lane(2), &[V128], Some(F64)),
        0x22 => mk(Imm::Lane(2), &[V128, F64], Some(V128)),
        0x52 => mk(Imm::None, &[V128, V128, V128], Some(V128)),
        0x54..=0x57 => mk(
            Imm::MemLane {
                align: op - 0x54,
                lanes: (16u32 >> (op - 0x54)) as u8,
            },
            &[I32, V128],
            Some(V128),
        ),
        0x58..=0x5b => mk(
            Imm::MemLane {
                align: op - 0x58,
                lanes: (16u32 >> (op - 0x58)) as u8,
            },
            &[I32, V128],
            None,
        ),
        0x53 | 0x63 | 0x64 | 0x83 | 0x84 | 0xa3 | 0xa4 | 0xc3 | 0xc4 => {
            mk(Imm::None, &[V128], Some(I32))
        }
        0x6b..=0x6d | 0x8b..=0x8d | 0xab..=0xad | 0xcb..=0xcd => {
            mk(Imm::None, &[V128, I32], Some(V128))
        }
        0x4d
        | 0x5e
        | 0x5f
        | 0x60..=0x62
        | 0x67..=0x6a
        | 0x74
        | 0x75
        | 0x7a
        | 0x7c..=0x7f
        | 0x80
        | 0x81
        | 0x87..=0x8a
        | 0x94
        | 0xa0
        | 0xa1
        | 0xa7..=0xaa
        | 0xc0
        | 0xc1
        | 0xc7..=0xca
        | 0xe0
        | 0xe1
        | 0xe3
        | 0xec
        | 0xed
        | 0xef
        | 0xf8..=0xff => mk(Imm::None, &[V128], Some(V128)),
        0x0e
        | 0x23..=0x51
        | 0x65
        | 0x66
        | 0x6e..=0x73
        | 0x76..=0x79
        | 0x7b
        | 0x82
        | 0x85
        | 0x86
        | 0x8e..=0x93
        | 0x95..=0x99
        | 0x9b..=0x9f
        | 0xae
        | 0xb1
        | 0xb5..=0xba
        | 0xbc..=0xbf
        | 0xce
        | 0xd1
        | 0xd5..=0xdf
        | 0xe4..=0xeb
        | 0xf0..=0xf7 => mk(Imm::None, &[V128, V128], Some(V128)),
        _ => return None,
    })
}

/// Advance `ip` past a 0xFD instruction's sub-opcode and immediates (`ip` is just after 0xFD).
pub fn skip_immediates(code: &[u8], ip: &mut usize) -> Result<(), String> {
    let op = read_uleb(code, ip)? as u32;
    let info = info(op).ok_or("wasm: unknown vector opcode")?;
    let fixed = match info.imm {
        Imm::None => 0,
        Imm::Mem { .. } => {
            read_uleb(code, ip)?;
            read_uleb(code, ip)?;
            0
        }
        Imm::MemLane { .. } => {
            read_uleb(code, ip)?;
            read_uleb(code, ip)?;
            1
        }
        Imm::Lane(_) => 1,
        Imm::Const | Imm::Shuffle => 16,
    };
    *ip = ip.checked_add(fixed).ok_or("wasm: truncated immediate")?;
    if *ip > code.len() {
        return Err("wasm: truncated immediate".into());
    }
    Ok(())
}

// ---- lanes ------------------------------------------------------------------------------------

trait Lane: Copy {
    const BYTES: usize;
    fn read(b: &[u8]) -> Self;
    fn write(self, b: &mut [u8]);
}

macro_rules! lane_impl {
    ($($t:ty),*) => {$(
        impl Lane for $t {
            const BYTES: usize = std::mem::size_of::<$t>();
            fn read(b: &[u8]) -> Self {
                <$t>::from_le_bytes(b[..Self::BYTES].try_into().unwrap())
            }
            fn write(self, b: &mut [u8]) {
                b[..Self::BYTES].copy_from_slice(&self.to_le_bytes());
            }
        }
    )*};
}
lane_impl!(u8, i8, u16, i16, u32, i32, u64, i64, f32, f64);

fn unpack<T: Lane, const N: usize>(v: u128) -> [T; N] {
    let b = v.to_le_bytes();
    std::array::from_fn(|i| T::read(&b[i * T::BYTES..]))
}

fn pack<T: Lane, const N: usize>(lanes: [T; N]) -> u128 {
    let mut b = [0u8; 16];
    for (i, x) in lanes.into_iter().enumerate() {
        x.write(&mut b[i * T::BYTES..]);
    }
    u128::from_le_bytes(b)
}

fn un<T: Lane, U: Lane, const N: usize>(a: u128, f: impl Fn(T) -> U) -> u128 {
    let a = unpack::<T, N>(a);
    pack::<U, N>(std::array::from_fn(|i| f(a[i])))
}

fn bin<T: Lane, U: Lane, const N: usize>(a: u128, b: u128, f: impl Fn(T, T) -> U) -> u128 {
    let (a, b) = (unpack::<T, N>(a), unpack::<T, N>(b));
    pack::<U, N>(std::array::from_fn(|i| f(a[i], b[i])))
}

/// A lane-wise comparison producing all-ones or all-zeros lanes of the integer type `$m`.
macro_rules! cmp {
    ($a:expr, $b:expr, $t:ty, $m:ty, $n:literal, $f:expr) => {{
        let f = $f;
        bin::<$t, $m, $n>($a, $b, move |x, y| if f(x, y) { !0 } else { 0 })
    }};
}

/// The ten integer comparisons in binary order: eq ne lt_s lt_u gt_s gt_u le_s le_u ge_s ge_u.
macro_rules! icmp {
    ($k:expr, $a:expr, $b:expr, $s:ty, $u:ty, $n:literal) => {
        match $k {
            0 => cmp!($a, $b, $s, $s, $n, |x, y| x == y),
            1 => cmp!($a, $b, $s, $s, $n, |x, y| x != y),
            2 => cmp!($a, $b, $s, $s, $n, |x, y| x < y),
            3 => cmp!($a, $b, $u, $s, $n, |x, y| x < y),
            4 => cmp!($a, $b, $s, $s, $n, |x, y| x > y),
            5 => cmp!($a, $b, $u, $s, $n, |x, y| x > y),
            6 => cmp!($a, $b, $s, $s, $n, |x, y| x <= y),
            7 => cmp!($a, $b, $u, $s, $n, |x, y| x <= y),
            8 => cmp!($a, $b, $s, $s, $n, |x, y| x >= y),
            _ => cmp!($a, $b, $u, $s, $n, |x, y| x >= y),
        }
    };
}

/// The six float comparisons: eq ne lt gt le ge.
macro_rules! fcmp {
    ($k:expr, $a:expr, $b:expr, $t:ty, $m:ty, $n:literal) => {
        match $k {
            0 => cmp!($a, $b, $t, $m, $n, |x, y| x == y),
            1 => cmp!($a, $b, $t, $m, $n, |x, y| x != y),
            2 => cmp!($a, $b, $t, $m, $n, |x, y| x < y),
            3 => cmp!($a, $b, $t, $m, $n, |x, y| x > y),
            4 => cmp!($a, $b, $t, $m, $n, |x, y| x <= y),
            _ => cmp!($a, $b, $t, $m, $n, |x, y| x >= y),
        }
    };
}

/// Lane-wise `extmul`: the low or high half of each input widened, then multiplied.
macro_rules! extmul {
    ($a:expr, $b:expr, $narrow:ty, $wide:ty, $n_in:literal, $n_out:literal, $high:expr) => {{
        let (x, y) = (unpack::<$narrow, $n_in>($a), unpack::<$narrow, $n_in>($b));
        let off = if $high { $n_out } else { 0 };
        pack::<$wide, $n_out>(std::array::from_fn(|i| {
            (x[i + off] as $wide).wrapping_mul(y[i + off] as $wide)
        }))
    }};
}

/// Lane-wise `extadd_pairwise`.
macro_rules! extadd {
    ($a:expr, $narrow:ty, $wide:ty, $n_in:literal, $n_out:literal) => {{
        let x = unpack::<$narrow, $n_in>($a);
        pack::<$wide, $n_out>(std::array::from_fn(|i| {
            (x[2 * i] as $wide) + (x[2 * i + 1] as $wide)
        }))
    }};
}

/// Lane-wise `extend_low`/`extend_high`.
macro_rules! extend {
    ($a:expr, $narrow:ty, $wide:ty, $n_in:literal, $n_out:literal, $high:expr) => {{
        let x = unpack::<$narrow, $n_in>($a);
        let off = if $high { $n_out } else { 0 };
        pack::<$wide, $n_out>(std::array::from_fn(|i| x[i + off] as $wide))
    }};
}

fn unary(op: u32, a: u128) -> Option<u128> {
    Some(match op {
        0x4d => !a,
        0x5e => {
            let x = unpack::<f64, 2>(a);
            pack::<f32, 4>([x[0] as f32, x[1] as f32, 0.0, 0.0])
        }
        0x5f => {
            let x = unpack::<f32, 4>(a);
            pack::<f64, 2>([x[0] as f64, x[1] as f64])
        }
        0x60 => un::<i8, i8, 16>(a, |x| x.wrapping_abs()),
        0x61 => un::<i8, i8, 16>(a, |x| x.wrapping_neg()),
        0x62 => un::<u8, u8, 16>(a, |x| x.count_ones() as u8),
        0x67 => un::<f32, f32, 4>(a, f32::ceil),
        0x68 => un::<f32, f32, 4>(a, f32::floor),
        0x69 => un::<f32, f32, 4>(a, f32::trunc),
        0x6a => un::<f32, f32, 4>(a, round_ties_even_f32),
        0x74 => un::<f64, f64, 2>(a, f64::ceil),
        0x75 => un::<f64, f64, 2>(a, f64::floor),
        0x7a => un::<f64, f64, 2>(a, f64::trunc),
        0x94 => un::<f64, f64, 2>(a, round_ties_even_f64),
        0x7c => extadd!(a, i8, i16, 16, 8),
        0x7d => extadd!(a, u8, u16, 16, 8),
        0x7e => extadd!(a, i16, i32, 8, 4),
        0x7f => extadd!(a, u16, u32, 8, 4),
        0x80 => un::<i16, i16, 8>(a, |x| x.wrapping_abs()),
        0x81 => un::<i16, i16, 8>(a, |x| x.wrapping_neg()),
        0x87 => extend!(a, i8, i16, 16, 8, false),
        0x88 => extend!(a, i8, i16, 16, 8, true),
        0x89 => extend!(a, u8, u16, 16, 8, false),
        0x8a => extend!(a, u8, u16, 16, 8, true),
        0xa0 => un::<i32, i32, 4>(a, |x| x.wrapping_abs()),
        0xa1 => un::<i32, i32, 4>(a, |x| x.wrapping_neg()),
        0xa7 => extend!(a, i16, i32, 8, 4, false),
        0xa8 => extend!(a, i16, i32, 8, 4, true),
        0xa9 => extend!(a, u16, u32, 8, 4, false),
        0xaa => extend!(a, u16, u32, 8, 4, true),
        0xc0 => un::<i64, i64, 2>(a, |x| x.wrapping_abs()),
        0xc1 => un::<i64, i64, 2>(a, |x| x.wrapping_neg()),
        0xc7 => extend!(a, i32, i64, 4, 2, false),
        0xc8 => extend!(a, i32, i64, 4, 2, true),
        0xc9 => extend!(a, u32, u64, 4, 2, false),
        0xca => extend!(a, u32, u64, 4, 2, true),
        0xe0 => un::<f32, f32, 4>(a, f32::abs),
        0xe1 => un::<f32, f32, 4>(a, |x| -x),
        0xe3 => un::<f32, f32, 4>(a, f32::sqrt),
        0xec => un::<f64, f64, 2>(a, f64::abs),
        0xed => un::<f64, f64, 2>(a, |x| -x),
        0xef => un::<f64, f64, 2>(a, f64::sqrt),
        0xf8 => un::<f32, i32, 4>(a, |x| sat_i32(x as f64)),
        0xf9 => un::<f32, u32, 4>(a, |x| sat_u32(x as f64)),
        0xfa => un::<i32, f32, 4>(a, |x| x as f32),
        0xfb => un::<u32, f32, 4>(a, |x| x as f32),
        0xfc => {
            let x = unpack::<f64, 2>(a);
            pack::<i32, 4>([sat_i32(x[0]), sat_i32(x[1]), 0, 0])
        }
        0xfd => {
            let x = unpack::<f64, 2>(a);
            pack::<u32, 4>([sat_u32(x[0]), sat_u32(x[1]), 0, 0])
        }
        0xfe => {
            let x = unpack::<i32, 4>(a);
            pack::<f64, 2>([x[0] as f64, x[1] as f64])
        }
        0xff => {
            let x = unpack::<u32, 4>(a);
            pack::<f64, 2>([x[0] as f64, x[1] as f64])
        }
        _ => return None,
    })
}

fn pmin<T: PartialOrd>(a: T, b: T) -> T {
    if b < a { b } else { a }
}

fn pmax<T: PartialOrd>(a: T, b: T) -> T {
    if a < b { b } else { a }
}

fn avgr_u8(a: u8, b: u8) -> u8 {
    ((a as u16 + b as u16 + 1) >> 1) as u8
}

fn avgr_u16(a: u16, b: u16) -> u16 {
    ((a as u32 + b as u32 + 1) >> 1) as u16
}

fn q15mulr_sat(a: i16, b: i16) -> i16 {
    ((a as i32 * b as i32 + 0x4000) >> 15).clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

fn narrow<S: Lane + Into<i32>, D: Lane, const N: usize, const M: usize>(
    a: u128,
    b: u128,
    min: i32,
    max: i32,
    make: impl Fn(i32) -> D,
) -> u128 {
    let (x, y) = (unpack::<S, N>(a), unpack::<S, N>(b));
    pack::<D, M>(std::array::from_fn(|i| {
        let v: i32 = (if i < N { x[i] } else { y[i - N] }).into();
        make(v.clamp(min, max))
    }))
}

fn dot(a: u128, b: u128) -> u128 {
    let (x, y) = (unpack::<i16, 8>(a), unpack::<i16, 8>(b));
    pack::<i32, 4>(std::array::from_fn(|i| {
        (x[2 * i] as i32 * y[2 * i] as i32).wrapping_add(x[2 * i + 1] as i32 * y[2 * i + 1] as i32)
    }))
}

fn swizzle(a: u128, idx: u128) -> u128 {
    let (x, k) = (a.to_le_bytes(), idx.to_le_bytes());
    u128::from_le_bytes(std::array::from_fn(|i| {
        x.get(k[i] as usize).copied().unwrap_or(0)
    }))
}

fn binary(op: u32, a: u128, b: u128) -> Option<u128> {
    Some(match op {
        0x0e => swizzle(a, b),
        0x23..=0x2c => icmp!(op - 0x23, a, b, i8, u8, 16),
        0x2d..=0x36 => icmp!(op - 0x2d, a, b, i16, u16, 8),
        0x37..=0x40 => icmp!(op - 0x37, a, b, i32, u32, 4),
        0x41..=0x46 => fcmp!(op - 0x41, a, b, f32, i32, 4),
        0x47..=0x4c => fcmp!(op - 0x47, a, b, f64, i64, 2),
        0x4e => a & b,
        0x4f => a & !b,
        0x50 => a | b,
        0x51 => a ^ b,
        0x65 => narrow::<i16, i8, 8, 16>(a, b, -128, 127, |v| v as i8),
        0x66 => narrow::<i16, u8, 8, 16>(a, b, 0, 255, |v| v as u8),
        0x6e => bin::<u8, u8, 16>(a, b, u8::wrapping_add),
        0x6f => bin::<i8, i8, 16>(a, b, i8::saturating_add),
        0x70 => bin::<u8, u8, 16>(a, b, u8::saturating_add),
        0x71 => bin::<u8, u8, 16>(a, b, u8::wrapping_sub),
        0x72 => bin::<i8, i8, 16>(a, b, i8::saturating_sub),
        0x73 => bin::<u8, u8, 16>(a, b, u8::saturating_sub),
        0x76 => bin::<i8, i8, 16>(a, b, i8::min),
        0x77 => bin::<u8, u8, 16>(a, b, u8::min),
        0x78 => bin::<i8, i8, 16>(a, b, i8::max),
        0x79 => bin::<u8, u8, 16>(a, b, u8::max),
        0x7b => bin::<u8, u8, 16>(a, b, avgr_u8),
        0x82 => bin::<i16, i16, 8>(a, b, q15mulr_sat),
        0x85 => narrow::<i32, i16, 4, 8>(a, b, -32768, 32767, |v| v as i16),
        0x86 => narrow::<i32, u16, 4, 8>(a, b, 0, 65535, |v| v as u16),
        0x8e => bin::<u16, u16, 8>(a, b, u16::wrapping_add),
        0x8f => bin::<i16, i16, 8>(a, b, i16::saturating_add),
        0x90 => bin::<u16, u16, 8>(a, b, u16::saturating_add),
        0x91 => bin::<u16, u16, 8>(a, b, u16::wrapping_sub),
        0x92 => bin::<i16, i16, 8>(a, b, i16::saturating_sub),
        0x93 => bin::<u16, u16, 8>(a, b, u16::saturating_sub),
        0x95 => bin::<u16, u16, 8>(a, b, u16::wrapping_mul),
        0x96 => bin::<i16, i16, 8>(a, b, i16::min),
        0x97 => bin::<u16, u16, 8>(a, b, u16::min),
        0x98 => bin::<i16, i16, 8>(a, b, i16::max),
        0x99 => bin::<u16, u16, 8>(a, b, u16::max),
        0x9b => bin::<u16, u16, 8>(a, b, avgr_u16),
        0x9c => extmul!(a, b, i8, i16, 16, 8, false),
        0x9d => extmul!(a, b, i8, i16, 16, 8, true),
        0x9e => extmul!(a, b, u8, u16, 16, 8, false),
        0x9f => extmul!(a, b, u8, u16, 16, 8, true),
        0xae => bin::<u32, u32, 4>(a, b, u32::wrapping_add),
        0xb1 => bin::<u32, u32, 4>(a, b, u32::wrapping_sub),
        0xb5 => bin::<u32, u32, 4>(a, b, u32::wrapping_mul),
        0xb6 => bin::<i32, i32, 4>(a, b, i32::min),
        0xb7 => bin::<u32, u32, 4>(a, b, u32::min),
        0xb8 => bin::<i32, i32, 4>(a, b, i32::max),
        0xb9 => bin::<u32, u32, 4>(a, b, u32::max),
        0xba => dot(a, b),
        0xbc => extmul!(a, b, i16, i32, 8, 4, false),
        0xbd => extmul!(a, b, i16, i32, 8, 4, true),
        0xbe => extmul!(a, b, u16, u32, 8, 4, false),
        0xbf => extmul!(a, b, u16, u32, 8, 4, true),
        0xce => bin::<u64, u64, 2>(a, b, u64::wrapping_add),
        0xd1 => bin::<u64, u64, 2>(a, b, u64::wrapping_sub),
        0xd5 => bin::<u64, u64, 2>(a, b, u64::wrapping_mul),
        0xd6 => cmp!(a, b, i64, i64, 2, |x, y| x == y),
        0xd7 => cmp!(a, b, i64, i64, 2, |x, y| x != y),
        0xd8 => cmp!(a, b, i64, i64, 2, |x, y| x < y),
        0xd9 => cmp!(a, b, i64, i64, 2, |x, y| x > y),
        0xda => cmp!(a, b, i64, i64, 2, |x, y| x <= y),
        0xdb => cmp!(a, b, i64, i64, 2, |x, y| x >= y),
        0xdc => extmul!(a, b, i32, i64, 4, 2, false),
        0xdd => extmul!(a, b, i32, i64, 4, 2, true),
        0xde => extmul!(a, b, u32, u64, 4, 2, false),
        0xdf => extmul!(a, b, u32, u64, 4, 2, true),
        0xe4 => bin::<f32, f32, 4>(a, b, |x, y| x + y),
        0xe5 => bin::<f32, f32, 4>(a, b, |x, y| x - y),
        0xe6 => bin::<f32, f32, 4>(a, b, |x, y| x * y),
        0xe7 => bin::<f32, f32, 4>(a, b, |x, y| x / y),
        0xe8 => bin::<f32, f32, 4>(a, b, wasm_min_f32),
        0xe9 => bin::<f32, f32, 4>(a, b, wasm_max_f32),
        0xea => bin::<f32, f32, 4>(a, b, pmin),
        0xeb => bin::<f32, f32, 4>(a, b, pmax),
        0xf0 => bin::<f64, f64, 2>(a, b, |x, y| x + y),
        0xf1 => bin::<f64, f64, 2>(a, b, |x, y| x - y),
        0xf2 => bin::<f64, f64, 2>(a, b, |x, y| x * y),
        0xf3 => bin::<f64, f64, 2>(a, b, |x, y| x / y),
        0xf4 => bin::<f64, f64, 2>(a, b, wasm_min_f64),
        0xf5 => bin::<f64, f64, 2>(a, b, wasm_max_f64),
        0xf6 => bin::<f64, f64, 2>(a, b, pmin),
        0xf7 => bin::<f64, f64, 2>(a, b, pmax),
        _ => return None,
    })
}

/// `shl`, `shr_s` and `shr_u`: the count is taken modulo the lane width.
fn shift(op: u32, a: u128, n: u32) -> Option<u128> {
    Some(match op {
        0x6b => un::<u8, u8, 16>(a, |x| x << (n & 7)),
        0x6c => un::<i8, i8, 16>(a, |x| x >> (n & 7)),
        0x6d => un::<u8, u8, 16>(a, |x| x >> (n & 7)),
        0x8b => un::<u16, u16, 8>(a, |x| x << (n & 15)),
        0x8c => un::<i16, i16, 8>(a, |x| x >> (n & 15)),
        0x8d => un::<u16, u16, 8>(a, |x| x >> (n & 15)),
        0xab => un::<u32, u32, 4>(a, |x| x << (n & 31)),
        0xac => un::<i32, i32, 4>(a, |x| x >> (n & 31)),
        0xad => un::<u32, u32, 4>(a, |x| x >> (n & 31)),
        0xcb => un::<u64, u64, 2>(a, |x| x << (n & 63)),
        0xcc => un::<i64, i64, 2>(a, |x| x >> (n & 63)),
        0xcd => un::<u64, u64, 2>(a, |x| x >> (n & 63)),
        _ => return None,
    })
}

/// `all_true`, `bitmask` and `any_true`, which yield an `i32`.
fn reduce(op: u32, a: u128) -> Option<i32> {
    fn all<T: Lane + PartialEq + Default, const N: usize>(a: u128) -> i32 {
        unpack::<T, N>(a).iter().all(|&x| x != T::default()) as i32
    }
    fn mask<T: Lane + Copy, const N: usize>(a: u128, negative: impl Fn(T) -> bool) -> i32 {
        unpack::<T, N>(a)
            .iter()
            .enumerate()
            .fold(0, |m, (i, &x)| m | (negative(x) as i32) << i)
    }
    Some(match op {
        0x53 => (a != 0) as i32,
        0x63 => all::<u8, 16>(a),
        0x83 => all::<u16, 8>(a),
        0xa3 => all::<u32, 4>(a),
        0xc3 => all::<u64, 2>(a),
        0x64 => mask::<i8, 16>(a, |x| x < 0),
        0x84 => mask::<i16, 8>(a, |x| x < 0),
        0xa4 => mask::<i32, 4>(a, |x| x < 0),
        0xc4 => mask::<i64, 2>(a, |x| x < 0),
        _ => return None,
    })
}

fn lane_bits(a: u128, lane: usize, size: usize) -> u64 {
    let b = a.to_le_bytes();
    let mut out = [0u8; 8];
    out[..size].copy_from_slice(&b[lane * size..(lane + 1) * size]);
    u64::from_le_bytes(out)
}

fn set_lane(a: u128, lane: usize, size: usize, bits: u64) -> u128 {
    let mut b = a.to_le_bytes();
    b[lane * size..(lane + 1) * size].copy_from_slice(&bits.to_le_bytes()[..size]);
    u128::from_le_bytes(b)
}

fn pop_v(stack: &mut Vec<Val>) -> Result<u128, String> {
    Ok(pop(stack)?.v128())
}

fn push_v(stack: &mut Vec<Val>, v: u128) {
    stack.push(Val::from_v128(v));
}

const BAD: &str = "wasm: unsupported vector opcode";

fn read_byte(code: &[u8], ip: &mut usize) -> Result<u8, String> {
    let b = *code.get(*ip).ok_or("wasm: truncated immediate")?;
    *ip += 1;
    Ok(b)
}

impl Store {
    /// Execute a 0xFD instruction; `ip` is just after the prefix byte.
    pub(super) fn op_simd(
        &mut self,
        mem_addr: Option<usize>,
        code: &[u8],
        ip: &mut usize,
        stack: &mut Vec<Val>,
    ) -> Result<(), String> {
        let op = read_uleb(code, ip)? as u32;
        let info = info(op).ok_or(BAD)?;
        match info.imm {
            Imm::Mem { .. } | Imm::MemLane { .. } => {
                return self.simd_mem(mem_addr.ok_or("wasm: no memory")?, op, code, ip, stack);
            }
            Imm::Const => {
                let end = *ip + 16;
                let bytes = code.get(*ip..end).ok_or("wasm: truncated immediate")?;
                *ip = end;
                push_v(stack, u128::from_le_bytes(bytes.try_into().unwrap()));
                return Ok(());
            }
            Imm::Shuffle => {
                let end = *ip + 16;
                let sel = code.get(*ip..end).ok_or("wasm: truncated immediate")?;
                *ip = end;
                let b = pop_v(stack)?.to_le_bytes();
                let a = pop_v(stack)?.to_le_bytes();
                let out = std::array::from_fn(|i| {
                    let k = sel[i] as usize;
                    if k < 16 {
                        a[k]
                    } else {
                        b.get(k - 16).copied().unwrap_or(0)
                    }
                });
                push_v(stack, u128::from_le_bytes(out));
                return Ok(());
            }
            Imm::Lane(lanes) => {
                let lane = read_byte(code, ip)? as usize;
                if lane >= lanes as usize {
                    return Err("wasm: lane index out of range".into());
                }
                return lane_op(op, lanes as usize, lane, stack);
            }
            Imm::None => {}
        }
        match (info.pops, info.push) {
            ([ValType::V128], Some(ValType::V128)) => {
                let a = pop_v(stack)?;
                push_v(stack, unary(op, a).ok_or(BAD)?);
            }
            ([ValType::V128, ValType::V128], Some(ValType::V128)) => {
                let b = pop_v(stack)?;
                let a = pop_v(stack)?;
                push_v(stack, binary(op, a, b).ok_or(BAD)?);
            }
            ([ValType::V128, ValType::I32], Some(ValType::V128)) => {
                let n = pop(stack)?.i32() as u32;
                let a = pop_v(stack)?;
                push_v(stack, shift(op, a, n).ok_or(BAD)?);
            }
            ([ValType::V128, ValType::V128, ValType::V128], _) => {
                let mask = pop_v(stack)?;
                let b = pop_v(stack)?;
                let a = pop_v(stack)?;
                push_v(stack, a & mask | b & !mask);
            }
            ([ValType::V128], Some(ValType::I32)) => {
                let a = pop_v(stack)?;
                stack.push(Val::I32(reduce(op, a).ok_or(BAD)?));
            }
            ([_], Some(ValType::V128)) => {
                let v = pop(stack)?;
                let splat = match op {
                    0x0f => pack::<u8, 16>([v.i32() as u8; 16]),
                    0x10 => pack::<u16, 8>([v.i32() as u16; 8]),
                    0x11 => pack::<u32, 4>([v.i32() as u32; 4]),
                    0x12 => pack::<u64, 2>([v.i64() as u64; 2]),
                    0x13 => pack::<f32, 4>([v.f32(); 4]),
                    0x14 => pack::<f64, 2>([v.f64(); 2]),
                    _ => return Err(BAD.into()),
                };
                push_v(stack, splat);
            }
            _ => return Err(BAD.into()),
        }
        Ok(())
    }

    fn simd_mem(
        &mut self,
        mem_addr: usize,
        op: u32,
        code: &[u8],
        ip: &mut usize,
        stack: &mut Vec<Val>,
    ) -> Result<(), String> {
        let mem = &mut self
            .memories
            .get_mut(mem_addr)
            .ok_or("wasm: no memory")?
            .bytes;
        let mem_len = mem.len();
        let access = |size: usize, ip: &mut usize, stack: &mut Vec<Val>| {
            Store::mem_effective_addr(mem_len, code, ip, stack, size)
        };
        match op {
            0x00 | 0x01..=0x06 | 0x07..=0x0a | 0x5c | 0x5d => {
                let size = match op {
                    0x00 => 16,
                    0x01..=0x06 | 0x0a | 0x5d => 8,
                    0x07 => 1,
                    0x08 => 2,
                    _ => 4,
                };
                let at = access(size, ip, stack)?;
                let mut raw = [0u8; 16];
                raw[..size].copy_from_slice(&mem[at..at + size]);
                let loaded = u128::from_le_bytes(raw);
                let v = match op {
                    0x01 => unary(0x87, loaded),
                    0x02 => unary(0x89, loaded),
                    0x03 => unary(0xa7, loaded),
                    0x04 => unary(0xa9, loaded),
                    0x05 => unary(0xc7, loaded),
                    0x06 => unary(0xc9, loaded),
                    0x07 => Some(pack::<u8, 16>([loaded as u8; 16])),
                    0x08 => Some(pack::<u16, 8>([loaded as u16; 8])),
                    0x09 => Some(pack::<u32, 4>([loaded as u32; 4])),
                    0x0a => Some(pack::<u64, 2>([loaded as u64; 2])),
                    _ => Some(loaded),
                };
                push_v(stack, v.ok_or(BAD)?);
            }
            0x0b => {
                let v = pop_v(stack)?;
                let at = access(16, ip, stack)?;
                mem[at..at + 16].copy_from_slice(&v.to_le_bytes());
            }
            0x54..=0x5b => {
                let (size, lanes) = match (op - 0x54) % 4 {
                    0 => (1, 16),
                    1 => (2, 8),
                    2 => (4, 4),
                    _ => (8, 2),
                };
                let v = pop_v(stack)?;
                let at = access(size, ip, stack)?;
                let lane = read_byte(code, ip)? as usize;
                if lane >= lanes {
                    return Err("wasm: lane index out of range".into());
                }
                if op < 0x58 {
                    let mut raw = [0u8; 8];
                    raw[..size].copy_from_slice(&mem[at..at + size]);
                    push_v(stack, set_lane(v, lane, size, u64::from_le_bytes(raw)));
                } else {
                    mem[at..at + size]
                        .copy_from_slice(&lane_bits(v, lane, size).to_le_bytes()[..size]);
                }
            }
            _ => return Err(BAD.into()),
        }
        Ok(())
    }
}

fn lane_op(op: u32, lanes: usize, lane: usize, stack: &mut Vec<Val>) -> Result<(), String> {
    let size = 16 / lanes;
    match op {
        0x15 | 0x16 | 0x18 | 0x19 | 0x1b | 0x1d | 0x1f | 0x21 => {
            let bits = lane_bits(pop_v(stack)?, lane, size);
            stack.push(match op {
                0x15 => Val::I32(bits as u8 as i8 as i32),
                0x16 => Val::I32(bits as u8 as i32),
                0x18 => Val::I32(bits as u16 as i16 as i32),
                0x19 => Val::I32(bits as u16 as i32),
                0x1b => Val::I32(bits as u32 as i32),
                0x1d => Val::I64(bits as i64),
                0x1f => Val::F32(f32::from_bits(bits as u32)),
                _ => Val::F64(f64::from_bits(bits)),
            });
        }
        _ => {
            let bits = pop(stack)?.to_bits();
            let a = pop_v(stack)?;
            push_v(stack, set_lane(a, lane, size, bits));
        }
    }
    Ok(())
}
