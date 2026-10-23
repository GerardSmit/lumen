//! Loop-invariant code motion.
//!
//! Natural loops come from back edges (a branch to a block dominating its source). Every loop
//! gets a preheader: the one block outside the loop that enters it, ending in a jump to the
//! header. An entering block that already is one is reused; otherwise a new block taking the
//! header's parameters is placed before the header and every entering edge is redirected to it.
//!
//! Loops are then processed innermost first, moving to the preheader, in dominance order:
//! - pure instructions whose operands are all defined outside the loop (or already hoisted);
//! - loads with an invariant address whose memory nothing in the loop may write: the load is
//!   readonly, or its alias class is known and no store or call in the loop writes that class
//!   (see [`MemFlags`], [`CallEffects`]). Loads of unknown class are never hoisted.
//!
//! The preheader runs whenever the loop is entered, even when the body would not run (the
//! header's test may exit at once) or would stop at an earlier exit. A hoisted load must then
//! not fault where the original would not have run: it is marked `notrap`, or its block runs on
//! every first iteration before the loop can be left — it dominates every exiting block and
//! every latch — and the load precedes any `trap_if` there.
//!
//! Stores, calls and branches stay in the loop: a guard whose condition becomes invariant still
//! branches there, on a value computed once. Cold blocks are left alone, and an address add (or
//! the shift or extension feeding one) whose single use stays in the same block is kept beside
//! it, where the backends fold it into the addressing mode.

use crate::cfg::Cfg;
use crate::ir::*;

struct Loop {
    header: Block,
    /// The header first, then the rest in reverse postorder.
    blocks: Vec<Block>,
    latches: Vec<Block>,
}

fn find_loops(cfg: &Cfg) -> Vec<Loop> {
    let mut mark = vec![u32::MAX; cfg.idom.len()];
    let mut loops = Vec::new();
    let mut work = Vec::new();
    for &h in &cfg.rpo {
        let latches: Vec<Block> = cfg.preds[h.index()]
            .iter()
            .copied()
            .filter(|&p| cfg.dominates(h, p))
            .collect();
        if latches.is_empty() {
            continue;
        }
        let id = loops.len() as u32;
        let mut blocks = vec![h];
        mark[h.index()] = id;
        work.clear();
        work.extend(latches.iter().copied());
        while let Some(x) = work.pop() {
            if mark[x.index()] == id {
                continue;
            }
            mark[x.index()] = id;
            blocks.push(x);
            work.extend(cfg.preds[x.index()].iter().copied());
        }
        blocks.sort_by_key(|b| cfg.rpo_index[b.index()]);
        loops.push(Loop {
            header: h,
            blocks,
            latches,
        });
    }
    loops
}

/// The only block entering `h` from outside the loop, when it ends in a jump.
fn existing_preheader(
    func: &Function,
    cfg: &Cfg,
    h: Block,
    in_loop: impl Fn(Block) -> bool,
) -> Option<Block> {
    let mut entering = cfg.preds[h.index()].iter().copied().filter(|&p| !in_loop(p));
    let p = entering.next()?;
    if entering.next().is_some() {
        return None;
    }
    let t = func.terminator(p)?;
    matches!(func.inst(t), InstData::Jump { .. }).then_some(p)
}

/// A new block with `h`'s parameters, jumping to `h`, that every entering edge now targets.
fn make_preheader(func: &mut Function, cfg: &Cfg, h: Block, in_loop: impl Fn(Block) -> bool) {
    let ph = func.create_block();
    let params: Vec<Value> = func.blocks[h.index()]
        .params
        .clone()
        .into_iter()
        .map(|p| {
            let ty = func.value_type(p);
            func.append_block_param(ph, ty)
        })
        .collect();
    let mut cold = func.blocks[h.index()].cold;
    let mut all_cold = true;
    for &p in &cfg.preds[h.index()] {
        if in_loop(p) {
            continue;
        }
        all_cold &= func.blocks[p.index()].cold;
        let t = func.terminator(p).expect("a predecessor ends in a branch");
        func.insts[t.index()].for_each_successor_mut(|c| {
            if c.block == h {
                c.block = ph;
            }
        });
    }
    cold |= all_cold;
    func.blocks[ph.index()].cold = cold;
    let jump = func.make_inst(InstData::Jump {
        dest: BlockCall {
            block: h,
            args: params,
        },
    });
    func.blocks[ph.index()].insts.push(jump);
    let at = func
        .layout
        .iter()
        .position(|&b| b == h)
        .unwrap_or(func.layout.len());
    func.layout.insert(at, ph);
}

