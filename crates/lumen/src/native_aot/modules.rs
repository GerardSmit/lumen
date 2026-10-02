//! AST-free module instantiation and closed-world evaluation.

use super::{metadata, NativeProgram, UnitState};
use crate::interpreter::{new_var_scope, Abrupt, Binding, Interp};
use crate::modules::NsBinding;
use crate::value::{Gc, Object, Property, Value};
use std::collections::BTreeSet;
use std::rc::Rc;

#[derive(Clone, PartialEq, Eq)]
enum Target {
    Local(u32, String),
    Namespace(u32),
    Native(String, String),
    NativeNamespace(String),
}

fn linked(unit: &metadata::Unit, source: &str) -> Option<u32> {
    unit.links
        .iter()
        .find(|(name, _)| name == source)
        .map(|(_, index)| *index)
}

fn resolve(
    interp: &Interp,
    data: &metadata::Metadata,
    unit: u32,
    name: &str,
    seen: &mut Vec<(u32, String)>,
) -> Option<Target> {
    if seen
        .iter()
        .any(|(index, key)| *index == unit && key == name)
    {
        return None;
    }
    seen.push((unit, name.to_owned()));
    let record = &data.units[unit as usize];
    for export in &record.exports {
        if export.exported != name || export.kind == 2 {
            continue;
        }
        let target = match export.kind {
            0 => {
                if let Some((source, spec)) = record.imports.iter().find_map(|import| {
                    import
                        .specs
                        .iter()
                        .find(|(_, _, local)| local == &export.local)
                        .map(|spec| (&import.source, spec))
                }) {
                    if spec.0 == 1 || spec.0 == 2 || spec.0 == 3 {
                        linked(record, source).map(Target::Namespace).or_else(|| interp.native_module_names.contains(source).then(|| Target::NativeNamespace(source.clone())))
                    } else {
                        linked(record, source).and_then(|dep| resolve(interp, data, dep, &spec.1, seen)).or_else(|| native_target(interp, source, &spec.1))
                    }
                } else {
                    Some(Target::Local(unit, export.local.clone()))
                }
            }
            1 => linked(record, &export.source).and_then(|dep| resolve(interp, data, dep, &export.local, seen)).or_else(|| native_target(interp, &export.source, &export.local)),
            3 => linked(record, &export.source).map(Target::Namespace).or_else(|| interp.native_module_names.contains(&export.source).then(|| Target::NativeNamespace(export.source.clone()))),
            _ => None,
        };
        seen.pop();
        return target;
    }
    if name != "default" {
        let mut found = None;
        for export in record.exports.iter().filter(|export| export.kind == 2) {
            let target = linked(record, &export.source).and_then(|dep| resolve(interp, data, dep, name, seen)).or_else(|| native_target(interp, &export.source, name));
            if let Some(target) = target {
                if found.as_ref().is_some_and(|old| old != &target) {
                    seen.pop();
                    return None;
                }
                found = Some(target);
            }
        }
        seen.pop();
        return found;
    }
    seen.pop();
    None
}

fn native_target(interp: &Interp, namespace: &str, name: &str) -> Option<Target> {
    if !interp.native_module_names.contains(namespace) { return None; }
    let object = interp.modules.get(namespace)?.as_obj()?;
    object.borrow().props.contains(name).then(|| Target::Native(namespace.into(), name.into()))
}

fn native_binding(interp: &mut Interp, namespace: &str, name: &str) -> Result<NsBinding, Abrupt> {
    let namespace = interp.modules[namespace].clone();
    if let Some(object) = namespace.as_obj() {
        if let Some(binding) = interp.module_ns.get(&(Gc::as_ptr(object) as usize)).and_then(|bindings| bindings.get(name)) {
            return Ok(binding.clone());
        }
    }
    Ok(NsBinding::Static(interp.get_member(&namespace, name)?))
}

fn bind_native(interp: &mut Interp, env: &crate::interpreter::Env, namespace: &str, name: &str, local: &str) -> Result<(), Abrupt> {
    match native_binding(interp, namespace, name)? {
        NsBinding::Live(target, key) => env.borrow_mut().link_import(local, target, key),
        NsBinding::Static(value) => { env.borrow_mut().vars.insert(local, Binding::data(value, false, true)); }
    }
    Ok(())
}

