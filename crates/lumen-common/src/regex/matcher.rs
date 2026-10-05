//! The backtracking matcher: runs a compiled program over a [`ReInput`].

use super::captures::Captures;
use super::charclass::{Flavor, PreMap};
use super::fold::{fold_eq, is_line_terminator_u32, is_word_ic, py_is_word, CaseFold};
use super::limits::{self, BacktrackLimit};
use super::program::{Dialect, FirstFilter, Inst, Regex, Rep};
use crate::limits::Abort;

/// Backtracking budget of one `exec` (all start positions together): `STEP_BASE` plus
/// `STEP_PER_ELEM` per subject element. Linear-time patterns spend a small constant per element,
/// so they never reach it; exponential or high-polynomial backtracking does, and the exec fails
/// with [`BacktrackLimit`] instead of hanging (never with a wrong "no match").
const STEP_BASE: u64 = 1 << 22;
const STEP_PER_ELEM: u64 = 1024;
/// Backtrack memory cap (entries of `bt` + `saved`, 16 bytes each): `MEM_BASE` plus
/// `MEM_PER_ELEM` per subject element, at most `MEM_MAX` (just under 512 MiB per buffer, so a
/// buffer's power-of-two growth never reaches the next doubling before the check fires).
const MEM_BASE: usize = 1 << 22;
const MEM_PER_ELEM: usize = 16;
const MEM_MAX: usize = (1 << 25) - (1 << 20);
/// Steps between the memory/budget checks' slow path.
const CHECK_INTERVAL: u64 = 1024;

fn find_ascii_literal(
    subject: &[u8],
    start: usize,
    literal: &[u8],
    sticky: bool,
) -> Option<(usize, usize)> {
    if start > subject.len() || literal.len() > subject.len().saturating_sub(start) {
        return None;
    }
    if sticky {
        return subject[start..]
            .starts_with(literal)
            .then_some((start, start + literal.len()));
    }
    let mut from = start;
    while from + literal.len() <= subject.len() {
        let found = subject.find_byte(from, literal[0])?;
        if found + literal.len() > subject.len() {
            return None;
        }
        if subject[found..].starts_with(literal) {
            return Some((found, found + literal.len()));
        }
        from = found + 1;
    }
    None
}

/// The matcher's view of a subject: element `i` as a code point / code unit. Monomorphized for
/// bytes (an ASCII subject — the common case, matched with no `Vec<u32>` materialization at all)
/// and for wide elements (anything non-ASCII).
pub trait ReInput: Copy {
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn at(&self, i: usize) -> u32;

    /// The elements as bytes when every one is below 0x80, enabling the literal-search fast path.
    #[inline(always)]
    fn ascii_bytes(&self) -> Option<&[u8]> {
        None
    }

    #[inline]
    fn find_byte(&self, mut from: usize, byte: u8) -> Option<usize> {
        while from < self.len() {
            if self.at(from) == byte as u32 {
                return Some(from);
            }
            from += 1;
        }
        None
    }
}

impl ReInput for &[u8] {
    #[inline(always)]
    fn len(&self) -> usize {
        <[u8]>::len(self)
    }
    #[inline(always)]
    fn at(&self, i: usize) -> u32 {
        self[i] as u32
    }

    #[inline(always)]
    fn ascii_bytes(&self) -> Option<&[u8]> {
        Some(self)
    }

    #[inline]
    fn find_byte(&self, from: usize, byte: u8) -> Option<usize> {
        let bytes = self.get(from..)?;
        let repeated = u64::from_ne_bytes([byte; 8]);
        let low_bits = 0x0101_0101_0101_0101u64;
        let high_bits = 0x8080_8080_8080_8080u64;
        #[allow(clippy::chunks_exact_to_as_chunks)] // `as_chunks` needs Rust 1.88; MSRV is 1.82
        let mut chunks = bytes.chunks_exact(8);
        for (chunk_index, chunk) in chunks.by_ref().enumerate() {
            let word = u64::from_ne_bytes(chunk.try_into().unwrap());
            let different = word ^ repeated;
            if different.wrapping_sub(low_bits) & !different & high_bits != 0 {
                if let Some(offset) = chunk.iter().position(|candidate| *candidate == byte) {
                    return Some(from + chunk_index * 8 + offset);
                }
            }
        }
        let tail_start = from + bytes.len() - chunks.remainder().len();
        chunks
            .remainder()
            .iter()
            .position(|candidate| *candidate == byte)
            .map(|offset| tail_start + offset)
    }
}

impl ReInput for &[char] {
    #[inline(always)]
    fn len(&self) -> usize {
        <[char]>::len(self)
    }
    #[inline(always)]
    fn at(&self, i: usize) -> u32 {
        self[i] as u32
    }
}

impl ReInput for &[u32] {
    #[inline(always)]
    fn len(&self) -> usize {
        <[u32]>::len(self)
    }
    #[inline(always)]
    fn at(&self, i: usize) -> u32 {
        self[i]
    }
}

struct SubMatch {
    matched: bool,
    end: usize,
    at: usize,
    hi: usize,
}

