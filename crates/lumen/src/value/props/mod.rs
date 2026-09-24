//! Named property maps and their shared dense side storage.
use crate::value::{Property, Value};
pub(in crate::value) use entries::EntryVec;
use shapes::SHAPE_EMPTY;
use std::rc::Rc;
pub(in crate::value) use storage::{DenseBuffers, DenseStorage, INLINE_PACKED_CAPACITY};
mod access;
mod array_builder;
mod elements;
mod entries;
mod mirror;
mod mutation;
mod shapes;
mod storage;
#[cfg(test)]
mod tests;
pub(crate) use shapes::{bump_proto_epoch, fn_key, proto_epoch, shape_table_census};
pub(in crate::value) use shapes::{array_length_shape, Shape, ShapeTable};
/// Sizes for a heap census (`LUMEN_HEAP_CENSUS`): entries used / reserved, how many of them are
/// named (shape-keyed), whether a dense sidecar exists, and whether the shape is owned by this
/// object alone (its key list is then per-object memory).
pub struct PropsCensus {
    pub entries_len: usize,
    /// Owned capacity; zero for a map sharing a template's entry block.
    pub entries_cap: usize,
    pub entries_shared: bool,
    pub named_len: usize,
    pub has_sidecar: bool,
    pub owned_shape_keys: usize,
}

/// The per-function closure templates (see `ast::Function::fn_maps`).
#[derive(Clone)]
pub struct FnMaps {
    /// The function object's map: `length`, `name` and, for prototype-bearing kinds, a
    /// `prototype` placeholder patched per closure. Without the placeholder its entries are a
    /// shared block every closure of the function references (see [`EntryVec`]).
    pub(crate) fn_map: Props,
    /// The fresh `.prototype` object's map (`constructor` placeholder), for prototype-bearing
    /// kinds.
    pub(crate) proto_map: Option<Props>,
    /// `fn_map` with the one inferred name NamedEvaluation gave a closure of this function, so
    /// every later closure named the same way (the usual case: one site, one name) shares
    /// again instead of copying its entries out to overwrite `name`.
    pub(crate) named: std::cell::OnceCell<Box<(Rc<str>, Props)>>,
}

/// A property map. Keys live in the [`Shape`], values here.
///
/// `entries` has two regions: slots `0..shape.len()` hold the *named* properties, in shape key
/// order (`entries[i]` is the value of `shape.keys[i]`, so a (shape, slot) pair recorded by an
/// inline cache from one object is valid on every object of that shape); slots past the named
/// prefix hold array *elements* (canonical-index keys that skipped the shape transition — see
/// `elem_mode`), each addressed only through the dense sidecar `elems[n] = slot`. Inserting a
/// named key while elements exist moves the element at the new named slot to the end, so the
/// named prefix stays contiguous.
pub struct Props {
    pub(in crate::value) entries: EntryVec,
    /// This object serves (or once served) as some object's prototype: structural changes to it
    /// bump the global [`proto_epoch`], invalidating every property-*creation* inline cache
    /// (their fill-time chain walks proved "no hop shadows this name" — see
    /// [`crate::bytecode::IC_CREATE`]). Set by the creation-IC fill walk itself, one-way.
    pub(in crate::value) proto_flag: std::cell::Cell<bool>,
    /// The shape's id, duplicated from `shape_rc` for the inline caches (a 32-bit compare).
    /// `SHAPE_EMPTY` while `shape_rc` is `None`. Bumped to a child on new-key
    /// insert, to a fresh owned id on a structural removal or an owned-shape mutation. Only
    /// consulted for non-exotic objects and named keys — array shapes encode the named-key
    /// sequence only, so a shape match never says anything about elements.
    pub(in crate::value) shape: u32,
    /// The shape holding this map's ordered named keys (`None` = the empty shape). See
    /// [`shapes::Shape`] for shared vs owned.
    pub(in crate::value) shape_rc: Option<Rc<Shape>>,
    /// One nullable cold-sidecar pointer shared by the packed elements, dense slot map,
    /// numeric mirror and its flags. Ordinary named-property objects allocate none of it. Within the sidecar,
    /// `elems[n]` is the `entries` slot of canonical-index key `n`, or `NO_SLOT`; see
    /// `note_inserted` and `get_index`.
    pub(in crate::value) elems: DenseStorage,
    /// The raw-f64 read mirror of the dense elements lives in the sidecar too. While
    /// `mirror_flags & MIRROR_OK`: `mirror.len() == elems.len()`, and for every `n`:
    /// `mirror[n]` is [`MIRROR_HOLE`] exactly when `elems[n]` names no element, else the element
    /// is a plain writable data property whose value is `Num(mirror[n])`. Element reads become
    /// one indexed load (no entry chase, no tag check), and `MIRROR_ALL_I32` lets int loops skip
    /// the exactness guard entirely. Entries stay authoritative: fast writers
    /// dual-store through [`Props::set_index_value`]; any foreign `&mut` escape (`get_index_mut`,
    /// `get_mut` / `entry_at_mut` on an index key) invalidates the mirror instead of tracking it.
    /// Some canonical-index key lives ONLY as a named (shape) key (inserted too far past the
    /// dense frontier — see `note_inserted`): `elems` coverage is no longer proof of element
    /// absence, so the dense append/pop fast paths stand down. One-way (sparse arrays are rare
    /// and stay sparse).
    pub(in crate::value) has_far: std::cell::Cell<bool>,
    /// This `Props` belongs to an `Exotic::Array` object: canonical-index key inserts skip the
    /// shape transition and land in the element region. Array shapes encode the *named*-key
    /// sequence only — elements must not churn it: a stable shape is what lets
    /// `arr.push(..)`/`arr.length` sites cache at all.
    pub(in crate::value) elem_mode: std::cell::Cell<bool>,
    /// Slack tracking for `new`: when this map belongs to a constructor, the largest small
    /// named-property count its instances finished construction with (0 = unknown). Fresh
    /// instances reserve that many entry slots, so the creation-IC appends of `this.x = ...`
    /// never regrow. Lives in the map's padding byte; per closure, like the object identity it
    /// used to be keyed by.
    pub(in crate::value) ctor_capacity: std::cell::Cell<u8>,
}

