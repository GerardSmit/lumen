//! The format-spec mini-language, `str.format` and `%` formatting.

use crate::pyint::{BigInt, PyInt};
use crate::num::{float_repr, to_num, Num};
use crate::object::*;
use crate::vm::*;

pub use crate::repr::ascii_escape;

#[derive(Default, Clone)]
pub struct Spec {
    pub fill: Option<char>,
    pub align: Option<char>,
    pub sign: Option<char>,
    pub alt: bool,
    pub zero: bool,
    pub z: bool,
    pub width: Option<usize>,
    pub grouping: Option<char>,
    pub precision: Option<usize>,
    pub ty: Option<char>,
}

fn pad(s: &str, width: usize, fill: char, align: char) -> String {
    let n = s.chars().count();
    if n >= width {
        return s.to_string();
    }
    let total = width - n;
    let (l, r) = match align {
        '<' => (0, total),
        '>' => (total, 0),
        '^' => (total / 2, total - total / 2),
        _ => (total, 0),
    };
    let mut out = String::with_capacity(s.len() + total);
    for _ in 0..l {
        out.push(fill);
    }
    out.push_str(s);
    for _ in 0..r {
        out.push(fill);
    }
    out
}

fn group_digits(digits: &str, sep: char, size: usize) -> String {
    let chars: Vec<char> = digits.chars().collect();
    let mut out = String::new();
    for (i, c) in chars.iter().enumerate() {
        if i > 0 && (chars.len() - i) % size == 0 {
            out.push(sep);
        }
        out.push(*c);
    }
    out
}

impl Interp {
    pub fn parse_spec(&mut self, spec: &str, tname: &str) -> R<Spec> {
        let chars: Vec<char> = spec.chars().collect();
        let mut i = 0;
        let mut s = Spec::default();
        let bad = |it: &mut Interp| it.value_error(&format!("Invalid format specifier '{}' for object of type '{}'", spec, tname));
        if chars.len() >= 2 && matches!(chars[1], '<' | '>' | '^' | '=') {
            s.fill = Some(chars[0]);
            s.align = Some(chars[1]);
            i = 2;
        } else if !chars.is_empty() && matches!(chars[0], '<' | '>' | '^' | '=') {
            s.align = Some(chars[0]);
            i = 1;
        }
        if i < chars.len() && matches!(chars[i], '+' | '-' | ' ') {
            s.sign = Some(chars[i]);
            i += 1;
        }
        if i < chars.len() && chars[i] == 'z' {
            s.z = true;
            i += 1;
        }
        if i < chars.len() && chars[i] == '#' {
            s.alt = true;
            i += 1;
        }
        if i < chars.len() && chars[i] == '0' {
            s.zero = true;
            i += 1;
        }
        let start = i;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        if i > start {
            let w: String = chars[start..i].iter().collect();
            let w: usize = w.parse().ok().filter(|&w| w <= i64::MAX as usize).ok_or_else(|| self.value_error("Too many decimal digits in format string"))?;
            self.check_str_len(w)?;
            s.width = Some(w);
        }
        if i < chars.len() && (chars[i] == ',' || chars[i] == '_') {
            s.grouping = Some(chars[i]);
            i += 1;
            if i < chars.len() && (chars[i] == ',' || chars[i] == '_') {
                return Err(self.value_error("Cannot specify both ',' and '_'."));
            }
        }
        if i < chars.len() && chars[i] == '.' {
            i += 1;
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            if i == start {
                return Err(self.value_error("Format specifier missing precision"));
            }
            let p: String = chars[start..i].iter().collect();
            let p: usize = p.parse().ok().filter(|&p| p <= i64::MAX as usize).ok_or_else(|| self.value_error("Too many decimal digits in format string"))?;
            s.precision = Some(p);
        }
        if i < chars.len() {
            s.ty = Some(chars[i]);
            i += 1;
        }
        if i < chars.len() {
            return Err(bad(self));
        }
        Ok(s)
    }

