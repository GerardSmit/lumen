//! Split out of builtins/mod.rs (behavior-preserving move).

use super::*;

pub(super) fn install_json(it: &mut Interp) {
    let j = it.new_object();
    it.def_method(&j, "stringify", 3, |i, _t, args| {
        crate::bytecode::reflect::caller_transparent(i);
        let value = arg(args, 0);
        let replacer = arg(args, 1);
        // The replacer is either a function, or an array PropertyList of keys (strings/numbers).
        let opts = if replacer.is_callable() {
            JsonOpts {
                func: Some(replacer),
                keys: None,
            }
        } else if json_is_array(i, &replacer)? {
            let len = match &replacer {
                Value::Obj(o) if proxy_pair(i, &replacer).is_none() => i.array_length(o),
                Value::Obj(o) => ab(i.to_length(o))?,
                _ => 0,
            };
            let mut list: Vec<String> = Vec::new();
            for k in 0..len {
                let item = ab(i.get_member(&replacer, &k.to_string()))?;
                // String/Number primitives and their wrappers contribute a key via ToString.
                let key = match &item {
                    Value::Str(s) => Some(s.to_string()),
                    Value::Num(n) => Some(i.num_to_str(*n)),
                    Value::Obj(o)
                        if matches!(o.borrow().exotic, Exotic::StrWrap | Exotic::NumWrap) =>
                    {
                        Some(ab(i.to_string(&item))?.to_string())
                    }
                    _ => None,
                };
                if let Some(key) = key {
                    if !list.contains(&key) {
                        list.push(key);
                    }
                }
            }
            JsonOpts {
                func: None,
                keys: Some(list),
            }
        } else {
            JsonOpts {
                func: None,
                keys: None,
            }
        };
        // The `space` argument: a Number/String wrapper is unwrapped first; then a Number becomes
        // that many spaces (clamped 0..10), a String its first 10 code units, else no indentation.
        let mut space = arg(args, 2);
        if let Value::Obj(o) = &space {
            let exotic = o.borrow().exotic.clone();
            match exotic {
                Exotic::NumWrap => space = Value::Num(ab(i.to_number(&space))?),
                Exotic::StrWrap => space = Value::Str(ab(i.to_string(&space))?),
                _ => {}
            }
        }
        let gap = match space {
            Value::Num(n) => {
                let n = if n.is_nan() { 0.0 } else { n.trunc() };
                " ".repeat(n.clamp(0.0, 10.0) as usize)
            }
            Value::Str(s) => s.chars().take(10).collect(),
            _ => String::new(),
        };
        // SerializeJSONProperty starts from a wrapper holder `{ "": value }`.
        // The wrapper is only observable by a replacer function (as its `this`).
        let holder = if opts.func.is_some() {
            let wrapper = i.new_object();
            set_data(&wrapper, "", value.clone());
            Value::Obj(wrapper)
        } else {
            Value::Undefined
        };
        let mut ser = JsonSer {
            opts: &opts,
            gap: &gap,
            indent: String::new(),
            stack: Vec::new(),
            out: String::new(),
        };
        if ser.property(i, &holder, JsonKey::Str(""), value)? {
            Ok(Value::from_string(ser.out))
        } else {
            Ok(Value::Undefined)
        }
    });
    it.def_method(&j, "parse", 2, |i, _t, args| {
        crate::bytecode::reflect::caller_transparent(i);
        let text = ab(i.to_string(&arg(args, 0)))?;
        let reviver = arg(args, 1);
        if !reviver.is_callable() {
            return JsonBytes::new(&text).document(i);
        }
        let chars: Vec<char> = text.chars().collect();
        let mut pos = 0;
        // A callable reviver walks the result via InternalizeJSONProperty from a `{ "": v }` root,
        // recording primitive source spans for its `context` argument.
        if reviver.is_callable() {
            let (v, record) = json_parse_recorded(i, &chars, &mut pos)?;
            json_skip_ws(&chars, &mut pos);
            if pos != chars.len() {
                return Err(i.make_error("SyntaxError", "Unexpected non-whitespace after JSON"));
            }
            let root = i.new_object();
            set_data(&root, "", v);
            let mut reviver = crate::bytecode::PreparedCall::new(i, reviver, Value::Undefined);
            let r =
                internalize_json_property(i, &Value::Obj(root), "", &mut reviver, Some(&record));
            reviver.finish(i);
            return r;
        }
        let v = json_parse_value(i, &chars, &mut pos)?;
        json_skip_ws(&chars, &mut pos);
        if pos != chars.len() {
            return Err(i.make_error("SyntaxError", "Unexpected non-whitespace after JSON"));
        }
        Ok(v)
    });
    it.def_method(&j, "rawJSON", 1, |i, _t, args| {
        let text = ab(i.to_string(&arg(args, 0)))?.to_string();
        let bytes: Vec<char> = text.chars().collect();
        if bytes.is_empty() {
            return Err(i.make_error("SyntaxError", "JSON.rawJSON: empty string"));
        }
        let is_ws = |c: char| matches!(c, '\t' | '\n' | '\r' | ' ');
        if is_ws(bytes[0]) || is_ws(*bytes.last().unwrap()) {
            return Err(i.make_error("SyntaxError", "JSON.rawJSON: leading/trailing whitespace"));
        }
        if bytes[0] == '{' || bytes[0] == '[' {
            return Err(i.make_error("SyntaxError", "JSON.rawJSON value must be a primitive"));
        }
        // Validate it is exactly one JSON value.
        let mut pos = 0;
        json_parse_value(i, &bytes, &mut pos)?;
        json_skip_ws(&bytes, &mut pos);
        if pos != bytes.len() {
            return Err(i.make_error("SyntaxError", "JSON.rawJSON: invalid JSON text"));
        }
        let o = i.new_object();
        o.borrow_mut().proto = None;
        set_data(&o, "rawJSON", Value::from_string(text.clone()));
        set_internal(&o, "\u{0}raw_json", Value::from_string(text));
        i.freeze_object(&Value::Obj(o.clone()));
        Ok(Value::Obj(o))
    });
    it.def_method(&j, "isRawJSON", 1, |_i, _t, args| {
        Ok(Value::Bool(
            matches!(arg(args, 0), Value::Obj(o) if o.borrow().props.contains("\u{0}raw_json")),
        ))
    });
    set_to_string_tag(it, &j, "JSON");
    set_builtin(&it.global, "JSON", Value::Obj(j));
}

