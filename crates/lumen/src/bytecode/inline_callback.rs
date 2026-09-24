//! Array callback methods with an arrow literal, lowered to a loop in the caller's frame.
//!
//! `arr.map(x => x + 1)` normally creates a closure and calls `Array.prototype.map`, which calls
//! the closure once per element: a closure allocation per call site execution plus a builtin
//! entry and a full call frame per element. When the callee is a *member call* of one of
//! [`CbMethod`]'s names whose first argument is an eligible arrow literal (see
//! [`Compiler::inline_array_callback`]), the compiler instead emits
//!
//! ```text
//! <obj>  GetMethod(name)  [<extra arg> StoreLocal(x)]      ; exactly one Get of the method
//! ArrayCbGuard(kind)  JumpIfFalse(fallback)                ; [O, F] -> [O, F, ok]
//!   Pop StoreLocal(a)  a.length → len                      ; fast path: a loop in this frame
//!   … per element: GetElem [ArrayCbHas when undefined] → the arrow's params → its body …
//!   ArrayCbDone(call position)
//!   Jump(end)
//! fallback:
//!   MakeClosure(arrow)  [LoadLocal(x)]  CallWithThis        ; exactly today's code
//! end:
//! ```
//!
//! `map` is the running example.
//!
//! ## The guard ([`Op::ArrayCbGuard`], evaluated once, after the arguments)
//! The fast path runs only when the receiver is an ordinary Array (not a proxy, no exotic
//! elements) whose prototype is this realm's `%Array.prototype%`, its `length` an own data
//! property, and the method the one real `Get` produced *is* the realm's intrinsic (recorded at
//! realm setup by [`record_intrinsics`], compared by object identity). `map`/`filter` also need
//! ArraySpeciesCreate to take its default route without running code: no own `constructor`,
//! `%Array.prototype%.constructor` the intrinsic `%Array%`, whose `@@species` is the original
//! getter. `reduce` without an initial value also needs the "no elements on the array
//! prototypes" protector (`Interp::array_append_unshadowed`), so its seed scan runs no user code
//! until it finds an element — an empty array can then take the fallback, which throws the
//! builtin's own TypeError.
//!
//! ## The loop (spec order, re-evaluated per element)
//! `len` is read once. Each index does exactly the builtin's step through
//! [`Op::ArrayCbElem`]: HasProperty then Get (`has`), or Get alone (`find*`), on the *current*
//! object — callbacks may push, pop, punch holes, add getters or swap the prototype, and every
//! such change is seen at the next step. `map` builds its result with the array-literal append
//! ops (a value, or a hole for an absent index, so the result has length `len` and holes where
//! the source had them); `filter` appends the kept `kValue`. Results are unobservable until
//! returned (ArraySpeciesCreate took the default route), which is what makes appending
//! equivalent to CreateDataPropertyOrThrow. `some`/`every`/`find*` exit early.
//!
//! ## The arrow
//! Its parameters get fresh slots of the caller (or alias the loop's own slots when a concise
//! body never assigns them); its `var`s are reset per iteration and its lexicals get their TDZ
//! per iteration (the body compiles as a block). It runs in the caller's frame, which is exactly
//! an arrow's `this`/`arguments`/`new.target`/`super`. `return` inside a block body jumps to
//! the per-element continuation. Refused (the call is compiled as before): async or generator
//! arrows, non-identifier/defaulted/rest parameters, a strictness different from the caller's,
//! direct `eval`, and any nested function that captures one of the arrow's own bindings (those
//! would need a fresh binding per iteration). Anything the body contains that the compiler
//! cannot lower rolls the attempt back ([`Checkpoint`]) and emits the generic call.
//!
//! ## Stack traces
//! The region between a guard and its [`Op::ArrayCbDone`] is an inlined callback. A frame whose
//! call site lies in one is expanded by `interpreter::stack_trace` into the arrow's frame (at
//! the call's position), `at Array.map (<anonymous>)`, and the caller at the map call's
//! position — V8's text. `ArrayCbElem` publishes its own pc as the frame's call site, so an
//! error thrown before the body's first call shows the arrow frame without a position, as a
//! real arrow frame that has made no call yet does. `ArrayCbDone` publishes the map call's
//! source position, as the returned builtin call would have left it.
//!
//! Known differences from the uninlined call: an element *getter* invoked by the step shows an
//! arrow frame above `Array.map`; legacy `f.caller` of a sloppy function called from the body
//! names the enclosing function instead of the arrow.
use super::{Bail, CResult, Chunk, CmpKind, Compiler, Op, UpdKind};
use crate::ast::{ArrayElem, Expr, Function, HoistOp, Pattern, Stmt};
use crate::interpreter::{Abrupt, Interp};
use crate::value::{Callable, Exotic, Gc, Value};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

/// The inlinable methods (the operand of [`Op::ArrayCbGuard`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum CbMethod {
    Map = 0,
    ForEach = 1,
    Filter = 2,
    Some = 3,
    Every = 4,
    Find = 5,
    FindIndex = 6,
    FindLast = 7,
    FindLastIndex = 8,
    Reduce = 9,
    /// `reduce` without an initial value (guards the elements protector too).
    ReduceNoInit = 10,
}