fn is_constant(d: &InstData) -> bool {
    matches!(
        d,
        InstData::Iconst { .. } | InstData::F32const { .. } | InstData::F64const { .. }
    )
}

/// Per-function tables shared by the loop analyses.
struct Scratch {
    uses: Vec<u32>,
    user: Vec<Inst>,
    inst_block: Vec<Block>,
    mark: Vec<u32>,
    stamp: u32,
    /// `defined[v] == stamp` while `v` is defined inside the loop being processed.
    defined: Vec<u32>,
    hoisted: Vec<bool>,
}

impl Scratch {
    fn new(func: &Function) -> Scratch {
        let nv = func.values.len();
        let mut s = Scratch {
            uses: vec![0; nv],
            user: vec![Inst(u32::MAX); nv],
            inst_block: vec![Block(u32::MAX); func.insts.len()],
            mark: vec![0; func.blocks.len()],
            stamp: 0,
            defined: vec![0; nv],
            hoisted: vec![false; func.insts.len()],
        };
        for &b in &func.layout {
            for &i in &func.blocks[b.index()].insts {
                s.inst_block[i.index()] = b;
                func.inst(i).for_each_arg(|a| {
                    s.uses[a.index()] += 1;
                    s.user[a.index()] = i;
                });
            }
        }
        s
    }
}

/// The instructions to hoist out of `lp`, in order, with their `hoisted` flags set. Constants
/// are never hoisted alone: one moves only when a hoisted instruction uses it.
fn analyze(func: &Function, cfg: &Cfg, lp: &Loop, s: &mut Scratch) -> Vec<Inst> {
    s.stamp += 1;
    let stamp = s.stamp;
    for &b in &lp.blocks {
        s.mark[b.index()] = stamp;
    }
    let mark = &s.mark;
    let in_loop = |b: Block| mark[b.index()] == stamp;

    // What the loop may write, and the block every exit and latch is dominated by.
    let mut writes = AliasSet::NONE;
    let mut anchor: Option<Block> = None;
    let mut meet = |b: Block| {
        anchor = Some(match anchor {
            None => b,
            Some(a) => cfg.common_dominator(a, b),
        })
    };
    for &b in &lp.blocks {
        for &p in &func.blocks[b.index()].params {
            s.defined[p.index()] = stamp;
        }
        let mut leaves = false;
        for &i in &func.blocks[b.index()].insts {
            let data = func.inst(i);
            if !is_constant(data) {
                for &r in func.results(i) {
                    s.defined[r.index()] = stamp;
                }
            }
            writes = writes.union(func.writes(i));
            leaves |= may_not_continue(data);
        }
        if leaves || cfg.succs[b.index()].iter().any(|&x| !in_loop(x)) {
            meet(b);
        }
    }
    for &l in &lp.latches {
        meet(l);
    }
    let anchor = anchor.unwrap_or(lp.header);

    let mut hoist: Vec<Inst> = Vec::new();
    for &b in &lp.blocks {
        if func.blocks[b.index()].cold {
            continue;
        }
        let mut first_iteration = cfg.dominates(b, anchor);
        for &i in &func.blocks[b.index()].insts {
            let data = func.inst(i);
            if may_not_continue(data) {
                first_iteration = false;
                continue;
            }
            if data.is_terminator() || func.results(i).is_empty() || is_constant(data) {
                continue;
            }
            let mut invariant = true;
            data.for_each_arg(|a| invariant &= s.defined[a.index()] != stamp);
            if !invariant {
                continue;
            }
            let movable = match data {
                InstData::Load { .. } => {
                    let flags = func.mem_flags(i);
                    let unwritten = flags.readonly
                        || (flags.class.is_known() && !writes.may_touch(flags.class));
                    unwritten && (flags.notrap || first_iteration)
                }
                d => d.is_pure(),
            };
            if movable {
                hoist.push(i);
                s.hoisted[i.index()] = true;
                for &r in func.results(i) {
                    s.defined[r.index()] = 0;
                }
            }
        }
    }

    // Keep address arithmetic next to the single memory access it folds into. Later users
    // come first, so an add is settled before the shift feeding it.
    for k in (0..hoist.len()).rev() {
        let i = hoist[k];
        if folds_into_user(func, i, s) {
            s.hoisted[i.index()] = false;
            for &r in func.results(i) {
                s.defined[r.index()] = stamp;
            }
        }
    }
    hoist.retain(|i| s.hoisted[i.index()]);

    let mut consts: Vec<Inst> = Vec::new();
    for &i in &hoist {
        func.inst(i).for_each_arg(|a| {
            if let ValueDef::Result(c, 0) = func.values[func.resolve(a).index()].def {
                if is_constant(func.inst(c))
                    && in_loop(s.inst_block[c.index()])
                    && !s.hoisted[c.index()]
                {
                    s.hoisted[c.index()] = true;
                    consts.push(c);
                }
            }
        });
    }
    consts.extend(hoist);
    consts
}

