//! The type system: classes, MRO, attribute protocol, instantiation.

use crate::dict::PyDict;
use crate::object::*;
use crate::vm::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const EXC_TABLE: &[(&str, &str)] = &[
    ("BaseException", "object"),
    ("SystemExit", "BaseException"),
    ("KeyboardInterrupt", "BaseException"),
    ("GeneratorExit", "BaseException"),
    ("Exception", "BaseException"),
    ("BaseExceptionGroup", "BaseException"),
    ("ExceptionGroup", "Exception"),
    ("StopIteration", "Exception"),
    ("StopAsyncIteration", "Exception"),
    ("ArithmeticError", "Exception"),
    ("FloatingPointError", "ArithmeticError"),
    ("OverflowError", "ArithmeticError"),
    ("ZeroDivisionError", "ArithmeticError"),
    ("AssertionError", "Exception"),
    ("AttributeError", "Exception"),
    ("BufferError", "Exception"),
    ("EOFError", "Exception"),
    ("ImportError", "Exception"),
    ("ModuleNotFoundError", "ImportError"),
    ("LookupError", "Exception"),
    ("IndexError", "LookupError"),
    ("KeyError", "LookupError"),
    ("MemoryError", "Exception"),
    ("NameError", "Exception"),
    ("UnboundLocalError", "NameError"),
    ("OSError", "Exception"),
    ("BlockingIOError", "OSError"),
    ("ChildProcessError", "OSError"),
    ("ConnectionError", "OSError"),
    ("BrokenPipeError", "ConnectionError"),
    ("ConnectionAbortedError", "ConnectionError"),
    ("ConnectionRefusedError", "ConnectionError"),
    ("ConnectionResetError", "ConnectionError"),
    ("FileExistsError", "OSError"),
    ("FileNotFoundError", "OSError"),
    ("InterruptedError", "OSError"),
    ("IsADirectoryError", "OSError"),
    ("NotADirectoryError", "OSError"),
    ("PermissionError", "OSError"),
    ("ProcessLookupError", "OSError"),
    ("TimeoutError", "OSError"),
    ("ReferenceError", "Exception"),
    ("RuntimeError", "Exception"),
    ("NotImplementedError", "RuntimeError"),
    ("RecursionError", "RuntimeError"),
    ("SyntaxError", "Exception"),
    ("IndentationError", "SyntaxError"),
    ("TabError", "IndentationError"),
    ("SystemError", "Exception"),
    ("TypeError", "Exception"),
    ("ValueError", "Exception"),
    ("UnicodeError", "ValueError"),
    ("UnicodeDecodeError", "UnicodeError"),
    ("UnicodeEncodeError", "UnicodeError"),
    ("UnicodeTranslateError", "UnicodeError"),
    ("Warning", "Exception"),
    ("DeprecationWarning", "Warning"),
    ("PendingDeprecationWarning", "Warning"),
    ("RuntimeWarning", "Warning"),
    ("SyntaxWarning", "Warning"),
    ("UserWarning", "Warning"),
    ("FutureWarning", "Warning"),
    ("ImportWarning", "Warning"),
    ("UnicodeWarning", "Warning"),
    ("BytesWarning", "Warning"),
    ("ResourceWarning", "Warning"),
    ("EncodingWarning", "Warning"),
];

pub const HOOK_GETATTRIBUTE: u8 = 1;
pub const HOOK_GETATTR: u8 = 2;
pub const HOOK_SETATTR: u8 = 4;
pub const HOOK_DELATTR: u8 = 8;

impl Interp {
    pub fn bootstrap_types(&mut self) {
        let t = &self.types;
        let obj = t.object.clone();
        let pairs: Vec<(Obj, Obj)> = vec![
            (t.type_.clone(), obj.clone()),
            (t.int.clone(), obj.clone()),
            (t.bool_.clone(), t.int.clone()),
            (t.float.clone(), obj.clone()),
            (t.complex.clone(), obj.clone()),
            (t.str_.clone(), obj.clone()),
            (t.list.clone(), obj.clone()),
            (t.tuple.clone(), obj.clone()),
            (t.dict.clone(), obj.clone()),
            (t.set.clone(), obj.clone()),
            (t.frozenset.clone(), obj.clone()),
            (t.bytes.clone(), obj.clone()),
            (t.bytearray.clone(), obj.clone()),
            (t.none_type.clone(), obj.clone()),
            (t.notimpl_type.clone(), obj.clone()),
            (t.ellipsis_type.clone(), obj.clone()),
            (t.function.clone(), obj.clone()),
            (t.method.clone(), obj.clone()),
            (t.builtin_function.clone(), obj.clone()),
            (t.module.clone(), obj.clone()),
            (t.cell.clone(), obj.clone()),
            (t.code.clone(), obj.clone()),
            (t.generator.clone(), obj.clone()),
            (t.coroutine.clone(), obj.clone()),
            (t.async_generator.clone(), obj.clone()),
            (t.asend.clone(), obj.clone()),
            (t.traceback.clone(), obj.clone()),
            (t.slice.clone(), obj.clone()),
            (t.range.clone(), obj.clone()),
            (t.property.clone(), obj.clone()),
            (t.staticmethod.clone(), obj.clone()),
            (t.classmethod.clone(), obj.clone()),
            (t.super_.clone(), obj.clone()),
            (t.dict_keys.clone(), obj.clone()),
            (t.dict_values.clone(), obj.clone()),
            (t.dict_items.clone(), obj.clone()),
            (t.list_iterator.clone(), obj.clone()),
            (t.list_reverseiterator.clone(), obj.clone()),
            (t.tuple_iterator.clone(), obj.clone()),
            (t.str_iterator.clone(), obj.clone()),
            (t.bytes_iterator.clone(), obj.clone()),
            (t.range_iterator.clone(), obj.clone()),
            (t.dict_keyiterator.clone(), obj.clone()),
            (t.dict_valueiterator.clone(), obj.clone()),
            (t.dict_itemiterator.clone(), obj.clone()),
            (t.set_iterator.clone(), obj.clone()),
            (t.iterator.clone(), obj.clone()),
            (t.callable_iterator.clone(), obj.clone()),
            (t.reversed.clone(), obj.clone()),
            (t.enumerate.clone(), obj.clone()),
            (t.zip.clone(), obj.clone()),
            (t.map.clone(), obj.clone()),
            (t.filter.clone(), obj.clone()),
            (t.frame.clone(), obj.clone()),
        ];
        self.set_bases(&obj, vec![]);
        for (ty, base) in pairs {
            self.set_bases(&ty, vec![base]);
        }
        let layouts: &[(&str, Layout)] = &[("BaseException", Layout::Exception)];
        for (name, base) in EXC_TABLE {
            let b = if *base == "object" { obj.clone() } else { self.exc_types[base].clone() };
            let layout = layouts.iter().find(|(n, _)| n == name).map(|(_, l)| *l).unwrap_or(Layout::Exception);
            let ty = new_type_raw(name, layout);
            self.set_bases(&ty, vec![b]);
            self.exc_types.insert(name, ty);
        }
        let (beg, eg, exc) = (self.exc_types["BaseExceptionGroup"].clone(), self.exc_types["ExceptionGroup"].clone(), self.exc_types["Exception"].clone());
        self.set_bases(&eg, vec![beg, exc]);
        self.add_exception_aliases();
    }

    fn add_exception_aliases(&mut self) {
        let oe = self.exc_types["OSError"].clone();
        self.exc_types.insert("IOError", oe.clone());
        self.exc_types.insert("EnvironmentError", oe);
    }

    pub fn set_bases(&mut self, ty: &Obj, bases: Vec<Obj>) {
        if let Kind::Type(td) = &ty.kind {
            *td.bases.borrow_mut() = bases.clone();
            let mro = self.compute_mro(ty, &bases).unwrap_or_else(|| vec![ty.clone()]);
            *td.mro.borrow_mut() = mro;
        }
    }

    pub fn compute_mro(&self, ty: &Obj, bases: &[Obj]) -> Option<Vec<Obj>> {
        let mut seqs: Vec<Vec<Obj>> = Vec::new();
        for b in bases {
            match &b.kind {
                Kind::Type(td) => seqs.push(td.mro.borrow().clone()),
                _ => return None,
            }
        }
        seqs.push(bases.to_vec());
        let mut result = vec![ty.clone()];
        loop {
            seqs.retain(|s| !s.is_empty());
            if seqs.is_empty() {
                return Some(result);
            }
            let mut cand: Option<Obj> = None;
            for s in &seqs {
                let c = &s[0];
                let in_tail = seqs.iter().any(|t| t[1..].iter().any(|x| Rc::ptr_eq(x, c)));
                if !in_tail {
                    cand = Some(c.clone());
                    break;
                }
            }
            let c = cand?;
            for s in seqs.iter_mut() {
                if Rc::ptr_eq(&s[0], &c) {
                    s.remove(0);
                }
            }
            result.push(c);
        }
    }

    pub fn exc_type(&self, name: &str) -> Obj {
        match self.exc_types.get(name) {
            Some(t) => t.clone(),
            None => self.types.object.clone(),
        }
    }

    pub fn new_exc(&mut self, cls: &Obj, args: Vec<Value>) -> Obj {
        let is_exact_builtin = false;
        let _ = is_exact_builtin;
        Object::with_cls(
            cls.clone(),
            Kind::Exception(RefCell::new(ExcData {
                args: Value::tuple(args),
                cause: None,
                context: None,
                suppress_context: false,
                ctx_set: false,
                tb: Vec::new(),
            })),
        )
    }

    pub fn new_exc_str(&mut self, name: &str, msg: &str) -> Obj {
        let cls = self.exc_type(name);
        let args = if msg.is_empty() && matches!(name, "GeneratorExit" | "StopIteration" | "StopAsyncIteration") { Vec::new() } else { vec![Value::str(msg)] };
        self.new_exc(&cls, args)
    }

    pub fn new_exc_val(&mut self, name: &str, v: Value) -> Obj {
        let cls = self.exc_type(name);
        self.new_exc(&cls, vec![v])
    }

