//! `_json` on `lumen_common::json`: the scanner and string quoting are shared; this adapter
//! builds Python objects, calls the hooks and runs the encoder.

/// json speedups
#[lumen_bind::module(name = "_json")]
pub mod _json {
    #![allow(clippy::new_ret_no_self)]
    use crate::bind::{opaque_instance, type_object, Py, This};
    use crate::dict::PyDict;
    use crate::object::*;
    use crate::pyint::{BigInt, PyInt};
    use crate::vm::{dict_set_str, Interp};
    use lumen_common::json::{self as core, Quote, Spelling};
    use lumen_common::smuggle::count_code_points;
    use std::borrow::Cow;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    /// CPython 3.12's C recursion limit, which bounds nesting in both directions.
    const DEPTH_LIMIT: usize = 10_000;

    const ASCII: Quote = Quote { ascii_only: true, spelling: Spelling::CodePoints, ..Quote::JSON };
    const UNICODE: Quote = Quote { spelling: Spelling::CodePoints, ..Quote::JSON };

    fn options(strict: bool) -> core::Options {
        core::Options { strict, constants: true, jsonc: false, spelling: Spelling::CodePoints }
    }

    fn not_a_string(it: &mut Interp, v: &Value) -> Obj {
        let t = it.tp_name_of(v);
        it.type_error(&format!("first argument must be a string, not {t}"))
    }

    fn too_deep(it: &mut Interp, what: &str) -> Obj {
        it.new_exc_str("RecursionError", &format!("maximum recursion depth exceeded{what}"))
    }

    fn deeper(it: &mut Interp, depth: &mut usize, what: &str) -> R<()> {
        *depth += 1;
        if *depth > DEPTH_LIMIT || lumen_common::stack::exhausted() {
            return Err(too_deep(it, what));
        }
        Ok(())
    }