fn names(interp: &Interp, data: &metadata::Metadata, unit: u32, seen: &mut Vec<u32>) -> BTreeSet<String> {
    if seen.contains(&unit) {
        return BTreeSet::new();
    }
    seen.push(unit);
    let record = &data.units[unit as usize];
    let mut result = BTreeSet::new();
    for export in &record.exports {
        if export.kind == 2 {
            if let Some(dep) = linked(record, &export.source) {
                result.extend(
                    names(interp, data, dep, seen)
                        .into_iter()
                        .filter(|name| name != "default"),
                );
            } else if interp.native_module_names.contains(&export.source) {
                if let Some(object) = interp.modules.get(&export.source).and_then(Value::as_obj) {
                    result.extend(object.borrow().props.keys().into_iter().filter(|key| key.as_ref() != "default" && !Interp::is_sym_key(key)).map(|key| key.to_string()));
                }
            }
        } else {
            result.insert(export.exported.clone());
        }
    }
    seen.pop();
    result
}

fn states<'a>(interp: &'a Interp, program: &NativeProgram) -> &'a [UnitState] {
    &interp.native_units[&program.image.blob_hash()]
}

fn cjs_exports(interp: &mut Interp, program: &NativeProgram, unit: u32) -> Result<Value, Abrupt> {
    let env = states(interp, program)[unit as usize].env.clone();
    let module = interp.get_var("%cjsmodule%", &env)?;
    interp.get_member(&module, "exports")
}

pub(super) fn require(interp: &mut Interp, program: &Rc<NativeProgram>, owner: Option<u32>, value: Value) -> Result<Value, Abrupt> {
    let name = interp.to_string(&value)?;
    let target = if let Some(owner) = owner {
        let record = &program.metadata.units[owner as usize];
        if !record.requires.iter().any(|specifier| specifier == name.as_str()) {
            return Err(interp.throw("EvalError", "require target is not in the native image"));
        }
        linked(record, name.as_str())
    } else {
        let path = name.strip_prefix("aot:/").unwrap_or(name.as_str());
        program.metadata.units.iter().position(|unit| unit.kind == 2 && unit.path == path).map(|index| index as u32)
    };
    if let Some(target) = target {
        if program.metadata.units[target as usize].kind == 2 && states(interp, program)[target as usize].evaluating {
            return cjs_exports(interp, program, target);
        }
        let result = run_unit(interp, program, target)?;
        if program.metadata.units[target as usize].kind != 2 && matches!(crate::eval::promise_fast::promise_state(&result), Some((crate::eval::promise_fast::PENDING, _))) {
            return Err(interp.throw("TypeError", "require cannot load an asynchronous native module"));
        }
        if program.metadata.units[target as usize].kind == 2 { cjs_exports(interp, program, target) }
        else { Ok(states(interp, program)[target as usize].namespace.clone()) }
    } else if interp.native_module_names.contains(name.as_str()) {
        let namespace = interp.modules[name.as_str()].clone();
        let default = interp.get_member(&namespace, "default")?;
        Ok(if matches!(default, Value::Undefined) { namespace } else { default })
    }
    else { Err(interp.throw("EvalError", "require target is not in the native image")) }
}

pub(super) fn make_require(interp: &mut Interp, program: &Rc<NativeProgram>, unit: u32) -> Result<Value, String> {
    if program.metadata.units.get(unit as usize).is_none_or(|record| record.kind != 2) { return Err("snapshot require references a non-CommonJS unit".into()); }
    let program = program.clone();
    Ok(interp.new_native_fn("require", 1, Rc::new(move |interp, _, args| {
        require(interp, &program, Some(unit), args.first().cloned().unwrap_or(Value::Undefined)).map_err(crate::interpreter::abrupt_value)
    })))
}

