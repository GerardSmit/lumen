//! Inlining of small `this` methods at direct method call sites (`recv.m(args)` whose
//! `GetMethod` the planner validated inline, see [`super::call`]).
//!
//! A method qualifies when its body is straight-line code over its parameters, constants and
//! the receiver's own data properties: `this.x` reads (`GetPropThis`), `this.x = v` stores
//! (`SetPropThisDrop`) and Number arithmetic / comparisons, ending in a `return`. The property
//! names are resolved at plan time against the receiver's shape, which the call re-checks (the
//! arguments may have run JS since the `GetMethod`); the method's identity is the direct
//! site's.
//!
//! Such a body can neither throw nor run JS once its inputs are known to be Numbers and its
//! properties plain data. So every check — the receiver's shape, each property being data (and
//! writable, holding a trivially droppable value, for a store), each operand being a Number —
//! is made before anything observable happens, and any failure takes the ordinary direct call
//! instead (nothing to undo). Stores are then committed in program order (reads after a store
//! of the same property see the stored value directly). No frame, no `fn_frames` record: the
//! inlined body cannot observe them.

use super::*;
use crate::bytecode::Op;
use lumen_codegen::ir::{Type, UnaryOp, Value as V};

/// Ops beyond which a method is not inlined.
const MAX_OPS: usize = 32;

/// A method body inlinable on receivers of one shape (see the module docs).
#[derive(Clone)]
pub(super) struct ThisBody {
    /// Weak: bodies are kept in per-site feedback, which must not keep callees alive. The
    /// compile that uses a body pins its callee, whose closure holds the chunk.
    pub chunk: std::rc::Weak<Chunk>,
    /// The receiver's shape id the property slots are resolved against.
    pub shape: u32,
    /// Per op: the receiver's entry slot a `GetPropThis` / `SetPropThisDrop` names.
    slots: Vec<u32>,
    /// The largest slot used; `None` when the body touches no slot (nothing to bound-check).
    max_slot: Option<u32>,
    /// Whether the body stores a property.
    writes: bool,
    /// The number of ops up to and including the first return.
    len: usize,
    /// The receiver was validated by the site's `GetMethod` with nothing run since (only
    /// loads between it and the call): its shape needs no re-check.
    pub fresh: bool,
}

/// The private-name resolver of function `f` (a `#x` of its body -> the key of the class
/// evaluation `f` closed over; fixed for `f`, whose identity the site guards).
pub(super) fn private_keys<'a>(i: &'a Interp, f: &Value) -> impl Fn(&str) -> Option<String> + 'a {
    let env = match f {
        Value::Obj(o) => o.try_borrow().ok().and_then(|b| match &b.call {
            crate::value::Callable::User(u) => Some(u.env.clone()),
            _ => None,
        }),
        _ => None,
    };
    move |name: &str| {
        let k = i.resolve_private(name, env.as_ref()?);
        Interp::is_private_key(&k).then_some(k)
    }
}

