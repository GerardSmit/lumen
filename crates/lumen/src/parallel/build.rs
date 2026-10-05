use super::parcel::{Buffer, HeapGuard, Intrinsic};
use super::{Limits, Parcel};
use crate::{
    interpreter::Interp,
    value::{Exotic, Gc, Object, Value, set_data},
};
use std::collections::HashMap;

pub(super) struct Builder {
    pub(super) memo: HashMap<usize, Gc>,
    limits: Limits,
    transfer: std::collections::HashSet<usize>,
    pub(super) class_objects: std::collections::HashSet<usize>,
    pub(super) class_scopes: std::collections::HashSet<usize>,
    pub(super) environments: HashMap<usize, crate::interpreter::Env>,
    pub(super) private_keys: HashMap<String, String>,
    /// Apply browser structured-clone restrictions rather than the broader
    /// graph migration rules used by `Lumen.parallel`.
    structured: bool,
    host_attachment: Option<Box<dyn FnMut(&Value) -> Option<usize>>>,
    // Object and scope memo roots must drop before the heap on clone failure.
    pub(super) parcel: Parcel,
}
impl Parcel {
    /// Copy a supported graph into an exclusively owned heap. Getters run on the sender.
    pub fn build(interp: &mut Interp, value: &Value, limits: Limits) -> Result<Self, Value> {
        Self::build_with_transfer(interp, value, &[], limits)
    }
    /// Build the graph, then detach validated buffers without copying their backing bytes.
    pub fn build_with_transfer(
        interp: &mut Interp,
        value: &Value,
        transfer: &[Value],
        limits: Limits,
    ) -> Result<Self, Value> {
        Self::build_checked(interp, value, transfer, limits, |_| Ok(()))
    }
    /// Clone a browser message graph, optionally transferring attached buffers.
    pub fn build_structured_with_transfer(
        interp: &mut Interp,
        value: &Value,
        transfer: &[Value],
        limits: Limits,
    ) -> Result<Self, Value> {
        Self::build_checked_mode(interp, value, transfer, limits, true, |_| Ok(()))
    }
    /// Clone a browser message graph and validate the sender's queue/channel
    /// reservation after getters run but before transferables are detached.
    pub fn build_structured_checked(
        interp: &mut Interp,
        value: &Value,
        transfer: &[Value],
        limits: Limits,
        validate: impl FnOnce(&mut Interp) -> Result<(), Value>,
    ) -> Result<Self, Value> {
        Self::build_checked_mode(interp, value, transfer, limits, true, validate)
    }
    /// Clone a structured message carrying host-owned transferable objects.
    /// `attachment_index` must return an index only for an object present in
    /// the validated transfer list. Attachment capability IDs are carried in
    /// the parcel's side table and never become sender-controlled JS values.
    pub fn build_structured_checked_with_attachments(
        interp: &mut Interp,
        value: &Value,
        transfer: &[Value],
        attachments: Vec<(u64, u8)>,
        limits: Limits,
        attachment_index: Box<dyn FnMut(&Value) -> Option<usize>>,
        validate: impl FnOnce(&mut Interp) -> Result<(), Value>,
    ) -> Result<Self, Value> {
        Self::build_checked_mode_with_attachments(
            interp,
            value,
            transfer,
            limits,
            true,
            Some(attachment_index),
            attachments,
            validate,
        )
    }
    pub(crate) fn build_checked(
        interp: &mut Interp,
        value: &Value,
        transfer: &[Value],
        limits: Limits,
        validate: impl FnOnce(&mut Interp) -> Result<(), Value>,
    ) -> Result<Self, Value> {
        Self::build_checked_mode(interp, value, transfer, limits, false, validate)
    }
    fn build_checked_mode(
        interp: &mut Interp,
        value: &Value,
        transfer: &[Value],
        limits: Limits,
        structured: bool,
        validate: impl FnOnce(&mut Interp) -> Result<(), Value>,
    ) -> Result<Self, Value> {
        Self::build_checked_mode_with_attachments(
            interp,
            value,
            transfer,
            limits,
            structured,
            None,
            Vec::new(),
            validate,
        )
    }
    fn build_checked_mode_with_attachments(
        interp: &mut Interp,
        value: &Value,
        transfer: &[Value],
        limits: Limits,
        structured: bool,
        host_attachment: Option<Box<dyn FnMut(&Value) -> Option<usize>>>,
        attachments: Vec<(u64, u8)>,
        validate: impl FnOnce(&mut Interp) -> Result<(), Value>,
    ) -> Result<Self, Value> {
        let mut pointers = std::collections::HashSet::new();
        for buffer in transfer {
            if !interp.is_transferable_array_buffer(buffer)
                || !pointers.insert(Gc::as_ptr(buffer.as_obj().unwrap()) as usize)
            {
                return Err(interp.make_error(
                    "DataCloneError",
                    "transfer requires distinct attached ArrayBuffers",
                ));
            }
        }
        let mut builder = Builder {
            parcel: Self::empty(),
            memo: HashMap::new(),
            limits,
            transfer: pointers,
            class_objects: Default::default(),
            class_scopes: Default::default(),
            environments: Default::default(),
            private_keys: Default::default(),
            structured,
            host_attachment,
        };
        builder.parcel.attachments = attachments;
        builder.parcel.bytes = builder
            .parcel
            .bytes
            .saturating_add(builder.parcel.attachments.len() * std::mem::size_of::<(u64, u8)>());
        if builder.parcel.bytes > builder.limits.bytes {
            return Err(interp.make_error("RangeError", "parcel limit exceeded"));
        }
        builder.parcel.root = builder.copy(interp, value, 0)?;
        // Getter code may have detached a buffer. Validate all entries before
        // detaching any; clone failure must leave the other buffers attached.
        for buffer in transfer {
            if !interp.is_transferable_array_buffer(buffer) {
                return Err(interp.make_error(
                    "DataCloneError",
                    "transfer buffer was detached during cloning",
                ));
            }
        }
        validate(interp)?;
        for pointer in &builder.transfer {
            let bytes = interp
                .array_buffers
                .remove(pointer)
                .expect("validated buffer")
                .detach()
                .expect("validated buffer is attached and unpinned");
            if let Some(object) = builder.memo.get(pointer) {
                builder
                    .parcel
                    .side
                    .buffers
                    .get_mut(&(Gc::as_ptr(object) as usize))
                    .expect("buffer metadata")
                    .bytes = bytes;
            }
        }
        Ok(builder.parcel)
    }
}
impl Builder {
    pub(super) fn charge(&mut self, interp: &Interp, bytes: usize) -> Result<(), Value> {
        self.parcel.bytes = self.parcel.bytes.saturating_add(bytes);
        if self.parcel.bytes > self.limits.bytes || self.parcel.objects > self.limits.objects {
            return Err(interp.make_error("RangeError", "parcel limit exceeded"));
        }
        Ok(())
    }
    pub(super) fn copy(
        &mut self,
        interp: &mut Interp,
        value: &Value,
        depth: usize,
    ) -> Result<Value, Value> {
        if depth > 256 {
            return Err(interp.make_error("RangeError", "parcel graph is too deep"));
        }
        self.charge(interp, std::mem::size_of::<Value>())?;
        Ok(match value {
            Value::Empty | Value::Undefined => Value::Undefined,
            Value::Null => Value::Null,
            Value::Bool(v) => Value::Bool(*v),
            Value::Num(v) => Value::Num(*v),
            Value::Str(text) => {
                self.charge(interp, text.len())?;
                Value::str(&**text)
            }
            Value::BigInt(number) => {
                let (negative, words) = number.words();
                self.charge(interp, words.len() * 8)?;
                Value::BigInt(crate::bigint::JsBigInt::from_words(
                    negative,
                    words.to_vec(),
                ))
            }
            Value::Sym(_) => {
                return Err(interp.make_error("DataCloneError", "Symbol cannot be cloned"));
            }
            Value::Obj(object) => {
                let pointer = Gc::as_ptr(object) as usize;
                if let Some(copied) = self.memo.get(&pointer) {
                    return Ok(Value::Obj(copied.clone()));
                }
                if let Some(index) = self
                    .host_attachment
                    .as_mut()
                    .and_then(|identify| identify(value))
                {
                    if index >= self.parcel.attachments.len() {
                        return Err(interp
                            .make_error("DataCloneError", "invalid host transferable reference"));
                    }
                    let copied = {
                        let _entered = HeapGuard::enter(&self.parcel.heap);
                        Object::new(None)
                    };
                    self.memo.insert(pointer, copied.clone());
                    self.parcel.protos.push((copied.clone(), Intrinsic::Object));
                    crate::value::set_data(
                        &copied,
                        "__lumenHostAttachmentIndex",
                        Value::Num(index as f64),
                    );
                    self.parcel.objects += 1;
                    self.charge(interp, std::mem::size_of::<Object>())?;
                    return Ok(Value::Obj(copied));
                }
                if self.structured
                    && (!matches!(object.borrow().call, crate::value::Callable::None)
                        || interp.class_info.contains_key(&pointer))
                {
                    return Err(interp.make_error("DataCloneError", "unsupported object"));
                }
                #[cfg(feature = "aot-native")]
                if self.structured && interp.native_classes.contains_key(&pointer) {
                    return Err(interp.make_error("DataCloneError", "unsupported object"));
                }
                #[cfg(feature = "aot-native")]
                let native_callable =
                    matches!(object.borrow().call, crate::value::Callable::Aot(_))
                        || interp.native_classes.contains_key(&pointer);
                #[cfg(not(feature = "aot-native"))]
                let native_callable = false;
                if interp.proxies.contains_key(&pointer)
                    || (!matches!(
                        object.borrow().call,
                        crate::value::Callable::None | crate::value::Callable::User(_)
                    ) && !native_callable
                        && !(self.class_objects.contains(&pointer)
                            && matches!(
                                object.borrow().call,
                                crate::value::Callable::AccessorGet(_)
                                    | crate::value::Callable::AccessorSet(_)
                            )))
                {
                    return Err(interp.make_error("DataCloneError", "unsupported object"));
                }
                interp.materialize(object);
                if interp.class_info.contains_key(&pointer) {
                    self.register_class(interp, object)?;
                }
                #[cfg(feature = "aot-native")]
                if let Some(class) = interp.native_classes.get(&pointer).cloned() {
                    self.register_native_class(interp, object, &class);
                }
                let array = object.borrow().exotic == Exotic::Array;
                let exotic = object.borrow().exotic;
                if matches!(exotic, Exotic::SymWrap | Exotic::Arguments) {
                    return Err(interp.make_error("DataCloneError", "unsupported object"));
                }
                let intrinsic = self.intrinsic(interp, object, pointer)?;
                self.parcel.objects += 1;
                self.charge(interp, std::mem::size_of::<Object>())?;
                let copied = {
                    let _entered = HeapGuard::enter(&self.parcel.heap);
                    if array {
                        Object::new_array_from_vec(None, Vec::new())
                    } else {
                        Object::new(None)
                    }
                };
                self.memo.insert(pointer, copied.clone());
                self.parcel.protos.push((copied.clone(), intrinsic));
                #[cfg(feature = "aot-native")]
                if let Some(class) = interp.native_classes.get(&pointer).cloned() {
                    self.copy_native_class(interp, &copied, &class, depth)?;
                }
                match &object.borrow().call {
                    crate::value::Callable::AccessorGet(key)
                    | crate::value::Callable::AccessorSet(key) => {
                        let key = self
                            .private_keys
                            .get(&***key)
                            .expect("class accessor backing key");
                        let key = std::rc::Rc::new(std::rc::Rc::<str>::from(key.as_str()));
                        copied.borrow_mut().call = if matches!(
                            object.borrow().call,
                            crate::value::Callable::AccessorGet(_)
                        ) {
                            crate::value::Callable::AccessorGet(key)
                        } else {
                            crate::value::Callable::AccessorSet(key)
                        };
                    }
                    _ => {}
                }
                let function = match &object.borrow().call {
                    crate::value::Callable::User(user) => {
                        Some((user.func.clone(), user.env.clone()))
                    }
                    _ => None,
                };
                if let Some((function, environment)) = function {
                    self.copy_function(interp, object, &copied, function, environment, depth)?;
                }
                #[cfg(feature = "aot-native")]
                {
                    let native = match &object.borrow().call {
                        crate::value::Callable::Aot(native) => Some((
                            native.program.clone(),
                            native.function_index,
                            native.env.clone(),
                        )),
                        _ => None,
                    };
                    if let Some((program, index, env)) = native {
                        let env = if std::rc::Rc::ptr_eq(&env, &interp.global_env) {
                            None
                        } else {
                            let mut scope = Some(env.clone());
                            while let Some(current) = scope {
                                if std::rc::Rc::ptr_eq(&current, &interp.global_env) {
                                    break;
                                }
                                self.class_scopes
                                    .insert(std::rc::Rc::as_ptr(&current) as usize);
                                scope = current.borrow().parent.clone();
                            }
                            Some(self.copy_environment(interp, &env, depth + 1)?)
                        };
                        self.parcel
                            .native_functions
                            .push(super::parcel::NativeFunction {
                                trusted_glue: program.trusted_glue,
                                object: copied.clone(),
                                bytes: program.bytes.clone(),
                                hash: program.image.blob_hash(),
                                index,
                                env,
                            });
                    }
                }
                self.copy_slots(interp, object, &copied, pointer, depth)?;
                let keys = if self.class_objects.contains(&pointer) {
                    object.borrow().props.iter().map(|(key, _)| key).collect()
                } else {
                    object.borrow().props.ordered_keys()
                };
                for key in keys {
                    if Interp::is_sym_key(&key) && !self.class_objects.contains(&pointer) {
                        continue;
                    }
                    if self.class_objects.contains(&pointer) {
                        self.charge(
                            interp,
                            key.len()
                                .saturating_add(std::mem::size_of::<crate::value::Property>()),
                        )?;
                        let property = object.borrow().props.get(&key).unwrap().clone();
                        let property = self.copy_descriptor(interp, &property, depth + 1)?;
                        if Interp::is_sym_key(&key) {
                            let name = interp
                                .wk_syms
                                .iter()
                                .find(|(_, _, symbol)| &**symbol == &*key)
                                .map(|(name, _, _)| *name)
                                .ok_or_else(|| {
                                    interp.make_error(
                                        "DataCloneError",
                                        "class has an unsupported symbol key",
                                    )
                                })?;
                            self.parcel
                                .symbol_keys
                                .push((copied.clone(), key.to_string(), name));
                        }
                        let key = self
                            .private_keys
                            .get(&*key)
                            .cloned()
                            .unwrap_or_else(|| key.to_string());
                        let _entered = HeapGuard::enter(&self.parcel.heap);
                        copied.borrow_mut().props.insert(key, property);
                        continue;
                    }
                    let enumerable = object
                        .borrow()
                        .props
                        .get(&key)
                        .is_some_and(|property| property.enumerable());
                    if !enumerable && !(array && &*key == "length") {
                        continue;
                    }
                    self.charge(
                        interp,
                        key.len()
                            .saturating_add(std::mem::size_of::<crate::value::Property>()),
                    )?;
                    // Getters execute with the sender current, outside the allocation guard.
                    let member = interp
                        .get_member(value, &key)
                        .map_err(|abrupt| match abrupt {
                            crate::interpreter::Abrupt::Throw(value) => value,
                            _ => interp.make_error("DataCloneError", "property read failed"),
                        })?;
                    let member = self.copy(interp, &member, depth + 1)?;
                    let _entered = HeapGuard::enter(&self.parcel.heap);
                    if array && &*key == "length" {
                        copied.borrow_mut().props.insert(
                            "length",
                            crate::value::Property::data(member, true, false, false),
                        );
                    } else {
                        set_data(&copied, &key, member);
                    }
                }
                if self.class_objects.contains(&pointer)
                    && (interp.class_info.contains_key(&pointer)
                        || native_callable
                        || matches!(object.borrow().call, crate::value::Callable::None))
                {
                    self.copy_class_prototype(interp, object, &copied, depth + 1)?;
                }
                Value::Obj(copied)
            }
        })
    }

