//! Arbitrary-precision BigInt, from scratch: sign + little-endian u64 magnitude behind an `Rc`
//! (clones are cheap; every operation allocates one fresh value).
//!
//! The magnitude kernels are sized for crypto-style code (`(a * b) % m` loops over 2048-bit
//! values): schoolbook multiply with a dedicated squaring path, Karatsuba above
//! [`KARATSUBA_LIMBS`], Knuth algorithm D for multi-limb division (remainder-only when the
//! quotient is not wanted), a hardware 128/64 divide for the single-limb steps, and chunked
//! radix conversion (one short division per `radix^k` chunk instead of per digit).

use std::cmp::Ordering;
use std::rc::Rc;

#[derive(Clone, Debug)]
pub struct JsBigInt(Rc<BigIntData>);

#[derive(Debug)]
struct BigIntData {
    /// True for negative values. Zero is always non-negative with an empty magnitude.
    neg: bool,
    /// Little-endian base-2^64 digits, no trailing zero limbs.
    mag: Vec<u64>,
}

/// Operand size (limbs, of the shorter factor) from which multiplication switches to Karatsuba.
const KARATSUBA_LIMBS: usize = 32;
/// The same threshold for squaring (the schoolbook square does half the products, so it wins
/// for longer).
const KARATSUBA_SQR_LIMBS: usize = 48;

fn trim(mut mag: Vec<u64>) -> Vec<u64> {
    while mag.last() == Some(&0) {
        mag.pop();
    }
    mag
}

/// `a` without its high zero limbs.
fn trimmed(a: &[u64]) -> &[u64] {
    let mut n = a.len();
    while n > 0 && a[n - 1] == 0 {
        n -= 1;
    }
    &a[..n]
}

fn mag_cmp(a: &[u64], b: &[u64]) -> Ordering {
    if a.len() != b.len() {
        return a.len().cmp(&b.len());
    }
    for i in (0..a.len()).rev() {
        match a[i].cmp(&b[i]) {
            Ordering::Equal => {}
            o => return o,
        }
    }
    Ordering::Equal
}

fn mag_add(a: &[u64], b: &[u64]) -> Vec<u64> {
    let (a, b) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    let mut out = Vec::with_capacity(a.len() + 1);
    out.extend_from_slice(a);
    let carry = add_into(&mut out, b);
    if carry {
        out.push(1);
    }
    out
}

/// `acc += b` over `acc.len()` limbs (`acc.len() >= b.len()`); returns the carry out of the top.
fn add_into(acc: &mut [u64], b: &[u64]) -> bool {
    let mut carry = false;
    let (lo, hi) = acc.split_at_mut(b.len());
    for (x, &y) in lo.iter_mut().zip(b) {
        let (s, c1) = x.overflowing_add(y);
        let (s, c2) = s.overflowing_add(carry as u64);
        *x = s;
        carry = c1 | c2;
    }
    if carry {
        for x in hi {
            let (s, c) = x.overflowing_add(1);
            *x = s;
            if !c {
                return false;
            }
        }
        return true;
    }
    false
}

/// `acc -= b` in place for `acc >= b`.
fn sub_into(acc: &mut [u64], b: &[u64]) {
    let mut borrow = false;
    let (lo, hi) = acc.split_at_mut(b.len());
    for (x, &y) in lo.iter_mut().zip(b) {
        let (d, b1) = x.overflowing_sub(y);
        let (d, b2) = d.overflowing_sub(borrow as u64);
        *x = d;
        borrow = b1 | b2;
    }
    if borrow {
        for x in hi {
            let (d, b) = x.overflowing_sub(1);
            *x = d;
            if !b {
                break;
            }
        }
    }
}

/// `a - b` for `a >= b`.
fn mag_sub(a: &[u64], b: &[u64]) -> Vec<u64> {
    let mut out = a.to_vec();
    sub_into(&mut out, b);
    trim(out)
}

/// `out[..b.len()] += x * b`, returning the carry limb (to be stored at `out[b.len()]`).
#[inline]
fn mac_row(out: &mut [u64], b: &[u64], x: u64) -> u64 {
    let mut carry = 0u64;
    for (o, &y) in out.iter_mut().zip(b) {
        let t = *o as u128 + x as u128 * y as u128 + carry as u128;
        *o = t as u64;
        carry = (t >> 64) as u64;
    }
    carry
}

/// Schoolbook product into a zeroed `out` of exactly `a.len() + b.len()` limbs.
fn mul_school(a: &[u64], b: &[u64], out: &mut [u64]) {
    for (i, &x) in a.iter().enumerate() {
        if x != 0 {
            out[i + b.len()] = mac_row(&mut out[i..], b, x);
        }
    }
}

