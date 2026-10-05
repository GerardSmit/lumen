//! Object shapes (hidden classes): the ordered key list every object of a shape shares, the
//! transition tree that makes structurally-identical objects converge on one shape, prototype
//! epochs, and common property keys.
use std::{
    cell::{Cell, OnceCell},
    rc::Rc,
};
/// The empty-object shape id: every `Props` starts here (as a `None` shape pointer) and all empty
/// objects share it, so adding the same first key to two of them lands on the same child shape.
pub(super) const SHAPE_EMPTY: u32 = 0;

/// The property-creation epoch (see [`Props::proto_flag`]): bumped whenever a marked prototype
/// mutates structurally, any `[[SetPrototypeOf]]` succeeds, or a `defineProperty` rewrites
/// attributes — every event that could shadow a creation IC's "the chain has no setter /
/// non-writable / own copy of this name" proof. Process-global and atomic, NOT thread-local:
/// generator/async bodies run JS on pooled worker threads sharing the same `Interp` (one thread
/// at a time via channel handoff, which also orders these accesses), so a bump from a worker
/// must be visible to caches validated on the main thread. Starts at 1; saturates at `u32::MAX`,
/// which no cache hit accepts — after ~4e9 invalidations the creation ICs simply turn off
/// instead of ABA-cycling.
static PROTO_EPOCH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

/// The current creation-IC epoch. `u32::MAX` = permanently invalidated (see [`PROTO_EPOCH`]).
#[inline]
pub(crate) fn proto_epoch() -> u32 {
    PROTO_EPOCH.load(std::sync::atomic::Ordering::Relaxed)
}

/// Invalidate every property-creation inline cache (see [`PROTO_EPOCH`]).
pub(crate) fn bump_proto_epoch() {
    let _ = PROTO_EPOCH.fetch_update(
        std::sync::atomic::Ordering::Relaxed,
        std::sync::atomic::Ordering::Relaxed,
        |v| Some(v.saturating_add(1)),
    );
}

/// Key count up to which a shape answers lookups by a linear scan of its key list; past it a hash
/// index is built once per shape and shared by every object of that shape.
pub(super) const INDEX_THRESHOLD: usize = 8;

/// Named-key count past which an object leaves the shared transition tree for an *owned* shape
/// (see [`Keys::Owned`]). Bounds the depth of a shared chain (and so the cost of walking one)
/// and stops dictionary-style objects (`o[k] = v` in a loop) from minting one shared shape per
/// insert.
pub(super) const OWNED_THRESHOLD: usize = 32;

/// Chain-walk lookups a shared shape past [`INDEX_THRESHOLD`] keys answers before it builds its
/// key list and hash index.
const PROBES_BEFORE_INDEX: u8 = 3;
const _: () = assert!(std::mem::align_of::<SlotIndex>() > PROBES_BEFORE_INDEX as usize);

/// `Shape::len_slot` / `proto_slot` "no such key".
pub(super) const NO_SLOT: u32 = u32::MAX;

/// An object shape: the ordered sequence of *named* property keys, shared by every object that
/// added the same keys in the same order. `entries[i]` of any object of this shape is the value
/// of key `i` (array element entries come after the named prefix and are keyed by the dense
/// sidecar instead — see `Props::entries`). Attributes are NOT encoded; the inline cache re-checks
/// accessor/writable at the slot.
///
/// Two kinds share this type (see [`Keys`]):
/// - **Shared** shapes live in the [`ShapeTable`] transition tree, are immutable, and are held
///   by the table until nothing else references them (see [`ShapeTable::sweep`]); `id` keys
///   [`ShapeTable::by_id`] and is never reused. Each stores only
///   its own last key and a pointer to its parent: a chain of `n` keys costs `n` keys, not
///   `n(n+1)/2`. The full ordered list is materialised once, on demand, for the shapes that
///   are iterated — most shapes in a chain are intermediates an object passes through during
///   construction and never asks for its key order.
/// - **Owned** shapes belong to exactly one object (strong count 1, never in the table): an
///   object that shrank structurally (delete) or grew past [`OWNED_THRESHOLD`] keys detaches to
///   one. Its key list mutates in place, and every structural mutation gives it a fresh `id` so
///   a stale inline cache (shape id, slot) can never match a different key list.
pub(crate) struct Shape {
    pub(super) id: u32,
    /// Named-key count: the chain depth of a shared shape, the list length of an owned one.
    len: u32,
    /// Slots of the two hottest keys (`arr.length` / every `new`), or [`NO_SLOT`]: answered
    /// without a scan or hash.
    pub(super) len_slot: u32,
    pub(super) proto_slot: u32,
    keys: Keys,
    /// Built lazily on first lookup once `len > INDEX_THRESHOLD` (shared shapes), or
    /// maintained eagerly on mutation (owned shapes).
    index: IndexCell,
}

