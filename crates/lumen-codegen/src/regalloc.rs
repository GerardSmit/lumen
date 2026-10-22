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

/// `pairs` grouped by key (`0..n`) into one allocation, each group in input order.
struct Groups<T> {
    offsets: Vec<u32>,
    items: Vec<T>,
}

impl<T: Copy + Default> Groups<T> {
    fn new(n: usize, pairs: &[(u32, T)]) -> Groups<T> {
        let mut offsets = vec![0u32; n + 1];
        for &(k, _) in pairs {
            offsets[k as usize + 1] += 1;
        }
        for i in 0..n {
            offsets[i + 1] += offsets[i];
        }
        let mut items = vec![T::default(); pairs.len()];
        let mut next = offsets.clone();
        for &(k, t) in pairs {
            items[next[k as usize] as usize] = t;
            next[k as usize] += 1;
        }
        Groups { offsets, items }
    }

    #[inline]
    fn of(&self, k: usize) -> &[T] {
        &self.items[self.offsets[k] as usize..self.offsets[k + 1] as usize]
    }
}

const NONE: u32 = u32::MAX;

pub fn allocate<I: MachInst>(code: &VCode<I>, info: &RegInfo) -> Allocation {
    let nv = code.vreg_class.len();
    let nb = code.blocks.len();

    // ----- per-block gen/kill and liveness -----
    // Upward-exposed uses (gen) and defs (kill) per block, as (block, value) pairs in block
    // order. Only a value in some block's gen can be live into (and so out of) any block:
    // liveness is solved over those "global" values alone (most values are block-local).
    // Every instruction's operands are collected once and read again below.
    let mut ops: Vec<Operand> = Vec::with_capacity(code.insts.len() * 3);
    let mut op_start: Vec<u32> = Vec::with_capacity(code.insts.len() + 1);
    let mut gen_pairs: Vec<(u32, u32)> = Vec::new();
    let mut kill_pairs: Vec<(u32, u32)> = Vec::new();
    let mut kill_at = vec![NONE; nv];
    let mut gen_at = vec![NONE; nv];
    for (bi, b) in code.blocks.iter().enumerate() {
        let bu = bi as u32;
        for &p in &b.params {
            if kill_at[p.index()] != bu {
                kill_at[p.index()] = bu;
                kill_pairs.push((bu, p.0));
            }
        }
        for i in b.start..b.end {
            debug_assert_eq!(op_start.len(), i);
            op_start.push(ops.len() as u32);
            let first = ops.len();
            code.insts[i].operands(&mut ops);
            let these = &ops[first..];
            for o in these {
                let v = o.vreg.index();
                if o.kind != OperandKind::Def && kill_at[v] != bu && gen_at[v] != bu {
                    gen_at[v] = bu;
                    gen_pairs.push((bu, o.vreg.0));
                }
            }
            for o in these {
                let v = o.vreg.index();
                if o.kind != OperandKind::Use && kill_at[v] != bu {
                    kill_at[v] = bu;
                    kill_pairs.push((bu, o.vreg.0));
                }
            }
        }
    }
    op_start.push(ops.len() as u32);
    drop((kill_at, gen_at));
    // Liveness per global value by path exploration: from each block where it is upward-exposed,
    // walk predecessors (live out of each) until a block that defines it. Linear in the total
    // size of the live ranges, where dense per-block bitsets were blocks x globals per pass.
    let mut pred_pairs: Vec<(u32, u32)> = Vec::new();
    for (bi, b) in code.blocks.iter().enumerate() {
        for &s in &b.succs {
            pred_pairs.push((s as u32, bi as u32));
        }
    }
    let preds = Groups::new(nb, &pred_pairs);
    drop(pred_pairs);
    let mut is_global = vec![false; nv];
    let mut globals: Vec<u32> = Vec::new();
    for &(_, v) in &gen_pairs {
        if !std::mem::replace(&mut is_global[v as usize], true) {
            globals.push(v);
        }
    }
    let gen_of = Groups::new(
        nv,
        &gen_pairs.iter().map(|&(b, v)| (v, b)).collect::<Vec<_>>(),
    );
    let kill_of = Groups::new(
        nv,
        &kill_pairs
            .iter()
            .filter(|&&(_, v)| is_global[v as usize])
            .map(|&(b, v)| (v, b))
            .collect::<Vec<_>>(),
    );
    drop((gen_pairs, kill_pairs, is_global));
    let mut live_in_pairs: Vec<(u32, u32)> = Vec::new();
    let mut live_out_pairs: Vec<(u32, u32)> = Vec::new();
    // `mark[b] == stamp` flags for the value being walked (one stamp per value).
    let (mut in_mark, mut out_mark) = (vec![0u32; nb], vec![0u32; nb]);
    let mut kill_mark = vec![0u32; nb];
    let mut work: Vec<u32> = Vec::new();
    for (gi, &v) in globals.iter().enumerate() {
        let stamp = gi as u32 + 1;
        for &b in kill_of.of(v as usize) {
            kill_mark[b as usize] = stamp;
        }
        for &b in gen_of.of(v as usize) {
            if in_mark[b as usize] != stamp {
                in_mark[b as usize] = stamp;
                live_in_pairs.push((b, v));
                work.push(b);
            }
        }
        while let Some(b) = work.pop() {
            for &p in preds.of(b as usize) {
                let pi = p as usize;
                if out_mark[pi] == stamp {
                    continue;
                }
                out_mark[pi] = stamp;
                live_out_pairs.push((p, v));
                if kill_mark[pi] != stamp && in_mark[pi] != stamp {
                    in_mark[pi] = stamp;
                    live_in_pairs.push((p, v));
                    work.push(p);
                }
            }
        }
    }
    let live_in = Groups::new(nb, &live_in_pairs);
    let live_out = Groups::new(nb, &live_out_pairs);
    drop((gen_of, kill_of, preds, in_mark, out_mark, kill_mark));
    drop((live_in_pairs, live_out_pairs));

    // ----- intervals: per-block live segments (lifetime holes) -----
    // Segments per value as linked entries (value-order is block order, so each list arrives
    // sorted; adjacent ones merge), flattened once complete.
    let mut seg_head = vec![NONE; nv];
    let mut seg_tail = vec![NONE; nv];
    let mut seg_pool: Vec<(u32, u32, u32)> = Vec::with_capacity(nv + nv / 2);
    let mut weight = vec![0f64; nv];
    let mut hint: Vec<Option<PReg>> = vec![None; nv];
    // Values that want to share a register: copy source/destination, jump argument/parameter.
    let mut related_pairs: Vec<(u32, VReg)> = Vec::new();
    // Clobber positions per physical register (sorted, since instructions are visited in order).
    let mut clobbers: Vec<Vec<u32>> = vec![Vec::new(); 64];
    // Per block: the (lo, hi) of each value touched in it, then flushed into the segments.
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
        for (set, pos) in [(live_in.of(bi), entry), (live_out.of(bi), exit)] {
            for &v in set {
                touch(VReg(v), pos, &mut lo, &mut hi, &mut touched);
            }
        }
        for i in b.start..b.end {
            let inst = &code.insts[i];
            let mut clob = inst.clobbers();
            for o in &ops[op_start[i] as usize..op_start[i + 1] as usize] {
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
                related_pairs.push((dst.0, src));
                related_pairs.push((src.0, dst));
            }
            if let Some((t, args)) = inst.jump_args() {
                for (&a, &p) in args.iter().zip(&code.blocks[t].params) {
                    related_pairs.push((a.0, p));
                    related_pairs.push((p.0, a));
                }
            }
            for r in clob.iter() {
                clobbers[r.bit() as usize].push(def_pos(i));
            }
        }
        for v in touched.drain(..) {
            let vi = v.index();
            let (l, h) = (lo[vi], hi[vi]);
            let tail = seg_tail[vi];
            if tail != NONE && seg_pool[tail as usize].1 + 1 >= l {
                let last = &mut seg_pool[tail as usize];
                last.1 = last.1.max(h);
            } else {
                let at = seg_pool.len() as u32;
                seg_pool.push((l, h, NONE));
                if tail == NONE {
                    seg_head[vi] = at;
                } else {
                    seg_pool[tail as usize].2 = at;
                }
                seg_tail[vi] = at;
            }
            lo[vi] = u32::MAX;
            hi[vi] = 0;
        }
    }
    drop((live_in, live_out, ops, op_start));
    let related = Groups::new(nv, &related_pairs);
    drop(related_pairs);
    let mut seg_offsets = Vec::with_capacity(nv + 1);
    let mut seg_items: Vec<(u32, u32)> = Vec::with_capacity(seg_pool.len());
    for &head in &seg_head {
        seg_offsets.push(seg_items.len() as u32);
        let mut at = head;
        while at != NONE {
            let (l, h, next) = seg_pool[at as usize];
            seg_items.push((l, h));
            at = next;
        }
    }
    seg_offsets.push(seg_items.len() as u32);
    drop((seg_head, seg_tail, seg_pool));
    let segs = |v: usize| &seg_items[seg_offsets[v] as usize..seg_offsets[v + 1] as usize];

    // Values by first position (ties by number): a counting sort over the positions.
    let order: Vec<usize> = {
        let positions = 2 * code.insts.len() + 2;
        let mut count = vec![0u32; positions + 1];
        let mut n = 0;
        for v in 0..nv {
            if !segs(v).is_empty() {
                count[segs(v)[0].0 as usize + 1] += 1;
                n += 1;
            }
        }
        for i in 0..positions {
            count[i + 1] += count[i];
        }
        let mut order = vec![0usize; n];
        for v in 0..nv {
            if !segs(v).is_empty() {
                let at = &mut count[segs(v)[0].0 as usize];
                order[*at as usize] = v;
                *at += 1;
            }
        }
        order
    };

    // Registers a call clobbers all share one position list: look each distinct list up once
    // per value.
    let clobber_class: Vec<u8> = (0..64)
        .map(|r| (0..=r).find(|&q| clobbers[q] == clobbers[r]).unwrap() as u8)
        .collect();
    let clobber_memo: [std::cell::Cell<(u32, bool)>; 64] =
        [const { std::cell::Cell::new((0, false)) }; 64];
    // Whether register `r` is overwritten strictly inside `(s, e]` of one of `v`'s segments.
    let clobbered = |r: PReg, v: usize| -> bool {
        let class = clobber_class[r.bit() as usize] as usize;
        let (stamp, known) = clobber_memo[class].get();
        if stamp == v as u32 + 1 {
            return known;
        }
        let list = &clobbers[class];
        let hit = segs(v).iter().any(|&(s, e)| {
            let i = list.partition_point(|&p| p <= s);
            i < list.len() && list[i] <= e
        });
        clobber_memo[class].set((v as u32 + 1, hit));
        hit
    };
    // Whether any of register `r`'s segments overlaps value `v`.
    let overlaps = |assigned: &[Vec<(u32, u32, u32)>], r: PReg, v: usize| -> bool {
        let list = &assigned[r.bit() as usize];
        segs(v).iter().any(|&(s, e)| {
            let i = list.partition_point(|x| x.1 < s);
            i < list.len() && list[i].0 <= e
        })
    };
    // Owners of register `r`'s segments that overlap value `v`.
    let overlapping = |assigned: &[Vec<(u32, u32, u32)>], r: PReg, v: usize| -> Vec<u32> {
        let list = &assigned[r.bit() as usize];
        let mut out = Vec::new();
        for &(s, e) in segs(v) {
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
    let mut used_callee_saved = RegSet::EMPTY;
    let allowed = [RegSet::of(&info.order[0]), RegSet::of(&info.order[1])];
    // Segments assigned to each physical register, sorted and disjoint: (start, end, vreg).
    let mut assigned: Vec<Vec<(u32, u32, u32)>> = vec![Vec::new(); 64];
    for &v in &order {
        let class = code.class(VReg(v as u32));
        let ci = class_index(class);
        let usable = |r: PReg| r.class == class && allowed[ci].contains(r) && !clobbered(r, v);
        let free = |assigned: &[Vec<(u32, u32, u32)>], r: PReg| {
            usable(r) && !overlaps(assigned, r, v)
        };
        let related_reg = related.of(v).iter().find_map(|s| match locs[s.index()] {
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
                        locs[o as usize] = Loc::Stack(u32::MAX);
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
                for &(s, e) in segs(v) {
                    let i = list.partition_point(|x| x.0 < s);
                    list.insert(i, (s, e, v as u32));
                }
            }
            None => locs[v] = Loc::Stack(u32::MAX),
        }
    }

    // Spill slots: a value takes the slot of a related value (a block parameter and its
    // arguments, a copy's ends) when their segments are disjoint, which turns the edge move
    // between two spilled values into nothing; otherwise a slot whose values all ended before
    // it starts.
    let spilled: Vec<usize> = order
        .iter()
        .copied()
        .filter(|&v| matches!(locs[v], Loc::Stack(_)))
        .collect();
    let mut slot_segs: Vec<Vec<(u32, u32)>> = Vec::new();
    let mut slot_end: Vec<u32> = Vec::new();
    let mut slot_free: Vec<bool> = Vec::new();
    let mut busy: std::collections::BinaryHeap<std::cmp::Reverse<(u32, u32)>> =
        std::collections::BinaryHeap::new();
    let mut free_slots: Vec<u32> = Vec::new();
    for v in spilled {
        let (start, end) = (segs(v)[0].0, segs(v).last().unwrap().1);
        while let Some(&std::cmp::Reverse((e, slot))) = busy.peek() {
            if e >= start {
                break;
            }
            busy.pop();
            if slot_end[slot as usize] == e && !slot_free[slot as usize] {
                slot_free[slot as usize] = true;
                free_slots.push(slot);
            }
        }
        let fits = |list: &Vec<(u32, u32)>| {
            segs(v).iter().all(|&(s, e)| {
                let i = list.partition_point(|x| x.1 < s);
                i >= list.len() || list[i].0 > e
            })
        };
        let mut slot = related.of(v).iter().find_map(|r| match locs[r.index()] {
            Loc::Stack(s) if s != u32::MAX && fits(&slot_segs[s as usize]) => Some(s),
            _ => None,
        });
        if slot.is_none() {
            while let Some(s) = free_slots.pop() {
                if slot_free[s as usize] {
                    slot = Some(s);
                    break;
                }
            }
        }
        let slot = slot.unwrap_or_else(|| {
            slot_segs.push(Vec::new());
            slot_end.push(0);
            slot_free.push(false);
            slot_segs.len() as u32 - 1
        });
        let si = slot as usize;
        slot_free[si] = false;
        let list = &mut slot_segs[si];
        for &(s, e) in segs(v) {
            let i = list.partition_point(|x| x.0 < s);
            list.insert(i, (s, e));
        }
        slot_end[si] = slot_end[si].max(end);
        busy.push(std::cmp::Reverse((slot_end[si], slot)));
        locs[v] = Loc::Stack(slot);
    }
    let num_slots = slot_segs.len() as u32;

    Allocation {
        locs,
        num_slots,
        used_callee_saved,
    }
}