/// Schoolbook square into a zeroed `out` of exactly `2 * a.len()` limbs: the off-diagonal
/// products once, doubled by a one-bit shift, plus the diagonal squares.
fn sqr_school(a: &[u64], out: &mut [u64]) {
    let n = a.len();
    for i in 0..n {
        let x = a[i];
        if x != 0 && i + 1 < n {
            out[i + n] = mac_row(&mut out[2 * i + 1..], &a[i + 1..], x);
        }
    }
    let mut top = 0u64;
    for o in out.iter_mut() {
        let v = *o;
        *o = (v << 1) | top;
        top = v >> 63;
    }
    let mut carry = 0u64;
    for i in 0..n {
        let sq = a[i] as u128 * a[i] as u128;
        let t = out[2 * i] as u128 + (sq as u64) as u128 + carry as u128;
        out[2 * i] = t as u64;
        let t = out[2 * i + 1] as u128 + (sq >> 64) + (t >> 64);
        out[2 * i + 1] = t as u64;
        carry = (t >> 64) as u64;
    }
}

/// Product of two magnitudes (any lengths, high zero limbs allowed), trimmed.
fn mag_mul(a: &[u64], b: &[u64]) -> Vec<u64> {
    let (a, b) = (trimmed(a), trimmed(b));
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    let mut out = vec![0u64; a.len() + b.len()];
    mul_into(a, b, &mut out);
    trim(out)
}

/// `out = a * b` for a zeroed `out` of exactly `a.len() + b.len()` limbs.
fn mul_into(a: &[u64], b: &[u64], out: &mut [u64]) {
    let (a, b) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    if b.len() < KARATSUBA_LIMBS {
        mul_school(a, b, out);
    } else if a.len() >= 2 * b.len() {
        // Unbalanced: multiply `b`-sized slices of `a` and accumulate.
        let mut tmp = vec![0u64; 2 * b.len()];
        let mut i = 0;
        while i < a.len() {
            let chunk = &a[i..(i + b.len()).min(a.len())];
            let t = &mut tmp[..chunk.len() + b.len()];
            t.fill(0);
            mul_into(chunk, b, t);
            add_into(&mut out[i..], trimmed(t));
            i += b.len();
        }
    } else {
        // Balanced Karatsuba: a = a1·B^m + a0, b = b1·B^m + b0 (b1 non-empty since b > a/2 ≥ m).
        let m = a.len() / 2;
        let (a0, a1) = (trimmed(&a[..m]), &a[m..]);
        let (b0, b1) = (trimmed(&b[..m]), &b[m..]);
        let z0 = mag_mul(a0, b0);
        let z2 = mag_mul(a1, b1);
        let mut z1 = mag_mul(&mag_add(a0, a1), &mag_add(b0, b1));
        sub_into(&mut z1, &z0);
        sub_into(&mut z1, &z2);
        add_into(out, &z0);
        add_into(&mut out[m..], trimmed(&z1));
        add_into(&mut out[2 * m..], &z2);
    }
}

/// `a * a`, trimmed.
fn mag_sqr(a: &[u64]) -> Vec<u64> {
    let a = trimmed(a);
    if a.is_empty() {
        return Vec::new();
    }
    let mut out = vec![0u64; 2 * a.len()];
    if a.len() < KARATSUBA_SQR_LIMBS {
        sqr_school(a, &mut out);
    } else {
        let m = a.len() / 2;
        let (a0, a1) = (trimmed(&a[..m]), &a[m..]);
        let z0 = mag_sqr(a0);
        let z2 = mag_sqr(a1);
        let mut z1 = mag_sqr(&mag_add(a0, a1));
        sub_into(&mut z1, &z0);
        sub_into(&mut z1, &z2);
        add_into(&mut out, &z0);
        add_into(&mut out[m..], trimmed(&z1));
        add_into(&mut out[2 * m..], &z2);
    }
    trim(out)
}

/// `(hi·2^64 + lo) / d` and its remainder, for `hi < d` (so the quotient fits a limb).
#[inline]
fn div2by1(hi: u64, lo: u64, d: u64) -> (u64, u64) {
    debug_assert!(hi < d);
    #[cfg(target_arch = "x86_64")]
    {
        let (q, r): (u64, u64);
        // SAFETY: `hi < d` guarantees the quotient fits in RAX, so `div` cannot fault.
        unsafe {
            core::arch::asm!(
                "div {d}",
                d = in(reg) d,
                inout("rax") lo => q,
                inout("rdx") hi => r,
                options(pure, nomem, nostack)
            );
        }
        (q, r)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let n = ((hi as u128) << 64) | lo as u128;
        ((n / d as u128) as u64, (n % d as u128) as u64)
    }
}