pub(super) fn install_snapshot_states(
    interp: &mut Interp,
    program: &Rc<NativeProgram>,
    restored: &super::snapshot::Restored,
) -> Result<(), String> {
    instantiate(interp, program).map_err(|error| format!("snapshot module instantiation: {}", interp.to_string(&crate::interpreter::abrupt_value(error)).map_or_else(|_| "failed".into(), |value| value.to_string())))?;
    let hash = program.image.blob_hash();
    let mut initial = interp.native_units.remove(&hash).ok_or("missing snapshot unit state")?.into_iter();
    let mut states = Vec::with_capacity(program.metadata.units.len());
    for (index, unit) in program.metadata.units.iter().enumerate() {
        let fresh = initial.next().ok_or("missing snapshot unit state")?;
        let name = format!("{}:{}", if unit.kind == 2 { "cjs" } else { "module" }, unit.path);
        if unit.kind == 2 && !restored.environments.contains_key(&name) {
            states.push(fresh);
            continue;
        }
        let env = if unit.kind == 0 {
            restored.environments.get("global")
        } else {
            restored.environments.get(&name)
        }
        .ok_or_else(|| format!("snapshot is missing environment {name}"))?
        .clone();
        let namespace = if unit.kind == 2 {
            let module = restored.roots.get(&name).ok_or_else(|| format!("snapshot is missing CommonJS module {name}"))?.clone();
            interp.get_member(&module, "exports").map_err(|_| format!("invalid CommonJS module {name}"))?
        } else if unit.kind == 0 {
            Value::Undefined
        } else {
            restored
                .roots
                .get(&name)
                .ok_or_else(|| format!("snapshot is missing namespace {name}"))?
                .clone()
        };
        states.push(UnitState {
            env,
            namespace,
            evaluation: None,
            cycle_root: index as u32,
            visiting: false,
            waiting: 0,
            evaluating: false,
            evaluated: true,
        });
    }
    interp
        .native_units
        .insert(program.image.blob_hash(), states);
    Ok(())
}