/// Append the JSON quoting of `s` (QuoteJSONString) to `out`.
fn json_quote_into(out: &mut String, s: &str) {
    out.push('"');
    let bytes = s.as_bytes();
    // Plain-ASCII runs need no escaping and are copied as slices.
    let mut run = 0usize;
    let mut k = 0usize;
    while k < bytes.len() {
        let b = bytes[k];
        if b >= 0x20 && b != b'"' && b != b'\\' && b < 0x80 {
            k += 1;
            continue;
        }
        out.push_str(&s[run..k]);
        if b >= 0x80 {
            // Non-ASCII tail: the character-level loop (smuggled surrogates).
            json_quote_chars(out, &s[k..]);
            out.push('"');
            return;
        }
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x08 => out.push_str("\\b"),
            0x0C => out.push_str("\\f"),
            c => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", c);
            }
        }
        k += 1;
        run = k;
    }
    out.push_str(&s[run..]);
    out.push('"');
}

fn json_quote_chars(out: &mut String, s: &str) {
    use std::fmt::Write as _;
    let mut chars = s.chars().peekable();
    while let Some(mut c) = chars.next() {
        // A smuggled pair round-trips as its real character. If that character itself falls in
        // the smuggle range (a real plane-16 PUA code point), keep the smuggled representation so
        // it isn't re-read as a lone surrogate below.
        if let Some(real) = chars.peek().and_then(|&n| crate::jstr::paired_char(c, n)) {
            if crate::jstr::smuggled(real).is_some() {
                out.push(c);
                out.push(chars.next().unwrap());
                continue;
            }
            chars.next();
            c = real;
        }
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => match crate::jstr::smuggled(c) {
                // Well-formed JSON.stringify: a lone surrogate is written as its \u escape.
                Some(u) => {
                    let _ = write!(out, "\\u{u:04x}");
                }
                None => out.push(c),
            },
        }
    }
}

/// JSON.stringify options: an optional function replacer and/or an array PropertyList of keys.
struct JsonOpts {
    func: Option<Value>,
    keys: Option<Vec<String>>,
}

/// SerializeJSONProperty state: the options, the output being built, the current indentation
/// and the stack of objects being serialized (cycle detection).
struct JsonSer<'a> {
    opts: &'a JsonOpts,
    gap: &'a str,
    indent: String,
    stack: Vec<usize>,
    out: String,
}

/// A property key as SerializeJSONProperty sees it; an array index is only spelled out as a
/// string when user code (toJSON / a replacer function) receives it.
#[derive(Clone, Copy)]
enum JsonKey<'k> {
    Str(&'k str),
    Index(usize),
}

impl JsonKey<'_> {
    fn to_value(self) -> Value {
        match self {
            JsonKey::Str(s) => Value::from_string(s.to_string()),
            JsonKey::Index(n) => Value::from_string(n.to_string()),
        }
    }
}

/// Get(value, "toJSON") with the ordinary lookup done in place along a chain of plain objects
/// (any exotic, proxy or accessor on the way takes the generic [[Get]]).
fn json_get_tojson(i: &mut Interp, value: &Value) -> Result<Value, Value> {
    if let Value::Obj(o) = value {
        let mut cur = o.clone();
        loop {
            let next = {
                let b = cur.borrow();
                if !b.ic_plain.get() || !matches!(b.exotic, Exotic::None | Exotic::Array) {
                    break;
                }
                match b.props.get("toJSON") {
                    Some(p) if p.accessor() => break,
                    Some(p) => return Ok(p.value()),
                    None => b.proto.clone(),
                }
            };
            match next {
                Some(p) => cur = p,
                None => return Ok(Value::Undefined),
            }
        }
    }
    ab(i.get_member(value, "toJSON"))
}

/// Get(O, key) for an object being serialized: an own plain data property is read in place.
fn json_get_prop(i: &mut Interp, o: &Gc, ov: &Value, key: &str) -> Result<Value, Value> {
    {
        let b = o.borrow();
        if b.ic_plain.get() && matches!(b.exotic, Exotic::None | Exotic::Array) {
            if let Some(p) = b.props.get(key) {
                if !p.accessor() {
                    return Ok(p.value());
                }
            }
        }
    }
    ab(i.get_member(ov, key))
}

