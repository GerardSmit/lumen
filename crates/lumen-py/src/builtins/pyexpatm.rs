//! `pyexpat` on the shared XML parser in `lumen_common::xml`: the `xmlparser` object, its handler
//! attributes, `ExpatError` and the `errors` and `model` submodules (`Modules/pyexpat.c`). The
//! parser reports itself as Expat 2.5.0.

/// Python wrapper for Expat parser.
#[lumen_bind::module(name = "pyexpat")]
pub mod pyexpat {
    #![allow(clippy::new_ret_no_self)]

    use crate::bind::{type_object, Py, This};
    use crate::object::*;
    use crate::vm::{dict_set_str, Interp};
    use lumen_common::xml::{self as xml, err, model, Attribute, DefaultMode, Flow, Handler, Model, Options, Parser, Position, Shared};
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    const H_START_ELEMENT: usize = 0;
    const H_END_ELEMENT: usize = 1;
    const H_PROCESSING_INSTRUCTION: usize = 2;
    const H_CHARACTER_DATA: usize = 3;
    const H_UNPARSED_ENTITY_DECL: usize = 4;
    const H_NOTATION_DECL: usize = 5;
    const H_START_NAMESPACE_DECL: usize = 6;
    const H_END_NAMESPACE_DECL: usize = 7;
    const H_COMMENT: usize = 8;
    const H_START_CDATA_SECTION: usize = 9;
    const H_END_CDATA_SECTION: usize = 10;
    const H_DEFAULT: usize = 11;
    const H_DEFAULT_EXPAND: usize = 12;
    const H_NOT_STANDALONE: usize = 13;
    const H_EXTERNAL_ENTITY_REF: usize = 14;
    const H_START_DOCTYPE_DECL: usize = 15;
    const H_END_DOCTYPE_DECL: usize = 16;
    const H_ENTITY_DECL: usize = 17;
    const H_XML_DECL: usize = 18;
    const H_ELEMENT_DECL: usize = 19;
    const H_ATTLIST_DECL: usize = 20;
    const H_SKIPPED_ENTITY: usize = 21;
    const HANDLER_COUNT: usize = 22;

    const CHARACTER_DATA_BUFFER_SIZE: i64 = 8192;
    const READ_SIZE: usize = 2048;
    const EXPAT_VERSION: (i64, i64, i64) = (2, 5, 0);
    const MAX_ERROR_CODE: u32 = 43;

    #[derive(Default)]
    pub struct State {
        error: Option<Obj>,
    }

    /// Everything a parse and its callbacks touch. Callbacks run Python code that may reach the
    /// parser object again, so the state lives behind an `Rc` of cells rather than in the object's
    /// own borrow.
    struct Inner {
        parser: RefCell<Option<Parser>>,
        shared: Rc<Shared>,
        handlers: RefCell<Vec<Value>>,
        /// 0: none, 1: `DefaultHandler`, 2: `DefaultHandlerExpand`; the last one set wins.
        default_kind: Cell<u8>,
        buffer: RefCell<Option<String>>,
        buffer_size: Cell<i64>,
        ordered: Cell<bool>,
        specified: Cell<bool>,
        ns_prefixes: Cell<bool>,
        exc: RefCell<Option<Obj>>,
        intern: Option<Obj>,
        /// Keeps the parent of an external entity parser alive.
        _parent: Option<Value>,
    }

    impl Inner {
        fn new(parser: Parser, intern: Option<Obj>, parent: Option<Value>) -> Inner {
            Inner {
                shared: parser.shared(),
                parser: RefCell::new(Some(parser)),
                handlers: RefCell::new(vec![Value::None; HANDLER_COUNT]),
                default_kind: Cell::new(0),
                buffer: RefCell::new(None),
                buffer_size: Cell::new(CHARACTER_DATA_BUFFER_SIZE),
                ordered: Cell::new(false),
                specified: Cell::new(false),
                ns_prefixes: Cell::new(false),
                exc: RefCell::new(None),
                intern,
                _parent: parent,
            }
        }

        fn handler_value(&self, i: usize) -> Value {
            self.handlers.borrow()[i].clone()
        }

        fn handler(&self, i: usize) -> Option<Value> {
            let v = self.handler_value(i);
            (!v.is_none()).then_some(v)
        }

        fn clear_handlers(&self) {
            for h in self.handlers.borrow_mut().iter_mut() {
                *h = Value::None;
            }
            self.default_kind.set(0);
        }
    }

    fn error_class(it: &mut Interp) -> Obj {
        match it.native_state::<State>().error.clone() {
            Some(c) => c,
            None => it.exc_type("Exception"),
        }
    }

    /// `set_xml_error`: an `ExpatError` carrying `code`, `lineno` and `offset`.
    fn expat_error(it: &mut Interp, code: u32, pos: Position, message: Option<&str>) -> Obj {
        let text = match message {
            Some(m) => m.to_string(),
            None => format!("{}: line {}, column {}", xml::error_string(code).unwrap_or(""), pos.line, pos.column),
        };
        let cls = error_class(it);
        let e = it.new_exc(&cls, vec![Value::string(text)]);
        it.set_exc_attr(&e, "code", Value::Int(code as i64));
        it.set_exc_attr(&e, "lineno", Value::Int(pos.line as i64));
        it.set_exc_attr(&e, "offset", Value::Int(pos.column as i64));
        e
    }

    /// Strings from the parser end at a NUL, like Expat's C strings (an empty namespace separator
    /// is a NUL).
    fn conv(s: &str) -> Value {
        match s.find('\0') {
            Some(n) => Value::str(&s[..n]),
            None => Value::str(s),
        }
    }

    fn conv_opt(s: Option<&str>) -> Value {
        s.map_or(Value::None, conv)
    }