/// In-place short division of `a` by the limb `d` (non-zero); returns the remainder. `a` is left
/// untrimmed.
fn div_small_in_place(a: &mut [u64], d: u64) -> u64 {
    let mut rem = 0u64;
    for x in a.iter_mut().rev() {
        let (q, r) = div2by1(rem, *x, d);
        *x = q;
        rem = r;
    }
    rem
}

/// `a << s` for `s < 64`, written into a new vector of `a.len() + extra` limbs.
fn shl_bits(a: &[u64], s: u32, extra: usize) -> Vec<u64> {
    let mut out = Vec::with_capacity(a.len() + extra);
    if s == 0 {
        out.extend_from_slice(a);
        out.resize(a.len() + extra, 0);
        return out;
    }
    let mut carry = 0u64;
    for &x in a {
        out.push((x << s) | carry);
        carry = x >> (64 - s);
    }
    if extra > 0 {
        out.push(carry);
        out.resize(a.len() + extra, 0);
    }
    out
}

/// `(quotient, remainder)` of `a / b` with `b` non-zero (both trimmed). The quotient is only
/// computed when `want_q` (otherwise it comes back empty).
fn mag_divmod(a: &[u64], b: &[u64], want_q: bool) -> (Vec<u64>, Vec<u64>) {
    if mag_cmp(a, b) == Ordering::Less {
        return (Vec::new(), a.to_vec());
    }
    if b.len() == 1 {
        let mut q = a.to_vec();
        let r = div_small_in_place(&mut q, b[0]);
        let q = if want_q { trim(q) } else { Vec::new() };
        return (q, if r == 0 { Vec::new() } else { vec![r] });
    }
    // Knuth, TAOCP vol. 2, 4.3.1 algorithm D, on base-2^64 digits.
    let n = b.len();
    let m = a.len() - n;
    let s = b[n - 1].leading_zeros();
    let v = shl_bits(b, s, 0);
    let mut u = shl_bits(a, s, 1);
    let mut q = if want_q { vec![0u64; m + 1] } else { Vec::new() };
    let (vtop, vnext) = (v[n - 1], v[n - 2]);
    for j in (0..=m).rev() {
        let (u2, u1, u0) = (u[j + n], u[j + n - 1], u[j + n - 2]);
        // Estimate q̂ from the top two limbs and refine it against the third (at most 2 off → 0/1).
        let (mut qhat, mut rhat, mut refine) = if u2 >= vtop {
            let (r, of) = u1.overflowing_add(vtop);
            (u64::MAX, r, !of)
        } else {
            let (q, r) = div2by1(u2, u1, vtop);
            (q, r, true)
        };
        while refine && qhat as u128 * vnext as u128 > ((rhat as u128) << 64 | u0 as u128) {
            qhat -= 1;
            let (r, of) = rhat.overflowing_add(vtop);
            rhat = r;
            refine = !of;
        }
        // u[j..=j+n] -= q̂ · v
        let mut mul_carry = 0u64;
        let mut borrow = false;
        for i in 0..n {
            let p = qhat as u128 * v[i] as u128 + mul_carry as u128;
            mul_carry = (p >> 64) as u64;
            let (d, b1) = u[j + i].overflowing_sub(p as u64);
            let (d, b2) = d.overflowing_sub(borrow as u64);
            u[j + i] = d;
            borrow = b1 | b2;
        }
        let (d, b1) = u[j + n].overflowing_sub(mul_carry);
        let (d, b2) = d.overflowing_sub(borrow as u64);
        u[j + n] = d;
        if b1 | b2 {
            // q̂ was one too large: add v back.
            qhat -= 1;
            let c = add_into(&mut u[j..j + n], &v);
            u[j + n] = u[j + n].wrapping_add(c as u64);
        }
        if want_q {
            q[j] = qhat;
        }
    }
    // Remainder: the low `n` limbs of `u`, shifted back down by `s`.
    u.truncate(n);
    if s != 0 {
        for i in 0..n {
            let hi = if i + 1 < n { u[i + 1] << (64 - s) } else { 0 };
            u[i] = (u[i] >> s) | hi;
        }
    }
    (if want_q { trim(q) } else { Vec::new() }, trim(u))
}

/// `acc = acc * m + c` in place; returns the carry limb out of the top.
fn mac_small(acc: &mut [u64], m: u64, c: u64) -> u64 {
    let mut carry = c;
    for x in acc.iter_mut() {
        let t = *x as u128 * m as u128 + carry as u128;
        *x = t as u64;
        carry = (t >> 64) as u64;
    }
    carry
}