/// A shape's optional [`SlotIndex`] in one word, so the probe counter [`Shape::find`] keeps
/// before indexing costs no extra field: values up to [`PROBES_BEFORE_INDEX`] count chain-walk
/// lookups, anything larger is the address of the boxed index.
struct IndexCell(Cell<usize>);

impl IndexCell {
    const fn new() -> Self {
        IndexCell(Cell::new(0))
    }

    fn from(index: Box<SlotIndex>) -> Self {
        IndexCell(Cell::new(Box::into_raw(index) as usize))
    }

    #[inline(always)]
    fn get(&self) -> Option<&SlotIndex> {
        let word = self.0.get();
        // SAFETY: a word past the probe range is a live `Box<SlotIndex>` owned by this cell,
        // replaced only through `&mut self` or while no index is set.
        (word > PROBES_BEFORE_INDEX as usize).then(|| unsafe { &*(word as *const SlotIndex) })
    }

    fn get_mut(&mut self) -> Option<&mut SlotIndex> {
        let word = self.0.get();
        // SAFETY: as in `get`, and `&mut self` makes the borrow unique.
        (word > PROBES_BEFORE_INDEX as usize).then(|| unsafe { &mut *(word as *mut SlotIndex) })
    }

    fn get_or_init(&self, make: impl FnOnce() -> Box<SlotIndex>) -> &SlotIndex {
        if self.get().is_none() {
            self.0.set(Box::into_raw(make()) as usize);
        }
        self.get().expect("just set")
    }

    /// Count one chain-walk lookup; false once the budget is spent.
    fn take_probe(&self) -> bool {
        let word = self.0.get();
        let spare = word < PROBES_BEFORE_INDEX as usize;
        if spare {
            self.0.set(word + 1);
        }
        spare
    }
}

impl Drop for IndexCell {
    fn drop(&mut self) {
        let word = self.0.get();
        if word > PROBES_BEFORE_INDEX as usize {
            // SAFETY: see `get`; the cell is going away, so nothing borrows the index.
            drop(unsafe { Box::from_raw(word as *mut SlotIndex) });
        }
    }
}

/// A shape's key → slot hash index: open addressing with linear probing over `u64` buckets, each
/// holding a 32-bit hash tag (high half) and the slot (low half). Keys are not duplicated — a
/// tag match is confirmed against the shape's key list — so a dictionary-sized object pays 8
/// bytes per bucket instead of a `(Rc<str>, u32)` entry.
pub(super) struct SlotIndex {
    buckets: Box<[u64]>,
    len: u32,
}

const EMPTY_BUCKET: u64 = u64::MAX;

#[inline]
fn key_tag(key: &str) -> u32 {
    (crate::fasthash::hash_bytes(key.as_bytes()) >> 32) as u32
}

impl SlotIndex {
    fn new(keys: &[Rc<str>]) -> SlotIndex {
        let cap = (keys.len() * 4 / 3 + 1).next_power_of_two().max(16);
        let mut index = SlotIndex {
            buckets: vec![EMPTY_BUCKET; cap].into_boxed_slice(),
            len: 0,
        };
        for (slot, k) in keys.iter().enumerate() {
            index.push(k, slot as u32);
        }
        index
    }

    #[inline]
    fn place(buckets: &mut [u64], bucket: u64) {
        let mask = buckets.len() - 1;
        let mut i = (bucket >> 32) as usize & mask;
        while buckets[i] != EMPTY_BUCKET {
            i = (i + 1) & mask;
        }
        buckets[i] = bucket;
    }