const METHODS: [CbMethod; 10] = [
    CbMethod::Map,
    CbMethod::ForEach,
    CbMethod::Filter,
    CbMethod::Some,
    CbMethod::Every,
    CbMethod::Find,
    CbMethod::FindIndex,
    CbMethod::FindLast,
    CbMethod::FindLastIndex,
    CbMethod::Reduce,
];

impl CbMethod {
    /// The method the compiler lowers for property name `n`: only the [`CbMethod::lowered`] ones.
    fn from_name(n: &str) -> Option<CbMethod> {
        METHODS.iter().copied().find(|m| m.name() == n && m.lowered())
    }

    /// Whether call sites of this method are lowered at all. All are: `map`'s per-element
    /// `ArrayAppend` runs through the JIT's direct `Helper::ArrayAppend` (it regressed while
    /// that op went through the generic-helper path; see docs/sync-callbacks.md).
    fn lowered(self) -> bool {
        true
    }

    pub(crate) fn from_u8(k: u8) -> Option<CbMethod> {
        match k {
            10 => Some(CbMethod::ReduceNoInit),
            _ => METHODS.get(k as usize).copied(),
        }
    }

    /// The property name (and the intrinsic's `name`).
    pub(crate) fn name(self) -> &'static str {
        match self {
            CbMethod::Map => "map",
            CbMethod::ForEach => "forEach",
            CbMethod::Filter => "filter",
            CbMethod::Some => "some",
            CbMethod::Every => "every",
            CbMethod::Find => "find",
            CbMethod::FindIndex => "findIndex",
            CbMethod::FindLast => "findLast",
            CbMethod::FindLastIndex => "findLastIndex",
            CbMethod::Reduce | CbMethod::ReduceNoInit => "reduce",
        }
    }

    /// V8's frame name for the builtin (`at Array.map (<anonymous>)`).
    pub(crate) fn frame_name(self) -> &'static str {
        match self {
            CbMethod::Map => "Array.map",
            CbMethod::ForEach => "Array.forEach",
            CbMethod::Filter => "Array.filter",
            CbMethod::Some => "Array.some",
            CbMethod::Every => "Array.every",
            CbMethod::Find => "Array.find",
            CbMethod::FindIndex => "Array.findIndex",
            CbMethod::FindLast => "Array.findLast",
            CbMethod::FindLastIndex => "Array.findLastIndex",
            CbMethod::Reduce | CbMethod::ReduceNoInit => "Array.reduce",
        }
    }

    /// Index into [`CbIntr::methods`].
    fn slot(self) -> usize {
        match self {
            CbMethod::ReduceNoInit => CbMethod::Reduce as usize,
            m => m as usize,
        }
    }

    /// `extra_protos` key of the realm's intrinsic.
    fn key(self) -> &'static str {
        match self {
            CbMethod::Map => "%InlineCb.map%",
            CbMethod::ForEach => "%InlineCb.forEach%",
            CbMethod::Filter => "%InlineCb.filter%",
            CbMethod::Some => "%InlineCb.some%",
            CbMethod::Every => "%InlineCb.every%",
            CbMethod::Find => "%InlineCb.find%",
            CbMethod::FindIndex => "%InlineCb.findIndex%",
            CbMethod::FindLast => "%InlineCb.findLast%",
            CbMethod::FindLastIndex => "%InlineCb.findLastIndex%",
            CbMethod::Reduce | CbMethod::ReduceNoInit => "%InlineCb.reduce%",
        }
    }

    fn needs_species(self) -> bool {
        matches!(self, CbMethod::Map | CbMethod::Filter)
    }
}

const ARRAY_KEY: &str = "%InlineCb.Array%";
const SPECIES_KEY: &str = "%InlineCb.species%";

// ---------------------------------------------------------------------------------------------
// Intrinsics
// ---------------------------------------------------------------------------------------------

/// Record the realm's array iteration methods, `%Array%` and its `@@species` getter as
/// installed (called once per realm by `builtins::install_array_rest`, before any user code).
pub(crate) fn record_intrinsics(it: &mut Interp, ap: &Gc, ctor: &Gc) {
    for m in METHODS {
        let f = match ap.borrow().props.get(m.name()).map(|p| p.value()) {
            Some(Value::Obj(f)) => f,
            _ => continue,
        };
        it.extra_protos.insert(m.key(), f);
    }
    it.extra_protos.insert(ARRAY_KEY, ctor.clone());
    if let Some(key) = crate::builtins::well_known_key(it, "species") {
        let getter = match ctor.borrow().props.get(&key) {
            Some(p) if p.accessor() => match p.getter() {
                Some(Value::Obj(g)) => Some(g.clone()),
                _ => None,
            },
            _ => None,
        };
        if let Some(g) = getter {
            it.extra_protos.insert(SPECIES_KEY, g);
        }
    }
}

/// A realm's recorded intrinsics, memoized by its `%Array.prototype%` (the held clones keep
/// every compared address alive).
pub(crate) struct CbIntr {
    array_proto: Gc,
    methods: [Option<Gc>; 10],
    array_ctor: Gc,
    species_getter: Gc,
    species_key: Rc<str>,
}

/// Per-interpreter memo state (a field of `LangCaches`).
#[derive(Default)]
pub(crate) struct CbCache {
    intr: RefCell<Option<Rc<CbIntr>>>,
    /// `constructor` on array instances (expected absent).
    own_ctor: crate::eval::fastpaths::SlotCache,
    /// `constructor` on `%Array.prototype%`.
    proto_ctor: crate::eval::fastpaths::SlotCache,
    /// `@@species` on `%Array%`.
    species: crate::eval::fastpaths::SlotCache,
}

