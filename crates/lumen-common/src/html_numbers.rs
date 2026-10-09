//! HTML's attribute-number algorithms, independent of DOM and host bindings.

/// HTML §2.3.4.3 parsing (not authoring validation). Consume the numeric
/// prefix, ignoring an incomplete exponent and trailing nonnumeric input.
/// Rust's decimal conversion supplies correctly rounded binary64 without an
/// intermediate rounded significand or a temporary normalization buffer.
pub fn parse_floating_point(input: &str) -> Option<f64> {
    let input = input.trim_start_matches(|c| matches!(c, '\t' | '\n' | '\x0c' | '\r' | ' '));
    let bytes = input.as_bytes();
    let mut end = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let integer_start = end;
    while bytes.get(end).is_some_and(u8::is_ascii_digit) { end += 1; }
    let integer_digits = end != integer_start;
    if bytes.get(end) == Some(&b'.') {
        end += 1;
        let fraction_start = end;
        while bytes.get(end).is_some_and(u8::is_ascii_digit) { end += 1; }
        if !integer_digits && end == fraction_start { return None; }
    } else if !integer_digits { return None; }
    let mantissa_end = end;
    if matches!(bytes.get(end), Some(b'e' | b'E')) {
        end += 1;
        if matches!(bytes.get(end), Some(b'+' | b'-')) { end += 1; }
        let exponent_start = end;
        while bytes.get(end).is_some_and(u8::is_ascii_digit) { end += 1; }
        if end == exponent_start { end = mantissa_end; }
    }
    let value = input[..end].parse::<f64>().ok()?;
    value.is_finite().then_some(if value == 0.0 { 0.0 } else { value })
}

/// A parsed HTML dimension. Percentages retain their numeric value in percent
/// units so callers can resolve them against the property's actual basis.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Dimension {
    Length(f64),
    Percentage(f64),
}

impl Dimension {
    pub fn value(self) -> f64 {
        match self { Self::Length(value) | Self::Percentage(value) => value }
    }
}