/// The body of method `chunk` as inlinable on `recv` (of shape `shape`), when it qualifies.
/// `private`: the method's private-name resolver (see [`private_keys`]).
pub(super) fn this_body(
    chunk: &std::rc::Rc<Chunk>,
    recv: &Value,
    shape: u32,
    private: &dyn Fn(&str) -> Option<String>,
) -> Option<ThisBody> {
    if chunk.ops.len() > MAX_OPS
        || chunk.activation_layout.is_some()
        || chunk.arguments_slot.is_some()
        || chunk.rest_slot.is_some()
        || chunk.env_this
        || chunk.derived
    {
        return None;
    }
    let Value::Obj(o) = recv else { return None };
    let b = o.try_borrow().ok()?;
    if !matches!(b.exotic, crate::value::Exotic::None)
        || !b.ic_plain.get()
        || b.props.shape() != shape
        || !b.props.shape_is_shared()
    {
        return None;
    }
    let mut slots = vec![u32::MAX; chunk.ops.len()];
    let mut max_slot: Option<u32> = None;
    let mut writes = false;
    let mut depth: usize = 0;
    // Per stack position: holds the receiver (`LoadThis`), which only a private access or a
    // `Pop` may consume.
    let mut this_at: Vec<bool> = Vec::new();
    for (pc, op) in chunk.ops.iter().enumerate() {
        let top_this = this_at.last() == Some(&true);
        let (pops, pushes) = match *op {
            Op::LoadThis => {
                this_at.push(true);
                depth += 1;
                continue;
            }
            Op::GetPrivate(n) | Op::SetPrivate(n) => {
                let set = matches!(op, Op::SetPrivate(_));
                let at = this_at.len().checked_sub(if set { 2 } else { 1 })?;
                if !this_at[at] || (set && top_this) {
                    return None;
                }
                let key = private(chunk.names.get(n as usize)?)?;
                let slot = b.props.slot_of(&key)?;
                if b.props.entry_at(slot)?.accessor() {
                    return None;
                }
                let slot = u32::try_from(slot).ok()?;
                slots[pc] = slot;
                max_slot = max_slot.max(Some(slot));
                writes |= set;
                if set {
                    (2, 1)
                } else {
                    (1, 1)
                }
            }
            Op::Pop => (1, 0),
            _ if top_this => return None,
            Op::GetPropThis(n, _) | Op::SetPropThisDrop(n, _) => {
                let name = chunk.names.get(n as usize)?;
                let slot = b.props.slot_of(name)?;
                if b.props.entry_at(slot)?.accessor() {
                    return None;
                }
                let slot = u32::try_from(slot).ok()?;
                slots[pc] = slot;
                max_slot = max_slot.max(Some(slot));
                if matches!(op, Op::SetPropThisDrop(..)) {
                    writes = true;
                    (1, 0)
                } else {
                    (0, 1)
                }
            }
            Op::LoadLocal(s) if (s as usize) < chunk.n_params => (0, 1),
            Op::Const(k) => match chunk.consts.get(k as usize)? {
                Value::Num(_) | Value::Bool(_) => (0, 1),
                _ => return None,
            },
            Op::Undef => (0, 1),
            Op::Dup => (1, 2),
            Op::Neg | Op::Plus => (1, 1),
            Op::Return => {
                return (depth == 1).then_some(ThisBody {
                    chunk: std::rc::Rc::downgrade(chunk),
                    shape,
                    slots,
                    max_slot,
                    writes,
                    len: pc + 1,
                    fresh: false,
                })
            }
            Op::ReturnUndef => {
                return (depth == 0).then_some(ThisBody {
                    chunk: std::rc::Rc::downgrade(chunk),
                    shape,
                    slots,
                    max_slot,
                    writes,
                    len: pc + 1,
                    fresh: false,
                })
            }
            ref o if inline::arith_of(o).is_some() || inline::cmp_of(o).is_some() => (2, 1),
            _ => return None,
        };
        depth = depth.checked_sub(pops)? + pushes;
        this_at.truncate(this_at.len().checked_sub(pops)?);
        this_at.resize(depth, false);
    }
    None
}

/// An abstract operand of the inlined body.
#[derive(Clone, Copy)]
enum Av {
    Num(V),
    Bool(V),
    Undef,
    /// A property's NaN-boxed word (I64), of any kind.
    Word(V),
    /// Argument entry `k` of the call (its kind is the caller's).
    Arg(usize),
    /// The receiver (consumed by a private access).
    This,
}

/// A kind-only [`Av`], for the dry run.
#[derive(Clone, Copy, PartialEq)]
enum K {
    Num,
    Bool,
    Undef,
    Word,
    /// An argument in memory (a Number at run time, or the call goes the ordinary way).
    Mem,
    /// The receiver (consumed by a private access).
    This,
}

fn numeric(k: K) -> bool {
    matches!(k, K::Num | K::Word | K::Mem)
}

