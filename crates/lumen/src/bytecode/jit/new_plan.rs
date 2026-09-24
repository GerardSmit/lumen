//! `new C(args)` through `C`'s constructor template (see `bytecode::ctor_plan`): no frame, no
//! body. The site guards the callee's identity; one helper ([`Helper::NewPlan`]) makes the
//! allocation — the plan's guards, `C.prototype` and its chain proof, the template instance with
//! its constant slots — and returns the instance's entry base; the argument slots are then plain
//! stores of the argument words (moved: `new` consumes its operands). The allocation stays its
//! own call followed by plain stores, so the instance could later be scalar-replaced.

use super::*;
use crate::bytecode::ctor_plan::{CtorPlan, PlanSrc};
use crate::value::{Property, PACK_BIGINT, PACK_BOOL, PACK_EMPTY, PACK_NULL, PACK_OBJ, PACK_STR,
    PACK_SYM, PACK_UNDEFINED};
use lumen_codegen::{BinaryOp, IntCC, MemKind, Type, Value as V};
use std::rc::Rc;

/// What a planned `new` site's code embeds (pinned in the plan's boxes).
pub(crate) struct PlanCell {
    /// The constructor (also `new.target`).
    pub pin: Value,
    pub plan: Rc<CtorPlan>,
}

/// A planned `new` site.
#[derive(Clone)]
pub(super) struct NewSite {
    /// The constructor's payload word, as a `Value` stores it.
    pub word: usize,
    /// Address of the pinned constructor `Value` (a `dname` load pushes it).
    pub pin: usize,
    /// Address of the [`PlanCell`].
    pub cell: usize,
    /// `(entry slot, argument index)`: the argument slots to store (each argument at most once).
    pub stores: Vec<(u32, u16)>,
}

/// The planned `new` site for constructor `c` with `argc` arguments, when `c` has a template.
pub(super) fn plan_site(
    interp: &Interp,
    callee: &Value,
    argc: usize,
    boxes: &mut Vec<Box<dyn std::any::Any>>,
) -> Option<NewSite> {
    let Value::Obj(c) = callee else { return None };
    let plan = crate::bytecode::ctor_plan::plan_ready(interp, c)?;
    let mut stores = Vec::new();
    for (k, s) in plan.slots.iter().enumerate() {
        if let PlanSrc::Arg(a) = s.src {
            if (a as usize) < argc {
                if stores.iter().any(|&(_, b)| b == a) {
                    // (A second store of one argument would need a clone.)
                    return None;
                }
                stores.push((u32::try_from(k).ok()?, a));
            }
        }
    }
    let pin: Box<Value> = Box::new(callee.clone());
    let cell: Box<PlanCell> = Box::new(PlanCell {
        pin: callee.clone(),
        plan,
    });
    let site = NewSite {
        word: super::value_word(callee),
        pin: &*pin as *const Value as usize,
        cell: &*cell as *const PlanCell as usize,
        stores,
    };
    boxes.push(pin);
    boxes.push(cell);
    Some(site)
}