/// Whether execution may leave the program or the function after `data` without reaching the
/// instructions behind it: a trap, or a call that may throw or diverge.
fn may_not_continue(data: &InstData) -> bool {
    matches!(
        data,
        InstData::TrapIf { .. } | InstData::Call { .. } | InstData::CallIndirect { .. }
    )
}

/// Hoist loop-invariant instructions into loop preheaders (creating those as needed, only for
/// loops that have something to hoist); whether the function changed.
pub fn licm(func: &mut Function) -> bool {
    if func.layout.is_empty() {
        return false;
    }
    func.resolve_aliases();
    let entry = func.entry();
    let mut cfg = Cfg::new(func);
    let mut loops = find_loops(&cfg);
    if loops.is_empty() {
        return false;
    }

    let mut made = false;
    {
        let mut s = Scratch::new(func);
        let mut stamp = 0u32;
        let mut needs: Vec<usize> = Vec::new();
        for (k, lp) in loops.iter().enumerate() {
            if lp.header == entry {
                continue;
            }
            let hoist = analyze(func, &cfg, lp, &mut s);
            for &i in &hoist {
                s.hoisted[i.index()] = false;
            }
            if hoist.is_empty() {
                continue;
            }
            let mark = &s.mark;
            let st = s.stamp;
            if existing_preheader(func, &cfg, lp.header, |b| mark[b.index()] == st).is_none() {
                needs.push(k);
            }
        }
        let mut mark = vec![0u32; func.blocks.len()];
        for k in needs {
            let lp = &loops[k];
            stamp += 1;
            for &b in &lp.blocks {
                mark[b.index()] = stamp;
            }
            let st = stamp;
            make_preheader(func, &cfg, lp.header, |b| mark.get(b.index()) == Some(&st));
            made = true;
        }
    }
    if made {
        cfg = Cfg::new(func);
        loops = find_loops(&cfg);
    }
    // Inner loops first: a loop's body contains every loop nested in it.
    loops.sort_by_key(|l| l.blocks.len());

    let mut s = Scratch::new(func);
    let mut changed = made;
    for lp in &loops {
        let h = lp.header;
        if h == entry {
            continue;
        }
        let hoist = analyze(func, &cfg, lp, &mut s);
        let mark = &s.mark;
        let st = s.stamp;
        let ph = existing_preheader(func, &cfg, h, |b| mark[b.index()] == st);
        let Some(ph) = ph.filter(|_| !hoist.is_empty()) else {
            for &i in &hoist {
                s.hoisted[i.index()] = false;
            }
            continue;
        };
        changed = true;
        for &b in &lp.blocks {
            let hoisted = &s.hoisted;
            func.blocks[b.index()]
                .insts
                .retain(|i| !hoisted[i.index()]);
        }
        let at = func.blocks[ph.index()].insts.len() - 1;
        func.blocks[ph.index()]
            .insts
            .splice(at..at, hoist.iter().copied());
        for &i in &hoist {
            s.inst_block[i.index()] = ph;
            s.hoisted[i.index()] = false;
        }
    }
    changed
}

/// Whether `inst` is address arithmetic the backends fold into its single user, which stays in
/// the same block.
fn folds_into_user(func: &Function, inst: Inst, s: &Scratch) -> bool {
    let (uses, user, inst_block, hoisted) = (&s.uses, &s.user, &s.inst_block, &s.hoisted);
    let [r] = func.results(inst) else {
        return false;
    };
    if uses[r.index()] != 1 || func.value_type(*r) != Type::I64 {
        return false;
    }
    let u = user[r.index()];
    if hoisted[u.index()] || inst_block[u.index()] != inst_block[inst.index()] {
        return false;
    }
    match func.inst(inst) {
        InstData::Binary {
            op: BinaryOp::Iadd, ..
        } => matches!(func.inst(u),
            InstData::Load { addr, .. } | InstData::Store { addr, .. } if addr == r),
        InstData::Binary {
            op: BinaryOp::Ishl, ..
        }
        | InstData::Convert {
            op: ConvOp::Sext, ..
        } => matches!(func.inst(u), InstData::Binary {
            op: BinaryOp::Iadd, ..
        }),
        _ => false,
    }
}
