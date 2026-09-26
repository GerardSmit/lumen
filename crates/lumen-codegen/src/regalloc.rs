//! Linear-scan register allocation over [`VCode`].
//!
//! Each virtual register gets one location for its whole lifetime — a physical register or a
//! stack slot — which is RyuJIT's single-def spill rule applied to SSA (docs/jit-notes/lsra.md):
//! a spilled value is stored once at its def and reloaded (or used from memory) at each use, and
//! no edge ever needs resolution moves except for block parameters, which the emitter handles as
//! parallel moves at the jump.
//!
//! Lifetimes are single ranges `[first position, last position]` over the linear block order,
//! extended across every block the value is live into or out of, so loop-carried values keep
//! their register through the whole loop. Registers overwritten by an instruction (clobbers, and
//! every fixed-register operand) block the ranges of values live across it; that is how values
//! live across calls end up in callee-saved registers or on the stack.
//!
//! Selection follows the RyuJIT order in simplified form: the hinted register (move source,
//! fixed operand) if free, then the allocation order (caller-saved first). When nothing is free,
//! the cheapest of the conflicting active values and the current value is spilled, where cost is
//! the sum of `8^loop_depth` over the value's references.

use crate::machinst::*;

/// A target's allocatable registers.
pub struct RegInfo {
    /// Allocation order per class (index by `RegClass as usize`): caller-saved first.
    pub order: [Vec<PReg>; 2],
    pub callee_saved: RegSet,
}

pub struct Allocation {
    pub locs: Vec<Loc>,
    pub num_slots: u32,
    /// Callee-saved registers the function must preserve.
    pub used_callee_saved: RegSet,
}

impl Allocation {
    pub fn loc(&self, v: VReg) -> Loc {
        self.locs[v.index()]
    }
}

fn class_index(c: RegClass) -> usize {
    match c {
        RegClass::Int => 0,
        RegClass::Float => 1,
    }
}

/// Instruction `i`'s use and def positions.
fn use_pos(i: usize) -> u32 {
    2 * i as u32
}
fn def_pos(i: usize) -> u32 {
    2 * i as u32 + 1
}

