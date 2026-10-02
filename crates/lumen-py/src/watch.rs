//! Watchers: callbacks the engine runs when a dict, a type, a code object or a function changes
//! (`PyDict_AddWatcher`, `PyType_AddWatcher`, `PyCode_AddWatcher`, `PyFunction_AddWatcher`), and
//! the type version tags (`tp_version_tag`) the type watchers key their events on.
//!
//! Changes that happen where no interpreter is at hand (a dict table mutating, an object being
//! freed) are queued and delivered by [`Interp::flush_watchers`], which the VM runs right after
//! the instructions that can cause them. A callback that fails is reported through
//! `sys.unraisablehook`.

use crate::dict::PyDict;
use crate::object::*;
use crate::vm::Interp;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

pub const MAX_WATCHERS: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Domain {
    Dict,
    Type,
    Code,
    Func,
}

impl Domain {
    fn index(self) -> usize {
        self as usize
    }

    fn word(self) -> &'static str {
        match self {
            Domain::Dict => "dict",
            Domain::Type => "type",
            Domain::Code => "code",
            Domain::Func => "func",
        }
    }

    fn invalid_id(self, id: i64) -> String {
        match self {
            Domain::Func => format!("invalid func watcher ID {id}"),
            d => format!("Invalid {} watcher ID {id}", d.word()),
        }
    }

    fn unset_id(self, id: i64) -> String {
        match self {
            Domain::Func => format!("no func watcher set for ID {id}"),
            d => format!("No {} watcher set for ID {id}", d.word()),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DictEvent {
    Added,
    Modified,
    Deleted,
    Cloned,
    Cleared,
    Deallocated,
}

impl DictEvent {
    pub fn name(self) -> &'static str {
        match self {
            DictEvent::Added => "PyDict_EVENT_ADDED",
            DictEvent::Modified => "PyDict_EVENT_MODIFIED",
            DictEvent::Deleted => "PyDict_EVENT_DELETED",
            DictEvent::Cloned => "PyDict_EVENT_CLONED",
            DictEvent::Cleared => "PyDict_EVENT_CLEARED",
            DictEvent::Deallocated => "PyDict_EVENT_DEALLOCATED",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FuncEvent {
    Create = 0,
    Destroy = 1,
    ModifyCode = 2,
    ModifyDefaults = 3,
    ModifyKwDefaults = 4,
}

impl FuncEvent {
    fn name(self) -> &'static str {
        match self {
            FuncEvent::Create => "PyFunction_EVENT_CREATE",
            FuncEvent::Destroy => "PyFunction_EVENT_DESTROY",
            FuncEvent::ModifyCode => "PyFunction_EVENT_MODIFY_CODE",
            FuncEvent::ModifyDefaults => "PyFunction_EVENT_MODIFY_DEFAULTS",
            FuncEvent::ModifyKwDefaults => "PyFunction_EVENT_MODIFY_KWDEFAULTS",
        }
    }
}

pub enum Event {
    Dict { event: DictEvent, key: Option<Value>, value: Option<Value> },
    Type(Obj),
    Code { create: bool },
    Func { event: FuncEvent, func: Option<Obj>, id: usize, value: Option<Value> },
}

pub type Callback = Rc<dyn Fn(&mut Interp, &Event) -> R<()>>;

enum Queued {
    Dict { mask: u8, addr: usize, event: DictEvent, key: Option<Value>, value: Option<Value> },
    Code { addr: usize },
    Func { id: usize },
}

thread_local! {
    static TABLE: RefCell<[[Option<Callback>; MAX_WATCHERS]; 4]> = RefCell::new(Default::default());
    static QUEUE: RefCell<Vec<Queued>> = const { RefCell::new(Vec::new()) };
    static PENDING: Cell<bool> = const { Cell::new(false) };
    static WATCHED_DICTS: RefCell<Vec<(usize, Weak<Object>)>> = const { RefCell::new(Vec::new()) };
    static CLEAR_EPOCH: Cell<u32> = const { Cell::new(1) };
    static NEXT_TAG: Cell<u32> = const { Cell::new(1) };
    static FUNC_WATCHED: Cell<bool> = const { Cell::new(false) };
    static CODE_WATCHED: Cell<bool> = const { Cell::new(false) };
}

/// The watcher bookkeeping, which follows the interpreter from thread to thread as the GIL
/// changes hands (see `threads`): it holds callbacks and queued objects.
pub(crate) struct WatchState {
    table: [[Option<Callback>; MAX_WATCHERS]; 4],
    queue: Vec<Queued>,
    pending: bool,
    watched_dicts: Vec<(usize, Weak<Object>)>,
    clear_epoch: u32,
    next_tag: u32,
    func_watched: bool,
    code_watched: bool,
}

pub(crate) fn state_take() -> WatchState {
    WatchState {
        table: TABLE.with(|t| std::mem::take(&mut *t.borrow_mut())),
        queue: QUEUE.with(|q| std::mem::take(&mut *q.borrow_mut())),
        pending: PENDING.with(|p| p.replace(false)),
        watched_dicts: WATCHED_DICTS.with(|w| std::mem::take(&mut *w.borrow_mut())),
        clear_epoch: CLEAR_EPOCH.with(|e| e.replace(1)),
        next_tag: NEXT_TAG.with(|n| n.replace(1)),
        func_watched: FUNC_WATCHED.with(|f| f.replace(false)),
        code_watched: CODE_WATCHED.with(|f| f.replace(false)),
    }
}

pub(crate) fn state_put(state: WatchState) {
    let table = TABLE.with(|t| std::mem::replace(&mut *t.borrow_mut(), state.table));
    let queue = QUEUE.with(|q| std::mem::replace(&mut *q.borrow_mut(), state.queue));
    PENDING.with(|p| p.set(state.pending));
    let dicts = WATCHED_DICTS.with(|w| std::mem::replace(&mut *w.borrow_mut(), state.watched_dicts));
    CLEAR_EPOCH.with(|e| e.set(state.clear_epoch));
    NEXT_TAG.with(|n| n.set(state.next_tag));
    FUNC_WATCHED.with(|f| f.set(state.func_watched));
    CODE_WATCHED.with(|f| f.set(state.code_watched));
    drop((table, queue, dicts));
}

fn set_pending() {
    let _ = PENDING.try_with(|p| p.set(true));
}

#[inline]
pub fn has_pending() -> bool {
    PENDING.with(|p| p.get())
}

fn refresh_flags() {
    let any = |d: Domain| TABLE.with(|t| t.borrow()[d.index()].iter().any(Option::is_some));
    FUNC_WATCHED.with(|f| f.set(any(Domain::Func)));
    CODE_WATCHED.with(|f| f.set(any(Domain::Code)));
}

/// Registers a callback in the first free slot of `d` (`Py*_AddWatcher`).
pub fn add(it: &mut Interp, d: Domain, cb: Callback) -> R<usize> {
    let slot = TABLE.with(|t| {
        let mut t = t.borrow_mut();
        let row = &mut t[d.index()];
        let id = row.iter().position(Option::is_none)?;
        row[id] = Some(cb);
        Some(id)
    });
    match slot {
        Some(id) => {
            refresh_flags();
            Ok(id)
        }
        None => Err(it.new_exc_str("RuntimeError", &format!("no more {} watcher IDs available", d.word()))),
    }
}

/// Checks that `id` names a registered watcher of `d`.
pub fn check(it: &mut Interp, d: Domain, id: i64) -> R<usize> {
    if !(0..MAX_WATCHERS as i64).contains(&id) {
        return Err(it.value_error(&d.invalid_id(id)));
    }
    let set = TABLE.with(|t| t.borrow()[d.index()][id as usize].is_some());
    if !set {
        return Err(it.value_error(&d.unset_id(id)));
    }
    Ok(id as usize)
}

/// `Py*_ClearWatcher`.
pub fn clear(it: &mut Interp, d: Domain, id: i64) -> R<()> {
    let id = check(it, d, id)?;
    TABLE.with(|t| t.borrow_mut()[d.index()][id] = None);
    refresh_flags();
    Ok(())
}

fn callbacks(d: Domain, mask: u8) -> Vec<(usize, Callback)> {
    TABLE.with(|t| {
        let t = t.borrow();
        (0..MAX_WATCHERS).filter(|i| mask & (1 << i) != 0).filter_map(|i| t[d.index()][i].clone().map(|c| (i, c))).collect()
    })
}

fn report(it: &mut Interp, e: Obj, message: String) {
    it.write_unraisable(&e, None, Some(&Value::string(message)));
}

/// `PyDict_Watch`.
pub fn watch_dict(it: &mut Interp, id: i64, dict: &Value) -> R<()> {
    let Value::Obj(o) = dict else { return Err(it.value_error("Cannot watch non-dictionary")) };
    let Kind::Dict(cell) = &o.kind else { return Err(it.value_error("Cannot watch non-dictionary")) };
    let id = check(it, Domain::Dict, id)?;
    let mut d = cell.borrow_mut();
    let addr = &*d as *const PyDict as usize;
    d.set_watch(d.watch() | (1 << id));
    drop(d);
    WATCHED_DICTS.with(|w| {
        let mut w = w.borrow_mut();
        w.retain(|(_, o)| o.strong_count() > 0);
        if !w.iter().any(|(a, _)| *a == addr) {
            w.push((addr, Rc::downgrade(o)));
        }
    });
    Ok(())
}

/// `PyDict_Unwatch`.
pub fn unwatch_dict(it: &mut Interp, id: i64, dict: &Value) -> R<()> {
    let Value::Obj(o) = dict else { return Err(it.value_error("Cannot watch non-dictionary")) };
    let Kind::Dict(cell) = &o.kind else { return Err(it.value_error("Cannot watch non-dictionary")) };
    let id = check(it, Domain::Dict, id)?;
    let mut d = cell.borrow_mut();
    d.set_watch(d.watch() & !(1 << id));
    Ok(())
}

/// Called by the dict table for a change of a watched dict.
pub(crate) fn dict_event(mask: u8, addr: usize, event: DictEvent, key: Option<&Value>, value: Option<&Value>) {
    let queued = Queued::Dict { mask, addr, event, key: key.cloned(), value: value.cloned() };
    let _ = QUEUE.try_with(|q| q.borrow_mut().push(queued));
    set_pending();
}

/// `PyType_Watch`: also gives the type a valid version tag, the state in which a change is
/// reported.
pub fn watch_type(it: &mut Interp, id: i64, ty: &Value) -> R<()> {
    let Value::Obj(o) = ty else { return Err(it.value_error("Cannot watch non-type")) };
    let Kind::Type(td) = &o.kind else { return Err(it.value_error("Cannot watch non-type")) };
    let id = check(it, Domain::Type, id)?;
    td.watched.set(td.watched.get() | (1 << id));
    td.flags.set(td.flags.get() | TF_WATCHED);
    assign_version(td);
    Ok(())
}

/// `PyType_Unwatch`.
pub fn unwatch_type(it: &mut Interp, id: i64, ty: &Value) -> R<()> {
    let Value::Obj(o) = ty else { return Err(it.value_error("Cannot watch non-type")) };
    let Kind::Type(td) = &o.kind else { return Err(it.value_error("Cannot watch non-type")) };
    let id = check(it, Domain::Type, id)?;
    let left = td.watched.get() & !(1 << id);
    td.watched.set(left);
    if left == 0 {
        td.flags.set(td.flags.get() & !TF_WATCHED);
    }
    Ok(())
}

fn epoch() -> u32 {
    CLEAR_EPOCH.with(Cell::get)
}

/// The type's version tag; 0 when none is assigned (never looked up since the last change or
/// since the type cache was cleared).
pub fn version_of(td: &TypeData) -> u32 {
    let v = td.version.get();
    if (v >> 32) as u32 == epoch() {
        v as u32
    } else {
        0
    }
}

/// `assign_version_tag`: gives the type a fresh tag unless it has a valid one.
pub fn assign_version(td: &TypeData) -> u32 {
    let cur = version_of(td);
    if cur != 0 {
        return cur;
    }
    let tag = NEXT_TAG.with(|n| {
        let t = n.get();
        n.set(t.wrapping_add(1).max(1));
        t
    });
    td.version.set(((epoch() as u64) << 32) | tag as u64);
    tag
}

/// Forces a tag (`type_assign_specific_version_unsafe`).
pub fn set_version(td: &TypeData, tag: u32) {
    td.version.set(((epoch() as u64) << 32) | tag as u64);
}

/// `sys._clear_type_cache`: every type loses its tag.
pub fn clear_type_cache() {
    CLEAR_EPOCH.with(|e| e.set(e.get().wrapping_add(1).max(1)));
}

/// Called on a type lookup, which gives the type a tag as the type cache does.
#[inline]
pub fn touch(td: &TypeData) {
    let v = td.version.get();
    if (v >> 32) as u32 != epoch() || v as u32 == 0 {
        assign_version(td);
    }
}

impl Interp {
    /// `PyType_Modified`: the type and its subclasses lose their tags, and the watchers of the
    /// ones that had one are told.
    pub fn type_modified(&mut self, ty: &Obj) {
        let Kind::Type(td) = &ty.kind else { return };
        if version_of(td) == 0 {
            return;
        }
        let subs: Vec<Obj> = self.subclass_registry.iter().filter_map(Weak::upgrade).filter(|s| s.type_data().is_some_and(|d| d.bases.borrow().iter().any(|b| Rc::ptr_eq(b, ty)))).collect();
        for s in subs {
            self.type_modified(&s);
        }
        let mask = td.watched.get();
        if mask != 0 {
            for (_, cb) in callbacks(Domain::Type, mask) {
                if let Err(e) = cb(self, &Event::Type(ty.clone())) {
                    self.write_unraisable(&e, None, Some(&Value::Obj(ty.clone())));
                }
            }
        }
        td.version.set(0);
    }

    /// `__call__` of a class was replaced: it and the subclasses that inherit its call lose
    /// `Py_TPFLAGS_HAVE_VECTORCALL`.
    pub fn type_call_changed(&mut self, ty: &Obj) {
        let Kind::Type(td) = &ty.kind else { return };
        td.flags.set(td.flags.get() & !TF_VECTORCALL);
        let subs: Vec<Obj> = self.subclass_registry.iter().filter_map(Weak::upgrade).filter(|s| s.type_data().is_some_and(|d| d.bases.borrow().first().is_some_and(|b| Rc::ptr_eq(b, ty)))).collect();
        for s in subs {
            let own_call = s.dict.borrow().as_ref().is_some_and(|d| matches!(&d.kind, Kind::Dict(d) if d.borrow().find_str(crate::object::hash_str("__call__"), "__call__").is_some()));
            if !own_call {
                self.type_call_changed(&s);
            }
        }
    }

    pub fn func_event(&mut self, event: FuncEvent, func: &Obj, value: Option<&Value>) {
        if !FUNC_WATCHED.with(Cell::get) {
            return;
        }
        let id = self.id_of(&Value::Obj(func.clone()));
        for (_, cb) in callbacks(Domain::Func, u8::MAX) {
            let ev = Event::Func { event, func: Some(func.clone()), id, value: value.cloned() };
            if let Err(e) = cb(self, &ev) {
                let repr = self.repr_of(&Value::Obj(func.clone())).unwrap_or_default();
                report(self, e, format!("{} watcher callback for {repr}", event.name()));
            }
        }
    }

    pub fn func_created(&mut self, func: &Obj) {
        self.func_event(FuncEvent::Create, func, None);
    }

    pub fn code_created(&mut self, code: &Obj) {
        if !CODE_WATCHED.with(Cell::get) {
            return;
        }
        for (_, cb) in callbacks(Domain::Code, u8::MAX) {
            if let Err(e) = cb(self, &Event::Code { create: true }) {
                let repr = self.repr_of(&Value::Obj(code.clone())).unwrap_or_default();
                report(self, e, format!("PY_CODE_EVENT_CREATE watcher callback for {repr}"));
            }
        }
    }

    /// Delivers the events queued by dict changes and frees.
    pub fn flush_watchers(&mut self) {
        loop {
            let batch = QUEUE.with(|q| std::mem::take(&mut *q.borrow_mut()));
            let _ = PENDING.try_with(|p| p.set(false));
            if batch.is_empty() {
                return;
            }
            for q in batch {
                match q {
                    Queued::Dict { mask, addr, event, key, value } => {
                        for (_, cb) in callbacks(Domain::Dict, mask) {
                            let ev = Event::Dict { event, key: key.clone(), value: value.clone() };
                            if let Err(e) = cb(self, &ev) {
                                report(self, e, format!("{} watcher callback for <dict at {addr:#x}>", event.name()));
                            }
                        }
                    }
                    Queued::Code { addr } => {
                        for (_, cb) in callbacks(Domain::Code, u8::MAX) {
                            if let Err(e) = cb(self, &Event::Code { create: false }) {
                                report(self, e, format!("PY_CODE_EVENT_DESTROY watcher callback for <code object at {addr:#x}>"));
                            }
                        }
                    }
                    Queued::Func { id } => {
                        for (_, cb) in callbacks(Domain::Func, u8::MAX) {
                            let ev = Event::Func { event: FuncEvent::Destroy, func: None, id, value: None };
                            if let Err(e) = cb(self, &ev) {
                                self.write_unraisable(&e, None, Some(&Value::string(format!("PyFunction_EVENT_DESTROY watcher callback for <function at {id:#x}>"))));
                            }
                        }
                    }
                }
            }
        }
    }
}

/// An object is being freed: a function that watchers know of is announced.
pub(crate) fn object_dropped(o: &Object) {
    match &o.kind {
        Kind::Function(_) => {
            if FUNC_WATCHED.try_with(Cell::get).unwrap_or(false) {
                let id = 0x7f3a_1c00_0000usize + o.identity() as usize * 48;
                let _ = QUEUE.try_with(|q| q.borrow_mut().push(Queued::Func { id }));
                set_pending();
            }
        }
        _ => {}
    }
}

/// A code object is being freed.
pub(crate) fn code_dropped(addr: usize) {
    if CODE_WATCHED.try_with(Cell::get).unwrap_or(false) {
        let _ = QUEUE.try_with(|q| q.borrow_mut().push(Queued::Code { addr }));
        set_pending();
    }
}