fn intrinsics(i: &Interp) -> Option<Rc<CbIntr>> {
    let c = &i.lang.array_cb;
    if let Some(x) = &*c.intr.borrow() {
        if Gc::ptr_eq(&x.array_proto, &i.array_proto) {
            return Some(x.clone());
        }
    }
    let e = &i.extra_protos;
    let mut methods: [Option<Gc>; 10] = Default::default();
    for m in METHODS {
        methods[m.slot()] = e.get(m.key()).cloned();
    }
    let x = Rc::new(CbIntr {
        array_proto: i.array_proto.clone(),
        methods,
        array_ctor: e.get(ARRAY_KEY)?.clone(),
        species_getter: e.get(SPECIES_KEY)?.clone(),
        species_key: crate::builtins::well_known_key(i, "species")?,
    });
    *c.intr.borrow_mut() = Some(x.clone());
    Some(x)
}

/// Whether ArraySpeciesCreate(a, n) takes its default route without running code (see the
/// module docs). `a` is already known to be an ordinary Array of this realm.
fn species_pristine(i: &Interp, x: &CbIntr, a: &Gc) -> bool {
    let c = &i.lang.array_cb;
    {
        let Ok(b) = a.try_borrow() else { return false };
        if c.own_ctor.get(&b.props, "constructor").is_some() {
            return false;
        }
    }
    {
        let Ok(pb) = x.array_proto.try_borrow() else {
            return false;
        };
        match c.proto_ctor.get(&pb.props, "constructor") {
            Some(p) if !p.accessor() => match p.value() {
                Value::Obj(f) if Gc::ptr_eq(&f, &x.array_ctor) => {}
                _ => return false,
            },
            _ => return false,
        }
    }
    let Ok(cb) = x.array_ctor.try_borrow() else {
        return false;
    };
    if !cb.ic_plain.get() {
        return false;
    }
    match c.species.get(&cb.props, &x.species_key) {
        Some(p) if p.accessor() => {
            matches!(p.getter(), Some(Value::Obj(g)) if Gc::ptr_eq(g, &x.species_getter))
        }
        _ => false,
    }
}

/// Some guard has passed: a chunk may hold a live inlined region (see [`regions_at`]).
static USED: AtomicBool = AtomicBool::new(false);

/// Whether any inlined callback region has ever run (stack traces skip the lookup otherwise).
#[inline]
pub(crate) fn used() -> bool {
    USED.load(Ordering::Relaxed)
}

/// The guard's check: the receiver's length when the fast path applies.
fn check(i: &Interp, kind: u8, o: &Value, f: &Value) -> Option<usize> {
    let m = CbMethod::from_u8(kind)?;
    let (Value::Obj(a), Value::Obj(fg)) = (o, f) else {
        return None;
    };
    let x = intrinsics(i)?;
    if !x.methods[m.slot()].as_ref().is_some_and(|g| Gc::ptr_eq(g, fg)) {
        return None;
    }
    let len = {
        let b = a.try_borrow().ok()?;
        if !b.ic_plain.get()
            || !matches!(b.exotic, Exotic::Array)
            || !b.proto.as_ref().is_some_and(|p| Gc::ptr_eq(p, &i.array_proto))
        {
            return None;
        }
        match b.props.length_property() {
            Some(p) if !p.accessor() => match p.value() {
                Value::Num(n) if (0.0..=4294967295.0).contains(&n) => n as usize,
                _ => return None,
            },
            _ => return None,
        }
    };
    // The chain is `%Array.prototype%` → `%Object.prototype%` (immutable prototype) → null:
    // no proxy can observe the element steps' order.
    match i.array_proto.try_borrow().ok()?.proto.as_ref() {
        Some(p) if Gc::ptr_eq(p, &i.object_proto) => {}
        _ => return None,
    }
    if m.needs_species() && !species_pristine(i, &x, a) {
        return None;
    }
    if m == CbMethod::ReduceNoInit && !i.array_append_unshadowed(a) {
        return None;
    }
    Some(len)
}

/// [`Op::ArrayCbGuard`] at `pc`: `[O, F]` → `[O, F, ok]`. Never
/// throws. Publishes `pc` as the frame's call site: until the body makes a call, a trace taken
/// in the loop shows the inlined frames (see [`Chunk::inline_regions_at`]).
pub(super) fn guard(i: &mut Interp, kind: u8, pc: usize, s: &mut Vec<Value>) {
    let n = s.len();
    let len = check(i, kind, &s[n - 2], &s[n - 1]);
    if len.is_some() {
        if !used() {
            USED.store(true, Ordering::Relaxed);
        }
        i.cur_site = crate::interpreter::frames::SITE_PC | pc as u32;
    }
    s.push(Value::Bool(len.is_some()));
}

