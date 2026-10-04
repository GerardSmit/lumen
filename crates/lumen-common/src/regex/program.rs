//! The compiled program and the [`Regex`] that owns it.

use super::charclass::{CharClass, Flavor, PreMap};
use super::compile::compile_program;
use super::fold::{fold_eq, CaseFold};
use super::ir::Node;
use std::rc::Rc;

/// Which language's matching rules the program follows where they differ: line terminators,
/// `$`, unset back references, and repeat-iteration semantics.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Dialect {
    #[default]
    Js,
    Python,
}

/// Matching options fixed when a [`Regex`] is built.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub ignore_case: bool,
    pub multiline: bool,
    pub dotall: bool,
    pub fold: CaseFold,
    pub dialect: Dialect,
}

impl Options {
    /// Python `str` pattern defaults: Unicode case folding and Python's anchors and repeats.
    pub fn python() -> Options {
        Options {
            ignore_case: false,
            multiline: false,
            dotall: false,
            fold: CaseFold::Python,
            dialect: Dialect::Python,
        }
    }
}

#[derive(Clone)]
pub(super) enum Inst {
    Char(u32),
    Any,
    Class(Rc<CharClass>),
    Save(usize),
    Split(usize, usize),
    Jmp(usize),
    Match,
    AssertStart,
    AssertEnd,
    AssertStartText,
    AssertEndText,
    AssertStartLine,
    AssertEndLine,
    WordBoundary(bool, Flavor),
    Backref(usize),
    BackrefMapped(usize, PreMap),
    /// Record `group` as the last one closed (Python dialect; stored in the slot after the
    /// last real group so backtracking undoes it like a capture).
    SetLast(usize),
    /// Matches via whichever of the groups captured (duplicate named groups).
    BackrefAlt(Rc<Vec<usize>>),
    /// Reset capture slots for groups `lo..=hi` at the start of a quantifier iteration.
    ClearCaps(usize, usize),
    Look {
        negate: bool,
        prog: Rc<Vec<Inst>>,
    },
    /// The body must match text ending at the current position.
    LookBehind {
        negate: bool,
        prog: Rc<Vec<Inst>>,
    },
    /// A fixed-width lookbehind: the body runs forward from `width` elements back.
    LookBack {
        negate: bool,
        width: usize,
        prog: Rc<Vec<Inst>>,
    },
    /// Runs the body once and continues from where it ended, discarding its alternatives.
    Atomic(Rc<Vec<Inst>>),
    /// Continue when the group has captured, else jump to the second operand.
    CondGroup(usize, usize),
    /// A repeated single-character matcher (`a*`, `\w+`, `.{2,5}`, `\p{L}+`). Consumed iteratively so
    /// a long run doesn't recurse once per character (which overflows the backtracking depth limit).
    Many {
        rep: Rep,
        min: usize,
        max: Option<usize>,
        greedy: bool,
    },
    /// Inline modifiers: push a new `(icase, multiline, dotall)` flag set for the group body
    /// (`Some` = add/remove, `None` = inherit), then `PopFlags` restores it.
    PushFlags(Option<bool>, Option<bool>, Option<bool>),
    PopFlags,
    /// `SetMark` records the position entering an optional quantifier iteration;
    /// `CheckProgress` FAILS (forcing backtracking into the body or out of the loop) when the
    /// iteration consumed nothing (the ECMAScript rule).
    SetMark(usize),
    CheckProgress(usize),
    /// The Python rule: an iteration that consumed nothing keeps its captures and leaves the
    /// loop by jumping to the second operand.
    ExitIfEmpty(usize, usize),
}

/// A single-codepoint matcher, for the `Inst::Many` fast path.
#[derive(Clone)]
pub(super) enum Rep {
    Char(u32),
    Any,
    Class(Rc<CharClass>),
}

/// What the compiled program says about how a match can begin.
pub(super) enum FirstFilter {
    /// No usable information — the scan tries every position.
    None,
    /// Every path first asserts the start of input: a match can only begin at position 0, so
    /// one attempt decides the whole scan.
    Anchored,
    /// Every path begins by consuming one element matching one of these atoms; positions whose
    /// element matches none can be skipped without entering the backtracker. The predicate is a
    /// superset of what the matcher accepts, so a pass is never wrong — only a reject is binding.
    Atoms(Vec<Rep>),
}

/// Compute the [`FirstFilter`] by ε-walking the program from its entry: through saves, jumps,
/// splits, capture clears and marks, collecting the first thing each path does. Anything not
/// modelled (assertions other than a leading start anchor, backrefs, lookarounds, inline flags,
/// or an ε-reachable `Match` — an empty-matchable pattern) disables the filter.
fn first_filter(prog: &[Inst], multiline: bool) -> FirstFilter {
    let mut atoms: Vec<Rep> = Vec::new();
    let mut asserts = 0usize;
    let mut line_sensitive = false;
    let mut stack = vec![0usize];
    let mut seen = vec![false; prog.len()];
    while let Some(pc) = stack.pop() {
        if seen[pc] {
            continue;
        }
        seen[pc] = true;
        match &prog[pc] {
            Inst::Save(_) | Inst::ClearCaps(..) | Inst::SetMark(_) => stack.push(pc + 1),
            Inst::Jmp(t) => stack.push(*t),
            Inst::Split(a, b) => {
                stack.push(*a);
                stack.push(*b);
            }
            Inst::Char(c) => atoms.push(Rep::Char(*c)),
            Inst::Any => atoms.push(Rep::Any),
            Inst::Class(cc) => atoms.push(Rep::Class(cc.clone())),
            Inst::Many { rep, min, .. } => {
                atoms.push(rep.clone());
                if *min == 0 {
                    stack.push(pc + 1); // may consume nothing — the next inst also "begins" a path
                }
            }
            Inst::AssertStart => {
                asserts += 1;
                line_sensitive |= multiline;
            }
            Inst::AssertStartText => asserts += 1,
            _ => return FirstFilter::None,
        }
    }
    if asserts > 0 {
        if atoms.is_empty() && !line_sensitive {
            FirstFilter::Anchored
        } else {
            FirstFilter::None
        }
    } else if !atoms.is_empty() {
        FirstFilter::Atoms(atoms)
    } else {
        FirstFilter::None
    }
}