/// See [`Props::mirror`].
pub(crate) const MIRROR_OK: u8 = 1;
pub(crate) const MIRROR_NO_HOLES: u8 = 2;
/// Every non-hole mirror value is an exact i32 (bit-identical through an i32 round trip, which
/// also excludes -0.0).
pub(crate) const MIRROR_ALL_I32: u8 = 4;
/// The mirror's hole sentinel: a quiet-NaN payload no arithmetic produces. A user CAN craft
/// this exact bit pattern (typed-array punning), so the write paths refuse to mirror it — it is
/// never stored as data, which is what makes reading it back as "absent" sound.
pub(crate) const MIRROR_HOLE: u64 = 0x7FF8_DEAD_0000_0001;

/// Exact-i32 (and not -0.0): the value survives an i32 round trip bit-identically.
#[inline]
pub(crate) fn f64_exact_i32(f: f64) -> bool {
    (f as i32 as f64).to_bits() == f.to_bits()
}

/// `elems` hole marker (also caps how many entries dense slots can address).
pub(super) const NO_SLOT: u32 = u32::MAX;

/// Byte offsets the optimizing tier's inline element / `length` reads rely on
/// (`bytecode::jit::layout`): `offset_of!` facts plus one measured `Rc<Shape>` distance (std's
/// `Rc` layout is not public). `props_*` are relative to the `Props`, `dense_*` to the
/// `DenseBuffers` the sidecar word points at.
pub(crate) struct PropsLayout {
    /// `EntryVec` pointer / `u32` length / `u32` capacity (capacity 0 with a length = shared,
    /// read-only block).
    pub entries_ptr: usize,
    pub entries_len: usize,
    pub entries_cap: usize,
    /// The `u32` shape id.
    pub shape: usize,
    /// `Option<Rc<Shape>>`: one word, null = the empty shape.
    pub shape_rc: usize,
    /// From the `Rc<Shape>` handle word to the shape's `u32` `len_slot` memo.
    pub shape_len_slot: usize,
    /// The sidecar: one word, null = no `DenseBuffers`.
    pub elems: usize,
    /// `Option<Box<Vec<Property>>>`: one word, null = not boxed-packed.
    pub dense_packed: usize,
    /// `InlinePacked`'s `u8` length and its slot array.
    pub dense_inline_len: usize,
    pub dense_inline_slots: usize,
    /// `Vec<u32>` slot map and `Vec<f64>` mirror (their inner layout is probed by the caller).
    pub dense_elems: usize,
    pub dense_mirror: usize,
    /// `u8` mirror flags.
    pub dense_mirror_flags: usize,
}

