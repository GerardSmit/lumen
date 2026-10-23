//! `String` static calls in compiled loops: the inline `String.fromCharCode(x)` /
//! `String.fromCodePoint(x)` arm, and direct calls of a one-expression wrapper around one of
//! them (`function(a) { return String.fromCodePoint(a) }`, the shape Haxe emits) folded to that
//! arm.
//!
//! A wrapper qualifies when its body is `LoadName GetMethod LoadLocal(0) CallWithThis(1)
//! Return` (a strict wrapper's call is a `TailCall`) with no activation, `arguments`, rest or
//! `this`. The folded call checks the callee's identity like the direct call, the argument
//! being a Number, and — through [`Helper::WrapGuard`], cached until JS may have run — that the
//! wrapper's own `String.<method>` read still yields the intrinsic. The fast arm then runs
//! neither JS nor anything observable (no frame, depth or GC bookkeeping, no `fn_frames`
//! record), and what it cannot do (an argument that makes `fromCodePoint` throw) takes the
//! ordinary direct call. A failed `WrapGuard` leaves the code (the interpreter makes the call)
//! and the next compile leaves the fold out.

use super::*;
use crate::bytecode::Op;
use lumen_codegen::{BinaryOp, ConvOp, FloatCC, IntCC, MemKind, Type, Value as V};

/// The `String` static function `cc` forwards its one parameter to, when its body has the shape
/// of a plain wrapper (see the module docs).
/// The pc of a forwarding wrapper's `LoadName`: past a directive prologue ('use strict'
/// compiles to `Const Pop`).
pub(crate) fn fwd_load_pc(cc: &Chunk) -> usize {
    match &cc.ops[..] {
        [Op::Const(_), Op::Pop, ..] => 2,
        _ => 0,
    }
}

pub(super) fn fwd_body(cc: &Chunk) -> Option<StrStatic> {
    if cc.n_params != 1
        || cc.n_slots != 1
        || cc.derived
        || cc.activation_layout.is_some()
        || cc.makes_env()
        || cc.arguments_slot.is_some()
        || cc.rest_slot.is_some()
        || cc.virt_base.is_some()
        || cc.uses_this()
    {
        return None;
    }
    let ops = &cc.ops[fwd_load_pc(cc)..];
    let (Op::LoadName(_, c), Op::GetMethod(m, _), Op::LoadLocal(0)) =
        (*ops.first()?, *ops.get(1)?, *ops.get(2)?)
    else {
        return None;
    };
    if c as usize >= cc.name_caches.len() {
        return None;
    }
    let call_return = matches!(
        (ops.get(3), ops.get(4)),
        (
            Some(Op::CallWithThis(1) | Op::TailCall(1, true)),
            Some(Op::Return)
        )
    );
    let tail_ok = match ops.get(5) {
        None => true,
        Some(Op::ReturnUndef) => ops.len() == 6,
        Some(_) => false,
    };
    if !call_return || !tail_ok {
        return None;
    }
    helpers::str_static_intrinsic(cc.names.get(m as usize)?)
}

