//! The current thread's allocation category tag, read by the counting allocator
//! ([`crate::fastalloc`]) when the `mem-stats` feature is on.

use std::cell::Cell;

thread_local! {
    static CUR_CAT: Cell<u8> = const { Cell::new(0) };
}

/// The current thread's allocation category tag.
#[inline(always)]
pub fn current() -> u8 {
    CUR_CAT.try_with(|c| c.get()).unwrap_or(0)
}

/// Set the current thread's category tag, returning the previous one.
#[inline(always)]
pub fn replace(cat: u8) -> u8 {
    CUR_CAT.try_with(|c| c.replace(cat)).unwrap_or(0)
}
