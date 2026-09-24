//! Inlining of small pure callees at call sites whose callee the planner resolved (see
//! [`super::build`]'s inline call sites: `f(args)` / `ns.f(args)` resolved from a name and
//! guarded by identity like the direct `#[op(fast)]` calls).
//!
//! A callee qualifies when its whole body is straight-line or forward-branching arithmetic on
//! its parameters and locals — Numbers and Booleans only, typed statically from the call
//! site's unboxed arguments. Such a body can neither throw, nor run JS, nor exit: once the
//! site's identity guard holds, the inlined copy computes exactly what the call would, and no
//! interpreter frame ever needs to exist for it (no deopt state to materialize). Anything else
//! (calls, property access, `this`, `arguments`, loops, other types) keeps the ordinary call.

use super::*;
use crate::bytecode::{ArithKind, CmpKind, Op, UpdKind};
use lumen_codegen::ir::{BinaryOp, FloatCC, IntCC, Type, UnaryOp, Value as V};
use lumen_codegen::builder::Variable;

/// Ops beyond which a callee is not inlined.
const MAX_INLINE_OPS: usize = 48;

/// The static shape of an inlinable callee body for one set of argument kinds.
#[derive(Clone, Debug)]
pub(super) struct Shape {
    /// Per slot: its kind (`true` = Number) when the body uses it.
    pub locals: Vec<Option<bool>>,
    /// Per pc: the operand-stack kinds when it starts a block (a jump target), else `None`.
    pub leaders: Vec<Option<Vec<bool>>>,
    pub reach: Vec<bool>,
    /// The kind of the returned value; `None` when the body returns `undefined`.
    pub ret: Option<bool>,
}

/// A resolved callee that may be inlined.
#[derive(Clone)]
pub(super) struct InlSite {
    pub chunk: std::rc::Rc<Chunk>,
    pub shape: Shape,
    pub argc: usize,
    /// The callee's object address (guarded by [`Helper::FastGuard`]).
    pub expect: usize,
}

pub(super) fn arith_of(op: &Op) -> Option<ArithKind> {
    Some(match op {
        Op::Add => ArithKind::Add,
        Op::Sub => ArithKind::Sub,
        Op::Mul => ArithKind::Mul,
        Op::Div => ArithKind::Div,
        Op::Mod => ArithKind::Mod,
        Op::BitAnd => ArithKind::BitAnd,
        Op::BitOr => ArithKind::BitOr,
        Op::BitXor => ArithKind::BitXor,
        Op::Shl => ArithKind::Shl,
        Op::Shr => ArithKind::Shr,
        Op::UShr => ArithKind::UShr,
        _ => return None,
    })
}

pub(super) fn cmp_of(op: &Op) -> Option<CmpKind> {
    Some(match op {
        Op::Lt => CmpKind::Lt,
        Op::Gt => CmpKind::Gt,
        Op::Le => CmpKind::Le,
        Op::Ge => CmpKind::Ge,
        Op::EqEq => CmpKind::EqEq,
        Op::NotEq => CmpKind::NotEq,
        Op::StrictEq => CmpKind::StrictEq,
        Op::StrictNotEq => CmpKind::StrictNotEq,
        _ => return None,
    })
}

fn is_eq(k: CmpKind) -> bool {
    matches!(
        k,
        CmpKind::EqEq | CmpKind::NotEq | CmpKind::StrictEq | CmpKind::StrictNotEq
    )
}

/// The kind of constant `k` (`true` = Number).
fn const_kind(chunk: &Chunk, k: u32) -> Option<bool> {
    match chunk.consts.get(k as usize)? {
        Value::Num(_) => Some(true),
        Value::Bool(_) => Some(false),
        _ => None,
    }
}

/// The callee `callee` as an inlining candidate for a call with `args` (argument kinds,
/// `true` = Number): its chunk and body shape, when the body qualifies (see the module docs).
pub(super) fn candidate(
    i: &Interp,
    callee: &Value,
    args: &[bool],
) -> Option<(std::rc::Rc<Chunk>, Shape)> {
    let c = crate::bytecode::inline_callee(i, callee)?;
    let chunk = c.chunk;
    if chunk.ops.len() > MAX_INLINE_OPS
        || chunk.activation_layout.is_some()
        || chunk.arguments_slot.is_some()
        || chunk.rest_slot.is_some()
        || chunk.uses_this()
        || args.len() < chunk.n_params
    {
        return None;
    }
    let shape = shape(&chunk, &args[..chunk.n_params])?;
    Some((chunk, shape))
}