    /// Add `key` (absent) at `slot`.
    fn push(&mut self, key: &str, slot: u32) {
        if (self.len as usize + 1) * 4 > self.buckets.len() * 3 {
            let mut grown = vec![EMPTY_BUCKET; self.buckets.len() * 2].into_boxed_slice();
            for &b in self.buckets.iter().filter(|&&b| b != EMPTY_BUCKET) {
                Self::place(&mut grown, b);
            }
            self.buckets = grown;
        }
        let bucket = ((key_tag(key) as u64) << 32) | slot as u64;
        Self::place(&mut self.buckets, bucket);
        self.len += 1;
    }

    #[inline]
    fn get(&self, keys: &[Rc<str>], key: &str) -> Option<u32> {
        let tag = key_tag(key);
        let mask = self.buckets.len() - 1;
        let mut i = tag as usize & mask;
        loop {
            let b = self.buckets[i];
            if b == EMPTY_BUCKET {
                return None;
            }
            if (b >> 32) as u32 == tag && key_eq(&keys[b as u32 as usize], key) {
                return Some(b as u32);
            }
            i = (i + 1) & mask;
        }
    }
}

/// Where a shape's keys live.
enum Keys {
    /// A shared shape: `key` is slot `len - 1`, the rest are the parent's. The empty root shape
    /// has no parent and a placeholder key that is never read (`len == 0`). `flat` is the whole
    /// ordered list, built on the first request for it.
    Chain {
        parent: Option<Rc<Shape>>,
        key: Rc<str>,
        flat: OnceCell<Box<[Rc<str>]>>,
    },
    /// An owned shape's mutable key list.
    Owned(Vec<Rc<str>>),
}

/// Pointer-equal (interned) or equal.
#[inline(always)]
fn key_eq(k: &Rc<str>, key: &str) -> bool {
    (k.as_ptr() == key.as_ptr() && k.len() == key.len()) || &**k == key
}

impl Shape {
    fn root() -> Shape {
        Shape {
            id: SHAPE_EMPTY,
            len: 0,
            len_slot: NO_SLOT,
            proto_slot: NO_SLOT,
            keys: Keys::Chain {
                parent: None,
                key: Rc::from(""),
                flat: OnceCell::new(),
            },
            index: IndexCell::new(),
        }
    }

    fn child(parent: &Rc<Shape>, id: u32, key: Rc<str>) -> Shape {
        let slot = parent.len;
        let (mut len_slot, mut proto_slot) = (parent.len_slot, parent.proto_slot);
        if &*key == "length" {
            len_slot = slot;
        } else if &*key == "prototype" {
            proto_slot = slot;
        }
        Shape {
            id,
            len: slot + 1,
            len_slot,
            proto_slot,
            keys: Keys::Chain {
                parent: Some(parent.clone()),
                key,
                flat: OnceCell::new(),
            },
            index: IndexCell::new(),
        }
    }

    /// An owned shape holding `keys` under a fresh id.
    fn new_owned(id: u32, keys: Vec<Rc<str>>) -> Shape {
        let mut shape = Shape {
            id,
            len: keys.len() as u32,
            len_slot: NO_SLOT,
            proto_slot: NO_SLOT,
            keys: Keys::Owned(keys),
            index: IndexCell::new(),
        };
        shape.remember_special_slots();
        if shape.len as usize > INDEX_THRESHOLD {
            shape.build_index();
        }
        shape
    }

    /// An owned copy of `self`'s key list under a fresh id.
    pub(super) fn to_owned_shape(&self, id: u32) -> Shape {
        Shape::new_owned(id, self.keys().to_vec())
    }

    #[inline]
    pub(super) fn owned(&self) -> bool {
        matches!(self.keys, Keys::Owned(_))
    }

    #[inline]
    pub(super) fn len(&self) -> usize {
        self.len as usize
    }

    /// The last key (slot `len - 1`), or `None` for an empty shape.
    pub(super) fn last_key(&self) -> Option<&Rc<str>> {
        match &self.keys {
            Keys::Chain { key, .. } => (self.len > 0).then_some(key),
            Keys::Owned(keys) => keys.last(),
        }
    }

