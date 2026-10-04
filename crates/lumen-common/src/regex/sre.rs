//! Front end for CPython's `sre` bytecode: translates the opcode array that `re._compiler`
//! produces (the `code` argument of `_sre.compile`) into the shared [`Node`] tree, so Python's
//! `re` runs on the same matcher as the JavaScript engine.
//!
//! Case-insensitive ops carry their own transformation ([`PreMap`]): `IN_UNI_IGNORE` tests the
//! lowercased subject character against a set that was lowercased when it was compiled, so the
//! global `ignore_case` option is never used.

use super::charclass::{Builtin, BuiltinSet, CharClass, Flavor, PreMap};
use super::ir::Node;
use super::program::{Options, Regex};

pub const MAGIC: u32 = 20221023;
pub const MAXREPEAT: u32 = u32::MAX;
pub const MAXGROUPS: u32 = (i32::MAX / 2) as u32;

const FAILURE: u32 = 0;
const SUCCESS: u32 = 1;
const ANY: u32 = 2;
const ANY_ALL: u32 = 3;
const ASSERT: u32 = 4;
const ASSERT_NOT: u32 = 5;
const AT: u32 = 6;
const BRANCH: u32 = 7;
const CATEGORY: u32 = 8;
const CHARSET: u32 = 9;
const BIGCHARSET: u32 = 10;
const GROUPREF: u32 = 11;
const GROUPREF_EXISTS: u32 = 12;
const IN: u32 = 13;
const INFO: u32 = 14;
const JUMP: u32 = 15;
const LITERAL: u32 = 16;
const MARK: u32 = 17;
const MAX_UNTIL: u32 = 18;
const MIN_UNTIL: u32 = 19;
const NOT_LITERAL: u32 = 20;
const NEGATE: u32 = 21;
const RANGE: u32 = 22;
const REPEAT: u32 = 23;
const REPEAT_ONE: u32 = 24;
const MIN_REPEAT_ONE: u32 = 26;
const ATOMIC_GROUP: u32 = 27;
const POSSESSIVE_REPEAT: u32 = 28;
const POSSESSIVE_REPEAT_ONE: u32 = 29;
const GROUPREF_IGNORE: u32 = 30;
const IN_IGNORE: u32 = 31;
const LITERAL_IGNORE: u32 = 32;
const NOT_LITERAL_IGNORE: u32 = 33;
const GROUPREF_LOC_IGNORE: u32 = 34;
const IN_LOC_IGNORE: u32 = 35;
const LITERAL_LOC_IGNORE: u32 = 36;
const NOT_LITERAL_LOC_IGNORE: u32 = 37;
const GROUPREF_UNI_IGNORE: u32 = 38;
const IN_UNI_IGNORE: u32 = 39;
const LITERAL_UNI_IGNORE: u32 = 40;
const NOT_LITERAL_UNI_IGNORE: u32 = 41;
const RANGE_UNI_IGNORE: u32 = 42;

const AT_BEGINNING: u32 = 0;
const AT_BEGINNING_LINE: u32 = 1;
const AT_BEGINNING_STRING: u32 = 2;
const AT_BOUNDARY: u32 = 3;
const AT_NON_BOUNDARY: u32 = 4;
const AT_END: u32 = 5;
const AT_END_LINE: u32 = 6;
const AT_END_STRING: u32 = 7;
const AT_LOC_BOUNDARY: u32 = 8;
const AT_LOC_NON_BOUNDARY: u32 = 9;
const AT_UNI_BOUNDARY: u32 = 10;
const AT_UNI_NON_BOUNDARY: u32 = 11;

/// Why a code array could not be turned into a [`Regex`].
#[derive(Debug, PartialEq, Eq)]
pub enum SreError {
    /// The array is not well-formed `sre` code.
    Invalid,
    /// The program exceeds a limit of the shared engine.
    Limit(String),
}

type Res<T> = Result<T, SreError>;

/// Build a [`Regex`] from `sre` code for a pattern with `groups` capture groups.
pub fn build(code: &[u32], groups: usize) -> Res<Regex> {
    let mut decoder = Decoder { code, groups };
    let (node, _) = decoder.seq(0, code.len(), true)?;
    if code.last() != Some(&SUCCESS) {
        return Err(SreError::Invalid);
    }
    Regex::build(&node, groups, Vec::new(), Options::python()).map_err(SreError::Limit)
}

struct Decoder<'a> {
    code: &'a [u32],
    groups: usize,
}

struct Frame {
    group: usize,
    items: Vec<Node>,
}

