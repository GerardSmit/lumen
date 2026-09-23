//! Mid-level optimizations over SSA.
//!
//! [`optimize`] runs the pipeline: unreachable-block removal, trivial block-parameter removal,
//! dominator-scoped GVN with constant folding, algebraic identities and branch folding, then
//! dead-code elimination (including dead block parameters and their branch arguments).

use crate::cfg::Cfg;
use crate::eval;
use crate::ir::*;
use std::collections::HashMap;

pub fn optimize(func: &mut Function) {
    remove_unreachable(func);
    simplify_params(func);
    gvn(func);
    remove_unreachable(func);
    simplify_params(func);
    dce(func);
    debug_assert!(
        crate::verify::verify(func).is_ok(),
        "{}",
        crate::verify::verify(func).unwrap_err()
    );
}

/// Drop blocks the entry cannot reach.
pub fn remove_unreachable(func: &mut Function) {
    let cfg = Cfg::new(func);
    func.layout.retain(|&b| cfg.is_reachable(b));
}

/// Every branch edge into `block`, as the terminator and the successor slot.
fn incoming(func: &Function, block: Block) -> Vec<(Inst, usize)> {
    let mut out = Vec::new();
    for &b in &func.layout {
        if let Some(t) = func.terminator(b) {
            for (i, c) in func.inst(t).successors().iter().enumerate() {
                if c.block == block {
                    out.push((t, i));
                }
            }
        }
    }
    out
}

/// Remove parameter `idx` of `block` and the matching argument on every incoming edge.
fn remove_param(func: &mut Function, block: Block, idx: usize) {
    for (t, slot) in incoming(func, block) {
        func.insts[t.index()].successors_mut()[slot].args.remove(idx);
    }
    func.blocks[block.index()].params.remove(idx);
    let params = func.blocks[block.index()].params.clone();
    for (i, p) in params.into_iter().enumerate().skip(idx) {
        if let ValueDef::Param(_, n) = &mut func.values[p.index()].def {
            *n = i as u32;
        }
    }
}

/// Replace parameters whose every incoming argument is one value (or the parameter itself)
/// by that value. Iterates to a fixpoint, since removing one can make another trivial.
pub fn simplify_params(func: &mut Function) {
    let entry = func.entry();
    let mut changed = true;
    while changed {
        changed = false;
        for bi in 0..func.layout.len() {
            let b = func.layout[bi];
            if b == entry {
                continue;
            }
            let edges = incoming(func, b);
            let mut i = 0;
            while i < func.blocks[b.index()].params.len() {
                let p = func.blocks[b.index()].params[i];
                let mut same: Option<Value> = None;
                let mut trivial = true;
                for &(t, slot) in &edges {
                    let a = func.resolve(func.inst(t).successors()[slot].args[i]);
                    if a == p || Some(a) == same {
                        continue;
                    }
                    if same.is_some() {
                        trivial = false;
                        break;
                    }
                    same = Some(a);
                }
                match (trivial, same) {
                    (true, Some(v)) => {
                        func.replace_uses(p, v);
                        remove_param(func, b, i);
                        changed = true;
                    }
                    _ => i += 1,
                }
            }
        }
        func.resolve_aliases();
    }
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
    let cfg = Cfg::new(func);
    let children = cfg.dom_children();
    let mut table: HashMap<InstData, Value> = HashMap::new();
    // Explicit DFS over the dominator tree; each frame remembers the keys it added.
    enum Step {
        Enter(Block),
        Leave(Vec<InstData>),
    }
    let mut stack = vec![Step::Enter(func.entry())];
    while let Some(step) = stack.pop() {
        let b = match step {
            Step::Leave(keys) => {
                for k in keys {
                    table.remove(&k);
                }
                continue;
            }
            Step::Enter(b) => b,
        };
        let mut added = Vec::new();
        let insts = func.blocks[b.index()].insts.clone();
        let mut removed = Vec::new();
        for inst in insts {
            // Operands may have been aliased by earlier replacements.
            let resolved: Vec<Value> = {
                let mut v = Vec::new();
                func.inst(inst).for_each_arg(|a| v.push(a));
                v
            };
            if resolved.iter().any(|&a| func.resolve(a) != a) {
                let f = &*func;
                let mut d = f.inst(inst).clone();
                d.map_args(|a| f.resolve(a));
                func.insts[inst.index()] = d;
            }
            let data = func.inst(inst).clone();

            // Branch folding.
            match &data {
                InstData::Brif { cond, then, else_ } => {
                    if let Some(c) = const_of(func, *cond) {
                        let dest = if c as u32 != 0 { then } else { else_ };
                        func.insts[inst.index()] = InstData::Jump { dest: dest.clone() };
                    } else if then == else_ {
                        func.insts[inst.index()] = InstData::Jump { dest: then.clone() };
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
                            let insts = &mut func.blocks[b.index()].insts;
                            let at = insts.iter().position(|&i| i == inst).unwrap();
                            insts.truncate(at + 1);
                            break;
                        }
                        None => {}
                    }
                    continue;
                }
                _ => {}
            }

            if !data.is_pure() && !is_foldable_trapping(&data) {
                continue;
            }
            let result = func.results(inst)[0];
            let ty = func.value_type(result);

            // Constant folding (trapping ops fold only when defined).
            let mut args_const = true;
            data.for_each_arg(|a| args_const &= const_of(func, a).is_some());
            if args_const && !matches!(data, InstData::Iconst { .. } | InstData::F32const { .. } | InstData::F64const { .. }) {
                if let Some(bits) = eval::pure_inst(func, &data, |a| const_of(func, a).unwrap()) {
                    func.insts[inst.index()] = const_data(ty, bits);
                }
            } else if let Some(v) = identity(func, &data) {
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
                    added.push(key);
                }
            }
        }
        if !removed.is_empty() {
            func.blocks[b.index()].insts.retain(|i| !removed.contains(i));
        }
        stack.push(Step::Leave(added));
        for &c in children[b.index()].iter().rev() {
            stack.push(Step::Enter(c));
        }
    }
    func.resolve_aliases();
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
    let mut edges: Vec<Vec<(Inst, usize)>> = vec![Vec::new(); func.blocks.len()];
    for &b in &func.layout {
        if let Some(t) = func.terminator(b) {
            for (i, c) in func.inst(t).successors().iter().enumerate() {
                edges[c.block.index()].push((t, i));
            }
        }
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
                func.inst(inst).for_each_arg(|a| mark(a, &mut live, &mut work));
            }
            ValueDef::Param(b, n) => {
                for &(t, slot) in &edges[b.index()] {
                    let a = func.inst(t).successors()[slot].args[n as usize];
                    mark(a, &mut live, &mut work);
                }
            }
            ValueDef::Alias(to) => mark(to, &mut live, &mut work),
        }
    }
    for bi in 0..func.layout.len() {
        let b = func.layout[bi];
        let f = &*func;
        let keep: Vec<Inst> = f.blocks[b.index()]
            .insts
            .iter()
            .copied()
            .filter(|&i| !f.inst(i).is_pure() || f.results(i).iter().any(|r| live[r.index()]))
            .collect();
        func.blocks[b.index()].insts = keep;
        if b == entry {
            continue;
        }
        let mut i = func.blocks[b.index()].params.len();
        while i > 0 {
            i -= 1;
            if !live[func.blocks[b.index()].params[i].index()] {
                remove_param(func, b, i);
            }
        }
    }
}