/// Abstract interpretation of the body over kinds (see [`Shape`]).
fn shape(chunk: &Chunk, args: &[bool]) -> Option<Shape> {
    let ops = &chunk.ops;
    let n = ops.len();
    let ns = chunk.n_slots;
    // The kind each slot is fixed to (its SSA variable's type).
    let mut kind: Vec<Option<bool>> = vec![None; ns];
    for (s, &k) in args.iter().enumerate() {
        kind[s] = Some(k);
    }
    // Incoming state per pc: (slots definitely written, stack kinds).
    let mut inc: Vec<Option<(Vec<bool>, Vec<bool>)>> = vec![None; n + 1];
    let mut leaders: Vec<Option<Vec<bool>>> = vec![None; n];
    let mut reach = vec![false; n];
    let mut ret: Option<Option<bool>> = None;
    let mut defined = vec![false; ns];
    defined[..args.len()].fill(true);
    inc[0] = Some((defined, Vec::new()));
    let merge = |inc: &mut Vec<Option<(Vec<bool>, Vec<bool>)>>,
                 q: usize,
                 d: &[bool],
                 st: &[bool]|
     -> Option<()> {
        match &mut inc[q] {
            None => inc[q] = Some((d.to_vec(), st.to_vec())),
            Some((d0, st0)) => {
                if st0.as_slice() != st {
                    return None;
                }
                for (a, &b) in d0.iter_mut().zip(d) {
                    *a &= b;
                }
            }
        }
        Some(())
    };
    for pc in 0..n {
        let Some((mut d, mut st)) = inc[pc].take() else {
            continue;
        };
        reach[pc] = true;
        let op = ops[pc];
        let slot = |s: u16| -> Option<usize> { ((s as usize) < ns).then_some(s as usize) };
        let mut falls = true;
        let mut jump: Option<usize> = None;
        let num2 = |st: &mut Vec<bool>| -> Option<()> {
            let b = st.pop()?;
            let a = st.pop()?;
            (a && b).then_some(())
        };
        if arith_of(&op).is_some() {
            num2(&mut st)?;
            st.push(true);
        } else if let Some(k) = cmp_of(&op) {
            let b = st.pop()?;
            let a = st.pop()?;
            if !(a && b || is_eq(k) && !a && !b) {
                return None;
            }
            st.push(false);
        } else {
            match op {
                Op::Const(k) => st.push(const_kind(chunk, k)?),
                Op::Dup => st.push(*st.last()?),
                Op::Pop => {
                    st.pop()?;
                }
                Op::Nip => {
                    let t = st.pop()?;
                    st.pop()?;
                    st.push(t);
                }
                Op::LoadLocal(s) => {
                    let s = slot(s)?;
                    if !d[s] {
                        return None;
                    }
                    st.push(kind[s]?);
                }
                Op::StoreLocal(s) => {
                    let s = slot(s)?;
                    let k = st.pop()?;
                    match kind[s] {
                        Some(k0) if k0 != k => return None,
                        _ => kind[s] = Some(k),
                    }
                    d[s] = true;
                }
                Op::UpdateLocal(s, u) => {
                    let s = slot(s)?;
                    if !d[s] || kind[s] != Some(true) {
                        return None;
                    }
                    if !matches!(u, UpdKind::IncDiscard | UpdKind::DecDiscard) {
                        st.push(true);
                    }
                }
                Op::ArithLL(_, dst, a, b) => {
                    let (dst, a, b) = (slot(dst)?, slot(a)?, slot(b)?);
                    if !d[a] || !d[b] || kind[a] != Some(true) || kind[b] != Some(true) {
                        return None;
                    }
                    if kind[dst] == Some(false) {
                        return None;
                    }
                    kind[dst] = Some(true);
                    d[dst] = true;
                }
                Op::ArithLK(_, dst, a, k) => {
                    let (dst, a) = (slot(dst)?, slot(a)?);
                    if !d[a] || kind[a] != Some(true) || const_kind(chunk, k) != Some(true) {
                        return None;
                    }
                    if kind[dst] == Some(false) {
                        return None;
                    }
                    kind[dst] = Some(true);
                    d[dst] = true;
                }
                Op::Neg | Op::Plus | Op::BitNot => {
                    if !st.pop()? {
                        return None;
                    }
                    st.push(true);
                }
                Op::Not => {
                    st.pop()?;
                    st.push(false);
                }
                Op::Jump(t) => {
                    falls = false;
                    jump = Some(t as usize);
                }
                Op::JumpIfFalse(t) => {
                    st.pop()?;
                    jump = Some(t as usize);
                }
                Op::JumpIfFalsePeek(t) | Op::JumpIfTruePeek(t) => {
                    st.last()?;
                    jump = Some(t as usize);
                }
                Op::JumpIfNotCmp(k, t) => {
                    let b = st.pop()?;
                    let a = st.pop()?;
                    if !(a && b || is_eq(k) && !a && !b) {
                        return None;
                    }
                    jump = Some(t as usize);
                }
                Op::JumpIfNotCmpLL(_, a, b, t) => {
                    let (a, b) = (slot(a)?, slot(b)?);
                    if !d[a] || !d[b] || kind[a] != Some(true) || kind[b] != Some(true) {
                        return None;
                    }
                    jump = Some(t as usize);
                }
                Op::JumpIfNotCmpLK(_, a, k, t) => {
                    let a = slot(a)?;
                    if !d[a] || kind[a] != Some(true) || const_kind(chunk, k) != Some(true) {
                        return None;
                    }
                    jump = Some(t as usize);
                }
                Op::Return => {
                    let k = st.pop()?;
                    if !st.is_empty() || ret.is_some_and(|r| r != Some(k)) {
                        return None;
                    }
                    ret = Some(Some(k));
                    falls = false;
                }
                Op::ReturnUndef => {
                    if !st.is_empty() || ret.is_some_and(|r| r.is_some()) {
                        return None;
                    }
                    ret = Some(None);
                    falls = false;
                }
                _ => return None,
            }
        }
        if let Some(t) = jump {
            // Forward branches only (no loops).
            if t <= pc || t >= n {
                return None;
            }
            merge(&mut inc, t, &d, &st)?;
            leaders[t] = Some(st.clone());
        }
        if falls {
            if pc + 1 >= n {
                return None;
            }
            merge(&mut inc, pc + 1, &d, &st)?;
        }
    }
    // A leader's stack kinds are the merged ones (checked equal on every edge).
    Some(Shape {
        locals: kind,
        leaders,
        reach,
        ret: ret?,
    })
}

