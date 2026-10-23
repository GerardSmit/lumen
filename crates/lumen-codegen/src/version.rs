//! Loop versioning: an innermost loop gets a fast copy that assumes its loop-invariant guards
//! hold, entered through a dispatcher that checks them once.
//!
//! # The fast copy
//!
//! The analysis looks at the loop with its rarely executed blocks set aside: blocks marked
//! cold, those only cold blocks reach, and those that only lead to cold blocks. Memory the
//! remaining (hot) blocks never write is *protected* (a store of a known class writes only its
//! bytes off its own address value, so a load of the same class from that value at a disjoint
//! offset stays protected). Every cold block is *cut*: the fast copy branches to the original
//! block instead (a slow path calling into the runtime may write anything), finishing the
//! iteration in the original loop. A load with an invariant address of protected memory is
//! then invariant in the fast copy, and so is everything pure computed from invariant values.
//!
//! The header's parameters are assumed invariant (each entering the fast copy holds the value
//! it had on entry) until shown otherwise, optimistically: a `brif` on an invariant condition
//! with one side staying in the hot part of the loop and the other leaving it (an exit, a cold
//! or cut block) is folded to the hot side, and a parameter whose latch arguments are not
//! itself under those folds is not invariant. That covers the front end's lazily filled caches
//! (`if cache == 0 { cache = fill() }` on a cold path) as well as guards on loaded values.
//!
//! # The dispatcher
//!
//! A new block `P'` takes the header's parameters, computes the invariant instructions the fast
//! copy uses (in loop order, so each load follows the guards that protected it in the loop) and
//! checks every folded condition; any failure jumps to the original header, otherwise it enters
//! the fast copy's header. The original loop's entries and latches both go to `P'`: an
//! iteration finished in the original loop (after a cut block, or because a guard failed) tries
//! the fast copy again on the next one, up to a limit per entry.
//!
//! A hoisted load must not fault where the loop would not have run it: it is `notrap`, or its
//! block runs on the first iteration however the fast copy is left — it dominates every latch
//! and every block that exits or is cut (folded edges are gone) — or the dispatcher checks the
//! branches that lead to it from the header (the exit test, say), computing their variant
//! conditions from the header's parameters. Nothing that may trap or not return precedes it
//! in its block or on the way (the rule of `licm`).
//!
//! Values of the original loop reaching code both copies now reach (the exits, and the blocks
//! the cut edges enter) get block parameters merging the two definitions.

use crate::cfg::Cfg;
use crate::ir::*;
use crate::licm::{find_loops, may_not_continue, Loop};
use lumen_common::fasthash::FastMap;

/// Loops above this many instructions are not copied.
const MAX_LOOP_INSTS: usize = 2500;

/// The original loop's count of misses (an iteration it finished: 1; a failed dispatch:
/// [`FAIL_WEIGHT`]), halved at each successful dispatch so that a cut taken now and then (the
/// safepoint, a cache refresh) never adds up. Above this it stops returning to the dispatcher.
const MAX_MISSES: i64 = 16;

const FAIL_WEIGHT: i64 = 4;

/// Version at most this many loops per function.
const MAX_LOOPS: usize = 16;

struct Plan {
    /// The fast copy's blocks (reachable once the folds apply, not cut), in loop order.
    order: Vec<Block>,
    fast: Vec<bool>,
    /// Values computed once in the dispatcher.
    inv: Vec<bool>,
    /// Instructions copied to the dispatcher.
    hoist: Vec<bool>,
    /// The successor slot a folded terminator keeps, by block.
    fold: Vec<Option<usize>>,
    /// The successor slot the dispatcher requires of a block's variant `brif` (the path a
    /// hoisted load needs the first iteration to take), by block.
    guard: Vec<Option<usize>>,
    /// Variant pure instructions the dispatcher computes for those conditions.
    replay: Vec<bool>,
    /// For an invariant block parameter of a non-header block: the value it always equals.
    rep: FastMap<Value, Value>,
}

