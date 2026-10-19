//! Split out of builtins/mod.rs (behavior-preserving move).

use super::*;
use lumen_common::json;
use std::borrow::Cow;

pub(super) fn parse_module_default(i:&mut Interp,text:&str)->Result<Value,Value> {
    json::Parser::new(text,PARSE).document(&mut JsSink::new(i,text))
}

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
            stack: Default::default(),
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
        let mut parser = json::Parser::new(&text, PARSE);
        if !reviver.is_callable() {
            return parser.document(&mut JsSink::new(i, &text));
        }
        // A callable reviver walks the result via InternalizeJSONProperty from a `{ "": v }` root,
        // recording primitive source spans for its `context` argument.
        let (v, record) = parser.document(&mut RecordSink { i, src: &text })?;
        let root = i.new_object();
        set_data(&root, "", v);
        let mut reviver = crate::bytecode::PreparedCall::new(i, reviver, Value::Undefined);
        let r = internalize_json_property(i, &Value::Obj(root), "", &mut reviver, Some(&record));
        reviver.finish(i);
        r
    });
    it.def_method(&j, "rawJSON", 1, |i, _t, args| {
        let text = ab(i.to_string(&arg(args, 0)))?.to_string();
        let (Some(first), Some(last)) = (text.bytes().next(), text.bytes().last()) else {
            return Err(i.make_error("SyntaxError", "JSON.rawJSON: empty string"));
        };
        let is_ws = |c: u8| matches!(c, b'\t' | b'\n' | b'\r' | b' ');
        if is_ws(first) || is_ws(last) {
            return Err(i.make_error("SyntaxError", "JSON.rawJSON: leading/trailing whitespace"));
        }
        if first == b'{' || first == b'[' {
            return Err(i.make_error("SyntaxError", "JSON.rawJSON value must be a primitive"));
        }
        json::Parser::new(&text, PARSE).document(&mut JsSink::new(i, &text))?;
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

/// QuoteJSONString: well-formed, so lone surrogates are written as `\u` escapes.
const STRINGIFY: json::Quote = json::Quote {
    lone_surrogates: true,
    spelling: json::Spelling::Utf16,
    ..json::Quote::JSON
};

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
    stack: crate::fasthash::FastSet<usize>,
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
                json::quote_into(&mut self.out, &s, &STRINGIFY);
                Ok(true)
            }
            Value::Obj(ref o) => {
                if o.borrow().call.is_fn() {
                    return Ok(false); // functions are omitted
                }
                let ptr = Gc::as_ptr(o) as usize;
                if !self.stack.insert(ptr) {
                    return Err(i.make_error("TypeError", "Converting circular structure to JSON"));
                }
                ab(i.check_native_stack())?;
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
                            i.materialize(o);
                            owned = match ta_info(i, o) {
                                // A TypedArray's enumerable own keys: its indices, then string
                                // expandos.
                                Some(info) => {
                                    let n = i.ta_len(&info).unwrap_or(0);
                                    let mut keys: Vec<Rc<str>> =
                                        (0..n).map(|k| Rc::from(k.to_string())).collect();
                                    keys.extend(ordered_enum_keys(o).into_iter().filter(|k| {
                                        k.parse::<usize>().is_err() && !TA_META_KEYS.contains(&&**k)
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
                        json::quote_into(&mut self.out, k, &STRINGIFY);
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
                self.stack.remove(&ptr);
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
        i.materialize(o);
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
        ab(i.check_native_stack())?;
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
                    .map(|o| {
                        i.materialize(o);
                        ordered_enum_keys(o).iter().map(|k| k.to_string()).collect()
                    })
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

/// A parallel parse tree recording the source text of every primitive leaf, so the JSON.parse
/// reviver can receive a `context` argument with a `source` property (ES2025 source-text access).
enum JsonRecord {
    Prim(String, Value),
    Arr(Vec<JsonRecord>),
    Obj(Vec<(String, JsonRecord)>),
}

const PARSE: json::Options = json::Options {
    strict: true,
    constants: false,
    jsonc: false,
    spelling: json::Spelling::Utf16,
};

fn parse_error(i: &mut Interp, src: &str, e: json::Error) -> Value {
    use json::ErrorKind as K;
    if e.kind == K::ExpectingValue && e.pos >= src.len() {
        return i.make_error("SyntaxError", "Unexpected end of JSON input");
    }
    let what = match e.kind {
        K::ExpectingValue => "Unexpected token",
        K::ExpectingKey => "Expected property name",
        K::ExpectingColon => "Expected ':' after property name",
        K::ExpectingComma => "Expected ',' or closing bracket",
        K::ExtraData => "Unexpected non-whitespace character after JSON",
        K::UnterminatedString => "Unterminated string",
        K::ControlCharacter => "Bad control character in string literal",
        K::BadEscape => "Bad escaped character",
        K::BadUnicodeEscape => "Bad Unicode escape",
        K::UnterminatedComment | K::TooDeep => e.kind.message(),
    };
    let at = crate::jstr::unit_len(&src[..e.pos.min(src.len())]);
    i.make_error("SyntaxError", format!("{what} in JSON at position {at}"))
}

/// A decoded string in canonical UTF-16 spelling: an escaped lone half next to a (literal or
/// escaped) partner recombines.
fn js_text(s: json::Str<'_>) -> Cow<'_, str> {
    match s.text {
        Cow::Owned(o) if s.lone_surrogate => Cow::Owned(crate::jstr::canonicalize(&o).unwrap_or(o)),
        text => text,
    }
}

fn js_string(s: json::Str<'_>) -> Value {
    match js_text(s) {
        Cow::Borrowed(b) => Value::str(b),
        Cow::Owned(o) => Value::from_string(o),
    }
}

fn js_constant(c: json::Constant) -> Value {
    Value::Num(match c {
        json::Constant::NaN => f64::NAN,
        json::Constant::Infinity => f64::INFINITY,
        json::Constant::NegInfinity => f64::NEG_INFINITY,
    })
}

/// JSON.parse without a reviver. Object keys that repeat within one parse share one key
/// allocation.
struct JsSink<'i, 'a> {
    i: &'i mut Interp,
    src: &'a str,
    keys: crate::fasthash::FastMap<&'a str, Rc<str>>,
}

impl<'i, 'a> JsSink<'i, 'a> {
    fn new(i: &'i mut Interp, src: &'a str) -> Self {
        JsSink {
            i,
            src,
            keys: Default::default(),
        }
    }
}

impl<'a> json::Sink<'a> for JsSink<'_, 'a> {
    type Value = Value;
    type Key = Rc<str>;
    type Object = Gc;
    type Array = Vec<Value>;
    type Error = Value;

    fn error(&mut self, e: json::Error) -> Value {
        parse_error(self.i, self.src, e)
    }
    fn enter(&mut self, _array: bool) -> Result<(), Value> {
        ab(self.i.check_native_stack())
    }
    fn null(&mut self) -> Result<Value, Value> {
        Ok(Value::Null)
    }
    fn bool(&mut self, b: bool) -> Result<Value, Value> {
        Ok(Value::Bool(b))
    }
    fn number(&mut self, n: json::Number<'a>) -> Result<Value, Value> {
        Ok(Value::Num(n.to_f64()))
    }
    fn string(&mut self, s: json::Str<'a>) -> Result<Value, Value> {
        Ok(js_string(s))
    }
    fn constant(&mut self, c: json::Constant) -> Result<Value, Value> {
        Ok(js_constant(c))
    }
    fn object(&mut self) -> Result<Gc, Value> {
        Ok(self.i.new_object())
    }
    #[inline]
    fn key(&mut self, s: json::Str<'a>) -> Result<Rc<str>, Value> {
        Ok(match js_text(s) {
            Cow::Borrowed(raw) => {
                if let Some(k) = self.keys.get(raw) {
                    return Ok(k.clone());
                }
                let k: Rc<str> = Rc::from(raw);
                self.keys.insert(raw, k.clone());
                k
            }
            Cow::Owned(o) => Rc::from(o),
        })
    }
    fn member(&mut self, obj: &mut Gc, key: Rc<str>, v: Value) -> Result<(), Value> {
        obj.borrow_mut().props.insert(key, Property::plain(v));
        Ok(())
    }
    fn end_object(&mut self, obj: Gc) -> Result<Value, Value> {
        Ok(Value::Obj(obj))
    }
    fn array(&mut self) -> Result<Vec<Value>, Value> {
        Ok(Vec::new())
    }
    fn element(&mut self, arr: &mut Vec<Value>, v: Value) -> Result<(), Value> {
        arr.push(v);
        Ok(())
    }
    fn end_array(&mut self, arr: Vec<Value>) -> Result<Value, Value> {
        Ok(self.i.make_array(arr))
    }
}

/// JSON.parse with a reviver: each value paired with its [`JsonRecord`].
struct RecordSink<'i, 'a> {
    i: &'i mut Interp,
    src: &'a str,
}

impl RecordSink<'_, '_> {
    fn prim(v: Value, src: &str) -> Result<(Value, JsonRecord), Value> {
        Ok((v.clone(), JsonRecord::Prim(src.to_string(), v)))
    }
}

impl<'a> json::Sink<'a> for RecordSink<'_, 'a> {
    type Value = (Value, JsonRecord);
    type Key = String;
    type Object = (Gc, Vec<(String, JsonRecord)>);
    type Array = (Vec<Value>, Vec<JsonRecord>);
    type Error = Value;

    fn error(&mut self, e: json::Error) -> Value {
        parse_error(self.i, self.src, e)
    }
    fn enter(&mut self, _array: bool) -> Result<(), Value> {
        ab(self.i.check_native_stack())
    }
    fn null(&mut self) -> Result<Self::Value, Value> {
        Self::prim(Value::Null, "null")
    }
    fn bool(&mut self, b: bool) -> Result<Self::Value, Value> {
        Self::prim(Value::Bool(b), if b { "true" } else { "false" })
    }
    fn number(&mut self, n: json::Number<'a>) -> Result<Self::Value, Value> {
        Self::prim(Value::Num(n.to_f64()), n.text)
    }
    fn string(&mut self, s: json::Str<'a>) -> Result<Self::Value, Value> {
        let raw = &self.src[s.start..s.end];
        Self::prim(js_string(s), raw)
    }
    fn constant(&mut self, c: json::Constant) -> Result<Self::Value, Value> {
        Self::prim(js_constant(c), c.text())
    }
    fn object(&mut self) -> Result<Self::Object, Value> {
        Ok((self.i.new_object(), Vec::new()))
    }
    fn key(&mut self, s: json::Str<'a>) -> Result<String, Value> {
        Ok(js_text(s).into_owned())
    }
    fn member(&mut self, obj: &mut Self::Object, key: String, v: Self::Value) -> Result<(), Value> {
        set_data(&obj.0, &key, v.0);
        obj.1.retain(|(k, _)| k != &key);
        obj.1.push((key, v.1));
        Ok(())
    }
    fn end_object(&mut self, obj: Self::Object) -> Result<Self::Value, Value> {
        Ok((Value::Obj(obj.0), JsonRecord::Obj(obj.1)))
    }
    fn array(&mut self) -> Result<Self::Array, Value> {
        Ok((Vec::new(), Vec::new()))
    }
    fn element(&mut self, arr: &mut Self::Array, v: Self::Value) -> Result<(), Value> {
        arr.0.push(v.0);
        arr.1.push(v.1);
        Ok(())
    }
    fn end_array(&mut self, arr: Self::Array) -> Result<Self::Value, Value> {
        Ok((self.i.make_array(arr.0), JsonRecord::Arr(arr.1)))
    }
}
