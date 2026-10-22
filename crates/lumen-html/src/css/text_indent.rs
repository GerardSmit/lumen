pub(super) const HANGING: u8 = 1;
pub(super) const EACH_LINE: u8 = 2;

/// Splits `<length-percentage> && hanging? && each-line?` into the length
/// token and the keyword mask.
pub(super) fn split(raw: &str) -> Option<(&str, u8)> {
    let mut length = None;
    let mut mode = 0u8;
    let tokens = super::components(raw.trim())?;
    for token in tokens {
        let bit = if token.eq_ignore_ascii_case("hanging") {
            HANGING
        } else if token.eq_ignore_ascii_case("each-line") {
            EACH_LINE
        } else {
            0
        };
        if bit == 0 {
            if length.replace(token).is_some() {
                return None;
            }
        } else if mode & bit != 0 {
            return None;
        } else {
            mode |= bit;
        }
    }
    Some((length?, mode))
}

pub(super) fn keywords(mode: u8) -> &'static str {
    match mode & 3 {
        1 => " hanging",
        2 => " each-line",
        3 => " hanging each-line",
        _ => "",
    }
}

pub(super) fn specified(raw: &str) -> Option<alloc::string::String> {
    let (length, mode) = split(raw)?;
    if super::math_function(length) {
        let length = super::typed_numeric::parse_numeric_expression(length)?.serialize_specified()?;
        return Some(alloc::format!("{length}{}", keywords(mode)));
    }
    Some(alloc::format!("{length}{}", keywords(mode)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_keywords_in_any_order() {
        assert_eq!(split("each-line hanging calc(2em + 3ex)"), Some(("calc(2em + 3ex)", 3)));
        assert_eq!(split("10px hanging hanging"), None);
        assert_eq!(split("hanging"), None);
        assert_eq!(specified("each-line 10px").as_deref(), Some("10px each-line"));
    }
}
