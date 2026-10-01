//! Resource limits: size caps for allocations a script can request, the interrupt flag, the
//! optional heap budget and CPython's integer/string digit limit.

use crate::object::*;
use crate::vm::{dict_get_str, Interp};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;

/// Longest list or tuple a script may build in one operation (16-byte values, so 1 GiB).
pub const MAX_SEQ_LEN: usize = 1 << 26;
/// Longest str, in UTF-8 bytes, a script may build in one operation.
pub const MAX_STR_LEN: usize = 1 << 30;
/// Longest bytes or bytearray a script may build in one operation.
pub const MAX_BYTES_LEN: usize = 1 << 30;

/// CPython's default for `sys.get_int_max_str_digits()`.
pub const DEFAULT_INT_MAX_STR_DIGITS: usize = 4300;
/// The smallest non-zero value `sys.set_int_max_str_digits` accepts.
pub const INT_MAX_STR_DIGITS_THRESHOLD: usize = 640;

/// Exit status of a script stopped by an interrupt (the shell convention for SIGINT).
pub const EXIT_INTERRUPTED: i32 = 130;

/// Requests that a running [`Interp`] stop. Cloneable and `Send`, so a watchdog thread or a
/// signal handler can hold one. The script unwinds with `KeyboardInterrupt`; the request stays
/// raised until [`InterruptHandle::clear`] (or the run that honoured it ends).
#[derive(Clone, Debug, Default)]
pub struct InterruptHandle(pub(crate) Arc<AtomicBool>);

impl InterruptHandle {
    pub fn new() -> InterruptHandle {
        InterruptHandle::default()
    }

    pub fn interrupt(&self) {
        self.0.store(true, Relaxed);
    }

    pub fn is_interrupted(&self) -> bool {
        self.0.load(Relaxed)
    }

    pub fn clear(&self) {
        self.0.store(false, Relaxed);
    }
}

thread_local! {
    static LIVE: Cell<isize> = const { Cell::new(0) };
    static LITERAL_DIGITS: Cell<usize> = const { Cell::new(DEFAULT_INT_MAX_STR_DIGITS) };
}

/// A global allocator that counts live bytes per thread so [`Interp::set_heap_limit`] can be
/// exact. A binary opts in with `#[global_allocator] static A: lumen_py::limits::CountingAlloc =
/// lumen_py::limits::CountingAlloc;`; without it the limit only rejects single allocations
/// larger than the budget.
pub struct CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            LIVE.with(|c| c.set(c.get() + layout.size() as isize));
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            LIVE.with(|c| c.set(c.get() + layout.size() as isize));
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE.with(|c| c.set(c.get() - layout.size() as isize));
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            LIVE.with(|c| c.set(c.get() + new_size as isize - layout.size() as isize));
        }
        p
    }
}

/// Bytes the calling thread currently holds through [`CountingAlloc`] (zero when the binary
/// did not install it).
pub fn live_bytes() -> isize {
    LIVE.with(|c| c.get())
}

/// The digit limit the lexer applies to decimal integer literals while [`Interp::compile_source`]
/// is running.
pub(crate) fn literal_digit_limit() -> usize {
    LITERAL_DIGITS.with(|c| c.get())
}

pub(crate) fn with_literal_digit_limit<T>(limit: usize, f: impl FnOnce() -> T) -> T {
    let prev = LITERAL_DIGITS.with(|c| c.replace(limit));
    let out = f();
    LITERAL_DIGITS.with(|c| c.set(prev));
    out
}

pub fn digit_limit_message(limit: usize, found: Option<usize>) -> String {
    match found {
        Some(n) => format!(
            "Exceeds the limit ({limit} digits) for integer string conversion: value has {n} digits; use sys.set_int_max_str_digits() to increase the limit"
        ),
        None => format!(
            "Exceeds the limit ({limit} digits) for integer string conversion; use sys.set_int_max_str_digits() to increase the limit"
        ),
    }
}

impl Interp {
    pub fn interrupt_handle(&self) -> InterruptHandle {
        InterruptHandle(self.interrupt.clone())
    }

    /// Replaces the interrupt flag with one the embedder owns.
    pub fn set_interrupt(&mut self, flag: Arc<AtomicBool>) {
        self.interrupt = flag;
    }

    /// Caps the bytes the script may hold on the heap, measured from now; exceeding it raises
    /// `MemoryError`. Zero removes the cap.
    pub fn set_heap_limit(&mut self, bytes: usize) {
        self.heap_limit = if bytes == 0 { usize::MAX } else { bytes };
        self.heap_base = live_bytes();
    }

    /// True when the last run ended because of an interrupt rather than the script's own exit.
    pub fn was_interrupted(&self) -> bool {
        self.interrupted
    }