    /// The parent chain from `self` down to (excluding) the root; each shape's `key`.
    #[inline]
    pub(super) fn chain(&self) -> impl Iterator<Item = (&Shape, &Rc<str>)> {
        let mut cur = Some(self);
        std::iter::from_fn(move || {
            let s = cur?;
            match &s.keys {
                Keys::Chain { parent, key, .. } if s.len > 0 => {
                    cur = parent.as_deref();
                    Some((s, key))
                }
                _ => None,
            }
        })
    }

    /// The ordered key list. Materialised on first use for a shared shape (and kept: a shape
    /// that is iterated once tends to be iterated again).
    pub(super) fn keys(&self) -> &[Rc<str>] {
        match &self.keys {
            Keys::Owned(keys) => keys,
            Keys::Chain { .. } if self.len == 0 => &[],
            Keys::Chain { flat, .. } => flat.get_or_init(|| {
                let mut keys: Vec<Rc<str>> = self.chain().map(|(_, k)| k.clone()).collect();
                keys.reverse();
                keys.into_boxed_slice()
            }),
        }
    }

    /// The key at `slot` (`< len`), without materialising a shared shape's list.
    pub(super) fn key_at(&self, slot: usize) -> &Rc<str> {
        match &self.keys {
            Keys::Owned(keys) => &keys[slot],
            Keys::Chain { flat, .. } => match flat.get() {
                Some(flat) => &flat[slot],
                None => {
                    let hops = self.len as usize - 1 - slot;
                    self.chain().nth(hops).expect("slot < len").1
                }
            },
        }
    }

    /// Whether the ordered list has been materialised (census).
    pub(super) fn flat_built(&self) -> bool {
        matches!(&self.keys, Keys::Chain { flat, .. } if flat.get().is_some())
    }

    /// A shared shape's index confirms hits against its materialised key list, so indexing one
    /// builds that list too.
    fn make_index(&self) -> Box<SlotIndex> {
        Box::new(SlotIndex::new(self.keys()))
    }

    fn build_index(&mut self) {
        self.index = IndexCell::from(self.make_index());
    }

    /// The slot of `key`, or `None`.
    #[inline(always)]
    pub(super) fn find(&self, key: &str) -> Option<u32> {
        // `length` and `prototype` are the hottest keys in array-heavy / allocation-heavy code
        // (every push/pop/length read; every `new`); their slots are memoized per shape.
        if key == "length" {
            return (self.len_slot != NO_SLOT).then_some(self.len_slot);
        } else if key == "prototype" {
            return (self.proto_slot != NO_SLOT).then_some(self.proto_slot);
        }
        if let Some(index) = self.index.get() {
            return index.get(self.keys(), key);
        }
        if self.len as usize > INDEX_THRESHOLD {
            // A shape an object grows through (an object literal or constructor adding its
            // ninth key and on) is probed once, to find the key absent, and then never again:
            // walking its chain for the first few lookups spares it a key list and a hash index
            // that nothing would read.
            if let Keys::Chain { flat, .. } = &self.keys {
                if flat.get().is_none() && self.index.take_probe() {
                    return self
                        .chain()
                        .find(|(_, k)| key_eq(k, key))
                        .map(|(s, _)| s.len - 1);
                }
            }
            return self
                .index
                .get_or_init(|| self.make_index())
                .get(self.keys(), key);
        }
        let flat: &[Rc<str>] = match &self.keys {
            Keys::Owned(keys) => keys,
            Keys::Chain { flat, .. } => match flat.get() {
                Some(flat) => flat,
                None => {
                    return self
                        .chain()
                        .find(|(_, k)| key_eq(k, key))
                        .map(|(s, _)| s.len - 1);
                }
            },
        };
        flat.iter().position(|k| key_eq(k, key)).map(|i| i as u32)
    }

    fn owned_keys_mut(&mut self) -> &mut Vec<Rc<str>> {
        match &mut self.keys {
            Keys::Owned(keys) => keys,
            Keys::Chain { .. } => unreachable!("shared shapes are immutable"),
        }
    }

