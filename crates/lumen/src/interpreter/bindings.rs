//! Binding maps with generation-checked entry addresses.
use super::Binding;
use crate::value::Value;
use std::ptr::NonNull;
use std::rc::Rc;

mod layout;
pub(crate) use layout::BindingLayout;

/// One scope's binding map, wrapping the raw hash map so every *structural* mutation — anything
/// that can move entries or change what a name resolves to (insert, remove, clear) — bumps a
/// generation counter. The bytecode tier's per-site name caches hold a raw `&Binding` pointer
/// plus the generation they resolved it at (see `bytecode::NameIc`): a matching generation
/// proves the map hasn't changed shape since, so the pointer is still valid *and* still the
/// right resolution. In-place binding writes (`get_mut`) intentionally don't bump — they can't
/// move entries, and a cache read-through observes the new value, which is exactly correct.
/// Reads pass through via `Deref`; mutations only exist as the inherent methods below, so a new
/// mutation site can't forget the bump (it won't compile).
pub struct VarMap {
    map: VarStorage,
    generation: std::cell::Cell<u32>,
    /// Set once a per-site name resolution walked through this map (see
    /// `interpreter::name_cache`): from then on its structural mutations bump the scope epoch.
    observed: std::cell::Cell<bool>,
    /// The owning scope's flags (`interpreter::SCOPE_*`), here to share this struct's padding.
    scope_flags: u8,
}

const SMALL_VAR_MAP_CAPACITY: usize = 8;

/// A template map's values: `layout.names.len()` bindings behind a thin pointer (the length
/// is the layout's), so the template variant fits the same 24 bytes as the others.
struct Slots(NonNull<Binding>);

impl Slots {
    fn new(values: Vec<Binding>) -> Slots {
        let b: Box<[Binding]> = values.into_boxed_slice();
        Slots(NonNull::new(Box::into_raw(b) as *mut Binding).expect("box pointer"))
    }
    #[inline]
    fn get<'a>(&'a self, layout: &BindingLayout) -> &'a [Binding] {
        // SAFETY: a `Slots` is only ever paired with the layout it was built for.
        unsafe { std::slice::from_raw_parts(self.0.as_ptr(), layout.names.len()) }
    }
    #[inline]
    fn get_mut<'a>(&'a mut self, layout: &BindingLayout) -> &'a mut [Binding] {
        // SAFETY: as in `get`; `&mut self` makes the access unique.
        unsafe { std::slice::from_raw_parts_mut(self.0.as_ptr(), layout.names.len()) }
    }
    fn into_vec(self, layout: &BindingLayout) -> Vec<Binding> {
        let p = std::ptr::slice_from_raw_parts_mut(self.0.as_ptr(), layout.names.len());
        // SAFETY: built by `Slots::new` from a boxed slice of exactly this length.
        unsafe { Box::from_raw(p) }.into_vec()
    }
}

enum VarStorage {
    Template(Rc<BindingLayout>, Slots),
    Small(Vec<(std::rc::Rc<str>, Binding)>),
    Large(Box<crate::fasthash::FastMap<std::rc::Rc<str>, Binding>>),
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<VarStorage>() == 24);
#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<VarMap>() == 32);

thread_local! {
    /// Layouts recently given to per-iteration copies, keyed by their names' identities.
    static COPY_LAYOUTS: std::cell::RefCell<Vec<Rc<BindingLayout>>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// A layout for exactly `entries`' names, in order — reused while the same names (the same
/// interned `Rc`s: a loop's declarations) come back, so a loop's per-iteration copies share
/// one.
fn shared_layout(entries: &[(Rc<str>, Binding)]) -> Option<Rc<BindingLayout>> {
    const KEEP: usize = 8;
    if entries.is_empty() {
        return None;
    }
    COPY_LAYOUTS.with(|c| {
        let mut c = c.borrow_mut();
        let same = |l: &BindingLayout| {
            l.names.len() == entries.len()
                && l.names
                    .iter()
                    .zip(entries)
                    .all(|(a, (b, _))| Rc::ptr_eq(a, b))
        };
        if let Some(i) = c.iter().position(|l| same(l)) {
            let l = c.remove(i);
            c.push(l.clone());
            return Some(l);
        }
        let l = BindingLayout::new(entries.iter().map(|(n, _)| n.clone()));
        if l.names.len() != entries.len() {
            return None;
        }
        if c.len() == KEEP {
            c.remove(0);
        }
        c.push(l.clone());
        Some(l)
    })
}

impl Drop for VarMap {
    fn drop(&mut self) {
        if let VarStorage::Template(..) = self.map {
            self.take_template();
        }
    }
}

impl Default for VarMap {
    fn default() -> Self {
        Self::from_storage(VarStorage::Small(Vec::new()), 0)
    }
}

pub enum VarIter<'a> {
    Template(std::iter::Zip<std::slice::Iter<'a, Rc<str>>, std::slice::Iter<'a, Binding>>),
    Small(std::slice::Iter<'a, (std::rc::Rc<str>, Binding)>),
    Large(std::collections::hash_map::Iter<'a, std::rc::Rc<str>, Binding>),
}

impl<'a> Iterator for VarIter<'a> {
    type Item = (&'a std::rc::Rc<str>, &'a Binding);
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            VarIter::Small(iter) => iter.next().map(|(name, binding)| (name, binding)),
            VarIter::Large(iter) => iter.next(),
            VarIter::Template(iter) => iter.next(),
        }
    }
}

