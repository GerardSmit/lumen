use super::{
    build::Builder,
    parcel::{HeapGuard, Intrinsic},
};
use crate::{
    ast::{Function, Stmt},
    interpreter::{new_scope, ClassInfo, Env, FieldInit, Interp},
    value::{Callable, Gc, Property, Value},
};
use std::{
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
};

impl Builder {
    #[cfg(feature = "aot-native")]
    pub(super) fn register_native_class(
        &mut self,
        interp: &Interp,
        constructor: &Gc,
        class: &crate::native_aot::classes::NativeClass,
    ) {
        self.class_objects.insert(Gc::as_ptr(constructor) as usize);
        let mut scope = Some(class.env.clone());
        while let Some(current) = scope {
            if Rc::ptr_eq(&current, &interp.global_env) {
                break;
            }
            self.class_scopes.insert(Rc::as_ptr(&current) as usize);
            for (name, binding) in current.borrow().vars.iter() {
                if name.starts_with('#') {
                    if let Value::Str(key) = &binding.value {
                        static NEXT: AtomicU64 = AtomicU64::new(1);
                        self.private_keys.entry(key.to_string()).or_insert_with(|| {
                            format!("{name}\u{1}native{}", NEXT.fetch_add(1, Ordering::Relaxed))
                        });
                    }
                }
            }
            scope = current.borrow().parent.clone();
        }
        for key in class
            .fields
            .iter()
            .map(|field| &field.key)
            .chain(class.private_members.iter().map(|(key, _)| key))
        {
            if Interp::is_private_key(key) {
                static NEXT: AtomicU64 = AtomicU64::new(1);
                self.private_keys.entry(key.clone()).or_insert_with(|| {
                    format!("{key}\u{1}native{}", NEXT.fetch_add(1, Ordering::Relaxed))
                });
            }
        }
        let prototype = constructor
            .borrow()
            .props
            .get("prototype")
            .and_then(|property| property.value().as_obj().cloned());
        for object in std::iter::once(constructor.clone()).chain(prototype) {
            self.class_objects.insert(Gc::as_ptr(&object) as usize);
            let properties = object
                .borrow()
                .props
                .iter()
                .map(|(_, property)| property.clone())
                .collect::<Vec<_>>();
            for property in properties.into_iter().chain(
                class
                    .private_members
                    .iter()
                    .map(|(_, property)| property.clone()),
            ) {
                for value in [
                    Some(property.value()),
                    property.getter().cloned(),
                    property.setter().cloned(),
                ]
                .into_iter()
                .flatten()
                {
                    if let Value::Obj(function) = value {
                        if matches!(
                            function.borrow().call,
                            Callable::Aot(_) | Callable::AccessorGet(_) | Callable::AccessorSet(_)
                        ) {
                            self.class_objects.insert(Gc::as_ptr(&function) as usize);
                        }
                    }
                }
            }
        }
    }

    #[cfg(feature = "aot-native")]
    pub(super) fn copy_native_class(
        &mut self,
        interp: &mut Interp,
        target: &Gc,
        class: &crate::native_aot::classes::NativeClass,
        depth: usize,
    ) -> Result<(), Value> {
        let env = self.copy_environment(interp, &class.env, depth + 1)?;
        let mut fields = Vec::new();
        for field in &class.fields {
            let mut copied = field.clone();
            copied.key = self
                .private_keys
                .get(&field.key)
                .cloned()
                .unwrap_or_else(|| field.key.clone());
            copied.transforms = field
                .transforms
                .iter()
                .map(|value| self.copy(interp, value, depth + 1))
                .collect::<Result<_, _>>()?;
            fields.push(copied);
        }
        let mut private_members = Vec::new();
        for (key, property) in &class.private_members {
            let key = self
                .private_keys
                .get(key)
                .cloned()
                .unwrap_or_else(|| key.clone());
            private_members.push((key, self.copy_descriptor(interp, property, depth + 1)?));
        }
        let initializers = class
            .initializers
            .iter()
            .map(|value| self.copy(interp, value, depth + 1))
            .collect::<Result<_, _>>()?;
        self.parcel.native_classes.push(super::parcel::NativeClass {
            trusted_glue: class.program.trusted_glue,
            object: target.clone(),
            bytes: class.program.bytes.clone(),
            hash: class.program.image.blob_hash(),
            env,
            derived: class.derived,
            body: class.body,
            fields,
            private_members,
            initializers,
        });
        Ok(())
    }