    pub fn runtime_error(&mut self, msg: &str) -> Obj {
        self.new_exc_str("RuntimeError", msg)
    }

    pub fn type_error(&mut self, msg: &str) -> Obj {
        self.new_exc_str("TypeError", msg)
    }

    pub fn value_error(&mut self, msg: &str) -> Obj {
        self.new_exc_str("ValueError", msg)
    }

    pub fn set_exc_attr(&mut self, e: &Obj, name: &str, v: Value) {
        let d = self.instance_dict(e);
        dict_set_str(&d, name, v);
    }

    pub fn instance_dict(&self, o: &Obj) -> Obj {
        let mut slot = o.dict.borrow_mut();
        match &*slot {
            Some(d) => d.clone(),
            None => {
                let d = Object::new(Kind::Dict(RefCell::new(PyDict::new())));
                *slot = Some(d.clone());
                d
            }
        }
    }

    // ---- type queries ----------------------------------------------------------------------

    pub fn type_of(&self, v: &Value) -> Obj {
        match v {
            Value::None => self.types.none_type.clone(),
            Value::Bool(_) => self.types.bool_.clone(),
            Value::Int(_) => self.types.int.clone(),
            Value::Float(_) => self.types.float.clone(),
            Value::NotImplemented => self.types.notimpl_type.clone(),
            Value::Ellipsis => self.types.ellipsis_type.clone(),
            Value::Obj(o) => self.type_of_obj(o),
        }
    }

    pub fn type_of_obj(&self, o: &Obj) -> Obj {
        match &o.cls {
            Some(c) => c.clone(),
            None => self.kind_type(&o.kind),
        }
    }

    pub fn kind_type(&self, k: &Kind) -> Obj {
        let t = &self.types;
        match k {
            Kind::Instance => t.object.clone(),
            Kind::Str(_) => t.str_.clone(),
            Kind::Int(_) => t.int.clone(),
            Kind::Float(_) => t.float.clone(),
            Kind::Complex(..) => t.complex.clone(),
            Kind::Tuple(_) => t.tuple.clone(),
            Kind::List(_) => t.list.clone(),
            Kind::Dict(_) => t.dict.clone(),
            Kind::Set(_) => t.set.clone(),
            Kind::FrozenSet(_) => t.frozenset.clone(),
            Kind::Bytes(_) => t.bytes.clone(),
            Kind::ByteArray(_) => t.bytearray.clone(),
            Kind::Type(_) => t.type_.clone(),
            Kind::Function(_) => t.function.clone(),
            Kind::Method(Value::Obj(f), _) if matches!(f.kind, Kind::Native(_)) => t.builtin_function.clone(),
            Kind::Method(..) => t.method.clone(),
            Kind::Native(_) => t.builtin_function.clone(),
            Kind::Module => t.module.clone(),
            Kind::Cell(_) => t.cell.clone(),
            Kind::Code(_) => t.code.clone(),
            Kind::Generator(g) => match g.kind {
                GenKind::Generator => t.generator.clone(),
                GenKind::Coroutine => t.coroutine.clone(),
                GenKind::AsyncGen => t.async_generator.clone(),
            },
            Kind::Exception(_) => self.exc_type("BaseException"),
            Kind::Slice(..) => t.slice.clone(),
            Kind::Range(_) | Kind::BigRange(_) => t.range.clone(),
            Kind::Iter(s) => match &*s.borrow() {
                IterState::List { .. } => t.list_iterator.clone(),
                IterState::Tuple { .. } => t.tuple_iterator.clone(),
                IterState::Str { .. } => t.str_iterator.clone(),
                IterState::Bytes { .. } => t.bytes_iterator.clone(),
                IterState::Range { .. } => t.range_iterator.clone(),
                IterState::Dict { kind, .. } => match kind {
                    ViewKind::Keys => t.dict_keyiterator.clone(),
                    ViewKind::Values => t.dict_valueiterator.clone(),
                    ViewKind::Items => t.dict_itemiterator.clone(),
                },
                IterState::Set { .. } => t.set_iterator.clone(),
                IterState::Seq { .. } => t.iterator.clone(),
                IterState::CallIter { .. } => t.callable_iterator.clone(),
                IterState::Reversed { seq, .. } => match seq {
                    Value::Obj(so) if matches!(so.kind, Kind::List(_)) && so.cls.is_none() => t.list_reverseiterator.clone(),
                    _ => t.reversed.clone(),
                },
                IterState::Enumerate { .. } => t.enumerate.clone(),
                IterState::Zip { .. } => t.zip.clone(),
                IterState::Map { .. } => t.map.clone(),
                IterState::Filter { .. } => t.filter.clone(),
                IterState::Native(_) | IterState::Running | IterState::Empty => t.iterator.clone(),
            },
            Kind::Property(_) => t.property.clone(),
            Kind::StaticMethod(_) => t.staticmethod.clone(),
            Kind::ClassMethod(_) => t.classmethod.clone(),
            Kind::Super(..) => t.super_.clone(),
            Kind::DictView(_, vk) => match vk {
                ViewKind::Keys => t.dict_keys.clone(),
                ViewKind::Values => t.dict_values.clone(),
                ViewKind::Items => t.dict_items.clone(),
            },
            Kind::Frame => t.frame.clone(),
            Kind::AsyncGenValue(_) | Kind::Opaque(_) => t.object.clone(),
        }
    }

    /// CPython's `tp_name`: the bare name of a class made by a `class` statement, `module.name`
    /// for a native type outside `builtins`.
    pub fn tp_name(&self, t: &Obj) -> String {
        let name = self.type_name(t);
        if self.is_heap(t) {
            return name;
        }
        match self.type_module(t) {
            Some(m) if m != "builtins" => format!("{m}.{name}"),
            _ => name,
        }
    }

    pub fn tp_name_of(&self, v: &Value) -> String {
        let t = self.type_of(v);
        self.tp_name(&t)
    }

    pub fn type_name_of(&self, v: &Value) -> String {
        let t = self.type_of(v);
        self.type_name(&t)
    }

    pub fn type_name(&self, t: &Obj) -> String {
        match &t.kind {
            Kind::Type(td) => td.name.borrow().to_string(),
            _ => "?".into(),
        }
    }

    pub fn type_layout(&self, t: &Obj) -> Layout {
        match &t.kind {
            Kind::Type(td) => td.layout.get(),
            _ => Layout::Object,
        }
    }

    pub fn is_subtype(&self, a: &Obj, b: &Obj) -> bool {
        if Rc::ptr_eq(a, b) {
            return true;
        }
        match &a.kind {
            Kind::Type(td) => td.mro.borrow().iter().any(|c| Rc::ptr_eq(c, b)),
            _ => false,
        }
    }

    pub fn is_exact(&self, v: &Value, t: &Obj) -> bool {
        match v {
            Value::Obj(o) => match &o.cls {
                Some(c) => Rc::ptr_eq(c, t),
                None => Rc::ptr_eq(&self.kind_type(&o.kind), t),
            },
            _ => Rc::ptr_eq(&self.type_of(v), t),
        }
    }

    pub fn lookup_mro(&self, cls: &Obj, name: &str) -> Option<Value> {
        let h = hash_str(name);
        if let Kind::Type(td) = &cls.kind {
            for c in td.mro.borrow().iter() {
                if let Some(d) = c.dict.borrow().as_ref() {
                    if let Kind::Dict(dd) = &d.kind {
                        let dd = dd.borrow();
                        if let Some(i) = dd.find_str(h, name) {
                            return dd.get(i).map(|e| e.val.clone());
                        }
                    }
                }
            }
        }
        None
    }

    pub fn lookup_mro_name(&self, cls: &Obj, name: &Obj) -> Option<Value> {
        let (h, s) = match &name.kind {
            Kind::Str(s) => (s.hash(), &s.s),
            _ => return None,
        };
        if let Kind::Type(td) = &cls.kind {
            for c in td.mro.borrow().iter() {
                if let Some(d) = c.dict.borrow().as_ref() {
                    if let Kind::Dict(dd) = &d.kind {
                        let dd = dd.borrow();
                        if let Some(i) = dd.find_str(h, s) {
                            return dd.get(i).map(|e| e.val.clone());
                        }
                    }
                }
            }
        }
        None
    }

    /// First class in the MRO whose own dict defines `name`, with the value.
    pub fn lookup_mro_with_owner(&self, cls: &Obj, name: &str) -> Option<(Obj, Value)> {
        let h = hash_str(name);
        if let Kind::Type(td) = &cls.kind {
            for c in td.mro.borrow().iter() {
                if let Some(d) = c.dict.borrow().as_ref() {
                    if let Kind::Dict(dd) = &d.kind {
                        let dd = dd.borrow();
                        if let Some(i) = dd.find_str(h, name) {
                            return dd.get(i).map(|e| (c.clone(), e.val.clone()));
                        }
                    }
                }
            }
        }
        None
    }

    fn hooks_of(&self, cls: &Obj) -> u8 {
        let td = match &cls.kind {
            Kind::Type(td) => td,
            _ => return 0,
        };
        let (epoch, bits) = td.hooks.get();
        if epoch == self.type_epoch {
            return bits;
        }
        let mut bits = 0;
        for (name, bit) in [
            ("__getattribute__", HOOK_GETATTRIBUTE),
            ("__getattr__", HOOK_GETATTR),
            ("__setattr__", HOOK_SETATTR),
            ("__delattr__", HOOK_DELATTR),
        ] {
            if let Some((owner, _)) = self.lookup_mro_with_owner(cls, name) {
                if !Rc::ptr_eq(&owner, &self.types.object) {
                    bits |= bit;
                }
            }
        }
        td.hooks.set((self.type_epoch, bits));
        bits
    }

    // ---- descriptors -----------------------------------------------------------------------

    pub fn is_data_descr(&self, v: &Value) -> bool {
        match v {
            Value::Obj(o) => match &o.kind {
                Kind::Property(_) => true,
                Kind::Instance | Kind::Opaque(_) => {
                    let c = self.type_of_obj(o);
                    self.lookup_mro(&c, "__set__").is_some() || self.lookup_mro(&c, "__delete__").is_some()
                }
                _ => false,
            },
            _ => false,
        }
    }