/// The dry run of `tb`'s body over operand kinds (`arg(k)`: argument `k`'s): the kind of its
/// result when every operand fits, else `None`.
fn dry(tb: &ThisBody, arg: &dyn Fn(usize) -> K) -> Option<K> {
    let mut st: Vec<K> = Vec::new();
    let mut stored: Vec<(u32, K)> = Vec::new();
    let mut ret = K::Undef;
    let ch = tb.chunk.upgrade()?;
    for (q, op) in ch.ops[..tb.len].iter().enumerate() {
        match *op {
            Op::LoadThis => st.push(K::This),
            Op::GetPropThis(..) | Op::GetPrivate(_) => {
                if matches!(op, Op::GetPrivate(_)) {
                    st.pop()?;
                }
                let s = tb.slots[q];
                st.push(match stored.iter().rev().find(|x| x.0 == s) {
                    Some(&(_, k)) => k,
                    None => K::Word,
                });
            }
            Op::SetPropThisDrop(..) | Op::SetPrivate(_) => {
                let k = match st.pop()? {
                    k @ (K::Num | K::Bool | K::Undef) => k,
                    K::Word | K::Mem => K::Num,
                    K::This => return None,
                };
                stored.push((tb.slots[q], k));
                if matches!(op, Op::SetPrivate(_)) {
                    st.pop()?;
                    st.push(k);
                }
            }
            Op::LoadLocal(s) => st.push(arg(s as usize)),
            Op::Const(k) => st.push(match ch.consts[k as usize] {
                Value::Num(_) => K::Num,
                _ => K::Bool,
            }),
            Op::Undef => st.push(K::Undef),
            Op::Dup => {
                let t = *st.last()?;
                st.push(t);
            }
            Op::Pop => {
                st.pop()?;
            }
            Op::Neg | Op::Plus => {
                if !numeric(st.pop()?) {
                    return None;
                }
                st.push(K::Num);
            }
            Op::Return => ret = st.pop().filter(|k| *k != K::This)?,
            Op::ReturnUndef => ret = K::Undef,
            ref o => {
                let (y, x) = (st.pop()?, st.pop()?);
                if !numeric(x) || !numeric(y) {
                    return None;
                }
                st.push(if inline::arith_of(o).is_some() { K::Num } else { K::Bool });
            }
        }
    }
    Some(ret)
}

/// The checked result of an inlined body, ready to commit.
enum Res {
    /// Wanted as a Number.
    Num(V),
    /// A property word: its `(tag, payload, is_ref)` (see [`layout::word_value`]).
    Word(V, (V, V, V)),
    Other(Av),
}