    /// A text being decoded, with a known byte/character position pair to count from.
    struct Doc<'a> {
        value: &'a Value,
        s: &'a PyStr,
        base: (usize, usize),
    }

    impl Doc<'_> {
        fn char_pos(&self, byte: usize) -> usize {
            if self.s.ascii {
                byte
            } else if byte >= self.base.0 {
                self.base.1 + count_code_points(&self.s.s[self.base.0..byte])
            } else {
                count_code_points(&self.s.s[..byte])
            }
        }

        /// `StopIteration(pos)` for a missing value (the decoder reports "Expecting value"),
        /// otherwise `json.decoder.JSONDecodeError(msg, doc, pos)`.
        fn error(&self, it: &mut Interp, e: core::Error) -> Obj {
            let pos = self.char_pos(e.pos) as i64;
            if e.kind == core::ErrorKind::ExpectingValue {
                return it.new_exc_val("StopIteration", Value::Int(pos));
            }
            let made = it.import_module("json.decoder").and_then(|m| {
                let cls = it.get_attr_str(&Value::Obj(m), "JSONDecodeError")?;
                it.call(&cls, vec![Value::str(e.kind.message()), self.value.clone(), Value::Int(pos)], Vec::new())
            });
            match made {
                Ok(Value::Obj(exc)) => exc,
                Ok(other) => {
                    let t = it.tp_name_of(&other);
                    it.type_error(&format!("exceptions must derive from BaseException, not {t}"))
                }
                Err(e) => e,
            }
        }
    }

    fn text_value(text: Cow<'_, str>) -> Value {
        match text {
            Cow::Borrowed(s) => Value::str(s),
            Cow::Owned(s) => Value::string(s),
        }
    }

    /// scanstring(string, end, strict=True) -> (string, end)
    ///
    /// Scan the string s for a JSON string. End is the index of the
    /// character in s after the quote that started the JSON string.
    /// Unescapes all valid JSON string escape sequences and raises ValueError
    /// on attempt to decode an invalid string. If strict is False then literal
    /// control characters are allowed in the string.
    ///
    /// Returns a tuple of the decoded string and the index of the character in s
    /// after the end quote.
    #[op]
    fn scanstring(it: &mut Interp, string: &Value, end: &Value, strict: Option<&Value>) -> R<Value> {
        let end = it.index_of(end)?;
        let strict = match strict {
            Some(v) => it.truthy(v)?,
            None => true,
        };
        let Some(s) = string.as_pystr() else {
            return Err(not_a_string(it, string));
        };
        if end < 0 || end as usize > s.nchars {
            return Err(it.value_error("end is out of bounds"));
        }
        let start = s.byte_offset(end as usize);
        let doc = Doc { value: string, s, base: (start, end as usize) };
        let mut p = core::Parser::new(&s.s, options(strict));
        p.pos = start;
        match p.string_body() {
            Ok(text) => {
                let next = doc.char_pos(p.pos) as i64;
                Ok(Value::tuple(vec![text_value(text.text), Value::Int(next)]))
            }
            Err(e) => Err(doc.error(it, e)),
        }
    }

    fn encode_string(s: &PyStr, q: &Quote) -> Value {
        let mut out = String::with_capacity(s.s.len() + 2);
        core::quote_into(&mut out, &s.s, q);
        Value::string(out)
    }

    /// encode_basestring_ascii(string) -> string
    ///
    /// Return an ASCII-only JSON representation of a Python string
    #[op]
    fn encode_basestring_ascii(it: &mut Interp, s: &Value) -> R<Value> {
        match s.as_pystr() {
            Some(p) => Ok(encode_string(p, &ASCII)),
            None => Err(not_a_string(it, s)),
        }
    }

    /// encode_basestring(string) -> string
    ///
    /// Return a JSON representation of a Python string
    #[op]
    fn encode_basestring(it: &mut Interp, s: &Value) -> R<Value> {
        match s.as_pystr() {
            Some(p) => Ok(encode_string(p, &UNICODE)),
            None => Err(not_a_string(it, s)),
        }
    }

    fn is_type(v: &Value, t: &Obj) -> bool {
        matches!(v, Value::Obj(o) if Rc::ptr_eq(o, t))
    }

    /// JSON scanner object
    #[class(name = "Scanner", module = "_json", skip(py))]
    #[derive(Clone)]
    pub struct Scanner {
        strict: bool,
        object_hook: Value,
        object_pairs_hook: Value,
        parse_float: Value,
        parse_int: Value,
        parse_constant: Value,
    }

    #[methods]
    impl Scanner {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[kw] context: &Value) -> R<Value> {
            let strict = it.get_attr_str(context, "strict")?;
            let strict = it.truthy(&strict)?;
            let object_hook = it.get_attr_str(context, "object_hook")?;
            let object_pairs_hook = it.get_attr_str(context, "object_pairs_hook")?;
            let parse_float = it.get_attr_str(context, "parse_float")?;
            let parse_int = it.get_attr_str(context, "parse_int")?;
            let parse_constant = it.get_attr_str(context, "parse_constant")?;
            let Value::Obj(cls) = &cls.0 else { unreachable!() };
            Ok(opaque_instance(cls, Scanner { strict, object_hook, object_pairs_hook, parse_float, parse_int, parse_constant }))
        }

        #[proto(call)]
        fn __call__(slf: This<Py<Self>>, it: &mut Interp, #[kw] string: &Value, #[kw] idx: &Value) -> R<Value> {
            let cfg = slf.0.borrow(it)?.clone();
            let idx = it.index_of(idx)?;
            let Some(s) = string.as_pystr() else {
                return Err(not_a_string(it, string));
            };
            scan_once(it, &cfg, string, s, idx)
        }

        #[getter]
        fn strict(&self) -> bool {
            self.strict
        }

        #[getter]
        fn object_hook(&self) -> Value {
            self.object_hook.clone()
        }

        #[getter]
        fn object_pairs_hook(&self) -> Value {
            self.object_pairs_hook.clone()
        }

        #[getter]
        fn parse_float(&self) -> Value {
            self.parse_float.clone()
        }

        #[getter]
        fn parse_int(&self) -> Value {
            self.parse_int.clone()
        }

        #[getter]
        fn parse_constant(&self) -> Value {
            self.parse_constant.clone()
        }
    }

    fn scan_once(it: &mut Interp, cfg: &Scanner, string: &Value, s: &PyStr, idx: i64) -> R<Value> {
        if idx < 0 {
            return Err(it.value_error("idx cannot be negative"));
        }
        if idx as usize >= s.nchars {
            return Err(it.new_exc_val("StopIteration", Value::Int(idx)));
        }
        let start = s.byte_offset(idx as usize);
        let mut p = core::Parser::new(&s.s, options(cfg.strict));
        p.pos = start;
        let mut sink = PySink {
            float: (!is_type(&cfg.parse_float, &it.types.float)).then(|| cfg.parse_float.clone()),
            int: (!is_type(&cfg.parse_int, &it.types.int)).then(|| cfg.parse_int.clone()),
            it,
            cfg,
            doc: Doc { value: string, s, base: (start, idx as usize) },
            memo: HashMap::new(),
            depth: 0,
        };
        let v = p.value(&mut sink)?;
        let end = sink.doc.char_pos(p.pos) as i64;
        Ok(Value::tuple(vec![v, Value::Int(end)]))
    }

    enum Members {
        Dict(Obj),
        Pairs(Vec<Value>),
    }

    struct PySink<'s, 'a> {
        it: &'s mut Interp,
        cfg: &'s Scanner,
        doc: Doc<'a>,
        /// `parse_float` / `parse_int` when they are not `float` / `int` themselves.
        float: Option<Value>,
        int: Option<Value>,
        /// Equal keys share one string object within a call.
        memo: HashMap<Cow<'a, str>, Value>,
        depth: usize,
    }

    impl<'a> core::Sink<'a> for PySink<'_, 'a> {
        type Value = Value;
        type Key = Value;
        type Object = Members;
        type Array = Vec<Value>;
        type Error = Obj;

        fn error(&mut self, e: core::Error) -> Obj {
            self.doc.error(self.it, e)
        }

        fn enter(&mut self, array: bool) -> R<()> {
            let what = if array {
                " while decoding a JSON array from a unicode string"
            } else {
                " while decoding a JSON object from a unicode string"
            };
            deeper(self.it, &mut self.depth, what)
        }

        fn leave(&mut self) {
            self.depth -= 1;
        }

        fn null(&mut self) -> R<Value> {
            Ok(Value::None)
        }

        fn bool(&mut self, b: bool) -> R<Value> {
            Ok(Value::Bool(b))
        }

        fn number(&mut self, n: core::Number<'a>) -> R<Value> {
            let custom = if n.is_float { &self.float } else { &self.int };
            if let Some(f) = custom {
                let f = f.clone();
                return self.it.call(&f, vec![Value::str(n.text)], Vec::new());
            }
            if n.is_float {
                return Ok(Value::Float(n.to_f64()));
            }
            if let Some(i) = n.small_int() {
                return Ok(Value::Int(i));
            }
            self.it.check_parse_digits(n.text.trim_start_matches('-').len())?;
            Ok(BigInt::parse_signed(n.text, 10).map(Value::big).unwrap_or(Value::Int(0)))
        }

        fn string(&mut self, s: core::Str<'a>) -> R<Value> {
            Ok(text_value(s.text))
        }

        fn constant(&mut self, c: core::Constant) -> R<Value> {
            let f = self.cfg.parse_constant.clone();
            self.it.call(&f, vec![Value::str(c.text())], Vec::new())
        }

        fn object(&mut self) -> R<Members> {
            Ok(if self.cfg.object_pairs_hook.is_none() {
                Members::Dict(Object::new(Kind::Dict(RefCell::new(PyDict::new()))))
            } else {
                Members::Pairs(Vec::new())
            })
        }

        fn key(&mut self, s: core::Str<'a>) -> R<Value> {
            if let Some(k) = self.memo.get(&*s.text) {
                return Ok(k.clone());
            }
            let k = Value::str(&s.text);
            self.memo.insert(s.text, k.clone());
            Ok(k)
        }

        fn member(&mut self, obj: &mut Members, key: Value, v: Value) -> R<()> {
            match obj {
                Members::Dict(d) => self.it.dict_set(d, key, v),
                Members::Pairs(p) => {
                    p.push(Value::tuple(vec![key, v]));
                    Ok(())
                }
            }
        }

        fn end_object(&mut self, obj: Members) -> R<Value> {
            match obj {
                Members::Pairs(p) => {
                    let hook = self.cfg.object_pairs_hook.clone();
                    self.it.call(&hook, vec![Value::list(p)], Vec::new())
                }
                Members::Dict(d) if self.cfg.object_hook.is_none() => Ok(Value::Obj(d)),
                Members::Dict(d) => {
                    let hook = self.cfg.object_hook.clone();
                    self.it.call(&hook, vec![Value::Obj(d)], Vec::new())
                }
            }
        }

        fn array(&mut self) -> R<Vec<Value>> {
            Ok(Vec::new())
        }

        fn element(&mut self, arr: &mut Vec<Value>, v: Value) -> R<()> {
            arr.push(v);
            Ok(())
        }

        fn end_array(&mut self, arr: Vec<Value>) -> R<Value> {
            Ok(Value::list(arr))
        }
    }

    /// _iterencode(obj, _current_indent_level) -> iterable
    #[class(name = "Encoder", module = "_json", skip(py))]
    #[derive(Clone)]
    pub struct Encoder {
        markers: Value,
        default: Value,
        encoder: Value,
        indent: Value,
        key_separator: Value,
        item_separator: Value,
        sort_keys: bool,
        skipkeys: bool,
        allow_nan: bool,
        /// The quoting of `encoder` when it is one of this module's own functions.
        fast: Option<Quote>,
    }

    /// The quoting `f` does, when it is `encode_basestring(_ascii)` itself.
    fn own_quoting(f: &Value) -> Option<Quote> {
        let Value::Obj(o) = f else { return None };
        let Kind::Native(nd) = &o.kind else { return None };
        let d = nd.desc?;
        if !matches!(d.owner, lumen_bind::Owner::Module("_json")) {
            return None;
        }
        match d.name {
            "encode_basestring_ascii" => Some(ASCII),
            "encode_basestring" => Some(UNICODE),
            _ => None,
        }
    }

    fn separator(it: &mut Interp, n: usize, v: &Value) -> R<Value> {
        if v.as_pystr().is_some() {
            return Ok(v.clone());
        }
        let t = it.tp_name_of(v);
        Err(it.type_error(&format!("make_encoder() argument {n} must be str, not {t}")))
    }

    #[methods]
    impl Encoder {
        #[constructor]
        #[allow(clippy::too_many_arguments)]
        fn new(
            cls: This<Value>,
            it: &mut Interp,
            #[kw] markers: &Value,
            #[kw] default: &Value,
            #[kw] encoder: &Value,
            #[kw] indent: &Value,
            #[kw] key_separator: &Value,
            #[kw] item_separator: &Value,
            #[kw] sort_keys: &Value,
            #[kw] skipkeys: &Value,
            #[kw] allow_nan: &Value,
        ) -> R<Value> {
            let key_separator = separator(it, 5, key_separator)?;
            let item_separator = separator(it, 6, item_separator)?;
            let sort_keys = it.truthy(sort_keys)?;
            let skipkeys = it.truthy(skipkeys)?;
            let allow_nan = it.truthy(allow_nan)?;
            let is_dict = matches!(markers, Value::Obj(o) if matches!(o.kind, Kind::Dict(_)));
            if !markers.is_none() && !is_dict {
                let t = it.tp_name_of(markers);
                return Err(it.type_error(&format!("make_encoder() argument 1 must be dict or None, not {t}")));
            }
            let enc = Encoder {
                markers: markers.clone(),
                default: default.clone(),
                encoder: encoder.clone(),
                indent: indent.clone(),
                key_separator,
                item_separator,
                sort_keys,
                skipkeys,
                allow_nan,
                fast: own_quoting(encoder),
            };
            let Value::Obj(cls) = &cls.0 else { unreachable!() };
            Ok(opaque_instance(cls, enc))
        }

        #[proto(call)]
        fn __call__(slf: This<Py<Self>>, it: &mut Interp, #[kw] obj: &Value, #[kw] _current_indent_level: &Value) -> R<Value> {
            let cfg = slf.0.borrow(it)?.clone();
            it.index_of(_current_indent_level)?;
            let mut w = Writer {
                key_sep: cfg.key_separator.as_str().unwrap_or_default().to_string(),
                item_sep: cfg.item_separator.as_str().unwrap_or_default().to_string(),
                cfg: &cfg,
                out: String::new(),
                depth: 0,
            };
            w.obj(it, obj)?;
            Ok(Value::tuple(vec![Value::string(w.out)]))
        }

        #[getter]
        fn markers(&self) -> Value {
            self.markers.clone()
        }

        #[getter]
        fn default(&self) -> Value {
            self.default.clone()
        }

        #[getter]
        fn encoder(&self) -> Value {
            self.encoder.clone()
        }

        #[getter]
        fn indent(&self) -> Value {
            self.indent.clone()
        }

        #[getter]
        fn key_separator(&self) -> Value {
            self.key_separator.clone()
        }

        #[getter]
        fn item_separator(&self) -> Value {
            self.item_separator.clone()
        }

        #[getter]
        fn sort_keys(&self) -> bool {
            self.sort_keys
        }

        #[getter]
        fn skipkeys(&self) -> bool {
            self.skipkeys
        }
    }

    const ENCODING: &str = " while encoding a JSON object";

    struct Writer<'e> {
        cfg: &'e Encoder,
        key_sep: String,
        item_sep: String,
        out: String,
        depth: usize,
    }

    impl Writer<'_> {
        fn obj(&mut self, it: &mut Interp, v: &Value) -> R<()> {
            match v {
                Value::None => self.out.push_str("null"),
                Value::Bool(true) => self.out.push_str("true"),
                Value::Bool(false) => self.out.push_str("false"),
                Value::Int(i) => self.out.push_str(&i.to_string()),
                Value::Float(f) => self.float(it, *f, v)?,
                Value::Obj(o) => match &o.kind {
                    Kind::Str(s) => self.string(it, v, s)?,
                    Kind::Int(b) => {
                        let text = it.int_to_decimal(b)?;
                        self.out.push_str(&text);
                    }
                    Kind::Float(f) => self.float(it, *f, v)?,
                    Kind::List(_) | Kind::Tuple(_) => {
                        deeper(it, &mut self.depth, ENCODING)?;
                        let r = self.list(it, v, o);
                        self.depth -= 1;
                        r?;
                    }
                    Kind::Dict(_) => {
                        deeper(it, &mut self.depth, ENCODING)?;
                        let r = self.dict(it, v, o);
                        self.depth -= 1;
                        r?;
                    }
                    _ => self.other(it, v)?,
                },
                _ => self.other(it, v)?,
            }
            Ok(())
        }

        /// An object JSON has no form for: through `default`, watching for cycles.
        fn other(&mut self, it: &mut Interp, v: &Value) -> R<()> {
            let mark = self.mark(it, v)?;
            let default = self.cfg.default.clone();
            let new = it.call(&default, vec![v.clone()], Vec::new())?;
            deeper(it, &mut self.depth, ENCODING)?;
            let r = self.obj(it, &new);
            self.depth -= 1;
            r?;
            self.unmark(it, mark)
        }

        fn float_text(&self, it: &mut Interp, f: f64, v: &Value) -> R<Cow<'static, str>> {
            if f.is_finite() {
                return Ok(Cow::Owned(crate::num::float_repr(f)));
            }
            if !self.cfg.allow_nan {
                let r = it.repr_of(v)?;
                return Err(it.value_error(&format!("Out of range float values are not JSON compliant: {r}")));
            }
            Ok(Cow::Borrowed(if f > 0.0 {
                "Infinity"
            } else if f < 0.0 {
                "-Infinity"
            } else {
                "NaN"
            }))
        }

        fn float(&mut self, it: &mut Interp, f: f64, v: &Value) -> R<()> {
            let text = self.float_text(it, f, v)?;
            self.out.push_str(&text);
            Ok(())
        }

        fn string(&mut self, it: &mut Interp, v: &Value, s: &PyStr) -> R<()> {
            if let Some(q) = &self.cfg.fast {
                core::quote_into(&mut self.out, &s.s, q);
                return Ok(());
            }
            let encoder = self.cfg.encoder.clone();
            let r = it.call(&encoder, vec![v.clone()], Vec::new())?;
            match r.as_pystr() {
                Some(t) => {
                    self.out.push_str(&t.s);
                    Ok(())
                }
                None => {
                    let t = it.tp_name_of(&r);
                    Err(it.type_error(&format!("encoder() must return a string, not {t}")))
                }
            }
        }

        fn mark(&mut self, it: &mut Interp, v: &Value) -> R<Option<Value>> {
            if self.cfg.markers.is_none() {
                return Ok(None);
            }
            let id = Value::Int(it.id_of(v) as i64);
            if it.contains(&self.cfg.markers, &id)? {
                return Err(it.value_error("Circular reference detected"));
            }
            it.setitem(&self.cfg.markers, id.clone(), v.clone())?;
            Ok(Some(id))
        }

        fn unmark(&mut self, it: &mut Interp, mark: Option<Value>) -> R<()> {
            match mark {
                Some(id) => it.delitem(&self.cfg.markers, &id),
                None => Ok(()),
            }
        }

        fn list(&mut self, it: &mut Interp, v: &Value, o: &Obj) -> R<()> {
            // A list is read live, as CPython does: `default` may shrink it mid-way.
            let item = |i: usize| match &o.kind {
                Kind::List(l) => l.borrow().get(i).cloned(),
                Kind::Tuple(t) => t.get(i).cloned(),
                _ => None,
            };
            if item(0).is_none() {
                self.out.push_str("[]");
                return Ok(());
            }
            let mark = self.mark(it, v)?;
            self.out.push('[');
            let mut i = 0;
            while let Some(x) = item(i) {
                if i > 0 {
                    self.out.push_str(&self.item_sep);
                }
                self.obj(it, &x)?;
                i += 1;
            }
            self.unmark(it, mark)?;
            self.out.push(']');
            Ok(())
        }

        fn dict(&mut self, it: &mut Interp, v: &Value, o: &Obj) -> R<()> {
            let Kind::Dict(d) = &o.kind else { unreachable!() };
            if d.borrow().is_empty() {
                self.out.push_str("{}");
                return Ok(());
            }
            let mark = self.mark(it, v)?;
            self.out.push('{');
            let mut first = true;
            if self.cfg.sort_keys || o.cls.is_some() {
                let items = it.call_method(v, "items", Vec::new())?;
                let items = Value::list(it.iterate_to_vec(&items)?);
                if self.cfg.sort_keys {
                    it.call_method(&items, "sort", Vec::new())?;
                }
                let items = items.as_obj().map(|l| match &l.kind {
                    Kind::List(l) => l.borrow().clone(),
                    _ => Vec::new(),
                });
                for item in items.unwrap_or_default() {
                    let pair = match &item {
                        Value::Obj(t) => match &t.kind {
                            Kind::Tuple(p) if p.len() == 2 => Some((p[0].clone(), p[1].clone())),
                            _ => None,
                        },
                        _ => None,
                    };
                    let Some((key, value)) = pair else {
                        return Err(it.value_error("items must return 2-tuples"));
                    };
                    self.member(it, &mut first, &key, &value)?;
                }
            } else {
                let entries: Vec<(Value, Value)> = d.borrow().iter().map(|e| (e.key.clone(), e.val.clone())).collect();
                for (key, value) in &entries {
                    self.member(it, &mut first, key, value)?;
                }
            }
            self.unmark(it, mark)?;
            self.out.push('}');
            Ok(())
        }

        fn member(&mut self, it: &mut Interp, first: &mut bool, key: &Value, value: &Value) -> R<()> {
            let key = match key {
                Value::Obj(o) if matches!(o.kind, Kind::Str(_)) => key.clone(),
                Value::Float(f) => Value::str(&self.float_text(it, *f, key)?),
                Value::Obj(o) if matches!(o.kind, Kind::Float(_)) => {
                    let Kind::Float(f) = o.kind else { unreachable!() };
                    Value::str(&self.float_text(it, f, key)?)
                }
                Value::Bool(true) => Value::str("true"),
                Value::Bool(false) => Value::str("false"),
                Value::None => Value::str("null"),
                Value::Int(i) => Value::string(i.to_string()),
                Value::Obj(o) if matches!(o.kind, Kind::Int(_)) => {
                    let Kind::Int(b) = &o.kind else { unreachable!() };
                    Value::string(it.int_to_decimal(b)?)
                }
                _ if self.cfg.skipkeys => return Ok(()),
                _ => {
                    let t = it.tp_name_of(key);
                    return Err(it.type_error(&format!("keys must be str, int, float, bool or None, not {t}")));
                }
            };
            if *first {
                *first = false;
            } else {
                self.out.push_str(&self.item_sep);
            }
            let s = key.as_pystr().expect("a str key");
            self.string(it, &key, s)?;
            self.out.push_str(&self.key_sep);
            self.obj(it, value)
        }
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        let scanner = type_object::<Scanner>(it);
        dict_set_str(&d, "make_scanner", Value::Obj(scanner));
        let encoder = type_object::<Encoder>(it);
        dict_set_str(&d, "make_encoder", Value::Obj(encoder));
    }
}