    /// `string_intern`.
    fn intern(it: &mut Interp, inner: &Inner, s: &str) -> R<Value> {
        let v = conv(s);
        let Some(d) = &inner.intern else { return Ok(v) };
        if let Some(found) = it.dict_get(d, &v)? {
            return Ok(found);
        }
        it.setitem(&Value::Obj(d.clone()), v.clone(), v.clone())?;
        Ok(v)
    }

    fn intern_opt(it: &mut Interp, inner: &Inner, s: Option<&str>) -> R<Value> {
        match s {
            Some(s) => intern(it, inner, s),
            None => Ok(Value::None),
        }
    }

    /// `call_character_handler`: an error clears the handlers.
    fn call_character(it: &mut Interp, inner: &Inner, data: String) -> R<()> {
        let Some(h) = inner.handler(H_CHARACTER_DATA) else { return Ok(()) };
        match it.call(&h, vec![Value::string(data)], Vec::new()) {
            Ok(_) => Ok(()),
            Err(e) => {
                inner.clear_handlers();
                Err(e)
            }
        }
    }

    /// `flush_character_buffer`.
    fn flush_buffer(it: &mut Interp, inner: &Inner) -> R<()> {
        let data = match inner.buffer.borrow_mut().as_mut() {
            Some(s) if !s.is_empty() => std::mem::take(s),
            _ => return Ok(()),
        };
        call_character(it, inner, data)
    }