impl JsonSer<'_> {
    fn newline(&mut self) {
        if !self.gap.is_empty() {
            self.out.push('\n');
            self.out.push_str(&self.indent);
        }
    }

    /// SerializeJSONProperty for `value` (already read from `holder[key]`). Writes the text and
    /// returns true, or writes nothing and returns false for `undefined`.
    fn property(
        &mut self,
        i: &mut Interp,
        holder: &Value,
        key: JsonKey,
        mut value: Value,
    ) -> Result<bool, Value> {
        if matches!(value, Value::Obj(_) | Value::BigInt(_)) {
            let tojson = json_get_tojson(i, &value)?;
            if tojson.is_callable() {
                value = ab(i.call(tojson, value.clone(), &[key.to_value()]))?;
            }
        }
        if let Some(func) = &self.opts.func {
            value = ab(i.call(func.clone(), holder.clone(), &[key.to_value(), value]))?;
        }
        if let Value::Obj(o) = &value {
            // A JSON.rawJSON object serializes as its stored raw text, verbatim.
            let (exotic, wrapped_bool, raw) = {
                let b = o.borrow();
                let raw = if matches!(b.exotic, Exotic::None) {
                    match b.props.get("\u{0}raw_json").map(|p| p.value()) {
                        Some(Value::Str(raw)) => Some(raw),
                        _ => None,
                    }
                } else {
                    None
                };
                (b.exotic, b.bool_wrap(), raw)
            };
            if let Some(raw) = raw {
                self.out.push_str(&raw);
                return Ok(true);
            }
            // A primitive-wrapper object re-coerces through ToNumber/ToString (so an overridden
            // valueOf/toString is observed); booleans read the wrapped datum directly.
            match exotic {
                Exotic::NumWrap => value = Value::Num(ab(i.to_number(&value))?),
                Exotic::StrWrap => value = Value::Str(ab(i.to_string(&value))?),
                Exotic::BoolWrap => value = Value::Bool(wrapped_bool.unwrap_or(false)),
                Exotic::BigIntWrap => {
                    return Err(i.make_error("TypeError", "Do not know how to serialize a BigInt"));
                }
                _ => {}
            }
        }
        match value {
            Value::Undefined | Value::Empty | Value::Sym(_) => Ok(false),
            Value::Null => {
                self.out.push_str("null");
                Ok(true)
            }
            Value::Bool(b) => {
                self.out.push_str(if b { "true" } else { "false" });
                Ok(true)
            }
            Value::Num(n) => {
                if !n.is_finite() {
                    self.out.push_str("null");
                } else if !num_fmt::fast_num_to_str(n, &mut self.out) {
                    self.out.push_str(&i.num_to_str(n));
                }
                Ok(true)
            }
            Value::BigInt(_) => {
                Err(i.make_error("TypeError", "Do not know how to serialize a BigInt"))
            }
            Value::Str(s) => {
                json_quote_into(&mut self.out, &s);
                Ok(true)
            }
            Value::Obj(ref o) => {
                if o.borrow().call.is_fn() {
                    return Ok(false); // functions are omitted
                }
                let ptr = Gc::as_ptr(o) as usize;
                if self.stack.contains(&ptr) {
                    return Err(i.make_error("TypeError", "Converting circular structure to JSON"));
                }
                self.stack.push(ptr);
                let outer_len = self.indent.len();
                self.indent.push_str(self.gap);
                // IsArray sees through proxies; key enumeration / length use proxy-aware
                // operations.
                let is_array = json_is_array(i, &value)?;
                let is_proxy = proxy_pair(i, &value).is_some();
                if is_array {
                    let len = if is_proxy {
                        ab(i.to_length(o))?
                    } else {
                        i.array_length(o)
                    };
                    self.out.push('[');
                    for k in 0..len {
                        if k > 0 {
                            self.out.push(',');
                        }
                        self.newline();
                        let v = array_fast::get_elem(i, o, &value, k)?;
                        if !self.property(i, &value, JsonKey::Index(k), v)? {
                            self.out.push_str("null");
                        }
                    }
                    self.indent.truncate(outer_len);
                    if len > 0 {
                        self.newline();
                    }
                    self.out.push(']');
                } else {
                    // An array replacer restricts the keys (in its order); else all enumerable
                    // own string keys, snapshotted before any value is read.
                    let owned: Vec<Rc<str>>;
                    let listed: Vec<Rc<str>>;
                    let keys: &[Rc<str>] = match &self.opts.keys {
                        Some(list) => {
                            listed = list.iter().map(|k| Rc::from(k.as_str())).collect();
                            &listed
                        }
                        None if is_proxy => {
                            owned = proxy_enum_string_keys(i, &value)?
                                .iter()
                                .filter_map(|k| match k {
                                    Value::Str(s) => Some(Rc::from(&**s)),
                                    _ => None,
                                })
                                .collect();
                            &owned
                        }
                        None => {
                            owned = match ta_info(i, o) {
                                // A TypedArray's enumerable own keys: its indices, then string
                                // expandos.
                                Some(info) => {
                                    let n = i.ta_len(&info).unwrap_or(0);
                                    let mut keys: Vec<Rc<str>> =
                                        (0..n).map(|k| Rc::from(k.to_string())).collect();
                                    keys.extend(ordered_enum_keys(o).into_iter().filter(|k| {
                                        k.parse::<usize>().is_err()
                                            && !TA_META_KEYS.contains(&&**k)
                                    }));
                                    keys
                                }
                                None => ordered_enum_keys(o),
                            };
                            &owned
                        }
                    };
                    self.out.push('{');
                    let mut any = false;
                    for k in keys {
                        let v = json_get_prop(i, o, &value, k)?;
                        let mark = self.out.len();
                        if any {
                            self.out.push(',');
                        }
                        self.newline();
                        json_quote_into(&mut self.out, k);
                        self.out.push(':');
                        if !self.gap.is_empty() {
                            self.out.push(' ');
                        }
                        if self.property(i, &value, JsonKey::Str(k), v)? {
                            any = true;
                        } else {
                            self.out.truncate(mark);
                        }
                    }
                    self.indent.truncate(outer_len);
                    if any {
                        self.newline();
                    }
                    self.out.push('}');
                }
                self.stack.pop();
                Ok(true)
            }
        }
    }
}

