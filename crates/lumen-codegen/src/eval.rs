//! Reference semantics of the pure operations, shared by the IR interpreter and constant folding.
//!
//! Values are raw bits in a `u64`: I32 and F32 occupy the low 32 bits with the upper bits zero.
//! `None` means the operation is undefined for these operands (see the opcode docs).

use crate::ir::*;

#[inline]
pub fn norm(ty: Type, bits: u64) -> u64 {
    match ty {
        Type::I32 | Type::F32 => bits & 0xffff_ffff,
        Type::I64 | Type::F64 => bits,
    }
}

pub fn iconst(ty: Type, imm: i64) -> u64 {
    norm(ty, imm as u64)
}

fn f32of(b: u64) -> f32 {
    f32::from_bits(b as u32)
}
fn f64of(b: u64) -> f64 {
    f64::from_bits(b)
}
fn bf32(v: f32) -> u64 {
    v.to_bits() as u64
}
fn bf64(v: f64) -> u64 {
    v.to_bits()
}

pub fn unary(op: UnaryOp, ty: Type, a: u64) -> u64 {
    use UnaryOp::*;
    match (ty, op) {
        (Type::I32, _) => {
            let x = a as u32;
            (match op {
                Clz => x.leading_zeros(),
                Ctz => x.trailing_zeros(),
                Popcnt => x.count_ones(),
                Eqz => (x == 0) as u32,
                Sext8 => x as i8 as i32 as u32,
                Sext16 => x as i16 as i32 as u32,
                _ => unreachable!("{op:?} on I32"),
            }) as u64
        }
        (Type::I64, _) => match op {
            Clz => a.leading_zeros() as u64,
            Ctz => a.trailing_zeros() as u64,
            Popcnt => a.count_ones() as u64,
            Eqz => (a == 0) as u64,
            Sext8 => a as i8 as i64 as u64,
            Sext16 => a as i16 as i64 as u64,
            Sext32 => a as i32 as i64 as u64,
            _ => unreachable!("{op:?} on I64"),
        },
        (Type::F32, _) => {
            let x = f32of(a);
            match op {
                Fneg => a ^ 0x8000_0000,
                Fabs => a & 0x7fff_ffff,
                Sqrt => bf32(x.sqrt()),
                Ceil => bf32(x.ceil()),
                Floor => bf32(x.floor()),
                Trunc => bf32(x.trunc()),
                Nearest => bf32(x.round_ties_even()),
                _ => unreachable!("{op:?} on F32"),
            }
        }
        (Type::F64, _) => {
            let x = f64of(a);
            match op {
                Fneg => a ^ (1 << 63),
                Fabs => a & !(1 << 63),
                Sqrt => bf64(x.sqrt()),
                Ceil => bf64(x.ceil()),
                Floor => bf64(x.floor()),
                Trunc => bf64(x.trunc()),
                Nearest => bf64(x.round_ties_even()),
                _ => unreachable!("{op:?} on F64"),
            }
        }
    }
}

macro_rules! fminmax {
    ($x:expr, $y:expr, $min:expr) => {{
        let (x, y) = ($x, $y);
        if x.is_nan() || y.is_nan() {
            x + y
        } else if x == y {
            // Only the sign of zero can differ.
            if $min == x.is_sign_negative() { x } else { y }
        } else if (x < y) == $min {
            x
        } else {
            y
        }
    }};
}

pub fn binary(op: BinaryOp, ty: Type, a: u64, b: u64) -> Option<u64> {
    use BinaryOp::*;
    Some(match ty {
        Type::I32 => {
            let (x, y) = (a as u32, b as u32);
            let (sx, sy) = (x as i32, y as i32);
            (match op {
                Iadd => x.wrapping_add(y),
                Isub => x.wrapping_sub(y),
                Imul => x.wrapping_mul(y),
                Sdiv => {
                    if y == 0 || (sx == i32::MIN && sy == -1) {
                        return None;
                    }
                    (sx / sy) as u32
                }
                Udiv => x.checked_div(y)?,
                Srem => {
                    if y == 0 {
                        return None;
                    }
                    sx.wrapping_rem(sy) as u32
                }
                Urem => x.checked_rem(y)?,
                Band => x & y,
                Bor => x | y,
                Bxor => x ^ y,
                Ishl => x.wrapping_shl(y),
                Ushr => x.wrapping_shr(y),
                Sshr => sx.wrapping_shr(y) as u32,
                Rotl => x.rotate_left(y % 32),
                Rotr => x.rotate_right(y % 32),
                _ => unreachable!("{op:?} on I32"),
            }) as u64
        }
        Type::I64 => {
            let (sx, sy) = (a as i64, b as i64);
            match op {
                Iadd => a.wrapping_add(b),
                Isub => a.wrapping_sub(b),
                Imul => a.wrapping_mul(b),
                Sdiv => {
                    if b == 0 || (sx == i64::MIN && sy == -1) {
                        return None;
                    }
                    (sx / sy) as u64
                }
                Udiv => a.checked_div(b)?,
                Srem => {
                    if b == 0 {
                        return None;
                    }
                    sx.wrapping_rem(sy) as u64
                }
                Urem => a.checked_rem(b)?,
                Band => a & b,
                Bor => a | b,
                Bxor => a ^ b,
                Ishl => a.wrapping_shl(b as u32),
                Ushr => a.wrapping_shr(b as u32),
                Sshr => sx.wrapping_shr(b as u32) as u64,
                Rotl => a.rotate_left((b % 64) as u32),
                Rotr => a.rotate_right((b % 64) as u32),
                _ => unreachable!("{op:?} on I64"),
            }
        }
        Type::F32 => {
            let (x, y) = (f32of(a), f32of(b));
            match op {
                Fadd => bf32(x + y),
                Fsub => bf32(x - y),
                Fmul => bf32(x * y),
                Fdiv => bf32(x / y),
                Fmin => bf32(fminmax!(x, y, true)),
                Fmax => bf32(fminmax!(x, y, false)),
                Fcopysign => (a & 0x7fff_ffff) | (b & 0x8000_0000),
                _ => unreachable!("{op:?} on F32"),
            }
        }
        Type::F64 => {
            let (x, y) = (f64of(a), f64of(b));
            match op {
                Fadd => bf64(x + y),
                Fsub => bf64(x - y),
                Fmul => bf64(x * y),
                Fdiv => bf64(x / y),
                Fmin => bf64(fminmax!(x, y, true)),
                Fmax => bf64(fminmax!(x, y, false)),
                Fcopysign => (a & !(1 << 63)) | (b & (1 << 63)),
                _ => unreachable!("{op:?} on F64"),
            }
        }
    })
}