impl Tr<'_, '_> {
    /// The `CallWithThis` of an inline call site: the callee body on the unboxed arguments
    /// replaces the placeholders and arguments with its result.
    pub(super) fn inline_call(&mut self, pc: usize, k: usize) -> Result<(), String> {
        let site = self.plan.inl[k].clone();
        let d = self.stack.len();
        let argc = site.argc;
        let mut args = Vec::with_capacity(argc);
        for e in &self.stack[d - argc..] {
            match *e {
                Entry::Num(x) => args.push((x, true)),
                Entry::Bool(b) => args.push((b, false)),
                other => {
                    return Err(format!("inline call argument {other:?} at {pc} is not unboxed"))
                }
            }
        }
        let (r, kind) = self.inline_body(&site, &args)?;
        // The placeholders: receiver and callee, or just the callee of a local-callee site.
        let local = self
            .plan
            .math
            .values()
            .any(|s| s.call == pc && s.slot.is_some());
        self.stack.truncate(d - argc - if local { 1 } else { 2 });
        match (r, kind) {
            (Some(r), Some(true)) => self.stack.push(Entry::Num(r)),
            (Some(r), Some(false)) => self.stack.push(Entry::Bool(r)),
            _ => {
                let at = self.stack.len();
                self.set_stack_tag(at, TAG_UNDEFINED);
                self.stack.push(Entry::Boxed);
            }
        }
        Ok(())
    }

