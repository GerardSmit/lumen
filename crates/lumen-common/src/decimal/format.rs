//! The format mini-language of `Decimal.__format__`: `[[fill]align][sign][z][#][0][width][,][.precision][type]`.

use super::coef::Mem;
use super::{Context, Decimal, Kind};

/// Locale conventions for the `n` type (and explicit overrides): the decimal point, the thousands
/// separator and the grouping in `localeconv()` form (`[3, 0]` = groups of three repeating, a
/// trailing `127` = no further grouping).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Locale {
    pub decimal_point: String,
    pub thousands_sep: String,
    pub grouping: Vec<i32>,
}

impl Default for Locale {
    /// The "C" locale.
    fn default() -> Locale {
        Locale { decimal_point: ".".into(), thousands_sep: String::new(), grouping: Vec::new() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FormatError {
    /// The specifier is malformed.
    Invalid,
    /// A width or precision does not fit an index.
    Overflow,
    /// The locale grouping is malformed.
    Grouping(&'static str),
    /// The result would exceed the allocation ceiling.
    Memory,
}

impl From<Mem> for FormatError {
    fn from(_: Mem) -> FormatError {
        FormatError::Memory
    }
}

/// A parsed format specifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatSpec {
    pub fill: char,
    pub align: char,
    pub sign: char,
    pub no_neg_0: bool,
    pub alt: bool,
    pub zeropad: bool,
    pub min_width: usize,
    pub thousands: bool,
    pub precision: Option<usize>,
    /// `e E f F g G %`, or `None` when absent. `n` is stored as `g` with `locale` set.
    pub ty: Option<char>,
    /// The `n` type: separators come from the locale.
    pub locale: bool,
}

struct Rest {
    sign: Option<char>,
    no_neg_0: bool,
    alt: bool,
    zeropad: bool,
    min_width: Option<usize>,
    thousands: bool,
    precision: Option<usize>,
    ty: Option<char>,
}

fn parse_number(s: &[char], mut i: usize, allow_leading_zero: bool) -> Result<Option<(usize, usize)>, FormatError> {
    let start = i;
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    if i == start || (!allow_leading_zero && s[start] == '0') {
        return Ok(None);
    }
    let mut v: usize = 0;
    for c in &s[start..i] {
        v = v.checked_mul(10).and_then(|v| v.checked_add(c.to_digit(10).unwrap_or(0) as usize)).ok_or(FormatError::Overflow)?;
    }
    Ok(Some((v, i)))
}

fn parse_rest(s: &[char]) -> Result<Option<Rest>, FormatError> {
    let mut i = 0;
    let mut r = Rest { sign: None, no_neg_0: false, alt: false, zeropad: false, min_width: None, thousands: false, precision: None, ty: None };
    if i < s.len() && matches!(s[i], '-' | '+' | ' ') {
        r.sign = Some(s[i]);
        i += 1;
    }
    if i < s.len() && s[i] == 'z' {
        r.no_neg_0 = true;
        i += 1;
    }
    if i < s.len() && s[i] == '#' {
        r.alt = true;
        i += 1;
    }
    if i < s.len() && s[i] == '0' {
        r.zeropad = true;
        i += 1;
    }
    if let Some((w, j)) = parse_number(s, i, false)? {
        r.min_width = Some(w);
        i = j;
    }
    if i < s.len() && s[i] == ',' {
        r.thousands = true;
        i += 1;
    }
    if i < s.len() && s[i] == '.' {
        let p = i + 1;
        let mut j = p;
        while j < s.len() && s[j].is_ascii_digit() {
            j += 1;
        }
        let digits = &s[p..j];
        if digits.is_empty() || (digits[0] == '0' && digits.len() > 1) {
            return Ok(None);
        }
        // A precision too large for an index is clamped: it cannot add digits anyway.
        let value = digits.iter().try_fold(0usize, |v, c| v.checked_mul(10)?.checked_add(c.to_digit(10)? as usize));
        r.precision = Some(value.unwrap_or(usize::MAX / 4));
        i = j;
    }
    if i < s.len() && matches!(s[i], 'e' | 'E' | 'f' | 'F' | 'g' | 'G' | 'n' | '%') {
        r.ty = Some(s[i]);
        i += 1;
    }
    Ok(if i == s.len() { Some(r) } else { None })
}

/// Parses a format specifier.
pub fn parse_format_spec(text: &str) -> Result<FormatSpec, FormatError> {
    let s: Vec<char> = text.chars().collect();
    let is_align = |c: char| matches!(c, '<' | '>' | '=' | '^');
    let mut attempts: Vec<(Option<char>, Option<char>, usize)> = Vec::new();
    if s.len() >= 2 && is_align(s[1]) {
        attempts.push((Some(s[0]), Some(s[1]), 2));
    }
    if !s.is_empty() && is_align(s[0]) {
        attempts.push((None, Some(s[0]), 1));
    }
    attempts.push((None, None, 0));
    let mut found = None;
    for (fill, align, skip) in attempts {
        if let Some(rest) = parse_rest(&s[skip..])? {
            found = Some((fill, align, rest));
            break;
        }
    }
    let Some((fill, align, rest)) = found else { return Err(FormatError::Invalid) };
    if rest.zeropad && (fill.is_some() || align.is_some()) {
        return Err(FormatError::Invalid);
    }
    let locale = rest.ty == Some('n');
    if locale && rest.thousands {
        return Err(FormatError::Invalid);
    }
    let mut precision = rest.precision;
    if precision == Some(0) && matches!(rest.ty, None | Some('g') | Some('G') | Some('n')) {
        precision = Some(1);
    }
    Ok(FormatSpec {
        fill: fill.unwrap_or(' '),
        align: align.unwrap_or('>'),
        sign: rest.sign.unwrap_or('-'),
        no_neg_0: rest.no_neg_0,
        alt: rest.alt,
        zeropad: rest.zeropad,
        min_width: rest.min_width.unwrap_or(0),
        thousands: rest.thousands,
        precision,
        ty: if locale { Some('g') } else { rest.ty },
        locale,
    })
}

fn format_sign(negative: bool, spec: &FormatSpec) -> &'static str {
    if negative {
        "-"
    } else if spec.sign == '+' {
        "+"
    } else if spec.sign == ' ' {
        " "
    } else {
        ""
    }
}

fn format_align(sign: &str, body: &str, spec: &FormatSpec) -> String {
    let used = sign.chars().count() + body.chars().count();
    let pad = spec.min_width.saturating_sub(used);
    let padding: String = std::iter::repeat(spec.fill).take(pad).collect();
    match spec.align {
        '<' => format!("{sign}{body}{padding}"),
        '=' => format!("{sign}{padding}{body}"),
        '^' => {
            let half = pad / 2;
            let left: String = std::iter::repeat(spec.fill).take(half).collect();
            let right: String = std::iter::repeat(spec.fill).take(pad - half).collect();
            format!("{left}{sign}{body}{right}")
        }
        _ => format!("{padding}{sign}{body}"),
    }
}

/// The length of the `index`th group from the right, `None` once the grouping is exhausted.
fn group_length(grouping: &[i32], index: usize) -> Result<Option<i32>, FormatError> {
    match grouping.last() {
        None => Ok(None),
        Some(0) if grouping.len() >= 2 => {
            let fixed = &grouping[..grouping.len() - 1];
            Ok(Some(if index < fixed.len() { fixed[index] } else { fixed[fixed.len() - 1] }))
        }
        Some(127) => Ok(grouping[..grouping.len() - 1].get(index).copied()),
        _ => Err(FormatError::Grouping("unrecognised format for grouping")),
    }
}

fn zeros(n: i64) -> String {
    "0".repeat(n.max(0) as usize)
}

fn insert_thousands_sep(digits: &str, sep: &str, grouping: &[i32], mut min_width: i64) -> Result<String, FormatError> {
    let mut rest: &str = digits;
    let mut groups: Vec<String> = Vec::new();
    let sep_len = sep.chars().count() as i64;
    let mut index = 0;
    let mut broke = false;
    while let Some(l) = group_length(grouping, index)? {
        index += 1;
        if l <= 0 {
            return Err(FormatError::Grouping("group length should be positive"));
        }
        let l = (rest.len() as i64).max(min_width).max(1).min(l as i64);
        let take = (l as usize).min(rest.len());
        groups.push(format!("{}{}", zeros(l - rest.len() as i64), &rest[rest.len() - take..]));
        rest = &rest[..rest.len() - take];
        min_width -= l;
        if rest.is_empty() && min_width <= 0 {
            broke = true;
            break;
        }
        min_width -= sep_len;
    }
    if !broke {
        let l = (rest.len() as i64).max(min_width).max(1);
        groups.push(format!("{}{}", zeros(l - rest.len() as i64), rest));
    }
    groups.reverse();
    Ok(groups.join(sep))
}

fn format_number(
    negative: bool,
    intpart: &str,
    fracpart: &str,
    exp: i128,
    spec: &FormatSpec,
    point: &str,
    sep: &str,
    grouping: &[i32],
) -> Result<String, FormatError> {
    let sign = format_sign(negative, spec);
    let ty = spec.ty.unwrap_or('g');
    let mut frac = if !fracpart.is_empty() || spec.alt { format!("{point}{fracpart}") } else { String::new() };
    if exp != 0 || matches!(ty, 'e' | 'E') {
        let echar = if matches!(ty, 'E' | 'G') { 'E' } else { 'e' };
        frac.push_str(&format!("{echar}{exp:+}"));
    }
    if ty == '%' {
        frac.push('%');
    }
    let min_width = if spec.zeropad { spec.min_width as i64 - frac.chars().count() as i64 - sign.len() as i64 } else { 0 };
    let int = insert_thousands_sep(intpart, sep, grouping, min_width)?;
    Ok(format_align(sign, &format!("{int}{frac}"), spec))
}

impl Decimal {
    /// Formats the value. `locale` supplies the decimal point, separator and grouping for the
    /// `n` type; given for any other type it overrides the defaults too.
    pub fn format(&self, spec: &FormatSpec, locale: Option<&Locale>, ctx: &Context) -> Result<String, FormatError> {
        let fixed = Locale { decimal_point: ".".into(), thousands_sep: if spec.thousands { ",".into() } else { String::new() }, grouping: vec![3, 0] };
        let loc = match (locale, spec.locale) {
            (Some(l), _) => l.clone(),
            (None, true) => Locale::default(),
            (None, false) => fixed,
        };
        let ty0 = spec.ty;
        if self.kind != Kind::Finite {
            let sign = format_sign(self.sign, spec);
            let mut body = self.copy_abs().to_sci_string(ctx.capitals);
            if ty0 == Some('%') {
                body.push('%');
            }
            return Ok(format_align(sign, &body, spec));
        }
        let ty = ty0.unwrap_or(if ctx.capitals { 'G' } else { 'g' });
        let mut spec = spec.clone();
        spec.ty = Some(ty);
        let spec = &spec;
        let mut value = self.clone();
        if ty == '%' {
            value = Decimal { exp: value.exp.saturating_add(2), ..value };
        }
        let rounding = ctx.round;
        if let Some(prec) = spec.precision {
            let prec = prec as u64;
            let limit = super::coef::MAX_DIGITS;
            match ty {
                'e' | 'E' => {
                    if prec >= limit {
                        return Err(FormatError::Memory);
                    }
                    value = value.round_places(prec + 1, rounding)?;
                }
                'f' | 'F' | '%' => {
                    if prec >= limit {
                        return Err(FormatError::Memory);
                    }
                    value = value.rescale(-(prec as i128), rounding)?;
                }
                _ => {
                    if value.digits() > prec {
                        value = value.round_places(prec, rounding)?;
                    }
                }
            }
        }
        if value.is_zero() && value.exp > 0 && matches!(ty, 'f' | 'F' | '%') {
            value = value.rescale(0, rounding)?;
        }
        let negative = if value.is_zero() && spec.no_neg_0 && value.sign { false } else { value.sign };
        let digits = value.coef.to_string_radix(10);
        let ndig = digits.len() as i128;
        let leftdigits = value.exp as i128 + ndig;
        let dotplace: i128 = match ty {
            'e' | 'E' => match (value.is_zero(), spec.precision) {
                (true, Some(p)) => 1 - p as i128,
                _ => 1,
            },
            'f' | 'F' | '%' => leftdigits,
            _ => {
                if value.exp <= 0 && leftdigits > -6 {
                    leftdigits
                } else {
                    1
                }
            }
        };
        let (intpart, fracpart) = if dotplace < 0 {
            ("0".to_string(), format!("{}{}", "0".repeat((-dotplace) as usize), digits))
        } else if dotplace > ndig {
            (format!("{}{}", digits, "0".repeat((dotplace - ndig) as usize)), String::new())
        } else {
            let d = dotplace as usize;
            let int = if d == 0 { "0".to_string() } else { digits[..d].to_string() };
            (int, digits[d..].to_string())
        };
        let exp = leftdigits - dotplace;
        format_number(negative, &intpart, &fracpart, exp, spec, &loc.decimal_point, &loc.thousands_sep, &loc.grouping)
    }
}

