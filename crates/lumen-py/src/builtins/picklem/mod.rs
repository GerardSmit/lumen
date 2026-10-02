//! `_pickle`: the pickler and unpickler of CPython's `_pickle.c` on the shared opcode, framing and
//! stack machinery of `lumen_common::pickle`.

mod load;
mod save;
mod shared;

use crate::bind::{is_instance, Py};
use crate::object::*;
use crate::vm::{dict_get_str, Interp};

pub use shared::ModState;

pub struct PbData {
    pub data: Vec<u8>,
    pub readonly: bool,
    pub contiguous: bool,
}

fn released_buffer(it: &mut Interp) -> Obj {
    it.value_error("operation forbidden on released PickleBuffer object")
}

pub fn is_pickle_buffer(it: &Interp, v: &Value) -> bool {
    is_instance::<_pickle::PickleBuffer>(it, v)
}

/// The memoryview a `PickleBuffer` wraps, `None` when `v` is not a `PickleBuffer`.
pub fn buffer_inner(it: &mut Interp, v: &Value) -> R<Option<Value>> {
    let Some(p) = Py::<_pickle::PickleBuffer>::from_value(it, v) else { return Ok(None) };
    let view = p.borrow(it)?.view.clone();
    match view {
        Some(view) => Ok(Some(view)),
        None => Err(released_buffer(it)),
    }
}

pub fn pickle_buffer_data(it: &mut Interp, v: &Value) -> R<PbData> {
    let Some(view) = buffer_inner(it, v)? else {
        return Err(it.type_error("expected a PickleBuffer"));
    };
    let contiguous = it.get_attr_str(&view, "contiguous")?;
    let contiguous = it.truthy(&contiguous)?;
    let readonly = it.get_attr_str(&view, "readonly")?;
    let readonly = it.truthy(&readonly)?;
    let data = if contiguous { load::buffer_bytes(it, &view)? } else { Vec::new() };
    Ok(PbData { data, readonly, contiguous })
}

fn memoryview_of(it: &mut Interp, obj: &Value) -> R<Value> {
    let ty = dict_get_str(&it.builtins, "memoryview").unwrap_or(Value::None);
    it.call(&ty, vec![obj.clone()], Vec::new())
}

