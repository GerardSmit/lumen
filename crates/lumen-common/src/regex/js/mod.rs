//! The ECMAScript front end: pattern parsing and flag handling for the regex core.
//!
//! Patterns arrive as *elements*: code points in `u`/`v` mode, UTF-16 code units otherwise, with
//! lone surrogates carried as the plane-16 scalars of [`crate::smuggle`] so every element is a `char`.

mod classset;
#[rustfmt::skip]
mod emoji;
mod parser;

use super::{CaseFold, Dialect, Node, Options, Regex};
use parser::Parser;

use crate::smuggle::{smuggle, smuggled};

/// The true code-point value of a pattern/subject element (smuggled surrogates decode).
pub fn cp_of_elem(c: char) -> u32 {
    match smuggled(c) {
        Some(u) => u as u32,
        None => c as u32,
    }
}

pub fn elem_of_cp(cp: u32) -> char {
    if (0xD800..0xE000).contains(&cp) {
        smuggle(cp as u16)
    } else {
        char::from_u32(cp).unwrap()
    }
}

/// A regular-expression SyntaxCharacter (the only chars an identity escape may name in /u mode).
pub(super) fn is_regex_syntax_char(c: char) -> bool {
    matches!(
        c,
        '^' | '$' | '\\' | '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|'
    )
}

pub(super) fn regex_ident_start(c: char) -> bool {
    if c.is_ascii() {
        return c == '$' || c == '_' || c.is_ascii_alphabetic();
    }
    uprop_has("ID_Start", c)
}
/// IdentifierPart for a capture-group name (ID_Continue ∪ {$, _, ZWNJ, ZWJ}).
pub(super) fn regex_ident_part(c: char) -> bool {
    if c.is_ascii() {
        return c == '$' || c == '_' || c.is_ascii_alphanumeric();
    }
    c == '\u{200C}' || c == '\u{200D}' || uprop_has("ID_Continue", c)
}

