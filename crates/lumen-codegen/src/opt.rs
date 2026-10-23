//! Mid-level optimizations over SSA.
//!
//! [`optimize`] runs the pipeline: unreachable-block removal, trivial block-parameter removal,
//! dominator-scoped GVN with constant folding, algebraic identities and branch folding,
//! loop-invariant code motion ([`crate::licm`]), load forwarding, then dead-code elimination
//! (including dead block parameters and their branch arguments).

use crate::cfg::Cfg;
use crate::eval;
use crate::ir::*;
use lumen_common::fasthash::FastMap;

pub fn optimize(func: &mut Function) {
    optimize_impl(func, true);
}

/// AOT size mode keeps the scalar loop body instead of cloning it.
pub fn optimize_for_size(func: &mut Function) {
    optimize_impl(func, false);
}

fn optimize_impl(func: &mut Function, unroll: bool) {
    // The CFG is reused while no pass changes a branch; removing unreachable blocks keeps it valid.
    let mut cfg = None;
    remove_unreachable(func);
    simplify_params(func);
    narrow_params(func);
    gvn_cached(func, &mut cfg);
    // Another GVN pass only has something to find after a change since the last one.
    let mut dirty = remove_unreachable(func);
    dirty |= simplify_params(func);
    dirty |= narrow_params(func);
    // Small constant-trip loops: unroll fully, then fold their induction values.
    if unroll && crate::unroll::unroll(func) {
        cfg = None;
        simplify_params(func);
        gvn_cached(func, &mut cfg);
        remove_unreachable(func);
        simplify_params(func);
        dirty = true;
    }
    // After GVN has merged duplicates, so each invariant value is hoisted once; before load
    // forwarding, which then sees the hoisted loads in the preheader.
    if crate::licm::licm(func) {
        cfg = None;
        dirty = true;
    }
    dirty |= forward_loads(func, &mut cfg);
    // Parameters simplified away or loads forwarded can expose constant branch conditions.
    if dirty {
        gvn_cached(func, &mut cfg);
        remove_unreachable(func);
        simplify_params(func);
    }
    sink_into_exits(func, &mut cfg);
    dce(func);
    debug_assert!(
        crate::verify::verify(func).is_ok(),
        "{}",
        crate::verify::verify(func).unwrap_err()
    );
}

/// Drop blocks the entry cannot reach; whether any went.
pub fn remove_unreachable(func: &mut Function) -> bool {
    let seen = crate::cfg::reachable(func);
    if func.layout.iter().all(|b| seen[b.index()]) {
        return false;
    }
    func.layout.retain(|b| seen[b.index()]);
    true
}

/// Every block's incoming branch edges as (terminator, successor slot), in one allocation.
struct Incoming {
    ranges: Vec<(u32, u32)>,
    edges: Vec<(Inst, u32)>,
}

impl Incoming {
    fn of(&self, b: Block) -> &[(Inst, u32)] {
        let (start, len) = self.ranges[b.index()];
        &self.edges[start as usize..(start + len) as usize]
    }
}

fn incoming_all(func: &Function) -> Incoming {
    let mut ranges = vec![(0u32, 0u32); func.blocks.len()];
    for &b in &func.layout {
        if let Some(t) = func.terminator(b) {
            for c in func.inst(t).successors() {
                ranges[c.block.index()].1 += 1;
            }
        }
    }
    let mut total = 0u32;
    for r in &mut ranges {
        r.0 = total;
        total += r.1;
        r.1 = 0;
    }
    let mut edges = vec![(Inst(0), 0u32); total as usize];
    for &b in &func.layout {
        if let Some(t) = func.terminator(b) {
            for (i, c) in func.inst(t).successors().iter().enumerate() {
                let r = &mut ranges[c.block.index()];
                edges[(r.0 + r.1) as usize] = (t, i as u32);
                r.1 += 1;
            }
        }
    }
    Incoming { ranges, edges }
}

/// Remove the parameters of `block` flagged in `drop` and the matching argument on each of its
/// incoming `edges`.
fn remove_params(func: &mut Function, block: Block, drop: &[bool], edges: &[(Inst, u32)]) {
    let keep = |i: usize| !drop[i];
    for &(t, slot) in edges {
        let args = &mut func.insts[t.index()].successor_mut(slot as usize).args;
        let mut i = 0;
        args.retain(|_| {
            i += 1;
            keep(i - 1)
        });
    }
    let mut i = 0;
    func.blocks[block.index()].params.retain(|_| {
        i += 1;
        keep(i - 1)
    });
    for (i, &p) in func.blocks[block.index()].params.iter().enumerate() {
        if let ValueDef::Param(_, n) = &mut func.values[p.index()].def {
            *n = i as u32;
        }
    }
}

