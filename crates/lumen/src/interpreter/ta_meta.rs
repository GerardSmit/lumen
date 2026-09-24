//! The guard for reading a TypedArray's `length` / `byteLength` / `byteOffset` / `buffer`
//! without calling the %TypedArray.prototype% getter: valid only while the lookup would reach the
//! realm's intrinsic accessor, i.e. no own property, no prototype on the way shadowing the key,
//! and the accessor's getter not replaced. Anything else takes the ordinary [[Get]].
use super::Interp;
use crate::eval::fastpaths::SlotCache;
use crate::value::{Exotic, Gc, Object, Value};
use std::cell::{Cell, RefCell};

/// The meta keys, in the index order the guard takes.
pub(crate) const TA_META_KEYS: [&str; 4] = ["length", "byteLength", "byteOffset", "buffer"];

/// `extra_protos` names under which `builtins::typedarray` registers the intrinsic getters.
pub(crate) const TA_META_GETTERS: [&str; 4] = [
    "%TypedArray.prototype.length getter%",
    "%TypedArray.prototype.byteLength getter%",
    "%TypedArray.prototype.byteOffset getter%",
    "%TypedArray.prototype.buffer getter%",
];

/// Prototype levels searched: a kind prototype (or a class's prototype), then up to two more.
const DEPTH: usize = 3;

#[derive(Default)]
pub(crate) struct TaMetaCaches {
    /// The intrinsic getters last seen (held, so their addresses can't be reused while cached).
    getters: RefCell<Option<[Gc; 4]>>,
    /// Their addresses (0 until first use), for the one-compare hit path.
    addrs: [Cell<usize>; 4],
    /// Per key: whether the instance has it as an own property (expected absent).
    own: [SlotCache; 4],
    /// Per key, per prototype level: where (or whether) the key lives under that level's shape.
    slots: [[SlotCache; DEPTH]; 4],
}

impl Interp {
    /// The index of `key` in [`TA_META_KEYS`].
    #[inline]
    pub(crate) fn ta_meta_index(key: &str) -> Option<usize> {
        match key {
            "length" => Some(0),
            "byteLength" => Some(1),
            "byteOffset" => Some(2),
            "buffer" => Some(3),
            _ => None,
        }
    }

    /// Whether `[[Get]]` of meta key `k` on the TypedArray `ta` (its borrowed object) resolves to
    /// the intrinsic getter, so the value can be computed directly. Pure: reads only.
    #[inline]
    pub(crate) fn ta_meta_intrinsic(&self, ta: &Object, k: usize) -> bool {
        let key = TA_META_KEYS[k];
        if self.ta_meta.own[k].get(&ta.props, key).is_some() {
            return false;
        }
        let mut cur: Option<&Gc> = ta.proto.as_ref();
        for depth in 0..DEPTH {
            let Some(p) = cur else {
                return false;
            };
            // SAFETY: pure reads; nothing below can borrow mutably.
            let Ok(b) = (unsafe { p.try_borrow_unguarded() }) else {
                return false;
            };
            if !b.ic_plain.get() || !matches!(b.exotic, Exotic::None) {
                return false;
            }
            if let Some(prop) = self.ta_meta.slots[k][depth].get(&b.props, key) {
                return prop.accessor()
                    && matches!(prop.getter(), Some(Value::Obj(g)) if self.is_ta_meta_getter(g, k));
            }
            cur = b.proto.as_ref();
        }
        false
    }

    /// Whether `g` is the current realm's intrinsic getter for meta key `k`.
    #[inline]
    fn is_ta_meta_getter(&self, g: &Gc, k: usize) -> bool {
        Gc::as_ptr(g) as usize == self.ta_meta.addrs[k].get() || self.ta_meta_refresh(g, k)
    }

    /// First use, or another realm's intrinsics (or a replaced getter): re-read them.
    #[inline(never)]
    fn ta_meta_refresh(&self, g: &Gc, k: usize) -> bool {
        let fresh: Option<Vec<Gc>> = TA_META_GETTERS
            .iter()
            .map(|n| self.extra_protos.get(n).cloned())
            .collect();
        let Some(fresh) = fresh.and_then(|v| <[Gc; 4]>::try_from(v).ok()) else {
            return false;
        };
        let hit = Gc::ptr_eq(&fresh[k], g);
        for (a, f) in self.ta_meta.addrs.iter().zip(&fresh) {
            a.set(Gc::as_ptr(f) as usize);
        }
        *self.ta_meta.getters.borrow_mut() = Some(fresh);
        hit
    }
}