impl Tr<'_, '_> {
    /// The body of `tb` on the receiver whose entries start at `eb`, arguments `entries[ab..]`
    /// (`argc` of them): every check branches to `full`, before anything is stored. Returns
    /// the checked result (as a Number when `want`) and the stores to commit.
    #[allow(clippy::too_many_arguments)]
    fn this_emit(
        &mut self,
        tb: &ThisBody,
        eb: V,
        entries: &[Entry],
        ab: usize,
        argc: usize,
        want: bool,
        full: Block,
    ) -> (Res, Vec<(u32, Av)>) {
        let mut st: Vec<Av> = Vec::new();
        let mut stored: Vec<(u32, Av)> = Vec::new();
        let mut checked: Vec<u32> = Vec::new();
        let mut ret = Av::Undef;
        // Alive: the compile pins the callee, whose closure holds the chunk.
        let ch = tb.chunk.upgrade().expect("inlined body's chunk");
        for (q, op) in ch.ops[..tb.len].iter().enumerate() {
            match *op {
                Op::LoadThis => st.push(Av::This),
                Op::GetPropThis(..) | Op::GetPrivate(_) => {
                    if matches!(op, Op::GetPrivate(_)) {
                        st.pop();
                    }
                    let s = tb.slots[q];
                    let v = match stored.iter().rev().find(|x| x.0 == s) {
                        Some(&(_, v)) => v,
                        None => Av::Word(layout::entry_word(&mut self.fb, eb, s, full)),
                    };
                    st.push(v);
                }
                Op::SetPropThisDrop(..) | Op::SetPrivate(_) => {
                    let v = st.pop().expect("dry run");
                    let v = match v {
                        Av::Word(_) | Av::Arg(_) => Av::Num(self.this_num(v, entries, ab, full)),
                        v => v,
                    };
                    let s = tb.slots[q];
                    if !checked.contains(&s) {
                        layout::entry_writable(&mut self.fb, eb, s, full);
                        checked.push(s);
                    }
                    stored.push((s, v));
                    if matches!(op, Op::SetPrivate(_)) {
                        st.pop();
                        st.push(v);
                    }
                }
                Op::LoadLocal(s) => st.push(if (s as usize) < argc {
                    match entries[ab + s as usize] {
                        Entry::Num(x) => Av::Num(x),
                        Entry::Bool(b) => Av::Bool(b),
                        _ => Av::Arg(s as usize),
                    }
                } else {
                    Av::Undef
                }),
                Op::Const(k) => st.push(match ch.consts[k as usize] {
                    Value::Num(x) => Av::Num(self.fb.f64const(x)),
                    Value::Bool(b) => Av::Bool(self.i32c(b as i64)),
                    _ => Av::Undef,
                }),
                Op::Undef => st.push(Av::Undef),
                Op::Dup => st.push(*st.last().expect("dry run")),
                Op::Pop => {
                    st.pop();
                }
                Op::Neg => {
                    let v = st.pop().expect("dry run");
                    let x = self.this_num(v, entries, ab, full);
                    st.push(Av::Num(self.fb.unary(UnaryOp::Fneg, x)));
                }
                Op::Plus => {
                    let v = st.pop().expect("dry run");
                    st.push(Av::Num(self.this_num(v, entries, ab, full)));
                }
                Op::Return => ret = st.pop().expect("dry run"),
                Op::ReturnUndef => ret = Av::Undef,
                ref o => {
                    let b = st.pop().expect("dry run");
                    let a = st.pop().expect("dry run");
                    let x = self.this_num(a, entries, ab, full);
                    let y = self.this_num(b, entries, ab, full);
                    let r = match inline::arith_of(o) {
                        Some(k) => match self.num_arith(k, x, y) {
                            Ok(r) => Av::Num(r),
                            // (Not reached: every arithmetic kind has a Number form.)
                            Err(_) => {
                                self.fb.jump(full, &[]);
                                let dead = self.fb.create_block();
                                self.fb.seal_block(dead);
                                self.fb.switch_to_block(dead);
                                Av::Num(x)
                            }
                        },
                        None => {
                            let k = inline::cmp_of(o).expect("dry run");
                            Av::Bool(self.inl_cmp(k, x, y, true))
                        }
                    };
                    st.push(r);
                }
            }
        }
        let res = match (want, ret) {
            (true, v) => Res::Num(self.this_num(v, entries, ab, full)),
            (false, Av::Word(bits)) => {
                Res::Word(bits, layout::word_value(&mut self.fb, bits, full))
            }
            (false, v) => Res::Other(v),
        };
        (res, stored)
    }

    /// Commit the stores of an inlined body.
    fn this_commit(&mut self, eb: V, stored: &[(u32, Av)]) {
        for &(s, v) in stored {
            let w = match v {
                Av::Num(x) => layout::num_word(&mut self.fb, x),
                Av::Bool(b) => layout::bool_word(&mut self.fb, b),
                _ => self
                    .fb
                    .iconst(Type::I64, crate::value::PACK_UNDEFINED as i64),
            };
            layout::entry_store(&mut self.fb, eb, s, w);
        }
        if !stored.is_empty() {
            // (As for any property store: drop the tracked array views.)
            let views: Vec<(lumen_codegen::builder::Variable, Type)> =
                self.arr_vars.values().map(|a| (a.kind, Type::I32)).collect();
            self.reset_vars(&views);
        }
    }

