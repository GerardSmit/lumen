//! `repr()` and `str()` for every builtin kind, plus string/bytes literal escaping.

use crate::containers::pydict_of;
use crate::num::float_repr;
use crate::object::*;
use crate::vm::*;
use std::rc::Rc;

pub fn is_printable(c: char) -> bool {
    if c == ' ' {
        return true;
    }
    if (c as u32) < 0x7f {
        return c as u32 >= 0x20;
    }
    use std::sync::OnceLock;
    static NON_PRINT: OnceLock<Vec<(u32, u32)>> = OnceLock::new();
    let r = NON_PRINT.get_or_init(|| {
        let mut v: Vec<(u32, u32)> = Vec::new();
        for k in ["gc=c", "gc=z"] {
            if let Some(rs) = lumen_common::unicode_props::lookup(k, None) {
                v.extend_from_slice(rs);
            }
        }
        v.sort();
        v
    });
    let u = c as u32;
    let i = r.partition_point(|&(lo, _)| lo <= u);
    !(i > 0 && r[i - 1].1 >= u)
}

pub fn str_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for cp in lumen_common::smuggle::code_points(s) {
        // `from_u32` is `None` exactly for the lone surrogates.
        match char::from_u32(cp) {
            Some('\\') => out.push_str("\\\\"),
            Some('\n') => out.push_str("\\n"),
            Some('\r') => out.push_str("\\r"),
            Some('\t') => out.push_str("\\t"),
            Some(c) if c == quote => {
                out.push('\\');
                out.push(c);
            }
            Some(c) if is_printable(c) => out.push(c),
            _ => push_escape(&mut out, cp),
        }
    }
    out.push(quote);
    out
}

fn push_escape(out: &mut String, u: u32) {
    if u < 0x100 {
        out.push_str(&format!("\\x{:02x}", u));
    } else if u < 0x10000 {
        out.push_str(&format!("\\u{:04x}", u));
    } else {
        out.push_str(&format!("\\U{:08x}", u));
    }
}

pub fn ascii_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for cp in lumen_common::smuggle::code_points(s) {
        if cp < 0x80 {
            out.push(cp as u8 as char);
        } else {
            push_escape(&mut out, cp);
        }
    }
    out
}

pub fn bytes_repr(b: &[u8]) -> String {
    let quote = if b.contains(&b'\'') && !b.contains(&b'"') { '"' } else { '\'' };
    let mut out = String::with_capacity(b.len() + 3);
    out.push('b');
    out.push(quote);
    for &c in b {
        match c {
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            c if c as char == quote => {
                out.push('\\');
                out.push(c as char);
            }
            0x20..=0x7e => out.push(c as char),
            c => out.push_str(&format!("\\x{:02x}", c)),
        }
    }
    out.push(quote);
    out
}

pub fn complex_repr(re: f64, im: f64) -> String {
    let fmt = |f: f64| {
        let s = float_repr(f);
        s.strip_suffix(".0").map(|x| x.to_string()).unwrap_or(s)
    };
    if re == 0.0 && re.is_sign_positive() {
        return format!("{}j", fmt(im));
    }
    let sign = if im.is_sign_negative() && !im.is_nan() { "" } else { "+" };
    format!("({}{}{}j)", fmt(re), sign, fmt(im))
}

impl Interp {
    pub fn id_of(&self, v: &Value) -> usize {
        match v {
            Value::Obj(o) => 0x7f3a_1c00_0000usize + o.identity() as usize * 48,
            Value::None => 0x1000,
            Value::NotImplemented => 0x1010,
            Value::Ellipsis => 0x1020,
            Value::Bool(b) => 0x1030 + *b as usize * 16,
            Value::Int(i) => 0x2000_0000usize.wrapping_add((*i as usize).wrapping_mul(32)),
            Value::Float(f) => 0x4000_0000usize.wrapping_add(f.to_bits() as usize),
        }
    }

    pub fn type_module(&self, t: &Obj) -> Option<String> {
        let d = t.dict.borrow();
        let m = dict_get_str(d.as_ref()?, "__module__")?;
        m.as_str().map(|s| s.to_string())
    }

    pub fn type_qualname(&self, t: &Obj) -> String {
        if let Kind::Type(td) = &t.kind {
            if let Some(q) = &*td.qualname.borrow() {
                return q.to_string();
            }
        }
        if let Some(d) = t.dict.borrow().as_ref() {
            if let Some(q) = dict_get_str(d, "__qualname__") {
                if let Some(s) = q.as_str() {
                    return s.to_string();
                }
            }
        }
        self.type_name(t)
    }