fn instantiate(interp: &mut Interp, program: &Rc<NativeProgram>) -> Result<(), Abrupt> {
    let hash = program.image.blob_hash();
    if interp.native_units.contains_key(&hash) {
        return Ok(());
    }
    let mut units = Vec::with_capacity(program.metadata.units.len());
    for (index, unit) in program.metadata.units.iter().enumerate() {
        let env = if unit.kind == 0 {
            interp.global_env.clone()
        } else {
            new_var_scope(Some(interp.global_env.clone()))
        };
        let namespace = Value::Obj(if unit.kind == 2 { interp.new_object() } else { Object::new_bare(None) });
        units.push(UnitState {
            env,
            namespace,
            evaluation: None,
            cycle_root: index as u32,
            visiting: false,
            waiting: 0,
            evaluating: false,
            evaluated: false,
        });
    }
    interp.native_units.insert(hash, units);
    for (index, unit) in program.metadata.units.iter().enumerate() {
        let env = states(interp, program)[index].env.clone();
        if unit.kind == 0 {
            for declaration in &unit.declarations {
                let name = declaration.name.as_str();
                if env.borrow().vars.contains_key(name) {
                    return Err(interp.throw("SyntaxError", format!("redeclaration of global binding '{name}'")));
                }
                let global = interp.global.borrow();
                let property = global.props.get(name);
                let allowed = match declaration.kind {
                    0 => property.is_some() || global.extensible,
                    3 => property.map_or(global.extensible, |property| property.configurable() || (!property.accessor() && property.writable() && property.enumerable())),
                    _ => !interp.global_var_names.contains(name) && property.is_none_or(|property| property.configurable()),
                };
                drop(global);
                if !allowed { return Err(interp.throw(if matches!(declaration.kind, 0 | 3) { "TypeError" } else { "SyntaxError" }, format!("cannot declare global binding '{name}'"))); }
            }
        }
        if unit.kind != 0 {
            let meta = interp.build_import_meta(&format!("aot:/{}", unit.path));
            crate::eval::bind(&env, "%importmeta%", meta);
        }
        if unit.kind == 2 {
            let exports = states(interp, program)[index].namespace.clone();
            let module = Value::Obj(interp.new_object());
            module.as_obj().unwrap().borrow_mut().props.insert("exports", Property::plain(exports.clone()));
            crate::eval::bind(&env, "%cjsmodule%", module.clone());
            crate::eval::bind(&env, "module", module);
            crate::eval::bind(&env, "exports", exports);
            crate::eval::bind(&env, "__filename", Value::from_string(unit.path.clone()));
            let directory = unit.path.rsplit_once('/').map_or("", |(directory, _)| directory);
            crate::eval::bind(&env, "__dirname", Value::from_string(directory.into()));
            let program = program.clone();
            let require = interp.new_native_fn("require", 1, Rc::new(move |interp, _, args| {
                require(interp, &program, Some(index as u32), args.first().cloned().unwrap_or(Value::Undefined)).map_err(crate::interpreter::abrupt_value)
            }));
            crate::eval::bind(&env, "require", require);
        }
        for declaration in &unit.declarations {
            if unit.kind == 2 && declaration.kind == 0 && matches!(declaration.name.as_str(), "module" | "exports" | "require" | "__filename" | "__dirname") { continue; }
            let (mutable, initialized) = match declaration.kind {
                0 | 3 => (true, true),
                1 | 5 | 6 => (true, false),
                _ => (false, false),
            };
            let value = declaration.function.map_or(Value::Undefined, |function| {
                interp.make_native_function(program.clone(), function, env.clone())
            });
            if unit.kind == 0 && matches!(declaration.kind, 0 | 3) {
                interp.global_var_names.insert(declaration.name.clone());
                let mut global = interp.global.borrow_mut();
                match global.props.get_mut(declaration.name.as_str()) {
                    None => { global.props.insert(declaration.name.as_str(), Property::data(value, true, true, false)); }
                    Some(property) if declaration.kind == 3 && property.configurable() => { *property = Property::data(value, true, true, false); }
                    Some(property) if declaration.kind == 3 => property.set_value(value),
                    _ => {}
                }
                continue;
            }
            env.borrow_mut().vars.insert(
                declaration.name.as_str(),
                Binding::data(value, mutable, initialized),
            );
            if matches!(declaration.kind, 1 | 2 | 4 | 5 | 6) {
                env.borrow_mut()
                    .push_lexical_name(Rc::from(declaration.name.as_str()));
            }
        }
    }
    // Every local target exists before imports are linked, so cycles are legal.
    for (index, unit) in program.metadata.units.iter().enumerate() {
        let env = states(interp, program)[index].env.clone();
        for import in &unit.imports {
            let Some(dep) = linked(unit, &import.source) else {
                if !interp.native_module_names.contains(&import.source) {
                    return Err(interp.throw("SyntaxError", "native import has no bundled target"));
                }
                let namespace = interp.modules[&import.source].clone();
                for (tag, imported, local) in &import.specs {
                    if matches!(tag, 0 | 4) {
                        if native_target(interp, &import.source, imported).is_none() { return Err(interp.throw("SyntaxError", format!("unresolved native import {imported}"))); }
                        bind_native(interp, &env, &import.source, imported, local)?;
                    } else { env.borrow_mut().vars.insert(local.as_str(), Binding::data(namespace.clone(), false, true)); }
                }
                continue;
            };
            for (tag, imported, local) in &import.specs {
                match tag {
                    0 | 4 => match resolve(interp, &program.metadata, dep, imported, &mut Vec::new()) {
                        Some(Target::Local(owner, name)) => {
                            let target = states(interp, program)[owner as usize].env.clone();
                            env.borrow_mut().link_import(local, target, name);
                        }
                        Some(Target::Namespace(owner)) => {
                            let namespace =
                                states(interp, program)[owner as usize].namespace.clone();
                            env.borrow_mut()
                                .vars
                                .insert(local.as_str(), Binding::data(namespace, false, true));
                        }
                        Some(Target::Native(namespace, name)) => bind_native(interp, &env, &namespace, &name, local)?,
                        Some(Target::NativeNamespace(namespace)) => { env.borrow_mut().vars.insert(local.as_str(), Binding::data(interp.modules[&namespace].clone(), false, true)); }
                        None => {
                            return Err(interp.throw(
                                "SyntaxError",
                                format!("unresolved native import {imported}"),
                            ))
                        }
                    },
                    1 | 2 | 3 => {
                        let namespace = states(interp, program)[dep as usize].namespace.clone();
                        env.borrow_mut()
                            .vars
                            .insert(local.as_str(), Binding::data(namespace, false, true));
                    }
                    _ => unreachable!("validated native import tag"),
                }
            }
        }
    }
    for (index, unit) in program.metadata.units.iter().enumerate() {
        let namespace = states(interp, program)[index].namespace.clone();
        if unit.kind == 2 { continue; }
        let Value::Obj(object) = &namespace else {
            unreachable!()
        };
        let mut live = crate::fasthash::FastMap::default();
        for name in names(interp, &program.metadata, index as u32, &mut Vec::new()) {
            match resolve(interp, &program.metadata, index as u32, &name, &mut Vec::new()) {
                Some(Target::Local(owner, local)) => {
                    let env = states(interp, program)[owner as usize].env.clone();
                    live.insert(name.clone(), NsBinding::Live(env, local));
                    object.borrow_mut().props.insert(
                        name.as_str(),
                        Property::data(Value::Undefined, true, true, false),
                    );
                }
                Some(Target::Namespace(owner)) => {
                    let value = states(interp, program)[owner as usize].namespace.clone();
                    live.insert(name.clone(), NsBinding::Static(value.clone()));
                    object
                        .borrow_mut()
                        .props
                        .insert(name.as_str(), Property::data(value, true, true, false));
                }
                Some(Target::Native(namespace, imported)) => {
                    live.insert(name.clone(), native_binding(interp, &namespace, &imported)?);
                    object.borrow_mut().props.insert(name.as_str(), Property::data(Value::Undefined, true, true, false));
                }
                Some(Target::NativeNamespace(namespace)) => {
                    let value = interp.modules[&namespace].clone();
                    live.insert(name.clone(), NsBinding::Static(value.clone()));
                    object.borrow_mut().props.insert(name.as_str(), Property::data(value, true, true, false));
                }
                None if unit.exports.iter().any(|export| export.kind != 2 && export.exported == name) => {
                    return Err(interp.throw("SyntaxError", format!("unresolved native export {name}")));
                }
                None => {}
            }
        }
        if let Some(tag) = crate::builtins::to_string_tag_key(interp) {
            object.borrow_mut().props.insert(
                tag,
                Property::data(Value::from_string("Module".into()), false, false, false),
            );
        }
        object.borrow_mut().extensible = false;
        object.borrow().ic_plain.set(false);
        interp.gc_pin(object);
        interp.module_ns.insert(Gc::as_ptr(object) as usize, live);
        interp.modules.insert(unit.path.clone(), namespace);
    }
    Ok(())
}