    /// Write the committed result `res` to stack index `at` (or pass it to `done` when it is
    /// wanted as a Number), then jump to `done`. `drop`: a Boxed stack entry to release once the
    /// result is taken — the receiver the result may have been read from (possibly at `at`).
    fn this_result(&mut self, res: Res, at: usize, drop: Option<usize>, done: Block) {
        let o = self.soff(at);
        match res {
            Res::Num(f) => {
                if let Some(i) = drop {
                    self.drop_at(i);
                }
                self.fb.jump(done, &[f]);
            }
            Res::Word(bits, (tag, payload, is_ref)) => {
                let refc = self.fb.create_block();
                let copy = self.fb.create_block();
                self.fb.brif(is_ref, refc, &[], copy, &[]);
                self.fb.seal_block(refc);
                self.fb.seal_block(copy);
                self.fb.switch_to_block(refc);
                // The new reference is taken before the receiver goes.
                let dst = self.sptr(at);
                let cv = self.i32c((drop == Some(at)) as i64);
                self.call(Helper::UnpackClone, &[dst, bits, cv]);
                if let Some(i) = drop.filter(|&i| i != at) {
                    self.drop_at(i);
                }
                self.fb.jump(done, &[]);
                self.fb.switch_to_block(copy);
                if let Some(i) = drop {
                    self.drop_at(i);
                }
                self.fb.store(MemKind::I32U8, self.stackp, tag, o);
                let byte = self.fb.convert(ConvOp::Wrap, Type::I32, payload);
                self.fb.store(MemKind::I32U8, self.stackp, byte, o + VALUE_BOOL);
                self.fb.store(MemKind::I64, self.stackp, payload, o + VALUE_PAYLOAD);
                self.fb.jump(done, &[]);
            }
            Res::Other(v) => {
                if let Some(i) = drop {
                    self.drop_at(i);
                }
                match v {
                    Av::Num(x) => {
                        let t = self.i32c(TAG_NUM as i64);
                        self.fb.store(MemKind::I32U8, self.stackp, t, o);
                        self.fb.store(MemKind::F64, self.stackp, x, o + VALUE_PAYLOAD);
                    }
                    Av::Bool(b) => {
                        let t = self.i32c(TAG_BOOL as i64);
                        self.fb.store(MemKind::I32U8, self.stackp, t, o);
                        self.fb.store(MemKind::I32U8, self.stackp, b, o + VALUE_BOOL);
                    }
                    _ => {
                        let t = self.i32c(TAG_UNDEFINED as i64);
                        self.fb.store(MemKind::I32U8, self.stackp, t, o);
                    }
                }
                self.fb.jump(done, &[]);
            }
        }
    }

    /// The call of a direct method site with an inlinable body (see the module docs): `false`
    /// (nothing emitted) when the call's operands do not fit, and the ordinary direct call is
    /// to be emitted instead.
    pub(super) fn inline_this_call(&mut self, pc: usize, argc: usize, tb: &ThisBody) -> bool {
        let d = self.stack.len();
        let base = d - argc - 2;
        let ab = base + 2;
        let entries = self.stack.clone();
        if matches!(entries[base], Entry::Num(_) | Entry::Bool(_)) {
            return false;
        }
        let arg_kind = |k: usize| -> K {
            if k >= argc {
                return K::Undef;
            }
            match entries[ab + k] {
                Entry::Num(_) => K::Num,
                Entry::Bool(_) => K::Bool,
                Entry::Boxed | Entry::Ref(..) => K::Mem,
            }
        };
        let Some(ret) = dry(tb, &arg_kind) else {
            return false;
        };
        let want = numeric(ret) && self.want_num(pc, base);
        if ret == K::Mem && !want {
            return false;
        }

        let full = self.fb.create_block();
        let done = self.fb.create_block();
        let param = want.then(|| self.fb.append_block_param(done, Type::F64));
        let this_p = match entries[base] {
            Entry::Ref(p, _) => p,
            _ => self.sptr(base),
        };
        let eb = if tb.fresh {
            layout::probed_entries(&mut self.fb, this_p, tb.max_slot, tb.writes, full)
        } else {
            layout::shaped_entries(
                &mut self.fb,
                this_p,
                tb.shape,
                tb.max_slot,
                tb.writes,
                full,
            )
        };
        let (res, stored) = self.this_emit(tb, eb, &entries, ab, argc, want, full);
        self.this_commit(eb, &stored);
        for k in ab..d {
            if entries[k] == Entry::Boxed {
                self.drop_at(k);
            }
        }
        let recv = (entries[base] == Entry::Boxed).then_some(base);
        self.this_result(res, base, recv, done);

        // ---- anything else: the ordinary direct call ----
        self.fb.seal_block(full);
        self.fb.switch_to_block(full);
        self.stack = entries;
        self.direct_call_full(pc, argc, true);
        self.slow_to_join(pc, base, want, done);
        self.fb.seal_block(done);
        self.fb.switch_to_block(done);
        self.stack.truncate(base);
        self.stack.push(match param {
            Some(p) => Entry::Num(p),
            None => Entry::Boxed,
        });
        true
    }