    /// The handler side of a parse: turns each event into the call of the Python handler.
    struct Bridge<'a> {
        it: &'a mut Interp,
        inner: &'a Inner,
    }

    impl<'a> Bridge<'a> {
        /// `flag_error`: remember the exception, drop the handlers and stop the parse.
        fn fail(&mut self, e: Obj) -> Flow {
            *self.inner.exc.borrow_mut() = Some(e);
            self.inner.clear_handlers();
            Flow::Abort
        }

        fn intern(&mut self, s: &str) -> R<Value> {
            intern(self.it, self.inner, s)
        }

        fn intern_opt(&mut self, s: Option<&str>) -> R<Value> {
            intern_opt(self.it, self.inner, s)
        }

        /// Flushes the text buffer, then calls handler `idx` with the arguments `build` makes.
        /// `None`: no handler is set.
        fn call<F>(&mut self, idx: usize, build: F) -> Result<Option<Value>, Flow>
        where
            F: FnOnce(&mut Bridge<'a>) -> R<Vec<Value>>,
        {
            let Some(h) = self.inner.handler(idx) else { return Ok(None) };
            if let Err(e) = flush_buffer(self.it, self.inner) {
                return Err(self.fail(e));
            }
            let args = match build(self) {
                Ok(a) => a,
                Err(e) => return Err(self.fail(e)),
            };
            match self.it.call(&h, args, Vec::new()) {
                Ok(v) => Ok(Some(v)),
                Err(e) => Err(self.fail(e)),
            }
        }

        fn fire<F>(&mut self, idx: usize, build: F) -> Flow
        where
            F: FnOnce(&mut Bridge<'a>) -> R<Vec<Value>>,
        {
            match self.call(idx, build) {
                Ok(None) => Flow::Unset,
                Ok(Some(_)) => Flow::Continue,
                Err(f) => f,
            }
        }

        /// Handlers whose result is a C `int`: zero means "not ok".
        fn fire_int<F>(&mut self, idx: usize, build: F) -> Flow
        where
            F: FnOnce(&mut Bridge<'a>) -> R<Vec<Value>>,
        {
            match self.call(idx, build) {
                Ok(None) => Flow::Unset,
                Ok(Some(Value::Int(0))) | Ok(Some(Value::Bool(false))) => Flow::Fail,
                Ok(Some(Value::Int(_))) | Ok(Some(Value::Bool(true))) => Flow::Continue,
                Ok(Some(v)) => {
                    let t = self.it.type_name_of(&v);
                    let e = self.it.type_error(&format!("'{t}' object cannot be interpreted as an integer"));
                    self.fail(e)
                }
                Err(f) => f,
            }
        }

        fn text(&mut self, idx: usize, s: &str) -> Flow {
            self.fire(idx, |_| Ok(vec![Value::str(s)]))
        }

        /// `my_CharacterDataHandler`.
        fn character_data(&mut self, data: &str) -> Flow {
            if self.inner.handler(H_CHARACTER_DATA).is_none() {
                return Flow::Unset;
            }
            let size = self.inner.buffer_size.get() as usize;
            let buffered = self.inner.buffer.borrow().is_some();
            if !buffered {
                return self.send_character(data.to_string());
            }
            let used = self.inner.buffer.borrow().as_ref().map_or(0, String::len);
            if used + data.len() > size {
                if let Err(e) = flush_buffer(self.it, self.inner) {
                    return self.fail(e);
                }
                if self.inner.handler(H_CHARACTER_DATA).is_none() {
                    return Flow::Continue;
                }
            }
            if data.len() > size {
                if let Some(b) = self.inner.buffer.borrow_mut().as_mut() {
                    b.clear();
                }
                return self.send_character(data.to_string());
            }
            if let Some(b) = self.inner.buffer.borrow_mut().as_mut() {
                b.push_str(data);
            }
            Flow::Continue
        }

        fn send_character(&mut self, data: String) -> Flow {
            match call_character(self.it, self.inner, data) {
                Ok(()) => Flow::Continue,
                Err(e) => self.fail(e),
            }
        }
    }

    /// `conv_content_model`, bottom-up over the flat layout (a node's children come after it).
    fn model_value(m: &Model) -> Value {
        let mut vals: Vec<Value> = vec![Value::None; m.nodes.len()];
        for i in (0..m.nodes.len()).rev() {
            let n = &m.nodes[i];
            let children = vals[n.first..n.first + n.count].to_vec();
            vals[i] = Value::tuple(vec![Value::Int(n.kind as i64), Value::Int(n.quant as i64), conv_opt(n.name.as_deref()), Value::tuple(children)]);
        }
        vals.into_iter().next().unwrap_or(Value::None)
    }

    impl Handler for Bridge<'_> {
        fn xml_decl(&mut self, version: Option<&str>, encoding: Option<&str>, standalone: i32) -> Flow {
            self.fire(H_XML_DECL, |_| Ok(vec![conv_opt(version), conv_opt(encoding), Value::Int(standalone as i64)]))
        }

        fn start_doctype(&mut self, name: &str, sysid: Option<&str>, pubid: Option<&str>, internal: bool) -> Flow {
            self.fire(H_START_DOCTYPE_DECL, |b| Ok(vec![b.intern(name)?, b.intern_opt(sysid)?, b.intern_opt(pubid)?, Value::Int(internal as i64)]))
        }

        fn end_doctype(&mut self) -> Flow {
            self.fire(H_END_DOCTYPE_DECL, |_| Ok(Vec::new()))
        }

        fn element_decl(&mut self, name: &str, m: &Model) -> Flow {
            self.fire(H_ELEMENT_DECL, |b| Ok(vec![b.intern(name)?, model_value(m)]))
        }

        fn attlist_decl(&mut self, element: &str, attribute: &str, atype: &str, default: Option<&str>, required: bool) -> Flow {
            self.fire(H_ATTLIST_DECL, |b| Ok(vec![b.intern(element)?, b.intern(attribute)?, conv(atype), conv_opt(default), Value::Int(required as i64)]))
        }

        #[allow(clippy::too_many_arguments)]
        fn entity_decl(
            &mut self,
            name: &str,
            is_param: bool,
            value: Option<&str>,
            base: Option<&str>,
            sysid: Option<&str>,
            pubid: Option<&str>,
            notation: Option<&str>,
        ) -> Flow {
            self.fire(H_ENTITY_DECL, |b| {
                Ok(vec![
                    b.intern(name)?,
                    Value::Int(is_param as i64),
                    conv_opt(value),
                    b.intern_opt(base)?,
                    b.intern_opt(sysid)?,
                    b.intern_opt(pubid)?,
                    b.intern_opt(notation)?,
                ])
            })
        }

        fn unparsed_entity_decl(&mut self, name: &str, base: Option<&str>, sysid: Option<&str>, pubid: Option<&str>, notation: &str) -> Flow {
            self.fire(H_UNPARSED_ENTITY_DECL, |b| Ok(vec![b.intern(name)?, b.intern_opt(base)?, b.intern_opt(sysid)?, b.intern_opt(pubid)?, b.intern(notation)?]))
        }

        fn notation_decl(&mut self, name: &str, base: Option<&str>, sysid: Option<&str>, pubid: Option<&str>) -> Flow {
            self.fire(H_NOTATION_DECL, |b| Ok(vec![b.intern(name)?, b.intern_opt(base)?, b.intern_opt(sysid)?, b.intern_opt(pubid)?]))
        }

        fn start_element(&mut self, name: &str, attrs: &[Attribute], specified: usize) -> Flow {
            self.fire(H_START_ELEMENT, |b| {
                let n = if b.inner.specified.get() { specified.min(attrs.len()) } else { attrs.len() };
                let container = if b.inner.ordered.get() {
                    let mut items = Vec::with_capacity(n * 2);
                    for a in &attrs[..n] {
                        items.push(b.intern(&a.name)?);
                        items.push(conv(&a.value));
                    }
                    Value::list(items)
                } else {
                    let d = Value::Obj(b.it.new_dict());
                    for a in &attrs[..n] {
                        let k = b.intern(&a.name)?;
                        b.it.setitem(&d, k, conv(&a.value))?;
                    }
                    d
                };
                Ok(vec![b.intern(name)?, container])
            })
        }

        fn end_element(&mut self, name: &str) -> Flow {
            self.fire(H_END_ELEMENT, |b| Ok(vec![b.intern(name)?]))
        }

        fn chardata(&mut self, data: &str) -> Flow {
            self.character_data(data)
        }

        fn comment(&mut self, data: &str) -> Flow {
            self.text(H_COMMENT, data)
        }

        fn processing_instruction(&mut self, target: &str, data: &str) -> Flow {
            self.fire(H_PROCESSING_INSTRUCTION, |b| Ok(vec![b.intern(target)?, conv(data)]))
        }

        fn start_cdata(&mut self) -> Flow {
            self.fire(H_START_CDATA_SECTION, |_| Ok(Vec::new()))
        }

        fn end_cdata(&mut self) -> Flow {
            self.fire(H_END_CDATA_SECTION, |_| Ok(Vec::new()))
        }

        fn start_namespace(&mut self, prefix: Option<&str>, uri: Option<&str>) -> Flow {
            self.fire(H_START_NAMESPACE_DECL, |b| Ok(vec![b.intern_opt(prefix)?, b.intern_opt(uri)?]))
        }

        fn end_namespace(&mut self, prefix: Option<&str>) -> Flow {
            self.fire(H_END_NAMESPACE_DECL, |b| Ok(vec![b.intern_opt(prefix)?]))
        }

        fn skipped_entity(&mut self, name: &str, is_param: bool) -> Flow {
            self.fire(H_SKIPPED_ENTITY, |b| Ok(vec![b.intern(name)?, Value::Int(is_param as i64)]))
        }

        fn external_entity_ref(&mut self, context: Option<&str>, base: Option<&str>, sysid: Option<&str>, pubid: Option<&str>) -> Flow {
            self.fire_int(H_EXTERNAL_ENTITY_REF, |b| Ok(vec![conv_opt(context), b.intern_opt(base)?, b.intern_opt(sysid)?, b.intern_opt(pubid)?]))
        }

        fn not_standalone(&mut self) -> Flow {
            self.fire_int(H_NOT_STANDALONE, |_| Ok(Vec::new()))
        }

        /// `PyUnknownEncodingHandler`: a single-byte encoding Python knows, as a 256-entry table.
        fn unknown_encoding(&mut self, name: &str) -> Option<Vec<i32>> {
            if self.inner.exc.borrow().is_some() {
                return None;
            }
            let bytes: Vec<u8> = (0..=255u8).collect();
            let text = match crate::codecs::decode(self.it, &bytes, name, "replace") {
                Ok(t) => t,
                Err(e) => {
                    self.fail(e);
                    return None;
                }
            };
            if text.chars().count() != 256 {
                let e = self.it.value_error("multi-byte encodings are not supported");
                self.fail(e);
                return None;
            }
            Some(text.chars().map(|c| if c == '\u{FFFD}' { -1 } else { c as i32 }).collect())
        }

        fn default_mode(&self) -> DefaultMode {
            match self.inner.default_kind.get() {
                1 => DefaultMode::Raw,
                2 => DefaultMode::Expand,
                _ => DefaultMode::None,
            }
        }

        fn default_text(&mut self, text: &str) -> Flow {
            let idx = if self.inner.default_kind.get() == 2 { H_DEFAULT_EXPAND } else { H_DEFAULT };
            self.text(idx, text)
        }
    }

    /// `XML_Parse` on the parser of `inner`, then `get_parse_result`.
    fn run_parse(it: &mut Interp, inner: &Inner, data: &[u8], fin: bool, utf8: bool) -> R<()> {
        let taken = inner.parser.borrow_mut().take();
        let Some(mut p) = taken else {
            return Err(expat_error(it, err::ABORTED, inner.shared.position(), None));
        };
        if utf8 {
            p.set_encoding("utf-8");
        }
        let result = {
            let mut bridge = Bridge { it: &mut *it, inner };
            p.parse(&mut bridge, data, fin)
        };
        *inner.parser.borrow_mut() = Some(p);
        let raised = inner.exc.borrow_mut().take();
        if let Some(e) = raised {
            return Err(e);
        }
        match result {
            Err(x) => Err(expat_error(it, x.code, inner.shared.error_position(), None)),
            Ok(()) => flush_buffer(it, inner),
        }
    }

    fn set_handler(it: &mut Interp, slf: &This<Py<XmlParser>>, idx: usize, value: &Value) -> R<()> {
        let inner = slf.0.borrow(it)?.inner.clone();
        if idx == H_CHARACTER_DATA {
            flush_buffer(it, &inner)?;
        }
        inner.handlers.borrow_mut()[idx] = value.clone();
        if idx == H_DEFAULT || idx == H_DEFAULT_EXPAND {
            let kind = match (value.is_none(), idx) {
                (true, _) => 0,
                (false, H_DEFAULT) => 1,
                _ => 2,
            };
            inner.default_kind.set(kind);
        }
        Ok(())
    }

    /// Return a new XML parser object.
    #[op(name = "ParserCreate")]
    fn parser_create(
        it: &mut Interp,
        #[kw] encoding: Option<&Value>,
        #[kw] namespace_separator: Option<&Value>,
        #[kw] intern: Option<&Value>,
    ) -> R<Py<XmlParser>> {
        let text_arg = |it: &mut Interp, v: Option<&Value>, name: &str| -> R<Option<String>> {
            match v {
                None => Ok(None),
                Some(v) if v.is_none() => Ok(None),
                Some(v) => match v.as_str() {
                    Some(s) => Ok(Some(s.to_string())),
                    None => {
                        let t = it.type_name_of(v);
                        Err(it.type_error(&format!("ParserCreate() argument '{name}' must be str or None, not {t}")))
                    }
                },
            }
        };
        let encoding = text_arg(it, encoding, "encoding")?;
        let separator = text_arg(it, namespace_separator, "namespace_separator")?;
        let namespace_separator = match separator {
            None => None,
            Some(s) if s.len() > 1 => {
                return Err(it.value_error("namespace_separator must be at most one character, omitted, or None"));
            }
            Some(s) => Some(s.chars().next().unwrap_or('\0')),
        };
        let intern = match intern {
            None => Some(it.new_dict()),
            Some(v) if v.is_none() => None,
            Some(v) => match dict_of(v) {
                Some(_) => v.as_obj().cloned(),
                None => return Err(it.type_error("intern must be a dictionary")),
            },
        };
        let parser = Parser::new(Options { namespace_separator, namespace_prefixes: false, encoding });
        Ok(Py::new(it, XmlParser { inner: Rc::new(Inner::new(parser, intern, None)) }))
    }

    /// Returns string error for given number.
    #[op(name = "ErrorString")]
    fn error_string(code: i64) -> Value {
        let text = u32::try_from(code).ok().filter(|c| *c <= MAX_ERROR_CODE).and_then(xml::error_string);
        text.map_or(Value::None, Value::str)
    }

    /// XML parser
    #[class(name = "xmlparser", module = "pyexpat", hint(py(final)))]
    pub struct XmlParser {
        inner: Rc<Inner>,
    }

    fn truthy_setter(it: &mut Interp, value: &Value, cell: &Cell<bool>) -> R<bool> {
        let b = it.truthy(value)?;
        cell.set(b);
        Ok(b)
    }

    fn threshold_error(it: &mut Interp, inner: &Inner, message: &str) -> Obj {
        expat_error(it, err::INVALID_ARGUMENT, inner.shared.position(), Some(message))
    }

    #[methods]
    impl XmlParser {
        /// Parse XML data.
        ///
        /// `isfinal' should be true at end of input.
        #[method(name = "Parse")]
        fn parse(slf: This<Py<Self>>, it: &mut Interp, data: &Value, #[default(false)] isfinal: bool) -> R<i64> {
            let inner = slf.0.borrow(it)?.inner.clone();
            match data.as_str() {
                Some(s) => run_parse(it, &inner, s.as_bytes(), isfinal, true)?,
                None => {
                    let bytes = it.buffer_bytes(data)?;
                    run_parse(it, &inner, &bytes, isfinal, false)?;
                }
            }
            Ok(1)
        }

        /// Parse XML data from file-like object.
        #[method(name = "ParseFile")]
        fn parse_file(slf: This<Py<Self>>, it: &mut Interp, file: &Value) -> R<i64> {
            let inner = slf.0.borrow(it)?.inner.clone();
            let read = match it.get_attr_str(file, "read") {
                Ok(r) => r,
                Err(e) if it.exc_is(&e, "AttributeError") => return Err(it.type_error("argument must have 'read' attribute")),
                Err(e) => return Err(e),
            };
            loop {
                let chunk = it.call(&read, vec![Value::Int(READ_SIZE as i64)], Vec::new())?;
                let is_bytes = matches!(&chunk, Value::Obj(o) if matches!(o.kind, Kind::Bytes(_) | Kind::ByteArray(_)));
                if !is_bytes {
                    let t = it.type_name_of(&chunk);
                    return Err(it.type_error(&format!("read() did not return a bytes object (type={t})")));
                }
                let data = it.bytes_of(&chunk)?;
                if data.len() > READ_SIZE {
                    return Err(it.value_error(&format!("read() returned too much data: {READ_SIZE} bytes requested, {} returned", data.len())));
                }
                run_parse(it, &inner, &data, data.is_empty(), false)?;
                if data.is_empty() {
                    return Ok(1);
                }
            }
        }

        /// Set the base URL for the parser.
        #[method(name = "SetBase")]
        fn set_base(&self, base: &str) {
            self.inner.shared.set_base(Some(base.to_string()));
        }

        /// Return base URL string for the parser.
        #[method(name = "GetBase")]
        fn get_base(&self) -> Value {
            conv_opt(self.inner.shared.base().as_deref())
        }

        /// Return the untranslated text of the input that caused the current event.
        ///
        /// If the event was generated by a large amount of text (such as a start tag
        /// for an element with many attributes), not all of the text may be available.
        #[method(name = "GetInputContext")]
        fn get_input_context(&self) -> Value {
            Value::None
        }

        /// Create a parser for parsing an external entity based on the information passed to the ExternalEntityRefHandler.
        #[method(name = "ExternalEntityParserCreate")]
        fn external_entity_parser_create(slf: This<Py<Self>>, it: &mut Interp, context: &Value, encoding: Option<&str>) -> R<Py<XmlParser>> {
            let context = match context {
                Value::None => None,
                v => match v.as_str() {
                    Some(s) => Some(s.to_string()),
                    None => {
                        let t = it.type_name_of(v);
                        return Err(it.type_error(&format!("ExternalEntityParserCreate() argument 1 must be str or None, not {t}")));
                    }
                },
            };
            let parent = slf.0.borrow(it)?.inner.clone();
            let parser = Parser::new_external(&parent.shared, context.as_deref(), encoding);
            let child = Inner::new(parser, parent.intern.clone(), Some(slf.0.value().clone()));
            child.buffer_size.set(parent.buffer_size.get());
            child.ordered.set(parent.ordered.get());
            child.specified.set(parent.specified.get());
            child.ns_prefixes.set(parent.ns_prefixes.get());
            child.default_kind.set(parent.default_kind.get());
            if parent.buffer.borrow().is_some() {
                *child.buffer.borrow_mut() = Some(String::new());
            }
            *child.handlers.borrow_mut() = parent.handlers.borrow().clone();
            Ok(Py::new(it, XmlParser { inner: Rc::new(child) }))
        }

        /// Controls parsing of parameter entities (including the external DTD subset).
        ///
        /// Possible flag values are XML_PARAM_ENTITY_PARSING_NEVER,
        /// XML_PARAM_ENTITY_PARSING_UNLESS_STANDALONE and
        /// XML_PARAM_ENTITY_PARSING_ALWAYS. Returns true if setting the flag
        /// was successful.
        #[method(name = "SetParamEntityParsing")]
        fn set_param_entity_parsing(&self, flag: i64) -> i64 {
            let mode = flag.clamp(0, 2) as u8;
            self.inner.shared.set_param_entity_parsing(mode) as i64
        }

        /// Allows the application to provide an artificial external subset if one is not specified as part of the document instance.
        ///
        /// This readily allows the use of a 'default' document type controlled by the
        /// application, while still getting the advantage of providing document type
        /// information to the parser. 'flag' defaults to True if not provided.
        #[method(name = "UseForeignDTD")]
        fn use_foreign_dtd(&self, it: &mut Interp, #[default(true)] flag: bool) -> R<()> {
            if self.inner.shared.use_foreign_dtd(flag) {
                Ok(())
            } else {
                Err(expat_error(it, err::CANT_CHANGE_FEATURE_ONCE_PARSING, self.inner.shared.position(), None))
            }
        }

        /// Sets the number of output bytes needed to activate protection against billion laughs attacks.
        ///
        /// The number of output bytes includes amplification from entity expansion
        /// and reading DTD files.
        ///
        /// Parser objects usually have a protection activation threshold of 8 MiB,
        /// but the actual default value depends on the underlying Expat library.
        ///
        /// Activation thresholds below 4 MiB are known to break support for DITA 1.3
        /// payload and are hence not recommended.
        #[method(name = "SetBillionLaughsAttackProtectionActivationThreshold")]
        fn set_billion_laughs_threshold(&self, it: &mut Interp, threshold: u64) -> R<()> {
            if !self.inner.shared.is_root() {
                return Err(threshold_error(it, &self.inner, "parser must be a root parser"));
            }
            self.inner.shared.set_billion_laughs_threshold(threshold);
            Ok(())
        }

        /// Sets the maximum tolerated amplification factor for protection against billion laughs attacks.
        ///
        /// The amplification factor is calculated as "(direct + indirect) / direct"
        /// while parsing, where "direct" is the number of bytes read from the primary
        /// document in parsing and "indirect" is the number of bytes added by expanding
        /// entities and reading external DTD files, combined.
        ///
        /// The 'max_factor' value must be a non-NaN floating point value greater than
        /// or equal to 1.0. Amplification factors greater than 30,000 can be observed
        /// in the middle of parsing even with benign files in practice. In particular,
        /// the activation threshold should be carefully chosen to avoid false positives.
        ///
        /// Parser objects usually have a maximum amplification factor of 100,
        /// but the actual default value depends on the underlying Expat library.
        #[method(name = "SetBillionLaughsAttackProtectionMaximumAmplification")]
        fn set_billion_laughs_amplification(&self, it: &mut Interp, max_factor: f64) -> R<()> {
            let factor = max_factor as f32;
            if factor.is_nan() || factor < 1.0 {
                return Err(threshold_error(it, &self.inner, "'max_factor' must be at least 1.0"));
            }
            if !self.inner.shared.is_root() {
                return Err(threshold_error(it, &self.inner, "parser must be a root parser"));
            }
            self.inner.shared.set_billion_laughs_max_amplification(factor);
            Ok(())
        }

        /// Enable/Disable reparse deferral; enabled by default with Expat >=2.6.0.
        #[method(name = "SetReparseDeferralEnabled")]
        fn set_reparse_deferral_enabled(&self, _enabled: bool) {}

        /// Retrieve reparse deferral enabled status; always returns false with Expat <2.6.0.
        #[method(name = "GetReparseDeferralEnabled")]
        fn get_reparse_deferral_enabled(&self) -> bool {
            false
        }

        #[getter(name = "intern")]
        fn get_intern(&self) -> Value {
            self.inner.intern.clone().map_or(Value::None, Value::Obj)
        }

        #[getter(name = "ErrorCode")]
        fn get_error_code(&self) -> i64 {
            self.inner.shared.error_code() as i64
        }

        #[getter(name = "ErrorLineNumber")]
        fn get_error_line_number(&self) -> i64 {
            self.error_position().line as i64
        }

        #[getter(name = "ErrorColumnNumber")]
        fn get_error_column_number(&self) -> i64 {
            self.error_position().column as i64
        }

        #[getter(name = "ErrorByteIndex")]
        fn get_error_byte_index(&self) -> i64 {
            self.error_position().byte as i64
        }

        #[getter(name = "CurrentLineNumber")]
        fn get_current_line_number(&self) -> i64 {
            self.inner.shared.position().line as i64
        }

        #[getter(name = "CurrentColumnNumber")]
        fn get_current_column_number(&self) -> i64 {
            self.inner.shared.position().column as i64
        }

        #[getter(name = "CurrentByteIndex")]
        fn get_current_byte_index(&self) -> i64 {
            self.inner.shared.position().byte as i64
        }

        #[getter(name = "buffer_size")]
        fn get_buffer_size(&self) -> i64 {
            self.inner.buffer_size.get()
        }

        #[setter(name = "buffer_size")]
        fn set_buffer_size(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
            let inner = slf.0.borrow(it)?.inner.clone();
            let Some(big) = value.as_bigint() else {
                return Err(it.type_error("buffer_size must be an integer"));
            };
            if big.is_negative() || big.to_i64() == Some(0) {
                return Err(it.value_error("buffer_size must be greater than zero"));
            }
            let max = i32::MAX as i64;
            let size = match big.to_i64() {
                Some(n) if n <= max => n,
                _ => return Err(it.value_error(&format!("buffer_size must not be greater than {max}"))),
            };
            if size == inner.buffer_size.get() {
                return Ok(());
            }
            flush_buffer(it, &inner)?;
            *inner.buffer.borrow_mut() = Some(String::new());
            inner.buffer_size.set(size);
            Ok(())
        }

        #[getter(name = "buffer_text")]
        fn get_buffer_text(&self) -> bool {
            self.inner.buffer.borrow().is_some()
        }

        #[setter(name = "buffer_text")]
        fn set_buffer_text(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
            let inner = slf.0.borrow(it)?.inner.clone();
            if it.truthy(value)? {
                let mut b = inner.buffer.borrow_mut();
                if b.is_none() {
                    *b = Some(String::new());
                }
            } else if inner.buffer.borrow().is_some() {
                flush_buffer(it, &inner)?;
                *inner.buffer.borrow_mut() = None;
            }
            Ok(())
        }

        #[getter(name = "buffer_used")]
        fn get_buffer_used(&self) -> i64 {
            self.inner.buffer.borrow().as_ref().map_or(0, String::len) as i64
        }

        #[getter(name = "namespace_prefixes")]
        fn get_namespace_prefixes(&self) -> bool {
            self.inner.ns_prefixes.get()
        }

        #[setter(name = "namespace_prefixes")]
        fn set_namespace_prefixes(&self, it: &mut Interp, value: &Value) -> R<()> {
            let on = truthy_setter(it, value, &self.inner.ns_prefixes)?;
            self.inner.shared.set_namespace_prefixes(on);
            Ok(())
        }

        #[getter(name = "ordered_attributes")]
        fn get_ordered_attributes(&self) -> bool {
            self.inner.ordered.get()
        }

        #[setter(name = "ordered_attributes")]
        fn set_ordered_attributes(&self, it: &mut Interp, value: &Value) -> R<()> {
            truthy_setter(it, value, &self.inner.ordered).map(|_| ())
        }

        #[getter(name = "specified_attributes")]
        fn get_specified_attributes(&self) -> bool {
            self.inner.specified.get()
        }

        #[setter(name = "specified_attributes")]
        fn set_specified_attributes(&self, it: &mut Interp, value: &Value) -> R<()> {
            truthy_setter(it, value, &self.inner.specified).map(|_| ())
        }

        #[getter(name = "StartElementHandler")]
        fn get_start_element_handler(&self) -> Value {
            self.inner.handler_value(0)
        }

        #[setter(name = "StartElementHandler")]
        fn set_start_element_handler(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
            set_handler(it, &slf, 0, value)
        }

        #[getter(name = "EndElementHandler")]
        fn get_end_element_handler(&self) -> Value {
            self.inner.handler_value(1)
        }

        #[setter(name = "EndElementHandler")]
        fn set_end_element_handler(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
            set_handler(it, &slf, 1, value)
        }

        #[getter(name = "ProcessingInstructionHandler")]
        fn get_processing_instruction_handler(&self) -> Value {
            self.inner.handler_value(2)
        }

        #[setter(name = "ProcessingInstructionHandler")]
        fn set_processing_instruction_handler(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
            set_handler(it, &slf, 2, value)
        }

        #[getter(name = "CharacterDataHandler")]
        fn get_character_data_handler(&self) -> Value {
            self.inner.handler_value(3)
        }

        #[setter(name = "CharacterDataHandler")]
        fn set_character_data_handler(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
            set_handler(it, &slf, 3, value)
        }

        #[getter(name = "UnparsedEntityDeclHandler")]
        fn get_unparsed_entity_decl_handler(&self) -> Value {
            self.inner.handler_value(4)
        }

        #[setter(name = "UnparsedEntityDeclHandler")]
        fn set_unparsed_entity_decl_handler(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
            set_handler(it, &slf, 4, value)
        }

        #[getter(name = "NotationDeclHandler")]
        fn get_notation_decl_handler(&self) -> Value {
            self.inner.handler_value(5)
        }

        #[setter(name = "NotationDeclHandler")]
        fn set_notation_decl_handler(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
            set_handler(it, &slf, 5, value)
        }

        #[getter(name = "StartNamespaceDeclHandler")]
        fn get_start_namespace_decl_handler(&self) -> Value {
            self.inner.handler_value(6)
        }

        #[setter(name = "StartNamespaceDeclHandler")]
        fn set_start_namespace_decl_handler(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
            set_handler(it, &slf, 6, value)
        }

        #[getter(name = "EndNamespaceDeclHandler")]
        fn get_end_namespace_decl_handler(&self) -> Value {
            self.inner.handler_value(7)
        }

        #[setter(name = "EndNamespaceDeclHandler")]
        fn set_end_namespace_decl_handler(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
            set_handler(it, &slf, 7, value)
        }

        #[getter(name = "CommentHandler")]
        fn get_comment_handler(&self) -> Value {
            self.inner.handler_value(8)
        }

        #[setter(name = "CommentHandler")]
        fn set_comment_handler(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
            set_handler(it, &slf, 8, value)
        }

        #[getter(name = "StartCdataSectionHandler")]
        fn get_start_cdata_section_handler(&self) -> Value {
            self.inner.handler_value(9)
        }

        #[setter(name = "StartCdataSectionHandler")]
        fn set_start_cdata_section_handler(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
            set_handler(it, &slf, 9, value)
        }

        #[getter(name = "EndCdataSectionHandler")]
        fn get_end_cdata_section_handler(&self) -> Value {
            self.inner.handler_value(10)
        }

        #[setter(name = "EndCdataSectionHandler")]
        fn set_end_cdata_section_handler(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
            set_handler(it, &slf, 10, value)
        }

        #[getter(name = "DefaultHandler")]
        fn get_default_handler(&self) -> Value {
            self.inner.handler_value(11)
        }

        #[setter(name = "DefaultHandler")]
        fn set_default_handler(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
            set_handler(it, &slf, 11, value)
        }

        #[getter(name = "DefaultHandlerExpand")]
        fn get_default_handler_expand(&self) -> Value {
            self.inner.handler_value(12)
        }

        #[setter(name = "DefaultHandlerExpand")]
        fn set_default_handler_expand(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
            set_handler(it, &slf, 12, value)
        }

        #[getter(name = "NotStandaloneHandler")]
        fn get_not_standalone_handler(&self) -> Value {
            self.inner.handler_value(13)
        }

        #[setter(name = "NotStandaloneHandler")]
        fn set_not_standalone_handler(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
            set_handler(it, &slf, 13, value)
        }

        #[getter(name = "ExternalEntityRefHandler")]
        fn get_external_entity_ref_handler(&self) -> Value {
            self.inner.handler_value(14)
        }

        #[setter(name = "ExternalEntityRefHandler")]
        fn set_external_entity_ref_handler(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
            set_handler(it, &slf, 14, value)
        }

        #[getter(name = "StartDoctypeDeclHandler")]
        fn get_start_doctype_decl_handler(&self) -> Value {
            self.inner.handler_value(15)
        }

        #[setter(name = "StartDoctypeDeclHandler")]
        fn set_start_doctype_decl_handler(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
            set_handler(it, &slf, 15, value)
        }

        #[getter(name = "EndDoctypeDeclHandler")]
        fn get_end_doctype_decl_handler(&self) -> Value {
            self.inner.handler_value(16)
        }

        #[setter(name = "EndDoctypeDeclHandler")]
        fn set_end_doctype_decl_handler(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
            set_handler(it, &slf, 16, value)
        }

        #[getter(name = "EntityDeclHandler")]
        fn get_entity_decl_handler(&self) -> Value {
            self.inner.handler_value(17)
        }

        #[setter(name = "EntityDeclHandler")]
        fn set_entity_decl_handler(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
            set_handler(it, &slf, 17, value)
        }

        #[getter(name = "XmlDeclHandler")]
        fn get_xml_decl_handler(&self) -> Value {
            self.inner.handler_value(18)
        }

        #[setter(name = "XmlDeclHandler")]
        fn set_xml_decl_handler(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
            set_handler(it, &slf, 18, value)
        }

        #[getter(name = "ElementDeclHandler")]
        fn get_element_decl_handler(&self) -> Value {
            self.inner.handler_value(19)
        }

        #[setter(name = "ElementDeclHandler")]
        fn set_element_decl_handler(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
            set_handler(it, &slf, 19, value)
        }

        #[getter(name = "AttlistDeclHandler")]
        fn get_attlist_decl_handler(&self) -> Value {
            self.inner.handler_value(20)
        }

        #[setter(name = "AttlistDeclHandler")]
        fn set_attlist_decl_handler(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
            set_handler(it, &slf, 20, value)
        }

        #[getter(name = "SkippedEntityHandler")]
        fn get_skipped_entity_handler(&self) -> Value {
            self.inner.handler_value(21)
        }

        #[setter(name = "SkippedEntityHandler")]
        fn set_skipped_entity_handler(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
            set_handler(it, &slf, 21, value)
        }
    }

    impl XmlParser {
        /// `XML_GetErrorLineNumber` and friends: the error position, or the current one before any
        /// error.
        fn error_position(&self) -> Position {
            if self.inner.shared.error_code() == 0 {
                self.inner.shared.position()
            } else {
                self.inner.shared.error_position()
            }
        }
    }

    fn add_errors_module(it: &mut Interp, d: &Obj) {
        let module = it.new_module("pyexpat.errors");
        let md = it.module_dict(&module);
        let codes = it.new_dict();
        let messages = it.new_dict();
        dict_set_str(&md, "__doc__", Value::str("Constants used to describe error conditions."));
        for code in 1..=MAX_ERROR_CODE {
            let (name, text) = xml::ERRORS[code as usize];
            dict_set_str(&md, name, Value::str(text));
            let _ = it.setitem(&Value::Obj(codes.clone()), Value::str(text), Value::Int(code as i64));
            let _ = it.setitem(&Value::Obj(messages.clone()), Value::Int(code as i64), Value::str(text));
        }
        dict_set_str(&md, "codes", Value::Obj(codes));
        dict_set_str(&md, "messages", Value::Obj(messages));
        dict_set_str(d, "errors", Value::Obj(module));
    }

    fn add_model_module(it: &mut Interp, d: &Obj) {
        let module = it.new_module("pyexpat.model");
        let md = it.module_dict(&module);
        dict_set_str(&md, "__doc__", Value::str("Constants used to interpret content model information."));
        let consts: [(&str, u8); 10] = [
            ("XML_CTYPE_EMPTY", model::EMPTY),
            ("XML_CTYPE_ANY", model::ANY),
            ("XML_CTYPE_MIXED", model::MIXED),
            ("XML_CTYPE_NAME", model::NAME),
            ("XML_CTYPE_CHOICE", model::CHOICE),
            ("XML_CTYPE_SEQ", model::SEQ),
            ("XML_CQUANT_NONE", model::QUANT_NONE),
            ("XML_CQUANT_OPT", model::QUANT_OPT),
            ("XML_CQUANT_REP", model::QUANT_REP),
            ("XML_CQUANT_PLUS", model::QUANT_PLUS),
        ];
        for (name, v) in consts {
            dict_set_str(&md, name, Value::Int(v as i64));
        }
        dict_set_str(d, "model", Value::Obj(module));
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        let exc = it.exc_type("Exception");
        let error = crate::builtins::native::new_type(it, "xml.parsers.expat", "ExpatError", Some(&exc), Layout::Exception);
        dict_set_str(&d, "error", Value::Obj(error.clone()));
        dict_set_str(&d, "ExpatError", Value::Obj(error.clone()));
        it.native_state::<State>().error = Some(error);
        let parser_type = type_object::<XmlParser>(it);
        dict_set_str(&d, "XMLParserType", Value::Obj(parser_type));
        let (major, minor, micro) = EXPAT_VERSION;
        dict_set_str(&d, "EXPAT_VERSION", Value::string(format!("expat_{major}.{minor}.{micro}")));
        dict_set_str(&d, "version_info", Value::tuple(vec![Value::Int(major), Value::Int(minor), Value::Int(micro)]));
        dict_set_str(&d, "native_encoding", Value::str("UTF-8"));
        add_errors_module(it, &d);
        add_model_module(it, &d);
        let features = [("sizeof(XML_Char)", 1), ("sizeof(XML_LChar)", 1), ("XML_DTD", 0), ("XML_CONTEXT_BYTES", 1024), ("XML_NS", 0)];
        let list = features.iter().map(|(n, v)| Value::tuple(vec![Value::str(n), Value::Int(*v)])).collect();
        dict_set_str(&d, "features", Value::list(list));
        let ints = [("XML_PARAM_ENTITY_PARSING_NEVER", 0), ("XML_PARAM_ENTITY_PARSING_UNLESS_STANDALONE", 1), ("XML_PARAM_ENTITY_PARSING_ALWAYS", 2)];
        for (name, v) in ints {
            dict_set_str(&d, name, Value::Int(v));
        }
    }
}