fn node_of(mut items: Vec<Node>) -> Node {
    match items.len() {
        0 => Node::Empty,
        1 => items.pop().unwrap(),
        _ => Node::Concat(items),
    }
}

fn never() -> Node {
    Node::Class(CharClass::new())
}

fn category_ranges(category: u32) -> Option<Vec<(u32, u32)>> {
    let (set, negated): (&[u32], bool) = match category {
        6 => (&[0x0A], false),
        7 => (&[0x0A], true),
        16 => (
            &[
                0x0A, 0x0B, 0x0C, 0x0D, 0x1C, 0x1D, 0x1E, 0x85, 0x2028, 0x2029,
            ],
            false,
        ),
        17 => (
            &[
                0x0A, 0x0B, 0x0C, 0x0D, 0x1C, 0x1D, 0x1E, 0x85, 0x2028, 0x2029,
            ],
            true,
        ),
        _ => return None,
    };
    let mut sorted = set.to_vec();
    sorted.sort_unstable();
    let mut ranges = Vec::new();
    let mut next = 0u32;
    if negated {
        for &c in &sorted {
            if c > next {
                ranges.push((next, c - 1));
            }
            next = c + 1;
        }
        ranges.push((next, 0x10FFFF));
    } else {
        for &c in &sorted {
            match ranges.last_mut() {
                Some((_, hi)) if *hi + 1 == c => *hi = c,
                _ => ranges.push((c, c)),
            }
        }
    }
    Some(ranges)
}

fn category_builtin(category: u32) -> Option<Builtin> {
    use BuiltinSet::*;
    use Flavor::*;
    Some(match category {
        0 => Builtin::new(Digit, false, PyAscii),
        1 => Builtin::new(Digit, true, PyAscii),
        2 => Builtin::new(Space, false, PyAscii),
        3 => Builtin::new(Space, true, PyAscii),
        4 | 8 => Builtin::new(Word, false, PyAscii),
        5 | 9 => Builtin::new(Word, true, PyAscii),
        10 => Builtin::new(Digit, false, PyUnicode),
        11 => Builtin::new(Digit, true, PyUnicode),
        12 => Builtin::new(Space, false, PyUnicode),
        13 => Builtin::new(Space, true, PyUnicode),
        14 => Builtin::new(Word, false, PyUnicode),
        15 => Builtin::new(Word, true, PyUnicode),
        _ => return None,
    })
}

fn push_runs(class: &mut CharClass, bits: impl Iterator<Item = (u32, bool)>) {
    let mut run: Option<(u32, u32)> = None;
    for (ch, on) in bits {
        match (&mut run, on) {
            (Some((_, hi)), true) if *hi + 1 == ch => *hi = ch,
            (_, true) => {
                if let Some((lo, hi)) = run.take() {
                    class.ranges.push((lo, hi));
                }
                run = Some((ch, ch));
            }
            _ => {}
        }
    }
    if let Some((lo, hi)) = run {
        class.ranges.push((lo, hi));
    }
}