/// Optimized C implementation for the Python pickle module.
#[lumen_bind::module(name = "_pickle")]
pub mod _pickle {
    use super::load::{find_class_impl, set_buffers, set_input_stream, set_string_input, Ld, UnpicklerCore};
    use super::save::{set_buffer_callback, set_protocol, PicklerCore, Pk};
    use super::shared::{attr_opt, pickling_error, rebuild_bound, shared, split_bound, str_arg, unpickling_error, ModState};
    use crate::bind::{KwArgs, Py, This};
    use crate::builtins::memview;
    use crate::builtins::native::new_type;
    use crate::object::*;
    use crate::vm::{dict_set_str, Interp};
    use std::rc::Rc;

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let exc = it.exc_type("Exception");
        let pickle_error = new_type(it, "_pickle", "PickleError", Some(&exc), Layout::Exception);
        let pickling = new_type(it, "_pickle", "PicklingError", Some(&pickle_error), Layout::Exception);
        let unpickling = new_type(it, "_pickle", "UnpicklingError", Some(&pickle_error), Layout::Exception);
        let d = it.module_dict(m);
        dict_set_str(&d, "PickleError", Value::Obj(pickle_error.clone()));
        dict_set_str(&d, "PicklingError", Value::Obj(pickling.clone()));
        dict_set_str(&d, "UnpicklingError", Value::Obj(unpickling.clone()));
        it.native_state::<ModState>().errors = Some((pickle_error, pickling, unpickling));
    }

    fn not_init(it: &mut Interp, which: &str, slf: &Value) -> String {
        format!("{}.__init__() was not called by {}.__init__()", which, it.tp_name_of(slf))
    }

    fn as_dict_obj(v: &Value) -> Option<&Obj> {
        match v {
            Value::Obj(o) if matches!(o.kind, Kind::Dict(_)) => Some(o),
            _ => None,
        }
    }

    /// Wrap a buffer-providing object as a picklable buffer.
    #[class(name = "PickleBuffer")]
    pub struct PickleBuffer {
        pub view: Option<Value>,
    }

    #[methods]
    impl PickleBuffer {
        #[constructor]
        fn new(it: &mut Interp, #[kw] buffer: &Value) -> R<PickleBuffer> {
            if memview::export(it, buffer)?.is_none() {
                let t = it.type_name_of(buffer);
                return Err(it.type_error(&format!("a bytes-like object is required, not '{}'", t)));
            }
            Ok(PickleBuffer { view: Some(super::memoryview_of(it, buffer)?) })
        }

        /// Return a memoryview of the raw memory underlying this buffer.
        /// Will raise BufferError is the buffer isn't contiguous.
        fn raw(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            let Some(view) = super::buffer_inner(it, slf.0.value())? else { unreachable!() };
            let contiguous = it.get_attr_str(&view, "contiguous")?;
            if !it.truthy(&contiguous)? {
                return Err(it.new_exc_str("BufferError", "cannot extract raw buffer from non-contiguous buffer"));
            }
            it.call_method(&view, "cast", vec![Value::str("B")])
        }

        /// Release the underlying buffer exposed by the PickleBuffer object.
        fn release(slf: This<Py<Self>>, it: &mut Interp) -> R<()> {
            let view = slf.0.borrow_mut(it)?.view.take();
            if let Some(view) = view {
                it.call_method(&view, "release", Vec::new())?;
            }
            Ok(())
        }
    }

    #[class(name = "PicklerMemoProxy", skip(py), hint(py(unhashable)))]
    pub struct PicklerMemoProxy {
        core: Rc<PicklerCore>,
    }

    fn pickler_memo_copy(it: &mut Interp, core: &PicklerCore) -> R<Value> {
        let dict = super::shared::new_dict();
        let entries: Vec<(usize, usize, Value)> = core.memo.borrow().iter().map(|(k, (idx, obj))| (*k, *idx, obj.clone())).collect();
        for (key, idx, obj) in entries {
            it.dict_set(&dict, Value::Int(key as i64), Value::tuple(vec![Value::Int(idx as i64), obj]))?;
        }
        Ok(Value::Obj(dict))
    }

    #[methods]
    impl PicklerMemoProxy {
        /// Remove all items from memo.
        fn clear(&self) {
            self.core.memo.borrow_mut().clear();
        }

        /// Copy the memo to a new object.
        fn copy(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            let core = slf.0.borrow(it)?.core.clone();
            pickler_memo_copy(it, &core)
        }

        /// Implement pickle support.
        #[method(name = "__reduce__")]
        fn reduce(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            let core = slf.0.borrow(it)?.core.clone();
            let contents = pickler_memo_copy(it, &core)?;
            Ok(Value::tuple(vec![Value::Obj(it.types.dict.clone()), Value::tuple(vec![contents])]))
        }
    }

    /// This takes a binary file for writing a pickle data stream.
    ///
    /// The optional *protocol* argument tells the pickler to use the given
    /// protocol; supported protocols are 0, 1, 2, 3, 4 and 5.  The default
    /// protocol is 4. It was introduced in Python 3.4, and is incompatible
    /// with previous versions.
    ///
    /// Specifying a negative protocol version selects the highest protocol
    /// version supported.  The higher the protocol used, the more recent the
    /// version of Python needed to read the pickle produced.
    ///
    /// The *file* argument must have a write() method that accepts a single
    /// bytes argument. It can thus be a file object opened for binary
    /// writing, an io.BytesIO instance, or any other custom object that meets
    /// this interface.
    ///
    /// If *fix_imports* is True and protocol is less than 3, pickle will try
    /// to map the new Python 3 names to the old module names used in Python
    /// 2, so that the pickle data stream is readable with Python 2.
    ///
    /// If *buffer_callback* is None (the default), buffer views are
    /// serialized into *file* as part of the pickle stream.
    ///
    /// If *buffer_callback* is not None, then it can be called any number
    /// of times with a buffer view.  If the callback returns a false value
    /// (such as None), the given buffer is out-of-band; otherwise the
    /// buffer is serialized in-band, i.e. inside the pickle stream.
    ///
    /// It is an error if *buffer_callback* is not None and *protocol*
    /// is None or smaller than 5.
    #[class(name = "Pickler")]
    pub struct Pickler {
        core: Rc<PicklerCore>,
    }

    #[methods]
    impl Pickler {
        #[constructor]
        fn new(#[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> Pickler {
            let _ = (args, kwargs);
            Pickler { core: Rc::new(PicklerCore::default()) }
        }

        #[proto(init)]
        fn __init__(
            slf: This<Py<Self>>,
            it: &mut Interp,
            #[kw] file: &Value,
            #[kw] protocol: Option<&Value>,
            #[kw]
            #[default(true)]
            fix_imports: bool,
            #[kw] buffer_callback: Option<&Value>,
        ) -> R<()> {
            let core = slf.0.borrow(it)?.core.clone();
            if core.write.borrow().is_some() {
                core.reset();
            }
            set_protocol(it, &core, protocol, fix_imports)?;
            match attr_opt(it, file, "write")? {
                Some(w) => *core.write.borrow_mut() = Some(w),
                None => return Err(it.type_error("file must have a 'write' attribute")),
            }
            set_buffer_callback(it, &core, buffer_callback)?;
            core.fast.set(0);
            core.fast_nesting.set(0);
            let me = slf.0.value();
            if let Some(f) = attr_opt(it, me, "persistent_id")? {
                *core.pers.borrow_mut() = Some(split_bound(f, me));
            }
            if core.dispatch_table.borrow().is_none() {
                let dt = attr_opt(it, me, "dispatch_table")?;
                *core.dispatch_table.borrow_mut() = dt;
            }
            Ok(())
        }

        /// Write a pickled representation of the given object to the open file.
        fn dump(slf: This<Py<Self>>, it: &mut Interp, obj: &Value) -> R<()> {
            let core = slf.0.borrow(it)?.core.clone();
            let sh = shared(it)?;
            if core.write.borrow().is_none() {
                let msg = not_init(it, "Pickler", slf.0.value());
                return Err(pickling_error(it, &sh, &msg));
            }
            core.out.borrow_mut().clear();
            let pk = Pk { p: &core, sh: &sh, slf: Some(slf.0.value()) };
            pk.dump(it, obj)?;
            pk.flush_to_file(it)
        }

        /// Clears the pickler's "memo".
        ///
        /// The memo is the data structure that remembers which objects the
        /// pickler has already seen, so that shared or recursive objects are
        /// pickled by reference and not by value.  This method is useful when
        /// re-using picklers.
        fn clear_memo(&self) {
            self.core.memo.borrow_mut().clear();
        }

        /// Returns size in memory, in bytes.
        #[method(name = "__sizeof__")]
        fn sizeof(&self) -> i64 {
            let memo = self.core.memo.borrow().capacity() as i64 * 32;
            let out = self.core.out.borrow().len() as i64;
            80 + memo + out
        }

        #[getter(name = "bin")]
        fn bin(&self) -> i64 {
            self.core.bin.get()
        }

        #[setter(name = "bin")]
        fn set_bin(&mut self, it: &mut Interp, value: &Value) -> R<()> {
            let v = it.index_of(value)?;
            self.core.bin.set(v);
            Ok(())
        }

        #[getter(name = "fast")]
        fn fast(&self) -> i64 {
            self.core.fast.get()
        }

        #[setter(name = "fast")]
        fn set_fast(&mut self, it: &mut Interp, value: &Value) -> R<()> {
            let v = it.index_of(value)?;
            self.core.fast.set(v);
            Ok(())
        }

        #[getter(name = "dispatch_table")]
        fn dispatch_table(&self, it: &mut Interp) -> R<Value> {
            match self.core.dispatch_table.borrow().clone() {
                Some(v) => Ok(v),
                None => Err(it.new_exc_str("AttributeError", "dispatch_table")),
            }
        }

        #[setter(name = "dispatch_table")]
        fn set_dispatch_table(&mut self, value: &Value) {
            *self.core.dispatch_table.borrow_mut() = Some(value.clone());
        }

        #[getter(name = "memo")]
        fn memo(&self, it: &mut Interp) -> Value {
            Py::new(it, PicklerMemoProxy { core: self.core.clone() }).into_value()
        }

        #[setter(name = "memo")]
        fn set_memo(&mut self, it: &mut Interp, value: &Value) -> R<()> {
            let mut fresh: std::collections::HashMap<usize, (usize, Value)> = std::collections::HashMap::new();
            if let Some(proxy) = Py::<PicklerMemoProxy>::from_value(it, value) {
                let other = proxy.borrow(it)?.core.clone();
                fresh = other.memo.borrow().clone();
            } else if let Some(d) = as_dict_obj(value) {
                let entries: Vec<Value> = match crate::containers::pydict_of(d) {
                    Some(pd) => pd.borrow().iter().map(|e| e.val.clone()).collect(),
                    None => Vec::new(),
                };
                for v in entries {
                    let Some(pair) = v.tuple_items().filter(|t| t.len() == 2) else {
                        return Err(it.type_error("'memo' values must be 2-item tuples"));
                    };
                    let id = it.index_of(&pair[0])?;
                    if id < 0 {
                        return Err(it.value_error("memo id must be non-negative"));
                    }
                    fresh.insert(it.id_of(&pair[1]), (id as usize, pair[1].clone()));
                }
            } else {
                let t = it.tp_name_of(value);
                return Err(it.type_error(&format!("'memo' attribute must be a PicklerMemoProxy object or dict, not {}", t)));
            }
            *self.core.memo.borrow_mut() = fresh;
            Ok(())
        }

        #[getter(name = "persistent_id")]
        fn persistent_id(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            let core = slf.0.borrow(it)?.core.clone();
            let pers = core.pers.borrow().clone();
            match pers {
                Some(f) => Ok(rebuild_bound(&f, slf.0.value())),
                None => Err(it.new_exc_str("AttributeError", "persistent_id")),
            }
        }

        #[setter(name = "persistent_id")]
        fn set_persistent_id(&mut self, it: &mut Interp, value: &Value) -> R<()> {
            if !it.is_callable(value) {
                return Err(it.type_error("persistent_id must be a callable taking one argument"));
            }
            *self.core.pers.borrow_mut() = Some((value.clone(), false));
            Ok(())
        }
    }

    #[class(name = "UnpicklerMemoProxy", skip(py), hint(py(unhashable)))]
    pub struct UnpicklerMemoProxy {
        core: Rc<UnpicklerCore>,
    }

    fn unpickler_memo_copy(it: &mut Interp, core: &UnpicklerCore) -> R<Value> {
        let dict = super::shared::new_dict();
        for (i, v) in core.memo_entries() {
            it.dict_set(&dict, Value::Int(i as i64), v)?;
        }
        Ok(Value::Obj(dict))
    }

    #[methods]
    impl UnpicklerMemoProxy {
        /// Remove all items from memo.
        fn clear(&self) {
            self.core.reset_memo();
        }

        /// Copy the memo to a new object.
        fn copy(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            let core = slf.0.borrow(it)?.core.clone();
            unpickler_memo_copy(it, &core)
        }

        /// Implement pickling support.
        #[method(name = "__reduce__")]
        fn reduce(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            let core = slf.0.borrow(it)?.core.clone();
            let contents = unpickler_memo_copy(it, &core)?;
            Ok(Value::tuple(vec![Value::Obj(it.types.dict.clone()), Value::tuple(vec![contents])]))
        }
    }

    /// This takes a binary file for reading a pickle data stream.
    ///
    /// The protocol version of the pickle is detected automatically, so no
    /// protocol argument is needed.  Bytes past the pickled object's
    /// representation are ignored.
    ///
    /// The argument *file* must have two methods, a read() method that takes
    /// an integer argument, and a readline() method that requires no
    /// arguments.  Both methods should return bytes.  Thus *file* can be a
    /// binary file object opened for reading, an io.BytesIO object, or any
    /// other custom object that meets this interface.
    ///
    /// Optional keyword arguments are *fix_imports*, *encoding* and *errors*,
    /// which are used to control compatibility support for pickle stream
    /// generated by Python 2.  If *fix_imports* is True, pickle will try to
    /// map the old Python 2 names to the new names used in Python 3.  The
    /// *encoding* and *errors* tell pickle how to decode 8-bit string
    /// instances pickled by Python 2; these default to 'ASCII' and 'strict',
    /// respectively.  The *encoding* can be 'bytes' to read these 8-bit
    /// string instances as bytes objects.
    #[class(name = "Unpickler")]
    pub struct Unpickler {
        core: Rc<UnpicklerCore>,
    }

    #[methods]
    impl Unpickler {
        #[constructor]
        fn new(#[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> Unpickler {
            let _ = (args, kwargs);
            Unpickler { core: Rc::new(UnpicklerCore::default()) }
        }

        #[proto(init)]
        fn __init__(
            slf: This<Py<Self>>,
            it: &mut Interp,
            #[kw] file: &Value,
            #[kwonly]
            #[default(true)]
            fix_imports: bool,
            #[kwonly] encoding: Option<&Value>,
            #[kwonly] errors: Option<&Value>,
            #[kwonly] buffers: Option<&Value>,
        ) -> R<()> {
            let core = slf.0.borrow(it)?.core.clone();
            if core.is_initialised() {
                core.clear();
            }
            set_input_stream(it, &core, file)?;
            let enc = str_arg(it, encoding, "ASCII", "Unpickler", "encoding")?;
            let err = str_arg(it, errors, "strict", "Unpickler", "errors")?;
            core.set_encoding(&enc, &err);
            set_buffers(it, &core, buffers)?;
            core.fix_imports.set(fix_imports);
            let me = slf.0.value();
            if let Some(f) = attr_opt(it, me, "persistent_load")? {
                *core.pers.borrow_mut() = Some(split_bound(f, me));
            }
            core.stack.borrow_mut().reset();
            core.reset_memo();
            core.proto.set(0);
            Ok(())
        }

        /// Load a pickle.
        ///
        /// Read a pickled object representation from the open file object given
        /// in the constructor, and return the reconstituted object hierarchy
        /// specified therein.
        fn load(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            let core = slf.0.borrow(it)?.core.clone();
            let sh = shared(it)?;
            if !core.is_initialised() {
                let msg = not_init(it, "Unpickler", slf.0.value());
                return Err(unpickling_error(it, &sh, &msg));
            }
            Ld { u: &core, sh: &sh, slf: Some(slf.0.value()) }.load(it)
        }

        /// Return an object from a specified module.
        ///
        /// If necessary, the module will be imported. Subclasses may override
        /// this method (e.g. to restrict unpickling of arbitrary classes and
        /// functions).
        ///
        /// This method is called whenever a class or a function object is
        /// needed.  Both arguments passed are str objects.
        fn find_class(slf: This<Py<Self>>, it: &mut Interp, module_name: &Value, global_name: &Value) -> R<Value> {
            let core = slf.0.borrow(it)?.core.clone();
            let sh = shared(it)?;
            find_class_impl(it, &sh, core.proto.get(), core.fix_imports.get(), module_name, global_name)
        }

        /// Returns size in memory, in bytes.
        #[method(name = "__sizeof__")]
        fn sizeof(&self) -> i64 {
            let memo = self.core.memo_size() as i64 * 8;
            let input = self.core.input.borrow().len() as i64;
            80 + memo + input
        }

        #[getter(name = "memo")]
        fn memo(&self, it: &mut Interp) -> Value {
            Py::new(it, UnpicklerMemoProxy { core: self.core.clone() }).into_value()
        }

        #[setter(name = "memo")]
        fn set_memo(&mut self, it: &mut Interp, value: &Value) -> R<()> {
            let mut entries: Vec<(usize, Value)> = Vec::new();
            if let Some(proxy) = Py::<UnpicklerMemoProxy>::from_value(it, value) {
                let other = proxy.borrow(it)?.core.clone();
                entries = other.memo_entries();
            } else if let Some(d) = as_dict_obj(value) {
                let items: Vec<(Value, Value)> = match crate::containers::pydict_of(d) {
                    Some(pd) => pd.borrow().iter().map(|e| (e.key.clone(), e.val.clone())).collect(),
                    None => Vec::new(),
                };
                for (k, v) in items {
                    if !k.is_int_like() {
                        return Err(it.type_error("memo key must be integers"));
                    }
                    let idx = match k.as_bigint().and_then(|b| b.to_i64()) {
                        Some(i) => i,
                        None => return Err(it.overflow_err("Python int too large to convert to C ssize_t")),
                    };
                    if idx < 0 {
                        return Err(it.value_error("memo key must be positive integers."));
                    }
                    entries.push((idx as usize, v));
                }
            } else {
                let t = it.tp_name_of(value);
                return Err(it.type_error(&format!("'memo' attribute must be an UnpicklerMemoProxy object or dict, not {}", t)));
            }
            let core = self.core.clone();
            core.reset_memo();
            for (idx, v) in entries {
                core.memo_put(it, idx, v)?;
            }
            Ok(())
        }

        #[getter(name = "persistent_load")]
        fn persistent_load(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            let core = slf.0.borrow(it)?.core.clone();
            let pers = core.pers.borrow().clone();
            match pers {
                Some(f) => Ok(rebuild_bound(&f, slf.0.value())),
                None => Err(it.new_exc_str("AttributeError", "persistent_load")),
            }
        }

        #[setter(name = "persistent_load")]
        fn set_persistent_load(&mut self, it: &mut Interp, value: &Value) -> R<()> {
            if !it.is_callable(value) {
                return Err(it.type_error("persistent_load must be a callable taking one argument"));
            }
            *self.core.pers.borrow_mut() = Some((value.clone(), false));
            Ok(())
        }
    }

    /// Write a pickled representation of obj to the open file object file.
    ///
    /// This is equivalent to ``Pickler(file, protocol).dump(obj)``, but may
    /// be more efficient.
    ///
    /// The optional *protocol* argument tells the pickler to use the given
    /// protocol; supported protocols are 0, 1, 2, 3, 4 and 5.  The default
    /// protocol is 4. It was introduced in Python 3.4, and is incompatible
    /// with previous versions.
    ///
    /// Specifying a negative protocol version selects the highest protocol
    /// version supported.  The higher the protocol used, the more recent the
    /// version of Python needed to read the pickle produced.
    ///
    /// The *file* argument must have a write() method that accepts a single
    /// bytes argument.  It can thus be a file object opened for binary
    /// writing, an io.BytesIO instance, or any other custom object that meets
    /// this interface.
    ///
    /// If *fix_imports* is True and protocol is less than 3, pickle will try
    /// to map the new Python 3 names to the old module names used in Python
    /// 2, so that the pickle data stream is readable with Python 2.
    ///
    /// If *buffer_callback* is None (the default), buffer views are serialized
    /// into *file* as part of the pickle stream.  It is an error if
    /// *buffer_callback* is not None and *protocol* is None or smaller than 5.
    #[op]
    fn dump(
        it: &mut Interp,
        #[kw] obj: &Value,
        #[kw] file: &Value,
        #[kw] protocol: Option<&Value>,
        #[kwonly]
        #[default(true)]
        fix_imports: bool,
        #[kwonly] buffer_callback: Option<&Value>,
    ) -> R<()> {
        let sh = shared(it)?;
        let core = PicklerCore::default();
        set_protocol(it, &core, protocol, fix_imports)?;
        match attr_opt(it, file, "write")? {
            Some(w) => *core.write.borrow_mut() = Some(w),
            None => return Err(it.type_error("file must have a 'write' attribute")),
        }
        set_buffer_callback(it, &core, buffer_callback)?;
        let pk = Pk { p: &core, sh: &sh, slf: None };
        pk.dump(it, obj)?;
        pk.flush_to_file(it)
    }

    /// Return the pickled representation of the object as a bytes object.
    ///
    /// The optional *protocol* argument tells the pickler to use the given
    /// protocol; supported protocols are 0, 1, 2, 3, 4 and 5.  The default
    /// protocol is 4. It was introduced in Python 3.4, and is incompatible
    /// with previous versions.
    ///
    /// Specifying a negative protocol version selects the highest protocol
    /// version supported.  The higher the protocol used, the more recent the
    /// version of Python needed to read the pickle produced.
    ///
    /// If *fix_imports* is True and *protocol* is less than 3, pickle will
    /// try to map the new Python 3 names to the old module names used in
    /// Python 2, so that the pickle data stream is readable with Python 2.
    ///
    /// If *buffer_callback* is None (the default), buffer views are serialized
    /// into *file* as part of the pickle stream.  It is an error if
    /// *buffer_callback* is not None and *protocol* is None or smaller than 5.
    #[op]
    fn dumps(
        it: &mut Interp,
        #[kw] obj: &Value,
        #[kw] protocol: Option<&Value>,
        #[kwonly]
        #[default(true)]
        fix_imports: bool,
        #[kwonly] buffer_callback: Option<&Value>,
    ) -> R<Value> {
        let sh = shared(it)?;
        let core = PicklerCore::default();
        set_protocol(it, &core, protocol, fix_imports)?;
        set_buffer_callback(it, &core, buffer_callback)?;
        let pk = Pk { p: &core, sh: &sh, slf: None };
        pk.dump(it, obj)?;
        let out = core.out.borrow_mut().take();
        Ok(Value::bytes(out))
    }

    /// Read and return an object from the pickle data stored in a file.
    ///
    /// This is equivalent to ``Unpickler(file).load()``, but may be more
    /// efficient.
    ///
    /// The protocol version of the pickle is detected automatically, so no
    /// protocol argument is needed.  Bytes past the pickled object's
    /// representation are ignored.
    ///
    /// The argument *file* must have two methods, a read() method that takes
    /// an integer argument, and a readline() method that requires no
    /// arguments.  Both methods should return bytes.  Thus *file* can be a
    /// binary file object opened for reading, an io.BytesIO object, or any
    /// other custom object that meets this interface.
    ///
    /// Optional keyword arguments are *fix_imports*, *encoding* and *errors*,
    /// which are used to control compatibility support for pickle stream
    /// generated by Python 2.  If *fix_imports* is True, pickle will try to
    /// map the old Python 2 names to the new names used in Python 3.  The
    /// *encoding* and *errors* tell pickle how to decode 8-bit string
    /// instances pickled by Python 2; these default to 'ASCII' and 'strict',
    /// respectively.  The *encoding* can be 'bytes' to read these 8-bit
    /// string instances as bytes objects.
    #[op]
    fn load(
        it: &mut Interp,
        #[kw] file: &Value,
        #[kwonly]
        #[default(true)]
        fix_imports: bool,
        #[kwonly] encoding: Option<&Value>,
        #[kwonly] errors: Option<&Value>,
        #[kwonly] buffers: Option<&Value>,
    ) -> R<Value> {
        let sh = shared(it)?;
        let core = UnpicklerCore::default();
        set_input_stream(it, &core, file)?;
        let enc = str_arg(it, encoding, "ASCII", "load", "encoding")?;
        let err = str_arg(it, errors, "strict", "load", "errors")?;
        core.set_encoding(&enc, &err);
        set_buffers(it, &core, buffers)?;
        core.fix_imports.set(fix_imports);
        Ld { u: &core, sh: &sh, slf: None }.load(it)
    }

    /// Read and return an object from the given pickle data.
    ///
    /// The protocol version of the pickle is detected automatically, so no
    /// protocol argument is needed.  Bytes past the pickled object's
    /// representation are ignored.
    ///
    /// Optional keyword arguments are *fix_imports*, *encoding* and *errors*,
    /// which are used to control compatibility support for pickle stream
    /// generated by Python 2.  If *fix_imports* is True, pickle will try to
    /// map the old Python 2 names to the new names used in Python 3.  The
    /// *encoding* and *errors* tell pickle how to decode 8-bit string
    /// instances pickled by Python 2; these default to 'ASCII' and 'strict',
    /// respectively.  The *encoding* can be 'bytes' to read these 8-bit
    /// string instances as bytes objects.
    #[op]
    fn loads(
        it: &mut Interp,
        data: &Value,
        #[kwonly]
        #[default(true)]
        fix_imports: bool,
        #[kwonly] encoding: Option<&Value>,
        #[kwonly] errors: Option<&Value>,
        #[kwonly] buffers: Option<&Value>,
    ) -> R<Value> {
        let sh = shared(it)?;
        let core = UnpicklerCore::default();
        set_string_input(it, &core, data)?;
        let enc = str_arg(it, encoding, "ASCII", "loads", "encoding")?;
        let err = str_arg(it, errors, "strict", "loads", "errors")?;
        core.set_encoding(&enc, &err);
        set_buffers(it, &core, buffers)?;
        core.fix_imports.set(fix_imports);
        Ld { u: &core, sh: &sh, slf: None }.load(it)
    }
}