    pub fn bind_descr(&mut self, descr: &Value, obj: &Value, cls: &Obj) -> R<Value> {
        let o = match descr {
            Value::Obj(o) => o,
            _ => return Ok(descr.clone()),
        };
        match &o.kind {
            Kind::Function(_) => Ok(Value::Obj(Object::new(Kind::Method(descr.clone(), obj.clone())))),
            Kind::Native(nd) if nd.method => Ok(Value::Obj(Object::new(Kind::Method(descr.clone(), obj.clone())))),
            Kind::Property(p) => {
                if p.fget.is_none() {
                    return Err(self.new_exc_str("AttributeError", "property has no getter"));
                }
                let g = p.fget.clone();
                self.call(&g, vec![obj.clone()], Vec::new())
            }
            Kind::StaticMethod(f) => Ok(f.clone()),
            Kind::ClassMethod(f) => Ok(Value::Obj(Object::new(Kind::Method(f.clone(), Value::Obj(cls.clone()))))),
            Kind::Instance | Kind::Exception(_) | Kind::Opaque(_) => {
                let c = self.type_of_obj(o);
                if let Some(g) = self.lookup_mro(&c, "__get__") {
                    let gb = self.bind_descr(&g, descr, &c)?;
                    return self.call(&gb, vec![obj.clone(), Value::Obj(cls.clone())], Vec::new());
                }
                Ok(descr.clone())
            }
            _ => Ok(descr.clone()),
        }
    }

    /// Descriptor access through the class itself (`Foo.attr`).
    pub fn bind_descr_cls(&mut self, descr: &Value, cls: &Obj) -> R<Value> {
        let o = match descr {
            Value::Obj(o) => o,
            _ => return Ok(descr.clone()),
        };
        match &o.kind {
            Kind::StaticMethod(f) => Ok(f.clone()),
            Kind::ClassMethod(f) => Ok(Value::Obj(Object::new(Kind::Method(f.clone(), Value::Obj(cls.clone()))))),
            Kind::Instance | Kind::Opaque(_) => {
                let c = self.type_of_obj(o);
                if let Some(g) = self.lookup_mro(&c, "__get__") {
                    let gb = self.bind_descr(&g, descr, &c)?;
                    return self.call(&gb, vec![Value::None, Value::Obj(cls.clone())], Vec::new());
                }
                Ok(descr.clone())
            }
            _ => Ok(descr.clone()),
        }
    }

    // ---- attribute access ------------------------------------------------------------------

    pub fn get_attr_str(&mut self, obj: &Value, name: &str) -> R<Value> {
        let n = match Value::str(name) {
            Value::Obj(o) => o,
            _ => unreachable!(),
        };
        self.get_attr(obj, &n)
    }

    pub fn attr_error(&mut self, obj: &Value, name: &str) -> Obj {
        let msg = match obj {
            Value::Obj(o) => match &o.kind {
                Kind::Type(td) => format!("type object '{}' has no attribute '{}'", td.name.borrow(), name),
                Kind::Module => {
                    let mn = o.dict.borrow().as_ref().and_then(|d| dict_get_str(d, "__name__")).and_then(|v| v.as_str().map(|s| s.to_string())).unwrap_or_default();
                    format!("module '{}' has no attribute '{}'", mn, name)
                }
                _ => format!("'{}' object has no attribute '{}'", self.tp_name_of(obj), name),
            },
            _ => format!("'{}' object has no attribute '{}'", self.tp_name_of(obj), name),
        };
        let e = self.new_exc_str("AttributeError", &msg);
        self.set_exc_attr(&e, "name", Value::str(name));
        self.set_exc_attr(&e, "obj", obj.clone());
        e
    }