/// Largest power of `radix` fitting in a limb, and its exponent (digits per chunk).
fn radix_chunk(radix: u32) -> (u64, usize) {
    let (mut p, mut k) = (radix as u64, 1usize);
    while let Some(n) = p.checked_mul(radix as u64) {
        p = n;
        k += 1;
    }
    (p, k)
}

impl JsBigInt {
    pub fn zero() -> Self {
        JsBigInt(Rc::new(BigIntData {
            neg: false,
            mag: Vec::new(),
        }))
    }
    fn make(neg: bool, mag: Vec<u64>) -> Self {
        let mag = trim(mag);
        JsBigInt(Rc::new(BigIntData {
            neg: neg && !mag.is_empty(),
            mag,
        }))
    }
    pub fn from_i128(v: i128) -> Self {
        let neg = v < 0;
        let u = v.unsigned_abs();
        Self::make(neg, vec![u as u64, (u >> 64) as u64])
    }
    pub fn from_u64(v: u64) -> Self {
        Self::make(false, vec![v])
    }
    /// The value as an i128, if it fits.
    pub fn to_i128(&self) -> Option<i128> {
        if self.0.mag.len() > 2 {
            return None;
        }
        let lo = *self.0.mag.first().unwrap_or(&0) as u128;
        let hi = *self.0.mag.get(1).unwrap_or(&0) as u128;
        let u = (hi << 64) | lo;
        if self.0.neg {
            if u > 1u128 << 127 {
                return None;
            }
            Some((u as i128).wrapping_neg())
        } else {
            if u >= 1u128 << 127 {
                return None;
            }
            Some(u as i128)
        }
    }
    /// The low 128 bits, two's-complement wrapped (for fixed-width storage like BigInt64Array).
    pub fn to_i128_wrapping(&self) -> i128 {
        let lo = *self.0.mag.first().unwrap_or(&0) as u128;
        let hi = *self.0.mag.get(1).unwrap_or(&0) as u128;
        let u = (hi << 64) | lo;
        let v = u as i128;
        if self.0.neg {
            v.wrapping_neg()
        } else {
            v
        }
    }
    pub fn is_zero(&self) -> bool {
        self.0.mag.is_empty()
    }
    pub fn is_negative(&self) -> bool {
        self.0.neg
    }
    pub fn bit_len(&self) -> usize {
        match self.0.mag.last() {
            None => 0,
            Some(&top) => (self.0.mag.len() - 1) * 64 + (64 - top.leading_zeros() as usize),
        }
    }

    pub fn neg(&self) -> Self {
        Self::make(!self.0.neg, self.0.mag.clone())
    }
    pub fn add(&self, o: &Self) -> Self {
        if self.0.neg == o.0.neg {
            Self::make(self.0.neg, mag_add(&self.0.mag, &o.0.mag))
        } else {
            match mag_cmp(&self.0.mag, &o.0.mag) {
                Ordering::Equal => Self::zero(),
                Ordering::Greater => Self::make(self.0.neg, mag_sub(&self.0.mag, &o.0.mag)),
                Ordering::Less => Self::make(o.0.neg, mag_sub(&o.0.mag, &self.0.mag)),
            }
        }
    }
    pub fn sub(&self, o: &Self) -> Self {
        if self.0.neg != o.0.neg {
            Self::make(self.0.neg, mag_add(&self.0.mag, &o.0.mag))
        } else {
            match mag_cmp(&self.0.mag, &o.0.mag) {
                Ordering::Equal => Self::zero(),
                Ordering::Greater => Self::make(self.0.neg, mag_sub(&self.0.mag, &o.0.mag)),
                Ordering::Less => Self::make(!self.0.neg, mag_sub(&o.0.mag, &self.0.mag)),
            }
        }
    }
    pub fn mul(&self, o: &Self) -> Self {
        let mag = if Rc::ptr_eq(&self.0, &o.0) || self.0.mag == o.0.mag {
            mag_sqr(&self.0.mag)
        } else {
            mag_mul(&self.0.mag, &o.0.mag)
        };
        Self::make(self.0.neg != o.0.neg, mag)
    }
    /// Truncating division; `None` on division by zero.
    pub fn div(&self, o: &Self) -> Option<Self> {
        if o.is_zero() {
            return None;
        }
        let (q, _) = mag_divmod(&self.0.mag, &o.0.mag, true);
        Some(Self::make(self.0.neg != o.0.neg, q))
    }
    /// Remainder with the dividend's sign; `None` on division by zero.
    pub fn rem(&self, o: &Self) -> Option<Self> {
        if o.is_zero() {
            return None;
        }
        let (_, r) = mag_divmod(&self.0.mag, &o.0.mag, false);
        Some(Self::make(self.0.neg, r))
    }
    /// Exponentiation; `None` for a negative exponent.
    pub fn pow(&self, o: &Self) -> Option<Self> {
        if o.0.neg {
            return None;
        }
        let mut e = o.to_i128().unwrap_or(i128::MAX) as u128;
        let mut base = self.clone();
        let mut acc = Self::from_u64(1);
        while e > 0 {
            if e & 1 == 1 {
                acc = acc.mul(&base);
            }
            e >>= 1;
            if e > 0 {
                base = base.mul(&base);
            }
        }
        Some(acc)
    }

