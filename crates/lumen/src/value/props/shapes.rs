//! Object shapes (hidden classes): the ordered key list every object of a shape shares, the
//! transition tree that makes structurally-identical objects converge on one shape, prototype
//! epochs, and common property keys.
use std::{cell::OnceCell, rc::Rc};
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

/// Stable address used by the ARM64 creation-IC template for the same relaxed epoch check.
#[inline]
pub(crate) fn proto_epoch_ptr() -> *const u32 {
    PROTO_EPOCH.as_ptr()
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
///   by the table forever; their `id` is their index in [`ShapeTable::by_id`]. Each stores only
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
    index: OnceCell<Box<crate::fasthash::FastMap<Rc<str>, u32>>>,
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
            index: OnceCell::new(),
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
            index: OnceCell::new(),
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
            index: OnceCell::new(),
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
    fn chain(&self) -> impl Iterator<Item = (&Shape, &Rc<str>)> {
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

    /// The ordered key list. Materialised on first use for a shared shape (and kept: the shape
    /// is immortal, and a shape that is iterated once tends to be iterated again).
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
    fn flat_built(&self) -> bool {
        matches!(&self.keys, Keys::Chain { flat, .. } if flat.get().is_some())
    }

    fn make_index(&self) -> Box<crate::fasthash::FastMap<Rc<str>, u32>> {
        let mut index = Box::<crate::fasthash::FastMap<Rc<str>, u32>>::default();
        match &self.keys {
            Keys::Owned(keys) => {
                for (slot, k) in keys.iter().enumerate() {
                    index.insert(k.clone(), slot as u32);
                }
            }
            Keys::Chain { .. } => {
                for (s, k) in self.chain() {
                    index.insert(k.clone(), s.len - 1);
                }
            }
        }
        index
    }

    fn build_index(&mut self) {
        self.index = OnceCell::from(self.make_index());
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
            return index.get(key).copied();
        }
        if self.len as usize > INDEX_THRESHOLD {
            return self
                .index
                .get_or_init(|| self.make_index())
                .get(key)
                .copied();
        }
        let flat: &[Rc<str>] = match &self.keys {
            Keys::Owned(keys) => keys,
            Keys::Chain { flat, .. } => match flat.get() {
                Some(flat) => flat,
                None => {
                    return self
                        .chain()
                        .find(|(_, k)| key_eq(k, key))
                        .map(|(s, _)| s.len - 1)
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
        self.owned_keys_mut().push(key.clone());
        self.len += 1;
        if let Some(index) = self.index.get_mut() {
            index.insert(key, slot);
        } else if self.len as usize > INDEX_THRESHOLD {
            self.build_index();
        }
    }

    /// Remove the key at `slot`, shifting later keys down. Owned shapes only.
    pub(super) fn remove_key(&mut self, slot: usize) {
        let Keys::Owned(keys) = &mut self.keys else {
            unreachable!("shared shapes are immutable")
        };
        let key = keys.remove(slot);
        self.len -= 1;
        if let Some(index) = self.index.get_mut() {
            index.remove(&key);
            for (j, k) in keys.iter().enumerate().skip(slot) {
                index.insert(k.clone(), j as u32);
            }
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

/// The object-shape transition tree plus the id → shape index the JIT's creation IC reads. A
/// shape id encodes an *ordered sequence of property keys* — two `Props` share an id exactly
/// when they added the same keys in the same order. `transitions[(parent, key)] = child` is
/// memoized, so structurally-identical objects converge on one shape — which is what makes a
/// shared per-site cache's shape compare meaningful. A structural *removal* can't be a tree
/// transition (it doesn't extend the key sequence), so it detaches the object to an owned shape
/// whose id no cache holds.
///
/// Lives in the [`crate::value::GcState`] shared by a driver thread and the coroutine workers that
/// run its generator bodies: objects flow between those threads, and a worker looking up a key on
/// an object the driver built must find its shape here.
pub(crate) struct ShapeTable {
    transitions: crate::fasthash::FastMap<(u32, Rc<str>), Rc<Shape>>,
    /// Shared shapes by id (`by_id[0]` is the empty shape). Never shrinks: a shape id recorded
    /// in an inline cache must stay resolvable for as long as the heap lives.
    by_id: Vec<Rc<Shape>>,
    /// Owned-shape ids count down from here so they never collide with a tree id.
    next_owned: u32,
    /// Shape reached by adding the intrinsic `"length"` key to an empty map. Array literals
    /// create this same one-property named map constantly.
    array_length: Option<Rc<Shape>>,
}

impl ShapeTable {
    pub(crate) fn new() -> ShapeTable {
        ShapeTable {
            transitions: Default::default(),
            by_id: vec![Rc::new(Shape::root())],
            next_owned: u32::MAX - 1,
            array_length: None,
        }
    }

    fn fresh_owned_id(&mut self) -> u32 {
        let id = self.next_owned;
        // Wrap back below the sentinel instead of colliding with the tree range for as long as
        // possible (4e9 detaches — the same ABA odds the previous fresh-id scheme accepted).
        self.next_owned = if id <= 1 { u32::MAX - 1 } else { id - 1 };
        id
    }

    fn transition(&mut self, parent: &Rc<Shape>, key: &Rc<str>) -> Rc<Shape> {
        if let Some(c) = self.transitions.get(&(parent.id, key.clone())) {
            return c.clone();
        }
        let id = self.by_id.len() as u32;
        assert!(id < 0x8000_0000, "shape table exhausted");
        let child = Rc::new(Shape::child(parent, id, key.clone()));
        self.by_id.push(child.clone());
        self.transitions
            .insert((parent.id, key.clone()), child.clone());
        child
    }
}

fn with_shapes<R>(f: impl FnOnce(&mut ShapeTable) -> R) -> R {
    crate::value::with_gc_state(|state| f(&mut state.shapes.borrow_mut()))
}

/// The child shape reached by adding `key` to shared shape `parent` (memoized so it is shared).
/// `None` parent = the empty shape.
pub(super) fn shape_transition(parent: Option<&Rc<Shape>>, key: &Rc<str>) -> Rc<Shape> {
    with_shapes(|t| match parent {
        Some(p) => t.transition(p, key),
        None => {
            let empty = t.by_id[0].clone();
            t.transition(&empty, key)
        }
    })
}

/// The shared shape with id `id` (a creation IC's recorded child). Panics on an owned id — the
/// fills that record ids only ever record shared ones.
pub(super) fn shape_by_id(id: u32) -> Rc<Shape> {
    with_shapes(|t| t.by_id[id as usize].clone())
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
pub(super) fn array_length_shape() -> Rc<Shape> {
    with_shapes(|t| {
        if let Some(s) = &t.array_length {
            return s.clone();
        }
        let empty = t.by_id[0].clone();
        let s = t.transition(&empty, &fn_key(0));
        t.array_length = Some(s.clone());
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
        for s in &t.by_id {
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

/// Address of the shared-shape `by_id` vector header, for the JIT creation template: it indexes
/// the vector with the cache's child shape id to obtain the stored `Rc<Shape>` pointer. The
/// header lives inside the `Arc<GcState>` the emitting thread and its workers share, so the
/// address is stable for the code's lifetime.
pub(crate) fn shape_table_by_id_ptr() -> *const Vec<Rc<Shape>> {
    with_shapes(|t| &t.by_id as *const Vec<Rc<Shape>>)
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