    pub fn get_attr(&mut self, obj: &Value, name: &Obj) -> R<Value> {
        let cls = self.type_of(obj);
        let hooks = self.hooks_of(&cls);
        if hooks == 0 {
            return self.generic_getattr(obj, &cls, name);
        }
        if hooks & HOOK_GETATTRIBUTE != 0 {
            let ga = self.lookup_mro(&cls, "__getattribute__").unwrap();
            let b = self.bind_descr(&ga, obj, &cls)?;
            match self.call(&b, vec![Value::Obj(name.clone())], Vec::new()) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if hooks & HOOK_GETATTR != 0 && self.exc_is(&e, "AttributeError") {
                        return self.call_getattr_hook(obj, &cls, name);
                    }
                    return Err(e);
                }
            }
        }
        match self.generic_getattr(obj, &cls, name) {
            Ok(v) => Ok(v),
            Err(e) => {
                if hooks & HOOK_GETATTR != 0 && self.exc_is(&e, "AttributeError") {
                    self.call_getattr_hook(obj, &cls, name)
                } else {
                    Err(e)
                }
            }
        }
    }

    fn call_getattr_hook(&mut self, obj: &Value, cls: &Obj, name: &Obj) -> R<Value> {
        let ga = self.lookup_mro(cls, "__getattr__").unwrap();
        let b = self.bind_descr(&ga, obj, cls)?;
        self.call(&b, vec![Value::Obj(name.clone())], Vec::new())
    }

    pub fn generic_getattr(&mut self, obj: &Value, cls: &Obj, name: &Obj) -> R<Value> {
        let nm = name.as_str_kind().unwrap_or("");
        if let Value::Obj(o) = obj {
            match &o.kind {
                Kind::Type(_) => return self.type_getattr(obj, o, name),
                Kind::Module => {
                    let d = o.dict.borrow().clone();
                    if let Some(v) = d.as_ref().and_then(|d| dict_get_name(d, name)) {
                        return Ok(v);
                    }
                    if let Some(v) = self.special_attr(obj, nm)? {
                        return Ok(v);
                    }
                    if let Some(descr) = self.lookup_mro_name(cls, name) {
                        return self.bind_descr(&descr, obj, cls);
                    }
                    if let Some(ga) = d.as_ref().and_then(|d| dict_get_str(d, "__getattr__")) {
                        return self.call(&ga, vec![Value::Obj(name.clone())], Vec::new());
                    }
                    return Err(self.attr_error(obj, nm));
                }
                Kind::Super(..) => return self.super_getattr(obj, o, name),
                _ => {}
            }
        }
        let descr = self.lookup_mro_name(cls, name);
        if let Some(d) = &descr {
            if self.is_data_descr(d) {
                return self.bind_descr(d, obj, cls);
            }
        }
        if let Value::Obj(o) = obj {
            if let Some(dd) = o.dict.borrow().as_ref() {
                if let Some(v) = dict_get_name(dd, name) {
                    return Ok(v);
                }
            }
        }
        if let Some(v) = self.special_attr(obj, nm)? {
            return Ok(v);
        }
        if let Some(d) = descr {
            return self.bind_descr(&d, obj, cls);
        }
        Err(self.attr_error(obj, nm))
    }

    fn type_getattr(&mut self, obj: &Value, cls: &Obj, name: &Obj) -> R<Value> {
        let nm = name.as_str_kind().unwrap_or("").to_string();
        let meta = self.type_of_obj(cls);
        let mdescr = self.lookup_mro_name(&meta, name);
        if let Some(d) = &mdescr {
            if self.is_data_descr(d) {
                return self.bind_descr(d, obj, &meta);
            }
        }
        if let Some(v) = self.type_special_attr(cls, &nm) {
            return Ok(v);
        }
        if let Some(v) = self.lookup_mro_name(cls, name) {
            return self.bind_descr_cls(&v, cls);
        }
        if let Some(d) = mdescr {
            return self.bind_descr(&d, obj, &meta);
        }
        Err(self.attr_error(obj, &nm))
    }

    pub fn type_special_attr(&mut self, cls: &Obj, nm: &str) -> Option<Value> {
        let td = match &cls.kind {
            Kind::Type(td) => td,
            _ => return None,
        };
        if !nm.starts_with("__") {
            return None;
        }
        Some(match nm {
            "__name__" => Value::str(&td.name.borrow()),
            "__qualname__" => match &*td.qualname.borrow() {
                Some(q) => Value::str(q),
                None => Value::str(&td.name.borrow()),
            },
            "__bases__" => Value::tuple(td.bases.borrow().iter().map(|b| Value::Obj(b.clone())).collect()),
            "__mro__" => Value::tuple(td.mro.borrow().iter().map(|b| Value::Obj(b.clone())).collect()),
            "__base__" => match td.bases.borrow().first() {
                Some(b) => Value::Obj(b.clone()),
                None => Value::None,
            },
            "__class__" => Value::Obj(self.type_of_obj(cls)),
            "__dict__" => {
                let d = cls.dict.borrow().clone().unwrap();
                self.new_mappingproxy(Value::Obj(d))
            }
            "__module__" => {
                if cls.dict.borrow().as_ref().and_then(|d| dict_get_str(d, "__module__")).is_some() {
                    return None;
                }
                Value::str("builtins")
            }
            "__doc__" => {
                if cls.dict.borrow().as_ref().and_then(|d| dict_get_str(d, "__doc__")).is_some() {
                    return None;
                }
                Value::None
            }
            "__subclasses__" => return None,
            "__type_params__" => match cls.dict.borrow().as_ref().and_then(|d| dict_get_str(d, "__type_params__")) {
                Some(v) => v,
                None => Value::tuple(Vec::new()),
            },
            _ => return None,
        })
    }

    fn super_getattr(&mut self, obj: &Value, o: &Obj, name: &Obj) -> R<Value> {
        let (typ, inst, objtype) = match &o.kind {
            Kind::Super(t, i, ot) => (t.clone(), i.clone(), ot.clone()),
            _ => unreachable!(),
        };
        let nm = name.as_str_kind().unwrap_or("").to_string();
        if nm == "__class__" {
            return Ok(Value::Obj(self.types.super_.clone()));
        }
        let start_cls = match &objtype {
            Value::Obj(c) => c.clone(),
            _ => return Err(self.attr_error(obj, &nm)),
        };
        let mro: Vec<Obj> = match &start_cls.kind {
            Kind::Type(td) => td.mro.borrow().clone(),
            _ => return Err(self.attr_error(obj, &nm)),
        };
        let typ_obj = match &typ {
            Value::Obj(t) => t.clone(),
            _ => return Err(self.attr_error(obj, &nm)),
        };
        let pos = mro.iter().position(|c| Rc::ptr_eq(c, &typ_obj)).map(|p| p + 1).unwrap_or(mro.len());
        for c in &mro[pos..] {
            let found = c.dict.borrow().as_ref().and_then(|d| dict_get_name(d, name));
            if let Some(v) = found {
                if inst.is_none() {
                    return self.bind_descr_cls(&v, &start_cls);
                }
                let inst_is_type = matches!(&inst, Value::Obj(io) if matches!(io.kind, Kind::Type(_)) && self.is_subtype(io, &typ_obj));
                if inst_is_type {
                    return self.bind_descr_cls(&v, &start_cls);
                }
                return self.bind_descr(&v, &inst, &start_cls);
            }
        }
        let t = self.type_name(&typ_obj);
        let _ = t;
        Err(self.new_exc_str("AttributeError", &format!("'super' object has no attribute '{}'", nm)))
    }

    pub fn special_attr(&mut self, obj: &Value, nm: &str) -> R<Option<Value>> {
        let o = match obj {
            Value::Obj(o) => o,
            Value::Int(_) | Value::Bool(_) => {
                return Ok(match nm {
                    "real" | "numerator" => Some(Value::Int(obj.as_i64().unwrap_or(0))),
                    "imag" => Some(Value::Int(0)),
                    "denominator" => Some(Value::Int(1)),
                    "__class__" => Some(Value::Obj(self.type_of(obj))),
                    _ => None,
                })
            }
            Value::Float(f) => {
                return Ok(match nm {
                    "real" => Some(Value::Float(*f)),
                    "imag" => Some(Value::Float(0.0)),
                    "__class__" => Some(Value::Obj(self.type_of(obj))),
                    _ => None,
                })
            }
            _ => return Ok(if nm == "__class__" { Some(Value::Obj(self.type_of(obj))) } else { None }),
        };
        match &o.kind {
            Kind::Exception(d) => {
                let d = d.borrow();
                match nm {
                    "args" => return Ok(Some(d.args.clone())),
                    "__cause__" => return Ok(Some(d.cause.clone().map(Value::Obj).unwrap_or(Value::None))),
                    "__context__" => return Ok(Some(d.context.clone().map(Value::Obj).unwrap_or(Value::None))),
                    "__suppress_context__" => return Ok(Some(Value::Bool(d.suppress_context))),
                    "__traceback__" => {
                        return Ok(Some(self.make_tb(&d.tb)));
                    }
                    _ => {}
                }
            }
            Kind::Function(f) => match nm {
                "__name__" => return Ok(Some(Value::str(&f.name.borrow()))),
                "__qualname__" => return Ok(Some(Value::str(&f.qualname.borrow()))),
                "__doc__" => return Ok(Some(f.code.doc.clone().unwrap_or(Value::None))),
                "__module__" => {
                    return Ok(Some(dict_get_str(&f.globals, "__name__").unwrap_or(Value::None)));
                }
                "__defaults__" => {
                    let d = f.defaults.borrow();
                    return Ok(Some(if d.is_empty() { Value::None } else { Value::tuple(d.clone()) }));
                }
                "__kwdefaults__" => {
                    let d = f.kwdefaults.borrow();
                    if d.is_empty() {
                        return Ok(Some(Value::None));
                    }
                    let mut pd = PyDict::new();
                    for (k, v) in d.iter() {
                        pd.insert_new(hash_str(k.as_str_kind().unwrap_or("")), Value::Obj(k.clone()), v.clone());
                    }
                    return Ok(Some(Value::dict(pd)));
                }
                "__globals__" => return Ok(Some(Value::Obj(f.globals.clone()))),
                "__code__" => return Ok(Some(Value::Obj(Object::new(Kind::Code(f.code.clone()))))),
                "__closure__" => {
                    if f.closure.is_empty() {
                        return Ok(Some(Value::None));
                    }
                    return Ok(Some(Value::tuple(f.closure.iter().map(|c| Value::Obj(c.clone())).collect())));
                }
                "__annotations__" => {
                    let a = f.annotations.borrow().clone();
                    return Ok(Some(match a {
                        Some(d) => Value::Obj(d),
                        None => {
                            let d = Object::new(Kind::Dict(RefCell::new(PyDict::new())));
                            *f.annotations.borrow_mut() = Some(d.clone());
                            Value::Obj(d)
                        }
                    }));
                }
                "__type_params__" => {
                    return Ok(Some(f.type_params.borrow().clone().unwrap_or_else(|| Value::tuple(Vec::new()))));
                }
                "__dict__" => {
                    let d = self.instance_dict(o);
                    return Ok(Some(Value::Obj(d)));
                }
                "__class__" => return Ok(Some(Value::Obj(self.types.function.clone()))),
                _ => {}
            },
            Kind::Method(f, this) => match nm {
                "__self__" => return Ok(Some(this.clone())),
                "__func__" => return Ok(Some(f.clone())),
                "__name__" | "__qualname__" | "__doc__" | "__module__" | "__wrapped__" | "__text_signature__" => {
                    return self.get_attr_str(f, nm).map(Some).or_else(|e| if nm == "__doc__" { Ok(Some(Value::None)) } else { Err(e) })
                }
                // Other attributes not defined by the method type come from the function.
                _ => {
                    let mt = self.type_of_obj(o);
                    if !nm.starts_with("__") && self.lookup_mro(&mt, nm).is_none() {
                        let f = f.clone();
                        return self.get_attr_str(&f, nm).map(Some);
                    }
                }
            },
            Kind::Native(n) => match (nm, n.desc) {
                ("__name__", _) => return Ok(Some(Value::str(n.name))),
                ("__qualname__", _) if matches!(n.owner, Some(NativeOwner::Class(_))) => {
                    if let Some(NativeOwner::Class(c)) = &n.owner {
                        return Ok(Some(Value::string(format!("{}.{}", self.type_name(c), n.name))));
                    }
                }
                ("__qualname__", Some(d)) if d.class().is_some() => {
                    return Ok(Some(Value::string(format!("{}.{}", crate::bind::args::class_name(d), n.name))));
                }
                ("__objclass__", _) if n.method => {
                    if let Some(NativeOwner::Class(c)) = &n.owner {
                        return Ok(Some(Value::Obj(c.clone())));
                    }
                }
                ("__qualname__", _) => return Ok(Some(Value::str(n.name))),
                ("__doc__", Some(d)) => return Ok(Some(d.doc.map(Value::str).unwrap_or(Value::None))),
                ("__doc__", None) => return Ok(Some(Value::None)),
                ("__text_signature__", d) => {
                    return Ok(Some(d.and_then(crate::bind::args::text_signature).map(Value::string).unwrap_or(Value::None)))
                }
                ("__module__", Some(d)) if d.class().is_none() && d.module().is_some() => {
                    return Ok(Some(Value::str(d.module().unwrap_or("builtins"))))
                }
                ("__module__", None) if matches!(n.owner, Some(NativeOwner::Module(_))) => {
                    if let Some(NativeOwner::Module(m)) = &n.owner {
                        return Ok(Some(Value::str(m)));
                    }
                }
                ("__module__", _) => return Ok(Some(Value::str("builtins"))),
                ("__self__", d) if d.is_none_or(|d| d.class().is_none()) => {
                    let own = match &n.owner {
                        Some(NativeOwner::Module(m)) => Some(&**m),
                        _ => None,
                    };
                    let m = d.and_then(|d| d.module()).or(own).unwrap_or("builtins").to_string();
                    return self.import_module(&m).map(|m| Some(Value::Obj(m)));
                }
                _ => {}
            },
            Kind::Generator(g) => match nm {
                "__name__" => return Ok(Some(Value::str(&g.name.borrow()))),
                "__qualname__" => return Ok(Some(Value::str(&g.qualname.borrow()))),
                "gi_running" | "cr_running" | "ag_running" => return Ok(Some(Value::Bool(matches!(*g.state.borrow(), GenState::Running)))),
                "gi_frame" | "cr_frame" | "ag_frame" => {
                    return Ok(Some(match *g.state.borrow() {
                        GenState::Done => Value::None,
                        _ => Value::Obj(Object::new(Kind::Frame)),
                    }))
                }
                "gi_code" | "cr_code" | "ag_code" => {
                    return Ok(Some(match &*g.state.borrow() {
                        GenState::Created(f) | GenState::Suspended(f) => Value::Obj(Object::new(Kind::Code(f.code.clone()))),
                        _ => Value::None,
                    }))
                }
                "gi_yieldfrom" | "cr_await" | "ag_await" => return Ok(Some(Value::None)),
                _ => {}
            },
            Kind::Code(c) => match nm {
                "co_name" => return Ok(Some(Value::str(&c.name))),
                "co_filename" => return Ok(Some(Value::str(&c.filename))),
                "co_firstlineno" => return Ok(Some(Value::Int(c.first_line as i64))),
                "co_argcount" => return Ok(Some(Value::Int(c.argcount as i64))),
                "co_posonlyargcount" => return Ok(Some(Value::Int(c.posonly as i64))),
                "co_kwonlyargcount" => return Ok(Some(Value::Int(c.kwonly as i64))),
                "co_qualname" => return Ok(Some(Value::str(&c.qualname))),
                "co_nlocals" => return Ok(Some(Value::Int(c.varnames.len() as i64))),
                "co_cellvars" => return Ok(Some(Value::tuple(c.cellvars.iter().map(|n| Value::str(n)).collect()))),
                "co_freevars" => return Ok(Some(Value::tuple(c.freevars.iter().map(|n| Value::str(n)).collect()))),
                "co_names" => return Ok(Some(Value::tuple(c.names.iter().map(|n| Value::Obj(n.clone())).collect()))),
                "co_varnames" => return Ok(Some(Value::tuple(c.varnames.iter().map(|n| Value::str(n)).collect()))),
                "co_flags" => return Ok(Some(Value::Int(c.flags as i64))),
                _ => {}
            },
            Kind::Slice(a, b, c) => match nm {
                "start" => return Ok(Some(a.clone())),
                "stop" => return Ok(Some(b.clone())),
                "step" => return Ok(Some(c.clone())),
                _ => {}
            },
            Kind::BigRange(r) => match nm {
                "start" => return Ok(Some(Value::big(r[0].clone()))),
                "stop" => return Ok(Some(Value::big(r[1].clone()))),
                "step" => return Ok(Some(Value::big(r[2].clone()))),
                _ => {}
            },
            Kind::Range(r) => match nm {
                "start" => return Ok(Some(Value::Int(r.start))),
                "stop" => return Ok(Some(Value::Int(r.stop))),
                "step" => return Ok(Some(Value::Int(r.step))),
                _ => {}
            },
            Kind::Property(p) => match nm {
                "fget" => return Ok(Some(p.fget.clone())),
                "fset" => return Ok(Some(p.fset.clone())),
                "fdel" => return Ok(Some(p.fdel.clone())),
                "__doc__" => return Ok(Some(p.doc.clone())),
                "__isabstractmethod__" => {
                    let parts = [p.fget.clone(), p.fset.clone(), p.fdel.clone()];
                    for f in &parts {
                        if !f.is_none() && self.is_abstract_value(f)? {
                            return Ok(Some(Value::Bool(true)));
                        }
                    }
                    return Ok(Some(Value::Bool(false)));
                }
                _ => {}
            },
            Kind::StaticMethod(f) | Kind::ClassMethod(f) => {
                if nm == "__func__" {
                    return Ok(Some(f.clone()));
                }
                if nm == "__isabstractmethod__" {
                    let f = f.clone();
                    return Ok(Some(Value::Bool(self.is_abstract_value(&f)?)));
                }
            }
            Kind::Complex(re, im) => match nm {
                "real" => return Ok(Some(Value::Float(*re))),
                "imag" => return Ok(Some(Value::Float(*im))),
                _ => {}
            },
            Kind::Int(_) => match nm {
                "real" | "numerator" => return Ok(Some(obj.clone())),
                "imag" => return Ok(Some(Value::Int(0))),
                "denominator" => return Ok(Some(Value::Int(1))),
                _ => {}
            },
            Kind::Super(t, _, ot) => match nm {
                "__thisclass__" => return Ok(Some(t.clone())),
                "__self_class__" => return Ok(Some(ot.clone())),
                _ => {}
            },
            Kind::Cell(c) if nm == "cell_contents" => {
                return match c.borrow().clone() {
                    Some(v) => Ok(Some(v)),
                    None => Err(self.value_error("Cell is empty")),
                };
            }
            _ => {}
        }
        match nm {
            "__class__" => Ok(Some(Value::Obj(self.type_of_obj(o)))),
            "__dict__" => {
                if matches!(o.kind, Kind::Instance) {
                    if let Some(c) = &o.cls {
                        if self.lookup_mro(c, "__slots__").is_some() {
                            if self.slots_forbid(c, "\0") {
                                return Ok(None);
                            }
                            let names = self.slot_names(c);
                            let src = self.instance_dict(o);
                            let copy = self.new_dict();
                            let entries: Vec<(Value, Value)> = match &src.kind {
                                Kind::Dict(d) => d.borrow().iter().map(|e| (e.key.clone(), e.val.clone())).collect(),
                                _ => Vec::new(),
                            };
                            for (k, v) in entries {
                                if !k.as_str().is_some_and(|s| names.iter().any(|n| n == s)) {
                                    self.dict_set(&copy, k, v)?;
                                }
                            }
                            return Ok(Some(Value::Obj(copy)));
                        }
                    }
                }
                if matches!(o.kind, Kind::Instance | Kind::Module | Kind::List(_) | Kind::Dict(_) | Kind::Exception(_) | Kind::Opaque(_)) || o.cls.is_some() {
                    let d = self.instance_dict(o);
                    return Ok(Some(Value::Obj(d)));
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    pub fn set_attr(&mut self, obj: &Value, name: &Obj, v: Value) -> R<()> {
        let cls = self.type_of(obj);
        let hooks = self.hooks_of(&cls);
        if hooks & HOOK_SETATTR != 0 {
            let sa = self.lookup_mro(&cls, "__setattr__").unwrap();
            let b = self.bind_descr(&sa, obj, &cls)?;
            self.call(&b, vec![Value::Obj(name.clone()), v], Vec::new())?;
            return Ok(());
        }
        self.generic_setattr(obj, &cls, name, v)
    }

    pub fn set_attr_str(&mut self, obj: &Value, name: &str, v: Value) -> R<()> {
        let n = match Value::str(name) {
            Value::Obj(o) => o,
            _ => unreachable!(),
        };
        self.set_attr(obj, &n, v)
    }

    /// Stores a class attribute past any descriptor: the slots `type`'s own getsets write.
    pub fn type_store_attr(&mut self, o: &Obj, name: &Obj, v: Value) -> R<()> {
        let Kind::Type(td) = &o.kind else { unreachable!() };
        let nm = name.as_str_kind().unwrap_or("").to_string();
        match nm.as_str() {
            "__name__" => {
                *td.name.borrow_mut() = v.as_str().unwrap_or("").into();
                return Ok(());
            }
            "__qualname__" => {
                let Some(q) = v.as_str() else {
                    let msg = format!("can only assign string to {}.__qualname__, not '{}'", td.name.borrow(), self.type_name_of(&v));
                    return Err(self.type_error(&msg));
                };
                *td.qualname.borrow_mut() = Some(q.into());
                return Ok(());
            }
            "__bases__" => {
                let items: Vec<Obj> = v.tuple_items().unwrap_or(&[]).iter().filter_map(|b| b.as_obj().cloned()).collect();
                self.set_bases(o, items);
                self.type_epoch += 1;
                return Ok(());
            }
            "__abstractmethods__" => {
                let abstract_ = self.truthy(&v)?;
                let d = self.instance_dict(o);
                dict_set_name(&d, name, v);
                let f = td.flags.get();
                td.flags.set(if abstract_ { f | TF_ABSTRACT } else { f & !TF_ABSTRACT });
                return Ok(());
            }
            _ => {}
        }
        let d = self.instance_dict(o);
        dict_set_name(&d, name, v);
        if nm.starts_with("__") {
            self.type_epoch += 1;
        }
        Ok(())
    }

    fn check_mutable_type(&mut self, obj: &Value, nm: &str) -> R<()> {
        if let Value::Obj(o) = obj {
            if let Kind::Type(td) = &o.kind {
                if td.flags.get() & TF_IMMUTABLE != 0 {
                    let tn = td.name.borrow().clone();
                    return Err(self.type_error(&format!("cannot set '{nm}' attribute of immutable type '{tn}'")));
                }
            }
        }
        Ok(())
    }

    pub fn generic_setattr(&mut self, obj: &Value, cls: &Obj, name: &Obj, v: Value) -> R<()> {
        let nm = name.as_str_kind().unwrap_or("").to_string();
        self.check_mutable_type(obj, &nm)?;
        let o = match obj {
            Value::Obj(o) => o,
            _ => return Err(self.new_exc_str("AttributeError", &format!("'{}' object has no attribute '{}'", self.tp_name_of(obj), nm))),
        };
        if let Some(d) = self.lookup_mro_name(cls, name) {
            if let Value::Obj(dobj) = &d {
                match &dobj.kind {
                    Kind::Property(p) => {
                        if p.fset.is_none() {
                            if let Some(owner) = native_getter_owner(dobj, &p.fget) {
                                let msg = format!("attribute '{}' of '{}' objects is not writable", nm, owner);
                                return Err(self.new_exc_str("AttributeError", &msg));
                            }
                            let msg = format!("property '{}' of '{}' object has no setter", nm, self.type_name(cls));
                            return Err(self.new_exc_str("AttributeError", &msg));
                        }
                        let f = p.fset.clone();
                        self.call(&f, vec![obj.clone(), v], Vec::new())?;
                        return Ok(());
                    }
                    Kind::Instance | Kind::Opaque(_) => {
                        let dc = self.type_of_obj(dobj);
                        if let Some(s) = self.lookup_mro(&dc, "__set__") {
                            let b = self.bind_descr(&s, &d, &dc)?;
                            self.call(&b, vec![obj.clone(), v], Vec::new())?;
                            return Ok(());
                        }
                    }
                    _ => {}
                }
            }
        }
        match &o.kind {
            Kind::Type(_) => self.type_store_attr(o, name, v),
            Kind::Exception(d) => {
                match nm.as_str() {
                    "args" => {
                        let items = self.iterate_to_vec(&v)?;
                        d.borrow_mut().args = Value::tuple(items);
                    }
                    "__cause__" => {
                        let mut dm = d.borrow_mut();
                        dm.cause = v.as_obj().cloned();
                        dm.suppress_context = true;
                    }
                    "__context__" => d.borrow_mut().context = v.as_obj().cloned(),
                    "__suppress_context__" => d.borrow_mut().suppress_context = self.truthy(&v)?,
                    "__traceback__" => {
                        if v.is_none() {
                            d.borrow_mut().tb.clear();
                        }
                    }
                    _ => {
                        let dd = self.instance_dict(o);
                        dict_set_name(&dd, name, v);
                    }
                }
                Ok(())
            }
            Kind::Function(f) => {
                match nm.as_str() {
                    "__name__" => *f.name.borrow_mut() = v.as_str().unwrap_or("").into(),
                    "__qualname__" => *f.qualname.borrow_mut() = v.as_str().unwrap_or("").into(),
                    "__defaults__" => {
                        *f.defaults.borrow_mut() = v.tuple_items().map(|t| t.to_vec()).unwrap_or_default();
                    }
                    "__annotations__" => *f.annotations.borrow_mut() = v.as_obj().cloned(),
                    "__type_params__" => {
                        if v.tuple_items().is_none() {
                            return Err(self.type_error("__type_params__ must be set to a tuple"));
                        }
                        *f.type_params.borrow_mut() = Some(v);
                    }
                    _ => {
                        let dd = self.instance_dict(o);
                        dict_set_name(&dd, name, v);
                    }
                }
                Ok(())
            }
            Kind::Instance | Kind::Module => {
                if nm == "__class__" {
                    return Err(self.new_exc_str("TypeError", "__class__ assignment not supported"));
                }
                if matches!(o.kind, Kind::Instance) && self.lookup_mro(cls, "__slots__").is_some() && self.slots_forbid(cls, &nm) {
                    let msg = format!("'{}' object has no attribute '{}'", self.tp_name(cls), nm);
                    return Err(self.new_exc_str("AttributeError", &msg));
                }
                let dd = self.instance_dict(o);
                dict_set_name(&dd, name, v);
                Ok(())
            }
            _ => {
                if o.cls.is_some() {
                    if self.lookup_mro(cls, "__slots__").is_some() && self.slots_forbid(cls, &nm) {
                        let msg = format!("'{}' object has no attribute '{}'", self.tp_name(cls), nm);
                        return Err(self.new_exc_str("AttributeError", &msg));
                    }
                    let dd = self.instance_dict(o);
                    dict_set_name(&dd, name, v);
                    Ok(())
                } else if matches!(o.kind, Kind::Cell(_)) && nm == "cell_contents" {
                    if let Kind::Cell(c) = &o.kind {
                        *c.borrow_mut() = Some(v);
                    }
                    Ok(())
                } else {
                    Err(self.new_exc_str("AttributeError", &format!("'{}' object has no attribute '{}'", self.tp_name_of(obj), nm)))
                }
            }
        }
    }

    /// Builds a traceback chain; entries are stored innermost first.
    pub fn make_tb(&self, entries: &[TbEntry]) -> Value {
        let mut next = Value::None;
        for e in entries {
            let o = Object::with_cls(self.types.traceback.clone(), Kind::Instance);
            let d = self.instance_dict(&o);
            dict_set_str(&d, "tb_lineno", Value::Int(e.line as i64));
            dict_set_str(&d, "tb_next", next);
            dict_set_str(&d, "tb_frame", self.dead_frame_object(e.code.clone(), e.globals.clone(), e.line, e.lasti));
            dict_set_str(&d, "tb_lasti", Value::Int(2 * e.lasti as i64));
            next = Value::Obj(o);
        }
        next
    }

    fn slot_names(&self, cls: &Obj) -> Vec<String> {
        let Kind::Type(td) = &cls.kind else { return Vec::new() };
        let mro = td.mro.borrow().clone();
        let mut out = Vec::new();
        for c in &mro {
            if let Some(names) = class_slots(c) {
                out.extend(names.iter().map(|n| n.to_string()));
            }
        }
        out
    }

    fn slots_forbid(&self, cls: &Obj, name: &str) -> bool {
        let Kind::Type(td) = &cls.kind else { return false };
        let mro = td.mro.borrow().clone();
        let mut allowed = false;
        for c in &mro {
            if Rc::ptr_eq(c, &self.types.object) {
                continue;
            }
            let Some(names) = class_slots(c) else {
                if self.is_heap(c) {
                    return false;
                }
                continue;
            };
            for n in names.iter() {
                match &**n {
                    "__dict__" => return false,
                    s if s == name => allowed = true,
                    _ => {}
                }
            }
        }
        !allowed
    }

    pub fn del_attr(&mut self, obj: &Value, name: &Obj) -> R<()> {
        let cls = self.type_of(obj);
        let hooks = self.hooks_of(&cls);
        if hooks & HOOK_DELATTR != 0 {
            let sa = self.lookup_mro(&cls, "__delattr__").unwrap();
            let b = self.bind_descr(&sa, obj, &cls)?;
            self.call(&b, vec![Value::Obj(name.clone())], Vec::new())?;
            return Ok(());
        }
        self.generic_delattr(obj, &cls, name)
    }

    pub fn generic_delattr(&mut self, obj: &Value, cls: &Obj, name: &Obj) -> R<()> {
        let nm = name.as_str_kind().unwrap_or("").to_string();
        self.check_mutable_type(obj, &nm)?;
        if nm == "__dict__" && matches!(obj, Value::Obj(o) if o.dict.borrow().is_some() || matches!(o.kind, Kind::Instance | Kind::Opaque(_))) {
            return Err(self.type_error("cannot delete __dict__"));
        }
        if let Some(d) = self.lookup_mro_name(cls, name) {
            if let Value::Obj(dobj) = &d {
                match &dobj.kind {
                    Kind::Property(p) => {
                        if p.fdel.is_none() {
                            if let Some(owner) = native_getter_owner(dobj, &p.fget) {
                                let msg = format!("attribute '{}' of '{}' objects is not writable", nm, owner);
                                return Err(self.new_exc_str("AttributeError", &msg));
                            }
                            let msg = format!("property '{}' of '{}' object has no deleter", nm, self.type_name(cls));
                            return Err(self.new_exc_str("AttributeError", &msg));
                        }
                        let f = p.fdel.clone();
                        self.call(&f, vec![obj.clone()], Vec::new())?;
                        return Ok(());
                    }
                    Kind::Instance | Kind::Opaque(_) => {
                        let dc = self.type_of_obj(dobj);
                        if let Some(s) = self.lookup_mro(&dc, "__delete__") {
                            let b = self.bind_descr(&s, &d, &dc)?;
                            self.call(&b, vec![obj.clone()], Vec::new())?;
                            return Ok(());
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Value::Obj(o) = obj {
            let dd = o.dict.borrow().clone();
            if let Some(dd) = dd {
                if dict_del_name(&dd, name).is_some() {
                    if matches!(o.kind, Kind::Type(_)) && nm.starts_with("__") {
                        self.type_epoch += 1;
                    }
                    return Ok(());
                }
            }
        }
        Err(self.attr_error(obj, &nm))
    }

    // ---- classes ---------------------------------------------------------------------------

    pub fn call_type(&mut self, cls: &Obj, args: Vec<Value>, kw: Vec<(Obj, Value)>) -> R<Value> {
        let meta = self.type_of_obj(cls);
        if !Rc::ptr_eq(&meta, &self.types.type_) {
            if let Some((owner, m)) = self.lookup_mro_with_owner(&meta, "__call__") {
                if !Rc::ptr_eq(&owner, &self.types.type_) {
                    let b = self.bind_descr(&m, &Value::Obj(cls.clone()), &meta)?;
                    return self.call(&b, args, kw);
                }
            }
        }
        self.type_call_default(cls, args, kw)
    }

    pub fn type_call_default(&mut self, cls: &Obj, args: Vec<Value>, kw: Vec<(Obj, Value)>) -> R<Value> {
        if Rc::ptr_eq(cls, &self.types.type_) {
            if args.len() == 1 && kw.is_empty() {
                return Ok(Value::Obj(self.type_of(&args[0])));
            }
            if args.len() != 3 {
                return Err(self.type_error("type() takes 1 or 3 arguments"));
            }
            let mut winner = cls.clone();
            if let Some(bases) = args[1].tuple_items() {
                for b in bases.iter() {
                    if let Value::Obj(bo) = b {
                        let bm = self.type_of_obj(bo);
                        if self.is_subtype(&bm, &winner) {
                            winner = bm;
                        }
                    }
                }
            }
            if !Rc::ptr_eq(&winner, cls) {
                return self.type_call_default(&winner, args, kw);
            }
            return self.type_new_from_args(cls.clone(), &args, kw);
        }
        let new = match self.lookup_mro(cls, "__new__") {
            Some(n) => n,
            None => return Err(self.type_error("cannot create instances")),
        };
        let is_obj_new = matches!((&new, &self.obj_new), (Value::Obj(a), Some(b)) if Rc::ptr_eq(a, b));
        let obj = if is_obj_new {
            let init = self.lookup_mro(cls, "__init__");
            let init_is_object = matches!((&init, &self.obj_init), (Some(Value::Obj(a)), Some(b)) if Rc::ptr_eq(a, b));
            if init_is_object && (!args.is_empty() || !kw.is_empty()) {
                let msg = format!("{}() takes no arguments", self.type_name(cls));
                return Err(self.type_error(&msg));
            }
            let o = self.alloc_instance(cls)?;
            if init_is_object {
                return Ok(o);
            }
            o
        } else {
            let mut a = Vec::with_capacity(args.len() + 1);
            a.push(Value::Obj(cls.clone()));
            a.extend(args.iter().cloned());
            self.call(&new, a, kw.clone())?
        };
        let ot = self.type_of(&obj);
        if !self.is_subtype(&ot, cls) {
            return Ok(obj);
        }
        if let Some(init) = self.lookup_mro(&ot, "__init__") {
            let is_obj_init = matches!((&init, &self.obj_init), (Value::Obj(a), Some(b)) if Rc::ptr_eq(a, b));
            if !is_obj_init {
                let b = self.bind_descr(&init, &obj, &ot)?;
                let r = self.call(&b, args, kw)?;
                if !r.is_none() {
                    let msg = format!("__init__() should return None, not '{}'", self.type_name_of(&r));
                    return Err(self.type_error(&msg));
                }
            }
        }
        Ok(obj)
    }

    pub fn alloc_instance(&mut self, cls: &Obj) -> R<Value> {
        if matches!(&cls.kind, Kind::Type(td) if td.flags.get() & TF_ABSTRACT != 0) {
            self.check_abstract(cls)?;
        }
        let layout = self.type_layout(cls);
        let exact = |t: &Obj, s: &Interp, base: &Obj| Rc::ptr_eq(t, base) || !s.is_heap(t);
        let kind = match layout {
            Layout::Object => Kind::Instance,
            Layout::List => Kind::List(RefCell::new(Vec::new())),
            Layout::Dict => Kind::Dict(RefCell::new(PyDict::new())),
            Layout::Set => Kind::Set(RefCell::new(PyDict::new_set())),
            Layout::FrozenSet => Kind::FrozenSet(RefCell::new(PyDict::new_set())),
            Layout::ByteArray => Kind::ByteArray(ba_store(Vec::new())),
            Layout::Exception => Kind::Exception(RefCell::new(ExcData {
                args: Value::tuple(Vec::new()),
                cause: None,
                context: None,
                suppress_context: false,
                ctx_set: false,
                tb: Vec::new(),
            })),
            Layout::Int => Kind::Int(crate::pyint::BigInt::zero()),
            Layout::Float => Kind::Float(0.0),
            Layout::Str => Kind::Str(PyStr::new("")),
            Layout::Tuple => Kind::Tuple(Vec::new()),
            Layout::Bytes => Kind::Bytes(Vec::new()),
            Layout::Module => Kind::Module,
            _ => Kind::Instance,
        };
        let _ = exact;
        let o = if Rc::ptr_eq(cls, &self.types.object) || (layout == Layout::Object && false) {
            Object::new(Kind::Instance)
        } else if Rc::ptr_eq(cls, &self.types.list)
            || Rc::ptr_eq(cls, &self.types.dict)
            || Rc::ptr_eq(cls, &self.types.set)
            || Rc::ptr_eq(cls, &self.types.frozenset)
        {
            Object::new(kind)
        } else {
            Object::with_cls(cls.clone(), kind)
        };
        Ok(Value::Obj(o))
    }

    fn is_abstract_value(&mut self, f: &Value) -> R<bool> {
        match self.get_attr_str(f, "__isabstractmethod__") {
            Ok(v) => self.truthy(&v),
            Err(_) => Ok(false),
        }
    }

    fn check_abstract(&mut self, cls: &Obj) -> R<()> {
        let names = match cls.dict.borrow().as_ref().and_then(|d| dict_get_str(d, "__abstractmethods__")) {
            Some(v) => self.iterate_to_vec(&v)?,
            None => return Ok(()),
        };
        let mut names: Vec<String> = names.iter().filter_map(|n| n.as_str().map(str::to_string)).collect();
        if names.is_empty() {
            return Ok(());
        }
        names.sort();
        let msg = format!(
            "Can't instantiate abstract class {} with abstract method{} {}",
            self.type_name(cls),
            if names.len() > 1 { "s" } else { "" },
            names.join(", ")
        );
        Err(self.type_error(&msg))
    }

    pub fn is_heap(&self, t: &Obj) -> bool {
        match &t.kind {
            Kind::Type(td) => td.flags.get() & TF_HEAP != 0,
            _ => false,
        }
    }

    pub fn type_new_from_args(&mut self, meta: Obj, args: &[Value], kw: Vec<(Obj, Value)>) -> R<Value> {
        let name = match args[0].as_str() {
            Some(s) => s.to_string(),
            None => return Err(self.type_error("type.__new__() argument 1 must be str")),
        };
        let bases: Vec<Obj> = match args[1].tuple_items() {
            Some(t) => {
                let mut v = Vec::new();
                for b in t {
                    match b {
                        Value::Obj(o) if matches!(o.kind, Kind::Type(_)) => v.push(o.clone()),
                        _ => return Err(self.type_error("bases must be types")),
                    }
                }
                v
            }
            None => return Err(self.type_error("type.__new__() argument 2 must be tuple")),
        };
        let ns = match &args[2] {
            Value::Obj(o) if matches!(o.kind, Kind::Dict(_)) => o.clone(),
            _ => return Err(self.type_error("type.__new__() argument 3 must be dict")),
        };
        self.new_class(&meta, &name, bases, &ns, kw)
    }

    pub fn new_class(&mut self, meta: &Obj, name: &str, mut bases: Vec<Obj>, ns: &Obj, kw: Vec<(Obj, Value)>) -> R<Value> {
        if bases.is_empty() {
            bases.push(self.types.object.clone());
        }
        let mut layout = Layout::Object;
        for b in &bases {
            if let Kind::Type(td) = &b.kind {
                if td.flags.get() & TF_FINAL != 0 {
                    let n = self.type_display(b);
                    return Err(self.type_error(&format!("type '{n}' is not an acceptable base type")));
                }
            }
            let bl = self.type_layout(b);
            if bl != Layout::Object {
                if layout == Layout::Object {
                    layout = bl;
                } else if layout != bl && !(matches!(layout, Layout::Int) && matches!(bl, Layout::Int)) {
                    return Err(self.type_error("multiple bases have instance lay-out conflict"));
                }
            }
        }
        let dict = Object::new(Kind::Dict(RefCell::new(match &ns.kind {
            Kind::Dict(d) => d.borrow().clone(),
            _ => PyDict::new(),
        })));
        let qualname: Option<Rc<str>> = match dict_get_str(&dict, "__qualname__") {
            Some(v) => match v.as_str() {
                Some(q) => {
                    let q = q.into();
                    dict_del_str(&dict, "__qualname__");
                    Some(q)
                }
                None => {
                    let msg = format!("type __qualname__ must be a str, not {}", self.type_name_of(&v));
                    return Err(self.type_error(&msg));
                }
            },
            None => None,
        };
        if dict_get_str(&dict, "__module__").is_none() {
            let m = self.frames.last().and_then(|f| dict_get_str(&f.globals, "__name__")).unwrap_or_else(|| Value::str("__main__"));
            dict_set_str(&dict, "__module__", m);
        }
        if dict_get_str(&dict, "__doc__").is_none() {
            dict_set_str(&dict, "__doc__", Value::None);
        }
        if dict_get_str(&dict, "__eq__").is_some() && dict_get_str(&dict, "__hash__").is_none() {
            dict_set_str(&dict, "__hash__", Value::None);
        }
        for name in ["__init_subclass__", "__class_getitem__"] {
            if let Some(Value::Obj(f)) = dict_get_str(&dict, name) {
                if matches!(f.kind, Kind::Function(_)) {
                    dict_set_str(&dict, name, Value::Obj(Object::new(Kind::ClassMethod(Value::Obj(f)))));
                }
            }
        }
        if let Some(Value::Obj(f)) = dict_get_str(&dict, "__new__") {
            if matches!(f.kind, Kind::Function(_)) {
                dict_set_str(&dict, "__new__", Value::Obj(Object::new(Kind::StaticMethod(Value::Obj(f)))));
            }
        }
        let slots = match dict_get_str(&dict, "__slots__") {
            None => None,
            Some(v) => {
                let items = if v.as_str().is_some() { vec![v] } else { self.iterate_to_vec(&v)? };
                let private: Option<Rc<str>> = Some(name.into());
                let mut names: Vec<Rc<str>> = Vec::with_capacity(items.len());
                for item in &items {
                    let Some(s) = item.as_str() else {
                        let t = self.type_name_of(item);
                        return Err(self.type_error(&format!("__slots__ items must be strings, not '{t}'")));
                    };
                    names.push(crate::symtable::mangle(&private, s));
                }
                Some(Rc::from(names))
            }
        };
        let cls_field = if Rc::ptr_eq(meta, &self.types.type_) { None } else { Some(meta.clone()) };
        let ty = Rc::new(Object {
            cls: cls_field,
            dict: RefCell::new(Some(dict.clone())),
            id: std::cell::Cell::new(0),
            kind: Kind::Type(TypeData {
                name: RefCell::new(name.into()),
                qualname: RefCell::new(qualname),
                bases: RefCell::new(Vec::new()),
                mro: RefCell::new(Vec::new()),
                layout: Cell::new(layout),
                flags: Cell::new(TF_HEAP),
                hooks: Cell::new((u64::MAX, 0)),
                slots: RefCell::new(slots),
            }),
        });
        match self.compute_mro(&ty, &bases) {
            Some(mro) => {
                if let Kind::Type(td) = &ty.kind {
                    *td.bases.borrow_mut() = bases.clone();
                    *td.mro.borrow_mut() = mro;
                }
            }
            None => {
                return Err(self.type_error("Cannot create a consistent method resolution order (MRO) for bases"));
            }
        }
        self.type_epoch += 1;
        self.subclass_registry.push(Rc::downgrade(&ty));
        let entries: Vec<(Value, Value)> = match &dict.kind {
            Kind::Dict(d) => d.borrow().iter().map(|e| (e.key.clone(), e.val.clone())).collect(),
            _ => Vec::new(),
        };
        for (k, v) in &entries {
            if let (Some(kn), Value::Obj(vo)) = (k.as_str(), v) {
                if matches!(vo.kind, Kind::Instance) {
                    let vc = self.type_of_obj(vo);
                    if let Some(sn) = self.lookup_mro(&vc, "__set_name__") {
                        let b = self.bind_descr(&sn, v, &vc)?;
                        self.call(&b, vec![Value::Obj(ty.clone()), Value::str(kn)], Vec::new())?;
                    }
                }
            }
        }
        let mro: Vec<Obj> = match &ty.kind {
            Kind::Type(td) => td.mro.borrow().clone(),
            _ => Vec::new(),
        };
        let mut init_sub = None;
        for c in &mro[1..] {
            if let Some(d) = c.dict.borrow().as_ref() {
                if let Some(v) = dict_get_str(d, "__init_subclass__") {
                    init_sub = Some(v);
                    break;
                }
            }
        }
        if let Some(f) = init_sub {
            let b = self.bind_descr_cls(&f, &ty)?;
            let b = match &b {
                Value::Obj(o) if matches!(o.kind, Kind::Function(_)) => {
                    Value::Obj(Object::new(Kind::Method(b.clone(), Value::Obj(ty.clone()))))
                }
                _ => b,
            };
            self.call(&b, Vec::new(), kw)?;
        }
        Ok(Value::Obj(ty))
    }

    pub fn build_class(&mut self, args: Vec<Value>, mut kw: Vec<(Obj, Value)>) -> R<Value> {
        if args.len() < 2 {
            return Err(self.type_error("__build_class__: not enough arguments"));
        }
        let func = match &args[0] {
            Value::Obj(o) if matches!(o.kind, Kind::Function(_)) => o.clone(),
            _ => return Err(self.type_error("__build_class__: func must be a function")),
        };
        let name = args[1].as_str().unwrap_or("").to_string();
        let mut base_vals: Vec<Value> = args[2..].to_vec();
        let mut meta_kw: Option<Value> = None;
        kw.retain(|(k, v)| {
            if k.as_str_kind() == Some("metaclass") {
                meta_kw = Some(v.clone());
                false
            } else {
                true
            }
        });
        let mut orig_bases_changed = false;
        let mut resolved: Vec<Value> = Vec::new();
        for b in &base_vals {
            let is_type = matches!(b, Value::Obj(o) if matches!(o.kind, Kind::Type(_)));
            if !is_type {
                let entries = match self.get_attr_str(b, "__mro_entries__") {
                    Ok(m) => Some(m),
                    Err(e) if self.exc_is(&e, "AttributeError") => None,
                    Err(e) => return Err(e),
                };
                if let Some(bound) = entries {
                    let r = self.call(&bound, vec![Value::tuple(args[2..].to_vec())], Vec::new())?;
                    if let Some(t) = r.tuple_items() {
                        resolved.extend(t.iter().cloned());
                        orig_bases_changed = true;
                        continue;
                    }
                }
            }
            resolved.push(b.clone());
        }
        let orig_bases = base_vals.clone();
        base_vals = resolved;
        let mut base_objs: Vec<Obj> = Vec::new();
        for b in &base_vals {
            match b {
                Value::Obj(o) if matches!(o.kind, Kind::Type(_)) => base_objs.push(o.clone()),
                _ => return Err(self.type_error("bases must be types")),
            }
        }
        let mut meta: Obj = match &meta_kw {
            Some(Value::Obj(m)) if matches!(m.kind, Kind::Type(_)) => m.clone(),
            Some(other) => {
                let _ = other;
                self.types.type_.clone()
            }
            None => match base_objs.first() {
                Some(b) => self.type_of_obj(b),
                None => self.types.type_.clone(),
            },
        };
        for b in &base_objs {
            let bm = self.type_of_obj(b);
            if self.is_subtype(&bm, &meta) {
                meta = bm;
            }
        }
        let meta_val = match &meta_kw {
            Some(m) if !matches!(m, Value::Obj(o) if matches!(o.kind, Kind::Type(_))) => m.clone(),
            _ => Value::Obj(meta.clone()),
        };
        let ns = match self.lookup_mro(&meta, "__prepare__") {
            Some(p) => {
                let b = self.bind_descr_cls(&p, &meta)?;
                let r = self.call(&b, vec![Value::str(&name), Value::tuple(base_vals.clone())], kw.clone())?;
                match r {
                    Value::Obj(o) => o,
                    _ => Object::new(Kind::Dict(RefCell::new(PyDict::new()))),
                }
            }
            None => Object::new(Kind::Dict(RefCell::new(PyDict::new()))),
        };
        if orig_bases_changed {
            dict_set_str(&ns, "__orig_bases__", Value::tuple(orig_bases));
        }
        self.call_body(&func, &ns)?;
        let cell = dict_del_str(&ns, "__classcell__");
        let dict_cell = dict_del_str(&ns, "__classdictcell__");
        let mut cargs = vec![Value::str(&name), Value::tuple(base_vals), Value::Obj(ns)];
        let cls = self.call(&meta_val, std::mem::take(&mut cargs), kw)?;
        if let Some(Value::Obj(c)) = cell {
            if let Kind::Cell(cc) = &c.kind {
                *cc.borrow_mut() = Some(cls.clone());
            }
        }
        if let (Some(Value::Obj(c)), Value::Obj(co)) = (dict_cell, &cls) {
            if let (Kind::Cell(cc), Some(d)) = (&c.kind, co.dict.borrow().clone()) {
                *cc.borrow_mut() = Some(Value::Obj(d));
            }
        }
        Ok(cls)
    }

    pub fn isinstance_value(&mut self, v: &Value, cls: &Value) -> R<bool> {
        if let Some(items) = cls.tuple_items() {
            for c in items {
                if self.isinstance_value(v, c)? {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
        match cls {
            Value::Obj(c) if matches!(c.kind, Kind::Type(_)) => {
                let meta = self.type_of_obj(c);
                if !Rc::ptr_eq(&meta, &self.types.type_) {
                    if let Some(m) = self.lookup_mro(&meta, "__instancecheck__") {
                        let b = self.bind_descr(&m, cls, &meta)?;
                        let r = self.call(&b, vec![v.clone()], Vec::new())?;
                        return self.truthy(&r);
                    }
                }
                let t = self.type_of(v);
                if self.is_subtype(&t, c) {
                    return Ok(true);
                }
                if let Value::Obj(o) = v {
                    if !matches!(o.kind, Kind::Type(_)) {
                        if let Some(Value::Obj(kc)) = self.special_attr(v, "__class__").ok().flatten() {
                            if !Rc::ptr_eq(&kc, &t) {
                                return Ok(self.is_subtype(&kc, c));
                            }
                        }
                    }
                }
                Ok(false)
            }
            _ if crate::builtins::alias::union_members(cls).is_none() && self.user_special(cls, "__instancecheck__").is_some() => {
                let r = self.call_special(cls, "__instancecheck__", vec![v.clone()])?;
                self.truthy(&r)
            }
            _ => match crate::builtins::alias::union_members(cls) {
                Some(members) => {
                    for m in &members {
                        if self.isinstance_value(v, m)? {
                            return Ok(true);
                        }
                    }
                    Ok(false)
                }
                None => Err(self.type_error("isinstance() arg 2 must be a type, a tuple of types, or a union")),
            },
        }
    }

    pub fn issubclass_value(&mut self, sub: &Value, cls: &Value) -> R<bool> {
        if let Some(items) = cls.tuple_items() {
            for c in items {
                if self.issubclass_value(sub, c)? {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
        let cls_is_type = matches!(cls, Value::Obj(c) if matches!(c.kind, Kind::Type(_)));
        if !cls_is_type && crate::builtins::alias::union_members(cls).is_none() && self.user_special(cls, "__subclasscheck__").is_some() {
            let r = self.call_special(cls, "__subclasscheck__", vec![sub.clone()])?;
            return self.truthy(&r);
        }
        let s = match sub {
            Value::Obj(o) if matches!(o.kind, Kind::Type(_)) => o.clone(),
            _ => return Err(self.type_error("issubclass() arg 1 must be a class")),
        };
        match cls {
            Value::Obj(c) if matches!(c.kind, Kind::Type(_)) => {
                let meta = self.type_of_obj(c);
                if !Rc::ptr_eq(&meta, &self.types.type_) {
                    if let Some(m) = self.lookup_mro(&meta, "__subclasscheck__") {
                        let b = self.bind_descr(&m, cls, &meta)?;
                        let r = self.call(&b, vec![sub.clone()], Vec::new())?;
                        return self.truthy(&r);
                    }
                }
                Ok(self.is_subtype(&s, c))
            }
            _ => match crate::builtins::alias::union_members(cls) {
                Some(members) => {
                    for m in &members {
                        if self.issubclass_value(sub, m)? {
                            return Ok(true);
                        }
                    }
                    Ok(false)
                }
                None => Err(self.type_error("issubclass() arg 2 must be a class, a tuple of classes, or a union")),
            },
        }
    }

    pub fn new_native(&self, name: &'static str, f: NativeFn, method: bool) -> Value {
        Value::Obj(Object::new(Kind::Native(NativeData { name, f, method, desc: None, owner: None })))
    }

    /// A module-level function of module `module`.
    pub fn new_module_native(&self, module: &str, name: &'static str, f: NativeFn) -> Value {
        let owner = Some(NativeOwner::Module(module.into()));
        Value::Obj(Object::new(Kind::Native(NativeData { name, f, method: false, desc: None, owner })))
    }

    pub fn reg(&mut self, ty: &Obj, name: &'static str, f: NativeFn) {
        let owner = Some(NativeOwner::Class(ty.clone()));
        let v = Value::Obj(Object::new(Kind::Native(NativeData { name, f, method: true, desc: None, owner })));
        if let Some(d) = ty.dict.borrow().as_ref() {
            dict_set_str(d, name, v);
        }
    }

    pub fn reg_static(&mut self, ty: &Obj, name: &'static str, f: NativeFn) {
        let n = self.new_native(name, f, false);
        let v = Value::Obj(Object::new(Kind::StaticMethod(n)));
        if let Some(d) = ty.dict.borrow().as_ref() {
            dict_set_str(d, name, v);
        }
    }

    pub fn reg_class(&mut self, ty: &Obj, name: &'static str, f: NativeFn) {
        let n = self.new_native(name, f, false);
        let v = Value::Obj(Object::new(Kind::ClassMethod(n)));
        if let Some(d) = ty.dict.borrow().as_ref() {
            dict_set_str(d, name, v);
        }
    }

    pub fn reg_new(&mut self, ty: &Obj, f: NativeFn) {
        let n = self.new_native("__new__", f, false);
        if let Some(d) = ty.dict.borrow().as_ref() {
            dict_set_str(d, "__new__", n);
        }
    }

    pub fn reg_prop(&mut self, ty: &Obj, name: &'static str, f: NativeFn) {
        let g = self.new_native(name, f, false);
        let p = Value::Obj(Object::new(Kind::Property(PropData { fget: g, fset: Value::None, fdel: Value::None, doc: Value::None })));
        if let Some(d) = ty.dict.borrow().as_ref() {
            dict_set_str(d, name, p);
        }
    }

    pub fn module_globals(&self) -> Obj {
        self.frames.last().map(|f| f.globals.clone()).unwrap_or_else(|| self.builtins.clone())
    }
}

/// The class of a native `#[getter]` property (CPython's `getset_descriptor`, whose write
/// errors read `attribute 'x' of 'mod.Cls' objects is not writable`).
fn native_getter_owner(descr: &Obj, fget: &Value) -> Option<String> {
    if let Some(Value::Obj(owner)) = descr.dict.borrow().as_ref().and_then(|d| dict_get_str(d, "__objclass__")) {
        if let Kind::Type(td) = &owner.kind {
            return Some(td.name.borrow().to_string());
        }
    }
    match fget {
        Value::Obj(o) => match &o.kind {
            Kind::Native(n) => n.desc.filter(|d| d.role == lumen_bind::Role::Getter).map(crate::bind::owner_of),
            _ => None,
        },
        _ => None,
    }
}

fn class_slots(c: &Obj) -> Option<Rc<[Rc<str>]>> {
    match &c.kind {
        Kind::Type(td) => td.slots.borrow().clone(),
        _ => None,
    }
}
