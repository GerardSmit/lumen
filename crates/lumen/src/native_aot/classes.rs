//! Class objects and initialization from native metadata.
use super::{metadata::MemberKey, NativeProgram};
use crate::interpreter::{new_scope, Abrupt, Binding, Env, Interp};
use crate::value::{Callable, Gc, Object, Property, Value};
use std::rc::Rc;

#[derive(Clone)]
pub(crate) struct Field {
    pub key: String,
    pub initializer: Option<u32>,
    pub named: bool,
    pub transforms: Vec<Value>,
}

pub(crate) struct NativeClass {
    pub program: Rc<NativeProgram>,
    pub env: Env,
    pub derived: bool,
    pub body: Option<u32>,
    pub fields: Vec<Field>,
    pub private_members: Vec<(String, Property)>,
    pub initializers: Vec<Value>,
}

impl NativeClass {
    pub(crate) fn visit_values(&self, mut visit: impl FnMut(&Value)) {
        for field in &self.fields { for transform in &field.transforms { visit(transform); } }
        for initializer in &self.initializers { visit(initializer); }
        for (_, property) in &self.private_members {
            if property.accessor() { if let Some(value) = property.getter() { visit(value); } if let Some(value) = property.setter() { visit(value); } }
            else { let value = property.value(); visit(&value); }
        }
    }
}

pub(crate) fn default_constructor(i: &mut Interp, _: Value, _: &[Value]) -> Result<Value, Value> {
    Err(i.make_error("TypeError", "Class constructor cannot be invoked without 'new'"))
}

/// Preserve serialized private brands while keeping future brands distinct.
pub(crate) fn restore_private_key(i: &mut Interp, key: &str) -> Result<(), String> {
    let serial = if let Some(serial) = key.strip_prefix("#\u{0}acc") {
        Some(serial)
    } else if key.starts_with('#') {
        key.split_once('\u{1}').map(|(_, serial)| serial)
    } else { None };
    if let Some(serial) = serial {
        let serial = serial.parse::<u64>().map_err(|_| "invalid snapshot private brand".to_string())?;
        if serial == u64::MAX { return Err("snapshot private brand serial exhausted".into()); }
        i.accessor_seq = i.accessor_seq.max(serial);
    }
    Ok(())
}

pub(crate) fn restore_class(i: &mut Interp, constructor: &Gc, class: NativeClass) -> Result<(), String> {
    for index in class.body.iter().chain(class.fields.iter().filter_map(|field| field.initializer.as_ref())) {
        if *index as usize >= class.program.metadata.functions.len() {
            return Err("snapshot class function index is out of range".into());
        }
    }
    for field in &class.fields { restore_private_key(i, &field.key)?; }
    for (key, _) in &class.private_members { restore_private_key(i, key)?; }
    if class.derived && class.body.is_some_and(|index| class.program.metadata.functions[index as usize].frame_flags & 4 == 0) {
        return Err("snapshot derived constructor lacks derived frame state".into());
    }
    if class.body.is_none() { constructor.borrow_mut().call = Callable::Native(default_constructor); }
    constructor.borrow_mut().is_constructor = true;
    i.gc_pin(constructor);
    i.native_classes.insert(Gc::as_ptr(constructor) as usize, Rc::new(class));
    Ok(())
}

pub(crate) fn init_method(i: &mut Interp, program: &Rc<NativeProgram>, index: u32, env: Env, key: String, kind: u32, object: Gc) -> Result<(), Abrupt> {
    let home = new_scope(Some(env));
    crate::eval::bind(&home, "%homeobject%", Value::Obj(object.clone()));
    let value = i.make_native_function(program.clone(), index, home);
    let name = i.fn_name_for_key(&key);
    match kind {
        0 => { i.set_fn_name(&value, &name); object.borrow_mut().props.insert(key, Property::plain(value)); }
        1 => { i.set_fn_name(&value, &format!("get {name}")); i.define_accessor(&object, &key, Some(value), None); }
        2 => { i.set_fn_name(&value, &format!("set {name}")); i.define_accessor(&object, &key, None, Some(value)); }
        _ => return Err(i.throw("Error", "invalid native method kind")),
    }
    Ok(())
}

pub(crate) fn make(i: &mut Interp, program: &Rc<NativeProgram>, function: u32, index: u32, env: Env, inferred_name: Option<&str>) -> Result<Value, Abrupt> {
    let saved = std::mem::replace(&mut i.strict, true);
    let result = make_strict(i, program, function, index, env, inferred_name);
    i.strict = saved;
    result
}

