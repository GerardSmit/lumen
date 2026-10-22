//! Process-wide interning of strings that APIs need as `&'static str` (property keys built at
//! run time). Each distinct string is allocated once and stays reachable from the table, so
//! repeated engines and repeated calls neither grow memory nor show up as leaks.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

/// The `&'static` copy of `s`, shared by every caller in the process.
pub fn intern(s: &str) -> &'static str {
    static TABLE: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut table = TABLE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(&found) = table.get(s) {
        return found;
    }
    let owned: &'static str = Box::leak(s.to_owned().into_boxed_str());
    table.insert(owned);
    owned
}

#[cfg(test)]
mod tests {
    #[test]
    fn equal_strings_share_one_allocation() {
        let a = super::intern(&format!("%{}%", "Intl.Locale"));
        let b = super::intern("%Intl.Locale%");
        assert_eq!(a, "%Intl.Locale%");
        assert!(std::ptr::eq(a, b));
    }
}
