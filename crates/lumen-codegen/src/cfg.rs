//! Control-flow analyses: reverse postorder, dominators (Cooper, Harvey & Kennedy, "A Simple,
//! Fast Dominance Algorithm") and natural-loop depth.

use crate::ir::{Block, Function};

pub struct Cfg {
    /// Reachable blocks in reverse postorder from the entry.
    pub rpo: Vec<Block>,
    /// Position of each block in `rpo`, or `usize::MAX` when unreachable.
    pub rpo_index: Vec<usize>,
    pub preds: Vec<Vec<Block>>,
    pub succs: Vec<Vec<Block>>,
    /// Immediate dominator; the entry is its own, unreachable blocks have `None`.
    pub idom: Vec<Option<Block>>,
    /// Loop nesting depth (0 outside loops).
    pub loop_depth: Vec<u32>,
}

impl Cfg {
    pub fn new(func: &Function) -> Cfg {
        let n = func.blocks.len();
        let mut succs = vec![Vec::new(); n];
        for &b in &func.layout {
            let mut s = func.successors(b);
            s.dedup();
            let mut uniq = Vec::new();
            for x in s {
                if !uniq.contains(&x) {
                    uniq.push(x);
                }
            }
            succs[b.index()] = uniq;
        }

        // Iterative DFS for postorder.
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

        let mut preds = vec![Vec::new(); n];
        for &b in &rpo {
            for &s in &succs[b.index()] {
                preds[s.index()].push(b);
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

        let mut cfg = Cfg {
            rpo,
            rpo_index,
            preds,
            succs,
            idom,
            loop_depth: vec![0; n],
        };
        cfg.compute_loops();
        cfg
    }

    pub fn is_reachable(&self, b: Block) -> bool {
        self.rpo_index[b.index()] != usize::MAX
    }

    /// Whether `a` dominates `b` (reflexive).
    pub fn dominates(&self, a: Block, mut b: Block) -> bool {
        if !self.is_reachable(b) {
            return true;
        }
        loop {
            if a == b {
                return true;
            }
            match self.idom[b.index()] {
                Some(d) if d != b => b = d,
                _ => return false,
            }
        }
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

    fn compute_loops(&mut self) {
        // A back edge p -> h has h dominating p; the loop body is everything reaching p without
        // passing h.
        let mut depth = vec![0u32; self.idom.len()];
        for &h in &self.rpo {
            for &p in &self.preds[h.index()] {
                if !self.dominates(h, p) {
                    continue;
                }
                let mut body = vec![h];
                let mut work = vec![p];
                while let Some(x) = work.pop() {
                    if body.contains(&x) {
                        continue;
                    }
                    body.push(x);
                    work.extend(self.preds[x.index()].iter().copied());
                }
                for b in body {
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
    use crate::ir::InstData;
    let n = f.blocks.len();
    let mut cold = vec![false; n];
    for &b in &order {
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
    let entry = f.entry();
    let (mut hot, rest): (Vec<Block>, Vec<Block>) = order
        .into_iter()
        .partition(|&b| b == entry || !cold[b.index()]);
    hot.extend(rest);
    hot
}
