//! Numeric tower: int (small and big), float, bool arithmetic and comparison.

use crate::ast::BinOp;
use crate::pyint::{BigInt, PyInt};
use crate::object::*;
use crate::vm::Interp;
use std::cmp::Ordering;

pub enum Num {
    I(i64),
    B(BigInt),
    F(f64),
}

pub fn to_num(v: &Value) -> Option<Num> {
    match v {
        Value::Int(i) => Some(Num::I(*i)),
        Value::Bool(b) => Some(Num::I(*b as i64)),
        Value::Float(f) => Some(Num::F(*f)),
        Value::Obj(o) => match &o.kind {
            Kind::Int(b) => Some(match b.to_i64() {
                Some(i) => Num::I(i),
                None => Num::B(b.clone()),
            }),
            Kind::Float(f) => Some(Num::F(*f)),
            _ => None,
        },
        _ => None,
    }
}

impl Num {
    pub fn is_float(&self) -> bool {
        matches!(self, Num::F(_))
    }

    pub fn big(&self) -> BigInt {
        match self {
            Num::I(i) => BigInt::from_i64(*i),
            Num::B(b) => b.clone(),
            Num::F(f) => BigInt::from_f64_trunc(*f),
        }
    }

    pub fn to_f64(&self) -> Option<f64> {
        match self {
            Num::I(i) => Some(*i as f64),
            Num::B(b) => b.to_float(),
            Num::F(f) => Some(*f),
        }
    }
}

pub fn float_floor_div_mod(x: f64, y: f64) -> (f64, f64) {
    let mut m = x % y;
    let mut d = (x - m) / y;
    if m != 0.0 {
        if (y < 0.0) != (m < 0.0) {
            m += y;
            d -= 1.0;
        }
    } else {
        m = 0.0f64.copysign(y);
    }
    let fd = if d != 0.0 {
        let f = d.floor();
        if d - f > 0.5 {
            f + 1.0
        } else {
            f
        }
    } else {
        0.0f64.copysign(x / y)
    };
    (fd, m)
}

pub fn cmp_int_float(i: &Num, f: f64) -> Option<Ordering> {
    if f.is_nan() {
        return None;
    }
    if f.is_infinite() {
        return Some(if f > 0.0 { Ordering::Less } else { Ordering::Greater });
    }
    if let Num::I(i) = i {
        if i.unsigned_abs() < (1 << 53) {
            return (*i as f64).partial_cmp(&f);
        }
    }
    let ib = i.big();
    let fi = BigInt::from_f64_trunc(f);
    match ib.cmp(&fi) {
        Ordering::Equal => {
            let frac = f - f.trunc();
            if frac > 0.0 {
                Some(Ordering::Less)
            } else if frac < 0.0 {
                Some(Ordering::Greater)
            } else {
                Some(Ordering::Equal)
            }
        }
        o => Some(o),
    }
}

pub fn num_cmp(a: &Num, b: &Num) -> Option<Ordering> {
    match (a, b) {
        (Num::I(x), Num::I(y)) => Some(x.cmp(y)),
        (Num::F(x), Num::F(y)) => x.partial_cmp(y),
        (Num::F(x), i) => cmp_int_float(i, *x).map(|o| o.reverse()),
        (i, Num::F(y)) => cmp_int_float(i, *y),
        (x, y) => Some(x.big().cmp(&y.big())),
    }
}

pub fn big_true_div(a: &BigInt, b: &BigInt) -> Option<f64> {
    if let (Some(x), Some(y)) = (a.to_float(), b.to_float()) {
        if x.abs() < 9007199254740992.0 && y.abs() < 9007199254740992.0 {
            return Some(x / y);
        }
    }
    let shift = (b.bit_len() as i64 - a.bit_len() as i64 + 66).max(0) as usize;
    let q = a.abs().shl(shift as u64).floor_div(&b.abs());
    let qf = q.to_float()?;
    let neg = a.is_negative() != b.is_negative();
    let r = qf * 2f64.powi(-(shift as i32));
    let r = if r == 0.0 && shift > 1000 {
        let s2 = shift as i32 - 1000;
        qf * 2f64.powi(-1000) * 2f64.powi(-s2)
    } else {
        r
    };
    Some(if neg { -r } else { r })
}