    pub fn format_value(&mut self, v: &Value, spec: &str) -> R<String> {
        if let Value::Obj(o) = v {
            if o.cls.is_some() {
                if let Some(m) = self.user_special(v, "__format__") {
                    let r = self.call_user_special(v, &m, vec![Value::str(spec)])?;
                    return match r.as_str() {
                        Some(s) => Ok(s.to_string()),
                        None => {
                            let t = self.type_name_of(&r);
                            Err(self.type_error(&format!("__format__ must return a str, not {}", t)))
                        }
                    };
                }
            }
        }
        self.native_format(v, spec)
    }

    pub fn native_format(&mut self, v: &Value, spec: &str) -> R<String> {
        match v {
            Value::Obj(o) if matches!(o.kind, Kind::Str(_)) => {
                let s = v.as_str().unwrap_or("").to_string();
                return self.format_str(&s, spec);
            }
            Value::Bool(b) => {
                if spec.is_empty() {
                    return Ok(if *b { "True".into() } else { "False".into() });
                }
                return self.format_int(&BigInt::from_i64(*b as i64), spec);
            }
            _ => {}
        }
        match to_num(v) {
            Some(Num::I(i)) => return self.format_int(&BigInt::from_i64(i), spec),
            Some(Num::B(b)) => return self.format_int(&b, spec),
            Some(Num::F(f)) => return self.format_float(f, spec),
            None => {}
        }
        if let Value::Obj(o) = v {
            if let Kind::Complex(re, im) = &o.kind {
                if spec.is_empty() {
                    return Ok(crate::repr::complex_repr(*re, *im));
                }
                let sp = self.parse_spec(spec, "complex")?;
                if matches!(sp.align, Some('=')) {
                    return Err(self.value_error("'=' alignment flag is not allowed in complex format specifier"));
                }
                let body = match sp.ty {
                    None if sp.precision.is_none() => crate::repr::complex_repr(*re, *im),
                    ty => {
                        let ty = ty.unwrap_or('r');
                        let part_spec = |plus: bool| {
                            let mut s = String::new();
                            if plus {
                                s.push('+');
                            } else if let Some(sg) = sp.sign {
                                s.push(sg);
                            }
                            if sp.alt {
                                s.push('#');
                            }
                            if let Some(p) = sp.precision {
                                s.push_str(&format!(".{}", p));
                            }
                            if ty != 'r' {
                                s.push(ty);
                            }
                            s
                        };
                        let show_re = !(*re == 0.0 && re.is_sign_positive() && sp.ty.is_none());
                        let i = self.format_float(*im, &part_spec(show_re))?;
                        let j = if matches!(ty, 'E' | 'F' | 'G') { 'J' } else { 'j' };
                        if show_re {
                            let r = self.format_float(*re, &part_spec(false))?;
                            let paren = sp.ty.is_none();
                            format!("{}{}{}{}{}{}", if paren { "(" } else { "" }, r, i, j, "", if paren { ")" } else { "" })
                        } else {
                            format!("{}{}", i, j)
                        }
                    }
                };
                return Ok(pad(&body, sp.width.unwrap_or(0), sp.fill.unwrap_or(' '), sp.align.unwrap_or('>')));
            }
        }
        if spec.is_empty() {
            return self.str_of(v);
        }
        let t = self.type_name_of(v);
        Err(self.type_error(&format!("unsupported format string passed to {}.__format__", t)))
    }

    fn format_str(&mut self, s: &str, spec: &str) -> R<String> {
        if spec.is_empty() {
            return Ok(s.to_string());
        }
        let sp = self.parse_spec(spec, "str")?;
        if sp.z {
            return Err(self.value_error("Negative zero coercion (z) not allowed in format specifier"));
        }
        match sp.ty {
            None | Some('s') => {}
            Some(c) => return Err(self.value_error(&format!("Unknown format code '{}' for object of type 'str'", c))),
        }
        if sp.sign.is_some() {
            return Err(self.value_error("Sign not allowed in string format specifier"));
        }
        if sp.alt {
            return Err(self.value_error("Alternate form (#) not allowed in string format specifier"));
        }
        if sp.align == Some('=') {
            return Err(self.value_error("'=' alignment not allowed in string format specifier"));
        }
        if let Some(g) = sp.grouping {
            return Err(self.value_error(&format!("Cannot specify '{}' with 's'.", g)));
        }
        let mut body: String = s.to_string();
        if let Some(p) = sp.precision {
            body = body.chars().take(p).collect();
        }
        let fill = sp.fill.unwrap_or(if sp.zero { '0' } else { ' ' });
        Ok(pad(&body, sp.width.unwrap_or(0), fill, sp.align.unwrap_or('<')))
    }