/// A compiled regular expression.
pub struct Regex {
    pub(super) prog: Vec<Inst>,
    pub(super) nmarks: usize,
    /// Start-position prescan derived from the program (see [`first_filter`]): lets the scan
    /// skip positions that cannot begin a match instead of running the backtracker at each.
    pub(super) first: FirstFilter,
    /// Exact leading ASCII byte, when the first-set proof has only one case-sensitive literal.
    /// The byte input scans this eight bytes at a time before entering the backtracker.
    pub(super) first_byte: Option<u8>,
    /// Capture-free, case-sensitive ASCII literal program. Searching it directly is equivalent
    /// to executing `Save(0), Char*, Save(1), Match`, without paying the backtracking VM dispatch.
    pub(super) literal_ascii: Option<Box<[u8]>>,
    /// [`FirstFilter::Atoms`] baked into a byte-indexed table (elements < 256): the scan loop
    /// becomes one load per position.
    pub(super) first_lut: Option<Box<[bool; 256]>>,
    /// The program opens with an unbounded greedy `C*` / `C{m,}` outside every group. A
    /// failed attempt at `s` then already tried every continuation an attempt at `s' ∈ (s, e)`
    /// could reach (`e` = end of the `C` run at `s`): the run's end choices from `s'` are a subset
    /// of those from `s`, and the continuation depends only on its position and its own
    /// captures, never on group 0's start. So the scan resumes at `e`, keeping `\w+@x`-style
    /// scans linear instead of quadratic per run.
    pub(super) lead_run: Option<Rep>,
    pub(super) options: Options,
    pub ngroups: usize,
    /// Capture slots the matcher allocates: two per group (group 0 included), plus two more in
    /// the Python dialect for the last-closed-group record.
    pub(super) nslots: usize,
    /// Named groups paired with their capture index.
    pub names: Vec<(String, usize)>,
}

impl Regex {
    /// Compile `node`, whose capture groups are numbered `1..=ngroups`.
    pub fn build(
        node: &Node,
        ngroups: usize,
        names: Vec<(String, usize)>,
        options: Options,
    ) -> Result<Regex, String> {
        let (prog, nmarks) = compile_program(node, ngroups, options.dialect)?;
        let first = first_filter(&prog, options.multiline);
        let first_byte = match &first {
            FirstFilter::Atoms(atoms) if !options.ignore_case && atoms.len() == 1 => {
                match &atoms[0] {
                    Rep::Char(c) if *c < 0x80 => Some(*c as u8),
                    _ => None,
                }
            }
            _ => None,
        };
        let literal_ascii = if !options.ignore_case && ngroups == 0 {
            literal_of(&prog)
        } else {
            None
        };
        let lead_run = match prog.get(1) {
            Some(Inst::Many {
                rep,
                max: None,
                greedy: true,
                ..
            }) => Some(rep.clone()),
            _ => None,
        };
        let mut re = Regex {
            nmarks,
            first,
            first_byte,
            literal_ascii,
            first_lut: None,
            lead_run,
            prog,
            options,
            ngroups,
            nslots: 2 * (ngroups + 1)
                + if options.dialect == Dialect::Python {
                    2
                } else {
                    0
                },
            names,
        };
        if let FirstFilter::Atoms(atoms) = &re.first {
            let mut lut = Box::new([false; 256]);
            for (c, slot) in lut.iter_mut().enumerate() {
                *slot = re.first_matches(atoms, c as u32);
            }
            re.first_lut = Some(lut);
        }
        Ok(re)
    }

    pub fn options(&self) -> &Options {
        &self.options
    }

    /// Whether a match could begin with element `c` — the [`FirstFilter::Atoms`] predicate.
    /// Deliberately a superset of what the matcher accepts (e.g. `Any` only excludes `\n`), so a
    /// pass costs a wasted attempt at worst; only a reject skips work.
    pub(super) fn first_matches(&self, atoms: &[Rep], c: u32) -> bool {
        let o = &self.options;
        atoms.iter().any(|rep| match rep {
            Rep::Char(ch) => *ch == c || (o.ignore_case && fold_eq(o.fold, c, *ch)),
            Rep::Any => o.dotall || c != '\n' as u32,
            Rep::Class(cc) => cc.matches(c, o.ignore_case, o.fold),
        })
    }
}

fn literal_of(prog: &[Inst]) -> Option<Box<[u8]>> {
    if prog.len() >= 4
        && matches!(prog.first(), Some(Inst::Save(0)))
        && matches!(prog.get(prog.len() - 2), Some(Inst::Save(1)))
        && matches!(prog.last(), Some(Inst::Match))
    {
        prog[1..prog.len() - 2]
            .iter()
            .map(|inst| match inst {
                Inst::Char(c) if *c < 0x80 => Some(*c as u8),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
    } else {
        None
    }
    .filter(|literal| !literal.is_empty())
    .map(Vec::into_boxed_slice)
}