    /// The inlined body's result and its kind (`None`: the body returns `undefined`).
    fn inline_body(
        &mut self,
        site: &InlSite,
        args: &[(V, bool)],
    ) -> Result<(Option<V>, Option<bool>), String> {
        let chunk = &*site.chunk;
        let sh = &site.shape;
        let ty = |num: bool| if num { Type::F64 } else { Type::I32 };
        let mut vars: Vec<Option<Variable>> = vec![None; chunk.n_slots];
        for (s, k) in sh.locals.iter().enumerate() {
            if let Some(num) = *k {
                vars[s] = Some(self.fb.declare_var(ty(num)));
            }
        }
        for (s, &(v, _)) in args.iter().take(chunk.n_params).enumerate() {
            if let Some(var) = vars[s] {
                self.fb.def_var(var, v);
            }
        }
        let n = chunk.ops.len();
        let mut blocks: Vec<Option<lumen_codegen::ir::Block>> = vec![None; n];
        for pc in 0..n {
            if let (true, Some(kinds)) = (sh.reach[pc], &sh.leaders[pc]) {
                let b = self.fb.create_block();
                for &k in kinds {
                    self.fb.append_block_param(b, ty(k));
                }
                blocks[pc] = Some(b);
            }
        }
        let done = self.fb.create_block();
        let result = sh.ret.map(|k| self.fb.append_block_param(done, ty(k)));
        let mut st: Vec<(V, bool)> = Vec::new();
        let mut open = true;
        for pc in 0..n {
            if !sh.reach[pc] {
                continue;
            }
            if let Some(b) = blocks[pc] {
                if open {
                    let vals: Vec<V> = st.iter().map(|x| x.0).collect();
                    self.fb.jump(b, &vals);
                }
                self.fb.seal_block(b);
                self.fb.switch_to_block(b);
                let kinds = sh.leaders[pc].clone().unwrap_or_default();
                let params = self.fb.block_params(b).to_vec();
                st = params.into_iter().zip(kinds).collect();
                open = true;
            }
            if !open {
                continue;
            }
            let op = chunk.ops[pc];
            let var = |s: u16| vars[s as usize].expect("shaped slot");
            if let Some(k) = arith_of(&op) {
                let (y, _) = st.pop().expect("shaped");
                let (x, _) = st.pop().expect("shaped");
                let r = self.num_arith(k, x, y)?;
                st.push((r, true));
                continue;
            }
            if let Some(k) = cmp_of(&op) {
                let (y, yn) = st.pop().expect("shaped");
                let (x, _) = st.pop().expect("shaped");
                let r = self.inl_cmp(k, x, y, yn);
                st.push((r, false));
                continue;
            }
            let mut branch: Option<(V, usize)> = None;
            match op {
                Op::Const(k) => match chunk.consts[k as usize] {
                    Value::Num(x) => st.push((self.fb.f64const(x), true)),
                    Value::Bool(b) => st.push((self.i32c(b as i64), false)),
                    _ => return Err("inline constant".into()),
                },
                Op::Dup => st.push(*st.last().expect("shaped")),
                Op::Pop => {
                    st.pop();
                }
                Op::Nip => {
                    let t = st.pop().expect("shaped");
                    st.pop();
                    st.push(t);
                }
                Op::LoadLocal(s) => {
                    let v = self.fb.use_var(var(s));
                    st.push((v, sh.locals[s as usize] == Some(true)));
                }
                Op::StoreLocal(s) => {
                    let (v, _) = st.pop().expect("shaped");
                    self.fb.def_var(var(s), v);
                }
                Op::UpdateLocal(s, u) => {
                    let old = self.fb.use_var(var(s));
                    let one = self.fb.f64const(1.0);
                    let inc = matches!(u, UpdKind::PreInc | UpdKind::PostInc | UpdKind::IncDiscard);
                    let new = self.fb.binary(
                        if inc { BinaryOp::Fadd } else { BinaryOp::Fsub },
                        old,
                        one,
                    );
                    self.fb.def_var(var(s), new);
                    match u {
                        UpdKind::PreInc | UpdKind::PreDec => st.push((new, true)),
                        UpdKind::PostInc | UpdKind::PostDec => st.push((old, true)),
                        _ => {}
                    }
                }
                Op::ArithLL(k, dst, a, b) => {
                    let x = self.fb.use_var(var(a));
                    let y = self.fb.use_var(var(b));
                    let r = self.num_arith(k, x, y)?;
                    self.fb.def_var(var(dst), r);
                }
                Op::ArithLK(k, dst, a, c) => {
                    let x = self.fb.use_var(var(a));
                    let Value::Num(c) = chunk.consts[c as usize] else {
                        return Err("inline constant".into());
                    };
                    let y = self.fb.f64const(c);
                    let r = self.num_arith(k, x, y)?;
                    self.fb.def_var(var(dst), r);
                }
                Op::Neg => {
                    let (x, _) = st.pop().expect("shaped");
                    st.push((self.fb.unary(UnaryOp::Fneg, x), true));
                }
                Op::Plus => {}
                Op::BitNot => {
                    let (x, _) = st.pop().expect("shaped");
                    let a = self.to_int32(x);
                    let m = self.i32c(-1);
                    let r = self.fb.binary(BinaryOp::Bxor, a, m);
                    st.push((self.fb.convert(ConvOp::FromSint, Type::F64, r), true));
                }
                Op::Not => {
                    let e = st.pop().expect("shaped");
                    let t = self.inl_truthy(e);
                    let one = self.i32c(1);
                    st.push((self.fb.binary(BinaryOp::Bxor, t, one), false));
                }
                Op::Jump(t) => {
                    let vals: Vec<V> = st.iter().map(|x| x.0).collect();
                    self.fb.jump(blocks[t as usize].expect("shaped leader"), &vals);
                    open = false;
                }
                Op::JumpIfFalse(t) => {
                    let e = st.pop().expect("shaped");
                    let c = self.inl_truthy(e);
                    let one = self.i32c(1);
                    branch = Some((self.fb.binary(BinaryOp::Bxor, c, one), t as usize));
                }
                Op::JumpIfFalsePeek(t) => {
                    let c = self.inl_truthy(*st.last().expect("shaped"));
                    let one = self.i32c(1);
                    branch = Some((self.fb.binary(BinaryOp::Bxor, c, one), t as usize));
                }
                Op::JumpIfTruePeek(t) => {
                    let c = self.inl_truthy(*st.last().expect("shaped"));
                    branch = Some((c, t as usize));
                }
                Op::JumpIfNotCmp(k, t) => {
                    let (y, yn) = st.pop().expect("shaped");
                    let (x, _) = st.pop().expect("shaped");
                    let c = self.inl_cmp(k, x, y, yn);
                    let one = self.i32c(1);
                    branch = Some((self.fb.binary(BinaryOp::Bxor, c, one), t as usize));
                }
                Op::JumpIfNotCmpLL(k, a, b, t) => {
                    let x = self.fb.use_var(var(a));
                    let y = self.fb.use_var(var(b));
                    let c = self.fb.fcmp(fcc(k), x, y);
                    let one = self.i32c(1);
                    branch = Some((self.fb.binary(BinaryOp::Bxor, c, one), t as usize));
                }
                Op::JumpIfNotCmpLK(k, a, c, t) => {
                    let x = self.fb.use_var(var(a));
                    let Value::Num(c) = chunk.consts[c as usize] else {
                        return Err("inline constant".into());
                    };
                    let y = self.fb.f64const(c);
                    let c = self.fb.fcmp(fcc(k), x, y);
                    let one = self.i32c(1);
                    branch = Some((self.fb.binary(BinaryOp::Bxor, c, one), t as usize));
                }
                Op::Return => {
                    let (v, _) = st.pop().expect("shaped");
                    self.fb.jump(done, &[v]);
                    open = false;
                }
                Op::ReturnUndef => {
                    self.fb.jump(done, &[]);
                    open = false;
                }
                other => return Err(format!("inline op {other:?}")),
            }
            if let Some((cond, t)) = branch {
                let vals: Vec<V> = st.iter().map(|x| x.0).collect();
                let next = self.fb.create_block();
                self.fb.brif(cond, blocks[t].expect("shaped leader"), &vals, next, &[]);
                self.fb.seal_block(next);
                self.fb.switch_to_block(next);
            }
        }
        self.fb.seal_block(done);
        self.fb.switch_to_block(done);
        Ok((result, sh.ret))
    }

    /// ToBoolean of an unboxed Number / Boolean.
    fn inl_truthy(&mut self, (x, num): (V, bool)) -> V {
        if !num {
            return x;
        }
        let z = self.fb.f64const(0.0);
        let nz = self.fb.fcmp(FloatCC::Ne, x, z);
        let ord = self.fb.fcmp(FloatCC::Eq, x, x);
        self.fb.binary(BinaryOp::Band, nz, ord)
    }

    /// `x <k> y` on two Numbers, or (equality kinds) two Booleans.
    pub(super) fn inl_cmp(&mut self, k: CmpKind, x: V, y: V, num: bool) -> V {
        if num {
            return self.fb.fcmp(fcc(k), x, y);
        }
        let cc = if matches!(k, CmpKind::EqEq | CmpKind::StrictEq) {
            IntCC::Eq
        } else {
            IntCC::Ne
        };
        self.fb.icmp(cc, x, y)
    }
}