impl Tr<'_, '_> {
    /// The String `f` makes of Number `x`, into `frame.stack[at]` (holding a trivially
    /// droppable value) before branching to `done`: the shared one-character string for a
    /// unit below 256, else [`Helper::StrStatic`]. `miss` when that declines (the call would
    /// throw); nothing is written then.
    pub(super) fn str_static_fast(
        &mut self,
        f: StrStatic,
        x: V,
        at: usize,
        done: Block,
        miss: Block,
    ) {
        let i = self.to_int32(x);
        let (u, exact) = match f {
            StrStatic::CharCode => {
                let m = self.i32c(0xFFFF);
                (self.fb.binary(BinaryOp::Band, i, m), None)
            }
            StrStatic::CodePoint => {
                let back = self.fb.convert(ConvOp::FromSint, Type::F64, i);
                (i, Some(self.fb.fcmp(FloatCC::Eq, back, x)))
            }
        };
        let limit = self.i32c(256);
        let mut small = self.fb.icmp(IntCC::Ult, u, limit);
        if let Some(e) = exact {
            small = self.fb.binary(BinaryOp::Band, small, e);
        }
        let helper = self.fb.create_block();
        let so = layout::rc_strong_offset();
        let units = super::call::unit_strs_offset();
        if let (Some(so), Some(units)) = (so, units) {
            let load = self.fb.create_block();
            self.fb.brif(small, load, &[], helper, &[]);
            self.fb.seal_block(load);
            self.fb.switch_to_block(load);
            let interp = self.fb.load(PTR_MEM, self.frame, FRAME_INTERP);
            let idx = if PTR == Type::I64 {
                self.fb.convert(ConvOp::Uext, PTR, u)
            } else {
                u
            };
            let size = self.ptrc(std::mem::size_of::<Option<crate::lstr::LStr>>() as i64);
            let off = self.fb.binary(BinaryOp::Imul, idx, size);
            let table = self.ptrc(units as i64);
            let table = self.fb.binary(BinaryOp::Iadd, interp, table);
            let slot = self.fb.binary(BinaryOp::Iadd, table, off);
            let s = self.fb.load(PTR_MEM, slot, 0);
            let zp = self.ptrc(0);
            let have = self.fb.icmp(IntCC::Ne, s, zp);
            let hit = self.fb.create_block();
            self.fb.brif(have, hit, &[], helper, &[]);
            self.fb.seal_block(hit);
            self.fb.switch_to_block(hit);
            self.retain_inline(s, so);
            let o = self.soff(at);
            self.set_stack_tag(at, TAG_STR);
            let z = self.i32c(0);
            self.fb
                .store(MemKind::I32U8, self.stackp, z, o + VALUE_BOOL);
            self.fb
                .store(PTR_MEM, self.stackp, s, o + VALUE_PAYLOAD);
            self.fb.jump(done, &[]);
        } else {
            self.fb.jump(helper, &[]);
        }
        self.fb.seal_block(helper);
        self.fb.switch_to_block(helper);
        let (kind, dst) = (self.i32c((f == StrStatic::CodePoint) as i64), self.sptr(at));
        let ok = self.call_status(Helper::StrStatic, &[self.frame, kind, x, dst]);
        let z = self.i32c(0);
        let done_ok = self.fb.icmp(IntCC::Ne, ok, z);
        self.fb.brif(done_ok, done, &[], miss, &[]);
    }

    /// The accumulator append the call at `pc` feeds, when the result goes straight into
    /// `c += <call>` (`StoreLocal(t) ArithLL(Add, c, c, t)` on a fresh temp `t`, the shape the
    /// compiler gives a local `+=`) and no entry below the call borrows from a slot or cell:
    /// `c`'s slot.
    fn append_fusion(&self, pc: usize, base: usize) -> Option<usize> {
        let (Op::StoreLocal(t), Op::ArithLL(ArithKind::Add, d, a, b)) =
            (*self.ops.get(pc + 1)?, *self.ops.get(pc + 2)?)
        else {
            return None;
        };
        let (du, tu) = (d as usize, t as usize);
        // Only the `+=` temp (the compiler's single unnamed slot, used once) may go unwritten.
        let ok = a == d
            && b == t
            && du != tu
            && self.chunk.native_slot_names().get(tu).is_some_and(|n| n.is_empty())
            && pc + 3 <= self.backedge
            && !self.is_leader(pc + 1)
            && !self.is_leader(pc + 2)
            && !self.is_leader(pc + 3)
            && self.kinds[du] == Kind::Boxed
            && self.vars[du].is_none()
            && self.kinds[tu] == Kind::Boxed
            && self.vars[tu].is_none()
            && !self.stack[..base]
                .iter()
                .any(|e| matches!(e, Entry::Ref(..)));
        ok.then_some(du)
    }

