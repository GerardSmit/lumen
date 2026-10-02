use super::parcel::{HeapGuard, Intrinsic};
use super::{Limits, Parcel};
use crate::{interpreter::Interp, value::{Exotic, Gc, Object, Value, set_data}};
use std::collections::HashMap;

struct Builder {
    memo: HashMap<usize, Gc>,
    limits: Limits,
    transfer: std::collections::HashSet<usize>,
    // Memo roots must drop before the heap, including on clone failure.
    parcel: Parcel,
}
impl Parcel {
    pub(crate) fn build(interp: &mut Interp, value: &Value, limits: Limits) -> Result<Self, Value> {
        Self::build_with_transfer(interp, value, &[], limits)
    }
    pub(crate) fn build_with_transfer(interp: &mut Interp, value: &Value, transfer: &[Value], limits: Limits) -> Result<Self, Value> {
        let mut pointers = std::collections::HashSet::new();
        for buffer in transfer {
            if !interp.is_transferable_array_buffer(buffer)
                || !pointers.insert(Gc::as_ptr(buffer.as_obj().unwrap()) as usize) {
                return Err(interp.make_error("DataCloneError", "transfer requires distinct attached ArrayBuffers"));
            }
        }
        let mut builder = Builder { parcel: Self::empty(), memo: HashMap::new(), limits, transfer: pointers };
        builder.parcel.root = builder.copy(interp, value, 0)?;
        // Getter code may have detached a buffer. Validate all entries before
        // detaching any; clone failure must leave the other buffers attached.
        for buffer in transfer {
            if !interp.is_transferable_array_buffer(buffer) {
                return Err(interp.make_error("DataCloneError", "transfer buffer was detached during cloning"));
            }
        }
        for pointer in &builder.transfer {
            let bytes = interp.array_buffers.remove(pointer).expect("validated buffer")
                .detach().expect("validated buffer is attached and unpinned");
            if let Some(object) = builder.memo.get(pointer) {
                builder.parcel.side.buffers.insert(Gc::as_ptr(object) as usize, bytes);
            }
        }
        Ok(builder.parcel)
    }
}
impl Builder {
    fn charge(&mut self, interp: &Interp, bytes: usize) -> Result<(), Value> {
        self.parcel.bytes = self.parcel.bytes.saturating_add(bytes);
        if self.parcel.bytes > self.limits.bytes || self.parcel.objects > self.limits.objects {
            return Err(interp.make_error("RangeError", "parcel limit exceeded"));
        }
        Ok(())
    }
    fn copy(&mut self, interp: &mut Interp, value: &Value, depth: usize) -> Result<Value, Value> {
        if depth > 256 { return Err(interp.make_error("RangeError", "parcel graph is too deep")); }
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
                Value::BigInt(crate::bigint::JsBigInt::from_words(negative, words.to_vec()))
            }
            Value::Sym(_) => return Err(interp.make_error("DataCloneError", "Symbol cannot be cloned")),
            Value::Obj(object) => {
                let pointer = Gc::as_ptr(object) as usize;
                if let Some(copied) = self.memo.get(&pointer) { return Ok(Value::Obj(copied.clone())); }
                if interp.proxies.contains_key(&pointer) || !matches!(object.borrow().call, crate::value::Callable::None | crate::value::Callable::User(_)) {
                    return Err(interp.make_error("DataCloneError", "unsupported object"));
                }
                interp.materialize(object);
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
                    if array { Object::new_array_from_vec(None, Vec::new()) } else { Object::new(None) }
                };
                self.memo.insert(pointer, copied.clone());
                self.parcel.protos.push((copied.clone(), intrinsic));
                let function = match &object.borrow().call {
                    crate::value::Callable::User(user) => Some((user.func.clone(), user.env.clone())),
                    _ => None,
                };
                if let Some((function, environment)) = function {
                    self.copy_function(interp, object, &copied, function, environment, depth)?;
                }
                self.copy_slots(interp, object, &copied, pointer, depth)?;
                let keys = object.borrow().props.ordered_keys();
                for key in keys {
                    if Interp::is_sym_key(&key) { continue; }
                    let enumerable = object.borrow().props.get(&key).is_some_and(|property| property.enumerable());
                    if !enumerable && !(array && &*key == "length") { continue; }
                    self.charge(interp, key.len())?;
                    // Getters execute with the sender current, outside the allocation guard.
                    let member = interp.get_member(value, &key).map_err(|abrupt| match abrupt {
                        crate::interpreter::Abrupt::Throw(value) => value,
                        _ => interp.make_error("DataCloneError", "property read failed"),
                    })?;
                    let member = self.copy(interp, &member, depth + 1)?;
                    let _entered = HeapGuard::enter(&self.parcel.heap);
                    if array && &*key == "length" {
                        copied.borrow_mut().props.insert("length", crate::value::Property::data(member, true, false, false));
                    } else {
                        set_data(&copied, &key, member);
                    }
                }
                Value::Obj(copied)
            }
        })
    }

    fn intrinsic(&self, interp: &Interp, object: &Gc, pointer: usize) -> Result<Intrinsic, Value> {
        if let crate::value::Callable::User(user) = &object.borrow().call {
            return Ok(match (user.func.is_async, user.func.is_generator) {
                (true, true) => Intrinsic::Extra("%AsyncGeneratorFunction.prototype%"),
                (true, false) => Intrinsic::Extra("%AsyncFunction.prototype%"),
                (false, true) => Intrinsic::Extra("%GeneratorFunction.prototype%"),
                _ => Intrinsic::Function,
            });
        }
        if interp.generators.contains_key(&pointer) || interp.module_ns.contains_key(&pointer)
            || object.borrow().props.iter().any(|(key, _)| Interp::is_private_key(&key) && &*key != crate::value::EXOTIC_SLOT) {
            return Err(interp.make_error("DataCloneError", "object has unsupported internal slots"));
        }
        if let Some(data) = interp.map_data.get(&pointer) {
            use crate::builtins::collection_data::CollectionKind;
            return match data.kind() {
                CollectionKind::Map => Ok(Intrinsic::Extra("Map")),
                CollectionKind::Set => Ok(Intrinsic::Extra("Set")),
                _ => Err(interp.make_error("DataCloneError", "weak collections cannot be cloned")),
            };
        }
        let object = object.borrow();
        let kind = match object.exotic {
            Exotic::Array => Intrinsic::Array,
            Exotic::StrWrap => Intrinsic::String,
            Exotic::NumWrap => Intrinsic::Number,
            Exotic::BoolWrap => Intrinsic::Boolean,
            Exotic::BigIntWrap => Intrinsic::Extra("BigInt"),
            Exotic::Error => {
                let name = interp.error_protos.iter().find(|(_, proto)| object.proto.as_ref().is_some_and(|p| Gc::as_ptr(p) == Gc::as_ptr(proto))).map(|(name, _)| *name).unwrap_or("Error");
                Intrinsic::Error(name)
            }
            _ => {
                if let Some((name, _)) = interp.extra_protos.iter().find(|(_, proto)| object.proto.as_ref().is_some_and(|p| Gc::as_ptr(p) == Gc::as_ptr(proto))) {
                    match *name {
                        "ArrayBuffer" | "SharedArrayBuffer" | "DataView" | "RegExp" | "Date" |
                        "Int8Array" | "Uint8Array" | "Uint8ClampedArray" | "Int16Array" | "Uint16Array" |
                        "Int32Array" | "Uint32Array" | "Float16Array" | "Float32Array" | "Float64Array" |
                        "BigInt64Array" | "BigUint64Array" | "%GeneratorPrototype%" | "%AsyncGeneratorPrototype%" => Intrinsic::Extra(name),
                        _ => return Err(interp.make_error("DataCloneError", "unsupported builtin object")),
                    }
                } else { Intrinsic::Object }
            }
        };
        Ok(kind)
    }

    fn copy_function(&mut self, interp: &mut Interp, source: &Gc, target: &Gc, function: std::rc::Rc<crate::ast::Function>, environment: crate::interpreter::Env, depth: usize) -> Result<(), Value> {
        if function.is_method {
            return Err(interp.make_error("DataCloneError", "methods and accessors cannot be cloned"));
        }
        let capture = |name: &str| interp.make_error("TypeError", format!("parallel function captures '{name}' from the outer scope; pass it in args"));
        if environment.borrow().under_with() { return Err(capture("with")); }
        if function.is_arrow {
            let flags = function.scan_flags();
            for (flag, name) in [(crate::ast::SCAN_THIS, "this"), (crate::ast::SCAN_ARGUMENTS, "arguments"), (crate::ast::SCAN_NEW_TARGET, "new.target")] {
                if flags & flag != 0 { return Err(capture(name)); }
            }
        }
        let names = crate::bytecode::function_free_identifiers(&function)
            .ok_or_else(|| interp.make_error("TypeError", "parallel function contains dynamic scope; pass helpers in args"))?;
        for name in names {
            let mut scope = Some(environment.clone());
            while let Some(current) = scope {
                if current.borrow().vars.contains_key(&name) { return Err(capture(&name)); }
                scope = current.borrow().parent.clone();
            }
            if interp.global.borrow().props.get(&name).is_some_and(|property| property.enumerable()) {
                return Err(capture(&name));
            }
        }
        let text = match &function.source {
            crate::ast::FnSource::Range { src, .. } => &**src,
            _ => function.source.as_str().unwrap_or(""),
        };
        let snapshot = crate::snapshot::encode(&[crate::ast::Stmt::FuncDecl(function.clone())], text);
        self.charge(interp, snapshot.len().saturating_add(text.len()))?;
        let decoded = {
            let _entered = HeapGuard::enter(&self.parcel.heap);
            crate::snapshot::decode(&snapshot, text)
        }.map_err(|message| interp.make_error("DataCloneError", message))?;
        let crate::ast::Stmt::FuncDecl(decoded) = decoded.into_iter().next().expect("function snapshot") else { unreachable!() };
        self.parcel.functions.push((target.clone(), decoded));
        target.borrow_mut().is_constructor = source.borrow().is_constructor;
        for key in ["name", "length", "prototype"] {
            if !source.borrow().props.contains(key) { continue; }
            let value = interp.get_member(&Value::Obj(source.clone()), key).map_err(|abrupt| match abrupt {
                crate::interpreter::Abrupt::Throw(value) => value,
                _ => interp.make_error("DataCloneError", "function property read failed"),
            })?;
            let value = self.copy(interp, &value, depth + 1)?;
            let _entered = HeapGuard::enter(&self.parcel.heap);
            if key == "prototype" {
                if let Value::Obj(prototype) = &value {
                    prototype.borrow_mut().props.insert("constructor", crate::value::Property::builtin(Value::Obj(target.clone())));
                }
            }
            target.borrow_mut().props.insert(key, crate::value::Property::builtin(value));
        }
        Ok(())
    }

    fn copy_slots(&mut self, interp: &mut Interp, source: &Gc, target: &Gc, pointer: usize, depth: usize) -> Result<(), Value> {
        let target_pointer = Gc::as_ptr(target) as usize;
        let exotic = source.borrow().exotic;
        let payload = source.borrow().exotic_payload();
        if let Some(payload) = payload {
            let payload = self.copy(interp, &payload, depth + 1)?;
            let _entered = HeapGuard::enter(&self.parcel.heap);
            target.borrow_mut().set_exotic(exotic, Some(payload));
        }
        if let Some(data) = interp.map_data.get(&pointer) {
            let kind = data.kind();
            let entries = data.iter().map(|(key, value)| (key.unpack(), value.unpack())).collect::<Vec<_>>();
            let mut copied = crate::builtins::collection_data::CollectionData::new(kind);
            for (key, value) in entries {
                let key = self.copy(interp, &key, depth + 1)?;
                let value = self.copy(interp, &value, depth + 1)?;
                copied.insert(key, value);
            }
            self.parcel.side.maps.insert(target_pointer, copied);
        }
        if let Some(buffer) = interp.array_buffers.get(&pointer) {
            let size = buffer.len();
            let bytes = if self.transfer.contains(&pointer) { Vec::new() } else { buffer.to_vec() };
            self.charge(interp, size)?;
            self.parcel.side.buffers.insert(target_pointer, bytes);
        }
        if let Some(shared) = interp.export_shared_array_buffer(&Value::Obj(source.clone()))? {
            self.parcel.side.shared.insert(target_pointer, shared);
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
            self.parcel.side.views.insert(target_pointer, (Gc::as_ptr(buffer.as_obj().unwrap()) as usize, offset, length, track));
            let _entered = HeapGuard::enter(&self.parcel.heap);
            target.borrow_mut().props.insert("\u{0}dv_buffer", crate::value::Property::builtin(buffer));
        }
        if let Some(regex) = interp.regexps.get(&pointer) {
            let regex = crate::regex::Regex::new(&regex.source, &regex.flags).map_err(|message| interp.make_error("DataCloneError", message))?;
            self.parcel.side.regexps.insert(target_pointer, std::rc::Rc::new(regex));
        }
        // Hidden builtin properties carry Date/buffer metadata; Error fields are
        // non-enumerable. These are copied separately from ordinary own keys.
        let keys = match exotic {
            Exotic::Error => vec!["name", "message", "stack", "cause", "code"],
            _ => {
                let mut keys = Vec::new();
                if interp.array_buffers.contains_key(&pointer) { keys.extend(["\u{0}ab_max_byte_length", "\u{0}ab_resizable", "\u{0}sab_id"]); }
                if interp.regexps.contains_key(&pointer) { keys.push("lastIndex"); }
                if source.borrow().proto.as_ref().is_some_and(|proto| interp.extra_protos.get("Date").is_some_and(|date| Gc::as_ptr(date) == Gc::as_ptr(proto))) { keys.push("\u{0}date_ms"); }
                keys
            },
        };
        for key in keys {
            if source.borrow().props.contains(key) {
                let value = interp.get_member(&Value::Obj(source.clone()), key).map_err(|abrupt| match abrupt {
                    crate::interpreter::Abrupt::Throw(value) => value,
                    _ => interp.make_error("DataCloneError", "internal property read failed"),
                })?;
                let value = self.copy(interp, &value, depth + 1)?;
                let _entered = HeapGuard::enter(&self.parcel.heap);
                target.borrow_mut().props.insert(key, crate::value::Property::builtin(value));
            }
        }
        Ok(())
    }
}