pub(super) fn run_unit(
    interp: &mut Interp,
    program: &Rc<NativeProgram>,
    unit: u32,
) -> Result<Value, Abrupt> {
    let value = run_member(interp, program, unit)?;
    let root = states(interp, program)[unit as usize].cycle_root;
    let evaluation = states(interp, program)[root as usize].evaluation.clone();
    if let Some(promise) = evaluation {
        match crate::eval::promise_fast::promise_state(&promise) {
            Some((crate::eval::promise_fast::REJECTED, error)) => Err(Abrupt::Throw(error)),
            Some((crate::eval::promise_fast::FULFILLED, _)) => Ok(value),
            _ => {
                let result = interp.new_promise();
                let namespace = states(interp, program)[unit as usize].namespace.clone();
                let script = program.metadata.units[unit as usize].kind == 0;
                let fulfilled = interp.new_native_fn("", 1, Rc::new(move |_, _, args| Ok(if script { args.first().cloned().unwrap_or(Value::Undefined) } else { namespace.clone() })));
                interp.perform_then(promise.as_obj().unwrap(), crate::eval::promise_fast::Reaction::then(fulfilled, Value::Undefined, result.clone(), interp.async_context.clone()));
                Ok(result)
            }
        }
    } else { Ok(value) }
}

fn run_member(interp: &mut Interp, program: &Rc<NativeProgram>, unit: u32) -> Result<Value, Abrupt> {
    instantiate(interp, program)?;
    let Some(record) = program.metadata.units.get(unit as usize) else {
        return Err(interp.throw("TypeError", "native unit index is out of range"));
    };
    let hash = program.image.blob_hash();
    if states(interp, program)[unit as usize].evaluated {
        return if record.kind == 2 { cjs_exports(interp, program, unit) }
        else { Ok(states(interp, program)[unit as usize].namespace.clone()) };
    }
    if let Some(promise) = &states(interp, program)[unit as usize].evaluation {
        return match crate::eval::promise_fast::promise_state(promise) {
            Some((crate::eval::promise_fast::REJECTED, error)) => Err(Abrupt::Throw(error)),
            _ => Ok(promise.clone()),
        };
    }
    interp.native_units.get_mut(&hash).unwrap()[unit as usize].evaluating = true;
    let root = states(interp, program)[unit as usize].cycle_root;
    if states(interp, program)[root as usize].evaluation.is_none() {
        for index in 0..program.metadata.units.len() {
            if reaches(program, unit, index as u32, &mut BTreeSet::new()) && reaches(program, index as u32, unit, &mut BTreeSet::new()) {
                interp.native_units.get_mut(&hash).unwrap()[index].cycle_root = unit;
            }
        }
    }
    let promise = interp.new_promise();
    interp.native_units.get_mut(&hash).unwrap()[unit as usize].evaluation = Some(promise.clone());
    advance(interp, program, unit, Some(0));
    match crate::eval::promise_fast::promise_state(&promise) {
        Some((crate::eval::promise_fast::FULFILLED, value)) => match record.kind {
            0 => Ok(value),
            2 => cjs_exports(interp, program, unit),
            _ => Ok(states(interp, program)[unit as usize].namespace.clone()),
        },
        Some((crate::eval::promise_fast::REJECTED, error)) => Err(Abrupt::Throw(error)),
        _ => Ok(promise),
    }
}

