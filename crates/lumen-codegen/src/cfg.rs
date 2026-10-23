//! Control-flow analyses: reverse postorder, dominators (Cooper, Harvey & Kennedy, "A Simple,
//! Fast Dominance Algorithm") and natural-loop depth.

use crate::ir::{Block, Function};

/// Per-block lists in one allocation, indexed by block number.
pub struct Adj {
    ranges: Vec<(u32, u32)>,
    items: Vec<Block>,
}

impl Adj {
    fn with_blocks(n: usize) -> Adj {
        Adj {
            ranges: vec![(0, 0); n],
            items: Vec::new(),
        }
    }
}

impl std::ops::Index<usize> for Adj {
    type Output = [Block];
    #[inline]
    fn index(&self, i: usize) -> &[Block] {
        let (start, len) = self.ranges[i];
        &self.items[start as usize..(start + len) as usize]
    }
}

pub struct Cfg {
    /// Reachable blocks in reverse postorder from the entry.
    pub rpo: Vec<Block>,
    /// Position of each block in `rpo`, or `usize::MAX` when unreachable.
    pub rpo_index: Vec<usize>,
    pub preds: Adj,
    pub succs: Adj,
    /// Immediate dominator; the entry is its own, unreachable blocks have `None`.
    pub idom: Vec<Option<Block>>,
    /// Loop nesting depth (0 outside loops); filled by [`Cfg::with_loops`].
    pub loop_depth: Vec<u32>,
    /// Preorder number and subtree size of each block in the dominator tree.
    dom_pre: Vec<u32>,
    dom_size: Vec<u32>,
}

/// The blocks the entry reaches, indexed by block number.
pub fn reachable(func: &Function) -> Vec<bool> {
    let mut seen = vec![false; func.blocks.len()];
    if func.layout.is_empty() {
        return seen;
    }
    let entry = func.entry();
    seen[entry.index()] = true;
    let mut work = vec![entry];
    while let Some(b) = work.pop() {
        for s in func.successors(b) {
            if !seen[s.index()] {
                seen[s.index()] = true;
                work.push(s);
            }
        }
    }
    seen
}

impl Cfg {
    /// Blocks, edges and dominators.
    pub fn new(func: &Function) -> Cfg {
        let n = func.blocks.len();
        let mut succs = Adj::with_blocks(n);
        for &b in &func.layout {
            let start = succs.items.len();
            for x in func.successors(b) {
                if !succs.items[start..].contains(&x) {
                    succs.items.push(x);
                }
            }
            succs.ranges[b.index()] = (start as u32, (succs.items.len() - start) as u32);
        }

        let mut rpo = Vec::new();
        let mut visited = vec![false; n];
        if !func.layout.is_empty() {
            let entry = func.entry();
            let mut stack = vec![(entry, 0usize)];
            visited[entry.index()] = true;
            while let Some(&mut (b, ref mut i)) = stack.last_mut() {
                if let Some(&s) = succs[b.index()].get(*i) {
                    *i += 1;
                    if !visited[s.index()] {
                        visited[s.index()] = true;
                        stack.push((s, 0));
                    }
                } else {
                    rpo.push(b);
                    stack.pop();
                }
            }
            rpo.reverse();
        }
        let mut rpo_index = vec![usize::MAX; n];
        for (i, &b) in rpo.iter().enumerate() {
            rpo_index[b.index()] = i;
        }

        let mut preds = Adj::with_blocks(n);
        for &b in &rpo {
            for &s in &succs[b.index()] {
                preds.ranges[s.index()].1 += 1;
            }
        }
        let mut total = 0u32;
        for r in &mut preds.ranges {
            r.0 = total;
            total += r.1;
            r.1 = 0;
        }
        preds.items = vec![Block(0); total as usize];
        for &b in &rpo {
            for &s in &succs[b.index()] {
                let r = &mut preds.ranges[s.index()];
                preds.items[(r.0 + r.1) as usize] = b;
                r.1 += 1;
            }
        }

        let mut idom: Vec<Option<Block>> = vec![None; n];
        if let Some(&entry) = rpo.first() {
            idom[entry.index()] = Some(entry);
            let mut changed = true;
            while changed {
                changed = false;
                for &b in rpo.iter().skip(1) {
                    let mut new: Option<Block> = None;
                    for &p in &preds[b.index()] {
                        if idom[p.index()].is_none() {
                            continue;
                        }
                        new = Some(match new {
                            None => p,
                            Some(q) => intersect(&idom, &rpo_index, p, q),
                        });
                    }
                    if new.is_some() && idom[b.index()] != new {
                        idom[b.index()] = new;
                        changed = true;
                    }
                }
            }
        }

        // A dominator precedes what it dominates in reverse postorder, so subtree sizes
        // accumulate backwards and preorder numbers forwards.
        let mut dom_size = vec![1u32; n];
        for &b in rpo.iter().skip(1).rev() {
            if let Some(d) = idom[b.index()] {
                dom_size[d.index()] += dom_size[b.index()];
            }
        }
        let mut dom_pre = vec![0u32; n];
        let mut next = vec![1u32; n];
        for &b in rpo.iter().skip(1) {
            if let Some(d) = idom[b.index()] {
                dom_pre[b.index()] = dom_pre[d.index()] + next[d.index()];
                next[d.index()] += dom_size[b.index()];
            }
        }

        Cfg {
            rpo,
            rpo_index,
            preds,
            succs,
            idom,
            loop_depth: vec![0; n],
            dom_pre,
            dom_size,
        }
    }

