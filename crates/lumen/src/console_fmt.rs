//! Native `util.format` for the `console` log family: the output Node's `util.inspect`
//! produces (default options, no colors) for primitives, plain objects, arrays and plain class
//! instances, so printing them never loads the JS `util` module.
//!
//! The formatter is deliberately partial. It reads own data properties only and runs no user
//! code, so on anything whose Node rendering needs more (functions, Maps/Sets/Dates/Errors,
//! proxies, accessors' values, symbol keys, `Symbol.toStringTag` / `util.inspect.custom`
//! anywhere on the prototype chain, circular references, sparse arrays, ...) it gives up with
//! [`Bail`] before anything observable has happened and the caller falls back to `util`.

use crate::interpreter::Interp;
use crate::value::{canonical_index, Callable, Exotic, Gc, Value};

struct Bail;
type R<T> = Result<T, Bail>;

const BREAK_LENGTH: usize = 80;
const COMPACT: usize = 3;
const MAX_ARRAY_LENGTH: usize = 100;
const MAX_STRING_LENGTH: usize = 10_000;
const DEFAULT_DEPTH: usize = 2;

impl Interp {
    /// `util.format(...args)` as `console.log` renders it, or `None` when the arguments need
    /// the full `util` implementation.
    pub fn console_format(&mut self, args: &[Value]) -> Option<String> {
        format_args(self, args).ok()
    }

    /// Whether `obj` still has `key` as an own accessor whose getter is `getter`.
    pub fn own_getter_is(&self, obj: &Value, key: &str, getter: &Value) -> bool {
        let Value::Obj(o) = obj else { return false };
        let Ok(b) = o.try_borrow() else { return false };
        b.props
            .get(key)
            .is_some_and(|p| p.accessor() && p.getter().is_some_and(|g| self.values_strict_equal(g, getter)))
    }
}

fn len16(s: &str) -> usize {
    if s.is_ascii() {
        s.len()
    } else {
        s.chars().map(char::len_utf16).sum()
    }
}

fn has_lone_surrogate(s: &str) -> bool {
    s.as_bytes().contains(&0xF4)
}

fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}'
            | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

fn format_number(i: &Interp, n: f64) -> String {
    if n == 0.0 && n.is_sign_negative() {
        "-0".to_string()
    } else {
        i.num_to_str(n)
    }
}

fn symbol_text(s: &crate::value::SymbolData) -> R<String> {
    match &s.description {
        Some(d) if has_lone_surrogate(d) => Err(Bail),
        Some(d) => Ok(format!("Symbol({d})")),
        None => Ok("Symbol()".to_string()),
    }
}

