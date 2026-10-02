//! Per-operation native entry points. Generated code owns the control flow.
use super::{NativeFrame, STATUS_OK, STATUS_THROW};
use crate::interpreter::Abrupt;
use crate::value::Value;

fn publish_slot(i: &mut crate::interpreter::Interp, frame: &NativeFrame, slot: u32, value: Value) -> Result<(), Abrupt> {
    let function = &frame.program.metadata.functions[frame.function as usize];
    if function.flags & 64 == 0 { return Ok(()); }
    let name = &function.slot_names[slot as usize];
    if std::rc::Rc::ptr_eq(&frame.env, &i.global_env) && i.global_var_names.contains(name.as_str()) {
        let global = Value::Obj(i.global.clone());
        return i.set_member(&global, name, value);
    }
    if let Some(binding) = frame.env.borrow_mut().vars.get_mut(name) {
        binding.value = value;
        binding.initialized = true;
    }
    Ok(())
}

pub(crate) fn address(id: u32) -> Option<usize> {
    if id == crate::native_ops::SHAPE_GUARD { return Some(shape_guard as usize); }
    if id == crate::native_ops::SHAPE_GET_PROP { return Some(shape_get_prop as usize); }
    let index = id.checked_sub(0x1000)?;
    let helpers: &[unsafe extern "C" fn(*mut NativeFrame, u32, u32, u32, u32, u32, u32) -> u32] =
        &include!("op_addresses.rs");
    helpers.get(index as usize).map(|helper| *helper as usize)
}

pub(crate) unsafe extern "C" fn shape_guard(frame: *mut NativeFrame, lo: u32, hi: u32) -> u32 {
    let frame = unsafe { &*frame };
    let expected = u64::from(lo) | (u64::from(hi) << 32);
    u32::from(expected != 0 && frame.stack.last().is_some_and(|value| crate::native_ops::shape_hash(value) == expected))
}

pub(crate) unsafe extern "C" fn shape_get_prop(frame: *mut NativeFrame, name: u32, _cache: u32, depth: u32, resume: u32) -> u32 {
    let frame = unsafe { &mut *frame };
    frame.depth = depth; frame.resume = resume;
    match execute(frame, "GetProp", name, 0, 0, 0) {
        Ok(status) => { frame.depth = frame.stack.len() as u32; status }
        Err(error) => super::failed(frame, error),
    }
}

unsafe extern "C" fn helper<const INDEX: usize>(
    frame: *mut NativeFrame,
    a: u32,
    b: u32,
    c: u32,
    d: u32,
    depth: u32,
    resume: u32,
) -> u32 {
    let frame = unsafe { &mut *frame };
    frame.depth = depth;
    frame.resume = resume;
    match execute(frame, crate::native_ops::OP_NAMES[INDEX], a, b, c, d) {
        Ok(status) => {
            frame.depth = frame.stack.len() as u32;
            status
        }
        Err(error) => super::failed(frame, error),
    }
}

fn pop(frame: &mut NativeFrame) -> Result<Value, Abrupt> {
    frame.stack.pop().ok_or_else(|| {
        unsafe { &mut *frame.interp }.throw("Error", "native operand stack underflow")
    })
}

fn update_kind(
    i: &mut crate::interpreter::Interp,
    kind: u32,
) -> Result<crate::bytecode::UpdKind, Abrupt> {
    use crate::bytecode::UpdKind::*;
    Ok(match kind {
        0 => PreInc,
        1 => PreDec,
        2 => PostInc,
        3 => PostDec,
        4 => IncDiscard,
        5 => DecDiscard,
        _ => return Err(i.throw("Error", "invalid native update kind")),
    })
}