pub fn float_repr(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    let sign = if f < 0.0 { "-" } else { "" };
    let s = format!("{:e}", f.abs());
    let (mant, exp) = s.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let n = digits.len() as i32;
    let decpt = exp + 1;
    let body = if (-4..16).contains(&exp) {
        if decpt <= 0 {
            format!("0.{}{}", "0".repeat((-decpt) as usize), digits)
        } else if decpt >= n {
            format!("{}{}.0", digits, "0".repeat((decpt - n) as usize))
        } else {
            format!("{}.{}", &digits[..decpt as usize], &digits[decpt as usize..])
        }
    } else {
        let m = if n > 1 { format!("{}.{}", &digits[..1], &digits[1..]) } else { digits.clone() };
        format!("{}e{}{:02}", m, if exp < 0 { '-' } else { '+' }, exp.abs())
    };
    format!("{}{}", sign, body)
}

fn frexp(x: f64) -> (f64, i32) {
    if x == 0.0 || x.is_nan() || x.is_infinite() {
        return (x, 0);
    }
    let bits = x.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32;
    if exp == 0 {
        let (m, e) = frexp(x * 2f64.powi(64));
        return (m, e - 64);
    }
    let m = f64::from_bits((bits & !(0x7ffu64 << 52)) | (1022u64 << 52));
    (m, exp - 1022)
}

pub fn hash_float(v: f64) -> i64 {
    const MODULUS: u64 = (1 << 61) - 1;
    if v.is_nan() {
        return (v.to_bits() >> 3) as i64 & (i64::MAX >> 1);
    }
    if v.is_infinite() {
        return if v > 0.0 { 314159 } else { -314159 };
    }
    let (mut m, mut e) = frexp(v);
    let sign: i64 = if m < 0.0 {
        m = -m;
        -1
    } else {
        1
    };
    let mut x: u64 = 0;
    while m != 0.0 {
        x = ((x << 28) & MODULUS) | (x >> (61 - 28));
        m *= 268435456.0;
        e -= 28;
        let y = m as u64;
        m -= y as f64;
        x += y;
        if x >= MODULUS {
            x -= MODULUS;
        }
    }
    let e = if e >= 0 { e % 61 } else { 61 - 1 - ((-1 - e) % 61) };
    x = ((x << e) & MODULUS) | (x >> (61 - e));
    let r = (x as i64) * sign;
    if r == -1 {
        -2
    } else {
        r
    }
}

fn mod_inverse(a: &BigInt, m: &BigInt) -> Option<BigInt> {
    let one = BigInt::from_i64(1);
    let (mut old_r, mut r) = (a.floor_mod(m), m.clone());
    let (mut old_s, mut s) = (one.clone(), BigInt::zero());
    while !r.is_zero() {
        let q = old_r.floor_div(&r);
        let nr = old_r.sub(&q.mul(&r));
        old_r = std::mem::replace(&mut r, nr);
        let ns = old_s.sub(&q.mul(&s));
        old_s = std::mem::replace(&mut s, ns);
    }
    if old_r.abs().cmp(&one) != std::cmp::Ordering::Equal {
        return None;
    }
    Some(old_s.floor_mod(m))
}

pub fn hash_int(i: i64) -> i64 {
    const M: u64 = (1 << 61) - 1;
    if i.unsigned_abs() < M {
        if i == -1 {
            -2
        } else {
            i
        }
    } else {
        BigInt::from_i64(i).py_hash()
    }
}

impl Interp {
    pub fn zero_div(&mut self, msg: &str) -> Obj {
        self.new_exc_str("ZeroDivisionError", msg)
    }

    pub fn overflow_err(&mut self, msg: &str) -> Obj {
        self.new_exc_str("OverflowError", msg)
    }

    /// `None` when either operand is not a number.
    pub fn num_binop(&mut self, op: BinOp, a: &Value, b: &Value) -> R<Option<Value>> {
        let (x, y) = match (to_num(a), to_num(b)) {
            (Some(x), Some(y)) => (x, y),
            _ => return Ok(None),
        };
        if let (Value::Bool(p), Value::Bool(q)) = (a, b) {
            match op {
                BinOp::BitAnd => return Ok(Some(Value::Bool(*p & *q))),
                BinOp::BitOr => return Ok(Some(Value::Bool(*p | *q))),
                BinOp::BitXor => return Ok(Some(Value::Bool(*p ^ *q))),
                _ => {}
            }
        }
        if x.is_float() || y.is_float() {
            let (fx, fy) = match (x.to_f64(), y.to_f64()) {
                (Some(p), Some(q)) => (p, q),
                _ => return Err(self.overflow_err("int too large to convert to float")),
            };
            return self.float_op(op, fx, fy).map(Some);
        }
        self.int_op(op, &x, &y).map(Some)
    }

