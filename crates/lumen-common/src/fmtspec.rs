//! The format-spec mini-language, `[[fill]align][sign][z][#][0][width][grouping][.precision][type]`,
//! parsed once for every formatter (int, float, complex, str, `Decimal`).
//!
//! The parser only splits the text into fields; what a field means for a given type (which types
//! allow a sign, whether `=` alignment is valid, ...) is up to each formatter. Text is read as a
//! code-point string (see [`crate::smuggle`]), so a fill may be a lone surrogate.

use crate::smuggle::code_points;

/// A parsed format specifier. Absent fields are `None` / `false`.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    /// A code point: the fill may be a lone surrogate or a reserved-block character.
    pub fill: Option<u32>,
    pub align: Option<char>,
    pub sign: Option<char>,
    pub alt: bool,
    pub zero: bool,
    /// The `z` flag: negative zero is coerced to positive zero.
    pub z: bool,
    pub width: Option<usize>,
    /// `,` or `_`.
    pub grouping: Option<char>,
    pub precision: Option<usize>,
    /// `,` or `_` after the precision: groups the fractional digits in threes.
    pub frac_grouping: Option<char>,
    pub ty: Option<char>,
}

/// Why a specifier did not parse; [`SpecError::message`] gives CPython's text where it has a
/// specific one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpecError {
    /// The specifier is malformed ("Invalid format specifier ...").
    Invalid,
    /// A width or precision does not fit an index.
    TooManyDigits,
    /// Both `,` and `_` were given.
    BothSeparators,
    /// A `.` without digits.
    MissingPrecision,
}

impl SpecError {
    /// CPython's message for the errors that have their own; `None` for [`SpecError::Invalid`],
    /// whose text names the specifier and the type.
    pub fn message(self) -> Option<&'static str> {
        match self {
            SpecError::Invalid => None,
            SpecError::TooManyDigits => Some("Too many decimal digits in format string"),
            SpecError::BothSeparators => Some("Cannot specify both ',' and '_'."),
            SpecError::MissingPrecision => Some("Format specifier missing precision"),
        }
    }
}

/// The fractional digits `frac` in groups of three from the decimal point, joined by `sep`.
pub fn group_fraction(frac: &str, sep: char) -> String {
    let mut out = String::with_capacity(frac.len() + frac.len() / 3);
    for (i, c) in frac.chars().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(sep);
        }
        out.push(c);
    }
    out
}

fn is_align(c: char) -> bool {
    matches!(c, '<' | '>' | '^' | '=')
}

/// Parses `spec` as CPython's `str.format` / `format()` mini-language.
pub fn parse(spec: &str) -> Result<Spec, SpecError> {
    parse_with(spec, false)
}

/// Like [`parse`], with the digit rules of `_pydecimal`'s specifier grammar: the width after the
/// `0` flag cannot start with `0`, and a precision has no leading zeros (`0` alone is fine).
pub fn parse_decimal(spec: &str) -> Result<Spec, SpecError> {
    parse_with(spec, true)
}

