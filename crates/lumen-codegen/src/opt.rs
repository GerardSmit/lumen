//! Mid-level optimizations over SSA.
//!
//! [`optimize`] runs the pipeline: unreachable-block removal, trivial block-parameter removal,
//! dominator-scoped GVN with constant folding, algebraic identities and branch folding, then
//! dead-code elimination (including dead block parameters and their branch arguments).

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
    gvn_cached(func, &mut cfg);
    // Another GVN pass only has something to find after a change since the last one.
    let mut dirty = remove_unreachable(func);
    dirty |= simplify_params(func);
    // Small constant-trip loops: unroll fully, then fold their induction values.
    if unroll && crate::unroll::unroll(func) {
        cfg = None;
        simplify_params(func);
        gvn_cached(func, &mut cfg);
        remove_unreachable(func);
        simplify_params(func);
        dirty = true;
    }
    // Parameters simplified away can expose constant branch conditions: fold them too.
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
    let mut terminators_changed = false;
    enum Step {
        Enter(Block),
        Leave(usize),
    }
    let mut stack = vec![Step::Enter(func.entry())];
    let mut removed: Vec<Inst> = Vec::new();
    while let Some(step) = stack.pop() {
        let b = match step {
            Step::Leave(mark) => {
                for k in log.drain(mark..) {
                    table.remove(&k);
                }
                continue;
            }
            Step::Enter(b) => b,
        };
        let mark = log.len();
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
                op: ConvOp::ToJsInt32 | ConvOp::ToSintSat,
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

            // Branch folding.
            match func.inst(inst) {
                InstData::Brif { cond, then, else_ } => {
                    let folded = if let Some(c) = const_of(func, *cond) {
                        Some(if c as u32 != 0 { then } else { else_ })
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

            let data = func.inst(inst);
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
        stack.push(Step::Leave(mark));
        for &c in children[b.index()].iter().rev() {
            stack.push(Step::Enter(c));
        }
    }
    func.resolve_aliases();
    if !terminators_changed {
        *cfg_slot = Some(cfg);
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