    /// Append `key` (absent) as the last slot. Owned shapes only.
    pub(super) fn push_key(&mut self, key: Rc<str>) {
        let slot = self.len;
        if &*key == "length" {
            self.len_slot = slot;
        } else if &*key == "prototype" {
            self.proto_slot = slot;
        }
        let indexed = match self.index.get_mut() {
            Some(index) => {
                index.push(&key, slot);
                true
            }
            None => false,
        };
        self.owned_keys_mut().push(key);
        self.len += 1;
        if !indexed && self.len as usize > INDEX_THRESHOLD {
            self.build_index();
        }
    }

    /// Remove the key at `slot`, shifting later keys down. Owned shapes only.
    pub(super) fn remove_key(&mut self, slot: usize) {
        let Keys::Owned(keys) = &mut self.keys else {
            unreachable!("shared shapes are immutable")
        };
        keys.remove(slot);
        self.len -= 1;
        if self.index.get().is_some() {
            self.build_index();
        }
        self.remember_special_slots();
    }

    /// Keep only the keys `keep(slot, key)` accepts. Owned shapes only.
    pub(super) fn retain_keys(&mut self, mut keep: impl FnMut(usize, &str) -> bool) {
        let mut slot = 0;
        let keys = self.owned_keys_mut();
        keys.retain(|k| {
            let ok = keep(slot, k);
            slot += 1;
            ok
        });
        self.len = keys.len() as u32;
        if self.index.get().is_some() {
            self.build_index();
        }
        self.remember_special_slots();
    }

    fn remember_special_slots(&mut self) {
        let (mut len_slot, mut proto_slot) = (NO_SLOT, NO_SLOT);
        for (slot, k) in self.keys().iter().enumerate() {
            if &**k == "length" {
                len_slot = slot as u32;
            } else if &**k == "prototype" {
                proto_slot = slot as u32;
            }
        }
        self.len_slot = len_slot;
        self.proto_slot = proto_slot;
    }
}

/// The object-shape transition tree plus the id → shape index. A
/// shape id encodes an *ordered sequence of property keys* — two `Props` share an id exactly
/// when they added the same keys in the same order. `transitions[(parent, key)] = child` is
/// memoized, so structurally-identical objects converge on one shape — which is what makes a
/// shared per-site cache's shape compare meaningful. A structural *removal* can't be a tree
/// transition (it doesn't extend the key sequence), so it detaches the object to an owned shape
/// whose id no cache holds.
///
/// Shapes no object, child shape or compiled code references are reclaimed by an amortized
/// [`ShapeTable::sweep`]. Ids are never reused, so an inline cache still holding a reclaimed
/// shape's id can never match a live object again; a creation IC resolving its recorded child
/// by id sees `None` and takes the ordinary transition.
///
/// Lives in the [`crate::value::GcState`] shared by a driver thread and the coroutine workers that
/// run its generator bodies: objects flow between those threads, and a worker looking up a key on
/// an object the driver built must find its shape here.
pub(crate) struct ShapeTable {
    transitions: crate::fasthash::FastMap<(u32, Rc<str>), Rc<Shape>>,
    /// Shared shapes by id (`0` is the empty shape). Each holds one strong count here and one in
    /// `transitions` (the root: only here).
    by_id: crate::fasthash::FastMap<u32, Rc<Shape>>,
    /// The id the next shared shape gets: monotonic, so a reclaimed id is never handed out again.
    next_id: u32,
    /// Live transitions out of each shape that has any (see [`MAX_TRANSITIONS`]).
    children: crate::fasthash::FastMap<u32, u32>,
    /// Shapes whose `Rc` handle compiled code embeds (see [`jit_shared_shape`]): never swept.
    pinned: crate::fasthash::FastSet<u32>,
    /// `by_id` size past which the next new shape triggers a sweep.
    sweep_at: usize,
    /// Owned-shape ids count down from here so they never collide with a tree id.
    next_owned: u32,
    /// Shape reached by adding the intrinsic `"length"` key to an empty map. Array literals
    /// create this same one-property named map constantly.
    array_length: Option<Rc<Shape>>,
    /// `{length, index, input, groups}`: the `RegExp.prototype.exec` match array's named map.
    exec_result: Option<Rc<Shape>>,
}