/// [`Op::ArrayCbHas`]: HasProperty(a, k), run only after `Get(a, k)` produced `undefined` (see
/// the module docs for why that order is unobservable here).
pub(super) fn has(i: &mut Interp, a: &Value, k: &Value) -> Result<bool, Abrupt> {
    let k = match k {
        Value::Num(k) if *k >= 0.0 && k.fract() == 0.0 => *k,
        _ => return Err(i.throw("TypeError", "array callback index out of range")),
    };
    if let (Value::Obj(o), Ok(n)) = (a, u32::try_from(k as u64)) {
        let beyond = {
            let Ok(b) = o.try_borrow() else {
                return i.js_has_property(a, &(k as u64).to_string());
            };
            if b.ic_plain.get() && matches!(b.exotic, Exotic::Array) {
                if b.props.get_index(n).is_some() {
                    return Ok(true);
                }
                // An Array has no own element at or past its length: the answer is the chain's.
                matches!(
                    b.props.length_property().map(|p| (p.accessor(), p.value())),
                    Some((false, Value::Num(len))) if k >= len
                )
            } else {
                false
            }
        };
        if beyond && i.array_append_unshadowed(o) {
            return Ok(false);
        }
    }
    i.js_has_property(a, &(k as u64).to_string())
}

// ---------------------------------------------------------------------------------------------
// Regions (stack traces)
// ---------------------------------------------------------------------------------------------

/// One inlined callback region of a finished chunk: `(guard pc, done pc)` exclusive, the
/// method call's source position and the method.
#[derive(Clone, Copy, Debug)]
pub(crate) struct InlineRegion {
    start: u32,
    end: u32,
    pos: u32,
    kind: u8,
}

fn scan_regions(ops: &[Op]) -> Box<[InlineRegion]> {
    let mut open: Vec<(u32, u8)> = Vec::new();
    let mut out = Vec::new();
    for (pc, op) in ops.iter().enumerate() {
        match *op {
            Op::ArrayCbGuard(k) => open.push((pc as u32, k)),
            Op::ArrayCbDone(pos) => {
                if let Some((start, kind)) = open.pop() {
                    out.push(InlineRegion {
                        start,
                        end: pc as u32,
                        pos,
                        kind,
                    });
                }
            }
            _ => {}
        }
    }
    out.sort_by_key(|r| r.start);
    out.into_boxed_slice()
}

impl Chunk {
    /// The inlined callback regions containing `pc`, innermost first: `(method call position,
    /// V8 frame name of the method)`.
    pub(crate) fn inline_regions_at(&self, pc: usize) -> Vec<(u32, &'static str)> {
        let regions = self.inline_cbs.get_or_init(|| scan_regions(&self.ops));
        let pc = pc as u32;
        let mut hits: Vec<&InlineRegion> = regions
            .iter()
            .filter(|r| r.start <= pc && pc < r.end)
            .collect();
        hits.sort_by_key(|r| std::cmp::Reverse(r.start));
        hits.iter()
            .map(|r| {
                let name = CbMethod::from_u8(r.kind).map_or("Array.<anonymous>", |m| m.frame_name());
                (r.pos, name)
            })
            .collect()
    }
}