struct Matcher<I: ReInput> {
    input: I,
    /// Element count the match may see: the input is treated as ending here.
    n: usize,
    caps: Vec<Option<usize>>,
    nslots: usize,
    marks: Vec<Option<usize>>,
    /// Steps spent by this `exec` across all start positions (see [`STEP_BASE`]).
    steps: u64,
    step_limit: u64,
    /// Next `steps` value at which the budget and backtrack memory are checked.
    check_at: u64,
    mem_limit: usize,
    /// Set once a budget is exceeded; every `run` then unwinds with failure.
    overflow: bool,
    /// Backtrack stack: choice points and undo records (see [`Bt`]).
    bt: Vec<Bt>,
    /// Capture snapshots referenced by [`Bt::Caps`] entries.
    saved: Vec<Option<usize>>,
    /// Choice points (`Alt`/`Greedy`/`Lazy`) currently on `bt`.
    choices: u32,
    /// Matching direction: a lookbehind body (compiled from the reversed AST) consumes leftward.
    back: bool,
    /// `(icase, multiline, dotall)` stack — the base flags, plus an entry per active `(?ims-ims:…)`
    /// inline-modifier group. Reads use the top; the group's Push/Pop instructions undo on backtrack.
    flags: Vec<(bool, bool, bool)>,
    fold: CaseFold,
    dialect: Dialect,
    /// Set while running the top-level program (not a lookaround or atomic body), where the
    /// final-position requirements below apply.
    top: bool,
    /// `FullMatch`: the match must end at `n`.
    must_end: bool,
    /// A match that is empty at this position is rejected (`usize::MAX`: none).
    reject_empty_at: usize,
    /// Position the last successful `Inst::Match` stopped at.
    match_pos: usize,
}

impl<I: ReInput> Matcher<I> {
    #[inline(always)]
    fn icase(&self) -> bool {
        self.flags.last().unwrap().0
    }
    #[inline(always)]
    fn multiline(&self) -> bool {
        self.flags.last().unwrap().1
    }
    #[inline(always)]
    fn dotall(&self) -> bool {
        self.flags.last().unwrap().2
    }
    /// Compare two subject/pattern code points under the active case rules.
    #[inline(always)]
    fn eqc_uu(&self, a: u32, b: u32) -> bool {
        a == b || (self.icase() && fold_eq(self.fold, a, b))
    }

    #[inline(always)]
    fn is_line_term(&self, c: u32) -> bool {
        match self.dialect {
            Dialect::Js => is_line_terminator_u32(c),
            Dialect::Python => c == 0x0A,
        }
    }

    #[inline(always)]
    fn at_start(&self, pos: usize) -> bool {
        pos == 0 || (self.multiline() && self.is_line_term(self.input.at(pos - 1)))
    }

    #[inline(always)]
    fn at_end(&self, pos: usize) -> bool {
        if pos == self.n {
            return true;
        }
        match self.dialect {
            Dialect::Js => self.multiline() && is_line_terminator_u32(self.input.at(pos)),
            Dialect::Python => {
                self.input.at(pos) == 0x0A && (self.multiline() || pos + 1 == self.n)
            }
        }
    }

    #[inline(always)]
    fn at_start_line(&self, pos: usize) -> bool {
        pos == 0 || self.is_line_term(self.input.at(pos - 1))
    }

    #[inline(always)]
    fn at_end_line(&self, pos: usize) -> bool {
        pos == self.n || self.is_line_term(self.input.at(pos))
    }

    fn is_word_char(&self, c: u32, flavor: Flavor) -> bool {
        match flavor {
            Flavor::Js => is_word_ic(c, self.icase(), self.fold == CaseFold::Full),
            Flavor::PyUnicode => py_is_word(c),
            Flavor::PyAscii => is_word_ic(c, false, false),
        }
    }

    /// `` (`want`) or `\B` at `pos`.
    fn word_boundary_ok(&self, pos: usize, want: bool, flavor: Flavor) -> bool {
        if self.dialect == Dialect::Python && self.n == 0 {
            // 3.14: \B matches the empty string, \b does not
            return !want;
        }
        let before = pos > 0 && self.is_word_char(self.input.at(pos - 1), flavor);
        let after = pos < self.n && self.is_word_char(self.input.at(pos), flavor);
        (before != after) == want
    }

    /// The next element to consume and the position after it, honouring the match direction.
    #[inline(always)]
    fn step(&self, pos: usize) -> Option<(u32, usize)> {
        if self.back {
            if pos > 0 {
                Some((self.input.at(pos - 1), pos - 1))
            } else {
                None
            }
        } else if pos < self.n {
            Some((self.input.at(pos), pos + 1))
        } else {
            None
        }
    }

    #[inline(always)]
    fn rep_matches(&self, rep: &Rep, c: u32) -> bool {
        match rep {
            Rep::Char(ch) => self.eqc_uu(c, *ch),
            // `.` excludes every LineTerminator (\n, \r, U+2028, U+2029) unless `s`.
            Rep::Any => self.dotall() || !self.is_line_term(c),
            Rep::Class(cc) => cc.matches(c, self.icase(), self.fold),
        }
    }