    fn apply_number_layout(&self, sp: &Spec, sign: &str, prefix: &str, digits: &str, default_align: char) -> String {
        let width = sp.width.unwrap_or(0);
        let fill = sp.fill.unwrap_or(if sp.zero { '0' } else { ' ' });
        let align = sp.align.unwrap_or(if sp.zero { '=' } else { default_align });
        let body_len = sign.chars().count() + prefix.chars().count() + digits.chars().count();
        if align == '=' {
            let mut d = digits.to_string();
            if body_len < width {
                let need = width - body_len;
                if fill == '0' && sp.grouping.is_some() {
                    let sep = sp.grouping.unwrap_or(',');
                    let size = if matches!(sp.ty, Some('b' | 'o' | 'x' | 'X')) { 4 } else { 3 };
                    let hex = size == 4;
                    let split = digits.find(|c: char| !((hex && c.is_ascii_alphanumeric()) || c.is_ascii_digit() || c == sep)).unwrap_or(digits.len());
                    let (int_part, rest) = digits.split_at(split);
                    let mut plain: String = int_part.chars().filter(|c| *c != sep).collect();
                    let total = width - sign.chars().count() - prefix.chars().count();
                    let mut g = group_digits(&plain, sep, size);
                    while g.chars().count() + rest.chars().count() < total {
                        plain.insert(0, '0');
                        g = group_digits(&plain, sep, size);
                    }
                    d = format!("{}{}", g, rest);
                    return format!("{}{}{}", sign, prefix, d);
                }
                let mut out = String::new();
                out.push_str(sign);
                out.push_str(prefix);
                for _ in 0..need {
                    out.push(fill);
                }
                out.push_str(&d);
                return out;
            }
            return format!("{}{}{}", sign, prefix, d);
        }
        let s = format!("{}{}{}", sign, prefix, digits);
        pad(&s, width, fill, align)
    }

    fn format_int(&mut self, n: &BigInt, spec: &str) -> R<String> {
        if spec.is_empty() {
            return self.int_to_decimal(n);
        }
        let sp = self.parse_spec(spec, "int")?;
        if sp.z && !matches!(sp.ty, Some('e' | 'E' | 'f' | 'F' | 'g' | 'G' | '%')) {
            return Err(self.value_error("Negative zero coercion (z) not allowed in format specifier"));
        }
        let ty = sp.ty.unwrap_or('d');
        if matches!(ty, 'e' | 'E' | 'f' | 'F' | 'g' | 'G' | '%') {
            let f = match n.to_float() {
                Some(f) => f,
                None => return Err(self.overflow_err("int too large to convert to float")),
            };
            return self.format_float(f, spec);
        }
        if !matches!(ty, 'd' | 'b' | 'o' | 'x' | 'X' | 'c' | 'n') {
            return Err(self.value_error(&format!("Unknown format code '{}' for object of type 'int'", ty)));
        }
        if sp.precision.is_some() {
            return Err(self.value_error("Precision not allowed in integer format specifier"));
        }
        if ty == 'c' {
            if sp.sign.is_some() {
                return Err(self.value_error("Sign not allowed with integer format specifier 'c'"));
            }
            let c = n.to_i64().and_then(|i| u32::try_from(i).ok()).and_then(char::from_u32);
            let c = match c {
                Some(c) => c,
                None => return Err(self.overflow_err("%c arg not in range(0x110000)")),
            };
            let fill = sp.fill.unwrap_or(' ');
            return Ok(pad(&c.to_string(), sp.width.unwrap_or(0), fill, sp.align.unwrap_or('>')));
        }
        let neg = n.is_negative();
        let mag = n.abs();
        let (radix, prefix) = match ty {
            'b' => (2, "0b"),
            'o' => (8, "0o"),
            'x' | 'X' => (16, "0x"),
            _ => (10, ""),
        };
        let mut digits = if radix == 10 { self.int_to_decimal(&mag)? } else { mag.to_string_radix(radix) };
        if ty == 'X' {
            digits = digits.to_uppercase();
        }
        if let Some(g) = sp.grouping {
            let size = if radix == 10 { 3 } else { 4 };
            if radix != 10 && g == ',' {
                return Err(self.value_error(&format!("Cannot specify ',' with '{}'.", ty)));
            }
            digits = group_digits(&digits, g, size);
        }
        let sign = if neg {
            "-"
        } else {
            match sp.sign {
                Some('+') => "+",
                Some(' ') => " ",
                _ => "",
            }
        };
        let prefix = if sp.alt {
            if ty == 'X' {
                "0X"
            } else {
                prefix
            }
        } else {
            ""
        };
        Ok(self.apply_number_layout_int(&sp, sign, prefix, &digits))
    }