pub fn allocate<I: MachInst>(code: &VCode<I>, info: &RegInfo) -> Allocation {
    let nv = code.vreg_class.len();
    let nb = code.blocks.len();

    // ----- per-block gen/kill and liveness -----
    // Upward-exposed uses (gen) and defs (kill) per block, as lists. Only a value in some
    // block's gen can be live into (and so out of) any block: liveness is solved over those
    // "global" values alone (most values are block-local).
    let mut ops = Vec::new();
    let mut gen_l: Vec<Vec<u32>> = vec![Vec::new(); nb];
    let mut kill_l: Vec<Vec<u32>> = vec![Vec::new(); nb];
    let mut kill_at = vec![u32::MAX; nv];
    let mut gen_at = vec![u32::MAX; nv];
    for (bi, b) in code.blocks.iter().enumerate() {
        let bu = bi as u32;
        for &p in &b.params {
            if kill_at[p.index()] != bu {
                kill_at[p.index()] = bu;
                kill_l[bi].push(p.0);
            }
        }
        for i in b.start..b.end {
            ops.clear();
            code.insts[i].operands(&mut ops);
            for o in &ops {
                let v = o.vreg.index();
                if o.kind != OperandKind::Def && kill_at[v] != bu && gen_at[v] != bu {
                    gen_at[v] = bu;
                    gen_l[bi].push(o.vreg.0);
                }
            }
            for o in &ops {
                let v = o.vreg.index();
                if o.kind != OperandKind::Use && kill_at[v] != bu {
                    kill_at[v] = bu;
                    kill_l[bi].push(o.vreg.0);
                }
            }
        }
    }
    drop((kill_at, gen_at));
    // Liveness per global value by path exploration: from each block where it is upward-exposed,
    // walk predecessors (live out of each) until a block that defines it. Linear in the total
    // size of the live ranges, where dense per-block bitsets were blocks x globals per pass.
    let mut preds: Vec<Vec<u32>> = vec![Vec::new(); nb];
    for (bi, b) in code.blocks.iter().enumerate() {
        for &s in &b.succs {
            preds[s].push(bi as u32);
        }
    }
    let mut gen_of: Vec<Vec<u32>> = vec![Vec::new(); nv];
    let mut globals: Vec<u32> = Vec::new();
    for (bi, l) in gen_l.iter().enumerate() {
        for &v in l {
            if gen_of[v as usize].is_empty() {
                globals.push(v);
            }
            gen_of[v as usize].push(bi as u32);
        }
    }
    let mut kill_of: Vec<Vec<u32>> = vec![Vec::new(); nv];
    for (bi, l) in kill_l.iter().enumerate() {
        for &v in l {
            if !gen_of[v as usize].is_empty() {
                kill_of[v as usize].push(bi as u32);
            }
        }
    }
    drop((gen_l, kill_l));
    let mut live_in: Vec<Vec<u32>> = vec![Vec::new(); nb];
    let mut live_out: Vec<Vec<u32>> = vec![Vec::new(); nb];
    // `mark[b] == stamp` flags for the value being walked (one stamp per value).
    let (mut in_mark, mut out_mark) = (vec![0u32; nb], vec![0u32; nb]);
    let mut kill_mark = vec![0u32; nb];
    let mut work: Vec<u32> = Vec::new();
    for (gi, &v) in globals.iter().enumerate() {
        let stamp = gi as u32 + 1;
        for &b in &kill_of[v as usize] {
            kill_mark[b as usize] = stamp;
        }
        for &b in &gen_of[v as usize] {
            if in_mark[b as usize] != stamp {
                in_mark[b as usize] = stamp;
                live_in[b as usize].push(v);
                work.push(b);
            }
        }
        while let Some(b) = work.pop() {
            for &p in &preds[b as usize] {
                let pi = p as usize;
                if out_mark[pi] == stamp {
                    continue;
                }
                out_mark[pi] = stamp;
                live_out[pi].push(v);
                if kill_mark[pi] != stamp && in_mark[pi] != stamp {
                    in_mark[pi] = stamp;
                    live_in[pi].push(v);
                    work.push(p);
                }
            }
        }
    }
    drop((gen_of, kill_of, preds, in_mark, out_mark, kill_mark));

    // ----- intervals: per-block live segments (lifetime holes) -----
    let mut segs: Vec<Vec<(u32, u32)>> = vec![Vec::new(); nv];
    let mut weight = vec![0f64; nv];
    let mut hint: Vec<Option<PReg>> = vec![None; nv];
    // Values that want to share a register: copy source/destination, jump argument/parameter.
    let mut related: Vec<Vec<VReg>> = vec![Vec::new(); nv];
    // Clobber positions per physical register (sorted, since instructions are visited in order).
    let mut clobbers: Vec<Vec<u32>> = vec![Vec::new(); 64];
    // Per block: the (lo, hi) of each value touched in it, then flushed into `segs`.
    let mut lo = vec![u32::MAX; nv];
    let mut hi = vec![0u32; nv];
    let mut touched: Vec<VReg> = Vec::new();
    let touch = |v: VReg, p: u32, lo: &mut Vec<u32>, hi: &mut Vec<u32>, t: &mut Vec<VReg>| {
        if lo[v.index()] == u32::MAX {
            t.push(v);
        }
        lo[v.index()] = lo[v.index()].min(p);
        hi[v.index()] = hi[v.index()].max(p);
    };
    for (bi, b) in code.blocks.iter().enumerate() {
        let w = 8f64.powi(b.loop_depth.min(6) as i32);
        let entry = use_pos(b.start);
        let exit = def_pos(b.end - 1);
        for &p in &b.params {
            touch(p, entry, &mut lo, &mut hi, &mut touched);
            weight[p.index()] += w;
        }
        for (set, pos) in [(&live_in[bi], entry), (&live_out[bi], exit)] {
            for &v in set {
                touch(VReg(v), pos, &mut lo, &mut hi, &mut touched);
            }
        }
        for i in b.start..b.end {
            let inst = &code.insts[i];
            ops.clear();
            inst.operands(&mut ops);
            let mut clob = inst.clobbers();
            for o in &ops {
                let v = o.vreg;
                weight[v.index()] += w;
                match o.kind {
                    OperandKind::Use => {
                        let p = if o.late { def_pos(i) } else { use_pos(i) };
                        touch(v, p, &mut lo, &mut hi, &mut touched);
                    }
                    OperandKind::Def => touch(v, def_pos(i), &mut lo, &mut hi, &mut touched),
                    OperandKind::Mod => {
                        touch(v, use_pos(i), &mut lo, &mut hi, &mut touched);
                        touch(v, def_pos(i), &mut lo, &mut hi, &mut touched);
                    }
                }
                if let Constraint::Fixed(r) = o.constraint {
                    clob.insert(r);
                    hint[v.index()].get_or_insert(r);
                }
            }
            if let Some((dst, src)) = inst.as_move() {
                related[dst.index()].push(src);
                related[src.index()].push(dst);
            }
            if let Some((t, args)) = inst.jump_args() {
                for (&a, &p) in args.iter().zip(&code.blocks[t].params) {
                    related[a.index()].push(p);
                    related[p.index()].push(a);
                }
            }
            for r in clob.iter() {
                clobbers[r.bit() as usize].push(def_pos(i));
            }
        }
        for v in touched.drain(..) {
            let s = &mut segs[v.index()];
            let (l, h) = (lo[v.index()], hi[v.index()]);
            // Blocks are visited in order, so segments arrive sorted; merge adjacent ones.
            match s.last_mut() {
                Some(last) if last.1 + 1 >= l => last.1 = last.1.max(h),
                _ => s.push((l, h)),
            }
            lo[v.index()] = u32::MAX;
            hi[v.index()] = 0;
        }
    }

    let mut order: Vec<usize> = (0..nv).filter(|&v| !segs[v].is_empty()).collect();
    order.sort_by_key(|&v| (segs[v][0].0, v));

    // Whether register `r` is overwritten strictly inside `(s, e]` of one of `v`'s segments.
    let clobbered = |r: PReg, v: usize| -> bool {
        let list = &clobbers[r.bit() as usize];
        segs[v].iter().any(|&(s, e)| {
            let i = list.partition_point(|&p| p <= s);
            i < list.len() && list[i] <= e
        })
    };
    // Owners of register `r`'s segments that overlap value `v`.
    let overlapping = |assigned: &[Vec<(u32, u32, u32)>], r: PReg, v: usize| -> Vec<u32> {
        let list = &assigned[r.bit() as usize];
        let mut out = Vec::new();
        for &(s, e) in &segs[v] {
            let mut i = list.partition_point(|x| x.1 < s);
            while i < list.len() && list[i].0 <= e {
                if !out.contains(&list[i].2) {
                    out.push(list[i].2);
                }
                i += 1;
            }
        }
        out
    };

    // ----- allocation: greedy over values in start order; registers hold disjoint segments -----
    let mut locs = vec![Loc::None; nv];
    let mut num_slots = 0u32;
    let mut used_callee_saved = RegSet::EMPTY;
    // Segments assigned to each physical register, sorted and disjoint: (start, end, vreg).
    let mut assigned: Vec<Vec<(u32, u32, u32)>> = vec![Vec::new(); 64];
    for &v in &order {
        let class = code.class(VReg(v as u32));
        let ci = class_index(class);
        let usable = |r: PReg| r.class == class && info.order[ci].contains(&r) && !clobbered(r, v);
        let free = |assigned: &[Vec<(u32, u32, u32)>], r: PReg| {
            usable(r) && overlapping(assigned, r, v).is_empty()
        };
        let related_reg = related[v].iter().find_map(|s| match locs[s.index()] {
            Loc::Reg(r) if free(&assigned, r) => Some(r),
            _ => None,
        });
        let mut choice = related_reg
            .or(hint[v].filter(|&r| free(&assigned, r)))
            .or_else(|| info.order[ci].iter().copied().find(|&r| free(&assigned, r)));
        if choice.is_none() {
            // Evict the cheapest set of values occupying one register, if cheaper than `v`.
            let best = info.order[ci]
                .iter()
                .copied()
                .filter(|&r| usable(r))
                .map(|r| {
                    let owners = overlapping(&assigned, r, v);
                    let cost: f64 = owners.iter().map(|&o| weight[o as usize]).sum();
                    (r, owners, cost)
                })
                .min_by(|x, y| x.2.total_cmp(&y.2));
            if let Some((r, owners, cost)) = best {
                if cost < weight[v] {
                    assigned[r.bit() as usize].retain(|x| !owners.contains(&x.2));
                    for o in owners {
                        locs[o as usize] = Loc::Stack(num_slots);
                        num_slots += 1;
                    }
                    choice = Some(r);
                }
            }
        }
        match choice {
            Some(r) => {
                locs[v] = Loc::Reg(r);
                if info.callee_saved.contains(r) {
                    used_callee_saved.insert(r);
                }
                let list = &mut assigned[r.bit() as usize];
                for &(s, e) in &segs[v] {
                    let i = list.partition_point(|x| x.0 < s);
                    list.insert(i, (s, e, v as u32));
                }
            }
            None => {
                locs[v] = Loc::Stack(num_slots);
                num_slots += 1;
            }
        }
    }

    Allocation {
        locs,
        num_slots,
        used_callee_saved,
    }
}
