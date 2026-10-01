//! `_string`: the format-string tokenizers behind `string.Formatter`.

use super::native::*;
use crate::object::*;
use crate::vm::*;

fn opt(s: Option<String>) -> Value {
    s.map(Value::string).unwrap_or(Value::None)
}

fn parse_error(it: &mut Interp, msg: &str) -> Obj {
    it.value_error(msg)
}

fn formatter_parser(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("formatter_parser", a, 1, 1)?;
    let Some(src) = a[0].as_str() else {
        let t = it.type_name_of(&a[0]);
        return Err(it.type_error(&format!("expected str, got {}", t)));
    };
    let chars: Vec<char> = src.chars().collect();
    let mut out: Vec<Value> = Vec::new();
    let mut lit = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '{' || c == '}' {
            if i + 1 < chars.len() && chars[i + 1] == c {
                lit.push(c);
                out.push(Value::tuple(vec![Value::string(std::mem::take(&mut lit)), Value::None, Value::None, Value::None]));
                i += 2;
                continue;
            }
            if c == '}' {
                return Err(parse_error(it, "Single '}' encountered in format string"));
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
                let msg = if i + 1 == chars.len() { "Single '{' encountered in format string" } else { "expected '}' before end of string" };
                return Err(parse_error(it, msg));
            }
            let field = &chars[i + 1..j];
            let mut k = 0;
            let mut bracket = 0;
            while k < field.len() {
                match field[k] {
                    '[' => bracket += 1,
                    ']' => bracket -= 1,
                    '!' | ':' if bracket <= 0 => break,
                    _ => {}
                }
                k += 1;
            }
            let name: String = field[..k].iter().collect();
            let mut conv = None;
            let mut spec = String::new();
            if k < field.len() && field[k] == '!' {
                if k + 1 >= field.len() {
                    return Err(parse_error(it, "end of string while looking for conversion specifier"));
                }
                conv = Some(field[k + 1].to_string());
                k += 2;
                if k < field.len() && field[k] != ':' {
                    return Err(parse_error(it, "expected ':' after conversion specifier"));
                }
            }
            if k < field.len() && field[k] == ':' {
                spec = field[k + 1..].iter().collect();
            }
            out.push(Value::tuple(vec![Value::string(std::mem::take(&mut lit)), Value::string(name), Value::string(spec), opt(conv)]));
            i = j + 1;
        } else {
            lit.push(c);
            i += 1;
        }
    }
    if !lit.is_empty() {
        out.push(Value::tuple(vec![Value::string(lit), Value::None, Value::None, Value::None]));
    }
    it.native_get_iter(&Value::list(out))
}

fn field_name_split(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("formatter_field_name_split", a, 1, 1)?;
    let Some(name) = a[0].as_str() else {
        let t = it.type_name_of(&a[0]);
        return Err(it.type_error(&format!("expected str, got {}", t)));
    };
    let chars: Vec<char> = name.chars().collect();
    let first_end = chars.iter().position(|c| *c == '.' || *c == '[').unwrap_or(chars.len());
    let first: String = chars[..first_end].iter().collect();
    let first_val = if !first.is_empty() && first.chars().all(|c| c.is_ascii_digit()) { first.parse::<i64>().map(Value::Int).unwrap_or_else(|_| Value::string(first.clone())) } else { Value::string(first) };
    let mut rest: Vec<Value> = Vec::new();
    let mut k = first_end;
    while k < chars.len() {
        if chars[k] == '.' {
            let mut e = k + 1;
            while e < chars.len() && chars[e] != '.' && chars[e] != '[' {
                e += 1;
            }
            if e == k + 1 {
                return Err(parse_error(it, "Empty attribute in format string"));
            }
            let attr: String = chars[k + 1..e].iter().collect();
            rest.push(Value::tuple(vec![Value::Bool(true), Value::string(attr)]));
            k = e;
        } else if chars[k] == '[' {
            let mut e = k + 1;
            while e < chars.len() && chars[e] != ']' {
                e += 1;
            }
            if e >= chars.len() {
                return Err(parse_error(it, "Missing ']' in format string"));
            }
            let key: String = chars[k + 1..e].iter().collect();
            if key.is_empty() {
                return Err(parse_error(it, "Empty attribute in format string"));
            }
            let kv = if key.chars().all(|c| c.is_ascii_digit()) { key.parse::<i64>().map(Value::Int).unwrap_or_else(|_| Value::string(key.clone())) } else { Value::string(key) };
            rest.push(Value::tuple(vec![Value::Bool(false), kv]));
            k = e + 1;
        } else {
            return Err(parse_error(it, "Only '.' or '[' may follow ']' in format field specifier"));
        }
    }
    let iter = it.native_get_iter(&Value::list(rest))?;
    Ok(Value::tuple(vec![first_val, iter]))
}

pub fn make(it: &mut Interp) -> Obj {
    let m = it.new_module("_string");
    let d = it.module_dict(&m);
    it.register_module("_string", &m);
    set_fn(it, &d, "formatter_parser", formatter_parser);
    set_fn(it, &d, "formatter_field_name_split", field_name_split);
    m
}
