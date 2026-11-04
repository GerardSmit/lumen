//! Runtime helpers for operations A32 + VFPv3 has no instruction for. Generated code calls
//! them with the AAPCS-VFP convention (`extern "C"` on the hard-float target); they are
//! referenced by absolute address, so code that uses them is not position independent across
//! processes (the JIT never serializes it).
//!
//! Semantics follow [`crate::eval`]: division by zero (undefined in the IR) yields 0 here
//! rather than trapping, `MIN / -1` wraps and `MIN % -1` is 0.

pub extern "C" fn sdiv32(a: i32, b: i32) -> i32 {
    if b == 0 {
        0
    } else {
        a.wrapping_div(b)
    }
}
pub extern "C" fn udiv32(a: u32, b: u32) -> u32 {
    a.checked_div(b).unwrap_or(0)
}
pub extern "C" fn srem32(a: i32, b: i32) -> i32 {
    if b == 0 {
        0
    } else {
        a.wrapping_rem(b)
    }
}
pub extern "C" fn urem32(a: u32, b: u32) -> u32 {
    a.checked_rem(b).unwrap_or(0)
}
pub extern "C" fn sdiv64(a: i64, b: i64) -> i64 {
    if b == 0 {
        0
    } else {
        a.wrapping_div(b)
    }
}
pub extern "C" fn udiv64(a: u64, b: u64) -> u64 {
    a.checked_div(b).unwrap_or(0)
}
pub extern "C" fn srem64(a: i64, b: i64) -> i64 {
    if b == 0 {
        0
    } else {
        a.wrapping_rem(b)
    }
}
pub extern "C" fn urem64(a: u64, b: u64) -> u64 {
    a.checked_rem(b).unwrap_or(0)
}
pub extern "C" fn shl64(a: u64, n: u32) -> u64 {
    a << (n & 63)
}
pub extern "C" fn ushr64(a: u64, n: u32) -> u64 {
    a >> (n & 63)
}
pub extern "C" fn sshr64(a: i64, n: u32) -> i64 {
    a >> (n & 63)
}
pub extern "C" fn rotl64(a: u64, n: u32) -> u64 {
    a.rotate_left(n & 63)
}
pub extern "C" fn rotr64(a: u64, n: u32) -> u64 {
    a.rotate_right(n & 63)
}
pub extern "C" fn i64_to_f64(a: i64) -> f64 {
    a as f64
}
pub extern "C" fn u64_to_f64(a: u64) -> f64 {
    a as f64
}
pub extern "C" fn i64_to_f32(a: i64) -> f32 {
    a as f32
}
pub extern "C" fn u64_to_f32(a: u64) -> f32 {
    a as f32
}
/// Saturating, NaN → 0.
pub extern "C" fn f64_to_i64(a: f64) -> i64 {
    a as i64
}
pub extern "C" fn f64_to_u64(a: f64) -> u64 {
    a as u64
}
/// NaN-propagating minimum with `-0 < +0`.
pub extern "C" fn f64_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a == b {
        if a.is_sign_negative() {
            a
        } else {
            b
        }
    } else if a < b {
        a
    } else {
        b
    }
}
/// NaN-propagating maximum with `-0 < +0`.
pub extern "C" fn f64_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a == b {
        if a.is_sign_positive() {
            a
        } else {
            b
        }
    } else if a > b {
        a
    } else {
        b
    }
}
/// JavaScript `ToInt32`: truncate, reduce modulo 2^32; NaN and infinities give 0.
pub extern "C" fn js_to_int32(a: f64) -> i32 {
    if !a.is_finite() {
        return 0;
    }
    ((a.trunc() % 4294967296.0) as i64) as i32
}

pub fn addr<T>(f: T) -> usize
where
    T: Copy,
{
    // SAFETY: only called with `extern "C" fn` items, which are pointer sized.
    assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<usize>());
    unsafe { std::mem::transmute_copy::<T, usize>(&f) }
}