    pub(super) fn copy_class_prototype(
        &mut self,
        interp: &mut Interp,
        source: &Gc,
        target: &Gc,
        depth: usize,
    ) -> Result<(), Value> {
        let parent = source.borrow().proto.clone();
        if let Some(tag) = parent
            .as_ref()
            .and_then(|p| self.intrinsic_reference(interp, &Value::Obj(p.clone())))
        {
            self.parcel.protos.push((target.clone(), tag));
        } else {
            let parent = self.copy(
                interp,
                &parent.map(Value::Obj).unwrap_or(Value::Null),
                depth,
            )?;
            self.parcel.class_protos.push((target.clone(), parent));
        }
        Ok(())
    }
    pub(super) fn register_class(
        &mut self,
        interp: &Interp,
        constructor: &Gc,
    ) -> Result<(), Value> {
        let pointer = Gc::as_ptr(constructor) as usize;
        if !self.class_objects.insert(pointer) {
            return Ok(());
        }
        let info = &interp.class_info[&pointer];
        for key in info.fields.iter().map(|f| f.key.as_str()).chain(
            constructor
                .borrow()
                .props
                .iter()
                .filter(|(key, _)| Interp::is_private_key(key))
                .map(|(key, _)| key.to_string())
                .collect::<Vec<_>>()
                .iter()
                .map(String::as_str),
        ) {
            if Interp::is_private_key(key) {
                static NEXT: AtomicU64 = AtomicU64::new(1);
                self.private_keys.entry(key.into()).or_insert_with(|| {
                    format!("{key}\u{1}backing{}", NEXT.fetch_add(1, Ordering::Relaxed))
                });
            }
        }
        let mut scope = Some(info.field_env.clone());
        for _ in 0..3 {
            let Some(current) = scope else {
                break;
            };
            self.class_scopes.insert(Rc::as_ptr(&current) as usize);
            for (name, binding) in current.borrow().vars.iter() {
                if name.starts_with('#') {
                    if let Value::Str(key) = &binding.value {
                        static NEXT: AtomicU64 = AtomicU64::new(1);
                        self.private_keys.entry(key.to_string()).or_insert_with(|| {
                            format!("{name}\u{1}parcel{}", NEXT.fetch_add(1, Ordering::Relaxed))
                        });
                    }
                }
            }
            scope = current.borrow().parent.clone();
        }
        let parent = info
            .field_env
            .borrow()
            .vars
            .get("%superclass%")
            .and_then(|binding| binding.value.as_obj().cloned());
        if let Some(parent) = parent.filter(|parent| {
            interp
                .class_info
                .contains_key(&(Gc::as_ptr(parent) as usize))
        }) {
            self.register_class(interp, &parent)?;
        }
        let prototype = constructor
            .borrow()
            .props
            .get("prototype")
            .and_then(|p| p.value().as_obj().cloned());
        for object in std::iter::once(constructor.clone()).chain(prototype) {
            self.class_objects.insert(Gc::as_ptr(&object) as usize);
            let properties = object
                .borrow()
                .props
                .iter()
                .map(|(_, p)| p.clone())
                .collect::<Vec<_>>();
            for property in properties {
                for value in [
                    Some(property.value()),
                    property.getter().cloned(),
                    property.setter().cloned(),
                ]
                .into_iter()
                .flatten()
                {
                    if let Value::Obj(function) = value {
                        if matches!(
                            function.borrow().call,
                            Callable::AccessorGet(_) | Callable::AccessorSet(_)
                        ) {
                            self.class_objects.insert(Gc::as_ptr(&function) as usize);
                        }
                        if let Callable::User(user) = &function.borrow().call {
                            let parent = user.env.borrow().parent.clone();
                            if parent.is_some_and(|scope| {
                                self.class_scopes.contains(&(Rc::as_ptr(&scope) as usize))
                            }) {
                                self.class_objects.insert(Gc::as_ptr(&function) as usize);
                                self.class_scopes.insert(Rc::as_ptr(&user.env) as usize);
                            }
                        }
                    }
                }
            }
        }
        for (_, property) in &info.private_members {
            for value in [
                Some(property.value()),
                property.getter().cloned(),
                property.setter().cloned(),
            ]
            .into_iter()
            .flatten()
            {
                if let Value::Obj(function) = value {
                    self.class_objects.insert(Gc::as_ptr(&function) as usize);
                }
            }
        }
        Ok(())
    }