    fn intrinsic(&self, interp: &Interp, object: &Gc, pointer: usize) -> Result<Intrinsic, Value> {
        #[cfg(feature = "aot-native")]
        {
            if interp.native_classes.contains_key(&pointer) {
                return Ok(Intrinsic::Function);
            }
            if let crate::value::Callable::Aot(native) = &object.borrow().call {
                let flags = native.program.metadata.functions[native.function_index as usize].flags;
                return Ok(match (flags & 8 != 0, flags & 4 != 0) {
                    (true, true) => Intrinsic::Extra("%AsyncGeneratorFunction.prototype%"),
                    (true, false) => Intrinsic::Extra("%AsyncFunction.prototype%"),
                    (false, true) => Intrinsic::Extra("%GeneratorFunction.prototype%"),
                    _ => Intrinsic::Function,
                });
            }
        }
        if self.class_objects.contains(&pointer)
            && matches!(
                object.borrow().call,
                crate::value::Callable::AccessorGet(_) | crate::value::Callable::AccessorSet(_)
            )
        {
            return Ok(Intrinsic::Function);
        }
        if self.class_objects.contains(&pointer)
            && matches!(object.borrow().call, crate::value::Callable::None)
        {
            return Ok(Intrinsic::Object);
        }
        if let crate::value::Callable::User(user) = &object.borrow().call {
            return Ok(match (user.func.is_async, user.func.is_generator) {
                (true, true) => Intrinsic::Extra("%AsyncGeneratorFunction.prototype%"),
                (true, false) => Intrinsic::Extra("%AsyncFunction.prototype%"),
                (false, true) => Intrinsic::Extra("%GeneratorFunction.prototype%"),
                _ => Intrinsic::Function,
            });
        }
        if interp.generators.contains_key(&pointer)
            || interp.is_weak_object(pointer)
            || interp.module_ns.contains_key(&pointer)
            || object.borrow().props.iter().any(|(key, _)| {
                Interp::is_private_key(&key)
                    && !self.class_objects.contains(&pointer)
                    && &*key != crate::value::EXOTIC_SLOT
                    && !(object.borrow().exotic == Exotic::Error
                        && matches!(&*key, "#\u{0}rawstack" | "#\u{0}stack"))
            })
        {
            return Err(
                interp.make_error("DataCloneError", "object has unsupported internal slots")
            );
        }
        if let Some(data) = interp.map_data.get(&pointer) {
            use crate::builtins::collection_data::CollectionKind;
            return match data.kind() {
                CollectionKind::Map => Ok(Intrinsic::Extra("Map")),
                CollectionKind::Set => Ok(Intrinsic::Extra("Set")),
                _ => Err(interp.make_error("DataCloneError", "weak collections cannot be cloned")),
            };
        }
        if let Some(info) = interp.typed_arrays.get(&pointer) {
            return Ok(Intrinsic::Extra(info.kind.name()));
        }
        if interp.data_views.contains_key(&pointer) {
            return Ok(Intrinsic::Extra("DataView"));
        }
        if interp.regexps.contains_key(&pointer) {
            return Ok(Intrinsic::Extra("RegExp"));
        }
        if interp.shared_buffers.contains_key(&pointer) {
            return Ok(Intrinsic::Extra("SharedArrayBuffer"));
        }
        if interp.array_buffers.contains_key(&pointer) {
            return Ok(Intrinsic::Extra("ArrayBuffer"));
        }
        if object.borrow().props.contains("\u{0}ab_max_byte_length") {
            return Err(
                interp.make_error("DataCloneError", "detached ArrayBuffer cannot be cloned")
            );
        }
        if object.borrow().exotic == Exotic::Date {
            return Ok(Intrinsic::Extra("Date"));
        }
        let object = object.borrow();
        let kind = match object.exotic {
            Exotic::Array => Intrinsic::Array,
            Exotic::StrWrap => Intrinsic::String,
            Exotic::NumWrap => Intrinsic::Number,
            Exotic::BoolWrap => Intrinsic::Boolean,
            Exotic::BigIntWrap => Intrinsic::Extra("BigInt"),
            Exotic::Error => {
                let name = interp
                    .error_protos
                    .iter()
                    .find(|(_, proto)| {
                        object
                            .proto
                            .as_ref()
                            .is_some_and(|p| Gc::as_ptr(p) == Gc::as_ptr(proto))
                    })
                    .map(|(name, _)| *name)
                    .unwrap_or("Error");
                Intrinsic::Error(name)
            }
            _ => {
                if let Some((name, _)) = interp.extra_protos.iter().find(|(_, proto)| {
                    object
                        .proto
                        .as_ref()
                        .is_some_and(|p| Gc::as_ptr(p) == Gc::as_ptr(proto))
                }) {
                    match *name {
                        "ArrayBuffer"
                        | "SharedArrayBuffer"
                        | "DataView"
                        | "RegExp"
                        | "Date"
                        | "Int8Array"
                        | "Uint8Array"
                        | "Uint8ClampedArray"
                        | "Int16Array"
                        | "Uint16Array"
                        | "Int32Array"
                        | "Uint32Array"
                        | "Float16Array"
                        | "Float32Array"
                        | "Float64Array"
                        | "BigInt64Array"
                        | "BigUint64Array"
                        | "%GeneratorPrototype%"
                        | "%AsyncGeneratorPrototype%" => Intrinsic::Extra(name),
                        _ => {
                            return Err(
                                interp.make_error("DataCloneError", "unsupported builtin object")
                            );
                        }
                    }
                } else {
                    Intrinsic::Object
                }
            }
        };
        Ok(kind)
    }

