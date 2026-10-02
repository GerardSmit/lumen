//! Lexical POSIX path algorithms behind Python's `posixpath`: `normpath` and the root split.

/// `posixpath.normpath`: collapses `//`, `.` and `..` components without touching the file
/// system. Exactly two leading slashes are kept (POSIX leaves them implementation-defined).
pub fn normpath(path: &str) -> String {
    let b = path.as_bytes();
    let initial = if b.starts_with(b"//") && !b.starts_with(b"///") {
        2
    } else {
        usize::from(b.first() == Some(&b'/'))
    };
    let mut comps: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp != ".." || (initial == 0 && comps.is_empty()) || comps.last() == Some(&"..") {
            comps.push(comp);
        } else {
            comps.pop();
        }
    }
    let mut out = "/".repeat(initial);
    out.push_str(&comps.join("/"));
    if out.is_empty() {
        out.push('.');
    }
    out
}

/// The length of the root of `path` (`0`, `1` for `/`, `2` for exactly two leading slashes).
/// POSIX paths have no drive.
pub fn root_len(path: &str) -> usize {
    let b = path.as_bytes();
    if b.first() != Some(&b'/') {
        0
    } else if b.get(1) != Some(&b'/') || b.get(2) == Some(&b'/') {
        1
    } else {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes() {
        assert_eq!(normpath(""), ".");
        assert_eq!(normpath("a//b/./c/../d"), "a/b/d");
        assert_eq!(normpath("//a"), "//a");
        assert_eq!(normpath("///a"), "/a");
        assert_eq!(normpath("/.."), "/");
        assert_eq!(normpath("../.."), "../..");
        assert_eq!(normpath("a/.."), ".");
        assert_eq!(normpath("a/b/"), "a/b");
    }

    #[test]
    fn roots() {
        assert_eq!(root_len("a"), 0);
        assert_eq!(root_len("/a"), 1);
        assert_eq!(root_len("//a"), 2);
        assert_eq!(root_len("///a"), 1);
    }
}
