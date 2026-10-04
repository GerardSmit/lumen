//! The current thread's allocation category tag, read by the counting allocator
//! ([`crate::fastalloc`]) when the `mem-stats` feature is on.

use std::cell::Cell;

/// Stable allocation category id reserved for native HTML and DOM work.
///
/// Keep this id in the common allocator-tag layer so host crates that do not depend on the
/// Lumen engine can attribute their Rust-side allocations to the same category.
pub const HTML_CATEGORY_ID: u8 = 12;

/// A typed allocator category tag. The numeric id remains shared with the engine's report
/// categories, while callers cannot accidentally pass an unrelated integer to [`enter`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CategoryTag(u8);

impl CategoryTag {
    pub const HTML: Self = Self(HTML_CATEGORY_ID);

    pub const fn from_id(id: u8) -> Self {
        Self(id)
    }
}

/// Restores the previous allocation category when dropped.
#[must_use]
pub struct Guard {
    #[cfg(feature = "mem-stats")]
    previous: u8,
}

#[cfg(feature = "mem-stats")]
impl Drop for Guard {
    fn drop(&mut self) {
        replace(self.previous);
    }
}

/// Attribute allocations on this thread until the guard drops.
///
/// With `mem-stats` disabled this is an inline no-op and does not access the TLS category tag.
#[inline(always)]
pub fn enter(category: CategoryTag) -> Guard {
    #[cfg(feature = "mem-stats")]
    {
        Guard {
            previous: replace(category.0),
        }
    }
    #[cfg(not(feature = "mem-stats"))]
    {
        let _ = category;
        Guard {}
    }
}

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
