//! The regular-expression tree a front end builds and [`super::Regex::build`] compiles.

use super::charclass::{CharClass, Flavor};

#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
pub enum Node {
    Empty,
    Char(u32),
    /// `.`: any character except the dialect's line terminators, or any at all under `dotall`.
    Any,
    Class(CharClass),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    /// A group; `Some(n)` captures into group `n` (1-based).
    Group(Option<usize>, Box<Node>),
    /// `Repeat(body, min, max, greedy)`; `max == None` is unbounded.
    Repeat(Box<Node>, usize, Option<usize>, bool),
    /// `^`: start of input, or after a line terminator under `multiline`.
    Start,
    /// `$`: end of input, or before a line terminator under `multiline`; the Python dialect also
    /// accepts the position before a final `\n`.
    End,
    /// `\A`: the start of the input regardless of `multiline`.
    StartText,
    /// `\Z`: the end of the input regardless of `multiline`.
    EndText,
    /// `\b` (`true`) or `\B` (`false`).
    WordB(bool, Flavor),
    Backref(usize),
    /// A placeholder a front end resolves to a group index before compiling.
    NamedBackref(String),
    /// Matches through whichever of the listed groups captured (duplicate named groups).
    BackrefAlt(Vec<usize>),
    /// `(?=…)` / `(?!…)`; the flag is "negated".
    Look(bool, Box<Node>),
    /// `(?<=…)` / `(?<!…)`: the body must match text *ending* at the current position.
    LookBehind(bool, Box<Node>),
    /// `(?>…)`: the body matches once, committing to its first successful path.
    Atomic(Box<Node>),
    /// `(?(n)yes|no)`: `yes` when group `n` has captured, else `no`.
    Cond {
        group: usize,
        yes: Box<Node>,
        no: Box<Node>,
    },
    /// `(?ims-ims:…)` inline-modifier group: `(add, remove)` flag deltas over `(i, m, s)`.
    Modifier {
        add: (bool, bool, bool),
        remove: (bool, bool, bool),
        inner: Box<Node>,
    },
}

impl Node {
    pub fn literal(s: &str) -> Node {
        Node::Concat(s.chars().map(|c| Node::Char(c as u32)).collect())
    }

    pub fn concat(items: Vec<Node>) -> Node {
        Node::Concat(items)
    }

    pub fn alt(items: Vec<Node>) -> Node {
        Node::Alt(items)
    }

    pub fn capture(index: usize, inner: Node) -> Node {
        Node::Group(Some(index), Box::new(inner))
    }

    pub fn non_capture(inner: Node) -> Node {
        Node::Group(None, Box::new(inner))
    }

    pub fn repeat(inner: Node, min: usize, max: Option<usize>, greedy: bool) -> Node {
        Node::Repeat(Box::new(inner), min, max, greedy)
    }

    /// A possessive quantifier: a greedy repeat that never gives characters back.
    pub fn possessive(inner: Node, min: usize, max: Option<usize>) -> Node {
        Node::atomic(Node::repeat(inner, min, max, true))
    }

    pub fn atomic(inner: Node) -> Node {
        Node::Atomic(Box::new(inner))
    }

    pub fn conditional(group: usize, yes: Node, no: Node) -> Node {
        Node::Cond {
            group,
            yes: Box::new(yes),
            no: Box::new(no),
        }
    }

    pub fn lookahead(negate: bool, inner: Node) -> Node {
        Node::Look(negate, Box::new(inner))
    }

    pub fn lookbehind(negate: bool, inner: Node) -> Node {
        Node::LookBehind(negate, Box::new(inner))
    }
}
