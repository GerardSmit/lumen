//! The interpreter loop's register-resident working state: the pc ([`PcReg`]) and the operand
//! stack ([`VmStack`]). Both are plain locals of `run_vm_frames` that write themselves back to
//! the running frame (its `pc` slot, its `Vec<Value>` length) whenever the loop leaves by any
//! path — `return`, `?`, or unwinding — so everything outside the loop keeps seeing the frame's
//! ordinary fields. Inside the loop nothing goes through the frame record: LLVM can keep the pc
//! and the stack pointer in registers instead of storing and reloading them on every op (a
//! store-to-load round trip on the critical path of each dispatch).

use crate::value::Value;

/// `run_vm_frames`' working pc: written back to the running frame's pc slot `dst` on drop, so
/// callers (`drive_vm`, coroutines, the JIT's single-step fallback) see the pc of the op after
/// the last one dispatched, exactly as when the loop wrote through `dst`. Derefs to the `usize`
/// so handlers read and write it as `*pc`.
pub(super) struct PcReg {
    pub(super) v: usize,
    pub(super) dst: *mut usize,
}

impl std::ops::Deref for PcReg {
    type Target = usize;
    #[inline(always)]
    fn deref(&self) -> &usize {
        &self.v
    }
}

impl std::ops::DerefMut for PcReg {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut usize {
        &mut self.v
    }
}

impl Drop for PcReg {
    #[inline(always)]
    fn drop(&mut self) {
        // SAFETY: `dst` is re-pointed at the running frame's pc on every frame switch (and before
        // any fallible step after the frame records may have moved), so it is live here.
        unsafe { *self.dst = self.v };
    }
}

/// The running frame's operand stack as three raw pointers into its `Vec<Value>`'s buffer:
/// `[base, sp)` are the live values, `[sp, end)` spare capacity. The `Vec`'s own length is stale
/// while the loop runs; [`VmStack::sync`] (and `Drop`) writes it back, [`VmStack::reload`]
/// re-reads the `Vec` after something else changed it. The buffer itself only moves when a push
/// outgrows it ([`VmStack::grow`]) — never behind the loop's back, since nothing else touches a
/// running frame's stack.
///
/// The method names mirror `Vec`'s so handler code reads the same.
pub(super) struct VmStack {
    sp: *mut Value,
    base: *mut Value,
    end: *mut Value,
    vec: *mut Vec<Value>,
}

impl VmStack {
    /// View `vec` (which must outlive the view or be re-pointed with [`VmStack::repoint`]).
    #[inline(always)]
    pub(super) unsafe fn new(vec: *mut Vec<Value>) -> VmStack {
        let mut s = VmStack {
            sp: std::ptr::null_mut(),
            base: std::ptr::null_mut(),
            end: std::ptr::null_mut(),
            vec,
        };
        s.reload();
        s
    }

    /// Switch to another frame's `vec` (the current one must already be synced).
    #[inline(always)]
    pub(super) unsafe fn attach(&mut self, vec: *mut Vec<Value>) {
        self.vec = vec;
        self.reload();
    }

    /// The same buffer's `Vec` header moved (its frame record was relocated): follow it.
    #[inline(always)]
    pub(super) fn repoint(&mut self, vec: *mut Vec<Value>) {
        self.vec = vec;
    }

    /// Re-read the `Vec` (after code outside the view pushed, popped or reallocated it).
    #[inline(always)]
    pub(super) unsafe fn reload(&mut self) {
        let v = &mut *self.vec;
        self.base = v.as_mut_ptr();
        self.sp = self.base.add(v.len());
        self.end = self.base.add(v.capacity());
    }

    /// Write the length back to the `Vec`.
    #[inline(always)]
    pub(super) fn sync(&mut self) {
        // SAFETY: `[base, sp)` are initialized values of `vec`'s buffer.
        unsafe { (*self.vec).set_len(self.len()) };
    }

    /// The synced `Vec`, for code that needs the real thing; call [`VmStack::reload`] after
    /// changing it through this.
    #[inline(always)]
    pub(super) fn vec(&mut self) -> &mut Vec<Value> {
        self.sync();
        unsafe { &mut *self.vec }
    }