    /// For a call of `f` at `pc` (operands `base..drop_to` of the stack, Number argument `x`)
    /// whose result is appended to a local string accumulator (see [`Tr::append_fusion`]):
    /// [`Helper::AppendUnit`] appends the character in place. On a hit the operands are
    /// dropped and the path is parked in `pending_append` to rejoin after the `ArithLL`; on a
    /// miss emission continues with the ordinary call, which builds the string and appends it
    /// as before.
    pub(super) fn unit_append(&mut self, pc: usize, f: StrStatic, x: V, base: usize, drop_to: usize) {
        if self.pending_append.is_some() {
            return;
        }
        let Some(dst) = self.append_fusion(pc, base) else {
            return;
        };
        let (dv, kind) = (
            self.i32c(dst as i64),
            self.i32c((f == StrStatic::CodePoint) as i64),
        );
        let r = self
            .call(Helper::AppendUnit, &[self.frame, dv, kind, x])
            .expect("AppendUnit returns a flag");
        let z = self.i32c(0);
        let hit = self.fb.icmp(IntCC::Ne, r, z);
        let (yes, no) = (self.fb.create_block(), self.fb.create_block());
        self.fb.brif(hit, yes, &[], no, &[]);
        self.fb.seal_block(yes);
        self.fb.seal_block(no);
        self.fb.switch_to_block(yes);
        let entries = self.stack.clone();
        for k in base..drop_to {
            if entries[k] == Entry::Boxed {
                self.drop_at(k);
            }
        }
        let end = self.fb.current_block();
        self.pending_append = Some((pc + 3, end, entries[..base].to_vec()));
        self.fb.switch_to_block(no);
    }