pub fn icmp(cc: IntCC, ty: Type, a: u64, b: u64) -> u64 {
    use IntCC::*;
    let (sa, sb) = match ty {
        Type::I32 => (a as u32 as i32 as i64, b as u32 as i32 as i64),
        _ => (a as i64, b as i64),
    };
    (match cc {
        Eq => a == b,
        Ne => a != b,
        Slt => sa < sb,
        Sle => sa <= sb,
        Sgt => sa > sb,
        Sge => sa >= sb,
        Ult => a < b,
        Ule => a <= b,
        Ugt => a > b,
        Uge => a >= b,
    }) as u64
}

pub fn fcmp(cc: FloatCC, ty: Type, a: u64, b: u64) -> u64 {
    let (x, y) = match ty {
        Type::F32 => (f32of(a) as f64, f32of(b) as f64),
        _ => (f64of(a), f64of(b)),
    };
    (match cc {
        FloatCC::Eq => x == y,
        FloatCC::Ne => x != y,
        FloatCC::Lt => x < y,
        FloatCC::Le => x <= y,
        FloatCC::Gt => x > y,
        FloatCC::Ge => x >= y,
    }) as u64
}

pub fn convert(op: ConvOp, from: Type, to: Type, a: u64) -> Option<u64> {
    use ConvOp::*;
    let fl = |a: u64| match from {
        Type::F32 => f32of(a) as f64,
        _ => f64of(a),
    };
    Some(match op {
        Wrap => a & 0xffff_ffff,
        Sext => a as u32 as i32 as i64 as u64,
        Uext => a & 0xffff_ffff,
        FromSint | FromUint => {
            let signed = op == FromSint;
            match (from, to) {
                (Type::I32, Type::F32) if signed => bf32(a as u32 as i32 as f32),
                (Type::I32, Type::F32) => bf32(a as u32 as f32),
                (Type::I32, Type::F64) if signed => bf64(a as u32 as i32 as f64),
                (Type::I32, Type::F64) => bf64(a as u32 as f64),
                (Type::I64, Type::F32) if signed => bf32(a as i64 as f32),
                (Type::I64, Type::F32) => bf32(a as f32),
                (Type::I64, Type::F64) if signed => bf64(a as i64 as f64),
                (Type::I64, Type::F64) => bf64(a as f64),
                _ => unreachable!(),
            }
        }
        ToSint | ToUint | ToSintSat | ToUintSat => {
            let x = fl(a);
            let signed = matches!(op, ToSint | ToSintSat);
            let sat = matches!(op, ToSintSat | ToUintSat);
            let t = x.trunc();
            let in_range = !x.is_nan()
                && match (to, signed) {
                    (Type::I32, true) => (-2147483648.0..=2147483647.0).contains(&t),
                    (Type::I32, false) => t > -1.0 && t <= 4294967295.0,
                    (Type::I64, true) => (-9223372036854775808.0..9223372036854775808.0).contains(&t),
                    (Type::I64, false) => t > -1.0 && t < 18446744073709551616.0,
                    _ => unreachable!(),
                };
            if !in_range && !sat {
                return None;
            }
            // `as` saturates and maps NaN to 0, which is exactly the saturating semantics.
            match (to, signed) {
                (Type::I32, true) => x as i32 as u32 as u64,
                (Type::I32, false) => x as u32 as u64,
                (Type::I64, true) => x as i64 as u64,
                (Type::I64, false) => x as u64,
                _ => unreachable!(),
            }
        }
        Promote => bf64(f32of(a) as f64),
        Demote => bf32(f64of(a) as f32),
        Bitcast => a,
    })
}

/// Evaluate a pure instruction over constant operands; `None` if undefined.
pub fn pure_inst(func: &Function, data: &InstData, arg: impl Fn(Value) -> u64) -> Option<u64> {
    let ty = |v: Value| func.value_type(v);
    Some(match data {
        InstData::Iconst { ty, imm } => iconst(*ty, *imm),
        InstData::F32const { bits } => *bits as u64,
        InstData::F64const { bits } => *bits,
        InstData::Unary { op, arg: a } => unary(*op, ty(*a), arg(*a)),
        InstData::Binary { op, args } => binary(*op, ty(args[0]), arg(args[0]), arg(args[1]))?,
        InstData::IntCmp { cc, args } => icmp(*cc, ty(args[0]), arg(args[0]), arg(args[1])),
        InstData::FloatCmp { cc, args } => fcmp(*cc, ty(args[0]), arg(args[0]), arg(args[1])),
        InstData::Select {
            cond,
            if_true,
            if_false,
        } => {
            if arg(*cond) as u32 != 0 {
                arg(*if_true)
            } else {
                arg(*if_false)
            }
        }
        InstData::Convert { op, to, arg: a } => convert(*op, ty(*a), *to, arg(*a))?,
        _ => return None,
    })
}