fn make_strict(i: &mut Interp, program: &Rc<NativeProgram>, function: u32, index: u32, env: Env, inferred_name: Option<&str>) -> Result<Value, Abrupt> {
    let class = &program.metadata.functions[function as usize].classes[index as usize];
    let outer = new_scope(Some(env));
    if !class.name.is_empty() {
        outer.borrow_mut().vars.insert(class.name.clone(), Binding::data(Value::Undefined, false, false));
    }
    let parent = class.superclass.map(|index| super::call(i, program, index, &outer, Value::Undefined, &[])).transpose()?;
    let (prototype_parent, constructor_parent) = match &parent {
        None => (Some(i.object_proto.clone()), None),
        Some(Value::Null) => (None, None),
        Some(value @ Value::Obj(object)) if i.value_is_constructor(value) => {
            let prototype = match i.get_member(value, "prototype")? {
                Value::Obj(prototype) => Some(prototype), Value::Null => None,
                _ => return Err(i.throw("TypeError", "Class extends value does not have a valid prototype property")),
            };
            (prototype, Some(object.clone()))
        }
        _ => return Err(i.throw("TypeError", "Class extends value is not a constructor or null")),
    };
    let derived = parent.is_some();
    let prototype = Object::new(prototype_parent.clone());
    let class_env = new_scope(Some(outer.clone()));
    for member in &class.members {
        if let MemberKey::Private(name) = &member.key {
            if !class_env.borrow().vars.contains_key(name) {
                i.accessor_seq += 1;
                crate::eval::bind(&class_env, name, Value::str(format!("{name}\u{1}{}", i.accessor_seq)));
            }
        }
    }
    let instance_env = new_scope(Some(class_env.clone()));
    crate::eval::bind(&instance_env, "%homeobject%", Value::Obj(prototype.clone()));
    crate::eval::bind(&instance_env, "%superproto%", prototype_parent.map(Value::Obj).unwrap_or(Value::Null));
    crate::eval::bind(&instance_env, "%superclass%", constructor_parent.clone().map(Value::Obj).unwrap_or(if derived { Value::Null } else { Value::Undefined }));
    let static_env = new_scope(Some(class_env.clone()));
    crate::eval::bind(&static_env, "%superproto%", Value::Obj(constructor_parent.clone().unwrap_or_else(|| i.function_proto.clone())));
    let body = class.members.iter().find(|member| member.kind == 0).and_then(|member| member.method);
    if derived && body.is_some_and(|index| program.metadata.functions[index as usize].frame_flags & 4 == 0) {
        return Err(i.throw("Error", "native derived constructor lacks derived frame state"));
    }
    let constructor = if let Some(index) = body {
        i.make_native_function(program.clone(), index, instance_env.clone())
    } else { Value::Obj(i.make_native(&class.name, 0, default_constructor)) };
    let object = constructor.as_obj().unwrap().clone();
    {
        let mut object = object.borrow_mut();
        object.is_constructor = true;
        object.proto = Some(constructor_parent.unwrap_or_else(|| i.function_proto.clone()));
        object.props.insert("prototype", Property::data(Value::Obj(prototype.clone()), false, false, false));
    }
    let name = if class.name.is_empty() { inferred_name.unwrap_or("") } else { &class.name };
    i.set_fn_name(&constructor, name);
    prototype.borrow_mut().props.insert("constructor", Property::builtin(constructor.clone()));
    crate::eval::bind(&instance_env, "%thisctor%", constructor.clone());
    crate::eval::bind(&static_env, "%homeobject%", constructor.clone());
    if !class.name.is_empty() { outer.borrow_mut().vars.get_mut(&class.name).unwrap().value = constructor.clone(); }
    let mut fields = Vec::new(); let mut private_members: Vec<(String, Property)> = Vec::new();
    let mut instance_initializers = Vec::new(); let mut static_initializers = Vec::new();
    let mut static_elements = Vec::new();
    for member in &class.members {
        if member.kind == 0 { continue; }
        let private = matches!(member.key, MemberKey::Private(_));
        let key = match &member.key {
            MemberKey::Public(name) => name.clone(), MemberKey::Private(name) => i.resolve_private(name, &class_env),
            MemberKey::Number(number) => i.to_property_key(&Value::Num(*number))?,
            MemberKey::Computed(index) => { let value = super::call(i, program, *index, &class_env, Value::Undefined, &[])?; i.to_property_key(&value)? }
        };
        if member.is_static && key == "prototype" && matches!(member.key, MemberKey::Computed(_)) { return Err(i.throw("TypeError", "classes may not have a static property named 'prototype'")); }
        let member_env = if member.is_static { &static_env } else { &instance_env };
        let target = if member.is_static { &object } else { &prototype };
        let inits = if member.is_static { &mut static_initializers } else { &mut instance_initializers };
        let display = key.split('\u{1}').next().unwrap_or(&key);
        match member.kind {
            1 | 2 | 3 => {
                let index = member.method.ok_or_else(|| i.throw("Error", "native class method missing"))?;
                let value = i.make_native_function(program.clone(), index, member_env.clone());
                let name = i.fn_name_for_key(display);
                i.set_fn_name(&value, &format!("{}{name}", match member.kind { 2 => "get ", 3 => "set ", _ => "" }));
                let value = i.decorate_callable_with(&member.decorators, value, match member.kind { 2 => "getter", 3 => "setter", _ => "method" }, &key, member.is_static, private, inits, |i, index| super::call(i, program, *index, &outer, Value::Undefined, &[]))?;
                if member.kind == 1 {
                    let property = if private { Property::data(value, false, false, false) } else { Property::builtin(value) };
                    if private && !member.is_static { private_members.push((key, property)); } else { target.borrow_mut().props.insert(key, property); }
                } else {
                    let (get, set) = if member.kind == 2 { (Some(value), None) } else { (None, Some(value)) };
                    if private && !member.is_static {
                        if let Some((_, property)) = private_members.iter_mut().find(|(name, _)| name == &key) {
                            if get.is_some() { property.set_getter(get); } if set.is_some() { property.set_setter(set); }
                        } else { private_members.push((key, Property::accessor_prop(get, set, false, false))); }
                    } else { i.define_class_accessor(target, &key, get, set); }
                }
            }
            4 | 5 => {
                let (field_key, transforms) = if member.kind == 5 {
                    i.accessor_seq += 1;
                    let backing: Rc<str> = Rc::from(format!("#\u{0}acc{}", i.accessor_seq));
                    let getter = i.make_accessor_fn(&key, &backing, true); let setter = i.make_accessor_fn(&key, &backing, false);
                    let (getter, setter, transforms) = i.decorate_accessor_with(&member.decorators, &key, member.is_static, getter, setter, inits, |i, index| super::call(i, program, *index, &outer, Value::Undefined, &[]))?;
                    if private && !member.is_static { private_members.push((key.clone(), Property::accessor_prop(Some(getter), Some(setter), false, false))); }
                    else { i.define_class_accessor(target, &key, Some(getter), Some(setter)); }
                    (backing.to_string(), transforms)
                } else {
                    let transforms = i.decorate_field_with(&member.decorators, &key, member.is_static, private, inits, |i, index| super::call(i, program, *index, &outer, Value::Undefined, &[]))?;
                    (key, transforms)
                };
                let field = Field { key: field_key, initializer: member.initializer, named: member.initializer_named, transforms };
                if member.is_static { static_elements.push((None, Some(field))); } else { fields.push(field); }
            }
            6 => static_elements.push((member.method, None)),
            _ => return Err(i.throw("Error", "invalid native class member kind")),
        }
    }
    if !class.name.is_empty() { outer.borrow_mut().vars.get_mut(&class.name).unwrap().initialized = true; }
    i.gc_pin(&object);
    i.native_classes.insert(Gc::as_ptr(&object) as usize, Rc::new(NativeClass { program: program.clone(), env: instance_env, derived, body, fields, private_members, initializers: instance_initializers }));
    for (block, field) in static_elements {
        let scope = new_scope(Some(static_env.clone())); crate::eval::bind(&scope, "this", constructor.clone());
        if let Some(index) = block { super::call(i, program, index, &scope, constructor.clone(), &[])?; }
        if let Some(field) = field { initialize_field(i, program, &scope, &constructor, &field)?; }
    }
    let constructor = i.decorate_callable_with(&class.decorators, constructor, "class", &class.name, false, false, &mut static_initializers, |i, index| super::call(i, program, *index, &outer, Value::Undefined, &[]))?;
    for initializer in static_initializers { i.call(initializer, constructor.clone(), &[])?; }
    Ok(constructor)
}