/// [`Helper::NewPlan`]: `(frame, cell, out: *mut Value, consume: u32) -> entries base` — the
/// template instance of `cell`'s plan (constant slots filled, argument slots `undefined`) into
/// `out` (dropping the callee there first when `consume`) and the address of its first entry;
/// 0 when not applicable now (a guard, the prototype chain, or a due collector poll: nothing
/// was done). Pure.
pub(crate) unsafe extern "C" fn new_plan_helper(
    f: *mut JitFrame,
    cell: *const PlanCell,
    out: *mut Value,
    consume: u32,
) -> usize {
    let i = &mut *(*f).interp;
    let cell = &*cell;
    let Value::Obj(c) = &cell.pin else { return 0 };
    let tick = i.gc_tick.wrapping_add(1);
    if tick & crate::interpreter::GC_CALL_POLL_MASK == 0 || i.terminating || i.multi_realm() {
        return 0;
    }
    if !cell.plan.guards_ok() {
        return 0;
    }
    let proto = match crate::bytecode::class_fields::own_prototype(c) {
        Some(Value::Obj(p)) => p,
        // (Single realm: GetFunctionRealm is the current one.)
        Some(_) => i.object_proto.clone(),
        None => return 0,
    };
    if !cell.plan.proto_ok(&proto) {
        return 0;
    }
    i.gc_tick = tick;
    let obj = cell.plan.instantiate(proto, &[]);
    let base = {
        let mut b = obj.borrow_mut();
        // (Never 0: an instance without entries has no stores.)
        b.props
            .entry_at_mut(0)
            .map_or(std::mem::align_of::<Property>(), |p| p as *mut Property as usize)
    };
    if consume != 0 {
        std::ptr::drop_in_place(out);
    }
    std::ptr::write(out, Value::Obj(obj));
    base
}

/// `PackedValue` tag bits by `Value` tag (see `layout`).
static PACK_BY_TAG: [u64; 9] = [
    PACK_UNDEFINED,
    PACK_EMPTY,
    PACK_NULL,
    PACK_BOOL,
    0,
    PACK_BIGINT,
    PACK_STR,
    PACK_SYM,
    PACK_OBJ,
];
const _: () = assert!(
    TAG_UNDEFINED == 0
        && TAG_EMPTY == 1
        && TAG_NULL == 2
        && TAG_BOOL == 3
        && TAG_NUM == 4
        && TAG_BIGINT == 5
        && TAG_STR == 6
        && TAG_SYM == 7
        && TAG_OBJ == 8
);

impl Tr<'_, '_> {
    /// The NaN-boxed word of the `Value` at `p` (ownership moves to the word).
    fn value_packed(&mut self, p: V) -> V {
        let tag = self.fb.load(MemKind::I32U8, p, 0);
        // (A handle is pointer-sized: only its bytes are the payload.)
        let pl = self.fb.load(PTR_MEM, p, VALUE_PAYLOAD);
        let pl = if PTR == Type::I64 {
            pl
        } else {
            self.fb.convert(lumen_codegen::ConvOp::Uext, Type::I64, pl)
        };
        let x = self.fb.load(MemKind::F64, p, VALUE_PAYLOAD);
        let b = self.fb.load(MemKind::I32U8, p, VALUE_BOOL);
        let numw = layout::num_word(&mut self.fb, x);
        let t = self.fb.convert(lumen_codegen::ConvOp::Uext, PTR, tag);
        let eight = self.ptrc(8);
        let at = self.fb.binary(BinaryOp::Imul, t, eight);
        let tab = self.ptrc(PACK_BY_TAG.as_ptr() as usize as i64);
        let tp = self.fb.binary(BinaryOp::Iadd, tab, at);
        let tbits = self.fb.load(MemKind::I64, tp, 0);
        let bw = self.fb.convert(lumen_codegen::ConvOp::Uext, Type::I64, b);
        let z = self.fb.iconst(Type::I64, 0);
        let c_bool = self.i32c(TAG_BOOL as i64);
        let is_bool = self.fb.icmp(IntCC::Eq, tag, c_bool);
        let c_num = self.i32c(TAG_NUM as i64);
        let is_ref = self.fb.icmp(IntCC::Ugt, tag, c_num);
        let is_num = self.fb.icmp(IntCC::Eq, tag, c_num);
        let low = self.fb.select(is_bool, bw, z);
        let low = self.fb.select(is_ref, pl, low);
        let w = self.fb.binary(BinaryOp::Bor, tbits, low);
        self.fb.select(is_num, numw, w)
    }