/// HTML §2.3.4.4. This intentionally differs from CSS number parsing: signs,
/// leading decimal points and exponents are not part of the accepted prefix,
/// and only a percent sign immediately after that prefix changes its kind.
pub fn parse_dimension(input: &str) -> Option<Dimension> {
    let bytes = input.as_bytes();
    let mut position = 0;
    while bytes.get(position).is_some_and(|byte| matches!(byte, b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')) {
        position += 1;
    }
    if !bytes.get(position).is_some_and(u8::is_ascii_digit) { return None; }
    let mut value = 0.0;
    while let Some(byte) = bytes.get(position).filter(|byte| byte.is_ascii_digit()) {
        value = value * 10.0 + f64::from(*byte - b'0');
        position += 1;
    }
    if bytes.get(position) == Some(&b'.') {
        position += 1;
        let mut divisor = 1.0;
        while let Some(byte) = bytes.get(position).filter(|byte| byte.is_ascii_digit()) {
            divisor *= 10.0;
            value += f64::from(*byte - b'0') / divisor;
            position += 1;
        }
    }
    Some(if bytes.get(position) == Some(&b'%') {
        Dimension::Percentage(value)
    } else { Dimension::Length(value) })
}

/// HTML §2.3.4.5 uses the same parse and rejects either kind of zero.
pub fn parse_nonzero_dimension(input: &str) -> Option<Dimension> {
    parse_dimension(input).filter(|dimension| dimension.value() != 0.0)
}

/// The sign of HTML §2.3.4.2's parsed integer. Callers that only distinguish
/// zero avoid truncation or overflow even for arbitrarily long digit prefixes.
pub fn parse_integer_sign(input: &str) -> Option<core::cmp::Ordering> {
    let input = input.trim_start_matches(|character| matches!(character, '\t' | '\n' | '\x0c' | '\r' | ' '));
    let bytes = input.as_bytes();
    let negative = bytes.first() == Some(&b'-');
    let first = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    if !bytes.get(first).is_some_and(u8::is_ascii_digit) { return None; }
    let nonzero = bytes[first..].iter().take_while(|byte| byte.is_ascii_digit()).any(|byte| *byte != b'0');
    Some(if !nonzero { core::cmp::Ordering::Equal } else if negative {
        core::cmp::Ordering::Less
    } else { core::cmp::Ordering::Greater })
}

/// HTML integer parsing with a long-range result. Trailing non-digits do not
/// invalidate the parsed prefix; an overflow is distinct from invalid syntax.
pub fn parse_integer_i32(input: &str) -> Option<i32> {
    let input = input.trim_start_matches(|character| matches!(character, '\t' | '\n' | '\x0c' | '\r' | ' '));
    let bytes = input.as_bytes();
    let negative = bytes.first() == Some(&b'-');
    let first = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    if !bytes.get(first).is_some_and(u8::is_ascii_digit) { return None; }
    let mut value = 0i32;
    for byte in bytes[first..].iter().take_while(|byte| byte.is_ascii_digit()) {
        value = value.checked_mul(10)?;
        let digit = i32::from(*byte - b'0');
        value = if negative { value.checked_sub(digit)? } else { value.checked_add(digit)? };
    }
    Some(value)
}

/// HTML §2.3.6. The caller supplies its existing CSS named-color lookup;
/// system colors and CSS functional syntax do not belong to this grammar.
/// The bounded normalization buffer holds at most 128 characters plus padding.
pub fn parse_legacy_rgb(input: &str, named: impl FnOnce(&str) -> Option<[u8; 3]>) -> Option<[u8; 3]> {
    if input.is_empty() { return None; }
    let input = input.trim_matches(|character| matches!(character, '\t' | '\n' | '\x0c' | '\r' | ' '));
    if input.eq_ignore_ascii_case("transparent") { return None; }
    if let Some(color) = named(input) { return Some(color); }
    let nibble = |character: char| if character.is_ascii_hexdigit() {
        character.to_digit(16).unwrap_or(0) as u8
    } else { 0 };
    if input.len() == 4 && input.starts_with('#') && input.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit) {
        let bytes = input.as_bytes();
        return Some(core::array::from_fn(|channel| nibble(char::from(bytes[channel + 1])) * 17));
    }
    let mut digits = [0u8; 129];
    let mut length = 0;
    for character in input.chars() {
        if length == 128 { break; }
        digits[length] = nibble(character);
        length += 1;
        if character as u32 > 0xffff && length < 128 {
            digits[length] = 0;
            length += 1;
        }
    }
    if input.starts_with('#') {
        digits.copy_within(1..length, 0);
        length -= 1;
    }
    while length == 0 || length % 3 != 0 { digits[length] = 0; length += 1; }
    let component_length = length / 3;
    let mut first = component_length.saturating_sub(8);
    while component_length - first > 2 && (0..3).all(|channel| digits[channel * component_length + first] == 0) {
        first += 1;
    }
    let used = (component_length - first).min(2);
    Some(core::array::from_fn(|channel| {
        digits[channel * component_length + first..channel * component_length + first + used]
            .iter().fold(0, |value, digit| value * 16 + digit)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specification_html_floating_point_prefixes_round_once_and_normalize_zero() {
        for (input, expected) in [(" +.5tail", 0.5), ("1.e2x", 100.0),
            ("1.", 1.0), ("1.e-", 1.0), ("2e+", 2.0), ("-0", 0.0),
            ("-1e-9999", 0.0), ("0.1000000000000000055511151231257827021181583404541015625", 0.1)] {
            let value = parse_floating_point(input).unwrap();
            assert_eq!(value, expected, "{input}");
            if value == 0.0 { assert!(!value.is_sign_negative()); }
        }
        for input in ["", ".", "-.e2", "+", "NaN", "Infinity", "\u{a0}1", "1e9999"] {
            assert_eq!(parse_floating_point(input), None, "{input}");
        }
        assert_eq!(parse_floating_point("1.7976931348623157e308"), Some(f64::MAX));
    }

    #[test]
    fn specification_html_signed_integer_sign_preserves_zero_and_unbounded_prefixes() {
        use core::cmp::Ordering::*;
        for (input, expected) in [(" -000tail", Equal), ("+0", Equal), (" -1x", Less),
            ("+22.0", Greater), ("0002", Greater)] {
            assert_eq!(parse_integer_sign(input), Some(expected));
        }
        for input in ["", "-", "+ 1", "-+2", "\u{a0}0", ".0"] {
            assert_eq!(parse_integer_sign(input), None);
        }
        assert_eq!(parse_integer_sign(&"9".repeat(400)), Some(Greater));
    }

    #[test]
    fn specification_html_dimensions_preserve_prefix_kind_and_nonzero_rules() {
        for (input, expected) in [
            (" \t12.5%junk", Dimension::Percentage(12.5)),
            ("3.%", Dimension::Percentage(3.0)),
            ("3.", Dimension::Length(3.0)),
            ("3 %", Dimension::Length(3.0)),
            ("3e2%", Dimension::Length(3.0)),
            ("3px%", Dimension::Length(3.0)),
            ("3.25.5%", Dimension::Length(3.25)),
            ("00.0%", Dimension::Percentage(0.0)),
        ] { assert_eq!(parse_dimension(input), Some(expected), "{input}"); }
        for input in ["", " \r", "+3", "-0", ".5", "\u{a0}3", "１"] {
            assert_eq!(parse_dimension(input), None, "{input}");
        }
        assert_eq!(parse_nonzero_dimension("0%"), None);
        assert_eq!(parse_nonzero_dimension("0.0junk"), None);
        assert_eq!(parse_nonzero_dimension("0.25%"), Some(Dimension::Percentage(0.25)));
        assert_eq!(parse_dimension(&"9".repeat(400)).unwrap().value(), f64::INFINITY);
    }

    #[test]
    fn specification_html_legacy_colors_normalize_code_points_and_components() {
        let parse = |input| parse_legacy_rgb(input, |_| None);
        assert_eq!(parse(""), None);
        assert_eq!(parse(" transparent \t"), None);
        for (input, expected) in [
            ("#aBc", [170, 187, 204]), ("#123456", [18, 52, 86]),
            ("chucknorris", [192, 0, 0]), ("  ", [0, 0, 0]),
            ("1", [1, 0, 0]), ("123", [1, 2, 3]),
            ("#000000001100000000220000000033", [17, 34, 51]),
            ("\u{1f600}1234", [0, 18, 52]),
        ] { assert_eq!(parse(input), Some(expected), "{input}"); }
        assert_eq!(parse_legacy_rgb(" ReD ", |name| name.eq_ignore_ascii_case("red").then_some([255, 0, 0])), Some([255, 0, 0]));
        assert_eq!(parse(&"f".repeat(400)), Some([255; 3]));
    }
}
