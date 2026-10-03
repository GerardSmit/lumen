//! Plain-text helpers shared by the string types.

/// `str.expandtabs`: every tab becomes the spaces up to the next multiple of `tabsize` columns
/// (it is dropped when `tabsize <= 0`); `\n` and `\r` restart the column count. `reserve` sees
/// the length the result grows to before each tab's padding is added and can stop the expansion.
pub fn expand_tabs<E>(s: &str, tabsize: i64, mut reserve: impl FnMut(usize) -> Result<(), E>) -> Result<String, E> {
    let mut out = String::with_capacity(s.len());
    let mut col = 0i64;
    for c in s.chars() {
        match c {
            '\t' => {
                if tabsize > 0 {
                    let n = tabsize - (col % tabsize);
                    reserve(out.len() + n as usize)?;
                    out.extend(std::iter::repeat_n(' ', n as usize));
                    col += n;
                }
            }
            '\n' | '\r' => {
                out.push(c);
                col = 0;
            }
            c => {
                out.push(c);
                col += 1;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_to_tab_stops() {
        let r: Result<String, ()> = expand_tabs("a\tbc\td\n\tx", 4, |_| Ok(()));
        assert_eq!(r.unwrap(), "a   bc  d\n    x");
        let r: Result<String, ()> = expand_tabs("a\tb", 0, |_| Ok(()));
        assert_eq!(r.unwrap(), "ab");
    }
}