    fn apply_number_layout_int(&self, sp: &Spec, sign: &str, prefix: &str, digits: &str) -> String {
        self.apply_number_layout(sp, sign, prefix, digits, '>')
    }

    pub fn format_float(&mut self, f: f64, spec: &str) -> R<String> {
        if spec.is_empty() {
            return Ok(float_repr(f));
        }
        let sp = self.parse_spec(spec, "float")?;
        if let Some(p) = sp.precision {
            if p > i32::MAX as usize {
                return Err(self.value_error("precision too big"));
            }
            self.check_str_len(p)?;
        }
        let ty = sp.ty;
        if let Some(c) = ty {
            if !matches!(c, 'e' | 'E' | 'f' | 'F' | 'g' | 'G' | 'n' | '%') {
                return Err(self.value_error(&format!("Unknown format code '{}' for object of type 'float'", c)));
            }
        }
        let neg = f.is_sign_negative() && !f.is_nan();
        let a = f.abs();
        let upper = matches!(ty, Some('E') | Some('F') | Some('G'));
        let mut body = if a.is_nan() {
            "nan".to_string()
        } else if a.is_infinite() {
            "inf".to_string()
        } else {
            match ty {
                Some('f') | Some('F') => format!("{:.*}", sp.precision.unwrap_or(6), a),
                Some('e') | Some('E') => fmt_exp(a, sp.precision.unwrap_or(6), sp.alt),
                Some('%') => format!("{:.*}", sp.precision.unwrap_or(6), a * 100.0),
                Some('g') | Some('G') | Some('n') => fmt_general(a, sp.precision.unwrap_or(6), sp.alt),
                _ => match sp.precision {
                    None => float_repr(a),
                    Some(p) => {
                        let g = fmt_general(a, p, sp.alt);
                        if g.contains('.') || g.contains('e') || g.contains("inf") || g.contains("nan") {
                            g
                        } else {
                            format!("{}.0", g)
                        }
                    }
                },
            }
        };
        if sp.alt && !body.contains('.') && !body.contains('e') && !a.is_nan() && !a.is_infinite() && matches!(ty, Some('f') | Some('F') | Some('%')) {
            body.push('.');
        }
        if ty == Some('%') {
            body.push('%');
        }
        if upper {
            body = body.to_uppercase();
        }
        if let Some(g) = sp.grouping {
            let (int_part, rest) = match body.find(|c: char| !c.is_ascii_digit()) {
                Some(i) => (body[..i].to_string(), body[i..].to_string()),
                None => (body.clone(), String::new()),
            };
            body = format!("{}{}", group_digits(&int_part, g, 3), rest);
        }
        let neg = neg && !(sp.z && !body.chars().any(|c| c.is_ascii_digit() && c != '0'));
        let sign = if neg {
            "-"
        } else {
            match sp.sign {
                Some('+') => "+",
                Some(' ') => " ",
                _ => "",
            }
        };
        Ok(self.apply_number_layout(&sp, sign, "", &body, '>'))
    }

    // ---- str.format --------------------------------------------------------------------------