/// [[Delete]] for the JSON reviver: trap-aware, discarding the boolean status (per `Perform ?`).
fn json_delete_prop(i: &mut Interp, holder: &Value, key: &str) -> Result<(), Value> {
    if let Some((target, handler)) = proxy_pair(i, holder) {
        ab(i.proxy_delete(target, handler, key))?;
        return Ok(());
    }
    if let Value::Obj(o) = holder {
        let configurable = o
            .borrow()
            .props
            .get(key)
            .map(|p| p.configurable())
            .unwrap_or(true);
        if configurable {
            o.borrow_mut().props.remove(key);
        }
    }
    Ok(())
}

/// CreateDataProperty for the JSON reviver: a { value, writable, enumerable, configurable: true }
/// data property, trap-aware for proxy holders.
fn json_create_data_prop(i: &mut Interp, holder: &Value, key: &str, v: Value) -> Result<(), Value> {
    // CreateDataProperty defines a fully-permissive data descriptor via [[DefineOwnProperty]], which
    // validates against any existing (e.g. non-configurable) property; a false result is not an error.
    let desc = i.new_object();
    set_data(&desc, "value", v);
    set_data(&desc, "writable", Value::Bool(true));
    set_data(&desc, "enumerable", Value::Bool(true));
    set_data(&desc, "configurable", Value::Bool(true));
    if let Some((target, handler)) = proxy_pair(i, holder) {
        ab(proxy_define_property(
            i,
            &target,
            &handler,
            key,
            &Value::Obj(desc),
        ))?;
        return Ok(());
    }
    if let Value::Obj(o) = holder {
        ab(define_own_property(i, o, key, &Value::Obj(desc)))?;
    }
    Ok(())
}

/// InternalizeJSONProperty: recursively walk the parsed value, applying the reviver bottom-up. The
/// optional record carries primitive source text for the reviver's `context.source`.
fn internalize_json_property(
    i: &mut Interp,
    holder: &Value,
    name: &str,
    reviver: &mut crate::bytecode::PreparedCall,
    record: Option<&JsonRecord>,
) -> Result<Value, Value> {
    let val = ab(i.get_member(holder, name))?;
    if matches!(val, Value::Obj(_)) {
        if json_is_array(i, &val)? {
            let len = match val.as_obj() {
                Some(o) => ab(i.to_length(o))?,
                None => 0,
            };
            for idx in 0..len {
                let k = idx.to_string();
                let child = match record {
                    Some(JsonRecord::Arr(elems)) => elems.get(idx),
                    _ => None,
                };
                let new_el = internalize_json_property(i, &val, &k, reviver, child)?;
                if matches!(new_el, Value::Undefined) {
                    json_delete_prop(i, &val, &k)?;
                } else {
                    json_create_data_prop(i, &val, &k, new_el)?;
                }
            }
        } else {
            let keys: Vec<String> = if proxy_pair(i, &val).is_some() {
                proxy_enum_string_keys(i, &val)?
                    .iter()
                    .filter_map(|k| match k {
                        Value::Str(s) => Some(s.to_string()),
                        _ => None,
                    })
                    .collect()
            } else if let Some(info) = val.as_obj().and_then(|o| ta_info(i, o)) {
                // A TypedArray holder (a reviver may graft one in) walks its integer indices.
                (0..i.ta_len(&info).unwrap_or(0))
                    .map(|k| k.to_string())
                    .collect()
            } else {
                val.as_obj()
                    .map(|o| ordered_enum_keys(o).iter().map(|k| k.to_string()).collect())
                    .unwrap_or_default()
            };
            for k in keys {
                let child = match record {
                    Some(JsonRecord::Obj(entries)) => {
                        entries.iter().find(|(key, _)| key == &k).map(|(_, r)| r)
                    }
                    _ => None,
                };
                let new_el = internalize_json_property(i, &val, &k, reviver, child)?;
                if matches!(new_el, Value::Undefined) {
                    json_delete_prop(i, &val, &k)?;
                } else {
                    json_create_data_prop(i, &val, &k, new_el)?;
                }
            }
        }
    }
    // The `context` argument: a primitive leaf whose value is still the originally-parsed one (i.e.
    // not forward-modified by the reviver) exposes its exact source text.
    let context = i.new_object();
    if let Some(JsonRecord::Prim(src, parsed)) = record {
        if !matches!(val, Value::Obj(_)) && same_value(&val, parsed) {
            set_data(&context, "source", Value::from_string(src.clone()));
        }
    }
    ab(reviver.call_with_this(
        i,
        holder.clone(),
        &mut [
            Value::from_string(name.to_string()),
            val,
            Value::Obj(context),
        ],
    ))
}