    /// Conservative viability test for the continuation at `pc` and `pos`.
    ///
    /// `Some(false)` proves that its first consuming instruction cannot match here; `Some(true)`
    /// is only a possible match, and `None` means stateful bytecode prevented a proof. Lazy
    /// quantifiers use this to skip impossible retry positions without changing match order.
    fn continuation_viable(
        &self,
        prog: &[Inst],
        mut pc: usize,
        pos: usize,
        mut budget: usize,
        mut tainted_groups: u128,
    ) -> Option<bool> {
        while budget > 0 {
            budget -= 1;
            match &prog[pc] {
                Inst::Char(expected) => {
                    return Some(
                        self.step(pos)
                            .is_some_and(|(found, _)| self.eqc_uu(found, *expected)),
                    );
                }
                Inst::Any => {
                    return Some(
                        self.step(pos)
                            .is_some_and(|(found, _)| self.dotall() || !self.is_line_term(found)),
                    );
                }
                Inst::Class(class) => {
                    return Some(
                        self.step(pos).is_some_and(|(found, _)| {
                            class.matches(found, self.icase(), self.fold)
                        }),
                    );
                }
                Inst::Many { rep, min, .. } => {
                    let matches_here = self
                        .step(pos)
                        .is_some_and(|(found, _)| self.rep_matches(rep, found));
                    if *min > 0 || matches_here {
                        return Some(matches_here);
                    }
                    pc += 1;
                }
                Inst::Backref(group) => {
                    let group = *group;
                    if group >= 128 || tainted_groups & (1u128 << group) != 0 {
                        return None;
                    }
                    if group == 0 || 2 * group + 1 >= self.caps.len() {
                        pc += 1;
                        continue;
                    }
                    match (self.caps[2 * group], self.caps[2 * group + 1]) {
                        (Some(a), Some(b)) if a != b => {
                            let first = self.input.at(a.min(b));
                            return Some(
                                self.step(pos)
                                    .is_some_and(|(found, _)| self.eqc_uu(found, first)),
                            );
                        }
                        (Some(_), Some(_)) => pc += 1,
                        _ if self.dialect == Dialect::Python => return Some(false),
                        _ => pc += 1, // an unset backreference consumes nothing
                    }
                }
                Inst::BackrefAlt(groups) => {
                    if groups
                        .iter()
                        .any(|&group| group >= 128 || tainted_groups & (1u128 << group) != 0)
                    {
                        return None;
                    }
                    let captured = groups.iter().copied().find_map(|group| {
                        match (self.caps[2 * group], self.caps[2 * group + 1]) {
                            (Some(a), Some(b)) => Some((a.min(b), a.max(b))),
                            _ => None,
                        }
                    });
                    match captured {
                        Some((a, b)) if a != b => {
                            let first = self.input.at(a);
                            return Some(
                                self.step(pos)
                                    .is_some_and(|(found, _)| self.eqc_uu(found, first)),
                            );
                        }
                        _ => pc += 1,
                    }
                }
                Inst::AssertStart => {
                    if !self.at_start(pos) {
                        return Some(false);
                    }
                    pc += 1;
                }
                Inst::AssertEnd => {
                    if !self.at_end(pos) {
                        return Some(false);
                    }
                    pc += 1;
                }
                Inst::AssertStartLine => {
                    if !self.at_start_line(pos) {
                        return Some(false);
                    }
                    pc += 1;
                }
                Inst::AssertEndLine => {
                    if !self.at_end_line(pos) {
                        return Some(false);
                    }
                    pc += 1;
                }
                Inst::SetLast(_) => pc += 1,
                Inst::AssertStartText => {
                    if pos != 0 {
                        return Some(false);
                    }
                    pc += 1;
                }
                Inst::AssertEndText => {
                    if pos != self.n {
                        return Some(false);
                    }
                    pc += 1;
                }
                Inst::WordBoundary(want, flavor) => {
                    if !self.word_boundary_ok(pos, *want, *flavor) {
                        return Some(false);
                    }
                    pc += 1;
                }
                Inst::Jmp(target) => pc = *target,
                Inst::Split(a, b) => {
                    let left = self.continuation_viable(prog, *a, pos, budget, tainted_groups);
                    let right = self.continuation_viable(prog, *b, pos, budget, tainted_groups);
                    return match (left, right) {
                        (Some(false), Some(false)) => Some(false),
                        (Some(true), _) | (_, Some(true)) => Some(true),
                        _ => None,
                    };
                }
                Inst::Match => return Some(true),
                // Capture writes do not themselves consume input. Keep walking, but remember
                // which groups have changed so a following backreference never consults stale
                // state. This is especially valuable for `.*?\k` continuations: the compiler's
                // group-end Save no longer hides the backreference's first-character filter.
                Inst::Save(slot) => {
                    let group = *slot / 2;
                    if group >= 128 {
                        return None;
                    }
                    tainted_groups |= 1u128 << group;
                    pc += 1;
                }
                Inst::ClearCaps(lo, hi) => {
                    if *hi >= 128 {
                        return None;
                    }
                    for group in *lo..=*hi {
                        tainted_groups |= 1u128 << group;
                    }
                    pc += 1;
                }
                Inst::SetMark(_) => pc += 1,
                // These affect the next predicate or can branch on mutable state.
                Inst::Look { .. }
                | Inst::LookBehind { .. }
                | Inst::LookBack { .. }
                | Inst::BackrefMapped(..)
                | Inst::Atomic(_)
                | Inst::CondGroup(..)
                | Inst::PushFlags(..)
                | Inst::PopFlags
                | Inst::CheckProgress(_)
                | Inst::ExitIfEmpty(..) => return None,
            }
        }
        None
    }