    /// A property read `obj.<name>` (at `pc`, result at stack index `at`; `drop`: the consumed
    /// Boxed receiver's index) that the planner resolved to a getter with an inlinable body
    /// (see [`Getter`]): the getter's identity is checked along the receiver's lookup chain,
    /// then its body runs inline; anything else takes the ordinary read. `false` (nothing
    /// emitted) when the body does not fit.
    pub(super) fn inline_getter(
        &mut self,
        pc: usize,
        obj: V,
        drop: Option<usize>,
        at: usize,
        slow: Slow,
        g: &Getter,
    ) -> bool {
        let Some(ret) = dry(&g.body, &|_| K::Undef) else {
            return false;
        };
        let want = numeric(ret) && self.want_num(pc, at);
        let pre = self.stack.clone();
        let miss = self.fb.create_block();
        let join = self.fb.create_block();
        let param = want.then(|| self.fb.append_block_param(join, Type::F64));
        layout::getter_probe(&mut self.fb, obj, &g.shapes, g.slot, g.getter as u64, miss);
        let eb = layout::probed_entries(&mut self.fb, obj, g.body.max_slot, false, miss);
        let (res, _) = self.this_emit(&g.body, eb, &pre, 0, 0, want, miss);
        self.this_result(res, at, drop, join);

        self.fb.seal_block(miss);
        self.fb.switch_to_block(miss);
        self.stack = pre.clone();
        if self.slow_path(pc, slow) {
            self.slow_to_join(pc, at, want, join);
        }
        self.fb.seal_block(join);
        self.fb.switch_to_block(join);
        self.stack = pre[..at].to_vec();
        self.stack.push(match param {
            Some(p) => Entry::Num(p),
            None => Entry::Boxed,
        });
        true
    }

    /// A property store `obj.<name> = v` (at `pc`, the value on top of the stack; `consume`:
    /// the receiver's stack index when the store consumes it) that the planner resolved to a
    /// setter with an inlinable body: the setter's identity is checked along the receiver's
    /// lookup chain, then its body runs inline with `v` as its argument, and the operands are
    /// released. Leaves the translation in the block where anything else continues (with the
    /// stack as before), after emitting a jump to `done` for the inline case; `false` (nothing
    /// emitted) when the body does not fit.
    pub(super) fn inline_setter(
        &mut self,
        obj: V,
        consume: Option<usize>,
        g: &Getter,
        done: Block,
    ) -> bool {
        let d = self.stack.len();
        let kind = match self.stack[d - 1] {
            Entry::Num(_) => K::Num,
            Entry::Bool(_) => K::Bool,
            Entry::Boxed | Entry::Ref(..) => K::Mem,
        };
        if dry(&g.body, &|k| if k == 0 { kind } else { K::Undef }).is_none() {
            return false;
        }
        let pre = self.stack.clone();
        let miss = self.fb.create_block();
        layout::accessor_probe(&mut self.fb, obj, &g.shapes, g.slot, g.getter as u64, true, miss);
        let eb = layout::probed_entries(&mut self.fb, obj, g.body.max_slot, g.body.writes, miss);
        let (_, stored) = self.this_emit(&g.body, eb, &pre, d - 1, 1, false, miss);
        self.this_commit(eb, &stored);
        if pre[d - 1] == Entry::Boxed {
            self.drop_at(d - 1);
        }
        if let Some(i) = consume {
            self.drop_at(i);
        }
        self.fb.jump(done, &[]);
        self.fb.seal_block(miss);
        self.fb.switch_to_block(miss);
        self.stack = pre;
        true
    }