    fn intrinsic_reference(&self, interp: &Interp, value: &Value) -> Option<Intrinsic> {
        let object = value.as_obj()?;
        let same = |other: &Gc| Gc::as_ptr(other) == Gc::as_ptr(object);
        for (proto, tag) in [
            (&interp.object_proto, Intrinsic::Object),
            (&interp.function_proto, Intrinsic::Function),
            (&interp.array_proto, Intrinsic::Array),
        ] {
            if same(proto) {
                return Some(tag);
            }
        }
        for (name, proto) in &interp.extra_protos {
            if same(proto) {
                return Some(Intrinsic::Extra(name));
            }
        }
        for (name, proto) in &interp.error_protos {
            if same(proto) {
                return Some(Intrinsic::Error(name));
            }
        }
        for (name, property) in interp.global.borrow().props.iter() {
            if !matches!(
                &*name,
                "Object"
                    | "Array"
                    | "Function"
                    | "String"
                    | "Number"
                    | "Boolean"
                    | "BigInt"
                    | "Date"
                    | "RegExp"
                    | "Map"
                    | "Set"
                    | "WeakMap"
                    | "WeakSet"
                    | "Promise"
                    | "ArrayBuffer"
                    | "SharedArrayBuffer"
                    | "DataView"
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
                    | "Error"
                    | "EvalError"
                    | "RangeError"
                    | "ReferenceError"
                    | "SyntaxError"
                    | "TypeError"
                    | "URIError"
                    | "AggregateError"
            ) {
                continue;
            }
            if !property.enumerable()
                && property.value().as_obj().is_some_and(|object| {
                    same(object) && matches!(object.borrow().call, Callable::Native(_))
                })
            {
                return Some(Intrinsic::Global(name.to_string()));
            }
        }
        None
    }

    pub(super) fn copy_environment(
        &mut self,
        interp: &mut Interp,
        source: &Env,
        depth: usize,
    ) -> Result<Env, Value> {
        if depth > 256 {
            return Err(interp.make_error("RangeError", "class scope limit exceeded"));
        }
        let pointer = Rc::as_ptr(source) as usize;
        if let Some(environment) = self.environments.get(&pointer) {
            return Ok(environment.clone());
        }
        if !self.class_scopes.contains(&pointer) {
            return Err(interp.make_error(
                "TypeError",
                "class captures an outer environment; pass it in args",
            ));
        }
        let target = {
            let _entered = HeapGuard::enter(&self.parcel.heap);
            new_scope(None)
        };
        self.environments.insert(pointer, target.clone());
        let parent = source.borrow().parent.clone();
        if let Some(parent) =
            parent.filter(|p| self.class_scopes.contains(&(Rc::as_ptr(p) as usize)))
        {
            target.borrow_mut().parent = Some(self.copy_environment(interp, &parent, depth + 1)?);
        } else {
            self.parcel.scopes.push(target.clone());
        }
        let bindings = source
            .borrow()
            .vars
            .iter()
            .map(|(name, binding)| (name.to_string(), binding.clone()))
            .collect::<Vec<_>>();
        for (name, mut binding) in bindings {
            self.charge(interp, name.len() + std::mem::size_of_val(&binding))?;
            binding.value = if let Some(tag) = self.intrinsic_reference(interp, &binding.value) {
                self.parcel
                    .scope_intrinsics
                    .push((target.clone(), name.clone(), tag));
                Value::Undefined
            } else if let Value::Str(key) = &binding.value {
                if let Some(key) = self.private_keys.get(&**key) {
                    Value::str(key)
                } else {
                    self.copy(interp, &binding.value, depth + 1)?
                }
            } else {
                self.copy(interp, &binding.value, depth + 1)?
            };
            let _entered = HeapGuard::enter(&self.parcel.heap);
            target.borrow_mut().vars.insert(name, binding);
        }
        Ok(target)
    }

    pub(super) fn copy_descriptor(
        &mut self,
        interp: &mut Interp,
        source: &Property,
        depth: usize,
    ) -> Result<Property, Value> {
        if source.accessor() {
            let get = source
                .getter()
                .map(|v| self.copy(interp, v, depth))
                .transpose()?;
            let set = source
                .setter()
                .map(|v| self.copy(interp, v, depth))
                .transpose()?;
            let _entered = HeapGuard::enter(&self.parcel.heap);
            Ok(Property::accessor_prop(
                get,
                set,
                source.enumerable(),
                source.configurable(),
            ))
        } else {
            let value = self.copy(interp, &source.value(), depth)?;
            let _entered = HeapGuard::enter(&self.parcel.heap);
            Ok(Property::data(
                value,
                source.writable(),
                source.enumerable(),
                source.configurable(),
            ))
        }
    }