    /// Run a nested program atomically at `pos`: its choice points are dropped afterwards, and
    /// the capture snapshot taken before it stays at `saved[at..]` for the caller to settle
    /// (`keep_caps` or `restore_caps`). `None` when the match must abort.
    #[inline(always)]
    fn sub_match(&mut self, sub: &[Inst], pos: usize, back: bool) -> Option<SubMatch> {
        if crate::stack::exhausted() {
            self.overflow = true;
            return None;
        }
        let at = self.saved.len();
        let hi = self.caps.len() / 2 - 1;
        self.saved.extend_from_slice(&self.caps);
        let (sub_base, sub_floor, nflags) = (self.bt.len(), self.choices, self.flags.len());
        let saved_back = std::mem::replace(&mut self.back, back);
        let saved_top = std::mem::replace(&mut self.top, false);
        let matched = self.run(sub, 0, pos);
        self.back = saved_back;
        self.top = saved_top;
        if self.overflow {
            return None;
        }
        self.bt.truncate(sub_base);
        self.choices = sub_floor;
        self.flags.truncate(nflags);
        self.saved.truncate(at + 2 * (hi + 1));
        Some(SubMatch {
            matched,
            end: self.match_pos,
            at,
            hi,
        })
    }

    /// Keep the captures a nested match set, recording the snapshot so backtracking restores them.
    #[inline(always)]
    fn keep_caps(&mut self, r: &SubMatch, floor: u32) {
        if self.choices != floor {
            self.bt.push(Bt::Caps {
                lo: 0,
                hi: r.hi as u32,
                at: r.at as u32,
            });
        } else {
            self.saved.truncate(r.at);
        }
    }

    /// Restore `caps[2*lo..2*hi+2]` from the snapshot at `saved[at..]` and drop the snapshot.
    #[inline]
    fn restore_caps(&mut self, lo: usize, hi: usize, at: usize) {
        let n = 2 * (hi - lo + 1);
        self.caps[2 * lo..2 * hi + 2].copy_from_slice(&self.saved[at..at + n]);
        self.saved.truncate(at);
    }

    /// Slow path of the per-step check: the step budget or the backtrack memory is exhausted.
    #[cold]
    fn over_budget(&mut self) -> bool {
        self.check_at = self.steps + CHECK_INTERVAL;
        let abort = limits::poll((self.bt.len() + self.saved.len()) * 16);
        if abort != Abort::None {
            limits::record_abort(abort);
            self.overflow = true;
        }
        if self.steps > self.step_limit || self.bt.len() + self.saved.len() > self.mem_limit {
            self.overflow = true;
        }
        self.overflow
    }

    #[inline(always)]
    fn push_choice(&mut self, e: Bt) {
        self.choices += 1;
        self.bt.push(e);
    }

    /// Record an undo entry, unless this run has no choice point to backtrack to: a failure then
    /// returns from the run, and every caller resets the state itself (the scan re-initializes
    /// it, a lookaround restores its capture snapshot and flag depth).
    #[inline(always)]
    fn push_undo(&mut self, floor: u32, e: Bt) {
        if self.choices != floor {
            self.bt.push(e);
        }
    }