fn parse_with(spec: &str, strict_digits: bool) -> Result<Spec, SpecError> {
    let cps: Vec<u32> = code_points(spec).collect();
    let chars: Vec<char> = cps.iter().map(|&c| char::from_u32(c).unwrap_or('\u{FFFD}')).collect();
    let n = chars.len();
    let mut i = 0;
    let mut s = Spec::default();
    if n >= 2 && is_align(chars[1]) {
        s.fill = Some(cps[0]);
        s.align = Some(chars[1]);
        i = 2;
    } else if n >= 1 && is_align(chars[0]) {
        s.align = Some(chars[0]);
        i = 1;
    }
    if i < n && matches!(chars[i], '+' | '-' | ' ') {
        s.sign = Some(chars[i]);
        i += 1;
    }
    if i < n && chars[i] == 'z' {
        s.z = true;
        i += 1;
    }
    if i < n && chars[i] == '#' {
        s.alt = true;
        i += 1;
    }
    if i < n && chars[i] == '0' {
        s.zero = true;
        i += 1;
    }
    let digits = |i: &mut usize| {
        let start = *i;
        while *i < n && chars[*i].is_ascii_digit() {
            *i += 1;
        }
        start..*i
    };
    let number = |range: std::ops::Range<usize>| -> Result<usize, SpecError> {
        let mut v: usize = 0;
        for c in &chars[range] {
            v = v
                .checked_mul(10)
                .and_then(|v| v.checked_add(c.to_digit(10).unwrap_or(0) as usize))
                .filter(|&v| v <= i64::MAX as usize)
                .ok_or(SpecError::TooManyDigits)?;
        }
        Ok(v)
    };
    let w = digits(&mut i);
    if !w.is_empty() {
        if strict_digits && chars[w.start] == '0' {
            return Err(SpecError::Invalid);
        }
        s.width = Some(number(w)?);
    }
    if i < n && (chars[i] == ',' || chars[i] == '_') {
        s.grouping = Some(chars[i]);
        i += 1;
        if i < n && (chars[i] == ',' || chars[i] == '_') {
            return Err(SpecError::BothSeparators);
        }
    }
    if i < n && chars[i] == '.' {
        i += 1;
        let p = digits(&mut i);
        let mut consumed = p.len();
        if !p.is_empty() {
            if strict_digits && chars[p.start] == '0' && p.len() > 1 {
                return Err(SpecError::Invalid);
            }
            s.precision = Some(number(p)?);
        }
        if i < n && chars[i] == ',' {
            s.frac_grouping = Some(',');
            i += 1;
            consumed += 1;
        }
        if i < n && chars[i] == '_' {
            if s.frac_grouping.is_some() {
                return Err(SpecError::BothSeparators);
            }
            s.frac_grouping = Some('_');
            i += 1;
            consumed += 1;
        }
        if i < n && chars[i] == ',' && s.frac_grouping == Some('_') {
            return Err(SpecError::BothSeparators);
        }
        if consumed == 0 {
            return Err(SpecError::MissingPrecision);
        }
    }
    if i < n {
        s.ty = Some(chars[i]);
        i += 1;
    }
    if i < n {
        return Err(SpecError::Invalid);
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_every_field() {
        let s = parse("*^+z#010,.3f").unwrap();
        assert_eq!(s.fill, Some('*' as u32));
        assert_eq!(s.align, Some('^'));
        assert_eq!(s.sign, Some('+'));
        assert!(s.z && s.alt && s.zero);
        assert_eq!(s.width, Some(10));
        assert_eq!(s.grouping, Some(','));
        assert_eq!(s.precision, Some(3));
        assert_eq!(s.ty, Some('f'));
    }

    #[test]
    fn alignment_without_fill() {
        let s = parse(">8").unwrap();
        assert_eq!((s.fill, s.align, s.width), (None, Some('>'), Some(8)));
        assert_eq!(parse("<<").unwrap().fill, Some('<' as u32));
    }

    #[test]
    fn errors() {
        assert_eq!(parse("1,_"), Err(SpecError::BothSeparators));
        assert_eq!(parse("."), Err(SpecError::MissingPrecision));
        assert_eq!(parse("99999999999999999999"), Err(SpecError::TooManyDigits));
        assert_eq!(parse("fx"), Err(SpecError::Invalid));
        assert_eq!(parse(".3_,f"), Err(SpecError::BothSeparators));
        assert_eq!(parse(".,6f"), Err(SpecError::Invalid));
    }

    #[test]
    fn groups_fraction_from_the_point() {
        assert_eq!(group_fraction("1234567", '_'), "123_456_7");
        assert_eq!(group_fraction("12", ','), "12");
        assert_eq!(group_fraction("", ','), "");
    }

    #[test]
    fn fractional_grouping() {
        let s = parse(",.6_f").unwrap();
        assert_eq!((s.grouping, s.precision, s.frac_grouping, s.ty), (Some(','), Some(6), Some('_'), Some('f')));
        let s = parse("._f").unwrap();
        assert_eq!((s.precision, s.frac_grouping), (None, Some('_')));
    }

    #[test]
    fn decimal_digit_rules() {
        assert!(parse(".05f").is_ok());
        assert_eq!(parse_decimal(".05f"), Err(SpecError::Invalid));
        assert!(parse_decimal(".0f").is_ok());
        assert_eq!(parse_decimal("00"), Err(SpecError::Invalid));
        assert!(parse_decimal("010").is_ok());
    }

    #[test]
    fn lone_surrogate_fill() {
        let mut text = String::new();
        crate::smuggle::push_code_point(&mut text, 0xD800);
        text.push('>');
        assert_eq!(parse(&text).unwrap().fill, Some(0xD800));
    }
}