    pub fn float_op(&mut self, op: BinOp, x: f64, y: f64) -> R<Value> {
        Ok(Value::Float(match op {
            BinOp::Add => x + y,
            BinOp::Sub => x - y,
            BinOp::Mult => x * y,
            BinOp::Div => {
                if y == 0.0 {
                    return Err(self.zero_div("division by zero"));
                }
                x / y
            }
            BinOp::FloorDiv => {
                if y == 0.0 {
                    return Err(self.zero_div("division by zero"));
                }
                float_floor_div_mod(x, y).0
            }
            BinOp::Mod => {
                if y == 0.0 {
                    return Err(self.zero_div("division by zero"));
                }
                float_floor_div_mod(x, y).1
            }
            BinOp::Pow => return self.float_pow(x, y),
            _ => return Ok(Value::NotImplemented),
        }))
    }

    pub fn float_pow(&mut self, x: f64, y: f64) -> R<Value> {
        if x == 0.0 && y < 0.0 {
            return Err(self.zero_div("zero to a negative power"));
        }
        if x < 0.0 && y.is_finite() && y != y.floor() {
            let r = (-x).powf(y);
            let ang = std::f64::consts::PI * y;
            return Ok(Value::Obj(Object::new(Kind::Complex(r * ang.cos(), r * ang.sin()))));
        }
        let r = x.powf(y);
        if r.is_infinite() && x.is_finite() && y.is_finite() {
            return Err(self.overflow_err("(34, 'Numerical result out of range')"));
        }
        Ok(Value::Float(r))
    }