impl Decoder<'_> {
    fn word(&self, at: usize) -> Res<u32> {
        self.code.get(at).copied().ok_or(SreError::Invalid)
    }

    fn at_most(&self, at: usize, limit: usize) -> Res<usize> {
        if at <= limit && limit <= self.code.len() {
            Ok(at)
        } else {
            Err(SreError::Invalid)
        }
    }

    fn group_ref(&self, arg: u32) -> Res<usize> {
        let group = arg as usize + 1;
        if group > self.groups {
            return Err(SreError::Invalid);
        }
        Ok(group)
    }

    fn ignore_class(&self, pre: Option<PreMap>, literal: u32, negate: bool) -> Node {
        let mut class = CharClass::new().with_char(literal).negated(negate);
        if let Some(pre) = pre {
            class = class.with_pre(pre);
        }
        Node::Class(class)
    }

    fn charset(&self, at: usize, end: usize, pre: Option<PreMap>) -> Res<Node> {
        let mut class = CharClass::new();
        let mut p = at;
        loop {
            match self.word(p)? {
                FAILURE => {
                    p += 1;
                    break;
                }
                NEGATE => {
                    if p != at {
                        return Err(SreError::Invalid);
                    }
                    class.negate = true;
                    p += 1;
                }
                LITERAL => {
                    let c = self.word(p + 1)?;
                    class.ranges.push((c, c));
                    p += 2;
                }
                RANGE => {
                    let (lo, hi) = (self.word(p + 1)?, self.word(p + 2)?);
                    class.ranges.push((lo, hi));
                    p += 3;
                }
                RANGE_UNI_IGNORE => {
                    let (lo, hi) = (self.word(p + 1)?, self.word(p + 2)?);
                    class.upper_ranges.push((lo, hi));
                    p += 3;
                }
                CATEGORY => {
                    let c = self.word(p + 1)?;
                    if let Some(b) = category_builtin(c) {
                        class.builtins.push(b);
                    } else if let Some(ranges) = category_ranges(c) {
                        class.ranges.extend(ranges);
                    } else {
                        return Err(SreError::Invalid);
                    }
                    p += 2;
                }
                CHARSET => {
                    let words = self.code.get(p + 1..p + 9).ok_or(SreError::Invalid)?;
                    push_runs(
                        &mut class,
                        (0..256u32).map(|ch| (ch, words[(ch / 32) as usize] >> (ch % 32) & 1 != 0)),
                    );
                    p += 9;
                }
                BIGCHARSET => {
                    let count = self.word(p + 1)? as usize;
                    let index_words = self.code.get(p + 2..p + 66).ok_or(SreError::Invalid)?;
                    let blocks_at = p + 66;
                    let blocks = self
                        .code
                        .get(blocks_at..blocks_at + count * 8)
                        .ok_or(SreError::Invalid)?;
                    let mut indices = [0u8; 256];
                    for (i, w) in index_words.iter().enumerate() {
                        indices[4 * i..4 * i + 4].copy_from_slice(&w.to_ne_bytes());
                    }
                    for (hi, &block) in indices.iter().enumerate() {
                        let block = block as usize;
                        if block >= count {
                            return Err(SreError::Invalid);
                        }
                        let words = &blocks[block * 8..block * 8 + 8];
                        push_runs(
                            &mut class,
                            (0..256u32).map(|lo| {
                                (
                                    (hi as u32) << 8 | lo,
                                    words[(lo / 32) as usize] >> (lo % 32) & 1 != 0,
                                )
                            }),
                        );
                    }
                    p += 66 + count * 8;
                }
                _ => return Err(SreError::Invalid),
            }
            if p > end {
                return Err(SreError::Invalid);
            }
        }
        if p != end {
            return Err(SreError::Invalid);
        }
        if let Some(pre) = pre {
            class.pre = Some(pre);
        }
        Ok(Node::Class(class))
    }

    /// Decode ops in `from..to`. With `stop_at_jump`, a top-level `JUMP` ends the sequence and
    /// its position is returned (the `GROUPREF_EXISTS` yes/no separator); otherwise the second
    /// element is `to`. At the top level a trailing `SUCCESS` ends the program.
    fn seq(&mut self, from: usize, to: usize, top: bool) -> Res<(Node, usize)> {
        self.seq_inner(from, to, top, false)
    }

    fn seq_inner(
        &mut self,
        from: usize,
        to: usize,
        top: bool,
        stop_at_jump: bool,
    ) -> Res<(Node, usize)> {
        if crate::stack::exhausted() {
            return Err(SreError::Limit(super::NEST_ERROR.into()));
        }
        if to > self.code.len() {
            return Err(SreError::Invalid);
        }
        let mut frames: Vec<Frame> = vec![Frame {
            group: 0,
            items: Vec::new(),
        }];
        let mut p = from;
        while p < to {
            let op = self.code[p];
            let items = &mut frames.last_mut().unwrap().items;
            match op {
                SUCCESS if top && p + 1 == to => {
                    p += 1;
                }
                FAILURE => {
                    items.push(never());
                    p += 1;
                }
                INFO => {
                    let skip = self.word(p + 1)? as usize;
                    p = self.at_most(p + 1 + skip, to)?;
                }
                JUMP if stop_at_jump => {
                    if frames.len() != 1 {
                        return Err(SreError::Invalid);
                    }
                    return Ok((node_of(frames.pop().unwrap().items), p));
                }
                LITERAL => {
                    items.push(Node::Char(self.word(p + 1)?));
                    p += 2;
                }
                NOT_LITERAL => {
                    items.push(self.ignore_class(None, self.word(p + 1)?, true));
                    p += 2;
                }
                LITERAL_IGNORE
                | LITERAL_UNI_IGNORE
                | LITERAL_LOC_IGNORE
                | NOT_LITERAL_IGNORE
                | NOT_LITERAL_UNI_IGNORE
                | NOT_LITERAL_LOC_IGNORE => {
                    let pre = match op {
                        LITERAL_IGNORE | NOT_LITERAL_IGNORE => PreMap::AsciiLower,
                        LITERAL_UNI_IGNORE | NOT_LITERAL_UNI_IGNORE => PreMap::PyLower,
                        _ => PreMap::AsciiEither,
                    };
                    let negate = matches!(
                        op,
                        NOT_LITERAL_IGNORE | NOT_LITERAL_UNI_IGNORE | NOT_LITERAL_LOC_IGNORE
                    );
                    items.push(self.ignore_class(Some(pre), self.word(p + 1)?, negate));
                    p += 2;
                }
                ANY => {
                    items.push(Node::Any);
                    p += 1;
                }
                ANY_ALL => {
                    items.push(Node::Class(CharClass::new().negated(true)));
                    p += 1;
                }
                IN | IN_IGNORE | IN_UNI_IGNORE | IN_LOC_IGNORE => {
                    let skip = self.word(p + 1)? as usize;
                    let next = self.at_most(p + 1 + skip, to)?;
                    let pre = match op {
                        IN => None,
                        IN_IGNORE => Some(PreMap::AsciiLower),
                        IN_UNI_IGNORE => Some(PreMap::PyLower),
                        _ => Some(PreMap::AsciiEither),
                    };
                    let node = self.charset(p + 2, next, pre)?;
                    frames.last_mut().unwrap().items.push(node);
                    p = next;
                }
                CATEGORY => {
                    let c = self.word(p + 1)?;
                    let class = if let Some(b) = category_builtin(c) {
                        CharClass::new().with_builtin(b)
                    } else if let Some(ranges) = category_ranges(c) {
                        let mut class = CharClass::new();
                        class.ranges = ranges;
                        class
                    } else {
                        return Err(SreError::Invalid);
                    };
                    items.push(Node::Class(class));
                    p += 2;
                }
                AT => {
                    items.push(match self.word(p + 1)? {
                        AT_BEGINNING => Node::Start,
                        AT_BEGINNING_LINE => Node::StartLine,
                        AT_BEGINNING_STRING => Node::StartText,
                        AT_BOUNDARY | AT_LOC_BOUNDARY => Node::WordB(true, Flavor::PyAscii),
                        AT_NON_BOUNDARY | AT_LOC_NON_BOUNDARY => {
                            Node::WordB(false, Flavor::PyAscii)
                        }
                        AT_END => Node::End,
                        AT_END_LINE => Node::EndLine,
                        AT_END_STRING => Node::EndText,
                        AT_UNI_BOUNDARY => Node::WordB(true, Flavor::PyUnicode),
                        AT_UNI_NON_BOUNDARY => Node::WordB(false, Flavor::PyUnicode),
                        _ => return Err(SreError::Invalid),
                    });
                    p += 2;
                }
                MARK => {
                    let mark = self.word(p + 1)? as usize;
                    let group = mark / 2 + 1;
                    if group > self.groups {
                        return Err(SreError::Invalid);
                    }
                    if mark & 1 == 0 {
                        frames.push(Frame {
                            group,
                            items: Vec::new(),
                        });
                    } else {
                        let frame = frames.pop().unwrap();
                        if frame.group != group || frames.is_empty() {
                            return Err(SreError::Invalid);
                        }
                        frames
                            .last_mut()
                            .unwrap()
                            .items
                            .push(Node::capture(group, node_of(frame.items)));
                    }
                    p += 2;
                }
                BRANCH => {
                    let mut alts = Vec::new();
                    let mut q = p + 1;
                    loop {
                        let skip = self.word(q)? as usize;
                        if skip == 0 {
                            q += 1;
                            break;
                        }
                        if skip < 3 {
                            return Err(SreError::Invalid);
                        }
                        let end = self.at_most(q + skip, to)?;
                        if self.word(end - 2)? != JUMP {
                            return Err(SreError::Invalid);
                        }
                        let (node, _) = self.seq_inner(q + 1, end - 2, false, false)?;
                        alts.push(node);
                        q = end;
                    }
                    self.at_most(q, to)?;
                    frames.last_mut().unwrap().items.push(Node::Alt(alts));
                    p = q;
                }
                REPEAT_ONE | MIN_REPEAT_ONE | POSSESSIVE_REPEAT_ONE => {
                    let skip = self.word(p + 1)? as usize;
                    let (min, max) = (self.word(p + 2)?, self.word(p + 3)?);
                    let next = self.at_most(p + 1 + skip, to)?;
                    if next < p + 5 || self.word(next - 1)? != SUCCESS {
                        return Err(SreError::Invalid);
                    }
                    let (body, _) = self.seq_inner(p + 4, next - 1, false, false)?;
                    let node = repeat_node(op, body, min, max);
                    frames.last_mut().unwrap().items.push(node);
                    p = next;
                }
                REPEAT | POSSESSIVE_REPEAT => {
                    let skip = self.word(p + 1)? as usize;
                    let (min, max) = (self.word(p + 2)?, self.word(p + 3)?);
                    let until = self.at_most(p + 1 + skip, to)?;
                    let closer = self.word(until)?;
                    let ok = if op == REPEAT {
                        closer == MAX_UNTIL || closer == MIN_UNTIL
                    } else {
                        closer == SUCCESS
                    };
                    if !ok {
                        return Err(SreError::Invalid);
                    }
                    let (body, _) = self.seq_inner(p + 4, until, false, false)?;
                    let kind = if op == POSSESSIVE_REPEAT {
                        POSSESSIVE_REPEAT_ONE
                    } else if closer == MAX_UNTIL {
                        REPEAT_ONE
                    } else {
                        MIN_REPEAT_ONE
                    };
                    let node = repeat_node(kind, body, min, max);
                    frames.last_mut().unwrap().items.push(node);
                    p = until + 1;
                }
                ATOMIC_GROUP => {
                    let skip = self.word(p + 1)? as usize;
                    let next = self.at_most(p + 1 + skip, to)?;
                    if next < p + 3 || self.word(next - 1)? != SUCCESS {
                        return Err(SreError::Invalid);
                    }
                    let (body, _) = self.seq_inner(p + 2, next - 1, false, false)?;
                    frames.last_mut().unwrap().items.push(Node::atomic(body));
                    p = next;
                }
                ASSERT | ASSERT_NOT => {
                    let skip = self.word(p + 1)? as usize;
                    let back = self.word(p + 2)? as usize;
                    let next = self.at_most(p + 1 + skip, to)?;
                    if next < p + 4 || self.word(next - 1)? != SUCCESS {
                        return Err(SreError::Invalid);
                    }
                    let (body, _) = self.seq_inner(p + 3, next - 1, false, false)?;
                    let negate = op == ASSERT_NOT;
                    let node = if back == 0 {
                        Node::lookahead(negate, body)
                    } else {
                        Node::LookBehindFixed {
                            negate,
                            width: back,
                            body: Box::new(body),
                        }
                    };
                    frames.last_mut().unwrap().items.push(node);
                    p = next;
                }
                GROUPREF => {
                    let g = self.group_ref(self.word(p + 1)?)?;
                    items.push(Node::Backref(g));
                    p += 2;
                }
                GROUPREF_IGNORE | GROUPREF_LOC_IGNORE | GROUPREF_UNI_IGNORE => {
                    let g = self.group_ref(self.word(p + 1)?)?;
                    let pre = if op == GROUPREF_UNI_IGNORE {
                        PreMap::PyLower
                    } else {
                        PreMap::AsciiLower
                    };
                    items.push(Node::BackrefMapped(g, pre));
                    p += 2;
                }
                GROUPREF_EXISTS => {
                    let g = self.group_ref(self.word(p + 1)?)?;
                    let skip = self.word(p + 2)? as usize;
                    let target = self.at_most(p + 1 + skip, to)?;
                    let (yes, stop) = self.seq_inner(p + 3, target, false, true)?;
                    let (no, end) = if stop < target && self.word(stop)? == JUMP {
                        let off = self.word(stop + 1)? as usize;
                        if stop + 2 != target {
                            return Err(SreError::Invalid);
                        }
                        let end = self.at_most(stop + 1 + off, to)?;
                        let (no, _) = self.seq_inner(target, end, false, false)?;
                        (no, end)
                    } else {
                        (Node::Empty, target)
                    };
                    frames
                        .last_mut()
                        .unwrap()
                        .items
                        .push(Node::conditional(g, yes, no));
                    p = end;
                }
                _ => return Err(SreError::Invalid),
            }
        }
        if p != to || frames.len() != 1 {
            return Err(SreError::Invalid);
        }
        Ok((node_of(frames.pop().unwrap().items), to))
    }
}

fn repeat_node(op: u32, body: Node, min: u32, max: u32) -> Node {
    let max = (max != MAXREPEAT).then_some(max as usize);
    match op {
        POSSESSIVE_REPEAT_ONE => Node::possessive(body, min as usize, max),
        MIN_REPEAT_ONE => Node::repeat(body, min as usize, max, false),
        _ => Node::repeat(body, min as usize, max, true),
    }
}