    /// Operand `v` of an inlined body as a Number (F64), branching to `miss` when it is not one.
    fn this_num(&mut self, v: Av, entries: &[Entry], ab: usize, miss: Block) -> V {
        match v {
            Av::Num(x) => x,
            Av::Word(bits) => layout::word_num(&mut self.fb, bits, miss),
            Av::Arg(k) => {
                let (p, off) = match entries[ab + k] {
                    Entry::Num(x) => return x,
                    Entry::Ref(p, _) => (p, 0),
                    _ => {
                        let o = self.soff(ab + k);
                        (self.stackp, o)
                    }
                };
                let tag = self.fb.load(MemKind::I32U8, p, off);
                let four = self.i32c(TAG_NUM as i64);
                let is = self.fb.icmp(IntCC::Eq, tag, four);
                let next = self.fb.create_block();
                self.fb.brif(is, next, &[], miss, &[]);
                self.fb.seal_block(next);
                self.fb.switch_to_block(next);
                self.fb.load(MemKind::F64, p, off + VALUE_PAYLOAD)
            }
            // (Excluded by the dry run.)
            Av::Bool(_) | Av::Undef | Av::This => {
                self.fb.jump(miss, &[]);
                let dead = self.fb.create_block();
                self.fb.seal_block(dead);
                self.fb.switch_to_block(dead);
                self.fb.f64const(0.0)
            }
        }
    }
}

/// A property read (or store) resolved to an inlinable getter (setter), see
/// [`Tr::inline_getter`] and [`Tr::inline_setter`].
#[derive(Clone)]
pub(super) struct Getter {
    /// The receiver's and prototypes' shape ids down to the holder.
    pub shapes: Vec<u32>,
    /// The holder's entry slot of the accessor.
    pub slot: u32,
    /// The getter's (setter's) payload word, as a `Value` stores it.
    pub getter: usize,
    pub body: ThisBody,
}

/// The getter `recv.<name>` resolves to, when its body is inlinable (reads only) on `recv` and
/// the lookup is a chain of ordinary plain objects of shared shapes (at most 4 levels); with
/// the getter function (for the plan to keep alive).
pub(super) fn getter_site(i: &Interp, recv: &Value, name: &str) -> Option<(Getter, Value)> {
    accessor_site(i, recv, name, false)
}

/// [`getter_site`] for a store: the setter `recv.<name> = v` calls, when its body (of one
/// parameter) is inlinable on `recv`.
pub(super) fn setter_site(i: &Interp, recv: &Value, name: &str) -> Option<(Getter, Value)> {
    accessor_site(i, recv, name, true)
}

fn accessor_site(i: &Interp, recv: &Value, name: &str, set: bool) -> Option<(Getter, Value)> {
    let Value::Obj(o) = recv else { return None };
    let mut cur = o.clone();
    let mut shapes = Vec::new();
    for _ in 0..4 {
        if !i.ordinary_get_ptr(crate::value::Gc::as_ptr(&cur) as usize) {
            return None;
        }
        let next = {
            let b = cur.try_borrow().ok()?;
            if !matches!(b.exotic, crate::value::Exotic::None)
                || !b.ic_plain.get()
                || !b.props.shape_is_shared()
            {
                return None;
            }
            shapes.push(b.props.shape());
            if let Some(slot) = b.props.slot_of(name) {
                let p = b.props.entry_at(slot)?;
                if !p.accessor() {
                    return None;
                }
                let g = if set { p.setter() } else { p.getter() }?.clone();
                if !matches!(g, Value::Obj(_)) {
                    return None;
                }
                let getter = super::value_word(&g);
                let slot = u32::try_from(slot).ok()?;
                drop(b);
                let ic = crate::bytecode::inline_callee(i, &g)?;
                if ic.arrow || ic.chunk.n_params != set as usize {
                    return None;
                }
                let body = this_body(&ic.chunk, recv, shapes[0], &private_keys(i, &g))?;
                if body.writes && !set {
                    return None;
                }
                return Some((
                    Getter {
                        shapes,
                        slot,
                        getter,
                        body,
                    },
                    g,
                ));
            }
            b.proto.clone()?
        };
        cur = next;
    }
    None
}