pub enum VarKeys<'a> {
    Template(std::slice::Iter<'a, Rc<str>>),
    Small(std::slice::Iter<'a, (std::rc::Rc<str>, Binding)>),
    Large(std::collections::hash_map::Keys<'a, std::rc::Rc<str>, Binding>),
}

impl<'a> Iterator for VarKeys<'a> {
    type Item = &'a std::rc::Rc<str>;
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            VarKeys::Small(iter) => iter.next().map(|(name, _)| name),
            VarKeys::Large(iter) => iter.next(),
            VarKeys::Template(iter) => iter.next(),
        }
    }
}

pub enum VarValues<'a> {
    Template(std::slice::Iter<'a, Binding>),
    Small(std::slice::Iter<'a, (std::rc::Rc<str>, Binding)>),
    Large(std::collections::hash_map::Values<'a, std::rc::Rc<str>, Binding>),
}

impl<'a> Iterator for VarValues<'a> {
    type Item = &'a Binding;
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            VarValues::Small(iter) => iter.next().map(|(_, binding)| binding),
            VarValues::Large(iter) => iter.next(),
            VarValues::Template(iter) => iter.next(),
        }
    }
}

impl VarMap {
    fn from_storage(map: VarStorage, generation: u32) -> Self {
        Self {
            map,
            generation: std::cell::Cell::new(generation),
            observed: std::cell::Cell::new(false),
            scope_flags: 0,
        }
    }

    #[inline]
    pub(crate) fn scope_flags(&self) -> u8 {
        self.scope_flags
    }

    pub(crate) fn set_scope_flags(&mut self, flags: u8) {
        self.scope_flags = flags;
    }

    /// Swap a template representation for an empty small one, handing back its parts.
    fn take_template(&mut self) -> Option<(Rc<BindingLayout>, Vec<Binding>)> {
        match std::mem::replace(&mut self.map, VarStorage::Small(Vec::new())) {
            VarStorage::Template(layout, slots) => {
                let values = slots.into_vec(&layout);
                Some((layout, values))
            }
            other => {
                self.map = other;
                None
            }
        }
    }

    /// (bindings, reserved slots, mode) for a heap census.
    pub(crate) fn census(&self) -> (usize, usize, &'static str) {
        match &self.map {
            VarStorage::Template(l, _) => (l.names.len(), l.names.len(), "template"),
            VarStorage::Small(v) => (v.len(), v.capacity(), "small"),
            VarStorage::Large(m) => (m.len(), m.capacity(), "large"),
        }
    }
    pub(crate) fn template_layout(&self) -> Option<&Rc<BindingLayout>> {
        match &self.map {
            VarStorage::Template(layout, _) if self.generation() == 0 => Some(layout),
            _ => None,
        }
    }

    pub(crate) fn template_binding(&self, slot: usize) -> Option<&Binding> {
        match &self.map {
            VarStorage::Template(layout, values) if self.generation() == 0 => {
                values.get(layout).get(slot)
            }
            _ => None,
        }
    }

    pub(crate) fn from_layout(layout: Rc<BindingLayout>) -> Self {
        let values = layout
            .names
            .iter()
            .map(|_| Binding::data(Value::Undefined, true, true))
            .collect();
        Self::from_storage(VarStorage::Template(layout, Slots::new(values)), 0)
    }

    /// The layout identity proves the slot's name without a string/hash lookup. In-place
    /// writes keep entry addresses stable, just like get_mut on the dynamic representation.
    pub(crate) fn layout_binding_mut(
        &mut self,
        expected: &Rc<BindingLayout>,
        slot: usize,
    ) -> Option<&mut Binding> {
        match &mut self.map {
            VarStorage::Template(layout, values) if Rc::ptr_eq(layout, expected) => {
                values.get_mut(layout).get_mut(slot)
            }
            _ => None,
        }
    }

    fn make_dynamic(&mut self) {
        if let Some((layout, values)) = self.take_template() {
            self.map =
                VarStorage::Large(Box::new(layout.names.iter().cloned().zip(values).collect()));
        }
    }