const _: () = {
    // Each is one nullable pointer word.
    assert!(std::mem::size_of::<Option<Rc<Shape>>>() == std::mem::size_of::<usize>());
    assert!(std::mem::size_of::<DenseStorage>() == std::mem::size_of::<usize>());
    assert!(std::mem::size_of::<Option<Box<Vec<Property>>>>() == std::mem::size_of::<usize>());
};

/// See [`PropsLayout`]. `None` when the `Rc<Shape>` probe is inconclusive.
pub(crate) fn jit_props_layout() -> Option<PropsLayout> {
    use std::mem::offset_of;
    // Measure handle word → `len_slot` on a real one-key shape whose `length` sits at slot 0.
    let mut probe = Props::new();
    probe.insert("length", Property::plain(Value::Num(0.0)));
    let rc = probe.shape_rc.as_ref()?;
    if rc.len_slot != 0 {
        return None;
    }
    let word = unsafe { *(rc as *const Rc<Shape> as *const usize) };
    let shape_len_slot = (std::ptr::addr_of!(rc.len_slot) as usize).wrapping_sub(word);
    if shape_len_slot > 4096 {
        return None;
    }
    Some(PropsLayout {
        entries_ptr: offset_of!(Props, entries) + entries::ENTRY_VEC_PTR,
        entries_len: offset_of!(Props, entries) + entries::ENTRY_VEC_LEN,
        entries_cap: offset_of!(Props, entries) + entries::ENTRY_VEC_CAP,
        shape: offset_of!(Props, shape),
        shape_rc: offset_of!(Props, shape_rc),
        shape_len_slot,
        elems: offset_of!(Props, elems),
        dense_packed: offset_of!(storage::DenseBuffers, packed),
        dense_inline_len: offset_of!(storage::DenseBuffers, inline_packed)
            + offset_of!(storage::InlinePacked, len),
        dense_inline_slots: offset_of!(storage::DenseBuffers, inline_packed)
            + offset_of!(storage::InlinePacked, slots),
        dense_elems: offset_of!(storage::DenseBuffers, elems),
        dense_mirror: offset_of!(storage::DenseBuffers, mirror),
        dense_mirror_flags: offset_of!(storage::DenseBuffers, mirror_flags),
    })
}

impl std::fmt::Debug for FnMaps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FnMaps")
            .field("fn_map", &self.fn_map)
            .field("proto_map", &self.proto_map)
            .finish()
    }
}

impl std::fmt::Debug for Props {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Props")
            .field("entries", &self.entries.len())
            .field("named", &self.named_len())
            .field("shape", &self.shape)
            .finish()
    }
}

impl Default for Props {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for Props {
    /// Closures clone their function's map template; literals clone per-site templates. Shared
    /// shapes are shared (one refcount bump); an owned shape is copied under a fresh id so the
    /// two objects never mutate one key list.
    fn clone(&self) -> Props {
        let shape_rc = self.clone_shape_rc();
        Props {
            entries: self.entries.clone(),
            proto_flag: std::cell::Cell::new(false),
            shape: shape_rc.as_ref().map_or(SHAPE_EMPTY, |s| s.id),
            shape_rc,
            elems: self.elems.clone(),
            has_far: std::cell::Cell::new(self.has_far.get()),
            elem_mode: std::cell::Cell::new(self.elem_mode.get()),
            ctor_capacity: std::cell::Cell::new(0),
        }
    }
}

impl Props {
    /// The shape for a copy of this map: shared shapes by reference, an owned one duplicated
    /// under a fresh id.
    fn clone_shape_rc(&self) -> Option<Rc<Shape>> {
        self.shape_rc.as_ref().map(|s| {
            if s.owned() {
                shapes::shape_owned_from(Some(s))
            } else {
                s.clone()
            }
        })
    }

