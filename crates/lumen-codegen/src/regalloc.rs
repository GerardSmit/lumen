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

#[derive(Clone)]
struct Interval {
    vreg: VReg,
    start: u32,
    end: u32,
    weight: f64,
    hint: Option<PReg>,
    /// Hint from a copy: take the source's register if it was allocated one.
    copy_of: Option<VReg>,
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
    let words = nv.div_ceil(64);
    let nb = code.blocks.len();

    // ----- per-block gen/kill and liveness -----
    let mut gen = vec![vec![0u64; words]; nb];
    let mut kill = vec![vec![0u64; words]; nb];
    let mut ops = Vec::new();
    let set = |s: &mut Vec<u64>, v: VReg| s[v.index() / 64] |= 1 << (v.index() % 64);
    let has = |s: &Vec<u64>, v: VReg| s[v.index() / 64] & (1 << (v.index() % 64)) != 0;
    for (bi, b) in code.blocks.iter().enumerate() {
        for &p in &b.params {
            set(&mut kill[bi], p);
        }
        for i in b.start..b.end {
            ops.clear();
            code.insts[i].operands(&mut ops);
            for o in &ops {
                if o.kind != OperandKind::Def && !has(&kill[bi], o.vreg) {
                    set(&mut gen[bi], o.vreg);
                }
            }
            for o in &ops {
                if o.kind != OperandKind::Use {
                    set(&mut kill[bi], o.vreg);
                }
            }
        }
    }
    let mut live_in = vec![vec![0u64; words]; nb];
    let mut live_out = vec![vec![0u64; words]; nb];
    let mut changed = true;
    while changed {
        changed = false;
        for bi in (0..nb).rev() {
            let mut out = vec![0u64; words];
            for &s in &code.blocks[bi].succs {
                for w in 0..words {
                    out[w] |= live_in[s][w];
                }
            }
            let mut inn = gen[bi].clone();
            for w in 0..words {
                inn[w] |= out[w] & !kill[bi][w];
            }
            if inn != live_in[bi] || out != live_out[bi] {
                live_in[bi] = inn;
                live_out[bi] = out;
                changed = true;
            }
        }
    }

    // ----- intervals -----
    let mut start = vec![u32::MAX; nv];
    let mut end = vec![0u32; nv];
    let mut weight = vec![0f64; nv];
    let mut hint: Vec<Option<PReg>> = vec![None; nv];
    let mut copy_of: Vec<Option<VReg>> = vec![None; nv];
    let touch = |v: VReg, p: u32, start: &mut Vec<u32>, end: &mut Vec<u32>| {
        start[v.index()] = start[v.index()].min(p);
        end[v.index()] = end[v.index()].max(p);
    };
    // Clobber positions per physical register (sorted, since instructions are visited in order).
    let mut clobbers: Vec<Vec<u32>> = vec![Vec::new(); 64];
    let mut has_call = false;
    for (bi, b) in code.blocks.iter().enumerate() {
        let w = 8f64.powi(b.loop_depth.min(6) as i32);
        let entry = use_pos(b.start);
        let exit = def_pos(b.end - 1);
        for &p in &b.params {
            touch(p, entry, &mut start, &mut end);
            weight[p.index()] += w;
        }
        for v in 0..nv {
            if live_in[bi][v / 64] & (1 << (v % 64)) != 0 {
                touch(VReg(v as u32), entry, &mut start, &mut end);
            }
            if live_out[bi][v / 64] & (1 << (v % 64)) != 0 {
                touch(VReg(v as u32), exit, &mut start, &mut end);
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
                        touch(v, p, &mut start, &mut end);
                    }
                    OperandKind::Def => touch(v, def_pos(i), &mut start, &mut end),
                    OperandKind::Mod => {
                        touch(v, use_pos(i), &mut start, &mut end);
                        touch(v, def_pos(i), &mut start, &mut end);
                    }
                }
                if let Constraint::Fixed(r) = o.constraint {
                    clob.insert(r);
                    hint[v.index()].get_or_insert(r);
                }
            }
            if let Some((dst, src)) = inst.as_move() {
                copy_of[dst.index()] = Some(src);
            }
            if inst.is_call() {
                has_call = true;
            }
            for r in clob.iter() {
                clobbers[r.bit() as usize].push(def_pos(i));
            }
        }
    }
    let _ = has_call;

    let mut intervals: Vec<Interval> = (0..nv)
        .filter(|&v| start[v] != u32::MAX)
        .map(|v| Interval {
            vreg: VReg(v as u32),
            start: start[v],
            end: end[v],
            weight: weight[v],
            hint: hint[v],
            copy_of: copy_of[v],
        })
        .collect();
    intervals.sort_by_key(|iv| (iv.start, iv.vreg));

    // Whether register `r` is overwritten strictly inside `(s, e]`.
    let conflicts = |r: PReg, s: u32, e: u32| -> bool {
        let list = &clobbers[r.bit() as usize];
        let i = list.partition_point(|&p| p <= s);
        i < list.len() && list[i] <= e
    };

    // ----- linear scan -----
    let mut locs = vec![Loc::None; nv];
    let mut num_slots = 0u32;
    let mut used_callee_saved = RegSet::EMPTY;
    // Active intervals per class: (end, vreg, reg, weight).
    let mut active: [Vec<(u32, VReg, PReg, f64)>; 2] = [Vec::new(), Vec::new()];
    for iv in &intervals {
        let class = code.class(iv.vreg);
        let ci = class_index(class);
        active[ci].retain(|a| a.0 >= iv.start);
        let busy = RegSet::of(&active[ci].iter().map(|a| a.2).collect::<Vec<_>>());
        let free = |r: PReg| !busy.contains(r) && !conflicts(r, iv.start, iv.end);

        let copy_hint = iv.copy_of.and_then(|s| match locs[s.index()] {
            Loc::Reg(r) => Some(r),
            _ => None,
        });
        let choice = copy_hint
            .filter(|&r| free(r))
            .or(iv.hint.filter(|&r| r.class == class && info.order[ci].contains(&r) && free(r)))
            .or_else(|| info.order[ci].iter().copied().find(|&r| free(r)));

        let reg = match choice {
            Some(r) => Some(r),
            None => {
                // Spill the cheapest of the current value and the active values whose register
                // could hold the current one.
                let victim = active[ci]
                    .iter()
                    .enumerate()
                    .filter(|(_, a)| !conflicts(a.2, iv.start, iv.end))
                    .min_by(|x, y| x.1 .3.total_cmp(&y.1 .3));
                match victim {
                    Some((vi, a)) if a.3 < iv.weight => {
                        let (_, vv, r, _) = active[ci].remove(vi);
                        locs[vv.index()] = Loc::Stack(num_slots);
                        num_slots += 1;
                        Some(r)
                    }
                    _ => None,
                }
            }
        };
        match reg {
            Some(r) => {
                locs[iv.vreg.index()] = Loc::Reg(r);
                if info.callee_saved.contains(r) {
                    used_callee_saved.insert(r);
                }
                active[ci].push((iv.end, iv.vreg, r, iv.weight));
            }
            None => {
                locs[iv.vreg.index()] = Loc::Stack(num_slots);
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