    /// Run `f` on the synced `Vec`, then pick up whatever it did.
    #[inline(always)]
    pub(super) fn with_vec<R>(&mut self, f: impl FnOnce(&mut Vec<Value>) -> R) -> R {
        self.sync();
        let r = f(unsafe { &mut *self.vec });
        unsafe { self.reload() };
        r
    }

    #[inline(always)]
    pub(super) fn len(&self) -> usize {
        // SAFETY: both point into the same buffer, `sp >= base`.
        unsafe { self.sp.offset_from(self.base) as usize }
    }

    #[cold]
    #[inline(never)]
    fn grow(&mut self) {
        self.sync();
        unsafe {
            (*self.vec).reserve(16);
            self.reload();
        }
    }

    #[inline(always)]
    pub(super) fn push(&mut self, v: Value) {
        if self.sp == self.end {
            self.grow();
        }
        unsafe {
            std::ptr::write(self.sp, v);
            self.sp = self.sp.add(1);
        }
    }

    /// Push a Number as one full-width store (see [`write_num`]).
    #[inline(always)]
    pub(super) fn push_num(&mut self, x: f64) {
        if self.sp == self.end {
            self.grow();
        }
        unsafe {
            write_num(self.sp, x);
            self.sp = self.sp.add(1);
        }
    }

    /// Push a payload-free value by tag as one full-width store (see [`write_tag`]).
    #[inline(always)]
    pub(super) fn push_tag(&mut self, tag: u8) {
        if self.sp == self.end {
            self.grow();
        }
        unsafe {
            write_tag(self.sp, tag);
            self.sp = self.sp.add(1);
        }
    }

    /// Push a Boolean as one full-width store (see [`write_bool`]).
    #[inline(always)]
    pub(super) fn push_bool(&mut self, b: bool) {
        if self.sp == self.end {
            self.grow();
        }
        unsafe {
            write_bool(self.sp, b);
            self.sp = self.sp.add(1);
        }
    }

    #[inline(always)]
    pub(super) fn pop(&mut self) -> Option<Value> {
        if self.sp == self.base {
            return None;
        }
        unsafe {
            self.sp = self.sp.sub(1);
            Some(std::ptr::read(self.sp))
        }
    }

    /// Pop the top value; underflow (a compiler bug) panics. Unlike `pop()`, the value moves
    /// as one 16-byte load: `Option<Value>` uses the tag's niche, so going through it copies the
    /// tag byte and then bytes 1..16 with misaligned loads, which never forward from the
    /// full-width store that pushed the value.
    #[inline(always)]
    pub(super) fn pop_val(&mut self) -> Value {
        if self.sp == self.base {
            underflow();
        }
        unsafe {
            self.sp = self.sp.sub(1);
            std::ptr::read(self.sp)
        }
    }

    /// Drop the top value (inline fast path for the refcount-free tags).
    #[inline(always)]
    pub(super) fn discard(&mut self) {
        let v = self.pop_val();
        drop_fast(v);
    }

    #[inline(always)]
    pub(super) fn last(&self) -> Option<&Value> {
        if self.sp == self.base {
            None
        } else {
            unsafe { Some(&*self.sp.sub(1)) }
        }
    }

    #[inline(always)]
    pub(super) fn last_mut(&mut self) -> Option<&mut Value> {
        if self.sp == self.base {
            None
        } else {
            unsafe { Some(&mut *self.sp.sub(1)) }
        }
    }

    /// The `k`-th value from the top (`0` = top). Panics past the bottom.
    #[inline(always)]
    pub(super) fn peek(&self, k: usize) -> &Value {
        assert!(k < self.len(), "vm stack underflow");
        unsafe { &*self.sp.sub(k + 1) }
    }

    /// Pointer to the `k`-th value from the top (`0` = top); the caller checked the depth.
    #[inline(always)]
    pub(super) unsafe fn top_ptr(&self, k: usize) -> *mut Value {
        self.sp.sub(k + 1)
    }

    /// Pop `n` values the caller already consumed or proved refcount-free (no drops run).
    #[inline(always)]
    pub(super) unsafe fn forget_top(&mut self, n: usize) {
        self.sp = self.sp.sub(n);
    }