    pub fn census(&self) -> PropsCensus {
        PropsCensus {
            entries_len: self.entries.len(),
            entries_cap: self.entries.capacity(),
            entries_shared: self.entries.is_shared(),
            named_len: self.named_len(),
            has_sidecar: self.elems.is_present(),
            owned_shape_keys: self
                .shape_rc
                .as_ref()
                .filter(|s| s.owned())
                .map_or(0, |s| s.len()),
        }
    }

    pub(crate) fn new() -> Props {
        Self::with_capacity(0)
    }

    pub(crate) fn with_capacity(capacity: usize) -> Props {
        Props {
            entries: EntryVec::with_capacity(capacity),
            shape: SHAPE_EMPTY,
            shape_rc: None,
            elems: DenseStorage::default(),
            proto_flag: std::cell::Cell::new(false),
            has_far: std::cell::Cell::new(false),
            elem_mode: std::cell::Cell::new(false),
            ctor_capacity: std::cell::Cell::new(0),
        }
    }

    /// Instantiate a compiler-proved plain-data object template with its final values.
    ///
    /// Cloning the whole template would clone every placeholder [`crate::value::PackedValue`] and then drop it
    /// again as the caller overwrote each slot. Object-heavy parsers do this millions of times.
    /// The shape is the reusable part; plain property descriptors are cheaper and safer to
    /// construct directly around the moved values.
    pub(crate) fn instantiate_plain<I>(&self, mut values: I) -> Props
    where
        I: ExactSizeIterator<Item = Value>,
    {
        assert_eq!(values.len(), self.entries.len(), "object-template arity");
        let mut entries = EntryVec::with_capacity(self.entries.len());
        entries.extend_exact(values.by_ref().map(Property::plain));
        debug_assert!(values.next().is_none());
        let shape_rc = self.clone_shape_rc();
        Props {
            entries,
            proto_flag: std::cell::Cell::new(false),
            shape: shape_rc.as_ref().map_or(SHAPE_EMPTY, |s| s.id),
            shape_rc,
            elems: self.elems.clone(),
            has_far: std::cell::Cell::new(self.has_far.get()),
            elem_mode: std::cell::Cell::new(self.elem_mode.get()),
            ctor_capacity: std::cell::Cell::new(0),
        }
    }

    /// Whether [`crate::value::Object::new_from_template`] may build an instance of this
    /// template in an object's inline slots: a plain named map with no element sidecar.
    #[inline]
    pub(crate) fn can_instantiate_inline(&self) -> bool {
        !self.elems.is_present() && !self.elem_mode.get()
    }

    /// An instance of this template with its shape but no entries yet (see
    /// [`Props::fill_plain`]).
    #[inline]
    pub(crate) fn instantiate_shell(&self) -> Props {
        let shape_rc = self.clone_shape_rc();
        Props {
            entries: EntryVec::new(),
            proto_flag: std::cell::Cell::new(false),
            shape: shape_rc.as_ref().map_or(SHAPE_EMPTY, |s| s.id),
            shape_rc,
            elems: DenseStorage::default(),
            has_far: std::cell::Cell::new(self.has_far.get()),
            elem_mode: std::cell::Cell::new(false),
            ctor_capacity: std::cell::Cell::new(0),
        }
    }

    /// Whether [`crate::value::Object::alloc_from_template`] can instantiate this template:
    /// a plain named map with no element sidecar, a shared (or empty) shape, and at most
    /// `inline` entries.
    #[inline]
    pub(crate) fn fast_template(&self, inline: usize) -> bool {
        self.entries.len() <= inline
            && !self.elems.is_present()
            && !self.elem_mode.get()
            && !self.shape_rc.as_ref().is_some_and(|s| s.owned())
    }

    /// An instance map of a [`fast_template`](Props::fast_template) whose `len` entries are
    /// already written at `buf` (a new box's inline slots, `cap` of them). Built by value
    /// inside the box write, so every field is one direct store.
    ///
    /// # Safety
    /// As [`EntryVec::inline_raw`]; `len` must equal this template's entry count.
    #[inline(always)]
    pub(in crate::value) unsafe fn instantiate_inline_raw(
        &self,
        buf: *mut crate::value::Property,
        len: usize,
        cap: usize,
    ) -> Props {
        Props {
            entries: EntryVec::inline_raw(buf, len, cap),
            proto_flag: std::cell::Cell::new(false),
            shape: self.shape,
            shape_rc: self.shape_rc.clone(),
            elems: DenseStorage::default(),
            has_far: std::cell::Cell::new(self.has_far.get()),
            elem_mode: std::cell::Cell::new(false),
            ctor_capacity: std::cell::Cell::new(0),
        }
    }

