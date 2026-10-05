//! The language-neutral core of a CPython-style cycle collector over reference-counted objects:
//! finding the nodes of a candidate set that only other nodes of the set keep alive, and the
//! generational policy that decides when and what to collect.
//!
//! An engine describes its candidate set as a [`Graph`] (reference counts and edges, no
//! pointers), asks [`unreachable_mask`] which nodes are cyclic garbage, then runs its own
//! finalizers and breaks the cycles. No OS calls and no allocation beyond `Vec`, so the module can
//! become `no_std + alloc`.

/// A candidate set of reference-counted nodes, numbered `0..len()`.
pub trait Graph {
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Strong references held on node `n` by anything (holders inside and outside the set).
    fn strong(&self, n: usize) -> usize;

    /// Calls `f` once per strong reference node `n` holds on a node of the set. Reporting a
    /// reference that is not a real strong one makes the target look externally held, so
    /// over-reporting only ever leaks.
    fn edges(&self, n: usize, f: &mut dyn FnMut(usize));
}

/// For every node, whether something outside the set (or reachable from such a node) holds it.
/// `false` marks cyclic garbage: nodes referenced only from within unreachable parts of the set.
pub fn reachable_mask<G: Graph + ?Sized>(g: &G) -> Vec<bool> {
    let n = g.len();
    let mut external: Vec<isize> = (0..n).map(|i| g.strong(i) as isize).collect();
    for i in 0..n {
        g.edges(i, &mut |j| external[j] -= 1);
    }
    let mut reachable = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    for (i, e) in external.iter().enumerate() {
        if *e != 0 {
            reachable[i] = true;
            stack.push(i);
        }
    }
    while let Some(i) = stack.pop() {
        g.edges(i, &mut |j| {
            if !reachable[j] {
                reachable[j] = true;
                stack.push(j);
            }
        });
    }
    reachable
}

/// The complement of [`reachable_mask`]: `true` for the nodes that are garbage.
pub fn unreachable_mask<G: Graph + ?Sized>(g: &G) -> Vec<bool> {
    let mut m = reachable_mask(g);
    for b in m.iter_mut() {
        *b = !*b;
    }
    m
}

/// Number of generations (CPython's `NUM_GENERATIONS`).
pub const GENERATIONS: usize = 3;

/// What a collection of one generation found, kept per generation (`gc.get_stats()`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GenStats {
    pub collections: usize,
    pub collected: usize,
    pub uncollectable: usize,
}

/// CPython's generational schedule: allocation counts, thresholds and the long-lived-object
/// heuristic that keeps full collections from going quadratic.
#[derive(Clone, Debug)]
pub struct Generations {
    /// Generation 0: allocations minus deallocations since its last collection; the older ones:
    /// collections of the generation below since their own last collection.
    pub count: [usize; GENERATIONS],
    pub threshold: [usize; GENERATIONS],
    pub stats: [GenStats; GENERATIONS],
    long_lived_pending: usize,
    long_lived_total: usize,
}

impl Default for Generations {
    fn default() -> Self {
        Generations::new()
    }
}

impl Generations {
    pub const fn new() -> Generations {
        Generations {
            count: [0; GENERATIONS],
            threshold: [700, 10, 10],
            stats: [GenStats { collections: 0, collected: 0, uncollectable: 0 }; GENERATIONS],
            long_lived_pending: 0,
            long_lived_total: 0,
        }
    }

    /// A container was allocated; true when generation 0 is over its threshold.
    #[inline]
    pub fn allocated(&mut self) -> bool {
        self.count[0] += 1;
        self.count[0] > self.threshold[0]
    }

    /// A container was freed.
    #[inline]
    pub fn freed(&mut self) {
        if self.count[0] > 0 {
            self.count[0] -= 1;
        }
    }

    /// The oldest generation whose count passed its threshold: that generation and every
    /// younger one are collected together. The oldest is skipped until enough long-lived objects
    /// accumulated since the last full collection.
    pub fn due(&self) -> Option<usize> {
        for i in (0..GENERATIONS).rev() {
            if self.count[i] > self.threshold[i] {
                if i == GENERATIONS - 1 && self.long_lived_pending < self.long_lived_total / 4 {
                    continue;
                }
                return Some(i);
            }
        }
        None
    }

    /// Counter bookkeeping at the start of collecting generation `g`.
    pub fn begin(&mut self, g: usize) {
        if g + 1 < GENERATIONS {
            self.count[g + 1] += 1;
        }
        for c in self.count.iter_mut().take(g + 1) {
            *c = 0;
        }
    }

    /// Bookkeeping at the end of collecting `g`: `survivors` objects stay tracked.
    pub fn end(&mut self, g: usize, survivors: usize, collected: usize, uncollectable: usize) {
        if g == GENERATIONS - 2 {
            self.long_lived_pending += survivors;
        } else if g == GENERATIONS - 1 {
            self.long_lived_pending = 0;
            self.long_lived_total = survivors;
        }
        let s = &mut self.stats[g];
        s.collections += 1;
        s.collected += collected;
        s.uncollectable += uncollectable;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Edges {
        strong: Vec<usize>,
        edges: Vec<Vec<usize>>,
    }

    impl Graph for Edges {
        fn len(&self) -> usize {
            self.strong.len()
        }
        fn strong(&self, n: usize) -> usize {
            self.strong[n]
        }
        fn edges(&self, n: usize, f: &mut dyn FnMut(usize)) {
            for &t in &self.edges[n] {
                f(t);
            }
        }
    }

    #[test]
    fn cycle_with_no_outside_holder_is_garbage() {
        // 0 <-> 1 held only by each other; 2 held from outside and holding 3; 3 held only by 2.
        let g = Edges { strong: vec![1, 1, 1, 1], edges: vec![vec![1], vec![0], vec![3], vec![]] };
        assert_eq!(unreachable_mask(&g), vec![true, true, false, false]);
    }

    #[test]
    fn tail_of_garbage_is_garbage() {
        let g = Edges { strong: vec![1, 1, 1], edges: vec![vec![1], vec![0, 2], vec![]] };
        assert_eq!(unreachable_mask(&g), vec![true, true, true]);
    }

    #[test]
    fn outside_holder_keeps_the_whole_cycle() {
        let g = Edges { strong: vec![2, 1], edges: vec![vec![1], vec![0]] };
        assert_eq!(unreachable_mask(&g), vec![false, false]);
    }

    #[test]
    fn schedule_follows_thresholds() {
        let mut g = Generations::new();
        g.count = [701, 0, 0];
        assert_eq!(g.due(), Some(0));
        g.begin(0);
        assert_eq!(g.count, [0, 1, 0]);
        g.count = [0, 11, 0];
        assert_eq!(g.due(), Some(1));
    }
}