    pub(crate) fn with_capacity(capacity: usize) -> VarMap {
        Self::from_storage(
            if capacity <= SMALL_VAR_MAP_CAPACITY {
                VarStorage::Small(Vec::with_capacity(capacity))
            } else {
                VarStorage::Large(Box::new(
                    crate::fasthash::FastMap::with_capacity_and_hasher(
                        capacity,
                        Default::default(),
                    ),
                ))
            },
            0,
        )
    }

    /// The structural generation (name-cache validation token).
    #[inline]
    pub(crate) fn generation(&self) -> u32 {
        self.generation.get()
    }
    #[inline]
    fn bump(&self) {
        if self.observed.get() {
            crate::value::bump_scope_epoch();
        }
        // Zero is reserved for pristine compiled layouts. Once structural mutation has
        // invalidated an activation's native binding base, wrapping must never revive it.
        self.generation
            .set(self.generation.get().wrapping_add(1).max(1));
    }
    pub fn insert(&mut self, k: impl Into<std::rc::Rc<str>>, v: Binding) -> Option<Binding> {
        self.bump();
        let k = k.into();
        if matches!(&self.map, VarStorage::Template(layout, _) if layout.slot(&k).is_none()) {
            self.make_dynamic();
        }
        match &mut self.map {
            VarStorage::Template(layout, values) => {
                let slot = layout.slot(&k).expect("existing layout binding");
                Some(std::mem::replace(&mut values.get_mut(layout)[slot], v))
            }
            VarStorage::Small(entries) => {
                if let Some((_, old)) = entries.iter_mut().find(|(name, _)| **name == *k) {
                    return Some(std::mem::replace(old, v));
                }
                if entries.len() < SMALL_VAR_MAP_CAPACITY {
                    // Grow by exactly what is needed: a scope's bindings are declared once, at
                    // entry, and closures keep tens of thousands of these vectors alive, so the
                    // doubling policy's slack is real memory, not amortisation.
                    if entries.len() == entries.capacity() {
                        entries.reserve_exact(1);
                    }
                    entries.push((k, v));
                    return None;
                }
                let mut large = crate::fasthash::FastMap::with_capacity_and_hasher(
                    entries.len() + 1,
                    Default::default(),
                );
                for (name, binding) in std::mem::take(entries) {
                    large.insert(name, binding);
                }
                let old = large.insert(k, v);
                self.map = VarStorage::Large(Box::new(large));
                old
            }
            VarStorage::Large(entries) => entries.insert(k, v),
        }
    }
    pub fn remove(&mut self, k: &str) -> Option<Binding> {
        self.bump();
        self.make_dynamic();
        match &mut self.map {
            VarStorage::Small(entries) => entries
                .iter()
                .position(|(name, _)| &**name == k)
                .map(|index| entries.swap_remove(index).1),
            VarStorage::Large(entries) => entries.remove(k),
            VarStorage::Template(..) => unreachable!("promoted above"),
        }
    }
    pub fn clear(&mut self) {
        self.bump();
        self.take_template();
        match &mut self.map {
            VarStorage::Small(entries) => entries.clear(),
            VarStorage::Large(entries) => entries.clear(),
            VarStorage::Template(..) => unreachable!("taken above"),
        }
    }
    /// In-place binding write: entries don't move, so the generation stays (see the type docs).
    pub fn get_mut(&mut self, k: &str) -> Option<&mut Binding> {
        match &mut self.map {
            VarStorage::Small(entries) => entries
                .iter_mut()
                .find(|(name, _)| &**name == k)
                .map(|(_, binding)| binding),
            VarStorage::Large(entries) => entries.get_mut(k),
            VarStorage::Template(layout, values) => {
                let slot = layout.slot(k)?;
                Some(&mut values.get_mut(layout)[slot])
            }
        }
    }
    pub fn get(&self, k: &str) -> Option<&Binding> {
        match &self.map {
            VarStorage::Small(entries) => entries
                .iter()
                .find(|(name, _)| &**name == k)
                .map(|(_, binding)| binding),
            VarStorage::Large(entries) => entries.get(k),
            VarStorage::Template(layout, values) => {
                layout.slot(k).map(|slot| &values.get(layout)[slot])
            }
        }
    }
    /// A copy of every binding (CreatePerIterationEnvironment). A small map's copy takes a
    /// shared layout for its names (see [`shared_layout`]), so every later iteration's copy
    /// carries just its values; a template's copy shares its layout. The copy's generation is
    /// non-zero: it is never a pristine compiled activation.
    pub(crate) fn copy_all(&self) -> VarMap {
        match &self.map {
            VarStorage::Small(entries) => {
                if let Some(layout) = shared_layout(entries) {
                    let values = entries.iter().map(|(_, b)| b.clone()).collect();
                    return Self::from_storage(VarStorage::Template(layout, Slots::new(values)), 1);
                }
                return Self::from_storage(VarStorage::Small(entries.clone()), 1);
            }
            VarStorage::Template(layout, values) => {
                let values = values.get(layout).to_vec();
                return Self::from_storage(
                    VarStorage::Template(layout.clone(), Slots::new(values)),
                    1,
                );
            }
            VarStorage::Large(_) => {}
        }
        let mut m = VarMap::with_capacity(0);
        for (k, v) in self.iter() {
            m.insert(k.clone(), v.clone());
        }
        m
    }
    /// [`VarMap::get`] by the interned name the binding was declared with: a pointer
    /// comparison per entry first, the string comparison only on a miss.
    #[inline]
    pub fn get_rc(&self, k: &Rc<str>) -> Option<&Binding> {
        match &self.map {
            VarStorage::Small(entries) => {
                if let Some((_, b)) = entries.iter().find(|(name, _)| Rc::ptr_eq(name, k)) {
                    return Some(b);
                }
            }
            VarStorage::Template(layout, values) => {
                if let Some(i) = layout.names.iter().position(|n| Rc::ptr_eq(n, k)) {
                    return Some(&values.get(layout)[i]);
                }
            }
            VarStorage::Large(_) => {}
        }
        self.get(k)
    }
    /// [`VarMap::get_mut`] by the interned name (see [`VarMap::get_rc`]).
    #[inline]
    pub fn get_rc_mut(&mut self, k: &Rc<str>) -> Option<&mut Binding> {
        match &mut self.map {
            VarStorage::Small(entries) => {
                let i = entries
                    .iter()
                    .position(|(name, _)| Rc::ptr_eq(name, k))
                    .or_else(|| entries.iter().position(|(name, _)| **name == **k))?;
                Some(&mut entries[i].1)
            }
            VarStorage::Large(entries) => entries.get_mut(&**k),
            VarStorage::Template(layout, values) => {
                let slot = match layout.names.iter().position(|n| Rc::ptr_eq(n, k)) {
                    Some(i) => i,
                    None => layout.slot(k)?,
                };
                Some(&mut values.get_mut(layout)[slot])
            }
        }
    }
    /// Mark this map as walked by a cached name resolution (see the `observed` field).
    #[inline]
    pub(crate) fn observe(&self) {
        self.observed.set(true);
    }
    /// The address of `k`'s binding (see [`VarMap::entry_ptrs`]).
    pub(crate) fn binding_ptr(&mut self, k: &str) -> Option<*mut Binding> {
        self.get_mut(k).map(|binding| binding as *mut Binding)
    }
    /// The addresses of `k`'s key and binding, for a per-site name cache. They stay valid until
    /// the next structural mutation of this map, which bumps the scope epoch.
    pub(crate) fn entry_ptrs(&mut self, k: &str) -> Option<(*const Rc<str>, *mut Binding)> {
        match &mut self.map {
            VarStorage::Small(entries) => entries
                .iter_mut()
                .find(|(name, _)| &**name == k)
                .map(|(name, binding)| (name as *const Rc<str>, binding as *mut Binding)),
            VarStorage::Large(entries) => {
                let name = entries.get_key_value(k)?.0 as *const Rc<str>;
                Some((name, entries.get_mut(k)? as *mut Binding))
            }
            VarStorage::Template(layout, values) => layout.slot(k).map(|slot| {
                (
                    &layout.names[slot] as *const Rc<str>,
                    &mut values.get_mut(layout)[slot] as *mut Binding,
                )
            }),
        }
    }
    pub fn contains_key(&self, k: &str) -> bool {
        self.get(k).is_some()
    }
    pub fn iter(&self) -> VarIter<'_> {
        match &self.map {
            VarStorage::Small(entries) => VarIter::Small(entries.iter()),
            VarStorage::Large(entries) => VarIter::Large(entries.iter()),
            VarStorage::Template(layout, values) => {
                VarIter::Template(layout.names.iter().zip(values.get(layout).iter()))
            }
        }
    }
    pub fn keys(&self) -> VarKeys<'_> {
        match &self.map {
            VarStorage::Small(entries) => VarKeys::Small(entries.iter()),
            VarStorage::Large(entries) => VarKeys::Large(entries.keys()),
            VarStorage::Template(layout, _) => VarKeys::Template(layout.names.iter()),
        }
    }
    pub fn values(&self) -> VarValues<'_> {
        match &self.map {
            VarStorage::Small(entries) => VarValues::Small(entries.iter()),
            VarStorage::Large(entries) => VarValues::Large(entries.values()),
            VarStorage::Template(layout, values) => VarValues::Template(values.get(layout).iter()),
        }
    }
}