    /// Run `prog` from `pc` at `pos`. Backtracking state lives on the heap stack `self.bt` (no
    /// native recursion per choice point), so long subjects cost no native stack; only
    /// lookaround bodies recurse, bounded by the pattern's nesting. On success the entries this
    /// call pushed stay on `self.bt` for the caller to keep or discard; on failure `self.bt` is
    /// back at its entry length (state not covered by `push_undo` is the caller's to reset).
    fn run(&mut self, prog: &[Inst], mut pc: usize, mut pos: usize) -> bool {
        let base = self.bt.len();
        let floor = self.choices;
        'exec: loop {
            // Forward execution until the program matches or this path fails.
            loop {
                self.steps += 1;
                if self.steps >= self.check_at && self.over_budget() {
                    self.bt.truncate(base);
                    self.choices = floor;
                    return false;
                }
                match &prog[pc] {
                    Inst::Match => {
                        if self.top
                            && ((self.must_end && pos != self.n) || pos == self.reject_empty_at)
                        {
                            break;
                        }
                        self.match_pos = pos;
                        return true;
                    }
                    Inst::Char(c) => match self.step(pos) {
                        Some((e, next)) if self.eqc_uu(e, *c) => {
                            pc += 1;
                            pos = next;
                        }
                        _ => break,
                    },
                    Inst::Any => match self.step(pos) {
                        Some((e, next)) if self.dotall() || !self.is_line_term(e) => {
                            pc += 1;
                            pos = next;
                        }
                        _ => break,
                    },
                    Inst::Class(cc) => match self.step(pos) {
                        Some((e, next)) if cc.matches(e, self.icase(), self.fold) => {
                            pc += 1;
                            pos = next;
                        }
                        _ => break,
                    },
                    Inst::Save(slot) => {
                        let slot = *slot;
                        let old = pack(self.caps[slot]);
                        self.push_undo(
                            floor,
                            Bt::Cap {
                                slot: slot as u32,
                                old,
                            },
                        );
                        self.caps[slot] = Some(pos);
                        pc += 1;
                    }
                    Inst::Split(a, b) => {
                        self.push_choice(Bt::Alt {
                            pc: *b as u32,
                            pos: pos as u32,
                        });
                        pc = *a;
                    }
                    Inst::SetMark(id) => {
                        let id = *id;
                        let old = pack(self.marks[id]);
                        self.push_undo(floor, Bt::Mark { id: id as u32, old });
                        self.marks[id] = Some(pos);
                        pc += 1;
                    }
                    Inst::CheckProgress(id) => {
                        if self.marks[*id] == Some(pos) {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::ExitIfEmpty(id, exit) => {
                        pc = if self.marks[*id] == Some(pos) {
                            *exit
                        } else {
                            pc + 1
                        };
                    }
                    Inst::Many {
                        rep,
                        min,
                        max,
                        greedy,
                    } => {
                        let (min, cap) = (*min, max.unwrap_or(usize::MAX));
                        let room = if self.back { pos } else { self.n - pos };
                        if *greedy {
                            let mut avail = 0;
                            while avail < cap
                                && avail < room
                                && self.rep_matches(rep, self.input.at(self.many_idx(pos, avail)))
                            {
                                avail += 1;
                            }
                            self.steps += avail as u64;
                            if avail < min {
                                break;
                            }
                            if avail > min {
                                self.push_choice(Bt::Greedy {
                                    pc: pc as u32,
                                    pos: pos as u32,
                                    n: avail as u32,
                                });
                            }
                            pos = self.many_end(pos, avail);
                            pc += 1;
                        } else {
                            if min > room
                                || !(0..min).all(|k| {
                                    self.rep_matches(rep, self.input.at(self.many_idx(pos, k)))
                                })
                            {
                                break;
                            }
                            match self.lazy_next(prog, pc, pos, min, true) {
                                Some(n) => {
                                    self.push_choice(Bt::Lazy {
                                        pc: pc as u32,
                                        pos: pos as u32,
                                        n: n as u32,
                                    });
                                    pos = self.many_end(pos, n);
                                    pc += 1;
                                }
                                None => break,
                            }
                        }
                    }
                    Inst::PushFlags(i, m, s) => {
                        let cur = *self.flags.last().unwrap();
                        self.flags.push((
                            i.unwrap_or(cur.0),
                            m.unwrap_or(cur.1),
                            s.unwrap_or(cur.2),
                        ));
                        self.push_undo(floor, Bt::FlagsPop);
                        pc += 1;
                    }
                    Inst::PopFlags => {
                        let (i, m, s) = self.flags.pop().unwrap();
                        self.push_undo(floor, Bt::FlagsPush(i, m, s));
                        pc += 1;
                    }
                    Inst::Jmp(t) => pc = *t,
                    Inst::AssertStart => {
                        if !self.at_start(pos) {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::AssertEnd => {
                        if !self.at_end(pos) {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::AssertStartLine => {
                        if !self.at_start_line(pos) {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::AssertEndLine => {
                        if !self.at_end_line(pos) {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::SetLast(group) => {
                        let slot = self.nslots - 2;
                        let old = pack(self.caps[slot]);
                        self.push_undo(
                            floor,
                            Bt::Cap {
                                slot: slot as u32,
                                old,
                            },
                        );
                        self.caps[slot] = Some(*group);
                        pc += 1;
                    }
                    Inst::AssertStartText => {
                        if pos != 0 {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::AssertEndText => {
                        if pos != self.n {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::WordBoundary(want, flavor) => {
                        if !self.word_boundary_ok(pos, *want, *flavor) {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::CondGroup(group, otherwise) => {
                        pc = match (self.caps[2 * group], self.caps[2 * group + 1]) {
                            (Some(_), Some(_)) => pc + 1,
                            _ => *otherwise,
                        };
                    }
                    Inst::Backref(g) => {
                        let g = *g;
                        if g == 0 || 2 * g + 1 >= self.caps.len() {
                            pc += 1; // invalid group: matches empty
                            continue;
                        }
                        match (self.caps[2 * g], self.caps[2 * g + 1]) {
                            (Some(a), Some(b)) => match self.backref_end(pos, a.min(b), a.max(b)) {
                                Some(next) => {
                                    pos = next;
                                    pc += 1;
                                }
                                None => break,
                            },
                            _ if self.dialect == Dialect::Python => break,
                            _ => pc += 1, // unset group matches empty
                        }
                    }
                    Inst::BackrefMapped(g, pre) => {
                        let g = *g;
                        if g == 0 || 2 * g + 1 >= self.caps.len() {
                            pc += 1;
                            continue;
                        }
                        match (self.caps[2 * g], self.caps[2 * g + 1]) {
                            (Some(a), Some(b)) => {
                                match self.backref_end_mapped(pos, a.min(b), a.max(b), *pre) {
                                    Some(next) => {
                                        pos = next;
                                        pc += 1;
                                    }
                                    None => break,
                                }
                            }
                            _ => break,
                        }
                    }
                    Inst::BackrefAlt(idxs) => {
                        // At most one same-named group can have captured; match through that one.
                        let g = idxs.iter().copied().find(|&g| {
                            2 * g + 1 < self.caps.len()
                                && self.caps[2 * g].is_some()
                                && self.caps[2 * g + 1].is_some()
                        });
                        match g {
                            None => pc += 1, // no group captured: matches empty
                            Some(g) => {
                                let (a, b) =
                                    (self.caps[2 * g].unwrap(), self.caps[2 * g + 1].unwrap());
                                match self.backref_end(pos, a.min(b), a.max(b)) {
                                    Some(next) => {
                                        pos = next;
                                        pc += 1;
                                    }
                                    None => break,
                                }
                            }
                        }
                    }
                    Inst::ClearCaps(lo, hi) => {
                        let (lo, hi) = (*lo, *hi);
                        if self.choices != floor {
                            let at = self.saved.len();
                            self.saved.extend_from_slice(&self.caps[2 * lo..2 * hi + 2]);
                            self.bt.push(Bt::Caps {
                                lo: lo as u32,
                                hi: hi as u32,
                                at: at as u32,
                            });
                        }
                        self.caps[2 * lo..2 * hi + 2].fill(None);
                        pc += 1;
                    }
                    Inst::Look { negate, prog: sub } | Inst::LookBehind { negate, prog: sub } => {
                        let negate = *negate;
                        // A lookbehind body (compiled from the reversed tree) matches
                        // right-to-left from `pos`; a nested lookahead always matches forward.
                        let dir = matches!(prog[pc], Inst::LookBehind { .. });
                        let Some(r) = self.sub_match(sub, pos, dir) else {
                            self.bt.truncate(base);
                            self.choices = floor;
                            return false;
                        };
                        if r.matched && !negate {
                            self.keep_caps(&r, floor);
                            pc += 1;
                        } else {
                            self.restore_caps(0, r.hi, r.at);
                            if r.matched || !negate {
                                break;
                            }
                            pc += 1;
                        }
                    }
                    Inst::LookBack {
                        negate,
                        width,
                        prog: sub,
                    } => {
                        let negate = *negate;
                        if pos < *width {
                            if negate {
                                pc += 1;
                                continue;
                            }
                            break;
                        }
                        let Some(r) = self.sub_match(sub, pos - *width, false) else {
                            self.bt.truncate(base);
                            self.choices = floor;
                            return false;
                        };
                        if r.matched && !negate {
                            self.keep_caps(&r, floor);
                            pc += 1;
                        } else {
                            self.restore_caps(0, r.hi, r.at);
                            if r.matched || !negate {
                                break;
                            }
                            pc += 1;
                        }
                    }
                    Inst::Atomic(sub) => {
                        let back = self.back;
                        let Some(r) = self.sub_match(sub, pos, back) else {
                            self.bt.truncate(base);
                            self.choices = floor;
                            return false;
                        };
                        if !r.matched {
                            self.restore_caps(0, r.hi, r.at);
                            break;
                        }
                        self.keep_caps(&r, floor);
                        pos = r.end;
                        pc += 1;
                    }
                }
            }
            // Backtrack: undo state changes down to the most recent choice point and resume there.
            loop {
                if self.bt.len() == base {
                    return false;
                }
                match self.bt.pop().unwrap() {
                    Bt::Alt { pc: p, pos: q } => {
                        self.choices -= 1;
                        pc = p as usize;
                        pos = q as usize;
                        continue 'exec;
                    }
                    Bt::Cap { slot, old } => self.caps[slot as usize] = unpack(old),
                    Bt::Mark { id, old } => self.marks[id as usize] = unpack(old),
                    Bt::FlagsPop => {
                        self.flags.pop();
                    }
                    Bt::FlagsPush(i, m, s) => self.flags.push((i, m, s)),
                    Bt::Caps { lo, hi, at } => {
                        self.restore_caps(lo as usize, hi as usize, at as usize)
                    }
                    Bt::Greedy { pc: p, pos: q, n } => {
                        self.choices -= 1;
                        let (p, q) = (p as usize, q as usize);
                        let Inst::Many { min, .. } = &prog[p] else {
                            unreachable!()
                        };
                        let n = n as usize - 1;
                        if n > *min {
                            self.push_choice(Bt::Greedy {
                                pc: p as u32,
                                pos: q as u32,
                                n: n as u32,
                            });
                        }
                        pc = p + 1;
                        pos = self.many_end(q, n);
                        continue 'exec;
                    }
                    Bt::Lazy { pc: p, pos: q, n } => {
                        self.choices -= 1;
                        let (p, q) = (p as usize, q as usize);
                        if let Some(n) = self.lazy_next(prog, p, q, n as usize, false) {
                            self.push_choice(Bt::Lazy {
                                pc: p as u32,
                                pos: q as u32,
                                n: n as u32,
                            });
                            pc = p + 1;
                            pos = self.many_end(q, n);
                            continue 'exec;
                        }
                    }
                }
            }
        }
    }

    #[inline(always)]
    fn many_idx(&self, pos: usize, k: usize) -> usize {
        if self.back {
            pos - 1 - k
        } else {
            pos + k
        }
    }

    #[inline(always)]
    fn many_end(&self, pos: usize, n: usize) -> usize {
        if self.back {
            pos - n
        } else {
            pos + n
        }
    }

    /// The next count a lazy `Many` at `pc` (started at `pos`) should try: `n` itself when
    /// `include` (and viable), else the smallest viable count above it. Extends one element at a
    /// time, so a lazy run never scans further ahead than the match needs.
    fn lazy_next(
        &mut self,
        prog: &[Inst],
        pc: usize,
        pos: usize,
        mut n: usize,
        include: bool,
    ) -> Option<usize> {
        let Inst::Many { rep, max, .. } = &prog[pc] else {
            unreachable!()
        };
        let cap = max.unwrap_or(usize::MAX);
        let room = if self.back { pos } else { self.n - pos };
        let mut first = include;
        loop {
            if !first {
                if n >= cap
                    || n >= room
                    || !self.rep_matches(rep, self.input.at(self.many_idx(pos, n)))
                {
                    return None;
                }
                n += 1;
            }
            first = false;
            self.steps += 1;
            if self.continuation_viable(prog, pc + 1, self.many_end(pos, n), 16, 0) != Some(false) {
                return Some(n);
            }
        }
    }

    /// A back reference comparing characters after the transformation `pre`.
    fn backref_end_mapped(&mut self, pos: usize, a: usize, b: usize, pre: PreMap) -> Option<usize> {
        let n = b - a;
        self.steps += n as u64;
        let start = if self.back {
            pos.checked_sub(n)?
        } else {
            if pos + n > self.n {
                return None;
            }
            pos
        };
        (0..n)
            .all(|i| pre.apply(self.input.at(start + i)) == pre.apply(self.input.at(a + i)))
            .then(|| self.many_end(pos, n))
    }

    /// Match a backreference to `input[a..b]` at `pos`, returning the position after it.
    #[inline]
    fn backref_end(&mut self, pos: usize, a: usize, b: usize) -> Option<usize> {
        let n = b - a;
        self.steps += n as u64;
        let start = if self.back {
            pos.checked_sub(n)?
        } else {
            if pos + n > self.n {
                return None;
            }
            pos
        };
        (0..n)
            .all(|i| self.eqc_uu(self.input.at(start + i), self.input.at(a + i)))
            .then(|| self.many_end(pos, n))
    }
}

const NO_POS: u32 = u32::MAX;

#[inline(always)]
fn pack(p: Option<usize>) -> u32 {
    p.map_or(NO_POS, |p| p as u32)
}

#[inline(always)]
fn unpack(p: u32) -> Option<usize> {
    (p != NO_POS).then_some(p as usize)
}

/// A backtrack-stack entry: a choice point to resume at, or an undo record for state changed
/// since the previous one. Positions fit in `u32` (subjects are capped below that in
/// `exec_impl`), keeping entries at 16 bytes.
enum Bt {
    Alt {
        pc: u32,
        pos: u32,
    },
    Cap {
        slot: u32,
        old: u32,
    },
    Mark {
        id: u32,
        old: u32,
    },
    FlagsPop,
    FlagsPush(bool, bool, bool),
    /// Restore `caps[2*lo..2*hi+2]` from the snapshot at `saved[at..]`.
    Caps {
        lo: u32,
        hi: u32,
        at: u32,
    },
    /// A greedy `Many` at `pc` currently consuming `n` from `pos`; retry with one fewer.
    Greedy {
        pc: u32,
        pos: u32,
        n: u32,
    },
    /// A lazy `Many` at `pc` currently consuming `n` from `pos`; retry with more.
    Lazy {
        pc: u32,
        pos: u32,
        n: u32,
    },
}

/// How a match attempt is anchored.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Mode {
    /// Try each start position from `start` onward.
    #[default]
    Search,
    /// A single attempt at `start` (ECMAScript sticky, Python `match`).
    Match,
    /// A single attempt at `start` whose match must also end at the end of the input
    /// (Python `fullmatch`).
    FullMatch,
}

/// Where and how to match: `start` is the first position tried (assertions and lookbehinds still
/// see the text before it), and `end`, when given, makes the input end there.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExecOptions {
    pub start: usize,
    pub end: Option<usize>,
    pub mode: Mode,
    /// Reject a match that is empty at `start` (the step after an empty match in a global scan).
    pub must_advance: bool,
}

impl ExecOptions {
    pub fn search(start: usize) -> ExecOptions {
        ExecOptions {
            start,
            ..Default::default()
        }
    }

    pub fn anchored(start: usize) -> ExecOptions {
        ExecOptions {
            start,
            mode: Mode::Match,
            ..Default::default()
        }
    }

    pub fn full(start: usize) -> ExecOptions {
        ExecOptions {
            start,
            mode: Mode::FullMatch,
            ..Default::default()
        }
    }
}

/// Backtrack-buffer capacity kept across `exec` calls; a huge match's stack is released.
const SCRATCH_KEEP: usize = 4096;

/// Recycled matcher working buffers (see [`Regex::exec`]).
#[derive(Default)]
struct MatchScratch {
    caps: Vec<Option<usize>>,
    marks: Vec<Option<usize>>,
    flags: Vec<(bool, bool, bool)>,
    bt: Vec<Bt>,
    saved: Vec<Option<usize>>,
}

thread_local! {
    static MATCH_SCRATCH: std::cell::RefCell<Option<MatchScratch>> =
        const { std::cell::RefCell::new(None) };
}

impl Regex {
    /// Match `input` and return the capture spans (element indices), or `None` for no match.
    pub fn exec<I: ReInput>(
        &self,
        input: I,
        opts: ExecOptions,
    ) -> Result<Option<Captures>, BacktrackLimit> {
        let n = opts.end.map_or(input.len(), |e| e.min(input.len()));
        if opts.start > n || n >= NO_POS as usize {
            return Ok(None);
        }
        if let Some(literal) = &self.literal_ascii {
            if let (Some(bytes), true) = (
                input.ascii_bytes(),
                n == input.len() && opts.mode != Mode::FullMatch && !opts.must_advance,
            ) {
                return Ok(find_ascii_literal(
                    bytes,
                    opts.start,
                    literal,
                    opts.mode == Mode::Match,
                )
                .map(Captures::one));
            }
        }
        let search = opts.mode == Mode::Search;
        let elems = n as u64 + 1;
        // One matcher for the whole scan, its working buffers recycled across `exec` calls via a
        // thread-local (a matcher is single-threaded).
        let mut scratch = MATCH_SCRATCH
            .with(|s| s.borrow_mut().take())
            .unwrap_or_default();
        scratch.caps.clear();
        scratch.caps.resize(self.nslots, None);
        scratch.marks.clear();
        scratch.marks.resize(self.nmarks, None);
        scratch.flags.clear();
        scratch.flags.push((
            self.options.ignore_case,
            self.options.multiline,
            self.options.dotall,
        ));
        let mut m = Matcher {
            input,
            n,
            caps: scratch.caps,
            nslots: self.nslots,
            marks: scratch.marks,
            steps: 0,
            step_limit: STEP_BASE.saturating_add(STEP_PER_ELEM.saturating_mul(elems)),
            check_at: CHECK_INTERVAL,
            mem_limit: MEM_BASE
                .saturating_add(MEM_PER_ELEM.saturating_mul(elems as usize))
                .min(MEM_MAX),
            overflow: false,
            bt: scratch.bt,
            saved: scratch.saved,
            choices: 0,
            back: false,
            flags: scratch.flags,
            fold: self.options.fold,
            dialect: self.options.dialect,
            top: true,
            must_end: opts.mode == Mode::FullMatch,
            reject_empty_at: if opts.must_advance {
                opts.start
            } else {
                usize::MAX
            },
            match_pos: 0,
        };
        let mut from = opts.start;
        let result = 'scan: loop {
            if from > n {
                break 'scan Ok(None);
            }
            // Prescan: skip positions that cannot begin a match. A single-attempt mode only ever
            // tries `start`, so the filter can save at most that attempt.
            if search {
                if let Some(byte) = self.first_byte {
                    match input.find_byte(from, byte) {
                        Some(found) if found < n => from = found,
                        _ => break 'scan Ok(None),
                    }
                } else {
                    match &self.first {
                        FirstFilter::Anchored => {
                            // The start anchor can only match at position 0: one attempt at
                            // `from` decides the scan (any later position fails the assert too).
                            if from > 0 {
                                break 'scan Ok(None);
                            }
                        }
                        FirstFilter::Atoms(atoms) => {
                            // Every path consumes an element first: find the next viable one. Small
                            // elements go through the precomputed table (one load per position).
                            loop {
                                if from >= n {
                                    break 'scan Ok(None);
                                }
                                let c = input.at(from);
                                let viable = match &self.first_lut {
                                    Some(lut) if (c as usize) < 256 => lut[c as usize],
                                    _ => self.first_matches(atoms, c),
                                };
                                if viable {
                                    break;
                                }
                                from += 1;
                            }
                        }
                        FirstFilter::None => {}
                    }
                }
            }
            m.caps.fill(None);
            m.marks.fill(None);
            m.flags.truncate(1);
            m.bt.clear();
            m.saved.clear();
            m.choices = 0;
            if m.run(&self.prog, 0, from) {
                break 'scan Ok(Some(Captures::from_slots(&m.caps, self.ngroups)));
            }
            if m.overflow {
                break 'scan Err(BacktrackLimit);
            }
            if !search {
                break 'scan Ok(None);
            }
            match &self.lead_run {
                Some(rep) => {
                    m.flags.truncate(1);
                    let mut e = from;
                    while e < n && m.rep_matches(rep, input.at(e)) {
                        e += 1;
                    }
                    from = e.max(from + 1);
                }
                _ => from += 1,
            }
        };
        m.bt.clear();
        m.bt.shrink_to(SCRATCH_KEEP);
        m.saved.clear();
        m.saved.shrink_to(SCRATCH_KEEP);
        MATCH_SCRATCH.with(|s| {
            *s.borrow_mut() = Some(MatchScratch {
                caps: m.caps,
                marks: m.marks,
                flags: m.flags,
                bt: m.bt,
                saved: m.saved,
            });
        });
        result
    }

    /// Match the code points of `text` (spans are code-point indices).
    pub fn exec_str(
        &self,
        text: &str,
        opts: ExecOptions,
    ) -> Result<Option<Captures>, BacktrackLimit> {
        if text.is_ascii() {
            self.exec(text.as_bytes(), opts)
        } else {
            let cps: Vec<u32> = text.chars().map(|c| c as u32).collect();
            self.exec(&cps[..], opts)
        }
    }
}