/// Version the innermost loops worth it; whether the function changed.
pub fn version_loops(func: &mut Function) -> bool {
    if func.layout.is_empty() {
        return false;
    }
    func.resolve_aliases();
    let mut done: Vec<bool> = Vec::new();
    let mut changed = false;
    for _ in 0..MAX_LOOPS {
        let cfg = Cfg::new(func);
        let loops = find_loops(&cfg);
        let entry = func.entry();
        let mut header_of = vec![u32::MAX; func.blocks.len()];
        for (k, lp) in loops.iter().enumerate() {
            header_of[lp.header.index()] = k as u32;
        }
        let mut applied = false;
        for lp in &loops {
            let h = lp.header;
            if h == entry || done.get(h.index()).copied().unwrap_or(false) {
                continue;
            }
            // Innermost: no other loop's header inside.
            if lp.blocks[1..].iter().any(|b| header_of[b.index()] != u32::MAX) {
                continue;
            }
            let size: usize = lp
                .blocks
                .iter()
                .map(|b| func.blocks[b.index()].insts.len())
                .sum();
            if size > MAX_LOOP_INSTS {
                continue;
            }
            let Some(plan) = plan(func, &cfg, lp) else {
                if done.len() <= h.index() {
                    done.resize(h.index() + 1, false);
                }
                done[h.index()] = true;
                continue;
            };
            let first_new = func.blocks.len();
            apply(func, &cfg, lp, &plan);
            done.resize(func.blocks.len(), false);
            done[h.index()] = true;
            for d in &mut done[first_new..] {
                *d = true;
            }
            applied = true;
            changed = true;
            break;
        }
        if !applied {
            break;
        }
    }
    changed
}

/// The nearest common dominator of two positions (`idom` points to smaller positions).
fn meet(idom: &[usize], mut x: usize, mut y: usize) -> usize {
    while x != y {
        while x > y {
            x = idom[x];
        }
        while y > x {
            y = idom[y];
        }
    }
    x
}

fn brif_cond(func: &Function, b: Block) -> Value {
    let t = func.terminator(b).expect("a loop block ends in a branch");
    let InstData::Brif { cond, .. } = *func.inst(t) else {
        unreachable!("only a brif is folded or guarded")
    };
    func.resolve(cond)
}

/// Whether the dispatcher can compute `v` from the header's parameters: pure instructions
/// (collected in `replay`) over them, invariant values (`roots`) and values from outside.
fn collect(
    func: &Function,
    in_loop_val: &dyn Fn(Value) -> bool,
    inv: &[bool],
    h: Block,
    v: Value,
    replay: &mut Vec<Inst>,
    roots: &mut Vec<Value>,
) -> bool {
    let v = func.resolve(v);
    if !in_loop_val(v) {
        return true;
    }
    if inv[v.index()] {
        roots.push(v);
        return true;
    }
    match func.values[v.index()].def {
        ValueDef::Param(b, _) => b == h,
        ValueDef::Result(i, _) => {
            if replay.contains(&i) {
                return true;
            }
            let d = func.inst(i);
            if d.is_terminator() || matches!(d, InstData::Load { .. }) || !d.is_pure() {
                return false;
            }
            replay.push(i);
            let mut ok = true;
            d.for_each_arg(|a| ok &= collect(func, in_loop_val, inv, h, a, replay, roots));
            ok
        }
        ValueDef::Alias(_) => false,
    }
}

/// Require, in `guard`, the branches from the header to the block at `k`; false when it is
/// not reached by a chain of blocks each the only one reaching the next, with nothing that
/// may not continue, and variant conditions the dispatcher can compute, or when `guard`
/// disagrees.
#[allow(clippy::too_many_arguments)]
fn peel(
    func: &Function,
    order: &[Block],
    fpred: &[Vec<(usize, usize)>],
    fold: &[Option<usize>],
    inv: &[bool],
    in_loop_val: &dyn Fn(Value) -> bool,
    h: Block,
    k: usize,
    guard: &mut [Option<usize>],
) -> bool {
    let mut need: Vec<(Block, usize)> = Vec::new();
    let mut x = k;
    while x != 0 {
        let [(p, s)] = fpred[x][..] else {
            return false;
        };
        let d = order[p];
        if func.blocks[d.index()]
            .insts
            .iter()
            .any(|&i| may_not_continue(func.inst(i)))
        {
            return false;
        }
        let t = func.terminator(d).expect("a loop block ends in a branch");
        match func.inst(t) {
            InstData::Jump { .. } => {}
            InstData::Brif { cond, .. } => {
                // An invariant condition left unfolded has both sides in the fast copy, and
                // requiring one picks the path of every iteration: only the `then` side, which
                // the front end emits for the expected case.
                if fold[d.index()].is_none() {
                    if s != 0 && inv[func.resolve(*cond).index()] {
                        return false;
                    }
                    need.push((d, s));
                }
            }
            _ => return false,
        }
        x = p;
    }
    for &(d, s) in &need {
        if guard[d.index()].is_some_and(|g| g != s) {
            return false;
        }
        let (mut ri, mut rv) = (Vec::new(), Vec::new());
        if !collect(func, in_loop_val, inv, h, brif_cond(func, d), &mut ri, &mut rv) {
            return false;
        }
    }
    for (d, s) in need {
        guard[d.index()] = Some(s);
    }
    true
}

fn def_block(func: &Function, inst_block: &[Block], v: Value) -> Option<Block> {
    match func.values[v.index()].def {
        ValueDef::Param(b, _) => Some(b),
        ValueDef::Result(i, _) => inst_block
            .get(i.index())
            .copied()
            .filter(|b| b.0 != u32::MAX),
        ValueDef::Alias(_) => None,
    }
}