    /// Cheap check run at backward jumps, calls and inside long native loops.
    #[inline(always)]
    pub fn poll(&mut self) -> R<()> {
        if self.interrupt.load(Relaxed) {
            return Err(self.interrupt_exc());
        }
        if self.heap_limit != usize::MAX {
            return self.check_heap(0);
        }
        Ok(())
    }

    #[cold]
    #[inline(never)]
    pub(crate) fn interrupt_exc(&mut self) -> Obj {
        self.new_exc_str("KeyboardInterrupt", "")
    }

    pub fn memory_error(&mut self) -> Obj {
        self.new_exc_str("MemoryError", "")
    }

    fn check_heap(&mut self, extra: usize) -> R<()> {
        let used = (live_bytes() - self.heap_base).max(0) as usize;
        if used.saturating_add(extra) > self.heap_limit {
            return Err(self.memory_error());
        }
        Ok(())
    }

    /// Fails with `MemoryError` before an allocation of `elems` units of `unit` bytes that is
    /// past `max` units or would overrun the heap budget.
    pub fn check_alloc(&mut self, elems: usize, unit: usize, max: usize) -> R<()> {
        if elems > max {
            return Err(self.memory_error());
        }
        if self.heap_limit != usize::MAX {
            self.check_heap(elems.saturating_mul(unit))?;
        }
        Ok(())
    }

    pub fn check_seq_len(&mut self, n: usize) -> R<()> {
        self.check_alloc(n, std::mem::size_of::<Value>(), MAX_SEQ_LEN)
    }

    pub fn check_str_len(&mut self, n: usize) -> R<()> {
        self.check_alloc(n, 1, MAX_STR_LEN)
    }

    pub fn check_bytes_len(&mut self, n: usize) -> R<()> {
        self.check_alloc(n, 1, MAX_BYTES_LEN)
    }

    /// An empty vector with room for `n` items, or `MemoryError`.
    pub fn vec_with_capacity<T>(&mut self, n: usize, max: usize) -> R<Vec<T>> {
        self.check_alloc(n, std::mem::size_of::<T>().max(1), max)?;
        let mut v = Vec::new();
        if v.try_reserve_exact(n).is_err() {
            return Err(self.memory_error());
        }
        Ok(v)
    }

    pub fn string_with_capacity(&mut self, n: usize) -> R<String> {
        self.check_str_len(n)?;
        let mut s = String::new();
        if s.try_reserve_exact(n).is_err() {
            return Err(self.memory_error());
        }
        Ok(s)
    }

    /// Rejects a big-integer result of about `bits` bits before it is computed.
    pub fn check_int_bits(&mut self, bits: u128) -> R<()> {
        if bits > lumen_common::bigint::MAX_BITS as u128 {
            return Err(self.memory_error());
        }
        if self.heap_limit != usize::MAX {
            self.check_heap((bits / 8) as usize)?;
        }
        Ok(())
    }

    pub fn int_max_str_digits(&self) -> usize {
        self.int_max_str_digits
    }

    /// `sys.set_int_max_str_digits`; `0` means unlimited, anything else must be at least
    /// [`INT_MAX_STR_DIGITS_THRESHOLD`].
    pub fn set_int_max_str_digits(&mut self, n: usize) -> bool {
        if n != 0 && n < INT_MAX_STR_DIGITS_THRESHOLD {
            return false;
        }
        self.int_max_str_digits = n;
        if let Some(sys) = self.sys_module.clone() {
            let d = self.module_dict(&sys);
            if let (Some(flags), Value::Obj(name)) = (dict_get_str(&d, "flags"), Value::str("int_max_str_digits")) {
                let _ = self.set_attr(&flags, &name, Value::Int(n as i64));
            }
        }
        true
    }

    /// Decimal text of `n`, or `ValueError` when it has more digits than the limit allows.
    pub fn int_to_decimal(&mut self, n: &crate::pyint::BigInt) -> R<String> {
        let limit = self.int_max_str_digits;
        if limit == 0 {
            return Ok(n.to_string_radix(10));
        }
        match n.to_string_radix_checked(10, limit + n.is_negative() as usize) {
            Some(s) => Ok(s),
            None => {
                self.poll()?;
                Err(self.value_error(&digit_limit_message(limit, None)))
            }
        }
    }

    /// Fails with `ValueError` when parsing `digits` decimal digits would pass the limit.
    pub fn check_parse_digits(&mut self, digits: usize) -> R<()> {
        let limit = self.int_max_str_digits;
        if limit != 0 && digits > limit {
            return Err(self.value_error(&digit_limit_message(limit, Some(digits))));
        }
        Ok(())
    }
}