    /// A planned `new` site (see the module docs); the ordinary construct when a guard fails.
    pub(super) fn plan_new(&mut self, pc: usize, argc: usize) {
        let site = self.plan.dnew[&pc].clone();
        let d = self.stack.len();
        let base = d - argc - 1;
        let ab = base + 1;
        // The arguments move into the instance: owned values (or unboxed).
        for k in ab..d {
            self.force(k);
        }
        if matches!(self.stack[base], Entry::Ref(_, s) if s.in_env()) {
            self.force(base);
        }
        let entries = self.stack.clone();
        let below: Vec<Entry> = entries[..base].to_vec();
        let slow = self.fb.create_block();
        let join = self.fb.create_block();
        let st_join = self.fb.append_block_param(join, Type::I32);
        let consume = match entries[base] {
            Entry::Ref(_, Src::Pin) => false,
            Entry::Ref(p, _) => {
                self.check_callee(p, 0, site.word, slow);
                false
            }
            Entry::Boxed => {
                let off = self.soff(base);
                let sp = self.stackp;
                self.check_callee(sp, off, site.word, slow);
                true
            }
            Entry::Num(_) | Entry::Bool(_) => {
                self.fb.jump(slow, &[]);
                let dead = self.fb.create_block();
                self.fb.seal_block(dead);
                self.fb.switch_to_block(dead);
                false
            }
        };
        let cellp = self.ptrc(site.cell as i64);
        let out = self.sptr(base);
        let cv = self.i32c(consume as i64);
        let eb = self
            .call(Helper::NewPlan, &[self.frame, cellp, out, cv])
            .expect("NewPlan returns a base");
        let z = self.ptrc(0);
        let ok = self.fb.icmp(IntCC::Ne, eb, z);
        self.guard_to(ok, slow);
        // ---- the argument slots ----
        let mut used = vec![false; argc];
        for &(slot, a) in &site.stores {
            let k = ab + a as usize;
            used[a as usize] = true;
            let w = match entries[k] {
                Entry::Num(x) => layout::num_word(&mut self.fb, x),
                Entry::Bool(b) => layout::bool_word(&mut self.fb, b),
                Entry::Boxed | Entry::Ref(..) => {
                    let p = self.sptr(k);
                    let w = self.value_packed(p);
                    self.set_stack_tag(k, TAG_UNDEFINED);
                    w
                }
            };
            layout::entry_store(&mut self.fb, eb, slot, w);
        }
        for (a, u) in used.iter().enumerate() {
            if !u && entries[ab + a] == Entry::Boxed {
                self.drop_at(ab + a);
            }
        }
        let z32 = self.i32c(0);
        self.fb.jump(join, &[z32]);

        // ---- the ordinary construct ----
        self.fb.seal_block(slow);
        self.fb.switch_to_block(slow);
        self.stack = entries;
        self.box_from(base);
        let st = self.call_status(Helper::GcPoll, &[self.frame]);
        let z = self.i32c(0);
        let ok = self.fb.icmp(IntCC::Eq, st, z);
        let go = self.fb.create_block();
        let bad = self.fb.create_block();
        self.fb.brif(ok, go, &[], bad, &[]);
        self.fb.seal_block(bad);
        self.fb.switch_to_block(bad);
        // The operands are the op's to consume.
        let (sp, nv) = (self.sptr(base), self.i32c((d - base) as i64));
        self.call(Helper::DropN, &[sp, nv]);
        self.fb.jump(join, &[st]);
        self.fb.seal_block(go);
        self.fb.switch_to_block(go);
        let (r, bv, dv) = (
            self.i32c(pc as i64),
            self.i32c(base as i64),
            self.i32c(d as i64),
        );
        let st = self.call_status(Helper::Generic, &[self.frame, r, bv, dv]);
        self.fb.jump(join, &[st]);

        self.fb.seal_block(join);
        self.fb.switch_to_block(join);
        self.invalidate_js();
        self.stack = below.clone();
        self.throw_if(st_join, pc, &below, base);
        self.soff(base);
        self.stack.push(Entry::Boxed);
    }
}