fn plan(func: &Function, cfg: &Cfg, lp: &Loop) -> Option<Plan> {
    let n = func.blocks.len();
    let h = lp.header;
    let mut in_loop = vec![false; n];
    for &b in &lp.blocks {
        in_loop[b.index()] = true;
    }
    let mut inst_block = vec![Block(u32::MAX); func.insts.len()];
    for &b in &lp.blocks {
        for &i in &func.blocks[b.index()].insts {
            inst_block[i.index()] = b;
        }
    }
    let in_loop_val = |v: Value| def_block(func, &inst_block, v).is_some_and(|b| in_loop[b.index()]);

    // Rarely executed: marked cold, or only reached through such a block.
    let mut cold = vec![false; n];
    for &b in &lp.blocks {
        let up = cfg.idom[b.index()].filter(|&d| d != b && in_loop[d.index()]);
        cold[b.index()] = func.blocks[b.index()].cold || up.is_some_and(|d| cold[d.index()]);
    }
    // And those that only lead to such blocks (a guard's miss edge through a jump).
    loop {
        let mut more = false;
        for &b in &lp.blocks {
            if cold[b.index()] || b == h {
                continue;
            }
            let t = func.terminator(b).expect("a loop block ends in a branch");
            let succ = func.inst(t).successors();
            if !succ.is_empty()
                && succ
                    .iter()
                    .all(|c| in_loop[c.block.index()] && cold[c.block.index()])
            {
                cold[b.index()] = true;
                more = true;
            }
        }
        if !more {
            break;
        }
    }
    if cold[h.index()] {
        return None;
    }
    // What the hot blocks write: calls by class; stores also by address, so a load of the
    // same class at a disjoint offset from the same base stays protected.
    let mut hot_writes = AliasSet::NONE;
    let mut hot_stores: Vec<(AliasClass, Value, i64, i64)> = Vec::new();
    for &b in &lp.blocks {
        if !cold[b.index()] {
            for &i in &func.blocks[b.index()].insts {
                match *func.inst(i) {
                    InstData::Store {
                        kind, addr, offset, ..
                    } => {
                        let c = func.mem_flags(i).class;
                        if c.is_known() {
                            let lo = offset as i64;
                            hot_stores.push((c, func.resolve(addr), lo, lo + kind.bytes() as i64));
                        } else {
                            hot_writes = hot_writes.union(func.writes(i));
                        }
                    }
                    _ => hot_writes = hot_writes.union(func.writes(i)),
                }
            }
        }
    }
    // The fast copy leaves rarely executed blocks to the original loop.
    let cut = &cold;
    let protected = |i: Inst| {
        let c = func.mem_flags(i).class;
        let InstData::Load { kind, addr, offset } = *func.inst(i) else {
            return false;
        };
        let (a, lo) = (func.resolve(addr), offset as i64);
        let hi = lo + kind.bytes() as i64;
        c.is_known()
            && !hot_writes.may_touch(c)
            && hot_stores
                .iter()
                .all(|&(sc, sa, slo, shi)| !sc.may_alias(c) || (sa == a && (shi <= lo || hi <= slo)))
    };
    let good = |b: Block| in_loop[b.index()] && !cut[b.index()];

    let hp = func.blocks[h.index()].params.clone();
    let mut inv_h = vec![true; hp.len()];
    let mut blocked = vec![false; func.insts.len()];
    let nv = func.values.len();
    loop {
        let mut reach = vec![false; n];
        reach[h.index()] = true;
        let mut fold: Vec<Option<usize>> = vec![None; n];
        let mut inv = vec![false; nv];
        let mut rep: FastMap<Value, Value> = FastMap::default();
        let is_inv = |inv: &[bool], v: Value| !in_loop_val(v) || inv[v.index()];
        let mut order = Vec::new();
        for &b in &lp.blocks {
            if !reach[b.index()] || cut[b.index()] {
                continue;
            }
            order.push(b);
            let params = &func.blocks[b.index()].params;
            if b == h {
                for (k, &p) in params.iter().enumerate() {
                    inv[p.index()] = inv_h[k];
                }
            } else {
                for (k, &p) in params.iter().enumerate() {
                    let mut same: Option<Value> = None;
                    let mut ok = true;
                    for &pr in &cfg.preds[b.index()] {
                        if !reach[pr.index()] || cut[pr.index()] || !in_loop[pr.index()] {
                            continue;
                        }
                        let t = func.terminator(pr).expect("a predecessor ends in a branch");
                        for (slot, c) in func.inst(t).successors().iter().enumerate() {
                            if c.block != b || fold[pr.index()].is_some_and(|s| s != slot) {
                                continue;
                            }
                            let a = func.resolve(c.args[k]);
                            let a = rep.get(&a).copied().unwrap_or(a);
                            if a == p {
                                continue;
                            }
                            if !is_inv(&inv, a) || same.is_some_and(|s| s != a) {
                                ok = false;
                            }
                            same = Some(a);
                        }
                    }
                    if let (true, Some(s)) = (ok, same) {
                        inv[p.index()] = true;
                        rep.insert(p, s);
                    }
                }
            }
            for &i in &func.blocks[b.index()].insts {
                let data = func.inst(i);
                if data.is_terminator() {
                    if let InstData::Brif { cond, then, else_ } = data {
                        if is_inv(&inv, func.resolve(*cond)) {
                            match (good(then.block), good(else_.block)) {
                                (true, false) => fold[b.index()] = Some(0),
                                (false, true) => fold[b.index()] = Some(1),
                                _ => {}
                            }
                        }
                    }
                    for (slot, c) in data.successors().iter().enumerate() {
                        if fold[b.index()].is_some_and(|s| s != slot) {
                            continue;
                        }
                        if in_loop[c.block.index()] && !cut[c.block.index()] {
                            reach[c.block.index()] = true;
                        }
                    }
                    continue;
                }
                let results = func.results(i);
                if results.is_empty() {
                    continue;
                }
                let mut args_inv = true;
                data.for_each_arg(|a| args_inv &= is_inv(&inv, func.resolve(a)));
                let movable = args_inv
                    && match data {
                        InstData::Load { .. } => {
                            let f = func.mem_flags(i);
                            !blocked[i.index()] && (f.readonly || protected(i))
                        }
                        d => d.is_pure(),
                    };
                if movable {
                    for &r in results {
                        inv[r.index()] = true;
                    }
                }
            }
        }

        let mut changed = false;
        // A header parameter stays invariant when every latch passes it back unchanged.
        let mut latches = Vec::new();
        for &b in &order {
            let t = func.terminator(b).expect("a loop block ends in a branch");
            for (slot, c) in func.inst(t).successors().iter().enumerate() {
                if c.block != h || fold[b.index()].is_some_and(|s| s != slot) {
                    continue;
                }
                if !latches.contains(&b) {
                    latches.push(b);
                }
                for (k, &a) in c.args.iter().enumerate() {
                    let a = func.resolve(a);
                    let a = rep.get(&a).copied().unwrap_or(a);
                    if inv_h[k] && a != hp[k] {
                        inv_h[k] = false;
                        changed = true;
                    }
                }
            }
        }
        if latches.is_empty() {
            return None;
        }

        // Dominators of the folded fast copy (a DAG once the latch edges are left out, in
        // loop order); a load that may fault must run on every iteration.
        let mut pos = vec![usize::MAX; n];
        for (k, &b) in order.iter().enumerate() {
            pos[b.index()] = k;
        }
        let mut idom = vec![usize::MAX; order.len()];
        let mut fpred: Vec<Vec<(usize, usize)>> = vec![Vec::new(); order.len()];
        idom[0] = 0;
        for (k, &b) in order.iter().enumerate().skip(1) {
            let mut d = usize::MAX;
            for &pr in &cfg.preds[b.index()] {
                let pk = pos[pr.index()];
                if pk == usize::MAX || pk >= k {
                    continue;
                }
                let t = func.terminator(pr).expect("a predecessor ends in a branch");
                let before = fpred[k].len();
                for (s, c) in func.inst(t).successors().iter().enumerate() {
                    if c.block == b && fold[pr.index()].is_none_or(|f| f == s) {
                        fpred[k].push((pk, s));
                    }
                }
                if fpred[k].len() == before {
                    continue;
                }
                d = if d == usize::MAX {
                    pk
                } else {
                    meet(&idom, d, pk)
                };
            }
            idom[k] = if d == usize::MAX { 0 } else { d };
        }
        // As in `licm`: a load that may fault must run on the first iteration whatever way
        // the loop is left, so its block dominates every latch and every block leaving the
        // fast copy (an exit or a cut edge; folded edges are gone), and nothing that may not
        // continue precedes it.
        let mut anchor: Option<usize> = None;
        for (k, &b) in order.iter().enumerate() {
            let t = func.terminator(b).expect("a loop block ends in a branch");
            let leaves = func.blocks[b.index()]
                .insts
                .iter()
                .any(|&i| may_not_continue(func.inst(i)))
                || func.inst(t).successors().iter().enumerate().any(|(s, c)| {
                    fold[b.index()].is_none_or(|f| f == s)
                        && (c.block == h || pos[c.block.index()] == usize::MAX)
                });
            if leaves {
                anchor = Some(anchor.map_or(k, |a| meet(&idom, a, k)));
            }
        }
        let mut spine = vec![false; order.len()];
        let mut x = anchor.unwrap_or(0);
        loop {
            spine[x] = true;
            if x == 0 {
                break;
            }
            x = idom[x];
        }
        // A load off that spine still runs on the first iteration when the dispatcher checks
        // the branches leading to it (the header's exit test, say): each block on the way is
        // the only one reaching the next, and none may not continue.
        let mut guard: Vec<Option<usize>> = vec![None; n];
        let mut peeled: Vec<Option<bool>> = vec![None; order.len()];
        for (k, &b) in order.iter().enumerate() {
            let mut clear = true;
            for &i in &func.blocks[b.index()].insts {
                let data = func.inst(i);
                if may_not_continue(data) {
                    clear = false;
                    continue;
                }
                if !(matches!(data, InstData::Load { .. })
                    && !func.mem_flags(i).notrap
                    && func.results(i).iter().any(|r| inv[r.index()])
                    && !blocked[i.index()])
                {
                    continue;
                }
                let ok = clear
                    && (spine[k]
                        || *peeled[k].get_or_insert_with(|| {
                            peel(func, &order, &fpred, &fold, &inv, &in_loop_val, h, k, &mut guard)
                        }));
                if !ok {
                    blocked[i.index()] = true;
                    changed = true;
                }
            }
        }
        if changed {
            continue;
        }

        // What the fast copy needs from the dispatcher: folded conditions, invariant operands
        // of the rest, and invariant values used after the loop.
        let mut need = vec![false; nv];
        let mut work: Vec<Value> = Vec::new();
        let mut fast = vec![false; n];
        for &b in &order {
            fast[b.index()] = true;
        }
        let root = |v: Value, need: &mut Vec<bool>, work: &mut Vec<Value>| {
            let v = func.resolve(v);
            if in_loop_val(v) && inv[v.index()] && !need[v.index()] {
                need[v.index()] = true;
                work.push(v);
            }
        };
        for &b in &order {
            for &i in &func.blocks[b.index()].insts {
                let data = func.inst(i);
                if let (Some(_), InstData::Brif { cond, .. }) = (fold[b.index()], data) {
                    root(*cond, &mut need, &mut work);
                    continue;
                }
                let hoisted = !data.is_terminator()
                    && !func.results(i).is_empty()
                    && func.results(i).iter().all(|r| inv[r.index()]);
                if !hoisted {
                    data.for_each_arg(|a| root(a, &mut need, &mut work));
                }
            }
        }
        for &b in &func.layout {
            if in_loop[b.index()] {
                continue;
            }
            for &i in &func.blocks[b.index()].insts {
                func.inst(i).for_each_arg(|a| root(a, &mut need, &mut work));
            }
        }
        let mut replay = vec![false; func.insts.len()];
        for &b in &order {
            if guard[b.index()].is_some() {
                let (mut ri, mut rv) = (Vec::new(), Vec::new());
                let cond = brif_cond(func, b);
                collect(func, &in_loop_val, &inv, h, cond, &mut ri, &mut rv);
                for i in ri {
                    replay[i.index()] = true;
                }
                for v in rv {
                    root(v, &mut need, &mut work);
                }
            }
        }
        let mut hoist = vec![false; func.insts.len()];
        let (mut loads, mut pure, mut folds) = (0usize, 0usize, 0usize);
        while let Some(v) = work.pop() {
            match func.values[v.index()].def {
                ValueDef::Result(i, _) => {
                    if !hoist[i.index()] {
                        hoist[i.index()] = true;
                        if matches!(func.inst(i), InstData::Load { .. }) {
                            loads += 1;
                        } else {
                            pure += 1;
                        }
                        func.inst(i).for_each_arg(|a| root(a, &mut need, &mut work));
                    }
                }
                ValueDef::Param(..) => {
                    if let Some(&r) = rep.get(&v) {
                        root(r, &mut need, &mut work);
                    }
                }
                ValueDef::Alias(_) => {}
            }
        }
        for &b in &order {
            folds += fold[b.index()].is_some() as usize;
        }
        // Constants alone are left to the backends' rematerialization.
        let consts = order
            .iter()
            .flat_map(|b| &func.blocks[b.index()].insts)
            .filter(|i| {
                hoist[i.index()]
                    && matches!(
                        func.inst(**i),
                        InstData::Iconst { .. } | InstData::F32const { .. } | InstData::F64const { .. }
                    )
            })
            .count();
        let pure = pure - consts;
        if folds + loads == 0 || 2 * folds + loads + pure < 4 {
            return None;
        }
        return Some(Plan {
            order,
            fast,
            inv,
            hoist,
            fold,
            guard,
            replay,
            rep,
        });
    }
}