/// The frames an inlined call site expands to: for `callee`'s compiled frame stopped at `pc`
/// inside inlined callback regions, the function (for its source), the innermost arrow frame's
/// position (the call op's own, or none), and per region, innermost first, the method call's
/// position and the builtin's frame name. `None` when `pc` lies in no region.
#[allow(clippy::type_complexity)]
pub(crate) fn expand(callee: &Gc, pc: usize) -> Option<(Rc<Function>, u32, Vec<(u32, &'static str)>)> {
    let func = match &callee.borrow().call {
        Callable::User(u) => u.func.clone(),
        _ => return None,
    };
    let (pos, hits) = match func.code.get() {
        Some(Some(chunk)) => (chunk.call_site_pos(pc), chunk.inline_regions_at(pc)),
        _ => return None,
    };
    if hits.is_empty() {
        return None;
    }
    Some((func, pos, hits))
}

// ---------------------------------------------------------------------------------------------
// Compile time
// ---------------------------------------------------------------------------------------------

/// Whether a call may compile to an inlined callback loop (`Function::scan_flags` counts it as
/// a loop, so its function compiles on the first call).
pub(crate) fn is_loop_call(callee: &Expr, args: &[ArrayElem]) -> bool {
    disabled_check()
        && matches!(callee, Expr::Member { optional: false, prop, .. } if CbMethod::from_name(prop).is_some())
        && matches!(args.first(), Some(ArrayElem::Item(Expr::Func(f))) if f.is_arrow)
        && args.len() <= 2
}

/// An inlined arrow body being compiled: where its `return`s go.
#[derive(Clone, Default)]
pub(super) struct InlineRet {
    /// Jumps (value on the stack) to the per-element continuation.
    exits: Vec<usize>,
    loops_len: usize,
    try_depth: u32,
    finallys_len: usize,
}

/// Compiler state to restore when an inline attempt bails (see the module docs).
struct Checkpoint {
    ops: usize,
    consts: usize,
    names: usize,
    scopes: usize,
    slot_names: usize,
    loops: usize,
    loop_jumps: Vec<(usize, usize)>,
    pending_labels: Vec<String>,
    uses_this: bool,
    obj_maps: u32,
    caches: usize,
    name_caches: usize,
    try_depth: u32,
    tdz_slots: std::collections::HashSet<u16>,
    tdz_pending: std::collections::HashSet<u16>,
    site: u32,
    sites: usize,
    funcs: usize,
    classes: usize,
    finallys: usize,
    assign_mode: bool,
    blk_envs: usize,
    pending_blk: usize,
    fresh_blk: bool,
    homed_pending: std::collections::HashSet<String>,
    tail_calls: bool,
    inline_ret: Option<InlineRet>,
}

/// Where a parameter's value comes from each iteration.
#[derive(Clone, Copy, PartialEq)]
enum Src {
    Elem,
    Index,
    Array,
    Acc,
}

/// The loop's slots.
struct Loop {
    a: u16,
    len: u16,
    k: u16,
    /// The element (`kValue`); also the first element parameter's own slot when aliased.
    e: u16,
    /// map/filter result, or reduce accumulator.
    r: u16,
}

impl Compiler {
    fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            ops: self.ops.len(),
            consts: self.consts.len(),
            names: self.names.len(),
            scopes: self.scopes.len(),
            slot_names: self.slot_names.len(),
            loops: self.loops.len(),
            loop_jumps: self
                .loops
                .iter()
                .map(|c| (c.breaks.len(), c.continues.len()))
                .collect(),
            pending_labels: self.pending_labels.clone(),
            uses_this: self.uses_this,
            obj_maps: self.obj_maps,
            caches: self.caches.len(),
            name_caches: self.name_caches.len(),
            try_depth: self.try_depth,
            tdz_slots: self.tdz_slots.clone(),
            tdz_pending: self.tdz_pending.clone(),
            site: self.site,
            sites: self.sites.len(),
            funcs: self.funcs.len(),
            classes: self.classes.len(),
            finallys: self.finallys.len(),
            assign_mode: self.assign_mode,
            blk_envs: self.blk_envs.len(),
            pending_blk: self.pending_blk.len(),
            fresh_blk: self.fresh_blk,
            homed_pending: self.homed_pending.clone(),
            tail_calls: self.tail_calls,
            inline_ret: self.inline_ret.clone(),
        }
    }

    fn rollback(&mut self, c: Checkpoint) {
        self.ops.truncate(c.ops);
        self.consts.truncate(c.consts);
        self.names.truncate(c.names);
        self.scopes.truncate(c.scopes);
        self.slot_names.truncate(c.slot_names);
        self.loops.truncate(c.loops);
        for (ctx, (b, k)) in self.loops.iter_mut().zip(c.loop_jumps) {
            ctx.breaks.truncate(b);
            ctx.continues.truncate(k);
        }
        self.pending_labels = c.pending_labels;
        self.uses_this = c.uses_this;
        self.obj_maps = c.obj_maps;
        self.caches.truncate(c.caches);
        self.name_caches.truncate(c.name_caches);
        self.try_depth = c.try_depth;
        self.tdz_slots = c.tdz_slots;
        self.tdz_pending = c.tdz_pending;
        self.site = c.site;
        self.sites.truncate(c.sites);
        self.funcs.truncate(c.funcs);
        self.classes.truncate(c.classes);
        self.finallys.truncate(c.finallys);
        self.assign_mode = c.assign_mode;
        self.blk_envs.truncate(c.blk_envs);
        self.pending_blk.truncate(c.pending_blk);
        self.fresh_blk = c.fresh_blk;
        self.homed_pending = c.homed_pending;
        self.tail_calls = c.tail_calls;
        self.inline_ret = c.inline_ret;
    }

    /// `obj.<prop>(args)` as an inlined callback loop when it qualifies (see the module docs).
    /// `true` = emitted; `false` = nothing emitted, compile the generic call.
    pub(super) fn inline_array_callback(&mut self, obj: &Expr, prop: &str, args: &[ArrayElem]) -> bool {
        let Some(m) = CbMethod::from_name(prop) else {
            return false;
        };
        let (f, extra) = match args {
            [ArrayElem::Item(Expr::Func(f))] => (f, None),
            [ArrayElem::Item(Expr::Func(f)), ArrayElem::Item(x)] => (f, Some(x)),
            _ => return false,
        };
        if !disabled_check() || !self.arrow_ok(f) {
            return false;
        }
        let kind = if m == CbMethod::Reduce && extra.is_none() {
            CbMethod::ReduceNoInit
        } else {
            m
        };
        let cp = self.checkpoint();
        let saved_reason = super::BAIL_REASON.with(|r| r.borrow().clone());
        match self.lower(obj, prop, kind, f, extra) {
            Ok(()) => true,
            Err(Bail) => {
                self.rollback(cp);
                super::BAIL_REASON.with(|r| *r.borrow_mut() = saved_reason);
                if super::bail_log_enabled() {
                    eprintln!("[tier] inline callback refused: .{prop}(arrow) compiles as a call");
                }
                false
            }
        }
    }

    /// The arrow-literal conditions that don't need a compile attempt.
    fn arrow_ok(&self, f: &Function) -> bool {
        if !f.is_arrow || f.is_async || f.is_generator || f.is_strict != self.strict {
            return false;
        }
        if !f
            .params
            .iter()
            .all(|p| !p.rest && p.default.is_none() && matches!(p.pattern, Pattern::Ident(_)))
        {
            return false;
        }
        if f.ensure_body().is_err() {
            return false;
        }
        // No nested function may capture the arrow's own bindings, no eval/with.
        match super::CaptureScan::run(f) {
            Some((captured, _, block_lets, _, blk_names)) => {
                captured.is_empty() && block_lets.is_empty() && blk_names.is_empty()
            }
            None => false,
        }
    }

    fn lower(
        &mut self,
        obj: &Expr,
        prop: &str,
        kind: CbMethod,
        f: &Rc<Function>,
        extra: Option<&Expr>,
    ) -> CResult {
        let call_pos = self.site.wrapping_sub(1);
        self.expr(obj)?;
        let n = self.name_idx(prop);
        let c = self.new_cache();
        self.emit(Op::GetMethod(n, c));
        let sx = match extra {
            Some(e) => {
                self.expr(e)?;
                let s = self.fresh_slot("%cb.arg");
                self.emit(Op::StoreLocal(s));
                Some(s)
            }
            None => None,
        };
        self.emit(Op::ArrayCbGuard(kind as u8));
        let j_fb = self.emit(Op::JumpIfFalse(0));
        let lp = Loop {
            a: self.fresh_slot("%cb.array"),
            len: self.fresh_slot("%cb.len"),
            k: self.fresh_slot("%cb.k"),
            e: u16::MAX,
            r: self.fresh_slot("%cb.result"),
        };
        let sf = if kind == CbMethod::ReduceNoInit {
            let s = self.fresh_slot("%cb.fn");
            self.emit(Op::StoreLocal(s));
            Some(s)
        } else {
            self.emit(Op::Pop);
            None
        };
        self.emit(Op::StoreLocal(lp.a));
        // `len` read once (the guard proved it an own data property holding a valid length, so
        // this Get is unobservable), through the property op the JIT types as a Number.
        self.emit(Op::LoadLocal(lp.a));
        let ln = self.name_idx("length");
        let lc = self.new_cache();
        self.emit(Op::GetProp(ln, lc));
        self.emit(Op::StoreLocal(lp.len));
        let seed_fail = self.fast_loop(kind, f, lp, sx)?;
        self.emit(Op::ArrayCbDone(call_pos));
        let j_end = self.emit(Op::Jump(0));
        // Reduce of an array with no element: nothing observable ran, so the builtin call
        // (which throws its own TypeError) takes over with the original operands.
        if let (Some(j), Some(sf)) = (seed_fail, sf) {
            self.patch(j.0);
            self.emit(Op::LoadLocal(j.1));
            self.emit(Op::LoadLocal(sf));
        }
        self.patch(j_fb);
        self.emit_closure(f, None);
        let argc: u16 = if let Some(s) = sx {
            self.emit(Op::LoadLocal(s));
            2
        } else {
            1
        };
        self.emit(Op::CallWithThis(argc));
        self.patch(j_end);
        Ok(())
    }

    /// Bind the arrow's parameters and `var`s in a fresh scope; returns the per-iteration copies
    /// (`(param slot, source)`, `None` = `undefined`), the `var` slots to reset, and the element
    /// slot.
    #[allow(clippy::type_complexity)]
    fn open_params(
        &mut self,
        f: &Function,
        roles: &[Src],
        lp: &Loop,
        body: &[Stmt],
    ) -> Result<(Vec<(u16, Option<Src>)>, Vec<u16>, u16), Bail> {
        let concise = match (f.expr_body, body) {
            (true, [Stmt::Return(Some(e))]) => Some(e),
            _ => None,
        };
        self.scopes.push(Vec::new());
        let mut copies = Vec::new();
        let mut elem_slot = None;
        let mut pnames = std::collections::HashSet::new();
        for (idx, p) in f.params.iter().enumerate() {
            let Pattern::Ident(name) = &p.pattern else {
                return Err(Bail);
            };
            pnames.insert(name.clone());
            let role = roles.get(idx).copied();
            let alias = match (role, concise) {
                (Some(r), Some(e)) if super::no_assign_to(e, name) => Some(r),
                _ => None,
            };
            match alias {
                Some(Src::Elem) => {
                    let s = self.fresh_slot(name);
                    self.scope_bind(name, s, false);
                    elem_slot = Some(s);
                }
                Some(Src::Index) => self.scope_bind(name, lp.k, false),
                Some(Src::Array) => self.scope_bind(name, lp.a, false),
                Some(Src::Acc) => self.scope_bind(name, lp.r, false),
                None => {
                    let s = self.fresh_slot(name);
                    self.scope_bind(name, s, false);
                    copies.push((s, role));
                }
            }
        }
        let mut resets = Vec::new();
        if concise.is_none() {
            for op in crate::interpreter::collect_hoist_ops(body, f.is_strict, &[]) {
                let forced = matches!(op, HoistOp::VarForce(_));
                match op {
                    HoistOp::Var(name) | HoistOp::VarForce(name) => {
                        if pnames.contains(&name) {
                            // A `for (var p …)` head re-declaring a parameter resets it at entry
                            // (see `Chunk::var_force_resets`); not modeled here.
                            if forced {
                                return Err(Bail);
                            }
                            continue;
                        }
                        if self.scopes.last().is_some_and(|s| s.iter().any(|(n, ..)| *n == name)) {
                            continue;
                        }
                        let s = self.fresh_slot(&name);
                        self.scope_bind(&name, s, false);
                        resets.push(s);
                    }
                    HoistOp::Fn(..) | HoistOp::AnnexB(..) => return Err(Bail),
                }
            }
        }
        let e = match elem_slot {
            Some(s) => s,
            None => self.fresh_slot("%cb.elem"),
        };
        Ok((copies, resets, e))
    }

    /// Store the parameters and reset the `var`s for one iteration.
    fn emit_binds(&mut self, copies: &[(u16, Option<Src>)], resets: &[u16], lp: &Loop) {
        for &(p, src) in copies {
            match src {
                Some(Src::Elem) => self.emit(Op::LoadLocal(lp.e)),
                Some(Src::Index) => self.emit(Op::LoadLocal(lp.k)),
                Some(Src::Array) => self.emit(Op::LoadLocal(lp.a)),
                Some(Src::Acc) => self.emit(Op::LoadLocal(lp.r)),
                None => self.emit(Op::Undef),
            };
            self.emit(Op::StoreLocal(p));
        }
        for &v in resets {
            self.emit(Op::Undef);
            self.emit(Op::StoreLocal(v));
        }
    }

    /// The arrow body, leaving its completion value (the concise expression, a `return`'s
    /// operand, or `undefined`) on the stack.
    fn inline_body(&mut self, f: &Function, body: &[Stmt]) -> CResult {
        if let (true, [Stmt::Return(Some(e))]) = (f.expr_body, body) {
            return self.expr(e);
        }
        let saved = self.inline_ret.replace(InlineRet {
            exits: Vec::new(),
            loops_len: self.loops.len(),
            try_depth: self.try_depth,
            finallys_len: self.finallys.len(),
        });
        let labels = std::mem::take(&mut self.pending_labels);
        let homed = std::mem::take(&mut self.homed_pending);
        let blk = std::mem::take(&mut self.blk_names);
        let tail = std::mem::replace(&mut self.tail_calls, false);
        self.scopes.push(Vec::new());
        let r = self.block_body(body);
        self.scopes.pop();
        self.tail_calls = tail;
        self.blk_names = blk;
        self.homed_pending = homed;
        self.pending_labels = labels;
        let ret = std::mem::replace(&mut self.inline_ret, saved).expect("pushed above");
        r?;
        self.emit(Op::Undef);
        for j in ret.exits {
            self.patch(j);
        }
        Ok(())
    }

    /// `return [e]` inside an inlined arrow body: the value to the per-element continuation.
    pub(super) fn inline_return(&mut self, arg: Option<&Expr>) -> CResult {
        let r = self.inline_ret.as_ref().expect("inside an inlined body");
        let (loops_len, floor, finallys_len) = (r.loops_len, r.try_depth, r.finallys_len);
        if self.finallys.len() > finallys_len
            || self.loops[loops_len..]
                .iter()
                .any(|c| c.foreach_iter.is_some() || c.foreach_async.is_some())
        {
            return Err(Bail);
        }
        match arg {
            Some(e) => self.expr(e)?,
            None => {
                self.emit(Op::Undef);
            }
        }
        for _ in floor..self.try_depth {
            self.emit(Op::PopHandler);
        }
        let j = self.emit(Op::Jump(0));
        self.inline_ret.as_mut().expect("checked").exits.push(j);
        Ok(())
    }

    /// One element step into slot `dst`: `Get(a, k)` (the JIT's native element read), and only
    /// when that is `undefined`, HasProperty (see the module docs). Falls through when present;
    /// returns the jump taken when the index is absent. The stack is empty on every path.
    fn element_step(&mut self, lp: &Loop, dst: u16, cu: u32) -> usize {
        self.emit(Op::LoadLocal(lp.a));
        self.emit(Op::LoadLocal(lp.k));
        self.emit(Op::GetElem);
        self.emit(Op::StoreLocal(dst));
        let j_def = self.emit(Op::JumpIfNotCmpLK(CmpKind::StrictEq, dst, cu, 0));
        self.emit(Op::LoadLocal(lp.a));
        self.emit(Op::LoadLocal(lp.k));
        self.emit(Op::ArrayCbHas);
        let j_absent = self.emit(Op::JumpIfFalse(0));
        self.patch(j_def);
        j_absent
    }

    /// The fast path: the loop, leaving the method's result on the stack. For `reduce` without
    /// an initial value, returns the "no element" jump (stack empty) with the array and length
    /// slots, which the caller routes to the builtin call.
    #[allow(clippy::type_complexity)]
    fn fast_loop(
        &mut self,
        kind: CbMethod,
        f: &Rc<Function>,
        mut lp: Loop,
        sx: Option<u16>,
    ) -> Result<Option<(usize, u16, u16)>, Bail> {
        use CbMethod as M;
        let body = f.body();
        let roles: &[Src] = match kind {
            M::Reduce | M::ReduceNoInit => &[Src::Acc, Src::Elem, Src::Index, Src::Array],
            _ => &[Src::Elem, Src::Index, Src::Array],
        };
        let (copies, resets, e) = self.open_params(f, roles, &lp, &body)?;
        lp.e = e;
        let c0 = self.const_idx(Value::Num(0.0));
        let mut seed_fail = None;
        // Result / accumulator setup and the start index.
        match kind {
            M::Map | M::Filter => {
                self.emit(Op::NewArrayLit);
                self.emit(Op::StoreLocal(lp.r));
            }
            M::Reduce => {
                self.emit(Op::LoadLocal(sx.ok_or(Bail)?));
                self.emit(Op::StoreLocal(lp.r));
            }
            _ => {}
        }
        match kind {
            M::FindLast | M::FindLastIndex => {
                self.emit(Op::LoadLocal(lp.len));
                self.emit(Op::StoreLocal(lp.k));
            }
            _ => {
                self.emit(Op::Const(c0));
                self.emit(Op::StoreLocal(lp.k));
            }
        }
        let cu = self.const_idx(Value::Undefined);
        if kind == M::ReduceNoInit {
            // Seed: the first present element, in index order.
            let seed = self.ops.len();
            let j_fail = self.emit(Op::JumpIfNotCmpLL(CmpKind::Lt, lp.k, lp.len, 0));
            let j_absent = self.element_step(&lp, lp.r, cu);
            self.emit(Op::UpdateLocal(lp.k, UpdKind::IncDiscard));
            let j_go = self.emit(Op::Jump(0));
            self.patch(j_absent);
            self.emit(Op::UpdateLocal(lp.k, UpdKind::IncDiscard));
            self.emit(Op::Jump(seed as u32));
            self.patch(j_go);
            seed_fail = Some((j_fail, lp.a, lp.len));
        }
        let has = !matches!(kind, M::Find | M::FindIndex | M::FindLast | M::FindLastIndex);
        let reverse = matches!(kind, M::FindLast | M::FindLastIndex);
        // Loop head.
        let top = self.ops.len();
        let j_exit = if reverse {
            let j = self.emit(Op::JumpIfNotCmpLK(CmpKind::Gt, lp.k, c0, 0));
            self.emit(Op::UpdateLocal(lp.k, UpdKind::DecDiscard));
            j
        } else {
            self.emit(Op::JumpIfNotCmpLL(CmpKind::Lt, lp.k, lp.len, 0))
        };
        let j_hole = if has {
            Some(self.element_step(&lp, lp.e, cu))
        } else {
            // find*: Get only.
            self.emit(Op::LoadLocal(lp.a));
            self.emit(Op::LoadLocal(lp.k));
            self.emit(Op::GetElem);
            self.emit(Op::StoreLocal(lp.e));
            None
        };
        self.emit_binds(&copies, &resets, &lp);
        self.inline_body(f, &body)?;
        // Consume the callback's value.
        let mut to_cont: Vec<usize> = Vec::new();
        let mut to_done: Vec<usize> = Vec::new();
        let mut to_fail: Vec<usize> = Vec::new();
        match kind {
            M::ForEach => {
                self.emit(Op::Pop);
            }
            M::Map => {
                let v = self.fresh_slot("%cb.value");
                self.emit(Op::StoreLocal(v));
                self.emit(Op::LoadLocal(lp.r));
                self.emit(Op::LoadLocal(v));
                self.emit(Op::ArrayAppend);
                self.emit(Op::Pop);
            }
            M::Filter => {
                to_cont.push(self.emit(Op::JumpIfFalse(0)));
                self.emit(Op::LoadLocal(lp.r));
                self.emit(Op::LoadLocal(lp.e));
                self.emit(Op::ArrayAppend);
                self.emit(Op::Pop);
            }
            M::Some => {
                to_cont.push(self.emit(Op::JumpIfFalse(0)));
                let t = self.const_idx(Value::Bool(true));
                self.emit(Op::Const(t));
                to_done.push(self.emit(Op::Jump(0)));
            }
            M::Every => {
                to_fail.push(self.emit(Op::JumpIfFalse(0)));
            }
            M::Find | M::FindLast => {
                to_cont.push(self.emit(Op::JumpIfFalse(0)));
                self.emit(Op::LoadLocal(lp.e));
                to_done.push(self.emit(Op::Jump(0)));
            }
            M::FindIndex | M::FindLastIndex => {
                to_cont.push(self.emit(Op::JumpIfFalse(0)));
                self.emit(Op::LoadLocal(lp.k));
                to_done.push(self.emit(Op::Jump(0)));
            }
            M::Reduce | M::ReduceNoInit => {
                self.emit(Op::StoreLocal(lp.r));
            }
        }
        // An absent index (the `has` step's hole): map keeps a hole in its result.
        if let Some(j) = j_hole {
            to_cont.push(self.emit(Op::Jump(0)));
            self.patch(j);
            if kind == M::Map {
                self.emit(Op::LoadLocal(lp.r));
                self.emit(Op::ArrayHole);
                self.emit(Op::Pop);
            }
        }
        for j in to_cont {
            self.patch(j);
        }
        if !reverse {
            self.emit(Op::UpdateLocal(lp.k, UpdKind::IncDiscard));
        }
        self.emit(Op::Jump(top as u32));
        self.scopes.pop();
        // Exits.
        self.patch(j_exit);
        match kind {
            M::ForEach | M::Find | M::FindLast => {
                self.emit(Op::Undef);
            }
            M::Map | M::Filter | M::Reduce | M::ReduceNoInit => {
                self.emit(Op::LoadLocal(lp.r));
            }
            M::Some => {
                let fl = self.const_idx(Value::Bool(false));
                self.emit(Op::Const(fl));
            }
            M::Every => {
                let t = self.const_idx(Value::Bool(true));
                self.emit(Op::Const(t));
                to_done.push(self.emit(Op::Jump(0)));
                for j in to_fail.drain(..) {
                    self.patch(j);
                }
                let fl = self.const_idx(Value::Bool(false));
                self.emit(Op::Const(fl));
            }
            M::FindIndex | M::FindLastIndex => {
                let m1 = self.const_idx(Value::Num(-1.0));
                self.emit(Op::Const(m1));
            }
        }
        for j in to_done {
            self.patch(j);
        }
        Ok(seed_fail)
    }
}

/// `LUMEN_INLINE_CB=0` turns the lowering off (A/B measurements, bisecting).
fn disabled_check() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("LUMEN_INLINE_CB").map_or(true, |v| v != "0"))
}