/// Transitions one shape may have before further new keys detach objects to owned shapes
/// (V8's `kMaxNumberOfTransitions`): objects keyed by unbounded data (`{[id]: v}`) would
/// otherwise mint a shared shape per distinct key.
const MAX_TRANSITIONS: u32 = 1536;

/// Floor for [`ShapeTable::sweep_at`].
const SWEEP_MIN: usize = 4096;

/// The last shared id: owned ids count down from `u32::MAX`.
const MAX_SHARED_ID: u32 = 0x7fff_ffff;

impl ShapeTable {
    pub(crate) fn new() -> ShapeTable {
        let mut by_id = crate::fasthash::FastMap::default();
        by_id.insert(SHAPE_EMPTY, Rc::new(Shape::root()));
        ShapeTable {
            transitions: Default::default(),
            by_id,
            next_id: SHAPE_EMPTY + 1,
            children: Default::default(),
            pinned: Default::default(),
            sweep_at: SWEEP_MIN,
            next_owned: u32::MAX - 1,
            array_length: None,
            exec_result: None,
        }
    }

    fn root(&self) -> Rc<Shape> {
        self.by_id[&SHAPE_EMPTY].clone()
    }

    fn fresh_owned_id(&mut self) -> u32 {
        let id = self.next_owned;
        // Wrap back below the sentinel instead of colliding with the tree range for as long as
        // possible (4e9 detaches — the same ABA odds the previous fresh-id scheme accepted).
        self.next_owned = if id <= 1 { u32::MAX - 1 } else { id - 1 };
        id
    }

    /// The memoized child of `parent` by `key`, created if absent. `None` when `capped` and
    /// `parent` already has [`MAX_TRANSITIONS`] children, or the id space is spent: the caller
    /// detaches to an owned shape.
    fn transition(&mut self, parent: &Rc<Shape>, key: &Rc<str>, capped: bool) -> Option<Rc<Shape>> {
        if let Some(c) = self.transitions.get(&(parent.id, key.clone())) {
            return Some(c.clone());
        }
        if self.by_id.len() > self.sweep_at {
            self.sweep();
        }
        let n = self.children.get(&parent.id).copied().unwrap_or(0);
        if capped && (n >= MAX_TRANSITIONS || self.next_id > MAX_SHARED_ID) {
            return None;
        }
        let id = self.next_id;
        assert!(id <= MAX_SHARED_ID, "shape table exhausted");
        self.next_id += 1;
        let child = Rc::new(Shape::child(parent, id, key.clone()));
        self.by_id.insert(id, child.clone());
        self.transitions
            .insert((parent.id, key.clone()), child.clone());
        self.children.insert(parent.id, n + 1);
        Some(child)
    }

    /// Drop every shared shape referenced only by this table (no object, child shape, cached
    /// handle or compiled code holds it), with its transition entry. Children always have larger
    /// ids than their parent, so one pass in descending id order also reclaims a parent whose
    /// last child goes earlier in the same pass. Re-arms at twice the surviving size, which
    /// keeps the cost amortized O(log n) per shape created.
    fn sweep(&mut self) {
        let mut ids: Vec<u32> = self.by_id.keys().copied().collect();
        ids.sort_unstable_by(|a, b| b.cmp(a));
        for id in ids {
            if id == SHAPE_EMPTY || self.pinned.contains(&id) {
                continue;
            }
            if self.by_id.get(&id).is_none_or(|s| Rc::strong_count(s) != 2) {
                continue;
            }
            let shape = self.by_id.remove(&id).expect("present");
            self.children.remove(&id);
            if let Keys::Chain {
                parent: Some(p),
                key,
                ..
            } = &shape.keys
            {
                self.transitions.remove(&(p.id, key.clone()));
                if let std::collections::hash_map::Entry::Occupied(mut e) =
                    self.children.entry(p.id)
                {
                    *e.get_mut() -= 1;
                    if *e.get() == 0 {
                        e.remove();
                    }
                }
            }
        }
        self.sweep_at = (self.by_id.len() * 2).max(SWEEP_MIN);
    }
}