    fn copy_function(
        &mut self,
        interp: &mut Interp,
        source: &Gc,
        target: &Gc,
        function: std::rc::Rc<crate::ast::Function>,
        environment: crate::interpreter::Env,
        depth: usize,
    ) -> Result<(), Value> {
        let class_member = self.class_objects.contains(&(Gc::as_ptr(source) as usize));
        if function.is_method && !class_member {
            return Err(
                interp.make_error("DataCloneError", "methods and accessors cannot be cloned")
            );
        }
        let capture = |name: &str| {
            interp.make_error(
                "TypeError",
                format!(
                    "parallel function captures '{name}' from the outer scope; pass it in args"
                ),
            )
        };
        if environment.borrow().under_with() {
            return Err(capture("with"));
        }
        if function.is_arrow {
            let flags = function.scan_flags();
            for (flag, name) in [
                (crate::ast::SCAN_THIS, "this"),
                (crate::ast::SCAN_ARGUMENTS, "arguments"),
                (crate::ast::SCAN_NEW_TARGET, "new.target"),
            ] {
                if flags & flag != 0 {
                    return Err(capture(name));
                }
            }
        }
        let names = crate::bytecode::function_free_identifiers(&function).ok_or_else(|| {
            interp.make_error(
                "TypeError",
                "parallel function contains dynamic scope; pass helpers in args",
            )
        })?;
        for name in names {
            let mut scope = Some(environment.clone());
            let mut class_binding = false;
            while let Some(current) = scope {
                if current.borrow().vars.contains_key(&name) {
                    if !self
                        .class_scopes
                        .contains(&(std::rc::Rc::as_ptr(&current) as usize))
                    {
                        return Err(capture(&name));
                    }
                    class_binding = true;
                    break;
                }
                scope = current.borrow().parent.clone();
            }
            if !class_binding
                && interp
                    .global
                    .borrow()
                    .props
                    .get(&name)
                    .is_some_and(|property| property.enumerable())
            {
                return Err(capture(&name));
            }
        }
        let text = match &function.source {
            crate::ast::FnSource::Range { src, .. } => &**src,
            _ => function.source.as_str().unwrap_or(""),
        };
        let snapshot =
            crate::snapshot::encode(&[crate::ast::Stmt::FuncDecl(function.clone())], text);
        self.charge(interp, snapshot.len().saturating_add(text.len()))?;
        let decoded = {
            let _entered = HeapGuard::enter(&self.parcel.heap);
            crate::snapshot::decode(&snapshot, text)
        }
        .map_err(|message| interp.make_error("DataCloneError", message))?;
        let crate::ast::Stmt::FuncDecl(decoded) =
            decoded.into_iter().next().expect("function snapshot")
        else {
            unreachable!()
        };
        let copied_env = if class_member {
            Some(self.copy_environment(interp, &environment, depth + 1)?)
        } else {
            None
        };
        self.parcel
            .functions
            .push((target.clone(), decoded, copied_env));
        if interp
            .class_info
            .contains_key(&(Gc::as_ptr(source) as usize))
        {
            self.copy_class_info(interp, source, target, &function, depth + 1)?;
        }
        target.borrow_mut().is_constructor = source.borrow().is_constructor;
        for key in ["name", "length", "prototype"] {
            if !source.borrow().props.contains(key) {
                continue;
            }
            let value = interp
                .get_member(&Value::Obj(source.clone()), key)
                .map_err(|abrupt| match abrupt {
                    crate::interpreter::Abrupt::Throw(value) => value,
                    _ => interp.make_error("DataCloneError", "function property read failed"),
                })?;
            let value = self.copy(interp, &value, depth + 1)?;
            let _entered = HeapGuard::enter(&self.parcel.heap);
            if key == "prototype" {
                if let Value::Obj(prototype) = &value {
                    prototype.borrow_mut().props.insert(
                        "constructor",
                        crate::value::Property::builtin(Value::Obj(target.clone())),
                    );
                }
            }
            target
                .borrow_mut()
                .props
                .insert(key, crate::value::Property::builtin(value));
        }
        Ok(())
    }