fn initialize_field(i: &mut Interp, program: &Rc<NativeProgram>, env: &Env, this: &Value, field: &Field) -> Result<(), Abrupt> {
    let scope = new_scope(Some(env.clone())); crate::eval::bind(&scope, "this", this.clone()); crate::eval::bind(&scope, "%fieldinit%", Value::Bool(true));
    let saved_super = std::mem::replace(&mut i.super_call_ok, false); let saved_field = std::mem::replace(&mut i.in_field_init_code, true);
    let result = field.initializer.map(|index| super::call(i, program, index, &scope, this.clone(), &[])).unwrap_or(Ok(Value::Undefined));
    i.super_call_ok = saved_super; i.in_field_init_code = saved_field;
    let mut value = result?;
    if field.named { i.set_fn_name(&value, field.key.split('\u{1}').next().unwrap_or(&field.key)); }
    for transform in &field.transforms { value = i.call(transform.clone(), this.clone(), &[value])?; }
    crate::bytecode::class_fields::define_field(i, this.clone(), &Rc::from(field.key.as_str()), false, value)
}

pub(crate) fn init_instance_fields(i: &mut Interp, class: &Rc<NativeClass>, this: &Value) -> Result<(), Abrupt> {
    if let Value::Obj(object) = this {
        if class.private_members.iter().any(|(key, _)| object.borrow().props.contains(key)) { return Err(i.throw("TypeError", "cannot initialize private methods of a class twice on the same object")); }
        if !class.private_members.is_empty() && !object.borrow().extensible { return Err(i.throw("TypeError", "cannot add private members to a non-extensible object")); }
        for (key, property) in &class.private_members { object.borrow_mut().props.insert(key.as_str(), property.clone()); }
    }
    for field in &class.fields { initialize_field(i, &class.program, &class.env, this, field)?; }
    for initializer in &class.initializers { i.call(initializer.clone(), this.clone(), &[])?; }
    Ok(())
}