    pub fn shl(&self, count: u64) -> Self {
        if self.is_zero() {
            return Self::zero();
        }
        let limbs = (count / 64) as usize;
        let bits = (count % 64) as u32;
        let mut mag = vec![0u64; limbs];
        let mut carry = 0u64;
        for &d in self.0.mag.iter() {
            if bits == 0 {
                mag.push(d);
            } else {
                mag.push((d << bits) | carry);
                carry = d >> (64 - bits);
            }
        }
        if carry != 0 {
            mag.push(carry);
        }
        Self::make(self.0.neg, mag)
    }
    /// Arithmetic right shift (floors toward negative infinity).
    pub fn shr(&self, count: u64) -> Self {
        let limbs = (count / 64) as usize;
        let bits = (count % 64) as u32;
        if limbs >= self.0.mag.len() {
            return if self.0.neg {
                Self::from_i128(-1)
            } else {
                Self::zero()
            };
        }
        let mut mag: Vec<u64> = self.0.mag[limbs..].to_vec();
        let mut lost = self.0.mag[..limbs].iter().any(|&d| d != 0);
        if bits != 0 {
            let mut prev = 0u64;
            if mag.first().map(|d| d & ((1 << bits) - 1) != 0) == Some(true) {
                lost = true;
            }
            for d in mag.iter_mut().rev() {
                let cur = *d;
                *d = (cur >> bits) | (prev << (64 - bits));
                prev = cur;
            }
        }
        let out = Self::make(self.0.neg, mag);
        // Negative values floor: any lost bit rounds away from zero.
        if self.0.neg && lost {
            out.sub(&Self::from_u64(1))
        } else {
            out
        }
    }

    pub fn bitand(&self, o: &Self) -> Self {
        self.bitop(o, |a, b| a & b)
    }
    pub fn bitor(&self, o: &Self) -> Self {
        self.bitop(o, |a, b| a | b)
    }
    pub fn bitxor(&self, o: &Self) -> Self {
        self.bitop(o, |a, b| a ^ b)
    }
    pub fn not(&self) -> Self {
        // ~x == -x - 1
        self.neg().sub(&Self::from_u64(1))
    }
    /// Bitwise op over two's-complement representations of arbitrary width.
    fn bitop(&self, o: &Self, f: fn(u64, u64) -> u64) -> Self {
        if !self.0.neg && !o.0.neg {
            // Both non-negative: the magnitudes are the two's-complement forms (zero-extended).
            let (a, b) = (&self.0.mag, &o.0.mag);
            let out: Vec<u64> = (0..a.len().max(b.len()))
                .map(|i| f(*a.get(i).unwrap_or(&0), *b.get(i).unwrap_or(&0)))
                .collect();
            return Self::make(false, out);
        }
        let n = self.0.mag.len().max(o.0.mag.len()) + 1;
        let a = self.twos(n);
        let b = o.twos(n);
        let out: Vec<u64> = (0..n).map(|i| f(a[i], b[i])).collect();
        Self::from_twos(out)
    }
    fn twos(&self, n: usize) -> Vec<u64> {
        let mut v = vec![0u64; n];
        for (i, &d) in self.0.mag.iter().enumerate() {
            v[i] = d;
        }
        if self.0.neg {
            for d in v.iter_mut() {
                *d = !*d;
            }
            let mut carry = 1u128;
            for d in v.iter_mut() {
                let s = *d as u128 + carry;
                *d = s as u64;
                carry = s >> 64;
                if carry == 0 {
                    break;
                }
            }
        }
        v
    }
    fn from_twos(mut v: Vec<u64>) -> Self {
        let neg = v.last().map(|&d| d >> 63 == 1).unwrap_or(false);
        if neg {
            // Negate: invert every limb, then add one.
            for d in v.iter_mut() {
                *d = !*d;
            }
            let mut carry = 1u128;
            for d in v.iter_mut() {
                let s = *d as u128 + carry;
                *d = s as u64;
                carry = s >> 64;
                if carry == 0 {
                    break;
                }
            }
        }
        Self::make(neg, v)
    }