/// JSON.parse without a reviver, over the source's UTF-8 bytes. Every structural character is
/// ASCII, so string contents are copied as byte runs; object keys that repeat within one parse
/// share one key allocation.
struct JsonBytes<'a> {
    b: &'a [u8],
    src: &'a str,
    pos: usize,
    keys: crate::fasthash::FastMap<&'a str, Rc<str>>,
}

impl<'a> JsonBytes<'a> {
    fn new(src: &'a str) -> Self {
        JsonBytes {
            b: src.as_bytes(),
            src,
            pos: 0,
            keys: Default::default(),
        }
    }

    #[inline]
    fn ws(&mut self) {
        while let Some(&c) = self.b.get(self.pos) {
            if matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    #[inline]
    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    fn document(&mut self, i: &mut Interp) -> Result<Value, Value> {
        let v = self.value(i)?;
        self.ws();
        if self.pos != self.b.len() {
            return Err(i.make_error("SyntaxError", "Unexpected non-whitespace after JSON"));
        }
        Ok(v)
    }

    fn value(&mut self, i: &mut Interp) -> Result<Value, Value> {
        self.ws();
        let Some(c) = self.peek() else {
            return Err(i.make_error("SyntaxError", "Unexpected end of JSON input"));
        };
        match c {
            b'{' => {
                self.pos += 1;
                let obj = i.new_object();
                self.ws();
                if self.peek() == Some(b'}') {
                    self.pos += 1;
                    return Ok(Value::Obj(obj));
                }
                loop {
                    self.ws();
                    if self.peek() != Some(b'"') {
                        return Err(i.make_error("SyntaxError", "Expected string key in JSON object"));
                    }
                    let key = self.key(i)?;
                    self.ws();
                    if self.peek() != Some(b':') {
                        return Err(i.make_error("SyntaxError", "Expected ':' in JSON object"));
                    }
                    self.pos += 1;
                    let v = self.value(i)?;
                    obj.borrow_mut().props.insert(key, Property::plain(v));
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.pos += 1,
                        Some(b'}') => {
                            self.pos += 1;
                            break;
                        }
                        _ => {
                            return Err(
                                i.make_error("SyntaxError", "Expected ',' or '}' in JSON object")
                            );
                        }
                    }
                }
                Ok(Value::Obj(obj))
            }
            b'[' => {
                self.pos += 1;
                let mut items = Vec::new();
                self.ws();
                if self.peek() == Some(b']') {
                    self.pos += 1;
                    return Ok(i.make_array(items));
                }
                loop {
                    items.push(self.value(i)?);
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.pos += 1,
                        Some(b']') => {
                            self.pos += 1;
                            break;
                        }
                        _ => {
                            return Err(
                                i.make_error("SyntaxError", "Expected ',' or ']' in JSON array")
                            );
                        }
                    }
                }
                Ok(i.make_array(items))
            }
            b'"' => {
                let s = self.string(i)?;
                Ok(match s {
                    Ok(raw) => Value::str(raw),
                    Err(owned) => Value::from_string(owned),
                })
            }
            b't' => self.lit(i, b"true", Value::Bool(true)),
            b'f' => self.lit(i, b"false", Value::Bool(false)),
            b'n' => self.lit(i, b"null", Value::Null),
            b'-' | b'0'..=b'9' => self.number(i),
            _ => Err(i.make_error("SyntaxError", "Unexpected token in JSON")),
        }
    }

    fn lit(&mut self, i: &mut Interp, lit: &[u8], v: Value) -> Result<Value, Value> {
        if self.b[self.pos..].starts_with(lit) {
            self.pos += lit.len();
            Ok(v)
        } else {
            Err(i.make_error("SyntaxError", "Invalid literal in JSON"))
        }
    }