fn with_shapes<R>(f: impl FnOnce(&mut ShapeTable) -> R) -> R {
    crate::value::with_gc_state(|state| f(&mut state.shapes.borrow_mut()))
}

/// The child shape reached by adding `key` to shared shape `parent` (memoized so it is shared).
/// `None` parent = the empty shape. `None` result: `parent` has too many transitions (see
/// [`MAX_TRANSITIONS`]) and the object should detach to an owned shape.
pub(super) fn shape_transition(parent: Option<&Rc<Shape>>, key: &Rc<str>) -> Option<Rc<Shape>> {
    with_shapes(|t| match parent {
        Some(p) => t.transition(p, key, true),
        None => {
            let empty = t.root();
            t.transition(&empty, key, true)
        }
    })
}

/// The shared shape with id `id` as its `Rc` handle word and key count, for compiled code that
/// switches an object to it. The shape is pinned: the code embeds the handle, so the table must
/// keep it for the heap's lifetime. `None` for an owned or reclaimed id.
pub(crate) fn jit_shared_shape(id: u32) -> Option<(usize, u32)> {
    with_shapes(|t| {
        let s = t.by_id.get(&id)?;
        let r = (unsafe { *(s as *const Rc<Shape> as *const usize) }, s.len);
        t.pinned.insert(id);
        Some(r)
    })
}

/// The address of the creation-IC epoch (a `u32`, see [`PROTO_EPOCH`]), for compiled code.
pub(crate) fn proto_epoch_addr() -> usize {
    PROTO_EPOCH.as_ptr() as usize
}

/// The shared shape with id `id` (a creation IC's recorded child), or `None` once it has been
/// reclaimed. The fills that record ids only ever record shared ones.
pub(super) fn shape_by_id(id: u32) -> Option<Rc<Shape>> {
    with_shapes(|t| t.by_id.get(&id).cloned())
}

/// A fresh owned shape holding `keys` (the object detaches from the tree).
pub(super) fn shape_owned_from(keys: Option<&Shape>) -> Rc<Shape> {
    with_shapes(|t| {
        let id = t.fresh_owned_id();
        Rc::new(match keys {
            Some(k) => k.to_owned_shape(id),
            None => Shape::new_owned(id, Vec::new()),
        })
    })
}

/// A fresh id for an owned shape that just mutated structurally.
pub(super) fn fresh_owned_id() -> u32 {
    with_shapes(|t| t.fresh_owned_id())
}

/// The `{length}` shape every array's named map starts from.
pub(in crate::value) fn array_length_shape() -> Rc<Shape> {
    with_shapes(|t| {
        if let Some(s) = &t.array_length {
            return s.clone();
        }
        let empty = t.root();
        let s = t.transition(&empty, &fn_key(0), false).expect("uncapped");
        t.array_length = Some(s.clone());
        s
    })
}

/// The `{length, index, input, groups}` shape of a `RegExp.prototype.exec` match array (the
/// array's own `length`, then the three data properties RegExpBuiltinExec creates in order).
pub(super) fn exec_result_shape() -> Rc<Shape> {
    let s = array_length_shape();
    with_shapes(|t| {
        if let Some(s) = &t.exec_result {
            return s.clone();
        }
        let mut s = s;
        for key in ["index", "input", "groups"] {
            s = t.transition(&s, &Rc::from(key), false).expect("uncapped");
        }
        t.exec_result = Some(s.clone());
        s
    })
}

/// Shared-shape table sizes for the heap census (`LUMEN_HEAP_CENSUS`).
pub struct ShapeTableCensus {
    /// Shared shapes in the transition tree (including the empty root).
    pub shapes: usize,
    /// Keys the shapes *represent* (sum of chain depths): what a flat per-shape list would
    /// store.
    pub keys: usize,
    /// Shapes whose ordered key list has been materialised, and the keys those lists hold.
    pub flat_lists: usize,
    pub flat_keys: usize,
    /// Shapes holding a hash index.
    pub indexes: usize,
    /// Approximate bytes: shape allocations, materialised lists and transition-table entries
    /// (not the hash indexes' tables).
    pub bytes: usize,
}

