//! The current thread's allocation category tag, read by the counting allocator
//! ([`crate::fastalloc`]) when the `mem-stats` feature is on.

use std::cell::Cell;

/// Stable allocation category id reserved for native HTML and DOM work.
///
/// Keep this id in the common allocator-tag layer so host crates that do not depend on the
/// Lumen engine can attribute their Rust-side allocations to the same category.
pub const HTML_CATEGORY_ID: u8 = 12;
/// ArrayBuffer, typed-array, Blob and other JS byte-store backing bytes.
pub const BUFFERS_CATEGORY_ID: u8 = 13;
/// HTTP/XHR/fetch bodies and the HTTP cache's reads.
pub const NETWORK_CATEGORY_ID: u8 = 14;
/// Decoded image pixels and the decode cache.
pub const IMAGE_CATEGORY_ID: u8 = 15;
/// Decoded audio buffers.
pub const AUDIO_CATEGORY_ID: u8 = 16;
/// Canvas surfaces and 2D-context buffers.
pub const CANVAS_CATEGORY_ID: u8 = 17;
/// JS string allocations.
pub const STRINGS_CATEGORY_ID: u8 = 18;
/// The embedding host's own structures.
pub const HOST_CATEGORY_ID: u8 = 19;
/// Object property storage: named-entry vectors owned by objects.
pub const PROPS_CATEGORY_ID: u8 = 20;
/// Array element storage (packed and classic dense arrays).
pub const ARRAYS_CATEGORY_ID: u8 = 21;
/// Scopes and environments: binding maps.
pub const SCOPES_CATEGORY_ID: u8 = 22;
/// Map, Set, WeakMap and WeakSet storage.
pub const COLLECTIONS_CATEGORY_ID: u8 = 23;
/// Closure and function-object metadata.
pub const FUNCTIONS_CATEGORY_ID: u8 = 24;
/// Shapes and shape transitions.
pub const SHAPES_CATEGORY_ID: u8 = 25;
/// Script source text handed over by the embedder.
pub const SOURCE_CATEGORY_ID: u8 = 26;
/// Ids a more specific category may replace: nothing more specific than "other", the
/// engine's `Runtime` phase and the embedder's thread-wide `Host` tag.
#[cfg(feature = "mem-stats")]
const GENERIC_IDS: [u8; 3] = [0, 9, HOST_CATEGORY_ID];

/// A typed allocator category tag. The numeric id remains shared with the engine's report
/// categories, while callers cannot accidentally pass an unrelated integer to [`enter`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CategoryTag(u8);

impl CategoryTag {
    pub const HTML: Self = Self(HTML_CATEGORY_ID);
    pub const BUFFERS: Self = Self(BUFFERS_CATEGORY_ID);
    pub const NETWORK: Self = Self(NETWORK_CATEGORY_ID);
    pub const IMAGE: Self = Self(IMAGE_CATEGORY_ID);
    pub const AUDIO: Self = Self(AUDIO_CATEGORY_ID);
    pub const CANVAS: Self = Self(CANVAS_CATEGORY_ID);
    pub const STRINGS: Self = Self(STRINGS_CATEGORY_ID);
    pub const HOST: Self = Self(HOST_CATEGORY_ID);
    pub const PROPS: Self = Self(PROPS_CATEGORY_ID);
    pub const ARRAYS: Self = Self(ARRAYS_CATEGORY_ID);
    pub const SCOPES: Self = Self(SCOPES_CATEGORY_ID);
    pub const COLLECTIONS: Self = Self(COLLECTIONS_CATEGORY_ID);
    pub const FUNCTIONS: Self = Self(FUNCTIONS_CATEGORY_ID);
    pub const SHAPES: Self = Self(SHAPES_CATEGORY_ID);
    pub const SOURCE: Self = Self(SOURCE_CATEGORY_ID);

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

/// Like [`enter`], but only while the current category is generic (`Other`, the engine's
/// `Runtime` phase or the embedder's `Host` tag): a string made while parsing stays under the parser's category.
#[inline(always)]
pub fn enter_if_generic(category: CategoryTag) -> Guard {
    #[cfg(feature = "mem-stats")]
    {
        let current = current();
        Guard {
            previous: if GENERIC_IDS.contains(&current) {
                replace(category.0)
            } else {
                current
            },
        }
    }
    #[cfg(not(feature = "mem-stats"))]
    {
        let _ = category;
        Guard {}
    }
}

/// Move the live block behind `v` to `category` without copying, for bytes that were allocated
/// before their purpose was known (a body read off the network that becomes an ArrayBuffer).
/// Nothing happens without `mem-stats`.
#[inline(always)]
pub fn retag_vec<T>(v: &Vec<T>, category: CategoryTag) {
    #[cfg(feature = "mem-stats")]
    crate::fastalloc::retag(
        v.as_ptr() as *const u8,
        v.capacity() * std::mem::size_of::<T>(),
        std::mem::align_of::<T>(),
        category.0,
    );
    #[cfg(not(feature = "mem-stats"))]
    let _ = (v, category);
}

/// [`retag_vec`] for a heap slice that was allocated exactly (a boxed slice's buffer).
#[inline(always)]
pub fn retag_slice<T>(s: &[T], category: CategoryTag) {
    #[cfg(feature = "mem-stats")]
    crate::fastalloc::retag(
        s.as_ptr() as *const u8,
        std::mem::size_of_val(s),
        std::mem::align_of::<T>(),
        category.0,
    );
    #[cfg(not(feature = "mem-stats"))]
    let _ = (s, category);
}

/// [`retag_vec`] for the allocation behind an `Rc<str>`: the reference counts plus the text,
/// rounded to the box's alignment.
#[inline(always)]
pub fn retag_rc_str(s: &std::rc::Rc<str>, category: CategoryTag) {
    #[cfg(feature = "mem-stats")]
    {
        const HEADER: usize = 2 * std::mem::size_of::<usize>();
        let align = std::mem::align_of::<usize>();
        crate::fastalloc::retag(
            unsafe { (std::rc::Rc::as_ptr(s) as *const u8).sub(HEADER) },
            (HEADER + s.len() + align - 1) & !(align - 1),
            align,
            category.0,
        );
    }
    #[cfg(not(feature = "mem-stats"))]
    let _ = (s, category);
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