    fn number(&mut self, i: &mut Interp) -> Result<Value, Value> {
        // Strict JSON number grammar: -?(0|[1-9]\d*)(\.\d+)?([eE][+-]?\d+)? — no leading zeros, a
        // mandatory integer part, and at least one digit after `.` / exponent.
        let start = self.pos;
        let digits = |p: &mut usize, b: &[u8]| {
            let s = *p;
            while b.get(*p).is_some_and(|c| c.is_ascii_digit()) {
                *p += 1;
            }
            *p - s
        };
        let neg = self.peek() == Some(b'-');
        if neg {
            self.pos += 1;
        }
        let int_start = self.pos;
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                digits(&mut self.pos, self.b);
            }
            _ => return Err(i.make_error("SyntaxError", "Invalid number in JSON")),
        }
        let int_end = self.pos;
        let mut simple = true;
        if self.peek() == Some(b'.') {
            simple = false;
            self.pos += 1;
            if digits(&mut self.pos, self.b) == 0 {
                return Err(i.make_error("SyntaxError", "Invalid number in JSON"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            simple = false;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if digits(&mut self.pos, self.b) == 0 {
                return Err(i.make_error("SyntaxError", "Invalid number in JSON"));
            }
        }
        if simple && int_end - int_start <= 15 {
            let mut n = 0u64;
            for &c in &self.b[int_start..int_end] {
                n = n * 10 + (c - b'0') as u64;
            }
            let n = n as f64;
            return Ok(Value::Num(if neg { -n } else { n }));
        }
        self.src[start..self.pos]
            .parse::<f64>()
            .map(Value::Num)
            .map_err(|_| i.make_error("SyntaxError", "Invalid number in JSON"))
    }

    /// An object key, shared across the parse when it has no escapes.
    fn key(&mut self, i: &mut Interp) -> Result<Rc<str>, Value> {
        match self.string(i)? {
            Ok(raw) => {
                if let Some(k) = self.keys.get(raw) {
                    return Ok(k.clone());
                }
                let k: Rc<str> = Rc::from(raw);
                self.keys.insert(raw, k.clone());
                Ok(k)
            }
            Err(owned) => Ok(Rc::from(owned)),
        }
    }

    /// A string literal at `pos` (on its opening quote): `Ok(slice)` when it is a verbatim span
    /// of the source (no escapes), else `Err(decoded)`.
    fn string(&mut self, i: &mut Interp) -> Result<Result<&'a str, String>, Value> {
        self.pos += 1; // opening quote
        let start = self.pos;
        // Fast scan: the common escape-free literal is a slice of the (canonical) source.
        loop {
            match self.b.get(self.pos) {
                None => return Err(i.make_error("SyntaxError", "Unterminated JSON string")),
                Some(b'"') => {
                    let s = &self.src[start..self.pos];
                    self.pos += 1;
                    return Ok(Ok(s));
                }
                Some(b'\\') => break,
                Some(&c) if c < 0x20 => {
                    return Err(
                        i.make_error("SyntaxError", "Unescaped control character in JSON string")
                    );
                }
                Some(_) => self.pos += 1,
            }
        }
        let mut s = String::from(&self.src[start..self.pos]);
        let mut surrogate = false;
        loop {
            let run = self.pos;
            loop {
                match self.b.get(self.pos) {
                    None => return Err(i.make_error("SyntaxError", "Unterminated JSON string")),
                    Some(b'"' | b'\\') => break,
                    Some(&c) if c < 0x20 => {
                        return Err(i.make_error(
                            "SyntaxError",
                            "Unescaped control character in JSON string",
                        ));
                    }
                    Some(_) => self.pos += 1,
                }
            }
            s.push_str(&self.src[run..self.pos]);
            if self.b[self.pos] == b'"' {
                self.pos += 1;
                break;
            }
            self.pos += 1; // backslash
            let Some(e) = self.peek() else {
                return Err(i.make_error("SyntaxError", "Bad escape in JSON"));
            };
            self.pos += 1;
            match e {
                b'"' => s.push('"'),
                b'\\' => s.push('\\'),
                b'/' => s.push('/'),
                b'n' => s.push('\n'),
                b't' => s.push('\t'),
                b'r' => s.push('\r'),
                b'b' => s.push('\u{0008}'),
                b'f' => s.push('\u{000C}'),
                b'u' => {
                    let n = self
                        .hex4(self.pos)
                        .ok_or_else(|| i.make_error("SyntaxError", "Bad \\u escape in JSON"))?;
                    self.pos += 4;
                    if (0xD800..0xDC00).contains(&n)
                        && self.b.get(self.pos) == Some(&b'\\')
                        && self.b.get(self.pos + 1) == Some(&b'u')
                    {
                        // A high surrogate followed by \uDCxx forms a pair.
                        if let Some(n2) = self.hex4(self.pos + 2) {
                            if (0xDC00..0xE000).contains(&n2) {
                                self.pos += 6;
                                let c = 0x10000 + ((n - 0xD800) << 10) + (n2 - 0xDC00);
                                s.push(char::from_u32(c).unwrap());
                                continue;
                            }
                        }
                    }
                    if (0xD800..0xE000).contains(&n) {
                        surrogate = true;
                        s.push(crate::jstr::smuggle(n as u16));
                    } else {
                        s.push(char::from_u32(n).unwrap_or('\u{FFFD}'));
                    }
                }
                _ => return Err(i.make_error("SyntaxError", "Bad escape in JSON")),
            }
        }
        if surrogate {
            // An escaped lone half next to a (literal or escaped) partner recombines.
            if let Some(c) = crate::jstr::canonicalize(&s) {
                s = c;
            }
        }
        Ok(Err(s))
    }

    fn hex4(&self, at: usize) -> Option<u32> {
        let h = self.b.get(at..at + 4)?;
        let mut n = 0u32;
        for &c in h {
            n = n * 16 + (c as char).to_digit(16)?;
        }
        Some(n)
    }
}

fn json_skip_ws(chars: &[char], pos: &mut usize) {
    while *pos < chars.len() && matches!(chars[*pos], ' ' | '\t' | '\n' | '\r') {
        *pos += 1;
    }
}

