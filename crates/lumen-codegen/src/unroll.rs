//! Full unrolling of small loops with a constant trip count.
//!
//! A loop qualifies when its header `H` has exactly one entering edge and one back edge, ends in
//! `brif(cmp(p, c))` with one target outside the loop (the exit `X`, whose only predecessor is
//! `H`), and `p` is a header parameter that enters as a constant and steps by a constant add or
//! subtract on the back edge. Evaluating the test from the constant start gives the trip count
//! `n`; when `n` is small and the copies fit the size budget, the loop's region — every block `H`
//! dominates except the code after the exit — is cloned `n + 1` times. Copy `k`'s back edge
//! enters copy `k + 1`; copy `k < n` branches straight into its body, copy `n` straight to `X`.
//! The original loop becomes unreachable. Side exits (breaks, returns, deopt exits) stay in each
//! copy with that copy's values. Later passes fold the now-constant induction values.

use crate::cfg::Cfg;
use crate::eval;
use crate::ir::*;
use std::collections::HashMap;

/// Iterations beyond which a loop is not unrolled.
pub const MAX_TRIPS: u64 = 8;
/// Instructions (all copies together) beyond which a loop is not unrolled.
pub const MAX_INSTS: usize = 600;
/// Loops unrolled per function at most.
const MAX_LOOPS: usize = 4;

/// Unroll every qualifying loop (innermost first, up to [`MAX_LOOPS`]). Returns whether the
/// function changed. Run [`crate::opt::optimize`]'s folding passes afterwards.
pub fn unroll(func: &mut Function) -> bool {
    let mut changed = false;
    for _ in 0..MAX_LOOPS {
        func.resolve_aliases();
        if !unroll_one(func) {
            break;
        }
        changed = true;
        crate::opt::remove_unreachable(func);
    }
    changed
}