/// Build the dispatcher and the fast copy of `lp` per `plan`.
fn apply(func: &mut Function, cfg: &Cfg, lp: &Loop, plan: &Plan) {
    let h = lp.header;
    let hp = func.blocks[h.index()].params.clone();
    // Iterations the original loop finished since the loop was entered.
    let hc = func.append_block_param(h, Type::I32);
    let n_old_values = func.values.len();
    let ph = func.create_block();
    let pp: Vec<Value> = hp
        .iter()
        .map(|&p| {
            let ty = func.value_type(p);
            func.append_block_param(ph, ty)
        })
        .collect();
    let pc = func.append_block_param(ph, Type::I32);
    let fail = func.create_block();
    let twin: FastMap<Block, Block> = plan
        .order
        .iter()
        .map(|&b| {
            let t = func.create_block();
            func.blocks[t.index()].cold = func.blocks[b.index()].cold;
            (b, t)
        })
        .collect();
    let fh = twin[&h];

    // The dispatcher: invariant instructions in loop order, each folded condition checked
    // after its block's instructions.
    let mut pmap: FastMap<Value, Value> = FastMap::default();
    for (k, &p) in hp.iter().enumerate() {
        pmap.insert(p, pp[k]);
    }
    fn get(pmap: &FastMap<Value, Value>, rep: &FastMap<Value, Value>, v: Value) -> Value {
        let mut v = v;
        loop {
            if let Some(&x) = pmap.get(&v) {
                return x;
            }
            match rep.get(&v) {
                Some(&r) => v = r,
                None => return v,
            }
        }
    }
    let mut cur = ph;
    let mut chain = vec![ph];
    for &b in &plan.order {
        let insts = func.blocks[b.index()].insts.clone();
        for &i in &insts {
            if !plan.hoist[i.index()] && !plan.replay[i.index()] {
                continue;
            }
            let mut data = func.inst(i).clone();
            data.map_args(|a| get(&pmap, &plan.rep, func.resolve(a)));
            let ni = func.make_inst(data);
            func.copy_inst_meta(i, ni);
            func.blocks[cur.index()].insts.push(ni);
            let (old, new) = (func.results(i).to_vec(), func.results(ni).to_vec());
            for (o, nw) in old.into_iter().zip(new) {
                pmap.insert(o, nw);
            }
        }
        if let Some(slot) = plan.fold[b.index()].or(plan.guard[b.index()]) {
            let cond = brif_cond(func, b);
            let c = get(&pmap, &plan.rep, cond);
            let next = func.create_block();
            let (then, else_) = if slot == 0 { (next, fail) } else { (fail, next) };
            let br = func.make_inst(InstData::Brif {
                cond: c,
                then: BlockCall {
                    block: then,
                    args: vec![],
                },
                else_: BlockCall {
                    block: else_,
                    args: vec![],
                },
            });
            func.blocks[cur.index()].insts.push(br);
            cur = next;
            chain.push(next);
        }
    }
    let half = func.make_inst(InstData::Iconst {
        ty: Type::I32,
        imm: 1,
    });
    let half_v = func.results(half)[0];
    let halved = func.make_inst(InstData::Binary {
        op: BinaryOp::Ushr,
        args: [pc, half_v],
    });
    let pc_half = func.results(halved)[0];
    func.blocks[cur.index()].insts.extend([half, halved]);
    let enter = func.make_inst(InstData::Jump {
        dest: BlockCall {
            block: fh,
            args: pp.iter().copied().chain([pc_half]).collect(),
        },
    });
    func.blocks[cur.index()].insts.push(enter);
    let weight = func.make_inst(InstData::Iconst {
        ty: Type::I32,
        imm: FAIL_WEIGHT,
    });
    let weight_v = func.results(weight)[0];
    let failed = func.make_inst(InstData::Binary {
        op: BinaryOp::Iadd,
        args: [pc, weight_v],
    });
    let pc2 = func.results(failed)[0];
    let back = func.make_inst(InstData::Jump {
        dest: BlockCall {
            block: h,
            args: pp.iter().copied().chain([pc2]).collect(),
        },
    });
    func.blocks[fail.index()]
        .insts
        .extend([weight, failed, back]);
    func.blocks[fail.index()].cold = true;

    // The fast copy.
    let mut vmap: FastMap<Value, Value> = FastMap::default();
    let map = |vmap: &FastMap<Value, Value>, v: Value| vmap.get(&v).copied().unwrap_or(v);
    for &b in &plan.order {
        let t = twin[&b];
        // (A parameter equal to an earlier value takes that value's copy.)
        let params = func.blocks[b.index()].params.clone();
        for &p in &params {
            let ty = func.value_type(p);
            let np = func.append_block_param(t, ty);
            let v = if p == hc {
                np
            } else {
                match (b == h, plan.inv[p.index()], plan.rep.get(&p)) {
                    (true, true, _) => pmap[&p],
                    (false, true, Some(&r)) => map(&vmap, r),
                    _ => np,
                }
            };
            vmap.insert(p, v);
        }
        let insts = func.blocks[b.index()].insts.clone();
        for &i in &insts {
            if plan.hoist[i.index()] {
                for &r in func.results(i).to_vec().iter() {
                    vmap.insert(r, pmap[&r]);
                }
                continue;
            }
            let mut data = func.inst(i).clone();
            if let (Some(slot), true) = (plan.fold[b.index()], data.is_terminator()) {
                let kept = data.successors()[slot].clone();
                data = InstData::Jump { dest: kept };
            }
            data.map_args(|a| map(&vmap, func.resolve(a)));
            let fc = vmap[&hc];
            data.for_each_successor_mut(|c| {
                if c.block == h {
                    c.args.push(fc);
                }
                if plan.fast[c.block.index()] {
                    c.block = twin[&c.block];
                }
            });
            let ni = func.make_inst(data);
            func.copy_inst_meta(i, ni);
            func.blocks[t.index()].insts.push(ni);
            let (old, new) = (func.results(i).to_vec(), func.results(ni).to_vec());
            for (o, nw) in old.into_iter().zip(new) {
                vmap.insert(o, nw);
            }
        }
    }

    // Entries of the original loop go to the dispatcher with a zero count. Its latches count
    // the iteration and return to the dispatcher until too many have been missed, then stay
    // in the original loop: it is a complete loop on its own.
    let pre = func.create_block();
    let prep: Vec<Value> = hp
        .iter()
        .map(|&p| {
            let ty = func.value_type(p);
            func.append_block_param(pre, ty)
        })
        .collect();
    let zero = func.make_inst(InstData::Iconst {
        ty: Type::I32,
        imm: 0,
    });
    let zero_v = func.results(zero)[0];
    let go = func.make_inst(InstData::Jump {
        dest: BlockCall {
            block: ph,
            args: prep.iter().copied().chain([zero_v]).collect(),
        },
    });
    func.blocks[pre.index()].insts.extend([zero, go]);

    let lt = func.create_block();
    let lp_params: Vec<Value> = hp
        .iter()
        .map(|&p| {
            let ty = func.value_type(p);
            func.append_block_param(lt, ty)
        })
        .collect();
    let one = func.make_inst(InstData::Iconst {
        ty: Type::I32,
        imm: 1,
    });
    let one_v = func.results(one)[0];
    let add = func.make_inst(InstData::Binary {
        op: BinaryOp::Iadd,
        args: [hc, one_v],
    });
    let cn = func.results(add)[0];
    let limit = func.make_inst(InstData::Iconst {
        ty: Type::I32,
        imm: MAX_MISSES,
    });
    let limit_v = func.results(limit)[0];
    let over = func.make_inst(InstData::IntCmp {
        cc: IntCC::Sgt,
        args: [cn, limit_v],
    });
    let over_v = func.results(over)[0];
    let br = func.make_inst(InstData::Brif {
        cond: over_v,
        then: BlockCall {
            block: h,
            args: lp_params.iter().copied().chain([hc]).collect(),
        },
        else_: BlockCall {
            block: ph,
            args: lp_params.iter().copied().chain([cn]).collect(),
        },
    });
    func.blocks[lt.index()]
        .insts
        .extend([one, add, limit, over, br]);
    func.blocks[lt.index()].cold = true;

    let mut in_loop = vec![false; func.blocks.len()];
    for &b in &lp.blocks {
        in_loop[b.index()] = true;
    }
    for &p in &cfg.preds[h.index()] {
        let to = if in_loop[p.index()] { lt } else { pre };
        let t = func.terminator(p).expect("a predecessor ends in a branch");
        func.insts[t.index()].for_each_successor_mut(|c| {
            if c.block == h {
                c.block = to;
            }
        });
    }
    for &b in &lp.blocks {
        func.blocks[b.index()].cold = true;
    }
    let at = func
        .layout
        .iter()
        .position(|&b| b == h)
        .unwrap_or(func.layout.len());
    let mut placed = vec![pre];
    placed.extend(chain);
    placed.push(fail);
    placed.push(lt);
    placed.extend(plan.order.iter().map(|b| twin[b]));
    func.layout.splice(at..at, placed);

    repair(func, lp, &vmap, n_old_values);
}

