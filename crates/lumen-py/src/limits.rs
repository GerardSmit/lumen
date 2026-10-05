//! Python's resource limits on top of `lumen_common::limits`: the size caps for allocations a
//! script can request, the interrupt flag, the optional heap budget and CPython's integer/string
//! digit limit.

use crate::object::*;
use crate::vm::{dict_get_str, Interp};
use lumen_common::bigint::digits_exceed;
use lumen_common::limits::{size, HeapBudget, InterruptHandle};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Longest list or tuple a script may build in one operation (16-byte values, so 1 GiB).
pub const MAX_SEQ_LEN: usize = 1 << 26;
/// Longest str, in UTF-8 bytes, a script may build in one operation.
pub const MAX_STR_LEN: usize = 1 << 30;
/// Longest bytes or bytearray a script may build in one operation.
pub const MAX_BYTES_LEN: usize = 1 << 30;

/// The smallest non-zero value `sys.set_int_max_str_digits` accepts.
pub const INT_MAX_STR_DIGITS_THRESHOLD: usize = 640;

/// Exit status of a script stopped by an interrupt (the shell convention for SIGINT).
pub const EXIT_INTERRUPTED: i32 = 130;

pub use crate::digits::{digit_limit_message, DEFAULT_INT_MAX_STR_DIGITS};
pub(crate) use crate::digits::{literal_digit_limit, with_literal_digit_limit};

impl Interp {
    pub fn interrupt_handle(&self) -> InterruptHandle {
        self.interrupt.clone()
    }

    /// Replaces the interrupt flag with one the embedder owns.
    pub fn set_interrupt(&mut self, flag: Arc<AtomicBool>) {
        self.interrupt = InterruptHandle::from_flag(flag);
    }

    /// Caps the bytes the script may hold on the heap, measured from now; exceeding it raises
    /// `MemoryError`. Zero removes the cap.
    pub fn set_heap_limit(&mut self, bytes: usize) {
        self.heap = HeapBudget::thread(bytes);
    }

    /// True when the last run ended because of an interrupt rather than the script's own exit.
    pub fn was_interrupted(&self) -> bool {
        self.interrupted
    }

    /// Cheap check run at backward jumps, calls and inside long native loops.
    #[inline(always)]
    pub fn poll(&mut self) -> R<()> {
        if self.interrupt.is_interrupted() {
            return Err(self.interrupt_exc());
        }
        crate::builtins::signalm::check(self)?;
        if crate::gc::due() {
            self.gc_auto();
        }
        if self.heap.is_set() {
            return self.check_heap(0);
        }
        Ok(())
    }

    #[cold]
    #[inline(never)]
    pub(crate) fn interrupt_exc(&mut self) -> Obj {
        let cls = self.exc_type("KeyboardInterrupt");
        self.new_exc(&cls, Vec::new())
    }

    pub fn memory_error(&mut self) -> Obj {
        self.new_exc_str("MemoryError", "")
    }

    fn check_heap(&mut self, extra: usize) -> R<()> {
        if self.heap.exceeded_by(extra) {
            if self.gc_reclaim() && !self.heap.exceeded_by(extra) {
                return Ok(());
            }
            return Err(self.memory_error());
        }
        Ok(())
    }

    /// Fails with `MemoryError` before an allocation of `elems` units of `unit` bytes that is
    /// past `max` units or would overrun the heap budget.
    pub fn check_alloc(&mut self, elems: usize, unit: usize, max: usize) -> R<()> {
        size::check(elems, max).map_err(|_| self.memory_error())?;
        if self.heap.is_set() {
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
        size::vec_with_capacity(n, max).map_err(|_| self.memory_error())
    }

    pub fn string_with_capacity(&mut self, n: usize) -> R<String> {
        self.check_str_len(n)?;
        size::string_with_capacity(n, MAX_STR_LEN).map_err(|_| self.memory_error())
    }

    /// Rejects a big-integer result of about `bits` bits before it is computed.
    pub fn check_int_bits(&mut self, bits: u128) -> R<()> {
        if bits > lumen_common::bigint::MAX_BITS as u128 {
            return Err(self.memory_error());
        }
        if self.heap.is_set() {
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
            if let (Some(flags), Value::Obj(name)) =
                (dict_get_str(&d, "flags"), Value::str("int_max_str_digits"))
            {
                let _ = self.set_attr(&flags, &name, Value::Int(n as i64));
            }
        }
        true
    }

    /// Decimal text of `n`, or `ValueError` when it has more digits than the limit allows.
    pub fn int_to_decimal(&mut self, n: &crate::pyint::BigInt) -> R<String> {
        let limit = self.int_max_str_digits;
        match n.to_decimal_limited(limit) {
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
        if digits_exceed(limit, digits) {
            return Err(self.value_error(&digit_limit_message(limit, Some(digits))));
        }
        Ok(())
    }
}