fn str_escape(s: &str) -> R<String> {
    if has_lone_surrogate(s) {
        return Err(Bail);
    }
    let mut quote = '\'';
    if s.contains('\'') {
        if !s.contains('"') {
            quote = '"';
        } else if !s.contains('`') && !s.contains("${") {
            quote = '`';
        }
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\'' if quote == '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || (0x7f..=0x9f).contains(&(c as u32)) => {
                out.push_str(&format!("\\x{:02X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    Ok(out)
}

fn json_quote(s: &str) -> R<String> {
    if has_lone_surrogate(s) {
        return Err(Bail);
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    Ok(out)
}

fn is_identifier_key(k: &str) -> bool {
    let mut bytes = k.bytes();
    matches!(bytes.next(), Some(b) if b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// `Number.parseInt(s)` for the digit runs where doing it in `f64` is exact.
fn parse_int(s: &str) -> R<f64> {
    let t = s.trim_start_matches(is_js_whitespace);
    let (negative, t) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let (radix, t) = match t.get(..2) {
        Some("0x" | "0X") => (16, &t[2..]),
        _ => (10, t),
    };
    let end = t.bytes().take_while(|b| (*b as char).is_digit(radix)).count();
    let digits = &t[..end];
    let magnitude = if digits.is_empty() {
        return Ok(f64::NAN);
    } else if radix == 10 {
        if digits.len() > 20 {
            return Err(Bail);
        }
        digits.parse::<f64>().map_err(|_| Bail)?
    } else {
        if digits.len() > 13 {
            return Err(Bail);
        }
        u64::from_str_radix(digits, 16).map_err(|_| Bail)? as f64
    };
    Ok(if negative { -magnitude } else { magnitude })
}

/// `Number.parseFloat(s)`.
fn parse_float(s: &str) -> R<f64> {
    let t = s.trim_start_matches(is_js_whitespace);
    let b = t.as_bytes();
    let mut p = 0;
    if matches!(b.first(), Some(b'+' | b'-')) {
        p = 1;
    }
    if t[p..].starts_with("Infinity") {
        return Ok(if b.first() == Some(&b'-') { f64::NEG_INFINITY } else { f64::INFINITY });
    }
    let int_start = p;
    while p < b.len() && b[p].is_ascii_digit() {
        p += 1;
    }
    let mut digits = p - int_start;
    if p < b.len() && b[p] == b'.' {
        let frac_start = p + 1;
        let mut q = frac_start;
        while q < b.len() && b[q].is_ascii_digit() {
            q += 1;
        }
        if digits > 0 || q > frac_start {
            digits += q - frac_start;
            p = q;
        }
    }
    if digits == 0 {
        return Ok(f64::NAN);
    }
    if p < b.len() && (b[p] == b'e' || b[p] == b'E') {
        let mut q = p + 1;
        if q < b.len() && (b[q] == b'+' || b[q] == b'-') {
            q += 1;
        }
        let exp_start = q;
        while q < b.len() && b[q].is_ascii_digit() {
            q += 1;
        }
        if q > exp_start {
            p = q;
        }
    }
    t[..p].parse::<f64>().map_err(|_| Bail)
}

fn format_args(i: &mut Interp, args: &[Value]) -> R<String> {
    let mut out = String::new();
    let mut a = 0usize;
    let mut join = "";
    if let Some(Value::Str(first)) = args.first() {
        let first = first.as_str();
        if args.len() == 1 {
            return Ok(first.to_string());
        }
        let bytes = first.as_bytes();
        let mut last = 0usize;
        let mut p = 0usize;
        while p + 1 < bytes.len() {
            if bytes[p] == b'%' {
                p += 1;
                let next = bytes[p];
                if a + 1 != args.len() {
                    let piece = match next {
                        b's' => {
                            a += 1;
                            Some(format_s(i, &args[a])?)
                        }
                        b'j' => {
                            a += 1;
                            Some(format_j(i, &args[a])?)
                        }
                        b'd' => {
                            a += 1;
                            Some(format_d(i, &args[a])?)
                        }
                        b'O' => {
                            a += 1;
                            Some(inspect(i, &args[a], DEFAULT_DEPTH)?)
                        }
                        b'i' => {
                            a += 1;
                            Some(format_i(i, &args[a])?)
                        }
                        b'f' => {
                            a += 1;
                            Some(format_f(i, &args[a])?)
                        }
                        b'c' => {
                            a += 1;
                            Some(String::new())
                        }
                        b'o' => return Err(Bail),
                        b'%' => {
                            out.push_str(&first[last..p]);
                            last = p + 1;
                            None
                        }
                        _ => None,
                    };
                    if let Some(piece) = piece {
                        if last != p - 1 {
                            out.push_str(&first[last..p - 1]);
                        }
                        out.push_str(&piece);
                        last = p + 1;
                    }
                } else if next == b'%' {
                    out.push_str(&first[last..p]);
                    last = p + 1;
                }
            }
            p += 1;
        }
        if last != 0 {
            a += 1;
            join = " ";
            if last < first.len() {
                out.push_str(&first[last..]);
            }
        }
    }
    while a < args.len() {
        out.push_str(join);
        match &args[a] {
            Value::Str(s) => out.push_str(s.as_str()),
            other => out.push_str(&inspect(i, other, DEFAULT_DEPTH)?),
        }
        join = " ";
        a += 1;
    }
    Ok(out)
}

fn bigint_text(v: &Value) -> String {
    match v {
        Value::BigInt(b) => format!("{}n", b.to_string_radix(10)),
        _ => String::new(),
    }
}

fn format_s(i: &mut Interp, v: &Value) -> R<String> {
    match v {
        Value::Num(n) => Ok(format_number(i, *n)),
        Value::BigInt(_) => Ok(bigint_text(v)),
        Value::Str(s) => Ok(s.as_str().to_string()),
        Value::Undefined => Ok("undefined".into()),
        Value::Null => Ok("null".into()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Sym(s) => symbol_text(s),
        Value::Obj(o) => {
            if v.is_callable() || !i.has_builtin_to_string(o) {
                return Err(Bail);
            }
            inspect(i, v, 0)
        }
        Value::Empty => Err(Bail),
    }
}

fn format_j(i: &Interp, v: &Value) -> R<String> {
    match v {
        Value::Str(s) => json_quote(s.as_str()),
        Value::Num(n) if n.is_finite() => Ok(i.num_to_str(*n)),
        Value::Num(_) | Value::Null => Ok("null".into()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Undefined | Value::Sym(_) => Ok("undefined".into()),
        _ => Err(Bail),
    }
}

fn format_d(i: &mut Interp, v: &Value) -> R<String> {
    let n = match v {
        Value::BigInt(_) => return Ok(bigint_text(v)),
        Value::Sym(_) => return Ok("NaN".into()),
        Value::Obj(_) | Value::Empty => return Err(Bail),
        other => i.coerce_number(other).map_err(|_| Bail)?,
    };
    Ok(format_number(i, n))
}

fn format_i(i: &mut Interp, v: &Value) -> R<String> {
    let n = match v {
        Value::BigInt(_) => return Ok(bigint_text(v)),
        Value::Sym(_) => return Ok("NaN".into()),
        Value::Obj(_) | Value::Empty => return Err(Bail),
        Value::Str(s) => parse_int(s.as_str())?,
        Value::Num(n) => parse_int(&i.num_to_str(*n))?,
        Value::Undefined => f64::NAN,
        Value::Null => f64::NAN,
        Value::Bool(_) => f64::NAN,
    };
    Ok(format_number(i, n))
}

fn format_f(i: &mut Interp, v: &Value) -> R<String> {
    let n = match v {
        Value::Sym(_) => return Ok("NaN".into()),
        Value::BigInt(b) => parse_float(&b.to_string_radix(10))?,
        Value::Obj(_) | Value::Empty => return Err(Bail),
        Value::Str(s) => parse_float(s.as_str())?,
        Value::Num(n) => {
            if *n == 0.0 {
                0.0
            } else {
                *n
            }
        }
        Value::Undefined | Value::Null | Value::Bool(_) => f64::NAN,
    };
    Ok(format_number(i, n))
}

fn inspect(i: &Interp, v: &Value, depth: usize) -> R<String> {
    let mut f = Inspector {
        i,
        indentation: 0,
        seen: Vec::new(),
        current_depth: 0,
        depth,
    };
    f.value(v, 0)
}

enum Slot {
    Value(Value),
    Getter,
    Setter,
    GetterSetter,
}

struct Inspector<'a> {
    i: &'a Interp,
    indentation: usize,
    seen: Vec<Gc>,
    current_depth: usize,
    depth: usize,
}

impl Interp {
    /// Node's `hasBuiltInToString` for a non-proxy object: no own or inherited user `toString`.
    fn has_builtin_to_string(&self, o: &Gc) -> bool {
        let mut cur = Some(o.clone());
        while let Some(c) = cur {
            let Ok(b) = c.try_borrow() else { return false };
            if !b.ic_plain.get() || !matches!(b.call, Callable::None) {
                return false;
            }
            if b.props.get("toString").is_some() {
                return Gc::ptr_eq(&c, &self.object_proto) || Gc::ptr_eq(&c, &self.array_proto);
            }
            cur = b.proto.clone();
        }
        true
    }
}

impl Inspector<'_> {
    fn value(&mut self, v: &Value, recurse: usize) -> R<String> {
        match v {
            Value::Obj(o) => self.object(o, recurse),
            Value::Null => Ok("null".into()),
            Value::Undefined => Ok("undefined".into()),
            Value::Bool(b) => Ok(b.to_string()),
            Value::Num(n) => Ok(format_number(self.i, *n)),
            Value::BigInt(_) => Ok(bigint_text(v)),
            Value::Sym(s) => symbol_text(s),
            Value::Str(s) => self.string(s.as_str()),
            Value::Empty => Err(Bail),
        }
    }

    fn string(&self, s: &str) -> R<String> {
        let len = len16(s);
        if len > MAX_STRING_LENGTH {
            return Err(Bail);
        }
        let room = BREAK_LENGTH as isize - self.indentation as isize - 4;
        if len > 16 && len as isize > room && s.contains('\n') {
            let mut parts = Vec::new();
            for line in s.split_inclusive('\n') {
                parts.push(str_escape(line)?);
            }
            let sep = format!(" +\n{}", " ".repeat(self.indentation + 2));
            return Ok(parts.join(&sep));
        }
        str_escape(s)
    }

    /// Fails unless every property on `o`'s prototype chain is one this formatter understands:
    /// no `Symbol.toStringTag` / `util.inspect.custom` (or an unrecognised symbol key), no
    /// exotic or callable objects, and each prototype's constructor is a user function (or the
    /// realm's `Object`).
    fn check_chain(&self, o: &Gc) -> R<()> {
        let mut cur = o.try_borrow().map_err(|_| Bail)?.proto.clone();
        while let Some(c) = cur {
            let b = c.try_borrow().map_err(|_| Bail)?;
            if !b.ic_plain.get() || !matches!(b.call, Callable::None) {
                return Err(Bail);
            }
            match b.exotic {
                Exotic::None => {}
                Exotic::Array if Gc::ptr_eq(&c, &self.i.array_proto) => {}
                _ => return Err(Bail),
            }
            self.check_symbols(b.props.iter_named().map(|(k, _)| k))?;
            if !Gc::ptr_eq(&c, &self.i.object_proto) && !Gc::ptr_eq(&c, &self.i.array_proto) {
                let ctor = b.props.get("constructor");
                let user = ctor.as_ref().is_some_and(|p| {
                    !p.accessor()
                        && matches!(p.value(), Value::Obj(f)
                            if matches!(f.try_borrow().map(|fb| matches!(fb.call, Callable::User(_))), Ok(true)))
                });
                if !user {
                    return Err(Bail);
                }
            }
            cur = b.proto.clone();
        }
        Ok(())
    }

    fn check_symbols<'k>(&self, keys: impl Iterator<Item = &'k std::rc::Rc<str>>) -> R<()> {
        for k in keys {
            if !Interp::is_sym_key(k) {
                continue;
            }
            let id: u64 = k[1..].parse().map_err(|_| Bail)?;
            let data = self.i.sym_registry.get(&id).ok_or(Bail)?;
            if matches!(data.description.as_deref(), Some("Symbol.toStringTag" | "nodejs.util.inspect.custom")) {
                return Err(Bail);
            }
        }
        Ok(())
    }

    /// Node's `getConstructorName` restricted to user-defined constructors and the realm's
    /// `Object`/`Array`: `Ok(None)` is a null-prototype object.
    fn constructor_name(&self, o: &Gc) -> R<Option<String>> {
        let first_proto = o.try_borrow().map_err(|_| Bail)?.proto.clone();
        let mut cur = Some(o.clone());
        while let Some(c) = cur {
            let b = c.try_borrow().map_err(|_| Bail)?;
            if let Some(p) = b.props.get("constructor") {
                if !p.accessor() {
                    let ctor = p.value();
                    if let (Value::Obj(f), true) = (&ctor, ctor.is_callable()) {
                        if let Some(name) = self.ctor_name_if_instance(&c, f, o)? {
                            return Ok(Some(name));
                        }
                    }
                }
            }
            cur = b.proto.clone();
        }
        if first_proto.is_none() {
            Ok(None)
        } else {
            Err(Bail)
        }
    }

    fn ctor_name_if_instance(&self, holder: &Gc, f: &Gc, instance: &Gc) -> R<Option<String>> {
        self.i.materialize(f);
        let fb = f.try_borrow().map_err(|_| Bail)?;
        let native_ok = (Gc::ptr_eq(holder, &self.i.object_proto) || Gc::ptr_eq(holder, &self.i.array_proto))
            && !matches!(fb.call, Callable::User(_));
        if !matches!(fb.call, Callable::User(_)) && !native_ok {
            return Err(Bail);
        }
        let name = match fb.props.get("name") {
            None => return Ok(None),
            Some(p) if p.accessor() => return Err(Bail),
            Some(p) => match p.value() {
                Value::Str(s) if has_lone_surrogate(s.as_str()) => return Err(Bail),
                Value::Str(s) => s.as_str().to_string(),
                _ => return Err(Bail),
            },
        };
        if name.is_empty() {
            return Ok(None);
        }
        let proto = match fb.props.get("prototype") {
            None => return Ok(None),
            Some(p) if p.accessor() => return Err(Bail),
            Some(p) => match p.value() {
                Value::Obj(g) => g,
                _ => return Ok(None),
            },
        };
        let mut walk = instance.try_borrow().map_err(|_| Bail)?.proto.clone();
        while let Some(w) = walk {
            if Gc::ptr_eq(&w, &proto) {
                return Ok(Some(name));
            }
            walk = w.try_borrow().map_err(|_| Bail)?.proto.clone();
        }
        Ok(None)
    }

    fn slot_text(&mut self, slot: &Slot, recurse: usize) -> R<String> {
        match slot {
            Slot::Value(v) => {
                self.indentation += 2;
                let s = self.value(v, recurse);
                self.indentation -= 2;
                s
            }
            Slot::Getter => Ok("[Getter]".into()),
            Slot::Setter => Ok("[Setter]".into()),
            Slot::GetterSetter => Ok("[Getter/Setter]".into()),
        }
    }

    fn slot_of(p: &crate::value::Property) -> Slot {
        if p.accessor() {
            match (p.getter().is_some(), p.setter().is_some()) {
                (true, true) => Slot::GetterSetter,
                (true, false) => Slot::Getter,
                (false, true) => Slot::Setter,
                (false, false) => Slot::Value(Value::Undefined),
            }
        } else {
            Slot::Value(p.value())
        }
    }

    fn key_text(key: &str) -> R<String> {
        if key == "__proto__" {
            Ok("['__proto__']".into())
        } else if is_identifier_key(key) {
            Ok(key.to_string())
        } else {
            str_escape(key)
        }
    }

    fn object(&mut self, o: &Gc, recurse: usize) -> R<String> {
        self.i.materialize(o);
        let (exotic, ic_plain, callable) = {
            let b = o.try_borrow().map_err(|_| Bail)?;
            (b.exotic, b.ic_plain.get(), !matches!(b.call, Callable::None))
        };
        if !ic_plain || callable || self.i.proxies.contains_key(&(Gc::as_ptr(o) as usize)) {
            return Err(Bail);
        }
        if self.seen.iter().any(|s| Gc::ptr_eq(s, o)) {
            return Err(Bail);
        }
        self.check_chain(o)?;
        let constructor = self.constructor_name(o)?;
        match exotic {
            Exotic::None => self.plain(o, constructor, recurse),
            Exotic::Array if constructor.as_deref() == Some("Array") => self.array(o, recurse),
            _ => Err(Bail),
        }
    }

    fn prefix(constructor: &Option<String>) -> String {
        match constructor {
            None => "[Object: null prototype] ".to_string(),
            Some(c) => format!("{c} "),
        }
    }

    fn too_deep(constructor: &Option<String>) -> String {
        match constructor {
            None => "[Object: null prototype]".to_string(),
            Some(c) => format!("[{c}]"),
        }
    }

    fn plain(&mut self, o: &Gc, constructor: Option<String>, recurse: usize) -> R<String> {
        let mut entries: Vec<(String, Slot)> = Vec::new();
        {
            let b = o.try_borrow().map_err(|_| Bail)?;
            for k in b.props.ordered_keys() {
                if Interp::is_sym_key(&k) {
                    return Err(Bail);
                }
                let Some(p) = b.props.get(&k) else { continue };
                if !p.enumerable() {
                    continue;
                }
                if has_lone_surrogate(&k) {
                    return Err(Bail);
                }
                entries.push((k.to_string(), Self::slot_of(&p)));
            }
        }
        let open = if constructor.as_deref() == Some("Object") {
            "{".to_string()
        } else {
            format!("{}{{", Self::prefix(&constructor))
        };
        if entries.is_empty() {
            return Ok(format!("{open}}}"));
        }
        if recurse > self.depth {
            return Ok(Self::too_deep(&constructor));
        }
        let recurse = recurse + 1;
        self.seen.push(o.clone());
        self.current_depth = recurse;
        let mut output = Vec::with_capacity(entries.len());
        for (key, slot) in &entries {
            let text = self.slot_text(slot, recurse)?;
            output.push(format!("{}: {text}", Self::key_text(key)?));
        }
        self.seen.pop();
        self.reduce(output, &open, "}", false, recurse, None)
    }

    fn array(&mut self, o: &Gc, recurse: usize) -> R<String> {
        let len = self.i.array_length(o);
        let shown = len.min(MAX_ARRAY_LENGTH);
        let mut items: Vec<Slot> = Vec::with_capacity(shown);
        let mut extras: Vec<(String, Slot)> = Vec::new();
        {
            let b = o.try_borrow().map_err(|_| Bail)?;
            for idx in 0..shown {
                let slot = match b.props.get_index(idx as u32) {
                    Some(p) => Self::slot_of(&p),
                    None => match b.props.get(&idx.to_string()) {
                        Some(p) => Self::slot_of(&p),
                        None => return Err(Bail),
                    },
                };
                if !matches!(slot, Slot::Value(_)) {
                    return Err(Bail);
                }
                items.push(slot);
            }
            for (k, p) in b.props.iter_named() {
                if Interp::is_private_key(k) || canonical_index(k).is_some() {
                    continue;
                }
                if Interp::is_sym_key(k) || has_lone_surrogate(k) {
                    return Err(Bail);
                }
                if p.enumerable() {
                    extras.push((k.to_string(), Self::slot_of(p)));
                }
            }
        }
        if len == 0 && extras.is_empty() {
            return Ok("[]".into());
        }
        if recurse > self.depth {
            return Ok("[Array]".into());
        }
        let recurse = recurse + 1;
        self.seen.push(o.clone());
        self.current_depth = recurse;
        let mut output = Vec::with_capacity(shown + 1 + extras.len());
        for slot in &items {
            output.push(self.slot_text(slot, recurse)?);
        }
        if len > shown {
            let remaining = len - shown;
            output.push(format!("... {remaining} more item{}", if remaining > 1 { "s" } else { "" }));
        }
        for (key, slot) in &extras {
            let text = self.slot_text(slot, recurse)?;
            output.push(format!("{}: {text}", Self::key_text(key)?));
        }
        self.seen.pop();
        self.reduce(output, "[", "]", true, recurse, Some(o))
    }

    fn element_is_numeric(&self, o: &Gc, idx: usize) -> R<bool> {
        let b = o.try_borrow().map_err(|_| Bail)?;
        let prop = match b.props.get_index(idx as u32) {
            Some(p) => Some(p),
            None => b.props.get(&idx.to_string()),
        };
        match prop {
            None => Ok(false),
            Some(p) if p.accessor() => Err(Bail),
            Some(p) => Ok(matches!(p.value(), Value::Num(_) | Value::BigInt(_))),
        }
    }

    fn reduce(
        &self,
        mut output: Vec<String>,
        open: &str,
        close: &str,
        array: bool,
        recurse: usize,
        value: Option<&Gc>,
    ) -> R<String> {
        let entries = output.len();
        if array && entries > 6 {
            output = self.group_array_elements(output, value)?;
        }
        if self.current_depth - recurse < COMPACT && entries == output.len() {
            let start = output.len() + self.indentation + len16(open) + 10;
            if self.below_break_length(&output, start) {
                let joined = output.join(", ");
                if !joined.contains('\n') {
                    return Ok(format!("{open} {joined} {close}"));
                }
            }
        }
        let indentation = format!("\n{}", " ".repeat(self.indentation));
        Ok(format!(
            "{open}{indentation}  {}{indentation}{close}",
            output.join(&format!(",{indentation}  "))
        ))
    }

    fn below_break_length(&self, output: &[String], start: usize) -> bool {
        let mut total = output.len() + start;
        if total + output.len() > BREAK_LENGTH {
            return false;
        }
        for entry in output {
            total += len16(entry);
            if total > BREAK_LENGTH {
                return false;
            }
        }
        true
    }

    fn group_array_elements(&self, output: Vec<String>, value: Option<&Gc>) -> R<Vec<String>> {
        let mut total_length = 0usize;
        let mut max_length = 0usize;
        let mut output_length = output.len();
        if MAX_ARRAY_LENGTH < output.len() {
            output_length -= 1;
        }
        const SEPARATOR_SPACE: usize = 2;
        let mut data_len = Vec::with_capacity(output_length);
        for entry in &output[..output_length] {
            if !entry.is_ascii() || entry.contains('\u{1b}') {
                return Err(Bail);
            }
            let width = entry.bytes().filter(|b| *b >= 0x20 && *b != 0x7f).count();
            data_len.push(width);
            total_length += width + SEPARATOR_SPACE;
            max_length = max_length.max(width);
        }
        let actual_max = max_length + SEPARATOR_SPACE;
        if actual_max * 3 + self.indentation < BREAK_LENGTH
            && (total_length as f64 / actual_max as f64 > 5.0 || max_length <= 6)
        {
            let average_bias = (actual_max as f64 - total_length as f64 / output.len() as f64).sqrt();
            let biased_max = (actual_max as f64 - 3.0 - average_bias).max(1.0);
            let by_shape = ((2.5 * biased_max * output_length as f64).sqrt() / biased_max + 0.5).floor();
            let by_width = ((BREAK_LENGTH - self.indentation) / actual_max) as f64;
            let columns = by_shape.min(by_width).min((COMPACT * 4) as f64).min(15.0);
            if columns <= 1.0 {
                return Ok(output);
            }
            let columns = columns as usize;
            let mut max_line_length = Vec::with_capacity(columns);
            for col in 0..columns {
                let mut line_length = 0;
                let mut j = col;
                while j < output_length {
                    line_length = line_length.max(data_len[j]);
                    j += columns;
                }
                max_line_length.push(line_length + SEPARATOR_SPACE);
            }
            let mut pad_start = true;
            if let Some(arr) = value {
                for idx in 0..output.len() {
                    if !self.element_is_numeric(arr, idx)? {
                        pad_start = false;
                        break;
                    }
                }
            }
            let mut grouped = Vec::new();
            let mut row = 0;
            while row < output_length {
                let max = (row + columns).min(output_length);
                let mut line = String::new();
                for j in row..max - 1 {
                    let cell = format!("{}, ", output[j]);
                    let width = max_line_length[j - row] + output[j].len() - data_len[j];
                    let pad = width.saturating_sub(cell.len());
                    if pad_start {
                        line.push_str(&" ".repeat(pad));
                        line.push_str(&cell);
                    } else {
                        line.push_str(&cell);
                        line.push_str(&" ".repeat(pad));
                    }
                }
                let j = max - 1;
                if pad_start {
                    let width = max_line_length[j - row] + output[j].len() - data_len[j] - SEPARATOR_SPACE;
                    line.push_str(&" ".repeat(width.saturating_sub(output[j].len())));
                }
                line.push_str(&output[j]);
                grouped.push(line);
                row += columns;
            }
            if MAX_ARRAY_LENGTH < output.len() {
                grouped.push(output[output_length].clone());
            }
            return Ok(grouped);
        }
        Ok(output)
    }
}