fn json_parse_value(i: &mut Interp, chars: &[char], pos: &mut usize) -> Result<Value, Value> {
    json_skip_ws(chars, pos);
    let c = *chars
        .get(*pos)
        .ok_or_else(|| i.make_error("SyntaxError", "Unexpected end of JSON input"))?;
    match c {
        '{' => {
            *pos += 1;
            let obj = i.new_object();
            json_skip_ws(chars, pos);
            if chars.get(*pos) == Some(&'}') {
                *pos += 1;
                return Ok(Value::Obj(obj));
            }
            loop {
                json_skip_ws(chars, pos);
                if chars.get(*pos) != Some(&'"') {
                    return Err(i.make_error("SyntaxError", "Expected string key in JSON object"));
                }
                let key = json_parse_string(i, chars, pos)?;
                json_skip_ws(chars, pos);
                if chars.get(*pos) != Some(&':') {
                    return Err(i.make_error("SyntaxError", "Expected ':' in JSON object"));
                }
                *pos += 1;
                let v = json_parse_value(i, chars, pos)?;
                set_data(&obj, &key, v);
                json_skip_ws(chars, pos);
                match chars.get(*pos) {
                    Some(',') => {
                        *pos += 1;
                    }
                    Some('}') => {
                        *pos += 1;
                        break;
                    }
                    _ => {
                        return Err(
                            i.make_error("SyntaxError", "Expected ',' or '}' in JSON object")
                        );
                    }
                }
            }
            Ok(Value::Obj(obj))
        }
        '[' => {
            *pos += 1;
            let mut items = Vec::new();
            json_skip_ws(chars, pos);
            if chars.get(*pos) == Some(&']') {
                *pos += 1;
                return Ok(i.make_array(items));
            }
            loop {
                items.push(json_parse_value(i, chars, pos)?);
                json_skip_ws(chars, pos);
                match chars.get(*pos) {
                    Some(',') => {
                        *pos += 1;
                    }
                    Some(']') => {
                        *pos += 1;
                        break;
                    }
                    _ => {
                        return Err(
                            i.make_error("SyntaxError", "Expected ',' or ']' in JSON array")
                        );
                    }
                }
            }
            Ok(i.make_array(items))
        }
        '"' => Ok(Value::from_string(json_parse_string(i, chars, pos)?)),
        't' => json_parse_lit(i, chars, pos, "true", Value::Bool(true)),
        'f' => json_parse_lit(i, chars, pos, "false", Value::Bool(false)),
        'n' => json_parse_lit(i, chars, pos, "null", Value::Null),
        '-' | '0'..='9' => {
            let start = *pos;
            // Strict JSON number grammar: -?(0|[1-9]\d*)(\.\d+)?([eE][+-]?\d+)? — no leading
            // zeros, a mandatory integer part, and at least one digit after `.` / exponent.
            let err = || i.make_error("SyntaxError", "Invalid number in JSON");
            let at = |p: usize| chars.get(p).copied();
            if at(*pos) == Some('-') {
                *pos += 1;
            }
            match at(*pos) {
                Some('0') => *pos += 1,
                Some('1'..='9') => {
                    while matches!(at(*pos), Some('0'..='9')) {
                        *pos += 1;
                    }
                }
                _ => return Err(err()),
            }
            if at(*pos) == Some('.') {
                *pos += 1;
                if !matches!(at(*pos), Some('0'..='9')) {
                    return Err(err());
                }
                while matches!(at(*pos), Some('0'..='9')) {
                    *pos += 1;
                }
            }
            if matches!(at(*pos), Some('e' | 'E')) {
                *pos += 1;
                if matches!(at(*pos), Some('+' | '-')) {
                    *pos += 1;
                }
                if !matches!(at(*pos), Some('0'..='9')) {
                    return Err(err());
                }
                while matches!(at(*pos), Some('0'..='9')) {
                    *pos += 1;
                }
            }
            let s: String = chars[start..*pos].iter().collect();
            s.parse::<f64>().map(Value::Num).map_err(|_| err())
        }
        _ => Err(i.make_error("SyntaxError", "Unexpected token in JSON")),
    }
}

/// A parallel parse tree recording the source text of every primitive leaf, so the JSON.parse
/// reviver can receive a `context` argument with a `source` property (ES2025 source-text access).
enum JsonRecord {
    Prim(String, Value),
    Arr(Vec<JsonRecord>),
    Obj(Vec<(String, JsonRecord)>),
}