    #[inline(always)]
    pub(super) fn truncate(&mut self, n: usize) {
        while self.len() > n {
            unsafe {
                self.sp = self.sp.sub(1);
                drop_fast(std::ptr::read(self.sp));
            }
        }
    }

    #[inline]
    pub(super) fn split_off(&mut self, at: usize) -> Vec<Value> {
        self.with_vec(|v| v.split_off(at))
    }

    #[inline(always)]
    pub(super) fn as_slice(&self) -> &[Value] {
        unsafe { std::slice::from_raw_parts(self.base, self.len()) }
    }

    #[inline(always)]
    pub(super) fn as_mut_slice(&mut self) -> &mut [Value] {
        unsafe { std::slice::from_raw_parts_mut(self.base, self.len()) }
    }
}

#[cold]
#[inline(never)]
fn underflow() -> ! {
    panic!("vm stack underflow")
}

impl Drop for VmStack {
    #[inline(always)]
    fn drop(&mut self) {
        self.sync();
    }
}

impl<I: std::slice::SliceIndex<[Value]>> std::ops::Index<I> for VmStack {
    type Output = I::Output;
    #[inline(always)]
    fn index(&self, i: I) -> &I::Output {
        &self.as_slice()[i]
    }
}

impl<I: std::slice::SliceIndex<[Value]>> std::ops::IndexMut<I> for VmStack {
    #[inline(always)]
    fn index_mut(&mut self, i: I) -> &mut I::Output {
        &mut self.as_mut_slice()[i]
    }
}

/// Drop a `Value`, skipping the out-of-line drop glue for the refcount-free tags `0..=4`.
#[inline(always)]
pub(super) fn drop_fast(v: Value) {
    if matches!(
        v,
        Value::Undefined | Value::Empty | Value::Null | Value::Bool(_) | Value::Num(_)
    ) {
        std::mem::forget(v);
    } else {
        drop(v);
    }
}

/// Store `Value::Num(x)` at `dst` (whose old contents are overwritten without being dropped) as
/// ONE 16-byte store. Writing the enum field by field (tag byte, then payload) makes the next
/// full-width load of the value — a clone, a pop into a temporary — miss store forwarding and
/// stall until both stores retire.
#[inline(always)]
pub(super) unsafe fn write_num(dst: *mut Value, x: f64) {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::*;
        // Value layout (`repr(u8)`): tag byte at 0 (Num = 4), f64 payload at 8.
        _mm_storeu_si128(
            dst as *mut __m128i,
            _mm_set_epi64x(x.to_bits() as i64, 4),
        );
    }
    #[cfg(not(target_arch = "x86_64"))]
    std::ptr::write(dst, Value::Num(x));
}

/// [`write_num`] for `Value::Bool(b)` (tag 3 at byte 0, the bool at byte 1).
#[inline(always)]
pub(super) unsafe fn write_bool(dst: *mut Value, b: bool) {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::*;
        _mm_storeu_si128(dst as *mut __m128i, _mm_set_epi64x(0, 3 | ((b as i64) << 8)));
    }
    #[cfg(not(target_arch = "x86_64"))]
    std::ptr::write(dst, Value::Bool(b));
}

/// Overwrite a slot holding anything with a Number: the old value's drop glue only runs for the
/// refcounted tags, then one full-width store.
#[inline(always)]
pub(super) fn set_num(slot: &mut Value, x: f64) {
    if !matches!(
        slot,
        Value::Undefined | Value::Empty | Value::Null | Value::Bool(_) | Value::Num(_)
    ) {
        unsafe { std::ptr::drop_in_place(slot) };
    }
    unsafe { write_num(slot, x) };
}