    pub fn str_format(&mut self, fmt: &str, args: &[Value], kw: &[(Obj, Value)]) -> R<String> {
        let mut auto = 0usize;
        let mut manual = false;
        let mut auto_used = false;
        self.format_template(fmt, args, kw, &mut auto, &mut manual, &mut auto_used, 2)
    }

    #[allow(clippy::too_many_arguments)]
    fn format_template(&mut self, fmt: &str, args: &[Value], kw: &[(Obj, Value)], auto: &mut usize, manual: &mut bool, auto_used: &mut bool, depth: u32) -> R<String> {
        if depth == 0 {
            return Err(self.value_error("Max string recursion exceeded"));
        }
        let chars: Vec<char> = fmt.chars().collect();
        let mut out = String::new();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c == '{' {
                if i + 1 < chars.len() && chars[i + 1] == '{' {
                    out.push('{');
                    i += 2;
                    continue;
                }
                let mut j = i + 1;
                let mut level = 1;
                while j < chars.len() {
                    match chars[j] {
                        '{' => level += 1,
                        '}' => {
                            level -= 1;
                            if level == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                if j >= chars.len() {
                    if i + 1 == chars.len() {
                        return Err(self.value_error("Single '{' encountered in format string"));
                    }
                    return Err(self.value_error("expected '}' before end of string"));
                }
                let field: String = chars[i + 1..j].iter().collect();
                let r = self.format_field(&field, args, kw, auto, manual, auto_used, depth)?;
                out.push_str(&r);
                i = j + 1;
            } else if c == '}' {
                if i + 1 < chars.len() && chars[i + 1] == '}' {
                    out.push('}');
                    i += 2;
                } else {
                    return Err(self.value_error("Single '}' encountered in format string"));
                }
            } else {
                out.push(c);
                i += 1;
            }
        }
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    fn format_field(&mut self, field: &str, args: &[Value], kw: &[(Obj, Value)], auto: &mut usize, manual: &mut bool, auto_used: &mut bool, depth: u32) -> R<String> {
        let chars: Vec<char> = field.chars().collect();
        let mut i = 0;
        let mut bracket = 0;
        while i < chars.len() {
            match chars[i] {
                '[' => bracket += 1,
                ']' => bracket -= 1,
                '!' | ':' if bracket <= 0 => break,
                _ => {}
            }
            i += 1;
        }
        let name: String = chars[..i].iter().collect();
        let mut conv: Option<char> = None;
        let mut spec = String::new();
        if i < chars.len() && chars[i] == '!' {
            if i + 1 >= chars.len() {
                return Err(self.value_error("end of string while looking for conversion specifier"));
            }
            conv = Some(chars[i + 1]);
            i += 2;
            if i < chars.len() && chars[i] != ':' {
                return Err(self.value_error("expected ':' after conversion specifier"));
            }
        }
        if i < chars.len() && chars[i] == ':' {
            spec = chars[i + 1..].iter().collect();
        }
        let first_end = name.find(['.', '[']).unwrap_or(name.len());
        let first = &name[..first_end];
        let rest = &name[first_end..];
        let mut value = if first.is_empty() {
            if *manual {
                return Err(self.value_error("cannot switch from manual field specification to automatic field numbering"));
            }
            *auto_used = true;
            let idx = *auto;
            *auto += 1;
            match args.get(idx) {
                Some(v) => v.clone(),
                None => return Err(self.new_exc_str("IndexError", &format!("Replacement index {} out of range for positional args tuple", idx))),
            }
        } else if first.chars().all(|c| c.is_ascii_digit()) {
            if *auto_used {
                return Err(self.value_error("cannot switch from automatic field numbering to manual field specification"));
            }
            *manual = true;
            let idx: usize = first.parse().unwrap_or(usize::MAX);
            match args.get(idx) {
                Some(v) => v.clone(),
                None => return Err(self.new_exc_str("IndexError", &format!("Replacement index {} out of range for positional args tuple", idx))),
            }
        } else {
            match Interp::kw_get(kw, first) {
                Some(v) => v.clone(),
                None => return Err(self.new_exc_val("KeyError", Value::str(first))),
            }
        };
        let rc: Vec<char> = rest.chars().collect();
        let mut k = 0;
        while k < rc.len() {
            if rc[k] == '.' {
                let mut e = k + 1;
                while e < rc.len() && rc[e] != '.' && rc[e] != '[' {
                    e += 1;
                }
                let attr: String = rc[k + 1..e].iter().collect();
                value = self.get_attr_str(&value, &attr)?;
                k = e;
            } else if rc[k] == '[' {
                let mut e = k + 1;
                while e < rc.len() && rc[e] != ']' {
                    e += 1;
                }
                if e >= rc.len() {
                    return Err(self.value_error("Missing ']' in format string"));
                }
                let key: String = rc[k + 1..e].iter().collect();
                let kv = if !key.is_empty() && key.chars().all(|c| c.is_ascii_digit()) { Value::Int(key.parse().unwrap_or(0)) } else { Value::str(&key) };
                value = self.getitem(&value, &kv)?;
                k = e + 1;
            } else {
                return Err(self.value_error("Only '.' or '[' may follow ']' in format field specifier"));
            }
        }
        value = match conv {
            None => value,
            Some('r') => Value::string(self.repr_of(&value)?),
            Some('s') => Value::string(self.str_of(&value)?),
            Some('a') => Value::string(ascii_escape(&self.repr_of(&value)?)),
            Some(c) => return Err(self.value_error(&format!("Unknown conversion specifier {}", c))),
        };
        let spec = if spec.contains('{') { self.format_template(&spec, args, kw, auto, manual, auto_used, depth - 1)? } else { spec };
        self.format_value(&value, &spec)
    }
}

fn fmt_exp(a: f64, prec: usize, alt: bool) -> String {
    let s = format!("{:.*e}", prec, a);
    let (m, e) = s.split_once('e').unwrap_or((&s, "0"));
    let ev: i32 = e.parse().unwrap_or(0);
    let m = if alt && prec == 0 { format!("{}.", m) } else { m.to_string() };
    format!("{}e{}{:02}", m, if ev < 0 { '-' } else { '+' }, ev.abs())
}

fn fmt_general(a: f64, prec: usize, alt: bool) -> String {
    let p = if prec == 0 { 1 } else { prec };
    if a == 0.0 {
        return if alt { format!("0.{}", "0".repeat(p - 1)) } else { "0".into() };
    }
    let es = format!("{:.*e}", p - 1, a);
    let (_, e) = es.split_once('e').unwrap_or(("", "0"));
    let x: i32 = e.parse().unwrap_or(0);
    if x >= -4 && x < p as i32 {
        let decimals = (p as i32 - 1 - x).max(0) as usize;
        let s = format!("{:.*}", decimals, a);
        if alt {
            s
        } else {
            strip_zeros(&s)
        }
    } else {
        let s = fmt_exp(a, p - 1, alt);
        if alt {
            s
        } else {
            let (m, e) = s.split_once('e').unwrap_or((&s, ""));
            format!("{}e{}", strip_zeros(m), e)
        }
    }
}

fn strip_zeros(s: &str) -> String {
    if s.contains('.') {
        let t = s.trim_end_matches('0');
        t.trim_end_matches('.').to_string()
    } else {
        s.to_string()
    }
}

// ---- % formatting ---------------------------------------------------------------------------------

pub fn percent_format(it: &mut Interp, fmt: &Value, args: &Value) -> R<String> {
    let f: Vec<char> = fmt.as_str().unwrap_or("").chars().collect();
    let (items, single_dict): (Vec<Value>, Option<Value>) = match args.tuple_items() {
        Some(t) => (t.to_vec(), None),
        None => {
            let is_map = matches!(args, Value::Obj(o) if matches!(o.kind, Kind::Dict(_)))
                || (matches!(args, Value::Obj(o) if o.cls.is_some()) && it.lookup_mro(&it.type_of(args), "__getitem__").is_some() && !matches!(args, Value::Obj(o) if matches!(o.kind, Kind::Str(_) | Kind::Tuple(_) | Kind::List(_))));
            (vec![args.clone()], if is_map { Some(args.clone()) } else { None })
        }
    };
    let mut next = 0usize;
    let mut out = String::new();
    let mut i = 0;
    while i < f.len() {
        let c = f[i];
        if c != '%' {
            out.push(c);
            i += 1;
            continue;
        }
        i += 1;
        let mut key: Option<String> = None;
        if i < f.len() && f[i] == '(' {
            let mut depth = 1;
            let mut j = i + 1;
            while j < f.len() && depth > 0 {
                match f[j] {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                j += 1;
            }
            if depth != 0 {
                return Err(it.value_error("incomplete format key"));
            }
            key = Some(f[i + 1..j - 1].iter().collect());
            i = j;
        }
        let (mut minus, mut plus, mut space, mut alt, mut zero) = (false, false, false, false, false);
        while i < f.len() {
            match f[i] {
                '-' => minus = true,
                '+' => plus = true,
                ' ' => space = true,
                '#' => alt = true,
                '0' => zero = true,
                _ => break,
            }
            i += 1;
        }
        let mut width: Option<usize> = None;
        if i < f.len() && f[i] == '*' {
            let v = items.get(next).cloned();
            next += 1;
            let w = match v {
                Some(v) => it.index_of(&v)?,
                None => return Err(it.type_error("not enough arguments for format string")),
            };
            if w < 0 {
                minus = true;
            }
            width = Some(w.unsigned_abs() as usize);
            i += 1;
        } else {
            let st = i;
            while i < f.len() && f[i].is_ascii_digit() {
                i += 1;
            }
            if i > st {
                let w: usize = f[st..i].iter().collect::<String>().parse().ok().filter(|&w| w <= i64::MAX as usize).ok_or_else(|| it.value_error("width too big"))?;
                width = Some(w);
            }
        }
        if let Some(w) = width {
            it.check_str_len(w)?;
        }
        let mut prec: Option<usize> = None;
        if i < f.len() && f[i] == '.' {
            i += 1;
            if i < f.len() && f[i] == '*' {
                let v = items.get(next).cloned();
                next += 1;
                let p = match v {
                    Some(v) => it.index_of(&v)?,
                    None => return Err(it.type_error("not enough arguments for format string")),
                };
                prec = Some(p.max(0) as usize);
                i += 1;
            } else {
                let st = i;
                while i < f.len() && f[i].is_ascii_digit() {
                    i += 1;
                }
                let digits: String = f[st..i].iter().collect();
                prec = Some(if digits.is_empty() { 0 } else { digits.parse().map_err(|_| it.value_error("precision too big"))? });
            }
            if prec.is_some_and(|p| p > i32::MAX as usize) {
                return Err(it.value_error("precision too big"));
            }
            if let Some(p) = prec {
                it.check_str_len(p)?;
            }
        }
        while i < f.len() && matches!(f[i], 'l' | 'h' | 'L') {
            i += 1;
        }
        if i >= f.len() {
            return Err(it.value_error("incomplete format"));
        }
        let ty = f[i];
        i += 1;
        if ty == '%' {
            out.push('%');
            continue;
        }
        let arg = if let Some(k) = &key {
            match &single_dict {
                Some(d) => it.getitem(d, &Value::str(k))?,
                None => return Err(it.type_error("format requires a mapping")),
            }
        } else {
            let v = items.get(next).cloned();
            next += 1;
            match v {
                Some(v) => v,
                None => return Err(it.type_error("not enough arguments for format string")),
            }
        };
        let body: String;
        let mut numeric = false;
        let mut sign = String::new();
        match ty {
            's' | 'r' | 'a' => {
                let mut s = match ty {
                    's' => it.str_of(&arg)?,
                    'r' => it.repr_of(&arg)?,
                    _ => ascii_escape(&it.repr_of(&arg)?),
                };
                if let Some(p) = prec {
                    s = s.chars().take(p).collect();
                }
                body = s;
            }
            'c' => {
                body = match &arg {
                    Value::Obj(o) if matches!(o.kind, Kind::Str(_)) => {
                        let s = arg.as_str().unwrap_or("");
                        if s.chars().count() != 1 {
                            return Err(it.type_error("%c requires an int or a unicode character, not a string of length 0 or more than 1"));
                        }
                        s.to_string()
                    }
                    _ => {
                        let n = match it.index_of(&arg) {
                            Ok(n) => n,
                            Err(_) => {
                                let t = it.type_name_of(&arg);
                                return Err(it.type_error(&format!("%c requires an int or a unicode character, not {}", t)));
                            }
                        };
                        match u32::try_from(n).ok().and_then(char::from_u32) {
                            Some(c) => c.to_string(),
                            None => return Err(it.overflow_err("%c arg not in range(0x110000)")),
                        }
                    }
                };
            }
            'd' | 'i' | 'u' | 'o' | 'x' | 'X' => {
                numeric = true;
                let big = match to_num(&arg) {
                    Some(Num::I(i)) => BigInt::from_i64(i),
                    Some(Num::B(b)) => b,
                    Some(Num::F(_)) if !matches!(ty, 'd' | 'i' | 'u') => {
                        return Err(it.type_error(&format!("%{} format: an integer is required, not float", ty)));
                    }
                    Some(Num::F(f)) => {
                        if f.is_nan() || f.is_infinite() {
                            return Err(it.value_error("cannot convert float NaN or infinity to integer"));
                        }
                        BigInt::from_f64_trunc(f.trunc())
                    }
                    None => {
                        if it.has_index(&arg) {
                            BigInt::from_i64(it.index_of(&arg)?)
                        } else {
                            let t = it.type_name_of(&arg);
                            let w = if matches!(ty, 'd' | 'i' | 'u') { "a real number" } else { "an integer" };
                            return Err(it.type_error(&format!("%{} format: {} is required, not {}", ty, w, t)));
                        }
                    }
                };
                let radix = match ty {
                    'o' => 8,
                    'x' | 'X' => 16,
                    _ => 10,
                };
                let mut d = if radix == 10 { it.int_to_decimal(&big.abs())? } else { big.abs().to_string_radix(radix) };
                if ty == 'X' {
                    d = d.to_uppercase();
                }
                if let Some(p) = prec {
                    while d.len() < p {
                        d.insert(0, '0');
                    }
                }
                let prefix = if alt {
                    match ty {
                        'o' => "0o",
                        'x' => "0x",
                        'X' => "0X",
                        _ => "",
                    }
                } else {
                    ""
                };
                sign = if big.is_negative() {
                    "-".into()
                } else if plus {
                    "+".into()
                } else if space {
                    " ".into()
                } else {
                    String::new()
                };
                sign.push_str(prefix);
                body = d;
            }
            'e' | 'E' | 'f' | 'F' | 'g' | 'G' => {
                numeric = true;
                let f = match it.float_arg(&arg) {
                    Ok(f) => f,
                    Err(e) => {
                        if it.exc_is(&e, "TypeError") {
                            let t = it.type_name_of(&arg);
                            return Err(it.type_error(&format!("must be real number, not {}", t)));
                        }
                        return Err(e);
                    }
                };
                let spec = format!("{}{}{}", if alt { "#" } else { "" }, prec.map(|p| format!(".{}", p)).unwrap_or_default(), ty);
                let s = it.format_float(f.abs(), &spec)?;
                sign = if f.is_sign_negative() && !f.is_nan() {
                    "-".into()
                } else if plus {
                    "+".into()
                } else if space {
                    " ".into()
                } else {
                    String::new()
                };
                body = s;
            }
            c => {
                return Err(it.value_error(&format!("unsupported format character '{}' (0x{:x}) at index {}", c, c as u32, i - 1)));
            }
        }
        let total = sign.chars().count() + body.chars().count();
        let w = width.unwrap_or(0);
        if total >= w {
            out.push_str(&sign);
            out.push_str(&body);
        } else if minus {
            out.push_str(&sign);
            out.push_str(&body);
            out.push_str(&" ".repeat(w - total));
        } else if zero && numeric {
            out.push_str(&sign);
            out.push_str(&"0".repeat(w - total));
            out.push_str(&body);
        } else {
            out.push_str(&" ".repeat(w - total));
            out.push_str(&sign);
            out.push_str(&body);
        }
    }
    if next < items.len() && single_dict.is_none() {
        return Err(it.type_error("not all arguments converted during string formatting"));
    }
    Ok(out)
}