/// Mirror of `json_parse_value` that also returns a `JsonRecord`. Containers are framed inline;
/// primitive leaves delegate to `json_parse_value` and capture their exact source span.
fn json_parse_recorded(
    i: &mut Interp,
    chars: &[char],
    pos: &mut usize,
) -> Result<(Value, JsonRecord), Value> {
    json_skip_ws(chars, pos);
    let c = *chars
        .get(*pos)
        .ok_or_else(|| i.make_error("SyntaxError", "Unexpected end of JSON input"))?;
    match c {
        '{' => {
            *pos += 1;
            let obj = i.new_object();
            let mut rec: Vec<(String, JsonRecord)> = Vec::new();
            json_skip_ws(chars, pos);
            if chars.get(*pos) == Some(&'}') {
                *pos += 1;
                return Ok((Value::Obj(obj), JsonRecord::Obj(rec)));
            }
            loop {
                json_skip_ws(chars, pos);
                if chars.get(*pos) != Some(&'"') {
                    return Err(i.make_error("SyntaxError", "Expected string key in JSON object"));
                }
                let key = json_parse_string(i, chars, pos)?;
                json_skip_ws(chars, pos);
                if chars.get(*pos) != Some(&':') {
                    return Err(i.make_error("SyntaxError", "Expected ':' in JSON object"));
                }
                *pos += 1;
                let (v, vr) = json_parse_recorded(i, chars, pos)?;
                set_data(&obj, &key, v);
                rec.retain(|(k, _)| k != &key);
                rec.push((key, vr));
                json_skip_ws(chars, pos);
                match chars.get(*pos) {
                    Some(',') => *pos += 1,
                    Some('}') => {
                        *pos += 1;
                        break;
                    }
                    _ => {
                        return Err(
                            i.make_error("SyntaxError", "Expected ',' or '}' in JSON object")
                        );
                    }
                }
            }
            Ok((Value::Obj(obj), JsonRecord::Obj(rec)))
        }
        '[' => {
            *pos += 1;
            let mut items = Vec::new();
            let mut rec = Vec::new();
            json_skip_ws(chars, pos);
            if chars.get(*pos) == Some(&']') {
                *pos += 1;
                return Ok((i.make_array(items), JsonRecord::Arr(rec)));
            }
            loop {
                let (v, vr) = json_parse_recorded(i, chars, pos)?;
                items.push(v);
                rec.push(vr);
                json_skip_ws(chars, pos);
                match chars.get(*pos) {
                    Some(',') => *pos += 1,
                    Some(']') => {
                        *pos += 1;
                        break;
                    }
                    _ => {
                        return Err(
                            i.make_error("SyntaxError", "Expected ',' or ']' in JSON array")
                        );
                    }
                }
            }
            Ok((i.make_array(items), JsonRecord::Arr(rec)))
        }
        _ => {
            let start = *pos;
            let v = json_parse_value(i, chars, pos)?;
            let src: String = chars[start..*pos].iter().collect();
            Ok((v.clone(), JsonRecord::Prim(src, v)))
        }
    }
}

fn json_parse_lit(
    i: &mut Interp,
    chars: &[char],
    pos: &mut usize,
    lit: &str,
    val: Value,
) -> Result<Value, Value> {
    for expect in lit.chars() {
        if chars.get(*pos) != Some(&expect) {
            return Err(i.make_error("SyntaxError", "Invalid literal in JSON"));
        }
        *pos += 1;
    }
    Ok(val)
}

fn json_parse_string(i: &mut Interp, chars: &[char], pos: &mut usize) -> Result<String, Value> {
    *pos += 1; // opening quote
    let mut s = String::new();
    loop {
        let c = *chars
            .get(*pos)
            .ok_or_else(|| i.make_error("SyntaxError", "Unterminated JSON string"))?;
        *pos += 1;
        match c {
            '"' => return Ok(s),
            '\\' => {
                let e = *chars
                    .get(*pos)
                    .ok_or_else(|| i.make_error("SyntaxError", "Bad escape in JSON"))?;
                *pos += 1;
                match e {
                    '"' => s.push('"'),
                    '\\' => s.push('\\'),
                    '/' => s.push('/'),
                    'n' => s.push('\n'),
                    't' => s.push('\t'),
                    'r' => s.push('\r'),
                    'b' => s.push('\u{0008}'),
                    'f' => s.push('\u{000C}'),
                    'u' => {
                        let hex: String = chars[*pos..(*pos + 4).min(chars.len())].iter().collect();
                        *pos += 4;
                        let n = u32::from_str_radix(&hex, 16)
                            .map_err(|_| i.make_error("SyntaxError", "Bad \\u escape in JSON"))?;
                        if (0xD800..0xDC00).contains(&n)
                            && chars.get(*pos) == Some(&'\\')
                            && chars.get(*pos + 1) == Some(&'u')
                        {
                            // A high surrogate followed by \uDCxx forms a pair.
                            let hex2: String = chars
                                [(*pos + 2).min(chars.len())..(*pos + 6).min(chars.len())]
                                .iter()
                                .collect();
                            if let Ok(n2) = u32::from_str_radix(&hex2, 16) {
                                if (0xDC00..0xE000).contains(&n2) {
                                    *pos += 6;
                                    let c = 0x10000 + ((n - 0xD800) << 10) + (n2 - 0xDC00);
                                    s.push(char::from_u32(c).unwrap());
                                    continue;
                                }
                            }
                        }
                        if (0xD800..0xE000).contains(&n) {
                            s.push(crate::jstr::smuggle(n as u16));
                        } else {
                            s.push(char::from_u32(n).unwrap_or('\u{FFFD}'));
                        }
                    }
                    _ => return Err(i.make_error("SyntaxError", "Bad escape in JSON")),
                }
            }
            // Unescaped control characters (U+0000–U+001F) are not allowed in JSON strings.
            c if (c as u32) < 0x20 => {
                return Err(
                    i.make_error("SyntaxError", "Unescaped control character in JSON string")
                );
            }
            c => s.push(c),
        }
    }
}