    pub fn cmp(&self, o: &Self) -> Ordering {
        match (self.0.neg, o.0.neg) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => mag_cmp(&self.0.mag, &o.0.mag),
            (true, true) => mag_cmp(&o.0.mag, &self.0.mag),
        }
    }
    /// Exact comparison with a finite f64 (NaN/±∞ handled by the caller).
    pub fn cmp_f64(&self, n: f64) -> Option<Ordering> {
        if n.is_nan() {
            return None;
        }
        if n == f64::INFINITY {
            return Some(Ordering::Less);
        }
        if n == f64::NEG_INFINITY {
            return Some(Ordering::Greater);
        }
        // Compare with the integer part, then break ties on the fraction.
        let trunc = Self::from_f64(n.trunc()).expect("finite trunc");
        match self.cmp(&trunc) {
            Ordering::Equal => {
                let frac = n - n.trunc();
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
    pub fn eq_f64(&self, n: f64) -> bool {
        n.is_finite() && n.fract() == 0.0 && self.cmp_f64(n) == Some(Ordering::Equal)
    }
    /// Exact conversion from an integral finite f64.
    pub fn from_f64(n: f64) -> Option<Self> {
        if !n.is_finite() || n.fract() != 0.0 {
            return None;
        }
        if n == 0.0 {
            return Some(Self::zero());
        }
        let neg = n < 0.0;
        let a = n.abs();
        let bits = a.to_bits();
        let exp = ((bits >> 52) & 0x7FF) as i64 - 1075;
        let frac = if (bits >> 52) & 0x7FF == 0 {
            bits & ((1u64 << 52) - 1)
        } else {
            (bits & ((1u64 << 52) - 1)) | (1u64 << 52)
        };
        let base = Self::make(neg, vec![frac]);
        Some(if exp >= 0 {
            base.shl(exp as u64)
        } else {
            base.shr((-exp) as u64)
        })
    }
    /// Correctly-rounded conversion to f64 (round-to-nearest, ties to even).
    pub fn to_f64(&self) -> f64 {
        let bl = self.bit_len();
        if bl == 0 {
            return 0.0;
        }
        let sign = if self.0.neg { -1.0 } else { 1.0 };
        if bl <= 64 {
            return self.0.mag[0] as f64 * sign;
        }
        // Work on the magnitude (`shr`/`shl` are arithmetic and would skew a negative value).
        let mag = Self::make(false, self.0.mag.clone());
        // Take the top 54 bits; round to 53 with a sticky bit for the rest.
        let shift = (bl - 54) as u64;
        let head_big = mag.shr(shift);
        let head = *head_big.0.mag.first().unwrap_or(&0); // 54 bits
        let sticky = head_big.shl(shift).cmp(&mag) != std::cmp::Ordering::Equal;
        let q = head >> 1;
        let round = head & 1 == 1;
        let up = round && (sticky || q & 1 == 1);
        let m = q + up as u64;
        m as f64 * 2f64.powi(shift as i32 + 1) * sign
    }

    /// Parse from digits (no sign) in the given radix.
    pub fn parse_radix(text: &str, radix: u32) -> Option<Self> {
        // Accumulate `k` digits at a time into one limb, then `acc = acc * radix^k + chunk` in
        // place.
        let (_, k) = radix_chunk(radix);
        let mut acc: Vec<u64> = Vec::new();
        let (mut chunk, mut scale, mut n, mut any) = (0u64, 1u64, 0usize, false);
        let flush = |acc: &mut Vec<u64>, scale: u64, chunk: u64| {
            let carry = mac_small(acc, scale, chunk);
            if carry != 0 {
                acc.push(carry);
            }
        };
        for c in text.chars() {
            let d = c.to_digit(radix)?;
            chunk = chunk * radix as u64 + d as u64;
            scale *= radix as u64;
            n += 1;
            any = true;
            if n == k {
                flush(&mut acc, scale, chunk);
                (chunk, scale, n) = (0, 1, 0);
            }
        }
        if n > 0 {
            flush(&mut acc, scale, chunk);
        }
        if any {
            Some(Self::make(false, acc))
        } else {
            None
        }
    }
    /// Decimal digits (used by StringToBigInt with an optional leading sign handled outside).
    pub fn parse_dec(text: &str) -> Option<Self> {
        if !text.chars().all(|c| c.is_ascii_digit()) || text.is_empty() {
            return None;
        }
        Self::parse_radix(text, 10)
    }

    pub fn to_string_radix(&self, radix: u32) -> String {
        if self.is_zero() {
            return "0".to_string();
        }
        // Peel off `k` digits per short division by `radix^k` (least significant chunk first).
        let (big, k) = radix_chunk(radix);
        let mut digits: Vec<u8> = Vec::with_capacity(self.bit_len() / 3 + k + 1);
        let mut cur = self.0.mag.clone();
        while !cur.is_empty() {
            let mut rem = div_small_in_place(&mut cur, big);
            while cur.last() == Some(&0) {
                cur.pop();
            }
            // Every chunk but the most significant is zero-padded to `k` digits.
            let mut emitted = 0;
            while rem != 0 || (!cur.is_empty() && emitted < k) {
                let d = (rem % radix as u64) as u32;
                digits.push(std::char::from_digit(d, radix).unwrap() as u8);
                rem /= radix as u64;
                emitted += 1;
            }
        }
        if self.0.neg {
            digits.push(b'-');
        }
        digits.reverse();
        String::from_utf8(digits).expect("ASCII digits")
    }
}

impl std::fmt::Display for JsBigInt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_string_radix(10))
    }
}