/// Replace parameters whose every incoming argument is one value (or the parameter itself)
/// by that value. Iterates to a fixpoint, since removing one can make another trivial.
pub fn simplify_params(func: &mut Function) -> bool {
    let entry = func.entry();
    let mut work: Vec<Block> = func
        .layout
        .iter()
        .copied()
        .filter(|&b| b != entry && !func.blocks[b.index()].params.is_empty())
        .collect();
    if work.is_empty() {
        return false;
    }
    // Removing parameters leaves the edges themselves unchanged.
    let incoming = incoming_all(func);
    let mut drop: Vec<bool> = Vec::new();
    let mut changed = true;
    let mut any_change = false;
    while changed {
        changed = false;
        for &b in &work {
            let edges = incoming.of(b);
            let n = func.blocks[b.index()].params.len();
            drop.clear();
            drop.resize(n, false);
            let mut any = false;
            for i in 0..n {
                let p = func.blocks[b.index()].params[i];
                let mut same: Option<Value> = None;
                let mut trivial = true;
                for &(t, slot) in edges {
                    let a = func.resolve(func.inst(t).successors()[slot as usize].args[i]);
                    if a == p || Some(a) == same {
                        continue;
                    }
                    if same.is_some() {
                        trivial = false;
                        break;
                    }
                    same = Some(a);
                }
                if let (true, Some(v)) = (trivial, same) {
                    func.replace_uses(p, v);
                    drop[i] = true;
                    any = true;
                }
            }
            if any {
                remove_params(func, b, &drop, edges);
                changed = true;
                any_change = true;
            }
        }
        work.retain(|b| !func.blocks[b.index()].params.is_empty());
    }
    if any_change {
        func.resolve_aliases();
    }
    any_change
}

/// The I32 `x` when `v` is `FromSint(x)` to F64.
fn exact_i32_source(func: &Function, v: Value) -> Option<Value> {
    let ValueDef::Result(inst, 0) = func.values[func.resolve(v).index()].def else {
        return None;
    };
    match *func.inst(inst) {
        InstData::Convert {
            op: ConvOp::FromSint,
            to: Type::F64,
            arg,
        } if func.value_type(arg) == Type::I32 => Some(func.resolve(arg)),
        _ => None,
    }
}

/// Whether `v` is an integer-to-float conversion: never NaN and never -0.0.
fn is_int_to_float(func: &Function, v: Value) -> bool {
    let ValueDef::Result(inst, 0) = func.values[func.resolve(v).index()].def else {
        return false;
    };
    matches!(
        func.inst(inst),
        InstData::Convert {
            op: ConvOp::FromSint | ConvOp::FromUint,
            ..
        }
    )
}