    /// Complete an [`instantiate_shell`](Props::instantiate_shell) map with its values, in
    /// shape order.
    #[inline]
    pub(crate) fn fill_plain<I>(&mut self, values: I)
    where
        I: ExactSizeIterator<Item = Value>,
    {
        assert_eq!(values.len(), self.named_len(), "object-template arity");
        assert_eq!(self.entries.len(), 0, "template instance already filled");
        self.entries.extend_exact(values.map(Property::plain));
    }

    /// Grow tiny property maps exactly: `Vec`'s default first allocation has room for four
    /// entries, while one- and two-property objects dominate real heaps. Past two entries
    /// resume geometric growth so larger maps retain amortized insertion.
    #[inline]
    pub(super) fn reserve_entry(&mut self) {
        if self.entries.len() == self.entries.capacity() {
            let additional = if self.entries.len() < 2 {
                1
            } else {
                self.entries.len()
            };
            self.entries.reserve_exact(additional);
        }
    }

    /// Turn the entries into a shared block (see [`EntryVec::make_shared`]): clones then share
    /// it and copy on their first write through this type. Only for templates whose entries
    /// are non-writable data — inline-cached stores write a writable slot in place after the
    /// shape check alone — and hold no object references, since the cycle collector counts
    /// each map's values as that object's own edges.
    pub(crate) fn share_entries(&mut self) {
        debug_assert!(self
            .entries
            .iter()
            .all(|p| !p.writable() && !p.accessor() && !matches!(p.value(), Value::Obj(_))));
        self.entries.make_shared();
    }

    /// Whether this map still holds `template`'s shared entry block untouched.
    pub(crate) fn shares_entries_with(&self, template: &Props) -> bool {
        self.entries.shares_with(&template.entries)
    }

    /// Mark this object as a live prototype (see `proto_flag`).
    #[inline]
    pub(crate) fn mark_proto(&self) {
        self.proto_flag.set(true);
    }

    /// Bump the creation-IC epoch if this object is a marked prototype (called by every
    /// structural mutation).
    #[inline]
    pub(super) fn note_structural(&self) {
        if self.proto_flag.get() {
            bump_proto_epoch();
        }
    }

    /// This map's shape id — the inline cache's structural validation token (see the `shape` field).
    #[inline]
    pub(crate) fn shape(&self) -> u32 {
        self.shape
    }

    /// Whether this map's shape is a shared (transition-tree) shape: only such shapes may be
    /// recorded as a creation IC's child, since a hit appends to the entries and switches to
    /// the recorded shape by id.
    #[inline]
    pub(crate) fn shape_is_shared(&self) -> bool {
        self.shape_rc.as_ref().is_none_or(|s| !s.owned())
    }

    /// The named keys, in slot order (materialises a shared shape's list on first use — see
    /// [`Shape::keys`]).
    #[inline]
    pub(super) fn shape_keys(&self) -> &[Rc<str>] {
        self.shape_rc.as_ref().map_or(&[], |s| s.keys())
    }

    /// Number of named (shape-keyed) entries: the length of the named prefix of `entries`.
    #[inline]
    pub(super) fn named_len(&self) -> usize {
        self.shape_rc.as_ref().map_or(0, |s| s.len())
    }

    /// The instance capacity learned for this constructor's map (see `ctor_capacity`).
    #[inline]
    pub(crate) fn construct_capacity_hint(&self) -> usize {
        self.ctor_capacity.get() as usize
    }

    /// Record an instance's final size (see [`Props::observed_instance_capacity`]).
    #[inline]
    pub(crate) fn note_construct_capacity(&self, observed: usize) {
        if observed > self.ctor_capacity.get() as usize {
            self.ctor_capacity.set(observed.min(u8::MAX as usize) as u8);
        }
    }

    /// Final named-property count of a small ordinary instance, recorded after a successful
    /// construct so later allocations can reserve the right capacity.
    pub(crate) fn observed_instance_capacity(&self) -> usize {
        if !self.elems.is_present() && self.entries.len() <= 16 {
            self.entries.len()
        } else {
            0
        }
    }
}