impl PartialEq for JsBigInt {
    fn eq(&self, other: &Self) -> bool {
        self.0.neg == other.0.neg && self.0.mag == other.0.mag
    }
}
impl Eq for JsBigInt {}

impl std::hash::Hash for JsBigInt {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.neg.hash(state);
        self.0.mag.hash(state);
    }
}

impl From<i128> for JsBigInt {
    fn from(v: i128) -> Self {
        Self::from_i128(v)
    }
}
impl From<i64> for JsBigInt {
    fn from(v: i64) -> Self {
        Self::from_i128(v as i128)
    }
}
impl From<u64> for JsBigInt {
    fn from(v: u64) -> Self {
        Self::from_u64(v)
    }
}

#[cfg(test)]
mod tests {
    use super::JsBigInt;
    use std::cmp::Ordering;

    fn big(s: &str) -> JsBigInt {
        let (neg, digits) = match s.strip_prefix('-') {
            Some(d) => (true, d),
            None => (false, s),
        };
        let v = JsBigInt::parse_dec(digits).unwrap();
        if neg {
            v.neg()
        } else {
            v
        }
    }

    #[test]
    fn parse_and_display_round_trip() {
        for s in [
            "0",
            "1",
            "-1",
            "18446744073709551616",
            "-340282366920938463463374607431768211456",
            "123456789012345678901234567890123456789012345678901234567890",
        ] {
            assert_eq!(big(s).to_string(), s);
        }
        assert_eq!(JsBigInt::parse_radix("ff", 16).unwrap().to_string(), "255");
        assert_eq!(JsBigInt::parse_radix("", 10), None);
        assert_eq!(JsBigInt::parse_radix("1_0", 10), None);
    }

    #[test]
    fn arithmetic_signs_and_carries() {
        let a = big("18446744073709551615"); // 2^64 - 1
        assert_eq!(
            a.add(&JsBigInt::from_u64(1)).to_string(),
            "18446744073709551616"
        );
        assert_eq!(a.sub(&a), JsBigInt::zero());
        assert_eq!(big("5").sub(&big("7")).to_string(), "-2");
        assert_eq!(big("-5").mul(&big("-7")).to_string(), "35");
        assert_eq!(
            a.mul(&a).to_string(),
            "340282366920938463426481119284349108225"
        );
    }

    #[test]
    fn division_truncates_toward_zero() {
        // BigInt / and % truncate (like Rust integer division), unlike shr which floors.
        assert_eq!(big("7").div(&big("2")).unwrap().to_string(), "3");
        assert_eq!(big("-7").div(&big("2")).unwrap().to_string(), "-3");
        assert_eq!(big("7").rem(&big("-2")).unwrap().to_string(), "1");
        assert_eq!(big("-7").rem(&big("2")).unwrap().to_string(), "-1");
        assert_eq!(big("7").div(&JsBigInt::zero()), None);
        assert_eq!(big("7").rem(&JsBigInt::zero()), None);
    }

    #[test]
    fn pow_and_negative_exponent() {
        assert_eq!(
            big("2").pow(&big("128")).unwrap().to_string(),
            "340282366920938463463374607431768211456"
        );
        assert_eq!(big("-3").pow(&big("3")).unwrap().to_string(), "-27");
        assert_eq!(big("2").pow(&big("-1")), None);
        assert_eq!(big("7").pow(&JsBigInt::zero()).unwrap().to_string(), "1");
    }

    #[test]
    fn shifts_floor_toward_negative_infinity() {
        assert_eq!(
            big("1").shl(130).to_string(),
            "1361129467683753853853498429727072845824"
        );
        assert_eq!(big("1").shl(130).shr(130).to_string(), "1");
        // Arithmetic right shift floors: -1 >> anything is -1, -5 >> 1 is -3.
        assert_eq!(big("-1").shr(200).to_string(), "-1");
        assert_eq!(big("-5").shr(1).to_string(), "-3");
        assert_eq!(big("5").shr(1).to_string(), "2");
        assert_eq!(big("4").shr(70), JsBigInt::zero());
    }