/// Comparisons whose outcome is fixed because an operand comes from an integer conversion:
/// `x == x` / `x != x` on a non-NaN, and a test for the -0.0 bit pattern.
fn fold_int_float_compare(func: &Function, data: &InstData) -> Option<u64> {
    const NEG_ZERO_BITS: u64 = 0x8000_0000_0000_0000;
    match data {
        InstData::FloatCmp {
            cc: cc @ (FloatCC::Eq | FloatCC::Ne),
            args: [a, b],
        } if func.resolve(*a) == func.resolve(*b) && is_int_to_float(func, *a) => {
            Some((*cc == FloatCC::Eq) as u64)
        }
        InstData::IntCmp {
            cc: cc @ (IntCC::Eq | IntCC::Ne),
            args: [a, b],
        } => {
            let other = match (const_of(func, *a), const_of(func, *b)) {
                (None, Some(NEG_ZERO_BITS)) => *a,
                (Some(NEG_ZERO_BITS), None) => *b,
                _ => return None,
            };
            let ValueDef::Result(inst, 0) = func.values[func.resolve(other).index()].def else {
                return None;
            };
            match *func.inst(inst) {
                InstData::Convert {
                    op: ConvOp::Bitcast,
                    to: Type::I64,
                    arg,
                } if func.value_type(arg) == Type::F64 && is_int_to_float(func, arg) => {
                    Some((*cc == IntCC::Ne) as u64)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Operand of a float comparison that is exactly an int32: its I32 source or an integral
/// constant in range.
enum ExactI32 {
    Value(Value),
    Imm(i32),
}

fn exact_i32_operand(func: &Function, v: Value) -> Option<ExactI32> {
    if let Some(x) = exact_i32_source(func, v) {
        return Some(ExactI32::Value(x));
    }
    let f = f64::from_bits(const_of(func, v).filter(|_| func.value_type(v) == Type::F64)?);
    (f.fract() == 0.0 && f >= i32::MIN as f64 && f <= i32::MAX as f64)
        .then_some(ExactI32::Imm(f as i32))
}

/// `FloatCmp` of two exact int32s as the equivalent signed `IntCmp`; at least one side must
/// be an int32 conversion, constant-only compares are left to constant folding.
fn int_compare_of_float_compare(
    func: &Function,
    data: &InstData,
) -> Option<(IntCC, ExactI32, ExactI32)> {
    let InstData::FloatCmp { cc, args: [a, b] } = data else {
        return None;
    };
    let (a, b) = (exact_i32_operand(func, *a)?, exact_i32_operand(func, *b)?);
    if matches!((&a, &b), (ExactI32::Imm(_), ExactI32::Imm(_))) {
        return None;
    }
    let cc = match cc {
        FloatCC::Eq => IntCC::Eq,
        FloatCC::Ne => IntCC::Ne,
        FloatCC::Lt => IntCC::Slt,
        FloatCC::Le => IntCC::Sle,
        FloatCC::Gt => IntCC::Sgt,
        FloatCC::Ge => IntCC::Sge,
    };
    Some((cc, a, b))
}

/// The value of `e`, inserting an I32 constant before the instruction at `at - 1` of `block`.
fn materialize_i32(func: &mut Function, block: Block, at: &mut usize, e: ExactI32) -> Value {
    match e {
        ExactI32::Value(v) => v,
        ExactI32::Imm(imm) => {
            let c = func.make_inst(InstData::Iconst {
                ty: Type::I32,
                imm: imm as i64,
            });
            func.blocks[block.index()].insts.insert(*at - 1, c);
            *at += 1;
            func.results(c)[0]
        }
    }
}

/// Carry an F64 block parameter as an I32 when every incoming argument is `FromSint` of an
/// I32 (or the parameter itself, around a loop): the conversion moves to the top of the block,
/// where GVN can see that the value is an exact integer. Iterates to a fixpoint, since a
/// narrowed parameter's conversion can make another parameter's arguments qualify.
fn narrow_params(func: &mut Function) -> bool {
    let entry = func.entry();
    let mut any_change = false;
    for _ in 0..8 {
        let incoming = incoming_all(func);
        let mut changed = false;
        for bi in 0..func.layout.len() {
            let b = func.layout[bi];
            if b == entry || incoming.of(b).is_empty() {
                continue;
            }
            let old_params = func.blocks[b.index()].params.clone();
            let mut drop = vec![false; old_params.len()];
            let mut narrowed: Vec<(usize, Value)> = Vec::new();
            for (i, &p) in old_params.iter().enumerate() {
                if func.value_type(p) != Type::F64 {
                    continue;
                }
                let qualifies = incoming.of(b).iter().all(|&(t, slot)| {
                    let a = func.resolve(func.inst(t).successors()[slot as usize].args[i]);
                    a == p || exact_i32_source(func, a).is_some()
                });
                let carries_value = incoming.of(b).iter().any(|&(t, slot)| {
                    func.resolve(func.inst(t).successors()[slot as usize].args[i]) != p
                });
                if !qualifies || !carries_value {
                    continue;
                }
                let narrow = func.append_block_param(b, Type::I32);
                for &(t, slot) in incoming.of(b) {
                    let a = func.resolve(func.inst(t).successors()[slot as usize].args[i]);
                    let arg = if a == p {
                        narrow
                    } else {
                        exact_i32_source(func, a).unwrap()
                    };
                    func.insts[t.index()].successor_mut(slot as usize).args.push(arg);
                }
                drop[i] = true;
                narrowed.push((i, narrow));
            }
            if narrowed.is_empty() {
                continue;
            }
            drop.resize(func.blocks[b.index()].params.len(), false);
            let mut at = 0;
            for &(i, narrow) in &narrowed {
                let widen = func.make_inst(InstData::Convert {
                    op: ConvOp::FromSint,
                    to: Type::F64,
                    arg: narrow,
                });
                func.blocks[b.index()].insts.insert(at, widen);
                at += 1;
                func.replace_uses(old_params[i], func.results(widen)[0]);
            }
            remove_params(func, b, &drop, incoming.of(b));
            changed = true;
        }
        func.resolve_aliases();
        any_change |= changed;
        if !changed {
            break;
        }
    }
    any_change
}

/// Memory contents known at a program point: the value a load of `kind` at `addr + offset` yields.
#[derive(Clone, Copy)]
struct Known {
    addr: Value,
    offset: i32,
    kind: MemKind,
    value: Value,
    class: AliasClass,
    /// Never written while the function runs: survives every store and call.
    readonly: bool,
}

const KNOWN_CAP: usize = 32;

/// `bits` after storing and reloading through the narrow `kind`.
fn through_memory(kind: MemKind, bits: u64) -> u64 {
    use MemKind::*;
    match kind {
        I32U8 | I64U8 => bits & 0xff,
        I32U16 | I64U16 => bits & 0xffff,
        I64U32 => bits & 0xffff_ffff,
        I32S8 => bits as u8 as i8 as i32 as u32 as u64,
        I64S8 => bits as u8 as i8 as i64 as u64,
        I32S16 => bits as u16 as i16 as i32 as u32 as u64,
        I64S16 => bits as u16 as i16 as i64 as u64,
        I64S32 => bits as u32 as i32 as i64 as u64,
        I32 | I64 | F32 | F64 | V128 => bits,
    }
}

/// Whether reloading what `Store { kind, value }` wrote yields `value` itself.
fn store_forwards(func: &Function, kind: MemKind, value: Value) -> bool {
    if func.value_type(value) != kind.ty() {
        return false;
    }
    if matches!(
        kind,
        MemKind::I32 | MemKind::I64 | MemKind::F32 | MemKind::F64 | MemKind::V128
    ) {
        return true;
    }
    if let Some(bits) = const_of(func, value) {
        return through_memory(kind, bits) == bits;
    }
    matches!(
        func.values[func.resolve(value).index()].def,
        ValueDef::Result(i, 0) if matches!(func.inst(i), InstData::Load { kind: k, .. } if *k == kind)
    )
}

/// Redundant-load elimination and store-to-load forwarding. Each block starts from the memory
/// facts at the end of its only predecessor (when that one is earlier in reverse postorder, so
/// every fact's value dominates it) and from nothing otherwise. A store to another address value
/// in the same or an unknown alias class, an overlapping store, or a call writing the fact's
/// class drops the facts it may affect; readonly facts survive everything. Terminators are
/// untouched; whether anything changed.
fn forward_loads(func: &mut Function, cfg_slot: &mut Option<Cfg>) -> bool {
    let cfg = cfg_slot.take().unwrap_or_else(|| Cfg::new(func));
    let incoming = incoming_all(func);
    let mut ends: Vec<Option<Vec<Known>>> = vec![None; func.blocks.len()];
    let mut changed = false;
    let mut removed: Vec<Inst> = Vec::new();
    for &b in &cfg.rpo {
        let mut known: Vec<Known> = match (incoming.of(b).len(), &cfg.preds[b.index()]) {
            (1, &[p]) if cfg.rpo_index[p.index()] < cfg.rpo_index[b.index()] => {
                ends[p.index()].clone().unwrap_or_default()
            }
            _ => Vec::new(),
        };
        removed.clear();
        for k in 0..func.blocks[b.index()].insts.len() {
            let inst = func.blocks[b.index()].insts[k];
            match func.inst(inst).clone() {
                InstData::Load { kind, addr, offset } => {
                    let addr = func.resolve(addr);
                    let result = func.results(inst)[0];
                    let flags = func.mem_flags(inst);
                    let hit = known
                        .iter()
                        .find(|e| e.addr == addr && e.offset == offset && e.kind == kind);
                    if let Some(e) = hit {
                        let value = func.resolve(e.value);
                        func.replace_uses(result, value);
                        removed.push(inst);
                        changed = true;
                    } else {
                        if known.len() == KNOWN_CAP {
                            known.remove(0);
                        }
                        known.push(Known {
                            addr,
                            offset,
                            kind,
                            value: result,
                            class: flags.class,
                            readonly: flags.readonly,
                        });
                    }
                }
                InstData::Store {
                    kind,
                    addr,
                    value,
                    offset,
                } => {
                    let addr = func.resolve(addr);
                    let class = func.mem_flags(inst).class;
                    let (lo, hi) = (offset as i64, offset as i64 + kind.bytes() as i64);
                    known.retain(|e| {
                        e.readonly
                            || (class.is_known()
                                && e.class.is_known()
                                && class.clamped() != e.class.clamped())
                            || e.addr == addr && {
                                let (elo, ehi) =
                                    (e.offset as i64, e.offset as i64 + e.kind.bytes() as i64);
                                ehi <= lo || hi <= elo
                            }
                    });
                    if store_forwards(func, kind, value) {
                        if known.len() == KNOWN_CAP {
                            known.remove(0);
                        }
                        known.push(Known {
                            addr,
                            offset,
                            kind,
                            value: func.resolve(value),
                            class,
                            readonly: false,
                        });
                    }
                }
                InstData::Prefetch { .. } | InstData::TrapIf { .. } => {}
                InstData::Call { .. } | InstData::CallIndirect { .. } => {
                    let writes = func.writes(inst);
                    known.retain(|e| e.readonly || !writes.may_touch(e.class));
                }
                d if d.is_pure() || d.is_terminator() => {}
                _ => known.clear(),
            }
        }
        if !removed.is_empty() {
            func.blocks[b.index()]
                .insts
                .retain(|i| !removed.contains(i));
        }
        ends[b.index()] = Some(known);
    }
    func.resolve_aliases();
    *cfg_slot = Some(cfg);
    changed
}

fn const_of(func: &Function, v: Value) -> Option<u64> {
    match func.values[func.resolve(v).index()].def {
        ValueDef::Result(inst, 0) => match func.inst(inst) {
            InstData::Iconst { ty, imm } => Some(eval::iconst(*ty, *imm)),
            InstData::F32const { bits } => Some(*bits as u64),
            InstData::F64const { bits } => Some(*bits),
            _ => None,
        },
        _ => None,
    }
}

fn const_data(ty: Type, bits: u64) -> InstData {
    match ty {
        Type::I32 => InstData::Iconst {
            ty,
            imm: bits as u32 as i32 as i64,
        },
        Type::I64 => InstData::Iconst {
            ty,
            imm: bits as i64,
        },
        Type::F32 => InstData::F32const { bits: bits as u32 },
        Type::F64 => InstData::F64const { bits },
        Type::V128 => unreachable!("vector constants are not scalar-folded"),
    }
}

/// An identity that makes `inst` equal to one of its operands.
fn identity(func: &Function, data: &InstData) -> Option<Value> {
    use BinaryOp::*;
    match data {
        InstData::Binary { op, args: [a, b] } => {
            let (ca, cb) = (const_of(func, *a), const_of(func, *b));
            let ty = func.value_type(*a);
            let ones = eval::norm(ty, u64::MAX);
            match op {
                Iadd | Bor | Bxor if cb == Some(0) => Some(*a),
                Iadd | Bor | Bxor if ca == Some(0) => Some(*b),
                Isub | Ishl | Ushr | Sshr | Rotl | Rotr if cb == Some(0) => Some(*a),
                Imul if cb == Some(1) => Some(*a),
                Imul if ca == Some(1) => Some(*b),
                Sdiv | Udiv if cb == Some(1) => Some(*a),
                Band if cb == Some(ones) => Some(*a),
                Band if ca == Some(ones) => Some(*b),
                Band | Bor if a == b => Some(*a),
                _ => None,
            }
        }
        InstData::Select {
            cond,
            if_true,
            if_false,
        } => {
            if if_true == if_false {
                return Some(*if_true);
            }
            const_of(func, *cond).map(|c| if c as u32 != 0 { *if_true } else { *if_false })
        }
        _ => None,
    }
}

/// Canonical form for hashing: commutative operands ordered, constants to the right.
fn canonical(func: &Function, data: &InstData) -> InstData {
    let mut d = data.clone();
    match &mut d {
        InstData::Binary { op, args } if op.is_commutative() => {
            let a_const = const_of(func, args[0]).is_some();
            let b_const = const_of(func, args[1]).is_some();
            if (a_const && !b_const) || (a_const == b_const && args[0] > args[1]) {
                args.swap(0, 1);
            }
        }
        InstData::IntCmp { cc, args } => {
            let a_const = const_of(func, args[0]).is_some();
            let b_const = const_of(func, args[1]).is_some();
            if (a_const && !b_const) || (a_const == b_const && args[0] > args[1]) {
                args.swap(0, 1);
                *cc = cc.swap();
            }
        }
        _ => {}
    }
    d
}

/// Dominator-scoped global value numbering with folding.
pub fn gvn(func: &mut Function) {
    gvn_cached(func, &mut None);
}

/// [`gvn`] over `cfg` when present (it must describe `func`); afterwards `cfg` is `None` if
/// the pass changed a terminator, and otherwise still describes `func`.
fn gvn_cached(func: &mut Function, cfg_slot: &mut Option<Cfg>) {
    let cfg = cfg_slot.take().unwrap_or_else(|| Cfg::new(func));
    let children = cfg.dom_children_adj();
    let mut table: FastMap<InstData, Value> = FastMap::default();
    let mut log: Vec<InstData> = Vec::new();
    let incoming = incoming_all(func);
    let mut facts: FastMap<Value, bool> = FastMap::default();
    let mut fact_log: Vec<Value> = Vec::new();
    let mut terminators_changed = false;
    enum Step {
        Enter(Block),
        Leave(usize, usize),
    }
    let mut stack = vec![Step::Enter(func.entry())];
    let mut removed: Vec<Inst> = Vec::new();
    while let Some(step) = stack.pop() {
        let b = match step {
            Step::Leave(mark, fact_mark) => {
                for k in log.drain(mark..) {
                    table.remove(&k);
                }
                for v in fact_log.drain(fact_mark..) {
                    facts.remove(&v);
                }
                continue;
            }
            Step::Enter(b) => b,
        };
        let mark = log.len();
        let fact_mark = fact_log.len();
        if let [(t, slot)] = *incoming.of(b) {
            if b != func.entry() {
                if let InstData::Brif { cond, .. } = func.inst(t) {
                    assume(func, &mut facts, &mut fact_log, *cond, slot == 0);
                }
            }
        }
        removed.clear();
        let mut at = 0;
        while at < func.blocks[b.index()].insts.len() {
            let inst = func.blocks[b.index()].insts[at];
            at += 1;
            let mut stale = false;
            if func.has_pending_aliases() {
                func.inst(inst).for_each_arg(|a| stale |= func.resolve(a) != a);
            }
            if stale {
                let values = &func.values;
                func.insts[inst.index()].map_args(|mut v| {
                    while let ValueDef::Alias(to) = values[v.index()].def {
                        v = to;
                    }
                    v
                });
            }
            // Every signed I32 is exactly representable as F64. Recover it without an FP
            // conversion when JS bit operations or array indexing consume an unboxed local.
            if let InstData::Convert {
                op: ConvOp::ToJsInt32 | ConvOp::ToSintSat | ConvOp::ToSint,
                to,
                arg,
            } = func.inst(inst)
            {
                if let ValueDef::Result(source, 0) = func.values[arg.index()].def {
                    if let InstData::Convert {
                        op: ConvOp::FromSint,
                        to: Type::F64,
                        arg: integer,
                    } = *func.inst(source)
                    {
                        if func.value_type(integer) == Type::I32 {
                            match *to {
                                Type::I32 => {
                                    let result = func.results(inst)[0];
                                    func.replace_uses(result, integer);
                                    removed.push(inst);
                                    continue;
                                }
                                Type::I64 => {
                                    func.insts[inst.index()] = InstData::Convert {
                                        op: ConvOp::Sext,
                                        to: Type::I64,
                                        arg: integer,
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }

            // An I32 widened to I64 converts to the same float as the I32 itself.
            if let InstData::Convert {
                op: ConvOp::FromSint,
                to,
                arg,
            } = *func.inst(inst)
            {
                if let ValueDef::Result(source, 0) = func.values[arg.index()].def {
                    if let InstData::Convert {
                        op: ConvOp::Sext,
                        arg: narrow,
                        ..
                    } = *func.inst(source)
                    {
                        if func.value_type(narrow) == Type::I32 {
                            func.insts[inst.index()] = InstData::Convert {
                                op: ConvOp::FromSint,
                                to,
                                arg: narrow,
                            };
                        }
                    }
                }
            }

            // Branch folding.
            match func.inst(inst) {
                InstData::Brif { cond, then, else_ } => {
                    let folded = if let Some(c) = const_of(func, *cond) {
                        Some(if c as u32 != 0 { then } else { else_ })
                    } else if let Some(&t) = facts.get(&func.resolve(*cond)) {
                        Some(if t { then } else { else_ })
                    } else if then == else_ {
                        Some(then)
                    } else {
                        None
                    };
                    if let Some(dest) = folded {
                        let dest = dest.clone();
                        func.insts[inst.index()] = InstData::Jump { dest };
                        terminators_changed = true;
                    }
                    continue;
                }
                InstData::BrTable {
                    index,
                    targets,
                    default,
                } => {
                    if let Some(c) = const_of(func, *index) {
                        let dest = targets.get(c as u32 as usize).unwrap_or(default).clone();
                        func.insts[inst.index()] = InstData::Jump { dest };
                        terminators_changed = true;
                    }
                    continue;
                }
                InstData::TrapIf { cond, code } => {
                    match const_of(func, *cond) {
                        Some(0) => removed.push(inst),
                        Some(_) => {
                            // Always traps: the rest of the block is dead.
                            let code = *code;
                            func.insts[inst.index()] = InstData::Trap { code };
                            func.blocks[b.index()].insts.truncate(at);
                            terminators_changed = true;
                            break;
                        }
                        None => {}
                    }
                    continue;
                }
                _ => {}
            }

            if let Some(bits) = fold_int_float_compare(func, func.inst(inst)) {
                func.insts[inst.index()] = const_data(Type::I32, bits);
            }
            if let Some((cc, lhs, rhs)) = int_compare_of_float_compare(func, func.inst(inst)) {
                let lhs = materialize_i32(func, b, &mut at, lhs);
                let rhs = materialize_i32(func, b, &mut at, rhs);
                func.insts[inst.index()] = InstData::IntCmp {
                    cc,
                    args: [lhs, rhs],
                };
            }
            let data = func.inst(inst);
            // A readonly load equals any dominating load of the same location.
            if matches!(data, InstData::Load { .. }) && func.mem_flags(inst).readonly {
                let result = func.results(inst)[0];
                match table.get(data) {
                    Some(&v) => {
                        func.replace_uses(result, v);
                        removed.push(inst);
                    }
                    None => {
                        let key = data.clone();
                        table.insert(key.clone(), result);
                        log.push(key);
                    }
                }
                continue;
            }
            if !data.is_pure() && !is_foldable_trapping(data) {
                continue;
            }
            // Multi-result arithmetic must preserve both the value and overflow flag.
            if func.results(inst).len() != 1 {
                continue;
            }
            let result = func.results(inst)[0];
            let ty = func.value_type(result);

            // Constant folding (trapping ops fold only when defined).
            let mut args_const = true;
            data.for_each_arg(|a| args_const &= const_of(func, a).is_some());
            if args_const
                && !matches!(
                    data,
                    InstData::Iconst { .. } | InstData::F32const { .. } | InstData::F64const { .. }
                )
            {
                if let Some(bits) = eval::pure_inst(func, data, |a| const_of(func, a).unwrap()) {
                    func.insts[inst.index()] = const_data(ty, bits);
                }
            } else if let Some(v) = identity(func, data) {
                func.replace_uses(result, func.resolve(v));
                removed.push(inst);
                continue;
            }
            if !func.inst(inst).is_pure() {
                continue;
            }

            let key = canonical(func, func.inst(inst));
            match table.get(&key) {
                Some(&v) => {
                    func.replace_uses(result, v);
                    removed.push(inst);
                }
                None => {
                    table.insert(key.clone(), result);
                    log.push(key);
                }
            }
        }
        if !removed.is_empty() {
            func.blocks[b.index()]
                .insts
                .retain(|i| !removed.contains(i));
        }
        stack.push(Step::Leave(mark, fact_mark));
        for &c in children[b.index()].iter().rev() {
            stack.push(Step::Enter(c));
        }
    }
    func.resolve_aliases();
    if !terminators_changed {
        *cfg_slot = Some(cfg);
    }
}

/// Record that `cond` is nonzero (`truth`) or zero for the current dominator scope, and what
/// that implies for the operands of a defining Band (nonzero) or Bor (zero).
fn assume(
    func: &Function,
    facts: &mut FastMap<Value, bool>,
    log: &mut Vec<Value>,
    cond: Value,
    truth: bool,
) {
    let cond = func.resolve(cond);
    if func.value_type(cond) != Type::I32 {
        return;
    }
    if !facts.contains_key(&cond) {
        facts.insert(cond, truth);
        log.push(cond);
    }
    if let ValueDef::Result(inst, 0) = func.values[cond.index()].def {
        if let InstData::Binary { op, args } = func.inst(inst) {
            if (*op == BinaryOp::Band && truth) || (*op == BinaryOp::Bor && !truth) {
                for &a in args {
                    let a = func.resolve(a);
                    if !facts.contains_key(&a) {
                        facts.insert(a, truth);
                        log.push(a);
                    }
                }
            }
        }
    }
}

fn is_foldable_trapping(data: &InstData) -> bool {
    matches!(data, InstData::Binary { .. } | InstData::Convert { .. })
}

/// Remove pure instructions and block parameters whose values are never used.
pub fn dce(func: &mut Function) {
    let mut live = vec![false; func.values.len()];
    let mut work = Vec::new();
    let mark = |v: Value, live: &mut Vec<bool>, work: &mut Vec<Value>| {
        if !live[v.index()] {
            live[v.index()] = true;
            work.push(v);
        }
    };
    // Branch edges into each block, for propagating parameter liveness.
    let edges = incoming_all(func);
    for &b in &func.layout {
        for &inst in &func.blocks[b.index()].insts {
            let d = func.inst(inst);
            if d.is_pure() {
                continue;
            }
            // Effects and control flow: operands are live, except branch arguments, which
            // are live only through their parameter.
            match d {
                InstData::Jump { .. } => {}
                InstData::Brif { cond, .. } => mark(*cond, &mut live, &mut work),
                InstData::BrTable { index, .. } => mark(*index, &mut live, &mut work),
                _ => d.for_each_arg(|a| mark(a, &mut live, &mut work)),
            }
            // Side-effecting results are kept regardless.
        }
    }
    let entry = func.entry();
    for &p in &func.blocks[entry.index()].params {
        mark(p, &mut live, &mut work);
    }
    while let Some(v) = work.pop() {
        match func.values[v.index()].def {
            ValueDef::Result(inst, _) => {
                func.inst(inst)
                    .for_each_arg(|a| mark(a, &mut live, &mut work));
            }
            ValueDef::Param(b, n) => {
                for &(t, slot) in edges.of(b) {
                    let a = func.inst(t).successors()[slot as usize].args[n as usize];
                    mark(a, &mut live, &mut work);
                }
            }
            ValueDef::Alias(to) => mark(to, &mut live, &mut work),
        }
    }
    let mut dead: Vec<bool> = Vec::new();
    for bi in 0..func.layout.len() {
        let b = func.layout[bi];
        let insts = std::mem::take(&mut func.blocks[b.index()].insts);
        let mut insts = insts;
        insts.retain(|&i| !func.inst(i).is_pure() || func.results(i).iter().any(|r| live[r.index()]));
        func.blocks[b.index()].insts = insts;
        if b == entry {
            continue;
        }
        dead.clear();
        dead.extend(
            func.blocks[b.index()]
                .params
                .iter()
                .map(|p| !live[p.index()]),
        );
        if dead.iter().any(|&d| d) {
            remove_params(func, b, &dead, edges.of(b));
        }
    }
}

/// Move pure instructions whose results only exits (blocks from which every path ends the
/// function, or marked cold) read into those exits, one copy per exit block that uses them:
/// the hot path stops computing values (a Number materialized for a deoptimization snapshot) it never reads, and
/// they stop occupying registers there.
/// Per-value lists of the blocks reading it, in one allocation (a list that outgrows its slot
/// moves to the end).
struct UseLists {
    ranges: Vec<(u32, u32, u32)>,
    items: Vec<Block>,
}

impl UseLists {
    fn list(&self, v: Value) -> &[Block] {
        let (start, len, _) = self.ranges[v.index()];
        &self.items[start as usize..(start + len) as usize]
    }

    fn push(&mut self, v: Value, b: Block) {
        let (start, len, cap) = self.ranges[v.index()];
        if len == cap {
            let new_cap = (cap * 2).max(4);
            let new_start = self.items.len() as u32;
            self.items
                .extend_from_within(start as usize..(start + len) as usize);
            self.items
                .resize(new_start as usize + new_cap as usize, Block(0));
            self.ranges[v.index()] = (new_start, len, new_cap);
        }
        let (start, len, cap) = self.ranges[v.index()];
        self.items[(start + len) as usize] = b;
        self.ranges[v.index()] = (start, len + 1, cap);
    }

    fn swap_remove_first(&mut self, v: Value, b: Block) {
        let (start, len, cap) = self.ranges[v.index()];
        let list = &mut self.items[start as usize..(start + len) as usize];
        if let Some(p) = list.iter().position(|&x| x == b) {
            list.swap(p, len as usize - 1);
            self.ranges[v.index()] = (start, len - 1, cap);
        }
    }

    fn remove_all(&mut self, v: Value, b: Block) {
        let (start, len, cap) = self.ranges[v.index()];
        let list = &mut self.items[start as usize..(start + len) as usize];
        let mut kept = 0;
        for i in 0..list.len() {
            if list[i] != b {
                list[kept] = list[i];
                kept += 1;
            }
        }
        self.ranges[v.index()] = (start, kept as u32, cap);
    }

    fn grow_to(&mut self, values: usize) {
        self.ranges.resize(values, (0, 0, 0));
    }
}

fn sink_into_exits(func: &mut Function, cfg_slot: &mut Option<Cfg>) {
    let cfg = cfg_slot.take().unwrap_or_else(|| Cfg::new(func));
    sink_with(func, &cfg);
    *cfg_slot = Some(cfg);
}

fn sink_with(func: &mut Function, cfg: &Cfg) {
    let mut cold = crate::cfg::cold_blocks(func, cfg, &cfg.rpo);
    for &b in &cfg.rpo {
        cold[b.index()] |= func.blocks[b.index()].cold;
    }
    if !cold.iter().any(|&c| c) {
        return;
    }
    let mut uses = UseLists {
        ranges: vec![(0, 0, 0); func.values.len()],
        items: Vec::new(),
    };
    for &b in &cfg.rpo {
        for &i in &func.blocks[b.index()].insts {
            func.inst(i).for_each_arg(|a| uses.ranges[a.index()].1 += 1);
        }
    }
    let mut total = 0u32;
    for r in &mut uses.ranges {
        r.0 = total;
        r.2 = r.1;
        total += r.1;
        r.1 = 0;
    }
    uses.items = vec![Block(0); total as usize];
    for &b in &cfg.rpo {
        for k in 0..func.blocks[b.index()].insts.len() {
            let i = func.blocks[b.index()].insts[k];
            func.inst(i).for_each_arg(|a| uses.push(a, b));
        }
    }
    let mut gone = vec![false; func.insts.len()];
    let mut targets: Vec<Block> = Vec::new();
    for &b in cfg.rpo.iter().rev() {
        if cold[b.index()] {
            continue;
        }
        let mut any_gone = false;
        for k in (0..func.blocks[b.index()].insts.len()).rev() {
            let i = func.blocks[b.index()].insts[k];
            let data = func.inst(i);
            if !data.is_pure() || data.is_terminator() || func.results(i).len() != 1 {
                continue;
            }
            let r = func.results(i)[0];
            let at = uses.list(r);
            if at.is_empty() || at.iter().any(|&u| u == b || !cold[u.index()]) {
                continue;
            }
            targets.clear();
            for &u in at {
                if !targets.contains(&u) {
                    targets.push(u);
                }
            }
            let data = data.clone();
            data.for_each_arg(|a| uses.swap_remove_first(a, b));
            for &u in &targets {
                let copy = func.make_inst(data.clone());
                let nv = func.results(copy)[0];
                if uses.ranges.len() <= nv.index() {
                    uses.grow_to(func.values.len());
                }
                for k in 0..func.blocks[u.index()].insts.len() {
                    let j = func.blocks[u.index()].insts[k];
                    func.insts[j.index()].map_args(|v| if v == r { nv } else { v });
                }
                let moved = uses.list(r).iter().filter(|&&x| x == u).count();
                uses.remove_all(r, u);
                for _ in 0..moved {
                    uses.push(nv, u);
                }
                data.for_each_arg(|a| uses.push(a, u));
                func.blocks[u.index()].insts.insert(0, copy);
            }
            gone[i.index()] = true;
            any_gone = true;
        }
        if any_gone {
            func.blocks[b.index()].insts.retain(|i| !gone[i.index()]);
        }
    }
}