// Layout facts `write_num`/`write_bool` rely on.
const _: () = {
    assert!(std::mem::size_of::<Value>() == 16);
    assert!(std::mem::align_of::<Value>() == 8);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_width_writes_match_the_enum() {
        let mut v = Value::Undefined;
        unsafe { write_num(&mut v, 2.5) };
        assert!(matches!(v, Value::Num(x) if x == 2.5));
        unsafe { write_bool(&mut v, true) };
        assert!(matches!(v, Value::Bool(true)));
        unsafe { write_bool(&mut v, false) };
        assert!(matches!(v, Value::Bool(false)));
        let mut s = Value::str("x");
        set_num(&mut s, -1.0);
        assert!(matches!(s, Value::Num(x) if x == -1.0));
    }

    #[test]
    fn stack_view_round_trips() {
        let mut vec: Vec<Value> = vec![Value::Num(1.0)];
        {
            let mut s = unsafe { VmStack::new(&mut vec) };
            for k in 0..100 {
                s.push_num(k as f64);
            }
            s.push(Value::str("a"));
            assert_eq!(s.len(), 102);
            assert!(matches!(s.pop(), Some(Value::Str(_))));
            s.truncate(50);
            assert!(matches!(s[49], Value::Num(x) if x == 48.0));
            let tail = s.split_off(40);
            assert_eq!(tail.len(), 10);
            assert_eq!(s.len(), 40);
        }
        assert_eq!(vec.len(), 40);
    }
}

/// Dynamic op / op-pair counts for superinstruction selection (scratch builds only:
/// `RUSTFLAGS="--cfg lumen_op_stats"`; written to `LUMEN_OP_STATS` every 2^22 ops).
#[cfg(lumen_op_stats)]
pub(super) fn op_stat(op: &super::Op) {
    const N: usize = 160;
    static mut PAIRS: [[u64; N]; N] = [[0; N]; N];
    static mut PREV: usize = 0;
    static mut TICK: u64 = 0;
    // SAFETY: scratch instrumentation; races only perturb counts.
    unsafe {
        let k = (*(op as *const super::Op as *const u8) as usize).min(N - 1);
        PAIRS[PREV][k] += 1;
        PREV = k;
        TICK += 1;
        if TICK & ((1 << 22) - 1) == 0 {
            if let Some(path) = std::env::var_os("LUMEN_OP_STATS") {
                let mut s = String::new();
                for a in 0..N {
                    for b in 0..N {
                        let c = PAIRS[a][b];
                        if c != 0 {
                            s.push_str(&format!("{a} {b} {c}\n"));
                        }
                    }
                }
                let _ = std::fs::write(path, s);
            }
        }
    }
}

/// `v.clone()` with the refcount-free tags (`0..=4`, the common case for locals and constants)
/// as one tag compare and a plain 16-byte copy — `Clone`'s per-variant match compiles to a jump
/// table, an extra indirect branch on every local read.
#[inline(always)]
pub(super) fn clone_fast(v: &Value) -> Value {
    // SAFETY: `Value` is `repr(u8)`: the tag byte is at offset 0.
    let tag = unsafe { *(v as *const Value as *const u8) };
    if tag <= 4 {
        // SAFETY: tags 0..=4 own no resources — a bitwise copy is the clone.
        unsafe { std::ptr::read(v) }
    } else {
        v.clone()
    }
}

/// Store a payload-free value (`Undefined` = 0, `Empty` = 1, `Null` = 2) at `dst` as one
/// full-width store (see [`write_num`]; the enum write would store only the tag byte).
#[inline(always)]
pub(super) unsafe fn write_tag(dst: *mut Value, tag: u8) {
    debug_assert!(tag <= 2);
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::*;
        _mm_storeu_si128(dst as *mut __m128i, _mm_set_epi64x(0, tag as i64));
    }
    #[cfg(not(target_arch = "x86_64"))]
    std::ptr::write(
        dst,
        match tag {
            0 => Value::Undefined,
            1 => Value::Empty,
            _ => Value::Null,
        },
    );
}

/// `*slot = v` without reading the old value as a whole: only its tag is inspected, and its
/// drop glue runs (in place) just for the refcounted tags. A whole-value read of the old slot
/// is a misaligned piecewise load (tag, then bytes 1..16) that misses store forwarding from
/// the full-width store that last wrote the slot — and LLVM hoists it above the tag test.
#[inline(always)]
pub(super) fn set_slot(slot: &mut Value, v: Value) {
    // SAFETY: `repr(u8)`: tag at offset 0; the slot is initialized.
    unsafe {
        if *(slot as *const Value as *const u8) > 4 {
            std::ptr::drop_in_place(slot);
        }
        std::ptr::write(slot, v);
    }
}