    pub fn type_display(&self, t: &Obj) -> String {
        let q = self.type_qualname(t);
        match self.type_module(t) {
            Some(m) if m != "builtins" => format!("{}.{}", m, q),
            _ => q,
        }
    }

    pub fn repr_of(&mut self, v: &Value) -> R<String> {
        match v {
            Value::None => return Ok("None".into()),
            Value::Bool(b) => return Ok(if *b { "True".into() } else { "False".into() }),
            Value::Int(i) => return Ok(i.to_string()),
            Value::Float(f) => return Ok(float_repr(*f)),
            Value::NotImplemented => return Ok("NotImplemented".into()),
            Value::Ellipsis => return Ok("Ellipsis".into()),
            Value::Obj(o) => {
                if o.cls.is_some() {
                    if let Some(m) = self.user_special(v, "__repr__") {
                        let r = self.call_user_special(v, &m, Vec::new())?;
                        return match r.as_str() {
                            Some(s) => Ok(s.to_string()),
                            None => {
                                let t = self.type_name_of(&r);
                                Err(self.type_error(&format!("__repr__ returned non-string (type {})", t)))
                            }
                        };
                    }
                }
            }
        }
        self.native_repr(v)
    }

    pub fn native_repr(&mut self, v: &Value) -> R<String> {
        let o = match v {
            Value::Obj(o) => o,
            _ => return self.repr_of(v),
        };
        match &o.kind {
            Kind::Str(s) => Ok(str_repr(&s.s)),
            Kind::Int(b) => self.int_to_decimal(b),
            Kind::Float(f) => Ok(float_repr(*f)),
            Kind::Complex(r, i) => Ok(complex_repr(*r, *i)),
            Kind::Bytes(b) => Ok(bytes_repr(b)),
            Kind::ByteArray(b) => {
                let r = bytes_repr(&b.bytes());
                Ok(self.wrap_cls_repr(o, "bytearray", &r))
            }
            Kind::Tuple(items) => {
                let items = items.clone();
                if self.repr_enter(o) {
                    return Ok("(...)".into());
                }
                let r = self.repr_seq(&items);
                self.repr_leave();
                let parts = r?;
                let body = if parts.len() == 1 { format!("({},)", parts[0]) } else { format!("({})", parts.join(", ")) };
                Ok(body)
            }
            Kind::List(l) => {
                if self.repr_enter(o) {
                    return Ok("[...]".into());
                }
                let items = l.borrow().clone();
                let r = self.repr_seq(&items);
                self.repr_leave();
                Ok(format!("[{}]", r?.join(", ")))
            }
            Kind::Dict(d) => {
                if self.repr_enter(o) {
                    return Ok("{...}".into());
                }
                let entries: Vec<(Value, Value)> = d.borrow().iter().map(|e| (e.key.clone(), e.val.clone())).collect();
                let r = self.repr_pairs(&entries);
                self.repr_leave();
                Ok(format!("{{{}}}", r?))
            }
            Kind::Set(d) | Kind::FrozenSet(d) => {
                let frozen = matches!(o.kind, Kind::FrozenSet(_));
                let tname = if o.cls.is_some() { self.type_name_of(v) } else if frozen { "frozenset".into() } else { "set".into() };
                if self.repr_enter(o) {
                    return Ok(format!("{}(...)", tname));
                }
                let items = d.borrow().keys();
                let r = self.repr_seq(&items);
                self.repr_leave();
                let parts = r?;
                if parts.is_empty() {
                    return Ok(format!("{}()", tname));
                }
                if frozen || o.cls.is_some() {
                    Ok(format!("{}({{{}}})", tname, parts.join(", ")))
                } else {
                    Ok(format!("{{{}}}", parts.join(", ")))
                }
            }
            Kind::DictView(d, vk) => {
                let items: Vec<Value> = match pydict_of(d) {
                    Some(p) => match vk {
                        ViewKind::Keys => p.borrow().keys(),
                        ViewKind::Values => p.borrow().values(),
                        ViewKind::Items => p.borrow().iter().map(|e| Value::tuple(vec![e.key.clone(), e.val.clone()])).collect(),
                    },
                    None => Vec::new(),
                };
                let parts = self.repr_seq(&items)?;
                let name = match vk {
                    ViewKind::Keys => "dict_keys",
                    ViewKind::Values => "dict_values",
                    ViewKind::Items => "dict_items",
                };
                Ok(format!("{}([{}])", name, parts.join(", ")))
            }
            Kind::Type(_) => Ok(format!("<class '{}'>", self.type_display(o))),
            Kind::Function(f) => Ok(format!("<function {} at {:#x}>", f.qualname.borrow(), self.id_of(v))),
            Kind::Method(f, this) => {
                if let Value::Obj(fo) = f {
                    if let Kind::Native(n) = &fo.kind {
                        let t = self.type_of(this);
                        return Ok(format!("<built-in method {} of {} object at {:#x}>", n.name, self.type_display(&t), self.id_of(this)));
                    }
                }
                let name = match self.get_attr_str(f, "__qualname__") {
                    Ok(q) => q.as_str().unwrap_or("?").to_string(),
                    Err(_) => "?".into(),
                };
                let r = self.repr_of(this)?;
                Ok(format!("<bound method {} of {}>", name, r))
            }
            Kind::Native(n) => {
                if let Some(d) = n.desc.filter(|d| n.method && d.class().is_some()) {
                    Ok(format!("<method '{}' of '{}' objects>", n.name, crate::bind::owner_of(d)))
                } else if n.method {
                    Ok(format!("<method '{}' of object>", n.name))
                } else {
                    Ok(format!("<built-in function {}>", n.name))
                }
            }
            Kind::Module => {
                let d = o.dict.borrow().clone();
                let name = d.as_ref().and_then(|d| dict_get_str(d, "__name__")).and_then(|v| v.as_str().map(|s| s.to_string())).unwrap_or_else(|| "?".into());
                let file = d.as_ref().and_then(|d| dict_get_str(d, "__file__")).and_then(|v| v.as_str().map(|s| s.to_string()));
                Ok(match file {
                    Some(f) => format!("<module '{}' from '{}'>", name, f),
                    None => format!("<module '{}'>", name),
                })
            }
            Kind::Generator(g) => {
                let kind = match g.kind {
                    GenKind::Generator => "generator",
                    GenKind::Coroutine => "coroutine",
                    GenKind::AsyncGen => "async_generator",
                };
                Ok(format!("<{} object {} at {:#x}>", kind, g.qualname.borrow(), self.id_of(v)))
            }
            Kind::Exception(d) => {
                let args = d.borrow().args.clone();
                let items = args.tuple_items().map(|t| t.to_vec()).unwrap_or_default();
                let parts = self.repr_seq(&items)?;
                let name = self.type_name_of(v);
                Ok(format!("{}({})", name, parts.join(", ")))
            }
            Kind::Slice(a, b, c) => {
                let (a, b, c) = (self.repr_of(a)?, self.repr_of(b)?, self.repr_of(c)?);
                Ok(format!("slice({}, {}, {})", a, b, c))
            }
            Kind::BigRange(r) => {
                let (a, b, c) = (self.int_to_decimal(&r[0])?, self.int_to_decimal(&r[1])?, self.int_to_decimal(&r[2])?);
                if r[2].cmp(&crate::pyint::BigInt::from_i64(1)) == std::cmp::Ordering::Equal {
                    Ok(format!("range({}, {})", a, b))
                } else {
                    Ok(format!("range({}, {}, {})", a, b, c))
                }
            }
            Kind::Range(r) => {
                if r.step == 1 {
                    Ok(format!("range({}, {})", r.start, r.stop))
                } else {
                    Ok(format!("range({}, {}, {})", r.start, r.stop, r.step))
                }
            }
            Kind::Code(c) => Ok(format!("<code object {} at {:#x}, file \"{}\", line {}>", c.name, self.id_of(v), c.filename, c.first_line)),
            Kind::Super(t, _, ot) => {
                let t = self.repr_of(t)?;
                let ot = self.repr_of(ot)?;
                Ok(format!("<super: {}, <{} object>>", t, ot))
            }
            Kind::Cell(c) => {
                let inner = c.borrow().clone();
                match inner {
                    Some(x) => {
                        let t = self.type_name_of(&x);
                        Ok(format!("<cell at {:#x}: {} object at {:#x}>", self.id_of(v), t, self.id_of(&x)))
                    }
                    None => Ok(format!("<cell at {:#x}: empty>", self.id_of(v))),
                }
            }
            _ => {
                let cls = self.type_of_obj(o);
                Ok(format!("<{} object at {:#x}>", self.type_display(&cls), self.id_of(v)))
            }
        }
    }

