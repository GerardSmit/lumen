//! Arbitrary-precision BigInt, from scratch: sign + little-endian u64 magnitude behind an `Rc`
//! (clones are cheap; every operation allocates one fresh value).
//!
//! Multiplication is schoolbook (with a dedicated squaring path), Karatsuba above
//! [`KARATSUBA_LIMBS`], and a three-prime number-theoretic transform above [`NTT_LIMBS`].
//! Division is Knuth algorithm D, switching to Burnikel–Ziegler recursive division above
//! [`BZ_LIMBS`]; single-limb steps divide by a precomputed reciprocal. Radix conversion is linear
//! for power-of-two radices and divide-and-conquer over precomputed `radix^(k·2^i)` powers
//! otherwise. Values are capped at [`MAX_BITS`]; the `checked_*` operations reject results past it
//! before allocating them.

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;

type Flag = Option<Arc<AtomicBool>>;

const IDLE: u8 = 0;
const ARMED: u8 = 1;
const ABORTED: u8 = 2;

thread_local! {
    static STATE: Cell<u8> = const { Cell::new(IDLE) };
    static FLAGS: RefCell<(Flag, Flag)> = const { RefCell::new((None, None)) };
}

/// Runs `f` with the long-running operations below polling `interrupt` and `deadline`. When
/// either is raised the operations stop early with meaningless results and this returns `None`
/// (the caller discards everything `f` produced). Outside this wrapper the polls are inert.
pub fn interruptible<T>(interrupt: &Flag, deadline: &Flag, f: impl FnOnce() -> T) -> Option<T> {
    if STATE.get() != IDLE {
        return Some(f());
    }
    FLAGS.with(|c| *c.borrow_mut() = (interrupt.clone(), deadline.clone()));
    STATE.set(ARMED);
    let out = f();
    let aborted = STATE.replace(IDLE) == ABORTED;
    FLAGS.with(|c| *c.borrow_mut() = (None, None));
    (!aborted).then_some(out)
}

#[inline(always)]
fn aborted() -> bool {
    STATE.get() == ABORTED
}

/// True once the host asked to stop; sticky until the enclosing [`interruptible`] returns.
#[inline]
fn poll() -> bool {
    match STATE.get() {
        IDLE => false,
        ABORTED => true,
        _ => poll_flags(),
    }
}

#[cold]
#[inline(never)]
fn poll_flags() -> bool {
    let hit = FLAGS.with(|c| {
        let (i, d) = &*c.borrow();
        i.as_ref().is_some_and(|f| f.load(Relaxed)) || d.as_ref().is_some_and(|f| f.load(Relaxed))
    });
    if hit {
        STATE.set(ABORTED);
    }
    hit
}

#[derive(Clone, Debug)]
pub struct JsBigInt(Rc<BigIntData>);

#[derive(Debug)]
struct BigIntData {
    /// True for negative values. Zero is always non-negative with an empty magnitude.
    neg: bool,
    /// Little-endian base-2^64 digits, no trailing zero limbs.
    mag: Vec<u64>,
}

/// Largest magnitude, in bits, a BigInt may have (V8's `BigInt::kMaxLengthBits`).
pub const MAX_BITS: u64 = 1 << 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BigIntError {
    TooLarge,
    DivisionByZero,
    NegativeExponent,
}

impl BigIntError {
    /// The RangeError message.
    pub fn message(self) -> &'static str {
        match self {
            BigIntError::TooLarge => "Maximum BigInt size exceeded",
            BigIntError::DivisionByZero => "Division by zero",
            BigIntError::NegativeExponent => "Exponent must be non-negative",
        }
    }
}

/// Operand size (limbs, of the shorter factor) from which multiplication switches to Karatsuba.
const KARATSUBA_LIMBS: usize = 32;
/// The same threshold for squaring (the schoolbook square does half the products, so it wins
/// for longer).
const KARATSUBA_SQR_LIMBS: usize = 48;
/// Operand size (limbs, of the shorter factor) from which multiplication uses the NTT.
const NTT_LIMBS: usize = 800;
/// The same threshold for squaring.
const NTT_SQR_LIMBS: usize = 1000;
/// Divisor size (limbs) from which division recurses Burnikel–Ziegler style.
const BZ_LIMBS: usize = 80;
/// Divisor size (limbs) from which radix conversion divides by multiplying with a Newton
/// reciprocal (Barrett) of each power it reuses.
const BARRETT_LIMBS: usize = 1000;
/// Divisor size (limbs) up to which [`reciprocal`] divides directly instead of iterating.
const RECIPROCAL_BASE_LIMBS: usize = 200;
/// Magnitude size (limbs) below which radix conversion uses repeated short division.
const TO_STRING_BASE_LIMBS: usize = 30;
/// Digit chunks (each one limb's worth of digits) below which parsing is a plain
/// multiply-accumulate loop.
const PARSE_BASE_CHUNKS: usize = 40;

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
    if acc.len() < b.len() {
        return false;
    }
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
    if acc.len() < b.len() {
        return;
    }
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
        if b.len() >= NTT_LIMBS && poll() {
            return;
        }
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
    if aborted() {
        return Vec::new();
    }
    trim(out)
}