    /// The direct call at `pc` of a wrapper around `f` (see the module docs): `false` (nothing
    /// emitted) when the call's operands do not fit, and the ordinary direct call is to be
    /// emitted instead.
    pub(super) fn fwd_call(
        &mut self,
        pc: usize,
        argc: usize,
        with_this: bool,
        f: StrStatic,
        env: usize,
    ) -> bool {
        let wt = with_this as usize;
        let d = self.stack.len();
        if argc != 1 || d < argc + 1 + wt {
            return false;
        }
        let base = d - argc - 1 - wt;
        let ci = base + wt;
        let ab = ci + 1;
        let entries = self.stack.clone();
        if matches!(entries[ci], Entry::Num(_) | Entry::Bool(_)) || matches!(entries[ab], Entry::Bool(_))
        {
            return false;
        }
        let Some(site) = self.plan.direct.get(&pc).cloned() else {
            return false;
        };
        let Some(&var) = self.math_vars.get(&pc) else {
            return false;
        };
        let full = self.fb.create_block();
        let join = self.fb.create_block();
        match entries[ci] {
            Entry::Ref(_, Src::Pin) => {}
            Entry::Ref(p, _) => self.check_cell(p, 0, site.cell, full),
            _ => {
                let off = self.soff(ci);
                let sp = self.stackp;
                self.check_cell(sp, off, site.cell, full);
            }
        }
        // Another closure of the same code (rebound by the check) may close over other bindings.
        let cellp = self.ptrc(site.cell as i64);
        let cell_env = self.fb.load(PTR_MEM, cellp, CELL_ENV);
        let planned = self.ptrc(env as i64);
        let same_env = self.fb.icmp(IntCC::Eq, cell_env, planned);
        self.guard_to(same_env, full);
        let x = match entries[ab] {
            Entry::Num(x) => x,
            e => {
                let (p, off) = match e {
                    Entry::Ref(p, _) => (p, 0),
                    _ => (self.stackp, self.soff(ab)),
                };
                let tag = self.fb.load(MemKind::I32U8, p, off);
                let num = self.i32c(TAG_NUM as i64);
                let is = self.fb.icmp(IntCC::Eq, tag, num);
                self.guard_to(is, full);
                self.fb.load(MemKind::F64, p, off + VALUE_PAYLOAD)
            }
        };
        let known = self.fb.use_var(var);
        let zero = self.i32c(0);
        let unknown = self.fb.icmp(IntCC::Eq, known, zero);
        let check = self.fb.create_block();
        let cont = self.fb.create_block();
        self.fb.brif(unknown, check, &[], cont, &[]);
        self.fb.seal_block(check);
        self.fb.switch_to_block(check);
        let pcv = self.i32c(pc as i64);
        let probe = self.plan.intr.get(&pc).cloned();
        let g = self.intr_guard(probe, Helper::WrapGuard, &[self.frame, pcv, cellp]);
        let zero = self.i32c(0);
        let bad = self.fb.icmp(IntCC::Eq, g, zero);
        let retire = self.fb.create_block();
        let good = self.fb.create_block();
        self.fb.brif(bad, retire, &[], good, &[]);
        self.fb.seal_block(retire);
        self.fb.switch_to_block(retire);
        self.fb.mark_cold();
        self.call(Helper::SiteFailed, &[self.frame, pcv]);
        self.emit_exit(pc, EXIT_RESUME, &entries, d, None);
        self.fb.seal_block(good);
        self.fb.switch_to_block(good);
        let one = self.i32c(1);
        self.fb.def_var(var, one);
        self.fb.jump(cont, &[]);
        self.fb.seal_block(cont);
        self.fb.switch_to_block(cont);

        // The result is built above the operands (the receiver / callee entries may own values
        // that live until it is known to be done).
        let done = self.fb.create_block();
        self.unit_append(pc, f, x, base, ab);
        self.str_static_fast(f, x, d, done, full);
        self.fb.seal_block(done);
        self.fb.switch_to_block(done);
        for k in base..ab {
            if entries[k] == Entry::Boxed {
                self.drop_at(k);
            }
        }
        let (so, bo) = (self.soff(d), self.soff(base));
        let sp = self.stackp;
        self.copy_value(sp, so, sp, bo);
        self.set_stack_tag(d, TAG_UNDEFINED);
        self.fb.jump(join, &[]);

        // ---- anything else: the ordinary direct call ----
        self.fb.seal_block(full);
        self.fb.switch_to_block(full);
        self.stack = entries;
        self.direct_call_full(pc, argc, with_this);
        self.fb.jump(join, &[]);
        self.fb.seal_block(join);
        self.fb.switch_to_block(join);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk_of(src: &str) -> std::rc::Rc<Chunk> {
        let program = crate::parser::parse_script(src, false).unwrap();
        let crate::ast::Stmt::FuncDecl(function) = &program[0] else {
            panic!("function")
        };
        crate::bytecode::compile(function).unwrap()
    }

    #[test]
    fn wrappers_around_string_statics_are_recognized() {
        let cases = [
            (
                "function f(a){return String.fromCodePoint(a)}",
                Some(StrStatic::CodePoint),
            ),
            (
                "function f(a){return String.fromCharCode(a)}",
                Some(StrStatic::CharCode),
            ),
            (
                "function f(a){'use strict';return String.fromCodePoint(a)}",
                Some(StrStatic::CodePoint),
            ),
            (
                "function f(a){return S.fromCharCode(a)}",
                Some(StrStatic::CharCode),
            ),
            ("function f(a,b){return String.fromCodePoint(a)}", None),
            ("function f(a){return String.fromCodePoint(a)+1}", None),
            ("function f(a){return String.fromCodePoint(a+1)}", None),
            ("function f(a){return String.fromCodePoint(a,a)}", None),
            ("function f(a){return Math.floor(a)}", None),
            ("function f(a){return this.fromCodePoint(a)}", None),
            ("function f(a){return String.raw(a)}", None),
            ("function f(a){var g=function(){return a};return String.fromCodePoint(a)}", None),
        ];
        for (src, want) in cases {
            assert_eq!(fwd_body(&chunk_of(src)), want, "{src}");
        }
    }
}
