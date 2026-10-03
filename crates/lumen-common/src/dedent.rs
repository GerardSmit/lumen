//! `textwrap.dedent`: removes the whitespace prefix common to every non-blank line (CPython's
//! `_PyUnicode_Dedent`, used for `python -c`).

/// The length of the longest run of spaces and tabs every non-blank line of `text` starts with.
fn common_leading_whitespace(text: &str) -> usize {
    let mut common: Option<&str> = None;
    for line in text.split('\n') {
        let indent_len = line.bytes().take_while(|&b| b == b' ' || b == b'\t').count();
        if indent_len == line.len() {
            continue;
        }
        if indent_len == 0 {
            return 0;
        }
        let indent = &line[..indent_len];
        common = Some(match common {
            None => indent,
            Some(c) => {
                let n = c.bytes().zip(indent.bytes()).take_while(|(a, b)| a == b).count();
                if n == 0 {
                    return 0;
                }
                &c[..n]
            }
        });
    }
    common.map_or(0, str::len)
}

/// `text` without the leading whitespace shared by all its non-blank lines; blank lines are
/// reduced to their newline. Behaves exactly like `textwrap.dedent`.
pub fn dedent(text: &str) -> String {
    let n = common_leading_whitespace(text);
    if n == 0 {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if line.bytes().all(|b| b == b' ' || b == b'\t') {
            continue;
        }
        out.push_str(&line[n..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::dedent;

    #[test]
    fn strips_common_prefix() {
        assert_eq!(dedent("  a\n    b\n  c"), "a\n  b\nc");
    }

    #[test]
    fn blank_lines_become_empty() {
        assert_eq!(dedent("  a\n   \n  b\n"), "a\n\nb\n");
    }

    #[test]
    fn unindented_line_keeps_everything() {
        assert_eq!(dedent("a\n  b"), "a\n  b");
    }

    #[test]
    fn tabs_and_spaces_are_distinct() {
        assert_eq!(dedent("\ta\n  b"), "\ta\n  b");
        assert_eq!(dedent("\t a\n\t b"), "a\nb");
    }

    #[test]
    fn trailing_blank_line_without_newline_is_emptied() {
        assert_eq!(dedent("  a\n  "), "a\n");
    }
}