/// `out = a * b` for a zeroed `out` of exactly `a.len() + b.len()` limbs.
fn mul_into(a: &[u64], b: &[u64], out: &mut [u64]) {
    let (a, b) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    if b.len() < KARATSUBA_LIMBS {
        // Rows over the longer operand: a long `a` times a few limbs runs a few tight rows.
        mul_school(b, a, out);
    } else if a.len() >= 2 * b.len() {
        // Unbalanced: multiply `b`-sized slices of `a` and accumulate.
        let mut tmp = vec![0u64; 2 * b.len()];
        let mut i = 0;
        while i < a.len() {
            if b.len() >= NTT_LIMBS && poll() {
                return;
            }
            let chunk = &a[i..(i + b.len()).min(a.len())];
            let t = &mut tmp[..chunk.len() + b.len()];
            t.fill(0);
            mul_into(chunk, b, t);
            add_into(&mut out[i..], trimmed(t));
            i += b.len();
        }
    } else if b.len() >= NTT_LIMBS {
        ntt_mul(a, Some(b), out);
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
    } else if a.len() >= NTT_SQR_LIMBS {
        ntt_mul(a, None, &mut out);
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
    if aborted() {
        return Vec::new();
    }
    trim(out)
}

/// Montgomery arithmetic modulo an NTT prime `p < 2^62` (values in `[0, p)`, `R = 2^64`).
struct Mont {
    p: u64,
    /// `-p⁻¹ mod 2^64`.
    pneg_inv: u64,
    /// `R² mod p`.
    r2: u64,
}

impl Mont {
    fn new(p: u64) -> Self {
        let mut inv = p;
        for _ in 0..6 {
            inv = inv.wrapping_mul(2u64.wrapping_sub(p.wrapping_mul(inv)));
        }
        let r = ((1u128 << 64) % p as u128) as u64;
        let r2 = ((r as u128 * r as u128) % p as u128) as u64;
        Mont { p, pneg_inv: inv.wrapping_neg(), r2 }
    }
    /// `a·b·R⁻¹ mod p`; also correct for any `a < 2^64` when `b < p`.
    #[inline(always)]
    fn mul(&self, a: u64, b: u64) -> u64 {
        let t = a as u128 * b as u128;
        let m = (t as u64).wrapping_mul(self.pneg_inv);
        let u = ((t + m as u128 * self.p as u128) >> 64) as u64;
        if u >= self.p {
            u - self.p
        } else {
            u
        }
    }
    #[inline(always)]
    fn add(&self, a: u64, b: u64) -> u64 {
        let s = a + b;
        if s >= self.p {
            s - self.p
        } else {
            s
        }
    }
    #[inline(always)]
    fn sub(&self, a: u64, b: u64) -> u64 {
        if a >= b {
            a - b
        } else {
            a + self.p - b
        }
    }
    fn to_mont(&self, x: u64) -> u64 {
        self.mul(x, self.r2)
    }
    fn pow(&self, mut base: u64, mut e: u64) -> u64 {
        let mut acc = self.to_mont(1);
        while e > 0 {
            if e & 1 == 1 {
                acc = self.mul(acc, base);
            }
            base = self.mul(base, base);
            e >>= 1;
        }
        acc
    }
}

/// Primes `c·2^32 + 1 < 2^62` with a generator of their multiplicative group. Their product
/// (~2^186) exceeds every convolution coefficient (< 2^128 · 2^25 at the size cap).
const NTT_PRIMES: [(u64, u64); 3] = [
    (0x3fff_ffee_0000_0001, 3),
    (0x3fff_ffb4_0000_0001, 19),
    (0x3fff_ffa0_0000_0001, 3),
];

/// `roots[j] = w^j` (Montgomery form) for a primitive `n`-th root of unity `w`, `j < n/2`.
fn ntt_roots(m: &Mont, g: u64, n: usize) -> Vec<u64> {
    let w = m.pow(m.to_mont(g), (m.p - 1) / n as u64);
    let mut roots = Vec::with_capacity(n / 2);
    let mut x = m.to_mont(1);
    for _ in 0..n / 2 {
        roots.push(x);
        x = m.mul(x, w);
    }
    roots
}

/// Transform size (elements) from which the NTT recurses one stage at a time, so the remaining
/// stages run on cache-resident halves.
const NTT_BLOCK: usize = 1 << 12;

/// One decimation-in-frequency stage over `a` (twiddle `j` is `roots[j·stride]`).
fn dif_stage(m: &Mont, a: &mut [u64], len: usize, roots: &[u64], stride: usize) {
    for block in a.chunks_exact_mut(2 * len) {
        let (lo, hi) = block.split_at_mut(len);
        let (u, v) = (lo[0], hi[0]);
        lo[0] = m.add(u, v);
        hi[0] = m.sub(u, v);
        for j in 1..len {
            let (u, v) = (lo[j], hi[j]);
            lo[j] = m.add(u, v);
            hi[j] = m.mul(m.sub(u, v), roots[j * stride]);
        }
    }
}

/// One decimation-in-time stage with inverse twiddles (`w^-k = -w^(n/2 - k)`).
fn dit_stage(m: &Mont, a: &mut [u64], len: usize, roots: &[u64], stride: usize) {
    let half = roots.len();
    for block in a.chunks_exact_mut(2 * len) {
        let (lo, hi) = block.split_at_mut(len);
        let (u, v) = (lo[0], hi[0]);
        lo[0] = m.add(u, v);
        hi[0] = m.sub(u, v);
        for j in 1..len {
            let v = m.mul(hi[j], m.p - roots[half - j * stride]);
            let u = lo[j];
            lo[j] = m.add(u, v);
            hi[j] = m.sub(u, v);
        }
    }
}

/// Roots of unity for every recursion depth: `tables[d][j] = w_n^j` for `n = N / 2^d`, `j < n/2`,
/// down to the block size (each level contiguous, so no stage reads twiddles strided).
fn ntt_tables(m: &Mont, g: u64, n: usize) -> Vec<Vec<u64>> {
    let mut tables = vec![ntt_roots(m, g, n)];
    let mut size = n;
    while size > NTT_BLOCK {
        size /= 2;
        let next = tables.last().unwrap().iter().step_by(2).copied().collect();
        tables.push(next);
    }
    tables
}

/// Forward transform (decimation in frequency): natural order in, bit-reversed order out.
fn ntt_forward(m: &Mont, a: &mut [u64], tables: &[Vec<u64>]) {
    let n = a.len();
    if n >= NTT_BLOCK && poll() {
        return;
    }
    if n > NTT_BLOCK {
        dif_stage(m, a, n / 2, &tables[0], 1);
        let (lo, hi) = a.split_at_mut(n / 2);
        ntt_forward(m, lo, &tables[1..]);
        ntt_forward(m, hi, &tables[1..]);
        return;
    }
    let mut len = n / 2;
    while len >= 1 {
        dif_stage(m, a, len, &tables[0], n / (2 * len));
        len /= 2;
    }
}

/// Inverse transform (decimation in time, unscaled): bit-reversed order in, natural order out.
fn ntt_inverse(m: &Mont, a: &mut [u64], tables: &[Vec<u64>]) {
    let n = a.len();
    if n >= NTT_BLOCK && poll() {
        return;
    }
    if n > NTT_BLOCK {
        let (lo, hi) = a.split_at_mut(n / 2);
        ntt_inverse(m, lo, &tables[1..]);
        ntt_inverse(m, hi, &tables[1..]);
        dit_stage(m, a, n / 2, &tables[0], 1);
        return;
    }
    let mut len = 1;
    while len < n {
        dit_stage(m, a, len, &tables[0], n / (2 * len));
        len *= 2;
    }
}

/// `out = a * b` (or `a²` when `b` is `None`) via the NTT modulo three primes and CRT, for a
/// zeroed `out` of exactly `a.len() + b.len()` limbs.
fn ntt_mul(a: &[u64], b: Option<&[u64]>, out: &mut [u64]) {
    let n = out.len().next_power_of_two().max(2);
    let mut residues: Vec<Vec<u64>> = Vec::with_capacity(3);
    for &(p, g) in &NTT_PRIMES {
        let m = Mont::new(p);
        let tables = ntt_tables(&m, g, n);
        let load = |x: &[u64]| {
            let mut v = vec![0u64; n];
            for (d, &s) in v.iter_mut().zip(x) {
                *d = m.to_mont(s);
            }
            ntt_forward(&m, &mut v, &tables);
            v
        };
        let mut fa = load(a);
        match b {
            Some(b) => {
                let fb = load(b);
                for (x, &y) in fa.iter_mut().zip(&fb) {
                    *x = m.mul(*x, y);
                }
            }
            None => {
                for x in fa.iter_mut() {
                    *x = m.mul(*x, *x);
                }
            }
        }
        ntt_inverse(&m, &mut fa, &tables);
        // Multiplying a Montgomery value by a plain `n⁻¹` leaves the plain, scaled residue.
        let n_inv = m.pow(m.to_mont(n as u64), p - 2);
        let n_inv_plain = m.mul(n_inv, 1);
        for x in fa.iter_mut().take(out.len()) {
            *x = m.mul(*x, n_inv_plain);
        }
        fa.truncate(out.len());
        residues.push(fa);
        if aborted() {
            return;
        }
    }
    // Garner: x = r1 + p1·t2 + p1·p2·t3, each coefficient < p1·p2·p3, accumulated with carries.
    let (p1, p2, p3) = (NTT_PRIMES[0].0, NTT_PRIMES[1].0, NTT_PRIMES[2].0);
    let (m2, m3) = (Mont::new(p2), Mont::new(p3));
    // Constants in Montgomery form, so one `mul` by them multiplies a plain value plainly.
    let inv_p1_mod_p2 = m2.pow(m2.to_mont(p1 % p2), p2 - 2);
    let p1_mod_p3 = m3.to_mont(p1 % p3);
    let p12 = p1 as u128 * p2 as u128;
    let inv_p12_mod_p3 = m3.pow(m3.to_mont((p12 % p3 as u128) as u64), p3 - 2);
    let (p12_lo, p12_hi) = (p12 as u64, (p12 >> 64) as u64);
    let (mut c0, mut c1) = (0u64, 0u64);
    for i in 0..out.len() {
        let (r1, r2, r3) = (residues[0][i], residues[1][i], residues[2][i]);
        let r1_2 = if r1 >= p2 { r1 - p2 } else { r1 };
        let t2 = m2.mul(m2.sub(r2, r1_2), inv_p1_mod_p2);
        let r1_3 = if r1 >= p3 { r1 - p3 } else { r1 };
        let s = m3.add(r1_3, m3.mul(t2, p1_mod_p3));
        let t3 = m3.mul(m3.sub(r3, s), inv_p12_mod_p3);
        // x = r1 + p1·t2 (< 2^125) + p12·t3 (< 2^186), plus the running carry.
        let low = r1 as u128 + p1 as u128 * t2 as u128;
        let a = p12_lo as u128 * t3 as u128;
        let b = p12_hi as u128 * t3 as u128;
        let s0 = (low as u64) as u128 + (a as u64) as u128 + c0 as u128;
        let s1 = (low >> 64) + (a >> 64) + (b as u64) as u128 + c1 as u128 + (s0 >> 64);
        let s2 = (b >> 64) + (s1 >> 64);
        out[i] = s0 as u64;
        c0 = s1 as u64;
        c1 = s2 as u64;
    }
    debug_assert!(c0 == 0 && c1 == 0);
}

/// Reciprocal of a single-limb divisor (Möller–Granlund, "Improved division by invariant
/// integers"): `d` normalized by `shift`, and `v = ⌊(2^128 − 1) / d⌋ − 2^64`.
struct Recip {
    d: u64,
    v: u64,
    shift: u32,
}

impl Recip {
    fn new(d: u64) -> Self {
        let shift = d.leading_zeros();
        let d = d << shift;
        let v = (u128::MAX / d as u128 - (1u128 << 64)) as u64;
        Recip { d, v, shift }
    }
    /// `(u1·2^64 + u0) / d` and its remainder, for the normalized `d` and `u1 < d`.
    #[inline(always)]
    fn div2by1(&self, u1: u64, u0: u64) -> (u64, u64) {
        let q = (self.v as u128 * u1 as u128).wrapping_add(((u1 as u128) << 64) | u0 as u128);
        let mut q1 = ((q >> 64) as u64).wrapping_add(1);
        let q0 = q as u64;
        let mut r = u0.wrapping_sub(q1.wrapping_mul(self.d));
        if r > q0 {
            q1 = q1.wrapping_sub(1);
            r = r.wrapping_add(self.d);
        }
        if r >= self.d {
            q1 += 1;
            r -= self.d;
        }
        (q1, r)
    }
}

/// In-place short division of `a` by the limb `d` (non-zero); returns the remainder. `a` is left
/// untrimmed.
fn div_small_in_place(a: &mut [u64], d: u64) -> u64 {
    if a.len() <= 2 {
        // Too short to amortize the reciprocal.
        let n = a.iter().rev().fold(0u128, |acc, &x| (acc << 64) | x as u128);
        let q = n / d as u128;
        for (i, x) in a.iter_mut().enumerate() {
            *x = (q >> (64 * i)) as u64;
        }
        return (n - q * d as u128) as u64;
    }
    div_small_recip(a, &Recip::new(d))
}

fn div_small_recip(a: &mut [u64], r: &Recip) -> u64 {
    let s = r.shift;
    if s == 0 {
        let mut rem = 0u64;
        for x in a.iter_mut().rev() {
            let (q, nr) = r.div2by1(rem, *x);
            *x = q;
            rem = nr;
        }
        return rem;
    }
    // Divide `a << s` by `d << s`, forming each shifted limb from two neighbours on the fly.
    let n = a.len();
    if n == 0 {
        return 0;
    }
    let mut rem = a[n - 1] >> (64 - s);
    for i in (0..n).rev() {
        let lo = if i > 0 { a[i - 1] >> (64 - s) } else { 0 };
        let (q, nr) = r.div2by1(rem, (a[i] << s) | lo);
        a[i] = q;
        rem = nr;
    }
    rem >> s
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
    if b.len() >= BZ_LIMBS && a.len() >= b.len() + BZ_LIMBS {
        bz_divmod(a, b)
    } else {
        knuth_divmod(a, b, want_q)
    }
}

/// `hi·B^k + lo` for `lo < B^k`, trimmed.
fn join_limbs(hi: &[u64], k: usize, lo: &[u64]) -> Vec<u64> {
    let mut out = Vec::with_capacity(k + hi.len());
    out.extend_from_slice(lo);
    if !hi.is_empty() {
        out.resize(k, 0);
        out.extend_from_slice(hi);
    }
    trim(out)
}

/// The limbs of `a` from `k` up (`a >> 64k`).
fn high_limbs(a: &[u64], k: usize) -> &[u64] {
    if a.len() > k {
        &a[k..]
    } else {
        &[]
    }
}

/// The low `k` limbs of `a`, trimmed (`a mod B^k`).
fn low_limbs(a: &[u64], k: usize) -> &[u64] {
    trimmed(&a[..a.len().min(k)])
}

/// Burnikel–Ziegler: schoolbook long division in base `B^n` (`n = b.len()`), each `2n / n` step
/// done by [`div2n1n`].
fn bz_divmod(a: &[u64], b: &[u64]) -> (Vec<u64>, Vec<u64>) {
    let s = b[b.len() - 1].leading_zeros();
    let b = shl_bits(b, s, 0);
    let a = trim(shl_bits(a, s, 1));
    let n = b.len();
    let chunks = a.len().div_ceil(n);
    let mut q = vec![0u64; chunks * n];
    let mut r: Vec<u64> = Vec::new();
    for i in (0..chunks).rev() {
        if poll() {
            return (Vec::new(), Vec::new());
        }
        let chunk = trimmed(&a[i * n..((i + 1) * n).min(a.len())]);
        let cur = join_limbs(&r, n, chunk);
        let (qi, ri) = div2n1n(&cur, &b);
        q[i * n..i * n + qi.len()].copy_from_slice(&qi);
        r = ri;
    }
    shr_bits_in_place(&mut r, s);
    (trim(q), trim(r))
}

/// `a >>= s` for `s < 64` (left untrimmed).
fn shr_bits_in_place(a: &mut [u64], s: u32) {
    if s != 0 {
        let mut carry = 0u64;
        for x in a.iter_mut().rev() {
            let v = *x;
            *x = (v >> s) | carry;
            carry = v << (64 - s);
        }
    }
}

/// `B^k` as limbs.
fn power_of_base(k: usize) -> Vec<u64> {
    let mut v = vec![0u64; k + 1];
    v[k] = 1;
    v
}

/// `⌊B^(2n) / d⌋` for a normalized `d` of `n` limbs: a Newton approximation, then an exact
/// correction.
fn reciprocal(d: &[u64]) -> Vec<u64> {
    if poll() {
        return Vec::new();
    }
    let n = d.len();
    let x = reciprocal_approx(d);
    // Bring B^(2n) − d·x into [0, d).
    let full = power_of_base(2 * n);
    let prod = mag_mul(d, &x);
    if mag_cmp(&prod, &full) == Ordering::Greater {
        let (mut q, r) = knuth_divmod(&mag_sub(&prod, &full), d, true);
        if !r.is_empty() {
            q = mag_add(&q, &[1]);
        }
        mag_sub(&x, &q)
    } else {
        let (q, _) = knuth_divmod(&mag_sub(&full, &prod), d, true);
        trim(mag_add(&x, &q))
    }
}

/// `B^(2n) / d` within a few units. From the top `k` limbs, `x ≈ B^(2k) / d_hi` has relative
/// error ~B^-k, so `X0 = x·B^(n−k)`; one step `X1 = X0 + X0·(B^(2n) − d·X0) / B^(2n)` squares it.
fn reciprocal_approx(d: &[u64]) -> Vec<u64> {
    if poll() {
        return Vec::new();
    }
    let n = d.len();
    if n <= RECIPROCAL_BASE_LIMBS {
        return mag_divmod(&power_of_base(2 * n), d, true).0;
    }
    let k = n / 2 + 2;
    let x = reciprocal_approx(&d[n - k..]);
    // In units of B^(n−k): d·X0 = d·x·B^(n−k) against B^(2n) = B^(n+k)·B^(n−k).
    let dx = mag_mul(d, &x);
    let top = power_of_base(n + k);
    let x0 = join_limbs(&x, n - k, &[]);
    if mag_cmp(&dx, &top) != Ordering::Greater {
        let e = mag_sub(&top, &dx);
        mag_add(&x0, high_limbs(&mag_mul(&x, &e), 2 * k))
    } else {
        let e = mag_sub(&dx, &top);
        let corr = mag_mul(&x, &e);
        let corr = high_limbs(&corr, 2 * k);
        if mag_cmp(corr, &x0) == Ordering::Greater {
            Vec::new()
        } else {
            mag_sub(&x0, corr)
        }
    }
}

/// A divisor prepared for repeated Barrett division: the normalized `d = b·2^shift` of `n` limbs
/// and `inv = ⌊B^(2n) / d⌋`.
struct Barrett {
    d: Vec<u64>,
    shift: u32,
    inv: Vec<u64>,
}

impl Barrett {
    fn new(b: &[u64]) -> Self {
        let shift = b[b.len() - 1].leading_zeros();
        let d = shl_bits(b, shift, 0);
        let inv = reciprocal(&d);
        Barrett { d, shift, inv }
    }

    /// `(a / b, a % b)`: long division in base `B^n`, each step by [`Self::div2n`].
    fn divmod(&self, a: &[u64]) -> (Vec<u64>, Vec<u64>) {
        let n = self.d.len();
        let a = trim(shl_bits(a, self.shift, 1));
        let chunks = a.len().div_ceil(n);
        let mut q = vec![0u64; chunks * n];
        let mut r: Vec<u64> = Vec::new();
        for i in (0..chunks).rev() {
            if poll() {
                return (Vec::new(), Vec::new());
            }
            let chunk = trimmed(&a[i * n..((i + 1) * n).min(a.len())]);
            let (qi, ri) = self.div2n(&join_limbs(&r, n, chunk));
            q[i * n..i * n + qi.len()].copy_from_slice(&qi);
            r = ri;
        }
        shr_bits_in_place(&mut r, self.shift);
        (trim(q), trim(r))
    }

    /// `(a / d, a % d)` for `a < d·B^n`. The estimate `⌊⌊a / B^(n−1)⌋ · inv / B^(n+1)⌋` is at most
    /// two below the true quotient and never above it.
    fn div2n(&self, a: &[u64]) -> (Vec<u64>, Vec<u64>) {
        let n = self.d.len();
        let mut q = high_limbs(&mag_mul(high_limbs(a, n - 1), &self.inv), n + 1).to_vec();
        let mut r = mag_sub(a, &mag_mul(&q, &self.d));
        if aborted() {
            return (Vec::new(), Vec::new());
        }
        while mag_cmp(&r, &self.d) != Ordering::Less {
            sub_into(&mut r, &self.d);
            r = trim(r);
            q = mag_add(&q, &[1]);
        }
        (q, r)
    }
}

/// `(a / b, a % b)` for a normalized `b` (top bit set) of `n` limbs and `a < b·B^n`.
fn div2n1n(a: &[u64], b: &[u64]) -> (Vec<u64>, Vec<u64>) {
    let n = b.len();
    if n < BZ_LIMBS {
        return knuth_divmod(trimmed(a), b, true);
    }
    if n % 2 == 1 {
        // Pad both by one zero limb so the halves are equal (the quotient is unchanged).
        let mut a2 = vec![0u64];
        a2.extend_from_slice(a);
        let mut b2 = vec![0u64];
        b2.extend_from_slice(b);
        let (q, r) = div2n1n(&trim(a2), &b2);
        return (q, high_limbs(&r, 1).to_vec());
    }
    if poll() {
        return (Vec::new(), Vec::new());
    }
    let h = n / 2;
    let (q1, r) = div3n2n(high_limbs(a, n), trimmed(&a[h.min(a.len())..n.min(a.len())]), b, h);
    let (q2, r) = div3n2n(&r, low_limbs(a, h), b, h);
    (join_limbs(&q1, h, &q2), r)
}

/// `([a12, a3] / b, [a12, a3] % b)` for `b = [b1, b2]` of `2h` limbs (normalized), `a3 < B^h`,
/// and `a12 < b·B^h`.
fn div3n2n(a12: &[u64], a3: &[u64], b: &[u64], h: usize) -> (Vec<u64>, Vec<u64>) {
    let (b1, b2) = (&b[h..], low_limbs(b, h));
    let (mut q, r1) = if high_limbs(a12, h) == b1 {
        // The quotient saturates: q = B^h − 1, r1 = a12 − b1·B^h + b1 = (a12 mod B^h) + b1.
        (vec![u64::MAX; h], mag_add(low_limbs(a12, h), b1))
    } else {
        div2n1n(a12, b1)
    };
    let d = mag_mul(&q, b2);
    if aborted() {
        return (Vec::new(), Vec::new());
    }
    let mut t = join_limbs(&r1, h, a3);
    while mag_cmp(&t, &d) == Ordering::Less {
        t = mag_add(&t, b);
        q = mag_sub(&q, &[1]);
    }
    sub_into(&mut t, &d);
    (trim(q), trim(t))
}

fn knuth_divmod(a: &[u64], b: &[u64], want_q: bool) -> (Vec<u64>, Vec<u64>) {
    if aborted() {
        return (Vec::new(), Vec::new());
    }
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
    let vrecip = Recip::new(vtop);
    for j in (0..=m).rev() {
        if j & 255 == 0 && poll() {
            return (Vec::new(), Vec::new());
        }
        let (u2, u1, u0) = (u[j + n], u[j + n - 1], u[j + n - 2]);
        // Estimate q̂ from the top two limbs and refine it against the third (at most 2 off → 0/1).
        let (mut qhat, mut rhat, mut refine) = if u2 >= vtop {
            let (r, of) = u1.overflowing_add(vtop);
            (u64::MAX, r, !of)
        } else {
            let (q, r) = vrecip.div2by1(u2, u1);
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

/// `[b, b², b⁴, …]` for `b = radix^k` (one limb's worth of digits), up to `levels` entries.
fn radix_powers(radix: u32, levels: usize) -> Vec<Vec<u64>> {
    let (big, _) = radix_chunk(radix);
    let mut pows = vec![vec![big]];
    while pows.len() < levels {
        let next = mag_sqr(pows.last().unwrap());
        if aborted() {
            break;
        }
        pows.push(next);
    }
    pows
}

fn digit_char(d: u32) -> u8 {
    if d < 10 {
        b'0' + d as u8
    } else {
        b'a' + (d - 10) as u8
    }
}

/// Appends the digits of `mag` (non-empty, trimmed), most significant first.
fn mag_to_radix(mag: &[u64], radix: u32, out: &mut Vec<u8>) {
    if radix.is_power_of_two() {
        let bits = radix.trailing_zeros() as usize;
        let total = (mag.len() - 1) * 64 + (64 - mag[mag.len() - 1].leading_zeros() as usize);
        let mask = (radix - 1) as u64;
        for i in (0..total.div_ceil(bits)).rev() {
            let (limb, off) = (i * bits / 64, i * bits % 64);
            let mut d = mag[limb] >> off;
            if off + bits > 64 && limb + 1 < mag.len() {
                d |= mag[limb + 1] << (64 - off);
            }
            out.push(digit_char((d & mask) as u32));
        }
        return;
    }
    let (big, k) = radix_chunk(radix);
    let recip = Recip::new(big);
    let mut pows: Vec<Vec<u64>> = Vec::new();
    if mag.len() >= TO_STRING_BASE_LIMBS {
        let mut levels = 1;
        while (1usize << levels) <= mag.len() / 2 + 1 {
            levels += 1;
        }
        pows = radix_powers(radix, levels);
        if aborted() {
            return;
        }
    }
    // A reciprocal pays off from about four divisions by the same power, i.e. two levels below
    // the root's.
    let root = (0..pows.len()).rev().find(|&l| 2 * pows[l].len() - 1 <= mag.len()).unwrap_or(0);
    let divs: Vec<Option<Barrett>> = pows
        .iter()
        .enumerate()
        .map(|(l, p)| (l + 2 <= root && p.len() >= BARRETT_LIMBS).then(|| Barrett::new(p)))
        .collect();
    if aborted() {
        return;
    }
    let ctx = RadixCtx { radix, k, recip, pows, divs };
    to_radix_dc(mag, None, &ctx, out);
}

/// Divide and conquer: `x = q·P + r` with `P = radix^(k·2^l)` about the square root of `x`, then
/// `q`'s digits followed by `r`'s, zero-padded to exactly `k·2^l`. With `width`, `x` is emitted
/// zero-padded to that many digits.
/// What [`to_radix_dc`] needs at every level: `radix^k` fits a limb, `pows[l] = radix^(k·2^l)`,
/// with a Barrett divisor for the large ones.
struct RadixCtx {
    radix: u32,
    k: usize,
    recip: Recip,
    pows: Vec<Vec<u64>>,
    divs: Vec<Option<Barrett>>,
}

fn to_radix_dc(x: &[u64], width: Option<usize>, ctx: &RadixCtx, out: &mut Vec<u8>) {
    if poll() {
        return;
    }
    let (radix, k, pows) = (ctx.radix, ctx.k, &ctx.pows);
    if x.len() < TO_STRING_BASE_LIMBS {
        // Peel off `k` digits per short division by `radix^k`, least significant chunk first.
        let start = out.len();
        let mut cur = x.to_vec();
        while !cur.is_empty() {
            let mut rem = div_small_recip(&mut cur, &ctx.recip);
            while cur.last() == Some(&0) {
                cur.pop();
            }
            for _ in 0..k {
                out.push(digit_char((rem % radix as u64) as u32));
                rem /= radix as u64;
            }
        }
        match width {
            Some(w) => out.resize(start + w, b'0'),
            None => {
                while out.len() > start + 1 && out.last() == Some(&b'0') {
                    out.pop();
                }
            }
        }
        out[start..].reverse();
        return;
    }
    let l = (0..pows.len()).rev().find(|&l| 2 * pows[l].len() - 1 <= x.len()).unwrap_or(0);
    let (q, r) = match &ctx.divs[l] {
        Some(div) => div.divmod(x),
        None => mag_divmod(x, &pows[l], true),
    };
    let low_width = k << l;
    match width {
        None if q.is_empty() => to_radix_dc(&r, None, ctx, out),
        None => {
            to_radix_dc(&q, None, ctx, out);
            to_radix_dc(&r, Some(low_width), ctx, out);
        }
        Some(w) => {
            to_radix_dc(&q, Some(w - low_width), ctx, out);
            to_radix_dc(&r, Some(low_width), ctx, out);
        }
    }
}

/// The magnitude spelled by `text` in `radix`: `Ok(None)` when it is empty or has a non-digit,
/// `Err(TooLarge)` past [`MAX_BITS`].
fn parse_mag(text: &str, radix: u32) -> Result<Option<Vec<u64>>, BigIntError> {
    let mut digits = Vec::with_capacity(text.len());
    for c in text.chars() {
        match c.to_digit(radix) {
            Some(d) => digits.push(d as u8),
            None => return Ok(None),
        }
    }
    if digits.is_empty() {
        return Ok(None);
    }
    let lead = digits.iter().position(|&d| d != 0).unwrap_or(digits.len());
    let digits = &digits[lead..];
    if digits.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let min_bits = (digits.len() - 1) as f64 * (radix as f64).log2();
    if min_bits >= MAX_BITS as f64 {
        return Err(BigIntError::TooLarge);
    }
    if radix.is_power_of_two() {
        let bits = radix.trailing_zeros() as usize;
        let mut mag = vec![0u64; (digits.len() * bits).div_ceil(64)];
        for (i, &d) in digits.iter().rev().enumerate() {
            let (limb, off) = (i * bits / 64, i * bits % 64);
            mag[limb] |= (d as u64) << off;
            if off + bits > 64 {
                mag[limb + 1] |= (d as u64) >> (64 - off);
            }
        }
        return Ok(Some(trim(mag)));
    }
    let (_, k) = radix_chunk(radix);
    let mut levels = 0;
    while (k << levels) < digits.len() {
        levels += 1;
    }
    let pows = if digits.len() > k * PARSE_BASE_CHUNKS {
        radix_powers(radix, levels)
    } else {
        Vec::new()
    };
    if aborted() {
        return Ok(Some(Vec::new()));
    }
    Ok(Some(parse_dc(digits, radix, k, &pows)))
}

/// Divide and conquer: the high digits' value times `radix^(k·2^l)` plus the low `k·2^l` digits'.
fn parse_dc(digits: &[u8], radix: u32, k: usize, pows: &[Vec<u64>]) -> Vec<u64> {
    if poll() {
        return Vec::new();
    }
    if digits.len() <= k * PARSE_BASE_CHUNKS {
        // Accumulate `k` digits at a time into one limb, then `acc = acc * radix^k + chunk`.
        let mut acc: Vec<u64> = Vec::with_capacity(digits.len() / k + 1);
        let first = match digits.len() % k {
            0 => k,
            n => n,
        };
        let mut start = 0;
        let mut len = first;
        while start < digits.len() {
            let (mut chunk, mut scale) = (0u64, 1u64);
            for &d in &digits[start..start + len] {
                chunk = chunk * radix as u64 + d as u64;
                scale *= radix as u64;
            }
            let carry = mac_small(&mut acc, scale, chunk);
            if carry != 0 {
                acc.push(carry);
            }
            start += len;
            len = k;
        }
        return trim(acc);
    }
    let l = (0..pows.len()).rev().find(|&l| (k << l) < digits.len()).unwrap();
    let split = digits.len() - (k << l);
    let hi = parse_dc(&digits[..split], radix, k, pows);
    let lo = parse_dc(&digits[split..], radix, k, pows);
    let mut out = mag_mul(&hi, &pows[l]);
    if out.len() < lo.len() {
        out.resize(lo.len(), 0);
    }
    if add_into(&mut out, &lo) {
        out.push(1);
    }
    trim(out)
}

/// In-place two's-complement negation over `v.len()` limbs.
fn negate_twos(v: &mut [u64]) {
    let mut carry = true;
    for d in v.iter_mut() {
        let (x, c) = (!*d).overflowing_add(carry as u64);
        *d = x;
        carry = c;
    }
}

/// Clears every bit at or above `bits` (with `v.len() == ceil(bits / 64)`).
fn mask_bits(v: &mut [u64], bits: u64) {
    if bits % 64 != 0 {
        if let Some(top) = v.last_mut() {
            *top &= (1u64 << (bits % 64)) - 1;
        }
    }
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
    pub fn words(&self) -> (bool, &[u64]) { (self.0.neg, &self.0.mag) }
    pub fn is_negative(&self) -> bool {
        self.0.neg
    }
    pub fn limbs(&self) -> usize {
        self.0.mag.len()
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
    /// The magnitude as a u128, when it has at most two limbs.
    fn small_mag(&self) -> Option<u128> {
        match self.0.mag.as_slice() {
            [] => Some(0),
            [lo] => Some(*lo as u128),
            [lo, hi] => Some(((*hi as u128) << 64) | *lo as u128),
            _ => None,
        }
    }
    fn from_parts(neg: bool, mag: u128) -> Self {
        Self::make(neg, vec![mag as u64, (mag >> 64) as u64])
    }

    pub fn mul(&self, o: &Self) -> Self {
        if let ([x], [y]) = (self.0.mag.as_slice(), o.0.mag.as_slice()) {
            return Self::from_parts(self.0.neg != o.0.neg, *x as u128 * *y as u128);
        }
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
        if let (Some(x), Some(y)) = (self.small_mag(), o.small_mag()) {
            return Some(Self::from_parts(self.0.neg != o.0.neg, x / y));
        }
        let (q, _) = mag_divmod(&self.0.mag, &o.0.mag, true);
        Some(Self::make(self.0.neg != o.0.neg, q))
    }
    /// Remainder with the dividend's sign; `None` on division by zero.
    pub fn rem(&self, o: &Self) -> Option<Self> {
        if o.is_zero() {
            return None;
        }
        if let (Some(x), Some(y)) = (self.small_mag(), o.small_mag()) {
            return Some(Self::from_parts(self.0.neg, x % y));
        }
        let (_, r) = mag_divmod(&self.0.mag, &o.0.mag, false);
        Some(Self::make(self.0.neg, r))
    }
    /// Exponentiation, refusing results past [`MAX_BITS`] before computing them.
    pub fn pow(&self, o: &Self) -> Result<Self, BigIntError> {
        if o.0.neg {
            return Err(BigIntError::NegativeExponent);
        }
        if o.is_zero() {
            return Ok(Self::from_u64(1));
        }
        let odd = o.0.mag[0] & 1 == 1;
        match self.0.mag.as_slice() {
            [] => return Ok(Self::zero()),
            [1] => return Ok(if self.0.neg && !odd { self.neg() } else { self.clone() }),
            _ => {}
        }
        let bits = self.bit_len() as u64;
        let e = match o.to_i128() {
            Some(e) if (e as u128) <= MAX_BITS as u128 => e as u64,
            _ => return Err(BigIntError::TooLarge),
        };
        // |self|^e has more than (bits − 1)·e bits; estimate it from log2 |self|.
        let top = self.0.mag.len().min(2);
        let mut lead = 0f64;
        for &d in &self.0.mag[self.0.mag.len() - top..] {
            lead = lead * 18446744073709551616.0 + d as f64;
        }
        let log2 = lead.log2() + 64.0 * (self.0.mag.len() - top) as f64;
        if (bits - 1) * e >= MAX_BITS || log2 * e as f64 > MAX_BITS as f64 + 1.0 {
            return Err(BigIntError::TooLarge);
        }
        let neg = self.0.neg && odd;
        let tz = self.0.mag.iter().position(|&d| d != 0).unwrap();
        let shift = tz as u64 * 64 + self.0.mag[tz].trailing_zeros() as u64;
        if shift + 1 == bits {
            // A power of two: one shift.
            let r = Self::from_u64(1).shl(shift * e);
            return Ok(if neg { r.neg() } else { r });
        }
        let mut e = e;
        let mut base = Self::make(false, self.0.mag.clone());
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
        if neg {
            acc = acc.neg();
        }
        acc.bounded()
    }

    fn bounded(self) -> Result<Self, BigIntError> {
        if self.bit_len() as u64 > MAX_BITS {
            Err(BigIntError::TooLarge)
        } else {
            Ok(self)
        }
    }
    pub fn checked_add(&self, o: &Self) -> Result<Self, BigIntError> {
        self.add(o).bounded()
    }
    /// `self ± 1` (the `++` / `--` operators).
    pub fn checked_step(&self, inc: bool) -> Result<Self, BigIntError> {
        let one = Self::from_u64(1);
        if inc {
            self.checked_add(&one)
        } else {
            self.checked_sub(&one)
        }
    }
    pub fn checked_sub(&self, o: &Self) -> Result<Self, BigIntError> {
        self.sub(o).bounded()
    }
    pub fn checked_mul(&self, o: &Self) -> Result<Self, BigIntError> {
        if self.is_zero() || o.is_zero() {
            return Ok(Self::zero());
        }
        // The product has at least bits(a) + bits(b) − 1 bits.
        if (self.bit_len() + o.bit_len()) as u64 > MAX_BITS + 1 {
            return Err(BigIntError::TooLarge);
        }
        self.mul(o).bounded()
    }
    pub fn checked_div(&self, o: &Self) -> Result<Self, BigIntError> {
        self.div(o).ok_or(BigIntError::DivisionByZero)
    }
    pub fn checked_rem(&self, o: &Self) -> Result<Self, BigIntError> {
        self.rem(o).ok_or(BigIntError::DivisionByZero)
    }
    pub fn checked_shl(&self, count: u128) -> Result<Self, BigIntError> {
        if self.is_zero() {
            return Ok(Self::zero());
        }
        if self.bit_len() as u128 + count > MAX_BITS as u128 {
            return Err(BigIntError::TooLarge);
        }
        Ok(self.shl(count as u64))
    }
    /// `self mod 2^bits` (BigInt.asUintN).
    pub fn as_uint_n(&self, bits: u64) -> Result<Self, BigIntError> {
        if !self.0.neg && self.bit_len() as u64 <= bits {
            return Ok(self.clone());
        }
        if bits > MAX_BITS {
            // Only a negative value gets here, and it wraps to nearly 2^bits.
            return Err(BigIntError::TooLarge);
        }
        Ok(Self::make(false, self.low_twos(bits)))
    }
    /// `self` wrapped into the signed `bits`-wide range (BigInt.asIntN).
    pub fn as_int_n(&self, bits: u64) -> Self {
        if bits == 0 {
            return Self::zero();
        }
        if (self.bit_len() as u64) < bits {
            return self.clone();
        }
        // Here bits ≤ bit_len ≤ MAX_BITS.
        let mut v = self.low_twos(bits);
        let top = bits - 1;
        if v.get((top / 64) as usize).is_some_and(|&d| d >> (top % 64) & 1 == 1) {
            // Negative: the magnitude is the `bits`-wide two's complement of `v`.
            negate_twos(&mut v);
            mask_bits(&mut v, bits);
            Self::make(true, v)
        } else {
            Self::make(false, v)
        }
    }
    /// The low `bits` bits of the two's-complement representation, as limbs.
    fn low_twos(&self, bits: u64) -> Vec<u64> {
        let limbs = bits.div_ceil(64) as usize;
        let mut v: Vec<u64> = self.0.mag.iter().copied().take(limbs).collect();
        v.resize(limbs, 0);
        if self.0.neg {
            negate_twos(&mut v);
        }
        mask_bits(&mut v, bits);
        v
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

    /// Parse from digits (no sign) in the given radix; `None` when malformed or too large.
    pub fn parse_radix(text: &str, radix: u32) -> Option<Self> {
        Self::parse_radix_checked(text, radix).ok().flatten()
    }
    /// Parse from digits (no sign): `Ok(None)` when malformed, `Err` past [`MAX_BITS`].
    pub fn parse_radix_checked(text: &str, radix: u32) -> Result<Option<Self>, BigIntError> {
        Ok(parse_mag(text, radix)?.map(|mag| Self::make(false, mag)))
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
        let mut digits: Vec<u8> = Vec::with_capacity(self.max_digits(radix) + 1);
        if self.0.neg {
            digits.push(b'-');
        }
        mag_to_radix(&self.0.mag, radix, &mut digits);
        String::from_utf8(digits).expect("ASCII digits")
    }
    /// [`Self::to_string_radix`], or `None` when the text would exceed `max_len` characters
    /// (decided before converting, from the bit length).
    pub fn to_string_radix_checked(&self, radix: u32, max_len: usize) -> Option<String> {
        let sign = self.0.neg as usize;
        let min_digits = (self.bit_len().saturating_sub(1) as f64 / (radix as f64).log2()) as usize;
        if min_digits + sign > max_len {
            return None;
        }
        let s = self.to_string_radix(radix);
        (s.len() <= max_len).then_some(s)
    }
    /// An upper bound on the digit count in `radix`.
    fn max_digits(&self, radix: u32) -> usize {
        (self.bit_len() as f64 / (radix as f64).log2()) as usize + 2
    }
}

#[cfg(test)]
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
        assert_eq!(big("2").pow(&big("-1")), Err(super::BigIntError::NegativeExponent));
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

    fn xorshift(seed: u64) -> impl FnMut() -> u64 {
        let mut st = seed;
        move || {
            st ^= st << 13;
            st ^= st >> 7;
            st ^= st << 17;
            st
        }
    }

    /// Random limbs with a non-zero top, including runs of all-ones and zeros.
    fn rand_mag(next: &mut impl FnMut() -> u64, n: usize) -> Vec<u64> {
        let mode = next() % 4;
        let mut v: Vec<u64> = (0..n)
            .map(|_| match mode {
                0 => u64::MAX,
                1 if next() % 3 == 0 => 0,
                _ => next(),
            })
            .collect();
        if let Some(top) = v.last_mut() {
            *top |= 1 + (next() % 2) * (1 << 63);
        }
        v
    }

    /// Digits by repeated single-digit short division: the quadratic reference.
    fn naive_to_string(mag: &[u64], radix: u32) -> String {
        if mag.is_empty() {
            return "0".into();
        }
        let mut cur = mag.to_vec();
        let mut out = Vec::new();
        while !cur.is_empty() {
            let r = super::div_small_in_place(&mut cur, radix as u64);
            cur = super::trim(cur);
            out.push(std::char::from_digit(r as u32, radix).unwrap());
        }
        out.iter().rev().collect()
    }

    #[test]
    fn large_mul_div_and_radix_conversion_agree() {
        let mut next = xorshift(0xD1B5_4A32_D192_ED03);
        let sizes = [1usize, 2, 3, 39, 40, 41, 79, 80, 81, 150, 333, 799, 800, 999, 1000, 2100, 3000, 6100];
        for (i, &la) in sizes.iter().enumerate() {
            for &lb in &sizes[..=i] {
                let a = rand_mag(&mut next, la);
                let b = rand_mag(&mut next, lb);
                let p = super::mag_mul(&a, &b);
                if la.min(lb) >= super::KARATSUBA_LIMBS {
                    let mut school = vec![0u64; la + lb];
                    super::mul_school(&a, &b, &mut school);
                    assert_eq!(p, super::trim(school), "mul {la}x{lb}");
                }
                assert_eq!(super::mag_sqr(&a), super::mag_mul(&a, &a.clone()), "sqr {la}");
                // (a·b + r) / b == a, remainder r, for r < b.
                let r = super::mag_divmod(&rand_mag(&mut next, lb), &b, false).1;
                let n = super::mag_add(&p, &r);
                let (q, rr) = super::mag_divmod(&n, &b, true);
                assert_eq!(q, a, "div {}/{lb}", n.len());
                assert_eq!(rr, r, "rem {}/{lb}", n.len());
                assert_eq!(super::mag_divmod(&n, &b, false).1, r);
                let (q2, r2) = super::mag_divmod(&n, &a, true);
                assert_eq!(super::trim(super::mag_add(&super::mag_mul(&q2, &a), &r2)), n);
                assert_eq!(super::mag_cmp(&r2, &a), Ordering::Less);
            }
        }
        for &n in &[1usize, 39, 40, 41, 120, 700, 1600, 9000] {
            let x = JsBigInt::make(next() % 2 == 1, rand_mag(&mut next, n));
            for radix in [10, 7, 16, 36, 2, 3, 8, 32] {
                let s = x.to_string_radix(radix);
                let digits = s.trim_start_matches('-');
                if n <= 120 {
                    assert_eq!(digits, naive_to_string(&x.0.mag, radix), "to_string {n} r{radix}");
                }
                let back = JsBigInt::parse_radix(digits, radix).unwrap();
                assert_eq!(back, JsBigInt::make(false, x.0.mag.clone()), "parse {n} r{radix}");
                assert_eq!(x.to_string_radix_checked(radix, s.len()), Some(s.clone()));
                assert_eq!(x.to_string_radix_checked(radix, s.len() - 1), None);
            }
        }
        // Powers of the conversion base and runs of zeros pad correctly.
        let ten = JsBigInt::from_u64(10);
        let p = ten.pow(&JsBigInt::from_u64(5000)).unwrap();
        let s = p.to_string();
        assert_eq!(s.len(), 5001);
        assert!(s.starts_with('1') && s[1..].bytes().all(|c| c == b'0'));
        assert_eq!(p.sub(&JsBigInt::from_u64(1)).to_string(), "9".repeat(5000));
        assert_eq!(JsBigInt::parse_radix(&"9".repeat(5000), 10), Some(p.sub(&JsBigInt::from_u64(1))));
        assert_eq!(JsBigInt::parse_radix(&format!("000{s}"), 10), Some(p));
        assert_eq!(JsBigInt::parse_radix("0000", 10), Some(JsBigInt::zero()));
    }

    #[test]
    fn newton_reciprocal_is_exact() {
        let mut next = xorshift(0x5DEE_CE66_D1CE_4E5B);
        for n in [201usize, 202, 203, 300, 777, 2500] {
            for _ in 0..3 {
                let mut d = rand_mag(&mut next, n);
                d[n - 1] |= 1 << 63;
                let num = super::power_of_base(2 * n);
                assert_eq!(super::reciprocal(&d), super::bz_divmod(&num, &d).0, "n={n}");
            }
        }
    }

    #[test]
    fn size_cap_rejects_before_allocating() {
        use super::{BigIntError::TooLarge, MAX_BITS};
        let two = JsBigInt::from_u64(2);
        let e = |n: u64| JsBigInt::from_u64(n);
        assert_eq!(two.pow(&e(1 << 33)), Err(TooLarge));
        assert_eq!(two.pow(&e(1 << 40)), Err(TooLarge));
        assert_eq!(two.pow(&e(MAX_BITS)), Err(TooLarge));
        assert_eq!(JsBigInt::from_u64(3).pow(&e(1 << 31)), Err(TooLarge));
        assert_eq!(JsBigInt::from_u64(3).pow(&e(700_000_000)), Err(TooLarge));
        assert_eq!(two.pow(&JsBigInt::from_u64(u64::MAX).shl(70)), Err(TooLarge));
        assert_eq!(JsBigInt::from_i128(-1).pow(&e(u64::MAX)).unwrap().to_string(), "-1");
        assert_eq!(JsBigInt::from_i128(-1).pow(&e(1 << 40)).unwrap().to_string(), "1");
        assert_eq!(JsBigInt::zero().pow(&e(1 << 40)).unwrap(), JsBigInt::zero());
        let top = two.pow(&e(MAX_BITS - 1)).unwrap();
        assert_eq!(top.bit_len() as u64, MAX_BITS);
        assert_eq!(top.checked_mul(&two), Err(TooLarge));
        assert_eq!(top.checked_add(&top), Err(TooLarge));
        assert_eq!(top.checked_shl(1), Err(TooLarge));
        assert_eq!(JsBigInt::from_u64(1).checked_shl(MAX_BITS as u128), Err(TooLarge));
        assert_eq!(two.pow(&e(1000)).unwrap().to_string_radix(16), format!("1{}", "0".repeat(250)));
        assert_eq!(JsBigInt::from_i128(-8).pow(&e(3)).unwrap().to_string(), "-512");
        assert_eq!(JsBigInt::from_u64(1).as_uint_n(MAX_BITS + 1).unwrap().to_string(), "1");
        assert_eq!(JsBigInt::from_i128(-1).as_uint_n(MAX_BITS + 1), Err(TooLarge));
        assert_eq!(JsBigInt::from_i128(-1).as_int_n(1 << 40).to_string(), "-1");
        let digits = (MAX_BITS as f64 / 10f64.log2()) as usize + 2;
        assert_eq!(JsBigInt::parse_radix_checked(&"9".repeat(digits), 10), Err(TooLarge));
    }

    #[test]
    fn as_int_n_and_as_uint_n_wrap() {
        let cases: [(i128, u64, &str, &str); 8] = [
            (258, 8, "2", "2"),
            (255, 8, "255", "-1"),
            (-1, 8, "255", "-1"),
            (-128, 8, "128", "-128"),
            (128, 8, "128", "-128"),
            (-1, 64, "18446744073709551615", "-1"),
            (1 << 64, 64, "0", "0"),
            (-5, 0, "0", "0"),
        ];
        for (v, bits, u, s) in cases {
            let n = JsBigInt::from_i128(v);
            assert_eq!(n.as_uint_n(bits).unwrap().to_string(), u, "asUintN({bits}, {v})");
            assert_eq!(n.as_int_n(bits).to_string(), s, "asIntN({bits}, {v})");
        }
    }
}