    fn copy_slots(
        &mut self,
        interp: &mut Interp,
        source: &Gc,
        target: &Gc,
        pointer: usize,
        depth: usize,
    ) -> Result<(), Value> {
        let target_pointer = Gc::as_ptr(target) as usize;
        let exotic = source.borrow().exotic;
        let payload = source.borrow().exotic_payload();
        if exotic == Exotic::Error {
            // Raw trace frames retain sender functions. Only the formatted stack crosses.
            let _entered = HeapGuard::enter(&self.parcel.heap);
            target
                .borrow_mut()
                .set_exotic(Exotic::Error, Some(Value::Undefined));
        } else if let Some(payload) = payload {
            let payload = self.copy(interp, &payload, depth + 1)?;
            let _entered = HeapGuard::enter(&self.parcel.heap);
            target.borrow_mut().set_exotic(exotic, Some(payload));
        }
        if let Some(data) = interp.map_data.get(&pointer) {
            let kind = data.kind();
            self.charge(
                interp,
                data.len()
                    .saturating_mul(std::mem::size_of::<(Value, Value)>()),
            )?;
            let entries = data
                .iter()
                .map(|(key, value)| (key.unpack(), value.unpack()))
                .collect::<Vec<_>>();
            let mut copied = crate::builtins::collection_data::CollectionData::new(kind);
            for (key, value) in entries {
                let key = self.copy(interp, &key, depth + 1)?;
                let value = self.copy(interp, &value, depth + 1)?;
                copied.insert(key, value);
            }
            self.parcel.side.maps.insert(target_pointer, copied);
        }
        if let Some(shared) = interp.export_shared_array_buffer(&Value::Obj(source.clone()))? {
            self.parcel.side.shared.insert(target_pointer, shared);
        } else if let Some(buffer) = interp.array_buffers.get(&pointer) {
            let size = buffer.len();
            let bytes = if self.transfer.contains(&pointer) {
                Vec::new()
            } else {
                buffer
                    .try_bytes()
                    .map_err(|_| interp.make_error("DataCloneError", "buffer is borrowed"))?
                    .to_vec()
            };
            self.charge(interp, size)?;
            let metadata = Buffer {
                bytes,
                max_len: buffer.is_resizable().then(|| buffer.max_len()),
                readonly: buffer.is_readonly(),
            };
            self.parcel.side.buffers.insert(target_pointer, metadata);
        }
        if let Some(mut info) = interp.typed_arrays.get(&pointer).copied() {
            let buffer = interp.ta_buffer[&pointer].clone();
            let buffer = self.copy(interp, &buffer, depth + 1)?;
            info.buffer = Gc::as_ptr(buffer.as_obj().unwrap()) as usize;
            self.parcel.side.typed.insert(target_pointer, info);
            self.parcel.side.ta_buffer.insert(target_pointer, buffer);
            target.borrow().ic_plain.set(false);
        }
        if let Some((buffer, offset, length, track)) = interp.data_views.get(&pointer).copied() {
            let buffer = Value::Obj(interp.gc_pins[&buffer].clone());
            let buffer = self.copy(interp, &buffer, depth + 1)?;
            self.parcel.side.views.insert(
                target_pointer,
                (
                    Gc::as_ptr(buffer.as_obj().unwrap()) as usize,
                    offset,
                    length,
                    track,
                ),
            );
            let _entered = HeapGuard::enter(&self.parcel.heap);
            target
                .borrow_mut()
                .props
                .insert("\u{0}dv_buffer", crate::value::Property::builtin(buffer));
        }
        if let Some(regex) = interp.regexps.get(&pointer) {
            let regex = crate::regex::Regex::new(&regex.source, &regex.flags)
                .map_err(|message| interp.make_error("DataCloneError", message))?;
            self.parcel
                .side
                .regexps
                .insert(target_pointer, std::rc::Rc::new(regex));
        }
        // Hidden builtin properties carry Date/buffer metadata; Error fields are
        // non-enumerable. These are copied separately from ordinary own keys.
        let keys = match exotic {
            Exotic::Error => vec!["name", "message", "stack", "cause", "code", "errors"],
            _ => {
                let mut keys = Vec::new();
                if interp.array_buffers.contains_key(&pointer) {
                    keys.extend([
                        "\u{0}ab_max_byte_length",
                        "\u{0}ab_resizable",
                        "\u{0}sab_id",
                    ]);
                }
                if interp.regexps.contains_key(&pointer) {
                    keys.push("lastIndex");
                }
                keys
            }
        };
        for key in keys {
            if source.borrow().props.contains(key) || (exotic == Exotic::Error && key == "stack") {
                let value = interp
                    .get_member(&Value::Obj(source.clone()), key)
                    .map_err(|abrupt| match abrupt {
                        crate::interpreter::Abrupt::Throw(value) => value,
                        _ => interp.make_error("DataCloneError", "internal property read failed"),
                    })?;
                let value = self.copy(interp, &value, depth + 1)?;
                let _entered = HeapGuard::enter(&self.parcel.heap);
                target
                    .borrow_mut()
                    .props
                    .insert(key, crate::value::Property::builtin(value));
            }
        }
        Ok(())
    }
}