    fn wrap_cls_repr(&self, o: &Obj, base: &str, inner: &str) -> String {
        if o.cls.is_some() {
            let cls = self.type_of_obj(o);
            format!("{}({})", self.type_name(&cls), inner)
        } else {
            format!("{}({})", base, inner)
        }
    }

    pub fn repr_enter(&mut self, o: &Obj) -> bool {
        let id = Rc::as_ptr(o) as *const u8 as usize;
        if self.repr_stack.contains(&id) {
            return true;
        }
        self.repr_stack.push(id);
        false
    }

    pub fn repr_leave(&mut self) {
        self.repr_stack.pop();
    }

    fn repr_seq(&mut self, items: &[Value]) -> R<Vec<String>> {
        let mut out = Vec::with_capacity(items.len());
        for i in items {
            out.push(self.repr_of(i)?);
        }
        Ok(out)
    }

    fn repr_pairs(&mut self, items: &[(Value, Value)]) -> R<String> {
        let mut out = String::new();
        for (n, (k, v)) in items.iter().enumerate() {
            if n > 0 {
                out.push_str(", ");
            }
            out.push_str(&self.repr_of(k)?);
            out.push_str(": ");
            out.push_str(&self.repr_of(v)?);
        }
        Ok(out)
    }

    pub fn str_of(&mut self, v: &Value) -> R<String> {
        if let Value::Obj(o) = v {
            if o.cls.is_some() {
                if let Some(m) = self.user_special(v, "__str__") {
                    let r = self.call_user_special(v, &m, Vec::new())?;
                    return match r.as_str() {
                        Some(s) => Ok(s.to_string()),
                        None => {
                            let t = self.type_name_of(&r);
                            Err(self.type_error(&format!("__str__ returned non-string (type {})", t)))
                        }
                    };
                }
            }
        }
        self.native_str(v)
    }

