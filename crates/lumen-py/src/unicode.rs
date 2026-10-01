//! Unicode predicates and normalization over the tables shared with the JS engine.

use lumen_common::{unicode_norm_impl, unicode_props};
use std::cmp::Ordering;
use std::sync::OnceLock;

type Ranges = &'static [(u32, u32)];

fn contains(ranges: Ranges, c: char) -> bool {
    let cp = c as u32;
    ranges
        .binary_search_by(|&(lo, hi)| {
            if hi < cp {
                Ordering::Less
            } else if lo > cp {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        })
        .is_ok()
}

fn table(cell: &'static OnceLock<Ranges>, name: &str, value: Option<&str>) -> Ranges {
    cell.get_or_init(|| unicode_props::lookup(name, value).expect("generated property table"))
}

pub fn is_xid_start(c: char) -> bool {
    static T: OnceLock<Ranges> = OnceLock::new();
    c == '_' || if c.is_ascii() { c.is_ascii_alphabetic() } else { contains(table(&T, "XID_Start", None), c) }
}

pub fn is_xid_continue(c: char) -> bool {
    static T: OnceLock<Ranges> = OnceLock::new();
    c == '_' || if c.is_ascii() { c.is_ascii_alphanumeric() } else { contains(table(&T, "XID_Continue", None), c) }
}

/// General category Nd (Python's `str.isdecimal`).
pub fn is_decimal(c: char) -> bool {
    static T: OnceLock<Ranges> = OnceLock::new();
    c.is_ascii_digit() || (!c.is_ascii() && contains(table(&T, "General_Category", Some("Nd")), c))
}

/// General category Lt.
pub fn is_titlecase(c: char) -> bool {
    static T: OnceLock<Ranges> = OnceLock::new();
    !c.is_ascii() && contains(table(&T, "General_Category", Some("Lt")), c)
}

pub fn nfkc(s: &str) -> String {
    if s.is_ascii() {
        return s.to_string();
    }
    let cps: Vec<u32> = s.chars().map(|c| c as u32).collect();
    unicode_norm_impl::normalize(&cps, "NFKC").into_iter().filter_map(char::from_u32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_classes() {
        assert!(is_xid_start('é') && is_xid_start('_') && is_xid_start('漢'));
        assert!(!is_xid_start('1') && !is_xid_start('\u{300}'));
        assert!(is_xid_continue('1') && is_xid_continue('\u{300}') && is_xid_continue('·'));
        assert!(!is_xid_continue('-') && !is_xid_continue(' '));
    }

    #[test]
    fn categories() {
        assert!(is_decimal('٣') && is_decimal('７') && is_decimal('5') && !is_decimal('²'));
        assert!(is_titlecase('ǅ') && !is_titlecase('Ǆ'));
    }

    #[test]
    fn nfkc_folds_compat_forms() {
        assert_eq!(nfkc("ｆｏｏ"), "foo");
        assert_eq!(nfkc("µ"), "μ");
        assert_eq!(nfkc("ﬁ"), "fi");
        assert_eq!(nfkc("abc"), "abc");
    }
}
