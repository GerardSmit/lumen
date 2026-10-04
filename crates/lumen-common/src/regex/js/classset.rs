//! `v`-flag (unicodeSets) character classes.

use crate::regex::{CharClass, Node};

// ---------------------------------------------------------------------------------------------
// `v`-flag (unicodeSets) character classes: ClassSetExpressions are evaluated at parse time into
// a concrete set of code-point ranges plus a set of multi-code-point strings.
// ---------------------------------------------------------------------------------------------

/// A `v`-mode class set: sorted, disjoint code-point ranges plus multi-code-point strings.
#[derive(Default, Clone)]
pub(super) struct ClassSet {
    pub(super) ranges: Vec<(u32, u32)>,
    pub(super) strings: Vec<Vec<char>>,
}

impl ClassSet {
    pub(super) fn normalize(&mut self) {
        self.ranges.sort_unstable();
        let mut out: Vec<(u32, u32)> = Vec::with_capacity(self.ranges.len());
        for &(lo, hi) in &self.ranges {
            if let Some(last) = out.last_mut() {
                if lo <= last.1.saturating_add(1) {
                    last.1 = last.1.max(hi);
                    continue;
                }
            }
            out.push((lo, hi));
        }
        self.ranges = out;
        self.strings.sort();
        self.strings.dedup();
    }

    pub(super) fn union(mut self, other: ClassSet) -> ClassSet {
        self.ranges.extend(other.ranges);
        self.strings.extend(other.strings);
        self.normalize();
        self
    }

    pub(super) fn intersect(mut self, other: ClassSet) -> ClassSet {
        let mut ranges = Vec::new();
        for &(a, b) in &self.ranges {
            for &(c, d) in &other.ranges {
                let lo = a.max(c);
                let hi = b.min(d);
                if lo <= hi {
                    ranges.push((lo, hi));
                }
            }
        }
        self.strings.retain(|s| other.strings.contains(s));
        self.ranges = ranges;
        self.normalize();
        self
    }

    pub(super) fn subtract(mut self, other: ClassSet) -> ClassSet {
        let mut ranges = self.ranges.clone();
        for &(c, d) in &other.ranges {
            let mut next = Vec::with_capacity(ranges.len() + 1);
            for &(a, b) in &ranges {
                if d < a || c > b {
                    next.push((a, b));
                    continue;
                }
                if a < c {
                    next.push((a, c - 1));
                }
                if b > d {
                    next.push((d + 1, b));
                }
            }
            ranges = next;
        }
        self.strings.retain(|s| !other.strings.contains(s));
        self.ranges = ranges;
        self.normalize();
        self
    }

    /// Complement over the full code-point space. A set containing strings may not be negated.
    pub(super) fn complement(mut self) -> Result<ClassSet, String> {
        if !self.strings.is_empty() {
            return Err("cannot negate a class set containing strings".into());
        }
        self.normalize();
        let mut out = Vec::new();
        let mut next = 0u32;
        for &(lo, hi) in &self.ranges {
            if lo > next {
                out.push((next, lo - 1));
            }
            next = hi.saturating_add(1);
        }
        if next <= 0x10FFFF {
            out.push((next, 0x10FFFF));
        }
        self.ranges = out;
        Ok(self)
    }

    pub(super) fn from_cp(c: u32) -> ClassSet {
        ClassSet {
            ranges: vec![(c, c)],
            strings: Vec::new(),
        }
    }
}

/// The concrete ranges of a `\d`/`\w`/`\s` class escape (for `v`-mode set arithmetic).
pub(super) fn builtin_class_set(b: char) -> ClassSet {
    let base = match b.to_ascii_lowercase() {
        'd' => vec![(0x30, 0x39)],
        'w' => vec![(0x30, 0x39), (0x41, 0x5A), (0x5F, 0x5F), (0x61, 0x7A)],
        's' => {
            let mut r = vec![
                (0x09, 0x0D),
                (0x20, 0x20),
                (0x85, 0x85),
                (0xA0, 0xA0),
                (0x1680, 0x1680),
                (0x2000, 0x200A),
                (0x2028, 0x2029),
                (0x202F, 0x202F),
                (0x205F, 0x205F),
                (0x3000, 0x3000),
                (0xFEFF, 0xFEFF),
            ];
            r.sort_unstable();
            r
        }
        _ => Vec::new(),
    };
    let mut set = ClassSet {
        ranges: base,
        strings: Vec::new(),
    };
    if b.is_ascii_uppercase() {
        set = set.complement().unwrap();
    }
    set
}