fn settle(interp: &mut Interp, program: &Rc<NativeProgram>, unit: u32, result: Result<Value, Abrupt>) {
    let hash = program.image.blob_hash();
    let state = &mut interp.native_units.get_mut(&hash).unwrap()[unit as usize];
    state.evaluating = false;
    state.visiting = false;
    let promise = state.evaluation.as_ref().unwrap().clone();
    let result = result.and_then(|value| match program.metadata.units[unit as usize].kind {
        0 => Ok(value),
        2 => cjs_exports(interp, program, unit),
        _ => Ok(states(interp, program)[unit as usize].namespace.clone()),
    });
    interp.native_units.get_mut(&hash).unwrap()[unit as usize].evaluated = result.is_ok();
    match result {
        Ok(value) => interp.resolve_promise(&promise, if program.metadata.units[unit as usize].kind == 0 { value } else { Value::Undefined }),
        Err(error) => {
            interp.reject_promise(&promise, crate::interpreter::abrupt_value(error));
            // The unit capability is internal; the import/entry promise reports failures.
            if let Some(object) = promise.as_obj() {
                let tracked = match &mut object.borrow_mut().call {
                    crate::value::Callable::Promise(state) => std::mem::replace(&mut state.tracked, false),
                    _ => false,
                };
                if tracked { interp.note_rejection_handled(object); }
            }
        },
    }
}

fn eager(record: &metadata::Unit, specifier: &str) -> bool {
    record.imports.iter().any(|import| import.source == specifier
        && (import.specs.is_empty() || import.specs.iter().any(|(tag, _, _)| *tag != 2)))
        || record.exports.iter().any(|export| export.source == specifier)
}

fn reaches(program: &NativeProgram, from: u32, target: u32, seen: &mut BTreeSet<u32>) -> bool {
    if from == target { return true; }
    if !seen.insert(from) { return false; }
    let record = &program.metadata.units[from as usize];
    record.links.iter().any(|(specifier, dep)| eager(record, specifier) && reaches(program, *dep, target, seen))
}

fn subscribe(interp: &mut Interp, program: &Rc<NativeProgram>, unit: u32, awaited: Value, dependency: bool) {
    let fulfilled_program = program.clone();
    let fulfilled = interp.new_native_fn("", 1, Rc::new(move |interp, _, args| {
        if dependency {
            let state = &mut interp.native_units.get_mut(&fulfilled_program.image.blob_hash()).unwrap()[unit as usize];
            state.waiting -= 1;
            if state.waiting == 0 && !state.visiting { advance(interp, &fulfilled_program, unit, None); }
        }
        else { settle(interp, &fulfilled_program, unit, Ok(args.first().cloned().unwrap_or(Value::Undefined))); }
        Ok(Value::Undefined)
    }));
    let rejected_program = program.clone();
    let rejected = interp.new_native_fn("", 1, Rc::new(move |interp, _, args| {
        settle(interp, &rejected_program, unit, Err(Abrupt::Throw(args.first().cloned().unwrap_or(Value::Undefined))));
        Ok(Value::Undefined)
    }));
    interp.perform_then(awaited.as_obj().unwrap(), crate::eval::promise_fast::Reaction::then(fulfilled, rejected, Value::Undefined, interp.async_context.clone()));
}