/// Give every value of the original loop a definition wherever both copies now reach a use
/// of it: block parameters where the original value and its copy (`vmap`) meet.
fn repair(func: &mut Function, lp: &Loop, vmap: &FastMap<Value, Value>, n_old: usize) {
    let cfg = Cfg::new(func);
    let n = func.blocks.len();
    let mut inst_block = vec![Block(u32::MAX); func.insts.len()];
    for &b in &func.layout {
        for &i in &func.blocks[b.index()].insts {
            inst_block[i.index()] = b;
        }
    }
    let mut in_loop = vec![false; n];
    for &b in &lp.blocks {
        in_loop[b.index()] = true;
    }
    // Uses no longer dominated by their definition, per value.
    let mut broken: FastMap<Value, Vec<Block>> = FastMap::default();
    for &b in &func.layout {
        if !cfg.is_reachable(b) {
            continue;
        }
        for &i in &func.blocks[b.index()].insts {
            func.inst(i).for_each_arg(|a| {
                if a.index() >= n_old {
                    return;
                }
                let Some(d) = def_block(func, &inst_block, a) else {
                    return;
                };
                if !in_loop[d.index()] || d == b || cfg.dominates(d, b) {
                    return;
                }
                let e = broken.entry(a).or_default();
                if !e.contains(&b) {
                    e.push(b);
                }
            });
        }
    }
    let mut values: Vec<Value> = broken.keys().copied().collect();
    values.sort_by_key(|v| v.0);
    for v in values {
        let uses = broken.remove(&v).unwrap_or_default();
        let d = def_block(func, &inst_block, v).expect("a placed definition");
        let twin = vmap.get(&v).copied().map(|t| func.resolve(t));
        let d2 = twin.and_then(|t| match func.values[t.index()].def {
            ValueDef::Param(b, _) => Some(b),
            ValueDef::Result(i, _) => {
                let b = inst_block.get(i.index()).copied().unwrap_or(Block(u32::MAX));
                (b.0 != u32::MAX).then_some(b)
            }
            ValueDef::Alias(_) => None,
        });
        let ty = func.value_type(v);
        // The blocks between the definitions and the uses.
        let mut seen: FastMap<Block, ()> = FastMap::default();
        let mut work: Vec<Block> = uses.clone();
        let mut region: Vec<Block> = Vec::new();
        while let Some(b) = work.pop() {
            if seen.insert(b, ()).is_some() {
                continue;
            }
            region.push(b);
            if b == d || Some(b) == d2 {
                continue;
            }
            for &p in &cfg.preds[b.index()] {
                work.push(p);
            }
        }
        // A parameter where two or more predecessors meet.
        let mut phi: FastMap<Block, Value> = FastMap::default();
        for &b in &region {
            if b != d && Some(b) != d2 && cfg.preds[b.index()].len() > 1 {
                let x = func.append_block_param(b, ty);
                phi.insert(b, x);
            }
        }
        let mut undef: Option<Value> = None;
        let at_end = |func: &mut Function, b: Block, undef: &mut Option<Value>| -> Value {
            let mut x = b;
            loop {
                if x == d {
                    return v;
                }
                if Some(x) == d2 {
                    return twin.expect("a copy where it is defined");
                }
                if let Some(&p) = phi.get(&x) {
                    return p;
                }
                match cfg.preds[x.index()].first() {
                    Some(&p) => x = p,
                    None => {
                        debug_assert!(false, "{v} has no definition reaching {b}");
                        return *undef.get_or_insert_with(|| zero_of(func, ty));
                    }
                }
            }
        };
        let phis: Vec<(Block, Value)> = phi.iter().map(|(&b, &x)| (b, x)).collect();
        for (b, _) in phis {
            for &p in &cfg.preds[b.index()] {
                let a = at_end(func, p, &mut undef);
                let t = func.terminator(p).expect("a predecessor ends in a branch");
                func.insts[t.index()].for_each_successor_mut(|c| {
                    if c.block == b {
                        c.args.push(a);
                    }
                });
            }
        }
        for u in uses {
            let r = if let Some(&p) = phi.get(&u) {
                p
            } else {
                let p = cfg.preds[u.index()][0];
                at_end(func, p, &mut undef)
            };
            let insts = func.blocks[u.index()].insts.clone();
            for i in insts {
                func.insts[i.index()].map_args(|a| if a == v { r } else { a });
            }
        }
        if let Some(z) = undef {
            let e = func.entry();
            let ValueDef::Result(zi, _) = func.values[z.index()].def else {
                unreachable!()
            };
            func.blocks[e.index()].insts.insert(0, zi);
        }
    }
}

fn zero_of(func: &mut Function, ty: Type) -> Value {
    let data = match ty {
        Type::F32 => InstData::F32const { bits: 0 },
        Type::F64 => InstData::F64const { bits: 0 },
        Type::V128 => InstData::Vzero,
        t => InstData::Iconst { ty: t, imm: 0 },
    };
    let i = func.make_inst(data);
    func.results(i)[0]
}