    #[test]
    fn twos_complement_bitwise() {
        assert_eq!(big("-1").bitand(&big("255")).to_string(), "255");
        assert_eq!(big("-2").bitor(&big("1")).to_string(), "-1");
        assert_eq!(big("-1").bitxor(&big("-1")), JsBigInt::zero());
        assert_eq!(big("0").not().to_string(), "-1");
        assert_eq!(
            big("18446744073709551616").not().to_string(),
            "-18446744073709551617"
        );
        // n & (2^64 - 1) is n mod 2^64 (asUintN's masking identity).
        let mask = big("18446744073709551615");
        assert_eq!(big("-1").bitand(&mask), mask);
    }

    #[test]
    fn exact_f64_comparison_at_extremes() {
        // 2^53 and 2^53 + 1: f64 can't tell them apart, the exact comparison must.
        let n = 9007199254740992f64; // 2^53
        assert_eq!(big("9007199254740993").cmp_f64(n), Some(Ordering::Greater));
        assert!(big("9007199254740992").eq_f64(n));
        assert!(!big("9007199254740993").eq_f64(n));
        assert_eq!(big("1").cmp_f64(1.5), Some(Ordering::Less));
        assert_eq!(big("2").cmp_f64(1.5), Some(Ordering::Greater));
        assert_eq!(
            big("-1").cmp_f64(f64::NEG_INFINITY),
            Some(Ordering::Greater)
        );
        assert_eq!(big("1").cmp_f64(f64::NAN), None);
        // Far beyond i128: the magnitude still orders correctly.
        assert_eq!(
            big("2").pow(&big("200")).unwrap().cmp_f64(1e60),
            Some(Ordering::Greater)
        );
        assert_eq!(
            big("2").pow(&big("200")).unwrap().cmp_f64(1e61),
            Some(Ordering::Less)
        );
    }

    #[test]
    fn f64_conversions() {
        assert_eq!(JsBigInt::from_f64(0.5), None);
        assert_eq!(JsBigInt::from_f64(f64::NAN), None);
        assert_eq!(
            JsBigInt::from_f64(1e21).unwrap().to_string(),
            "1000000000000000000000"
        );
        assert_eq!(big("-9007199254740992").to_f64(), -9007199254740992.0);
        assert_eq!(big("2").pow(&big("100")).unwrap().to_f64(), 2f64.powi(100));
    }

    #[test]
    fn karatsuba_and_knuth_d_agree_with_schoolbook() {
        // xorshift limbs, including all-ones / sparse patterns that stress q̂ correction.
        let mut st = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            st ^= st << 13;
            st ^= st >> 7;
            st ^= st << 17;
            st
        };
        for round in 0..300 {
            let la = 1 + (next() % 140) as usize;
            let lb = 1 + (next() % 140) as usize;
            let mut gen = |n: usize| -> Vec<u64> {
                (0..n)
                    .map(|i| match round % 4 {
                        0 => u64::MAX,
                        1 if i % 3 == 0 => 0,
                        2 if i + 1 == n => 1 << 63,
                        _ => next(),
                    })
                    .collect()
            };
            let a = super::trim(gen(la));
            let b = super::trim(gen(lb));
            if a.is_empty() || b.is_empty() {
                continue;
            }
            let mut school = vec![0u64; a.len() + b.len()];
            super::mul_school(&a, &b, &mut school);
            let school = super::trim(school);
            assert_eq!(super::mag_mul(&a, &b), school, "mul {la}x{lb}");
            assert_eq!(super::mag_sqr(&a), super::mag_mul(&a, &a.clone()), "sqr {la}");
            let (q, r) = super::mag_divmod(&a, &b, true);
            assert_eq!(super::mag_cmp(&r, &b), Ordering::Less);
            let back = super::mag_add(&super::mag_mul(&q, &b), &r);
            assert_eq!(super::trim(back), a, "divmod {la}/{lb}");
            assert_eq!(super::mag_divmod(&a, &b, false).1, r);
        }
    }

    #[test]
    fn i128_round_trips_and_wrapping() {
        assert_eq!(JsBigInt::from_i128(i128::MIN).to_i128(), Some(i128::MIN));
        assert_eq!(JsBigInt::from_i128(i128::MAX).to_i128(), Some(i128::MAX));
        let over = JsBigInt::from_i128(i128::MAX).add(&JsBigInt::from_u64(1));
        assert_eq!(over.to_i128(), None);
        assert_eq!(over.to_i128_wrapping(), i128::MIN);
    }
}