    pub fn native_str(&mut self, v: &Value) -> R<String> {
        match v {
            Value::Obj(o) => match &o.kind {
                Kind::Str(s) => Ok(s.s.to_string()),
                Kind::Exception(d) => {
                    if self.exc_is(o, "BaseExceptionGroup") {
                        return Ok(crate::builtins::excgroup::group_text(o));
                    }
                    let args = d.borrow().args.clone();
                    if self.exc_is(o, "OSError") {
                        if let Some(s) = self.oserror_str(o)? {
                            return Ok(s);
                        }
                    }
                    if self.exc_is(o, "UnicodeError") {
                        if let Some(s) = self.unicode_exc_str(o)? {
                            return Ok(s);
                        }
                    }
                    let items = args.tuple_items().map(|t| t.to_vec()).unwrap_or_default();
                    match items.len() {
                        0 => Ok(String::new()),
                        1 => {
                            if self.is_exc_named(o, "KeyError") {
                                self.repr_of(&items[0])
                            } else {
                                self.str_of(&items[0])
                            }
                        }
                        _ => self.repr_of(&args),
                    }
                }
                _ => self.repr_of(v),
            },
            Value::Int(i) => Ok(i.to_string()),
            _ => self.repr_of(v),
        }
    }

    fn is_exc_named(&self, o: &Obj, name: &str) -> bool {
        let t = self.exc_type(name);
        let c = self.type_of_obj(o);
        self.is_subtype(&c, &t)
    }

    pub fn str_value(&mut self, v: &Value) -> R<Value> {
        if let Value::Obj(o) = v {
            if matches!(o.kind, Kind::Str(_)) && o.cls.is_none() {
                return Ok(v.clone());
            }
        }
        Ok(Value::string(self.str_of(v)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printable_follows_unicode_categories() {
        assert!(is_printable('a'));
        assert!(is_printable('\u{20ac}'));
        assert!(is_printable(' '));
        assert!(!is_printable('\u{200b}'));
        assert!(!is_printable('\u{a0}'));
        assert!(!is_printable('\u{d7ff}'));
        assert!(!is_printable('\u{7f}'));
    }

    #[test]
    fn str_repr_quotes_and_escapes() {
        assert_eq!(str_repr("a'b"), "\"a'b\"");
        assert_eq!(str_repr("a'b\"c"), "'a\\'b\"c'");
        assert_eq!(str_repr("tab\t\u{0}"), "'tab\\t\\x00'");
        assert_eq!(str_repr("\u{200b}"), "'\\u200b'");
    }

    #[test]
    fn bytes_repr_escapes_non_ascii() {
        assert_eq!(bytes_repr(b"a\xffb'"), "b\"a\\xffb'\"");
    }
}