    pub(super) fn copy_class_info(
        &mut self,
        interp: &mut Interp,
        source: &Gc,
        target: &Gc,
        function: &Function,
        depth: usize,
    ) -> Result<(), Value> {
        let info = &interp.class_info[&(Gc::as_ptr(source) as usize)];
        let fields = info
            .fields
            .iter()
            .map(|f| (f.key.clone(), f.init.clone(), f.transforms.clone()))
            .collect::<Vec<_>>();
        let environment = info.field_env.clone();
        let derived = info.derived;
        let initializers = info.instance_initializers.clone();
        let private_members = info.private_members.clone();
        let mut copied_fields = Vec::new();
        let text = match &function.source {
            crate::ast::FnSource::Range { src, .. } => &**src,
            _ => function.source.as_str().unwrap_or(""),
        };
        for (index, (key, init, transforms)) in fields.into_iter().enumerate() {
            if Interp::is_sym_key(&key) {
                let name = interp
                    .wk_syms
                    .iter()
                    .find(|(_, _, symbol)| &**symbol == key)
                    .map(|(name, _, _)| *name)
                    .ok_or_else(|| {
                        interp.make_error("DataCloneError", "class has an unsupported symbol field")
                    })?;
                self.parcel
                    .field_symbols
                    .push((Gc::as_ptr(target) as usize, index, name));
            }
            let init = if let Some(expr) = init {
                let mut scan = function.clone();
                scan.params.clear();
                scan.body = std::cell::RefCell::new(Some(Rc::new(vec![Stmt::Expr(expr.clone())])));
                scan.lazy = std::cell::RefCell::new(None);
                let names = crate::bytecode::function_free_identifiers(&scan).ok_or_else(|| {
                    interp.make_error("TypeError", "dynamic class field scope is unsupported")
                })?;
                for name in names {
                    let mut scope = Some(environment.clone());
                    let mut class_binding = false;
                    while let Some(current) = scope {
                        if current.borrow().vars.contains_key(&name) {
                            if !self.class_scopes.contains(&(Rc::as_ptr(&current) as usize)) {
                                return Err(interp.make_error("TypeError", format!("parallel class captures '{name}' from the outer scope; pass it in args")));
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
                            .is_some_and(|p| p.enumerable())
                    {
                        return Err(interp.make_error("TypeError", format!("parallel class captures '{name}' from the outer scope; pass it in args")));
                    }
                }
                let bytes = crate::snapshot::encode(&[Stmt::Expr(expr)], text);
                self.charge(interp, bytes.len() + text.len())?;
                let body = {
                    let _entered = HeapGuard::enter(&self.parcel.heap);
                    crate::snapshot::decode(&bytes, text)
                }
                .map_err(|message| interp.make_error("DataCloneError", message))?;
                let Some(Stmt::Expr(expr)) = body.into_iter().next() else {
                    unreachable!()
                };
                Some(expr)
            } else {
                None
            };
            let transforms = transforms
                .iter()
                .map(|v| self.copy(interp, v, depth))
                .collect::<Result<Vec<_>, _>>()?;
            copied_fields.push(FieldInit {
                key: self.private_keys.get(&key).cloned().unwrap_or(key),
                init,
                transforms,
            });
        }
        let field_env = self.copy_environment(interp, &environment, depth)?;
        let initializers = initializers
            .iter()
            .map(|v| self.copy(interp, v, depth))
            .collect::<Result<Vec<_>, _>>()?;
        let mut members = Vec::new();
        for (key, property) in private_members {
            members.push((
                self.private_keys.get(&key).cloned().unwrap_or(key),
                self.copy_descriptor(interp, &property, depth)?,
            ));
        }
        self.parcel.side.classes.insert(
            Gc::as_ptr(target) as usize,
            ClassInfo {
                fields: copied_fields,
                field_env,
                derived,
                instance_initializers: initializers,
                private_members: members,
                field_code: crate::interpreter::class_fields::FieldCode::new(),
                plan: Default::default(),
            },
        );
        Ok(())
    }
}