fn execute(
    frame: &mut NativeFrame,
    op: &str,
    a: u32,
    b: u32,
    c: u32,
    d: u32,
) -> Result<u32, Abrupt> {
    let i = unsafe { &mut *frame.interp };
    let program = frame.program.clone();
    let function = &program.metadata.functions[frame.function as usize];
    match op {
        "Const" => frame.stack.push(function.constants[a as usize].clone()),
        "Undef" => frame.stack.push(Value::Undefined),
        "Dup" => {
            let value = pop(frame)?;
            frame.stack.push(value.clone());
            frame.stack.push(value);
        }
        "Dup2" => {
            let right = pop(frame)?;
            let left = pop(frame)?;
            frame
                .stack
                .extend([left.clone(), right.clone(), left, right]);
        }
        "Pop" => {
            pop(frame)?;
        }
        "LoadLocal" => {
            let value = frame.slots[a as usize].clone();
            if matches!(value, Value::Empty) {
                return Err(i.throw(
                    "ReferenceError",
                    format!(
                        "cannot access '{}' before initialization",
                        function.slot_names[a as usize]
                    ),
                ));
            }
            frame.stack.push(value);
        }
        "StoreLocal" => {
            let value = pop(frame)?;
            frame.slots[a as usize] = value.clone();
            publish_slot(i, frame, a, value)?;
        }
        "Tdz" => frame.slots[a as usize] = Value::Empty,
        "UpdateLocal" => {
            let old = frame.slots[a as usize].clone();
            if matches!(old, Value::Empty) {
                return Err(i.throw(
                    "ReferenceError",
                    format!(
                        "cannot access '{}' before initialization",
                        function.slot_names[a as usize]
                    ),
                ));
            }
            let kind = update_kind(i, b)?;
            let mut stored = None;
            crate::bytecode::step_and_store(i, &mut frame.stack, kind, old, |_, value| {
                frame.slots[a as usize] = value.clone();
                stored = Some(value);
                Ok(())
            })?;
            if let Some(value) = stored { publish_slot(i, frame, a, value)?; }
        }
        "UpdateName" | "UpdateNameCached" => {
            let name = &function.names[a as usize];
            let old = i.get_var(name, &frame.env)?;
            let kind = update_kind(i, if op == "UpdateNameCached" { c } else { b })?;
            crate::bytecode::step_and_store(i, &mut frame.stack, kind, old, |i, value| {
                i.assign_free_name(name, value, &frame.env)
            })?;
        }
        "UpdateProp" | "UpdateElem" => {
            let key = if op == "UpdateProp" {
                function.names[a as usize].clone()
            } else {
                let key = pop(frame)?;
                i.to_property_key(&key)?
            };
            let object = pop(frame)?;
            let old = i.get_member(&object, &key)?;
            let kind = update_kind(i, if op == "UpdateProp" { c } else { a })?;
            crate::bytecode::step_and_store(i, &mut frame.stack, kind, old, |i, value| {
                i.set_member(&object, &key, value)
            })?;
        }
        "UpdateCap" => {
            let name = &function.names[a as usize];
            let old = i.get_var(name, &frame.env)?;
            let kind = update_kind(i, b)?;
            crate::bytecode::step_and_store(i, &mut frame.stack, kind, old, |i, value| {
                i.assign_free_name(name, value, &frame.env)
            })?;
        }
        "LoadName" | "LoadCap" => frame
            .stack
            .push(i.get_var(&function.names[a as usize], &frame.env)?),
        "StoreCap" | "StoreCapInit" => {
            let value = pop(frame)?;
            let name = &function.names[a as usize];
            if op == "StoreCap" {
                i.assign_free_name(name, value, &frame.env)?;
            } else {
                let mut scope = Some(frame.env.clone());
                let mut initialized = false;
                while let Some(env) = scope {
                    let mut env = env.borrow_mut();
                    if let Some(binding) = env.vars.get_mut(name) {
                        binding.value = value.clone();
                        binding.initialized = true;
                        initialized = true;
                        break;
                    }
                    scope = env.parent.clone();
                }
                if !initialized {
                    if !i.global_var_names.contains(name.as_str()) { return Err(i.throw("ReferenceError", "native captured binding missing")); }
                    let global = Value::Obj(i.global.clone());
                    i.set_member(&global, name, value)?;
                }
            }
        }
        "StoreName" | "StoreNameCached" => {
            let value = pop(frame)?;
            i.assign_free_name(&function.names[a as usize], value, &frame.env)?;
        }
        "LoadThis" => frame.stack.push(frame.this_val.clone()),
        "LoadLexicalThis" => frame.stack.push(i.lexical_this(&frame.env)?),
        "MakeClosure" => {
            let child = function.children[a as usize];
            let env = frame.pending_env.take().unwrap_or_else(|| frame.env.clone());
            let value = i.make_native_function(program.clone(), child, env);
            if b != u32::MAX {
                i.set_fn_name(&value, &function.names[b as usize]);
            }
            frame.stack.push(value);
        }
        "ArgsLen" => frame.stack.push(crate::bytecode::virt_len(
            i,
            &frame.slots,
            a as u16,
            b as u16,
        )?),
        "ArgsGet" => {
            let key = pop(frame)?;
            frame.stack.push(crate::bytecode::virt_get(
                i,
                &frame.slots,
                a as u16,
                b as u16,
                key,
            )?);
        }
        "ApplyArgs" => {
            let at = frame
                .stack
                .len()
                .checked_sub(3)
                .ok_or_else(|| i.throw("Error", "native argument stack underflow"))?;
            let value = crate::bytecode::virt_apply(
                i,
                &frame.slots,
                a as u16,
                b as u16,
                &frame.stack[at..],
            )?;
            frame.stack.truncate(at);
            frame.stack.push(value);
        }
        "LoadNameForCall" => {
            let (callee, receiver) = i.get_var_with(&function.names[a as usize], &frame.env)?;
            frame
                .stack
                .extend([receiver.unwrap_or(Value::Undefined), callee]);
        }
        "GetProp" => {
            let object = pop(frame)?;
            frame
                .stack
                .push(i.get_member(&object, &function.names[a as usize])?);
        }
        "GetPropThis" => frame
            .stack
            .push(i.get_member(&frame.this_val, &function.names[a as usize])?),
        "GetPropLocal" => frame
            .stack
            .push(i.get_member(&frame.slots[a as usize], &function.names[b as usize])?),
        "SetProp" | "SetPropDrop" => {
            let value = pop(frame)?;
            let object = pop(frame)?;
            i.set_member(&object, &function.names[a as usize], value.clone())?;
            if op == "SetProp" {
                frame.stack.push(value);
            }
        }
        "SetPropThisDrop" => {
            let value = pop(frame)?;
            i.set_member(&frame.this_val, &function.names[a as usize], value)?;
        }
        "SetPropLocalDrop" => {
            let value = pop(frame)?;
            i.set_member(&frame.slots[a as usize], &function.names[b as usize], value)?;
        }
        "GetElem" => {
            let key = pop(frame)?;
            let object = pop(frame)?;
            let key = i.to_property_key(&key)?;
            frame.stack.push(i.get_member(&object, &key)?);
        }
        "GetElemLocal" => {
            let key = pop(frame)?;
            let key = i.to_property_key(&key)?;
            frame
                .stack
                .push(i.get_member(&frame.slots[a as usize], &key)?);
        }
        "ToPropKey" | "ToPropKeyLocal" => {
            let key = frame
                .stack
                .last()
                .ok_or_else(|| i.throw("Error", "native operand stack underflow"))?;
            if !matches!(key, Value::Num(_) | Value::Str(_)) {
                let key = pop(frame)?;
                let base = if op == "ToPropKeyLocal" {
                    &frame.slots[a as usize]
                } else {
                    frame
                        .stack
                        .last()
                        .ok_or_else(|| i.throw("Error", "native operand stack underflow"))?
                };
                if matches!(base, Value::Undefined | Value::Null) {
                    return Err(i.throw("TypeError", "cannot access property of null or undefined"));
                }
                frame.stack.push(Value::str(i.to_property_key(&key)?));
            }
        }
        "GetMethod" | "GetMethodElem" => {
            let key = if op == "GetMethod" {
                function.names[a as usize].clone()
            } else {
                let key = pop(frame)?;
                i.to_property_key(&key)?
            };
            let object = pop(frame)?;
            let method = i.get_member(&object, &key)?;
            frame.stack.extend([object, method]);
        }
        "SetElem" | "SetElemDrop" | "SetElemLocal" | "SetElemLocalDrop" => {
            let value = pop(frame)?;
            let key = pop(frame)?;
            let object = if op.starts_with("SetElemLocal") {
                frame.slots[a as usize].clone()
            } else {
                pop(frame)?
            };
            let key = i.to_property_key(&key)?;
            i.set_member(&object, &key, value.clone())?;
            if !op.ends_with("Drop") {
                frame.stack.push(value);
            }
        }
        "Add" | "Sub" | "Mul" | "Div" | "Mod" | "BitAnd" | "BitOr" | "BitXor" | "Shl" | "Shr"
        | "UShr" | "Lt" | "Gt" | "Le" | "Ge" | "EqEq" | "NotEq" | "StrictEq" | "StrictNotEq"
        | "InstanceOf" | "GenBin" => {
            let operator = match op {
                "Add" => "+",
                "Sub" => "-",
                "Mul" => "*",
                "Div" => "/",
                "Mod" => "%",
                "BitAnd" => "&",
                "BitOr" => "|",
                "BitXor" => "^",
                "Shl" => "<<",
                "Shr" => ">>",
                "UShr" => ">>>",
                "Lt" => "<",
                "Gt" => ">",
                "Le" => "<=",
                "Ge" => ">=",
                "EqEq" => "==",
                "NotEq" => "!=",
                "StrictEq" => "===",
                "StrictNotEq" => "!==",
                "InstanceOf" => "instanceof",
                _ => &function.names[a as usize],
            };
            let right = pop(frame)?;
            let left = pop(frame)?;
            frame.stack.push(i.binary(operator, left, right)?);
        }
        "Neg" | "Plus" | "Not" | "BitNot" | "Typeof" | "Void" => {
            let operator = match op {
                "Neg" => "-",
                "Plus" => "+",
                "Not" => "!",
                "BitNot" => "~",
                "Typeof" => "typeof",
                _ => "void",
            };
            let value = pop(frame)?;
            frame
                .stack
                .push(crate::native_ops::unary(i, operator, value)?);
        }
        "TypeofName" => frame
            .stack
            .push(i.typeof_name_vm(&function.names[a as usize], &frame.env)?),
        "TypeofIs" => {
            let value = pop(frame)?;
            let kind = crate::bytecode::TypeofKind::of(i, &value) as u32;
            frame.stack.push(Value::Bool((kind == a) != (b != 0)));
        }
        "ArithLL" | "ArithLK" => {
            let operator = ["+", "-", "*", "/", "%", "&", "|", "^", "<<", ">>", ">>>"]
                .get(a as usize)
                .ok_or_else(|| i.throw("Error", "invalid native arithmetic kind"))?;
            let left = frame.slots[c as usize].clone();
            let right = if op == "ArithLL" {
                frame.slots[d as usize].clone()
            } else {
                function.constants[d as usize].clone()
            };
            if matches!(left, Value::Empty) || matches!(right, Value::Empty) {
                return Err(i.throw(
                    "ReferenceError",
                    "cannot access binding before initialization",
                ));
            }
            let value = i.binary(operator, left, right)?;
            frame.slots[b as usize] = value.clone();
            publish_slot(i, frame, b, value)?;
        }
        "JumpIfNotCmp" | "JumpIfNotCmpLL" | "JumpIfNotCmpLK" => {
            let operator = ["<", ">", "<=", ">=", "==", "!=", "===", "!=="]
                .get(a as usize)
                .ok_or_else(|| i.throw("Error", "invalid native comparison kind"))?;
            let (left, right) = if op == "JumpIfNotCmp" {
                let right = pop(frame)?;
                (pop(frame)?, right)
            } else {
                (
                    frame.slots[b as usize].clone(),
                    if op == "JumpIfNotCmpLL" {
                        frame.slots[c as usize].clone()
                    } else {
                        function.constants[c as usize].clone()
                    },
                )
            };
            if matches!(left, Value::Empty) || matches!(right, Value::Empty) {
                return Err(i.throw(
                    "ReferenceError",
                    "cannot access binding before initialization",
                ));
            }
            let value = i.binary(operator, left, right)?;
            return Ok(if i.to_boolean(&value) { STATUS_OK } else { 4 });
        }
        "ToStr" => {
            let value = pop(frame)?;
            frame.stack.push(Value::Str(i.to_string(&value)?));
        }
        "Call" | "CallWithThis" | "New" => {
            let at = frame
                .stack
                .len()
                .checked_sub(a as usize)
                .ok_or_else(|| i.throw("Error", "native argument stack underflow"))?;
            let below = if op == "CallWithThis" { 2 } else { 1 };
            let start = at
                .checked_sub(below)
                .ok_or_else(|| i.throw("Error", "native call stack underflow"))?;
            let callee = frame.stack[at - 1].clone();
            let value = if op == "New" {
                i.construct(callee, &frame.stack[at..])?
            } else {
                let receiver = if below == 2 {
                    frame.stack[at - 2].clone()
                } else {
                    Value::Undefined
                };
                i.call(callee, receiver, &frame.stack[at..])?
            };
            frame.stack.truncate(start);
            frame.stack.push(value);
        }
        "MakeArray" => {
            let at = frame
                .stack
                .len()
                .checked_sub(a as usize)
                .ok_or_else(|| i.throw("Error", "native array stack underflow"))?;
            let array = i.make_array_iter(frame.stack.drain(at..));
            frame.stack.push(array);
        }
        "MakeObject" => {
            let at = frame
                .stack
                .len()
                .checked_sub(b as usize)
                .ok_or_else(|| i.throw("Error", "native object stack underflow"))?;
            let keys = function.names[a as usize..a as usize + b as usize]
                .iter()
                .map(|name| std::rc::Rc::<str>::from(name.as_str()))
                .collect::<Vec<_>>();
            let values = frame.stack.split_off(at);
            frame.stack.push(i.make_plain_object_vm(&keys, values));
        }
        "MakeRegExp" => frame
            .stack
            .push(i.make_regexp_literal(
                &std::rc::Rc::from(function.names[a as usize].as_str()),
                &std::rc::Rc::from(function.names[b as usize].as_str()),
            )?),
        "ForInKeys" => {
            let base = pop(frame)?;
            let keys = i.for_in_keys(&base)?;
            frame.stack.push(i.make_array(keys));
        }
        "ForInStepL" => {
            let value =
                crate::bytecode::for_in::step(i, &mut frame.slots, a as u16, b as u16, c as u16)?;
            let more = value.is_some();
            frame
                .stack
                .extend([value.unwrap_or(Value::Undefined), Value::Bool(more)]);
        }
        "GetIter" => {
            let value = pop(frame)?;
            let (iterator, next) = i.get_iterator(&value)?;
            frame.stack.extend([iterator, next]);
        }
        "IterStepL" => {
            let value = i.iterator_step(&frame.slots[a as usize], &frame.slots[b as usize])?;
            let more = value.is_some();
            frame
                .stack
                .extend([value.unwrap_or(Value::Undefined), Value::Bool(more)]);
        }
        "IterCloseL" => i.iterator_close_normal(&frame.slots[a as usize])?,
        "IterRestL" => {
            let iterator = frame.slots[a as usize].clone();
            let next = frame.slots[b as usize].clone();
            let mut values = Vec::new();
            while let Some(value) = i.iterator_step(&iterator, &next)? {
                values.push(value);
            }
            frame.stack.push(i.make_array(values));
        }
        "IterAbortL" => {
            let error = pop(frame)?;
            i.iterator_close(&frame.slots[a as usize]);
            return Err(Abrupt::Throw(error));
        }
        "DeleteProp" => {
            let value = pop(frame)?;
            frame.stack.push(crate::native_ops::delete_property(
                i,
                value,
                &function.names[a as usize],
                b != 0,
            )?);
        }
        "DeleteElem" => {
            let key = pop(frame)?;
            let value = pop(frame)?;
            frame
                .stack
                .push(crate::native_ops::delete_element(i, value, key, a != 0)?);
        }
        "DestructureGuard" => {
            let value = frame
                .stack
                .last()
                .ok_or_else(|| i.throw("Error", "native operand stack underflow"))?;
            crate::native_ops::destructure_guard(i, value)?;
        }
        "GetPrivate" => crate::bytecode::ext_ops::get_private(
            i,
            &frame.env,
            &function.names[a as usize],
            &mut frame.stack,
        )?,
        "SetPrivate" => crate::bytecode::ext_ops::set_private(
            i,
            &frame.env,
            &function.names[a as usize],
            &mut frame.stack,
        )?,
        "GetPrivateMethod" => crate::bytecode::ext_ops::get_private_method(
            i,
            &frame.env,
            &function.names[a as usize],
            &mut frame.stack,
        )?,
        "PrivateIn" => crate::bytecode::ext_ops::private_in(
            i,
            &frame.env,
            &function.names[a as usize],
            &mut frame.stack,
        )?,
        "UpdatePrivate" => {
            let kind = update_kind(i, b)?;
            crate::bytecode::ext_ops::update_private(
                i,
                &frame.env,
                &function.names[a as usize],
                kind,
                &mut frame.stack,
            )?;
        }
        "NewObject" => frame.stack.push(Value::Obj(i.new_object())),
        "InitProp" => crate::bytecode::ext_ops::init_prop(
            i,
            &function.names[a as usize],
            b != 0,
            &mut frame.stack,
        )?,
        "InitPropComputed" => {
            crate::bytecode::ext_ops::init_prop_computed(i, a != 0, &mut frame.stack)?
        }
        "InitMethod" => {
            let key = if b == u32::MAX { let key = pop(frame)?; i.to_property_key(&key)? } else { function.names[b as usize].clone() };
            let object = frame.stack.last().and_then(Value::as_obj).cloned().ok_or_else(|| i.throw("Error", "native object literal missing"))?;
            let env = frame.pending_env.take().unwrap_or_else(|| frame.env.clone());
            super::classes::init_method(i, &program, function.children[a as usize], env, key, c, object)?;
        }
        "MakeClass" => {
            let env = frame.pending_env.take().unwrap_or_else(|| frame.env.clone());
            let name = (b != u32::MAX).then(|| function.names[b as usize].as_str());
            frame.stack.push(super::classes::make(i, &program, frame.function, a, env, name)?);
        }
        "CopyDataProps" => crate::bytecode::ext_ops::copy_data_props(i, &mut frame.stack)?,
        "SetProtoLit" => crate::bytecode::ext_ops::set_proto_lit(&mut frame.stack),
        "Nip" => {
            let value = pop(frame)?;
            pop(frame)?;
            frame.stack.push(value);
        }
        "SuperGet" => crate::bytecode::ext_ops::super_get(
            i,
            &frame.env,
            Some(&function.names[a as usize]),
            &mut frame.stack,
        )?,
        "SuperGetElem" => {
            crate::bytecode::ext_ops::super_get(i, &frame.env, None, &mut frame.stack)?
        }
        "SuperBase" => crate::bytecode::ext_ops::super_base(i, &frame.env, &mut frame.stack)?,
        "SuperCtor" => crate::bytecode::derived::super_ctor(i, &frame.env, &mut frame.stack)?,
        "SuperCall" | "SuperCallSpread" => crate::bytecode::derived::super_call(
            i,
            &frame.env,
            &mut frame.stack,
            a as usize,
            op == "SuperCallSpread",
        )?,
        "DerivedReturn" => {
            let value = pop(frame)?;
            frame.result = i.derived_construct_result(&frame.env, value)?;
        }
        "DefineField" => {
            let value = pop(frame)?;
            let receiver = pop(frame)?;
            crate::bytecode::class_fields::define_field(
                i,
                receiver,
                &std::rc::Rc::from(function.names[a as usize].as_str()),
                b != 0,
                value,
            )?;
        }
        "SuperMethod" => crate::bytecode::ext_ops::super_method(
            i,
            &frame.env,
            &frame.this_val,
            Some(&function.names[a as usize]),
            b != 0,
            &mut frame.stack,
        )?,
        "SuperMethodElem" => crate::bytecode::ext_ops::super_method(
            i,
            &frame.env,
            &frame.this_val,
            None,
            a != 0,
            &mut frame.stack,
        )?,
        "ObjRest" => {
            let keys = function.names[a as usize..a as usize + b as usize]
                .iter()
                .map(|name| std::rc::Rc::<str>::from(name.as_str()))
                .collect::<Vec<_>>();
            crate::bytecode::ext_ops::obj_rest(i, &keys, &mut frame.stack)?;
        }
        "NewArrayLit" => frame.stack.push(i.make_array(Vec::new())),
        "ArrayAppend" => {
            let value = pop(frame)?;
            let array = frame
                .stack
                .last()
                .and_then(Value::as_obj)
                .ok_or_else(|| i.throw("Error", "native array literal missing"))?;
            crate::bytecode::ext_ops::append_one(array, value);
        }
        "ArrayAppendSpread" => {
            let value = pop(frame)?;
            let items = i.iterate(&value)?;
            crate::bytecode::ext_ops::array_append(&mut frame.stack, items, false);
        }
        "ArrayHole" => {
            crate::bytecode::ext_ops::array_append(&mut frame.stack, std::iter::empty(), true)
        }
        "ArrayCbGuard" => crate::bytecode::inline_callback::guard(
            i,
            a as u8,
            frame.resume.saturating_sub(1) as usize,
            &mut frame.stack,
        ),
        "ArrayCbHas" => {
            let key = pop(frame)?;
            let array = pop(frame)?;
            frame
                .stack
                .push(Value::Bool(crate::bytecode::inline_callback::has(
                    i, &array, &key,
                )?));
        }
        "ArrayCbDone" => i.cur_site = a,
        "LoadNewTarget" => frame.stack.push(i.new_target.clone()),
        "Concat" => {
            let at = frame
                .stack
                .len()
                .checked_sub(a as usize)
                .ok_or_else(|| i.throw("Error", "native concat stack underflow"))?;
            let value = crate::bytecode::concat_strs(i, &frame.stack[at..])?;
            frame.stack.truncate(at);
            frame.stack.push(value);
        }
        "BlkNew" => {
            crate::bytecode::block_env::new_env(&frame.env, &mut frame.slots, a as u16, b as u16)
        }
        "BlkDecl" => crate::bytecode::block_env::declare(
            &frame.slots,
            a as u16,
            &std::rc::Rc::from(function.names[b as usize].as_str()),
            c != 0,
        ),
        "BlkCopy" => crate::bytecode::block_env::copy(&mut frame.slots, a as u16),
        "InEnv" => frame.pending_env = Some(crate::bytecode::block_env::env_of(&frame.slots[a as usize])),
        "BlkLoad" => frame.stack.push(crate::bytecode::block_env::load(
            i,
            &frame.slots,
            a as u16,
            &std::rc::Rc::from(function.names[b as usize].as_str()),
        )?),
        "BlkStore" | "BlkInit" => {
            let value = pop(frame)?;
            crate::bytecode::block_env::store(
                i,
                &frame.slots,
                a as u16,
                &std::rc::Rc::from(function.names[b as usize].as_str()),
                value,
                op == "BlkInit",
            )?;
        }
        "BlkUpdate" => {
            let kind = update_kind(i, c)?;
            crate::bytecode::block_env::update(
                i,
                &mut frame.stack,
                &frame.slots,
                a as u16,
                &std::rc::Rc::from(function.names[b as usize].as_str()),
                kind,
            )?;
        }
        "GetAsyncIter" => {
            let value = pop(frame)?;
            crate::bytecode::for_await::get_async_iter(i, &mut frame.stack, value)?;
        }
        "AsyncIterNext" => frame.stack.push(crate::bytecode::for_await::next(
            i,
            &mut frame.slots,
            a as u16,
            b as u16,
            c as u16,
        )?),
        "AsyncIterResult" => {
            let value = pop(frame)?;
            crate::bytecode::for_await::result(i, &mut frame.stack, &frame.slots, a as u16, value)?;
        }
        "AsyncCloseCall" => {
            crate::bytecode::for_await::close_call(
                i,
                &mut frame.stack,
                &frame.slots,
                a as u16,
                b as u16,
            )?;
            let has_return = pop(frame)?;
            return Ok(if i.to_boolean(&has_return) {
                4
            } else {
                STATUS_OK
            });
        }
        "DestructureArr" => {
            let value = pop(frame)?;
            let (iterator, next) = i.get_iterator(&value)?;
            let mut done = false;
            for _ in 0..a {
                let value = if done {
                    None
                } else {
                    i.iterator_step(&iterator, &next)?
                };
                done |= value.is_none();
                frame.stack.push(value.unwrap_or(Value::Undefined));
            }
            if !done {
                i.iterator_close_normal(&iterator)?;
            }
        }
        "CallSpread" | "CallSpreadThis" | "NewSpread" => {
            let spread = pop(frame)?;
            let count = a
                .checked_sub(1)
                .ok_or_else(|| i.throw("Error", "invalid native spread argument count"))?
                as usize;
            let at = frame
                .stack
                .len()
                .checked_sub(count)
                .ok_or_else(|| i.throw("Error", "native spread stack underflow"))?;
            let mut args = frame.stack.split_off(at);
            let (iterator, next) = i.get_iterator(&spread)?;
            while let Some(value) = i.iterator_step(&iterator, &next)? {
                args.push(value);
            }
            let callee = pop(frame)?;
            let value = if op == "NewSpread" {
                i.construct(callee, &args)?
            } else {
                let receiver = if op == "CallSpreadThis" {
                    pop(frame)?
                } else {
                    Value::Undefined
                };
                i.call(callee, receiver, &args)?
            };
            frame.stack.push(value);
        }
        "TailCall" | "TailCallSpread" => {
            let count = if op == "TailCallSpread" {
                let spread = pop(frame)?;
                let count = a
                    .checked_sub(1)
                    .ok_or_else(|| i.throw("Error", "invalid native spread argument count"))?
                    as usize;
                let at = frame
                    .stack
                    .len()
                    .checked_sub(count)
                    .ok_or_else(|| i.throw("Error", "native spread stack underflow"))?;
                let mut args = frame.stack.split_off(at);
                let (iterator, next) = i.get_iterator(&spread)?;
                while let Some(value) = i.iterator_step(&iterator, &next)? {
                    args.push(value);
                }
                let count = args.len();
                frame.stack.extend(args);
                count
            } else {
                a as usize
            };
            let at = frame
                .stack
                .len()
                .checked_sub(count)
                .ok_or_else(|| i.throw("Error", "native tail call stack underflow"))?;
            let below = 1 + usize::from(b != 0);
            let start = at
                .checked_sub(below)
                .ok_or_else(|| i.throw("Error", "native tail call stack underflow"))?;
            let callee = frame.stack[at - 1].clone();
            let receiver = if b != 0 {
                frame.stack[at - 2].clone()
            } else {
                Value::Undefined
            };
            if frame.tail_allowed && i.tco_ok {
                i.pending_tail = Some(Box::new((callee, receiver, frame.stack[at..].to_vec())));
                frame.result = Value::Undefined;
            } else {
                frame.result = i.call(callee, receiver, &frame.stack[at..])?;
            }
            frame.stack.truncate(start);
        }
        "LoadCallee" => {
            let callee = match i.fn_frames.last() {
                Some(frame) => frame.callee(),
                None => return Err(i.throw("Error", "native function frame missing")),
            };
            frame.stack.push(Value::Obj(callee));
        }
        "ImportCall" => {
            let options = if b != 0 { Some(pop(frame)?) } else { None };
            let specifier = pop(frame)?;
            frame.stack.push(super::import_call(
                i,
                &program,
                frame.function,
                specifier,
                options,
                a,
            )?);
        }
        "AppendProp" => {
            let right = pop(frame)?;
            let left = pop(frame)?;
            let object = pop(frame)?;
            let value = i.binary("+", left, right)?;
            i.set_member(&object, &function.names[a as usize], value)?;
        }
        "AsyncCloseCheck" => {
            let value = pop(frame)?;
            crate::bytecode::for_await::close_check(i, &frame.slots, a as u16, value)?;
        }
        "AsyncDelegateInit" => {
            let value = pop(frame)?;
            crate::bytecode::generator::async_init(i, &mut frame.stack, value)?;
        }
        "AsyncDelegateCall" => {
            let received = pop(frame)?;
            crate::bytecode::generator::async_call(
                i,
                &mut frame.stack,
                &mut frame.slots,
                a as u16,
                b as u16,
                c as u16,
                received,
            )?;
        }
        "AsyncDelegateResult" => {
            let value = pop(frame)?;
            crate::bytecode::generator::async_result(
                i,
                &mut frame.stack,
                &mut frame.slots,
                a as u16,
                b as u16,
                value,
            )?;
        }
        "AsyncDelegateCloseReject" => {
            let error = pop(frame)?;
            return Err(crate::bytecode::generator::async_close_reject(
                i,
                &frame.slots,
                a as u16,
                b as u16,
                error,
            ));
        }
        "AsyncDelegateSpecial" => {
            let error = if b != 0 { Some(pop(frame)?) } else { None };
            crate::bytecode::generator::async_special(i, &frame.slots, a as u16, error)?;
        }
        "Await" | "Yield" => {
            frame.parked = if op == "Await" { super::coroutines::Parked::Await } else { super::coroutines::Parked::Yield };
            frame.result = pop(frame)?;
            return Ok(super::STATUS_SUSPEND);
        }
        "InitialYield" => {
            frame.parked = super::coroutines::Parked::Start;
            frame.result = Value::Undefined;
            return Ok(super::STATUS_SUSPEND);
        }
        "YieldDelegate" => {
            if let Some(value) = crate::bytecode::generator::delegate_step(
                i,
                &mut frame.slots,
                a as u16,
                &mut frame.stack,
            )? {
                frame.parked = super::coroutines::Parked::Delegate;
                frame.result = value;
                return Ok(super::STATUS_SUSPEND);
            }
        }
        // The native CFG retains the following comparison chain as its switch fallback.
        "SwitchLK" | "PushHandler" | "PopHandler" => {}
        "Jump" => {}
        "JumpIfFalse" => {
            let value = pop(frame)?;
            return Ok(if i.to_boolean(&value) { STATUS_OK } else { 4 });
        }
        "JumpIfFalsePeek" | "JumpIfTruePeek" | "JumpIfNotNullishPeek" => {
            let value = frame
                .stack
                .last()
                .ok_or_else(|| i.throw("Error", "native operand stack underflow"))?;
            let taken = match op {
                "JumpIfFalsePeek" => !i.to_boolean(value),
                "JumpIfTruePeek" => i.to_boolean(value),
                _ => !matches!(value, Value::Undefined | Value::Null),
            };
            return Ok(if taken { 4 } else { STATUS_OK });
        }
        "Throw" => {
            frame.exception = pop(frame)?;
            return Ok(STATUS_THROW);
        }
        "Return" => frame.result = pop(frame)?,
        "ReturnUndef" => frame.result = Value::Undefined,
        _ => return Err(i.throw("Error", format!("invalid native operation {op}"))),
    }
    Ok(STATUS_OK)
}