pub(crate) fn constructor_on(i: &mut Interp, ctor: &Value, this: &Value, args: &[Value]) -> Result<Value, Abrupt> {
    let object = ctor.as_obj().ok_or_else(|| i.throw("TypeError", "native constructor is not an object"))?;
    let class = i.native_classes.get(&(Gc::as_ptr(object) as usize)).cloned();
    if let Some(class) = class {
        if !class.derived { init_instance_fields(i, &class, this)?; }
        if let Some(body) = class.body { return super::call(i, &class.program, body, &class.env, this.clone(), args); }
        if class.derived {
            let parent = crate::builtins::js_get_prototype_of(i, ctor).map_err(Abrupt::Throw)?;
            if !i.value_is_constructor(&parent) { return Err(i.throw("TypeError", "super constructor is not a constructor")); }
            i.pending_new_target = i.new_target.clone();
            let returned = i.run_constructor_on(&parent, this, args)?;
            let result = if matches!(returned, Value::Obj(_)) { returned } else { this.clone() };
            init_instance_fields(i, &class, &result)?;
            return Ok(result);
        }
        return Ok(Value::Undefined);
    }
    let callable = object.borrow().call.clone();
    match callable {
        Callable::Aot(native) => super::call(i, &native.program, native.function_index, &native.env, this.clone(), args),
        _ => Err(i.throw("TypeError", "value is not a native constructor")),
    }
}

pub(crate) fn construct(i: &mut Interp, ctor: Value, args: &[Value], new_target: Value) -> Result<Value, Abrupt> {
    let object = ctor.as_obj().ok_or_else(|| i.throw("TypeError", "value is not a constructor"))?;
    if !object.borrow().is_constructor { return Err(i.throw("TypeError", "value is not a constructor")); }
    let prototype = i.get_member(&new_target, "prototype")?;
    let prototype = prototype.as_obj().cloned().unwrap_or_else(|| i.object_proto.clone());
    let this = Value::Obj(Object::new(Some(prototype)));
    let saved_constructing = std::mem::replace(&mut i.constructing, true);
    let saved_target = std::mem::replace(&mut i.new_target, new_target);
    let saved_super = i.super_call_ok;
    let derived = i.native_classes.get(&(Gc::as_ptr(object) as usize)).is_some_and(|class| class.derived);
    i.super_call_ok = derived;
    let result = constructor_on(i, &ctor, &this, args);
    i.constructing = saved_constructing; i.new_target = saved_target; i.super_call_ok = saved_super;
    match result? {
        value @ Value::Obj(_) => Ok(value),
        Value::Undefined if derived => Err(i.throw("ReferenceError", "derived constructor did not initialize this")),
        _ if derived => Err(i.throw("TypeError", "derived constructors may only return object or undefined")),
        _ => Ok(this),
    }
}
