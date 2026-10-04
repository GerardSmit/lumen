//! A from-scratch backtracking regular-expression engine shared by the JavaScript engine and
//! `lumen-py`, with no dependencies.
//!
//! A front end builds a [`Node`] tree — [`js`] parses ECMAScript patterns, and other languages
//! construct the tree directly with the `Node` constructors — and [`Regex::build`] compiles it,
//! under the language's [`Options`], to a flat program. [`Regex::exec`] runs the program over any
//! [`ReInput`] (a code-unit or code-point view of the subject) with an explicit heap backtrack
//! stack, a step budget, and the polled interrupt, deadline and heap limits of [`limits`].
//!
//! Beyond the ECMAScript core (literals, classes, anchors, greedy and lazy quantifiers, groups,
//! back references, lookaround, inline modifiers) the IR carries what Python's `re` needs: atomic
//! groups and possessive quantifiers, conditional groups, absolute `\A`/`\Z` anchors, Python's
//! `$`, back references that fail when their group is unset, Python's case folding and
//! `\d \w \s \b` definitions, and `match` / `fullmatch` / `search` modes with an end bound.

mod captures;
mod charclass;
mod compile;
mod fold;
#[rustfmt::skip]
mod fold_table;
pub mod js;
pub mod limits;
mod matcher;
mod program;
pub mod sre;

pub use crate::limits::Abort;
pub use captures::Captures;
pub use charclass::{Builtin, BuiltinSet, CharClass, Flavor, PreMap};
pub(crate) use compile::NEST_ERROR;
pub use fold::{
    canonicalize_legacy, fold_canon, fold_eq, fold_orbit, js_whitespace, py_fold, py_is_decimal,
    py_is_space, py_is_word, py_lower, py_upper, CaseFold,
};
pub use ir::Node;
pub use limits::{set_host_poll, take_abort, BacktrackLimit, BACKTRACK_LIMIT_MSG};
pub use matcher::{ExecOptions, Mode, ReInput};
pub use program::{Dialect, Options, Regex};

mod ir;

#[cfg(test)]
mod tests;