    /// [`Cfg::new`] plus loop nesting depths.
    pub fn with_loops(func: &Function) -> Cfg {
        let mut cfg = Cfg::new(func);
        cfg.compute_loops();
        cfg
    }

    pub fn is_reachable(&self, b: Block) -> bool {
        self.rpo_index[b.index()] != usize::MAX
    }

    /// Whether `a` dominates `b` (reflexive).
    #[inline]
    pub fn dominates(&self, a: Block, b: Block) -> bool {
        if !self.is_reachable(b) {
            return true;
        }
        if !self.is_reachable(a) {
            return false;
        }
        let (pa, pb) = (self.dom_pre[a.index()], self.dom_pre[b.index()]);
        pa <= pb && pb < pa + self.dom_size[a.index()]
    }

    /// The nearest block dominating both `a` and `b` (both reachable).
    pub fn common_dominator(&self, a: Block, b: Block) -> Block {
        intersect(&self.idom, &self.rpo_index, a, b)
    }

    /// Children of each block in the dominator tree, in RPO order.
    pub fn dom_children(&self) -> Vec<Vec<Block>> {
        let mut ch = vec![Vec::new(); self.idom.len()];
        for &b in self.rpo.iter().skip(1) {
            if let Some(d) = self.idom[b.index()] {
                ch[d.index()].push(b);
            }
        }
        ch
    }

    /// Children of each block in the dominator tree, in RPO order.
    pub fn dom_children_adj(&self) -> Adj {
        let n = self.idom.len();
        let mut ch = Adj::with_blocks(n);
        for &b in self.rpo.iter().skip(1) {
            if let Some(d) = self.idom[b.index()] {
                ch.ranges[d.index()].1 += 1;
            }
        }
        let mut total = 0u32;
        for r in &mut ch.ranges {
            r.0 = total;
            total += r.1;
            r.1 = 0;
        }
        ch.items = vec![Block(0); total as usize];
        for &b in self.rpo.iter().skip(1) {
            if let Some(d) = self.idom[b.index()] {
                let r = &mut ch.ranges[d.index()];
                ch.items[(r.0 + r.1) as usize] = b;
                r.1 += 1;
            }
        }
        ch
    }

    fn compute_loops(&mut self) {
        // A back edge p -> h has h dominating p; the loop body is everything reaching p without
        // passing h.
        let mut depth = vec![0u32; self.idom.len()];
        // `mark[b] == stamp` while `b` is in the body being collected (one stamp per back edge).
        let mut mark = vec![0u32; self.idom.len()];
        let mut stamp = 0u32;
        let mut body = Vec::new();
        let mut work = Vec::new();
        for &h in &self.rpo {
            for &p in &self.preds[h.index()] {
                if !self.dominates(h, p) {
                    continue;
                }
                stamp += 1;
                body.clear();
                body.push(h);
                mark[h.index()] = stamp;
                work.clear();
                work.push(p);
                while let Some(x) = work.pop() {
                    if mark[x.index()] == stamp {
                        continue;
                    }
                    mark[x.index()] = stamp;
                    body.push(x);
                    work.extend(self.preds[x.index()].iter().copied());
                }
                for &b in &body {
                    depth[b.index()] += 1;
                }
            }
        }
        self.loop_depth = depth;
    }
}

fn intersect(idom: &[Option<Block>], rpo_index: &[usize], mut a: Block, mut b: Block) -> Block {
    while a != b {
        while rpo_index[a.index()] > rpo_index[b.index()] {
            a = idom[a.index()].expect("processed");
        }
        while rpo_index[b.index()] > rpo_index[a.index()] {
            b = idom[b.index()].expect("processed");
        }
    }
    a
}

/// Block placement: move the cold blocks — those from which every path ends the function
/// (a return or trap: in JIT code, the exits) — after all the others, keeping the relative
/// order of each group. The hot path then runs through fewer taken jumps and the exits' code
/// stays out of its way. Values a cold block defines are only used in blocks it dominates,
/// which are cold too, so no hot block reads one.
pub fn sink_cold(f: &Function, cfg: &Cfg, order: Vec<Block>) -> Vec<Block> {
    let mut cold = cold_blocks(f, cfg, &order);
    // Blocks dominated by one marked rarely executed (a slow path and what only it reaches)
    // go last too: they read only values their dominators define, so no hot block reads one.
    for &b in &order {
        let mut x = b;
        loop {
            if f.blocks[x.index()].cold {
                cold[b.index()] = true;
                break;
            }
            match cfg.idom[x.index()] {
                Some(d) if d != x => x = d,
                _ => break,
            }
        }
    }
    let entry = f.entry();
    let (mut hot, rest): (Vec<Block>, Vec<Block>) = order
        .into_iter()
        .partition(|&b| b == entry || !cold[b.index()]);
    hot.extend(rest);
    hot
}

/// The blocks from which every path ends the function (a return or trap), indexed by block.
pub fn cold_blocks(f: &Function, cfg: &Cfg, order: &[Block]) -> Vec<bool> {
    use crate::ir::InstData;
    let n = f.blocks.len();
    let mut cold = vec![false; n];
    for &b in order {
        if let Some(t) = f.terminator(b) {
            if matches!(f.inst(t), InstData::Return { .. } | InstData::Trap { .. }) {
                cold[b.index()] = true;
            }
        }
    }
    loop {
        let mut changed = false;
        for &b in order.iter().rev() {
            let s = &cfg.succs[b.index()];
            if !cold[b.index()] && !s.is_empty() && s.iter().all(|x| cold[x.index()]) {
                cold[b.index()] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    cold
}