fn advance(interp: &mut Interp, program: &Rc<NativeProgram>, unit: u32, start: Option<usize>) {
    let record = &program.metadata.units[unit as usize];
    if !matches!(states(interp, program)[unit as usize].evaluation.as_ref().and_then(crate::eval::promise_fast::promise_state), Some((crate::eval::promise_fast::PENDING, _))) { return; }
    let result = (|| {
      if let Some(start) = start {
        interp.native_units.get_mut(&program.image.blob_hash()).unwrap()[unit as usize].visiting = true;
        for (specifier, dep) in record.links.iter().skip(start) {
            if eager(record, specifier) {
                // A back edge sees the already-instantiated namespace, preserving cycles.
                if states(interp, program)[*dep as usize].visiting { continue; }
                run_member(interp, program, *dep)?;
                let dependency = states(interp, program)[*dep as usize].evaluation.clone().unwrap_or(Value::Undefined);
                match crate::eval::promise_fast::promise_state(&dependency) {
                    Some((crate::eval::promise_fast::REJECTED, error)) => return Err(Abrupt::Throw(error)),
                    Some((crate::eval::promise_fast::PENDING, _)) => {
                        interp.native_units.get_mut(&program.image.blob_hash()).unwrap()[unit as usize].waiting += 1;
                        subscribe(interp, program, unit, dependency, true);
                    }
                    _ => {}
                }
            }
        }
        interp.native_units.get_mut(&program.image.blob_hash()).unwrap()[unit as usize].visiting = false;
        if states(interp, program)[unit as usize].waiting != 0 { return Ok(None); }
      }
        let env = states(interp, program)[unit as usize].env.clone();
        let saved_require = interp.global.borrow().props.get("require").map(|prop| (*prop).clone());
        let native_program = program.clone();
        let native_require = interp.new_native_fn("require", 1, Rc::new(move |interp, _, args| {
            require(interp, &native_program, None, args.first().cloned().unwrap_or(Value::Undefined)).map_err(crate::interpreter::abrupt_value)
        }));
        interp.global.borrow_mut().props.insert("require", Property::plain(native_require));
        let this = if record.kind == 2 { states(interp, program)[unit as usize].namespace.clone() } else { Value::Undefined };
        let result = super::call(
            interp,
            program,
            record.top_function,
            &env,
            this,
            &[],
        );
        if let Some(saved) = saved_require { interp.global.borrow_mut().props.insert("require", saved); }
        else { interp.global.borrow_mut().props.remove("require"); }
        let value = result?;
        if program.metadata.functions[record.top_function as usize].flags & 8 != 0 {
            match crate::eval::promise_fast::promise_state(&value) {
                Some((crate::eval::promise_fast::FULFILLED, result)) => return Ok(Some(result)),
                Some((crate::eval::promise_fast::REJECTED, error)) => return Err(Abrupt::Throw(error)),
                Some(_) => { subscribe(interp, program, unit, value, false); return Ok(None); }
                None => return Err(interp.throw("Error", "native async unit did not return a promise")),
            }
        }
        Ok(Some(value))
    })();
    match result {
        Ok(Some(value)) => settle(interp, program, unit, Ok(value)),
        Err(error) => settle(interp, program, unit, Err(error)),
        Ok(None) => {}
    }
}

pub(super) fn import_call(
    interp: &mut Interp,
    program: &Rc<NativeProgram>,
    current_function: u32,
    specifier: Value,
    options: Option<Value>,
    phase: u32,
) -> Result<Value, Abrupt> {
    let promise = interp.new_promise();
    let name = match interp.to_string(&specifier) {
        Ok(name) => name,
        Err(error) => {
            interp.reject_promise(&promise, crate::interpreter::abrupt_value(error));
            return Ok(promise);
        }
    };
    let owner = program
        .metadata
        .units
        .iter()
        .enumerate()
        .rev()
        .find(|(_, unit)| unit.top_function <= current_function)
        .map(|(index, _)| index);
    let target = owner
        .and_then(|owner| linked(&program.metadata.units[owner], name.as_str()))
        .or_else(|| {
            program
                .metadata
                .units
                .iter()
                .position(|unit| unit.path == name.as_str())
                .map(|n| n as u32)
        });
    let result = if phase == 1 || options.is_some() {
        Err(interp.throw(
            "SyntaxError",
            "native import phase or attributes are unavailable",
        ))
    } else if let Some(target) = target {
        run_unit(interp, program, target)
    } else if interp.native_module_names.contains(name.as_str()) {
        Ok(interp.modules[name.as_str()].clone())
    } else {
        Err(interp.throw("EvalError", "source is not in the native image"))
    };
    match result {
        Ok(namespace) => interp.resolve_promise(&promise, namespace),
        Err(error) => interp.reject_promise(&promise, crate::interpreter::abrupt_value(error)),
    }
    Ok(promise)
}
