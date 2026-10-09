//! The SVG `number` token (sign, digits, fraction, exponent) shared by path data and number lists.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SvgNumberError { NoDigits, NoExponentDigits }

/// End offset of the SVG number starting at `start`. A bare `1.` and `.5` are numbers; an `e`
/// that follows a mantissa must carry exponent digits, and a mantissa needs at least one digit.
pub fn svg_number_token(bytes: &[u8], start: usize) -> Result<usize, SvgNumberError> {
    let digits = |mut at: usize| { let from = at; while bytes.get(at).is_some_and(u8::is_ascii_digit) { at += 1; } (at, at - from) };
    let mut end = start + usize::from(matches!(bytes.get(start), Some(b'+' | b'-')));
    let (after_integer, mut count) = digits(end);
    end = after_integer;
    if bytes.get(end) == Some(&b'.') {
        let (after_fraction, fraction) = digits(end + 1);
        end = after_fraction;
        count += fraction;
    }
    if count == 0 { return Err(SvgNumberError::NoDigits); }
    if matches!(bytes.get(end), Some(b'e' | b'E')) {
        end += 1 + usize::from(matches!(bytes.get(end + 1), Some(b'+' | b'-')));
        let (after_exponent, exponent) = digits(end);
        if exponent == 0 { return Err(SvgNumberError::NoExponentDigits); }
        end = after_exponent;
    }
    Ok(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svg_number_token_accepts_signs_bare_dots_and_exponents_and_stops_at_the_token_end() {
        for (input, end) in [("1", 1), ("-1.5x", 4), ("+.5,", 3), ("1.", 2), ("1.e2", 4), ("-.5E-3 ", 6),
            ("1.5.5", 3), ("1-2", 1), ("12e+7z", 5)] {
            assert_eq!(svg_number_token(input.as_bytes(), 0), Ok(end), "{input}");
        }
        assert_eq!(svg_number_token(b"x 4.25", 2), Ok(6));
    }

    #[test]
    fn svg_number_token_rejects_missing_mantissa_digits_and_dangling_exponents() {
        for input in ["", ".", "-", "+.", "-.e2", "e1", "x"] {
            assert_eq!(svg_number_token(input.as_bytes(), 0), Err(SvgNumberError::NoDigits), "{input}");
        }
        for input in ["1e", "1e+", "2.5E-", "1ex", "1e,2"] {
            assert_eq!(svg_number_token(input.as_bytes(), 0), Err(SvgNumberError::NoExponentDigits), "{input}");
        }
    }
}