fn uprop_has(name: &str, c: char) -> bool {
    let u = c as u32;
    crate::unicode_props::lookup(name, None).is_some_and(|r| {
        r.binary_search_by(|&(lo, hi)| {
            if u < lo {
                std::cmp::Ordering::Greater
            } else if u > hi {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
    })
}

fn count_capture_groups(chars: &[char]) -> usize {
    let mut n = 0;
    let mut i = 0;
    let mut in_class = false;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '[' if !in_class => in_class = true,
            ']' if in_class => in_class = false,
            '(' if !in_class => {
                let plain = chars.get(i + 1) != Some(&'?');
                let named = chars.get(i + 2) == Some(&'<')
                    && !matches!(chars.get(i + 3), Some('=') | Some('!'));
                if plain || named {
                    n += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    n
}

/// Whether the pattern contains a named capture group `(?<name>…)` (not a lookbehind `(?<=`/`(?<!`).
fn has_named_group(b: &[char]) -> bool {
    let mut i = 0;
    while i + 2 < b.len() {
        if b[i] == '(' && b[i + 1] == '?' && b[i + 2] == '<' {
            let after = b.get(i + 3).copied();
            if after != Some('=') && after != Some('!') {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// The largest numeric back reference in the pattern (0 when there are none).
pub(super) fn max_backref(node: &Node, out: &mut usize) {
    match node {
        Node::Backref(n) => *out = (*out).max(*n),
        Node::Concat(items) | Node::Alt(items) => {
            for n in items {
                max_backref(n, out);
            }
        }
        Node::Group(_, inner)
        | Node::Repeat(inner, _, _, _)
        | Node::Look(_, inner)
        | Node::LookBehind(_, inner)
        | Node::Modifier { inner, .. } => max_backref(inner, out),
        _ => {}
    }
}

/// Replace each `\k<name>` (`Node::NamedBackref`) with the numeric `Backref` of its group. Names are
/// validated before this runs, so an unknown name resolves to group 0 (never matches), harmlessly.
/// Reject same-name capture groups that could both match (i.e. live in the same concatenation);
/// duplicates spread across different alternation branches are allowed (ES2025).
fn validate_group_names(node: &Node, names: &[(String, usize)]) -> Result<(), String> {
    collect_group_names(node, names)?;
    Ok(())
}

fn collect_group_names(
    node: &Node,
    names: &[(String, usize)],
) -> Result<std::collections::HashSet<String>, String> {
    use std::collections::HashSet;
    match node {
        Node::Group(idx, inner) => {
            let mut s = collect_group_names(inner, names)?;
            if let Some(idx) = idx {
                if let Some((name, _)) = names.iter().find(|(_, i)| i == idx) {
                    if !s.insert(name.clone()) {
                        return Err(format!("duplicate group name {name}"));
                    }
                }
            }
            Ok(s)
        }
        Node::Look(_, inner) | Node::LookBehind(_, inner) | Node::Repeat(inner, _, _, _) => {
            collect_group_names(inner, names)
        }
        Node::Modifier { inner, .. } => collect_group_names(inner, names),
        Node::Concat(children) => {
            let mut all = HashSet::new();
            for c in children {
                for n in collect_group_names(c, names)? {
                    if !all.insert(n.clone()) {
                        return Err(format!("duplicate group name {n}"));
                    }
                }
            }
            Ok(all)
        }
        Node::Alt(branches) => {
            let mut union = HashSet::new();
            for b in branches {
                union.extend(collect_group_names(b, names)?);
            }
            Ok(union)
        }
        _ => Ok(std::collections::HashSet::new()),
    }
}

fn resolve_named_backrefs(node: &mut Node, names: &[(String, usize)]) {
    match node {
        Node::NamedBackref(name) => {
            let idxs: Vec<usize> = names
                .iter()
                .filter(|(n, _)| n == name)
                .map(|(_, i)| *i)
                .collect();
            *node = match idxs.len() {
                0 => Node::Backref(0),
                1 => Node::Backref(idxs[0]),
                _ => Node::BackrefAlt(idxs),
            };
        }
        Node::Concat(v) | Node::Alt(v) => {
            v.iter_mut().for_each(|n| resolve_named_backrefs(n, names))
        }
        Node::Group(_, inner)
        | Node::Repeat(inner, ..)
        | Node::Look(_, inner)
        | Node::LookBehind(_, inner)
        | Node::Modifier { inner, .. } => resolve_named_backrefs(inner, names),
        _ => {}
    }
}

/// The parsed `dgimsuvy` flag set.
pub struct Flags {
    pub unicode: bool,
    pub unicode_sets: bool,
    pub has_indices: bool,
    pub global: bool,
    pub ignore_case: bool,
    pub multiline: bool,
    pub dotall: bool,
    pub sticky: bool,
    /// The flags in the canonical order of the `flags` accessor.
    pub canonical: String,
}

impl Flags {
    pub fn parse(flags: &str) -> Result<Flags, String> {
        let mut seen = String::new();
        for f in flags.chars() {
            if !"dgimsuvy".contains(f) {
                return Err(format!("invalid regular expression flag {f}"));
            }
            if seen.contains(f) {
                return Err(format!("duplicate regular expression flag {f}"));
            }
            seen.push(f);
        }
        if flags.contains('u') && flags.contains('v') {
            return Err("the u and v regular expression flags are mutually exclusive".into());
        }
        Ok(Flags {
            unicode: flags.contains('u') || flags.contains('v'),
            unicode_sets: flags.contains('v'),
            has_indices: flags.contains('d'),
            global: flags.contains('g'),
            ignore_case: flags.contains('i'),
            multiline: flags.contains('m'),
            dotall: flags.contains('s'),
            sticky: flags.contains('y'),
            canonical: "dgimsuvy".chars().filter(|c| flags.contains(*c)).collect(),
        })
    }
}

/// Parse a pattern (as elements, see the module docs) and build its program.
pub fn compile(elems: Vec<char>, flags: &Flags) -> Result<Regex, String> {
    let named_mode = flags.unicode || has_named_group(&elems);
    let total_groups = count_capture_groups(&elems);
    let mut p = Parser::new(elems, flags, named_mode, total_groups);
    let mut ast = p.parse_alt()?;
    if !p.at_end() {
        return Err("unexpected character in pattern".into());
    }
    // Resolve `\k<name>` references now that every group name is known.
    for name in p.name_refs() {
        if !p.names().iter().any(|(n, _)| n == name) {
            return Err(format!("invalid named back reference <{name}>"));
        }
    }
    // Duplicate group names are allowed only across distinct alternation branches.
    validate_group_names(&ast, p.names())?;
    // In Unicode mode a decimal escape must name an existing capture group.
    if flags.unicode {
        let mut max_ref = 0usize;
        max_backref(&ast, &mut max_ref);
        if max_ref > p.ngroups() {
            return Err(format!(
                "back reference \\{max_ref} exceeds the number of capture groups"
            ));
        }
    }
    resolve_named_backrefs(&mut ast, p.names());
    let ngroups = p.ngroups();
    Regex::build(
        &ast,
        ngroups,
        p.into_names(),
        Options {
            ignore_case: flags.ignore_case,
            multiline: flags.multiline,
            dotall: flags.dotall,
            fold: if flags.unicode {
                CaseFold::Full
            } else {
                CaseFold::Legacy
            },
            dialect: Dialect::Js,
        },
    )
}