/// The derivable Unicode "properties of strings" (UTS #51 definitions built from the bundled
/// emoji binary-property tables). The RGI_* curated lists are not derivable and stay unsupported.
pub(super) fn property_of_strings(name: &str) -> Option<ClassSet> {
    let ranges_of = |prop: &str| -> Vec<(u32, u32)> {
        crate::unicode_props::lookup(prop, None)
            .map(|r| r.to_vec())
            .unwrap_or_default()
    };
    match name {
        "Basic_Emoji" => {
            // Emoji_Presentation singletons, plus (Emoji minus Emoji_Presentation) + FE0F.
            let ep = ClassSet {
                ranges: ranges_of("Emoji_Presentation"),
                strings: Vec::new(),
            };
            let emoji = ClassSet {
                ranges: ranges_of("Emoji"),
                strings: Vec::new(),
            };
            let text_only = emoji.subtract(ep.clone());
            let mut strings = Vec::new();
            for &(lo, hi) in &text_only.ranges {
                for u in lo..=hi {
                    if let Some(c) = char::from_u32(u) {
                        strings.push(vec![c, '\u{FE0F}']);
                    }
                }
            }
            let mut set = ep;
            set.strings = strings;
            set.normalize();
            Some(set)
        }
        "Emoji_Keycap_Sequence" => {
            let mut strings = Vec::new();
            for c in "#*0123456789".chars() {
                strings.push(vec![c, '\u{FE0F}', '\u{20E3}']);
            }
            Some(ClassSet {
                ranges: Vec::new(),
                strings,
            })
        }
        "RGI_Emoji_Modifier_Sequence" => {
            let bases = ranges_of("Emoji_Modifier_Base");
            let mut strings = Vec::new();
            for &(lo, hi) in &bases {
                for u in lo..=hi {
                    if let Some(c) = char::from_u32(u) {
                        for m in 0x1F3FB..=0x1F3FF {
                            strings.push(vec![c, char::from_u32(m).unwrap()]);
                        }
                    }
                }
            }
            Some(ClassSet {
                ranges: Vec::new(),
                strings,
            })
        }
        "RGI_Emoji_Flag_Sequence" => Some(ClassSet {
            ranges: Vec::new(),
            strings: super::emoji::RGI_FLAG_SEQUENCES
                .iter()
                .map(|s| s.chars().collect())
                .collect(),
        }),
        "RGI_Emoji_ZWJ_Sequence" => Some(ClassSet {
            ranges: Vec::new(),
            strings: super::emoji::RGI_ZWJ_SEQUENCES
                .iter()
                .map(|s| s.chars().collect())
                .collect(),
        }),
        "RGI_Emoji" => {
            // The union table: single code points join the ranges, sequences the strings.
            let mut set = ClassSet {
                ranges: Vec::new(),
                strings: Vec::new(),
            };
            for s in super::emoji::RGI_EMOJI_ALL {
                let cs: Vec<char> = s.chars().collect();
                if cs.len() == 1 {
                    set.ranges.push((cs[0] as u32, cs[0] as u32));
                } else {
                    set.strings.push(cs);
                }
            }
            set.normalize();
            Some(set)
        }
        "RGI_Emoji_Tag_Sequence" => {
            // The three RGI tag sequences: england, scotland, wales.
            let mk = |tags: &str| {
                let mut v = vec!['\u{1F3F4}'];
                for c in tags.chars() {
                    v.push(char::from_u32(0xE0000 + c as u32).unwrap());
                }
                v.push('\u{E007F}');
                v
            };
            Some(ClassSet {
                ranges: Vec::new(),
                strings: vec![mk("gbeng"), mk("gbsct"), mk("gbwls")],
            })
        }
        _ => None,
    }
}

/// A `\q{...}` alternative: a single char joins the ranges; longer sequences join the strings.
pub(super) fn push_q_alternative(set: &mut ClassSet, alt: Vec<char>) {
    match alt.len() {
        0 => set.strings.push(Vec::new()),
        1 => set.ranges.push((alt[0] as u32, alt[0] as u32)),
        _ => set.strings.push(alt),
    }
}

/// Compile a computed class set: an alternation of its strings (longest first, so the greedy
/// match prefers the longest sequence) plus a plain range class. Lone-surrogate ranges are
/// dropped (input is scalar values).
pub(super) fn class_set_to_node(mut set: ClassSet) -> Node {
    set.normalize();
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    for &(lo, hi) in &set.ranges {
        let mut push = |a: u32, b: u32| {
            if a <= b {
                ranges.push((a, b));
            }
        };
        if lo <= 0xD7FF && hi >= 0xE000 {
            push(lo, 0xD7FF);
            push(0xE000, hi);
        } else if !(0xD800..=0xDFFF).contains(&lo) || !(0xD800..=0xDFFF).contains(&hi) {
            push(lo.clamp(0, 0x10FFFF), hi.min(0x10FFFF));
        }
    }
    let mut class = CharClass::new();
    class.ranges = ranges;
    let class = Node::Class(class);
    if set.strings.is_empty() {
        return class;
    }
    let mut strings = set.strings;
    strings.sort_by_key(|b| std::cmp::Reverse(b.len()));
    let mut alts: Vec<Node> = strings
        .into_iter()
        .map(|cs| {
            if cs.is_empty() {
                Node::Empty
            } else {
                Node::Concat(cs.into_iter().map(|c| Node::Char(c as u32)).collect())
            }
        })
        .collect();
    alts.push(class);
    Node::Group(None, Box::new(Node::Alt(alts)))
}