    fn int_op(&mut self, op: BinOp, x: &Num, y: &Num) -> R<Value> {
        if let (Num::I(a), Num::I(b)) = (x, y) {
            let (a, b) = (*a, *b);
            match op {
                BinOp::Add => {
                    if let Some(r) = a.checked_add(b) {
                        return Ok(Value::Int(r));
                    }
                }
                BinOp::Sub => {
                    if let Some(r) = a.checked_sub(b) {
                        return Ok(Value::Int(r));
                    }
                }
                BinOp::Mult => {
                    if let Some(r) = a.checked_mul(b) {
                        return Ok(Value::Int(r));
                    }
                }
                BinOp::FloorDiv => {
                    if b == 0 {
                        return Err(self.zero_div("division by zero"));
                    }
                    if !(a == i64::MIN && b == -1) {
                        let mut q = a / b;
                        if a % b != 0 && ((a < 0) != (b < 0)) {
                            q -= 1;
                        }
                        return Ok(Value::Int(q));
                    }
                }
                BinOp::Mod => {
                    if b == 0 {
                        return Err(self.zero_div("division by zero"));
                    }
                    if b == -1 {
                        return Ok(Value::Int(0));
                    }
                    let mut r = a % b;
                    if r != 0 && ((r < 0) != (b < 0)) {
                        r += b;
                    }
                    return Ok(Value::Int(r));
                }
                BinOp::Div => {
                    if b == 0 {
                        return Err(self.zero_div("division by zero"));
                    }
                    if a.unsigned_abs() < (1 << 53) && b.unsigned_abs() < (1 << 53) {
                        return Ok(Value::Float(a as f64 / b as f64));
                    }
                }
                BinOp::BitAnd => return Ok(Value::Int(a & b)),
                BinOp::BitOr => return Ok(Value::Int(a | b)),
                BinOp::BitXor => return Ok(Value::Int(a ^ b)),
                BinOp::LShift => {
                    if b < 0 {
                        return Err(self.value_error("negative shift count"));
                    }
                    if a == 0 {
                        return Ok(Value::Int(0));
                    }
                    if b < 62 {
                        let r = a << b;
                        if (r >> b) == a {
                            return Ok(Value::Int(r));
                        }
                    }
                }
                BinOp::RShift => {
                    if b < 0 {
                        return Err(self.value_error("negative shift count"));
                    }
                    return Ok(Value::Int(if b >= 64 { if a < 0 { -1 } else { 0 } } else { a >> b }));
                }
                BinOp::Pow => {
                    if b < 0 {
                        if a == 0 {
                            return Err(self.zero_div("zero to a negative power"));
                        }
                        return self.float_pow(a as f64, b as f64);
                    }
                    if let Some(r) = a.checked_pow(b.min(u32::MAX as i64) as u32) {
                        if b <= u32::MAX as i64 {
                            return Ok(Value::Int(r));
                        }
                    }
                }
                _ => return Ok(Value::NotImplemented),
            }
        }
        let a = x.big();
        let b = y.big();
        Ok(match op {
            BinOp::Add => Value::big(a.add(&b)),
            BinOp::Sub => Value::big(a.sub(&b)),
            BinOp::Mult => Value::big(a.mul(&b)),
            BinOp::FloorDiv => {
                if b.is_zero() {
                    return Err(self.zero_div("division by zero"));
                }
                Value::big(a.floor_div(&b))
            }
            BinOp::Mod => {
                if b.is_zero() {
                    return Err(self.zero_div("division by zero"));
                }
                Value::big(a.floor_mod(&b))
            }
            BinOp::Div => {
                if b.is_zero() {
                    return Err(self.zero_div("division by zero"));
                }
                match big_true_div(&a, &b) {
                    Some(f) => Value::Float(f),
                    None => return Err(self.overflow_err("integer division result too large for a float")),
                }
            }
            BinOp::BitAnd => Value::big(a.bitand(&b)),
            BinOp::BitOr => Value::big(a.bitor(&b)),
            BinOp::BitXor => Value::big(a.bitxor(&b)),
            BinOp::LShift => {
                if b.is_negative() {
                    return Err(self.value_error("negative shift count"));
                }
                if a.is_zero() {
                    return Ok(Value::Int(0));
                }
                match b.to_u64() {
                    Some(n) if n < (1 << 32) => Value::big(a.shl(n)),
                    _ => return Err(self.new_exc_str("OverflowError", "too many digits in integer")),
                }
            }
            BinOp::RShift => {
                if b.is_negative() {
                    return Err(self.value_error("negative shift count"));
                }
                match b.to_u64() {
                    Some(n) if n < (1 << 32) => Value::big(a.shr(n)),
                    _ => Value::Int(if a.is_negative() { -1 } else { 0 }),
                }
            }
            BinOp::Pow => {
                if b.is_negative() {
                    let fa = a.to_float();
                    let fb = b.to_float();
                    return match (fa, fb) {
                        (Some(p), Some(q)) => self.float_pow(p, q),
                        _ => Err(self.overflow_err("int too large to convert to float")),
                    };
                }
                match b.to_u64() {
                    Some(n) => match a.pow(&BigInt::from_u64(n)) {
                        Ok(v) => Value::big(v),
                        Err(_) => return Err(self.new_exc_str("MemoryError", "")),
                    },
                    None => {
                        if a.is_zero() || a == BigInt::from_i64(1) {
                            Value::big(a)
                        } else {
                            return Err(self.new_exc_str("MemoryError", ""));
                        }
                    }
                }
            }
            _ => Value::NotImplemented,
        })
    }

    pub fn int_pow_mod(&mut self, a: &BigInt, e: &BigInt, m: &BigInt) -> R<Value> {
        if m.is_zero() {
            return Err(self.value_error("pow() 3rd argument cannot be 0"));
        }
        if e.is_negative() {
            let Some(inv) = mod_inverse(a, m) else {
                return Err(self.value_error("base is not invertible for the given modulus"));
            };
            return Ok(Value::big(inv.pow_mod(&e.neg(), m).expect("modulus and exponent checked")));
        }
        Ok(Value::big(a.pow_mod(e, m).expect("modulus and exponent checked")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_repr_matches_cpython() {
        assert_eq!(float_repr(0.1), "0.1");
        assert_eq!(float_repr(1.0), "1.0");
        assert_eq!(float_repr(1e16), "1e+16");
        assert_eq!(float_repr(1e-5), "1e-05");
        assert_eq!(float_repr(123456789012345680.0), "1.2345678901234568e+17");
        assert_eq!(float_repr(-0.0), "-0.0");
        assert_eq!(float_repr(f64::INFINITY), "inf");
        assert_eq!(float_repr(0.0001), "0.0001");
    }

    #[test]
    fn numeric_hashes_agree_across_types() {
        assert_eq!(hash_float(3.0), hash_int(3));
        assert_eq!(hash_float(-1.0), -2);
        assert_eq!(hash_int(-1), -2);
        assert_eq!(hash_float(f64::INFINITY), 314159);
    }
}