fn const_bits(func: &Function, v: Value) -> Option<u64> {
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

fn def_inst(func: &Function, v: Value) -> Option<Inst> {
    match func.values[func.resolve(v).index()].def {
        ValueDef::Result(inst, 0) => Some(inst),
        _ => None,
    }
}

/// A loop found unrollable.
struct Plan {
    header: Block,
    /// The in-loop target of the header's test and the exit.
    body: Block,
    exit: Block,
    /// The blocks to clone, in reverse postorder (the header first).
    region: Vec<Block>,
    trips: u64,
}

fn unroll_one(func: &mut Function) -> bool {
    let cfg = Cfg::new(func);
    // Innermost loops first: deepest headers first.
    let mut headers: Vec<Block> = cfg
        .rpo
        .iter()
        .copied()
        .filter(|&h| cfg.preds[h.index()].iter().any(|&p| cfg.dominates(h, p)))
        .collect();
    headers.sort_by_key(|h| std::cmp::Reverse(cfg.loop_depth[h.index()]));
    for h in headers {
        if let Some(plan) = analyze(func, &cfg, h) {
            apply(func, &cfg, &plan);
            return true;
        }
    }
    false
}

fn analyze(func: &Function, cfg: &Cfg, h: Block) -> Option<Plan> {
    let entry = func.entry();
    if h == entry {
        return None;
    }
    // Exactly one entering edge and one back edge (count edges, not blocks).
    let mut enter: Option<(Block, usize)> = None;
    let mut back: Option<(Block, usize)> = None;
    for &p in &cfg.preds[h.index()] {
        let t = func.terminator(p)?;
        for (slot, c) in func.inst(t).successors().iter().enumerate() {
            if c.block != h {
                continue;
            }
            let e = if cfg.dominates(h, p) { &mut back } else { &mut enter };
            if e.is_some() {
                return None;
            }
            *e = Some((p, slot));
        }
    }
    let (pre, pre_slot) = enter?;
    let (latch, latch_slot) = back?;
    // The natural loop.
    let mut in_loop = vec![false; func.blocks.len()];
    in_loop[h.index()] = true;
    let mut work = vec![latch];
    while let Some(x) = work.pop() {
        if in_loop[x.index()] {
            continue;
        }
        in_loop[x.index()] = true;
        work.extend(cfg.preds[x.index()].iter().copied());
    }
    // The test.
    let term = func.terminator(h)?;
    let InstData::Brif { cond, then, else_ } = func.inst(term) else {
        return None;
    };
    let (body, exit, cont_on_true) = match (in_loop[then.block.index()], in_loop[else_.block.index()]) {
        (true, false) => (then.block, else_.block, true),
        (false, true) => (else_.block, then.block, false),
        _ => return None,
    };
    if cfg.preds[exit.index()].len() != 1 || !cfg.dominates(h, exit) {
        return None;
    }
    let cmp = def_inst(func, *cond)?;
    let (cmp_data, args) = match func.inst(cmp) {
        d @ InstData::IntCmp { args, .. } | d @ InstData::FloatCmp { args, .. } => (d, *args),
        _ => return None,
    };
    let params = &func.blocks[h.index()].params;
    let is_param = |v: Value| params.iter().position(|&p| p == func.resolve(v));
    let (k, other) = match (is_param(args[0]), is_param(args[1])) {
        (Some(k), None) => (k, args[1]),
        (None, Some(k)) => (k, args[0]),
        _ => return None,
    };
    let limit = const_bits(func, other)?;
    let p = params[k];
    // Start: the constant on the entering edge.
    let start = {
        let t = func.terminator(pre)?;
        const_bits(func, func.inst(t).successors()[pre_slot].args[k])?
    };
    // Step: `p + c` / `p - c` on the back edge.
    let next = {
        let t = func.terminator(latch)?;
        func.inst(t).successors()[latch_slot].args[k]
    };
    let step_inst = def_inst(func, next)?;
    let step_data = func.inst(step_inst).clone();
    let InstData::Binary { op, args: sargs } = &step_data else {
        return None;
    };
    let step_const = match op {
        BinaryOp::Iadd | BinaryOp::Fadd => {
            if func.resolve(sargs[0]) == p {
                sargs[1]
            } else if func.resolve(sargs[1]) == p {
                sargs[0]
            } else {
                return None;
            }
        }
        BinaryOp::Isub | BinaryOp::Fsub if func.resolve(sargs[0]) == p => sargs[1],
        _ => return None,
    };
    let step_c = const_bits(func, step_const)?;
    // Count the trips by running the test and the step on constants.
    let mut v = start;
    let mut trips = 0u64;
    loop {
        let c = eval::pure_inst(func, cmp_data, |a| {
            if func.resolve(a) == p {
                v
            } else {
                limit
            }
        })?;
        if (c as u32 != 0) != cont_on_true {
            break;
        }
        trips += 1;
        if trips > MAX_TRIPS {
            return None;
        }
        v = eval::pure_inst(func, &step_data, |a| {
            if func.resolve(a) == p {
                v
            } else {
                step_c
            }
        })?;
    }
    if trips == 0 {
        return None;
    }
    // The region: blocks `h` dominates, minus the code after the exit.
    let region: Vec<Block> = cfg
        .rpo
        .iter()
        .copied()
        .filter(|&b| cfg.dominates(h, b) && !cfg.dominates(exit, b))
        .collect();
    let size: usize = region
        .iter()
        .map(|b| func.blocks[b.index()].insts.len())
        .sum();
    if size.saturating_mul(trips as usize + 1) > MAX_INSTS {
        return None;
    }
    // No other edge may re-enter the header (a second back edge from a side path).
    for &b in &region {
        if b == latch {
            continue;
        }
        if let Some(t) = func.terminator(b) {
            if func.inst(t).successors().iter().any(|c| c.block == h) {
                return None;
            }
        }
    }
    Some(Plan {
        header: h,
        body,
        exit,
        region,
        trips,
    })
}

fn apply(func: &mut Function, cfg: &Cfg, plan: &Plan) {
    let h = plan.header;
    let n = plan.trips as usize;
    // Clone the region n + 1 times; copy k's back edge enters copy k + 1 (the last copy's
    // back edge is dead once its test is resolved to the exit).
    let mut copies: Vec<(HashMap<Block, Block>, HashMap<Value, Value>)> = Vec::with_capacity(n + 1);
    for _ in 0..=n {
        let mut bmap = HashMap::new();
        let mut vmap = HashMap::new();
        for &b in &plan.region {
            let nb = func.create_block();
            bmap.insert(b, nb);
            let params = func.blocks[b.index()].params.clone();
            for p in params {
                let ty = func.value_type(p);
                let np = func.append_block_param(nb, ty);
                vmap.insert(p, np);
            }
        }
        copies.push((bmap, vmap));
    }
    for k in 0..=n {
        let next_h = if k < n { copies[k + 1].0[&h] } else { h };
        for &b in &plan.region {
            let nb = copies[k].0[&b];
            let insts = func.blocks[b.index()].insts.clone();
            for inst in insts {
                let mut data = func.inst(inst).clone();
                {
                    let (bmap, vmap) = &copies[k];
                    data.map_args(|v| *vmap.get(&v).unwrap_or(&v));
                    for c in data.successors_mut() {
                        c.block = if c.block == h {
                            next_h
                        } else {
                            *bmap.get(&c.block).unwrap_or(&c.block)
                        };
                    }
                }
                let ni = func.make_inst(data);
                let olds = func.inst_results[inst.index()].clone();
                let news = func.inst_results[ni.index()].clone();
                for (o, nv) in olds.into_iter().zip(news) {
                    copies[k].1.insert(o, nv);
                }
                func.blocks[nb.index()].insts.push(ni);
            }
        }
        // Resolve the copy's test: into the body for the first n, to the exit for the last.
        let nh = copies[k].0[&h];
        let t = *func.blocks[nh.index()].insts.last().expect("header terminator");
        let target = if k == n {
            plan.exit
        } else if plan.body == h {
            next_h
        } else {
            copies[k].0[&plan.body]
        };
        let InstData::Brif { then, else_, .. } = func.inst(t).clone() else {
            unreachable!("the header ends in its test")
        };
        let dest = if then.block == target { then } else { else_ };
        func.insts[t.index()] = InstData::Jump { dest };
    }
    // The entering edge enters copy 0.
    let h0 = copies[0].0[&h];
    for &p in &cfg.preds[h.index()] {
        if cfg.dominates(h, p) {
            continue;
        }
        if let Some(t) = func.terminator(p) {
            for c in func.insts[t.index()].successors_mut() {
                if c.block == h {
                    c.block = h0;
                }
            }
        }
    }
    // Code after the exit read the header's values: now the last copy's.
    let last = &copies[n].1;
    let header_vals: HashMap<Value, Value> = {
        let mut m = HashMap::new();
        for &p in &func.blocks[h.index()].params {
            m.insert(p, last[&p]);
        }
        for &i in &func.blocks[h.index()].insts {
            for &r in &func.inst_results[i.index()] {
                m.insert(r, last[&r]);
            }
        }
        m
    };
    let after: Vec<Block> = cfg
        .rpo
        .iter()
        .copied()
        .filter(|&b| cfg.dominates(plan.exit, b))
        .collect();
    for b in after {
        let insts = func.blocks[b.index()].insts.clone();
        for i in insts {
            func.insts[i.index()].map_args(|v| *header_vals.get(&v).unwrap_or(&v));
        }
    }
    // Layout: the copies where the loop was.
    let at = func.layout.iter().position(|&b| b == h).unwrap_or(func.layout.len());
    let mut new_blocks = Vec::new();
    for (bmap, _) in &copies {
        for b in &plan.region {
            new_blocks.push(bmap[b]);
        }
    }
    func.layout.splice(at..at, new_blocks);
}