pub(crate) fn shape_table_census() -> ShapeTableCensus {
    with_shapes(|t| {
        let mut c = ShapeTableCensus {
            shapes: t.by_id.len(),
            keys: 0,
            flat_lists: 0,
            flat_keys: 0,
            indexes: 0,
            bytes: 0,
        };
        for s in t.by_id.values() {
            c.keys += s.len();
            if s.flat_built() {
                c.flat_lists += 1;
                c.flat_keys += s.len();
            }
            if s.index.get().is_some() {
                c.indexes += 1;
            }
        }
        let rc_box = std::mem::size_of::<Shape>() + 2 * std::mem::size_of::<usize>();
        let transition = std::mem::size_of::<((u32, Rc<str>), Rc<Shape>)>();
        c.bytes = c.shapes * rc_box
            + c.flat_keys * std::mem::size_of::<Rc<str>>()
            + t.transitions.len() * transition;
        c
    })
}

thread_local! {
    /// Interned key strings for small array indices — every dense array element key "0".."63"
    /// shares one allocation per thread instead of allocating per element.
    static INDEX_KEYS: Vec<Rc<str>> = (0..64).map(|i| Rc::from(i.to_string().as_str())).collect();
    /// Interned keys for the properties every function object carries — closure creation in a
    /// hot loop would otherwise allocate each key string per closure.
    static FN_KEYS: [Rc<str>; 4] = [
        Rc::from("length"),
        Rc::from("name"),
        Rc::from("prototype"),
        Rc::from("constructor"),
    ];
}

/// The property key for array index `n`, interned for small `n`.
pub(crate) fn index_key(n: usize) -> Rc<str> {
    if n < 64 {
        INDEX_KEYS.with(|k| k[n].clone())
    } else {
        Rc::from(n.to_string().as_str())
    }
}

/// Interned `"length"` / `"name"` / `"prototype"` / `"constructor"` keys (see `FN_KEYS`).
pub(crate) fn fn_key(i: usize) -> Rc<str> {
    FN_KEYS.with(|k| k[i].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sweep() {
        with_shapes(|t| t.sweep());
    }

    #[test]
    fn unreferenced_shapes_are_reclaimed_and_their_ids_never_reused() {
        std::thread::spawn(|| {
            let key: Rc<str> = Rc::from("reclaim_probe");
            let parent = shape_transition(None, &key).unwrap();
            let child = shape_transition(Some(&parent), &Rc::from("second")).unwrap();
            let (pid, cid) = (parent.id, child.id);
            drop(parent);
            sweep();
            assert!(shape_by_id(pid).is_some(), "a live child keeps its parent");
            drop(child);
            sweep();
            assert!(shape_by_id(cid).is_none() && shape_by_id(pid).is_none());
            let again = shape_transition(None, &key).unwrap();
            assert!(again.id > cid, "a reclaimed id is not handed out again");
            // Compiled code embeds a pinned shape's handle: it outlives every other reference.
            let id = again.id;
            assert!(jit_shared_shape(id).is_some());
            drop(again);
            sweep();
            assert!(shape_by_id(id).is_some());
            assert!(array_length_shape().id == exec_result_shape().chain().last().unwrap().0.id);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn a_shape_mints_at_most_max_transitions_children() {
        std::thread::spawn(|| {
            let parent = shape_transition(None, &Rc::from("cap_probe")).unwrap();
            let held: Vec<_> = (0..MAX_TRANSITIONS)
                .map(|i| shape_transition(Some(&parent), &Rc::from(format!("c{i}"))).unwrap())
                .collect();
            assert!(shape_transition(Some(&parent), &Rc::from("one_more")).is_none());
            // Existing transitions still resolve.
            assert!(Rc::ptr_eq(
                &shape_transition(Some(&parent), &Rc::from("c0")).unwrap(),
                &held[0]
            ));
            drop(held);
            sweep();
            assert!(shape_transition(Some(&parent), &Rc::from("one_more")).is_some());
        })
        .join()
        .unwrap();
    }
}
