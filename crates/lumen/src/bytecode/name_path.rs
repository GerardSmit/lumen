//! Hash-free lexical resolution through guarded scope paths, including fresh activations.
use super::Chunk;
use crate::interpreter::{Binding, BindingLayout, Env, Interp, Scope};
use crate::value::{Exotic, Gc, Value};
use std::cell::RefCell;
use std::rc::{Rc, Weak};

const MAX_DEPTH: usize = 8;

enum ScopeGuard {
    Layout(Weak<BindingLayout>),
    Exact {
        scope: Weak<RefCell<Scope>>,
        generation: u32,
    },
}

impl ScopeGuard {
    fn new(env: &Env, scope: &Scope) -> Self {
        match scope.vars.template_layout() {
            Some(layout) => Self::Layout(Rc::downgrade(layout)),
            None => Self::Exact {
                scope: Rc::downgrade(env),
                generation: scope.vars.generation(),
            },
        }
    }

    fn matches(&self, pointer: *const RefCell<Scope>, scope: &Scope) -> bool {
        if scope.with_obj.is_some() {
            return false;
        }
        match self {
            Self::Layout(expected) => scope
                .vars
                .template_layout()
                .is_some_and(|layout| Rc::as_ptr(layout) == expected.as_ptr()),
            Self::Exact {
                scope: expected,
                generation,
            } => pointer == expected.as_ptr() && scope.vars.generation() == *generation,
        }
    }
}

enum Holder {
    /// The final exact-scope guard proves this entry has not moved.
    Binding(*const Binding),
    /// The final layout guard proves the index names the same binding in a fresh activation.
    Slot(usize),
    Global {
        object: crate::value::WeakGc,
        shape: u32,
        slot: usize,
    },
}

pub(super) struct NamePath {
    guards: Vec<ScopeGuard>,
    holder: Holder,
}

impl NamePath {
    /// The scope the path ends in, when every guard on the way still holds.
    fn resolve(&self, env: &Env) -> Option<*const RefCell<Scope>> {
        let mut pointer = Rc::as_ptr(env);
        for (index, guard) in self.guards.iter().enumerate() {
            let scope = unsafe { &*pointer }.try_borrow().ok()?;
            if !guard.matches(pointer, &scope) {
                return None;
            }
            if index + 1 == self.guards.len() {
                return Some(pointer);
            }
            pointer = Rc::as_ptr(scope.parent.as_ref()?);
        }
        None
    }

    /// The binding a scope-holder path resolves to, when it is initialized, no import, and (for
    /// `write`) mutable. Its address is stable until JS runs (only JS restructures a scope).
    fn binding_ptr(&self, env: &Env, write: bool) -> Option<*mut Binding> {
        let pointer = self.resolve(env)?;
        let b: *mut Binding = match &self.holder {
            Holder::Binding(p) => *p as *mut Binding,
            Holder::Slot(slot) => {
                let mut scope = unsafe { &*pointer }.try_borrow_mut().ok()?;
                let layout = scope.vars.template_layout()?.clone();
                scope.vars.layout_binding_mut(&layout, *slot)? as *mut Binding
            }
            Holder::Global { .. } => return None,
        };
        let bd = unsafe { &*b };
        (bd.initialized && bd.import_ref.is_none() && (!write || bd.mutable)).then_some(b)
    }

    /// The global object's entry slot a global-holder path resolves to, while it is a data
    /// property at the recorded shape.
    fn global_slot(&self, interp: &Interp, env: &Env) -> Option<usize> {
        let Holder::Global {
            object,
            shape,
            slot,
        } = &self.holder
        else {
            return None;
        };
        let pointer = self.resolve(env)?;
        if pointer != Rc::as_ptr(&interp.global_env) || object.as_ptr() != Gc::as_ptr(&interp.global) {
            return None;
        }
        global_value(interp, &interp.global, *shape, *slot)?;
        Some(*slot)
    }

    fn read(&self, interp: &Interp, env: &Env) -> Option<Value> {
        // The caller's strong env handle owns the entire parent chain. No JS, GC or scope
        // mutation occurs during this walk, so borrowed pointers avoid per-hop Rc churn.
        let mut pointer = Rc::as_ptr(env);
        for (index, guard) in self.guards.iter().enumerate() {
            let scope = unsafe { &*pointer }.borrow();
            if !guard.matches(pointer, &scope) {
                return None;
            }
            if index + 1 == self.guards.len() {
                return self.read_holder(interp, pointer, &scope);
            }
            pointer = Rc::as_ptr(scope.parent.as_ref()?);
        }
        None
    }

    /// [`NamePath::read`] of an object value, as its [`Gc::as_ptr`] (no clone); `None` for a
    /// miss or any other value.
    fn read_obj_ptr(&self, interp: &Interp, env: &Env) -> Option<*const RefCell<crate::value::Object>> {
        let mut pointer = Rc::as_ptr(env);
        for (index, guard) in self.guards.iter().enumerate() {
            let scope = unsafe { &*pointer }.borrow();
            if !guard.matches(pointer, &scope) {
                return None;
            }
            if index + 1 == self.guards.len() {
                let binding = match &self.holder {
                    Holder::Binding(p) => unsafe { &**p },
                    Holder::Slot(slot) => scope.vars.template_binding(*slot)?,
                    Holder::Global {
                        object,
                        shape,
                        slot,
                    } => {
                        if pointer != Rc::as_ptr(&interp.global_env)
                            || object.as_ptr() != Gc::as_ptr(&interp.global)
                            || !interp.ordinary_get_ptr(Gc::as_ptr(&interp.global) as usize)
                        {
                            return None;
                        }
                        let g = interp.global.borrow();
                        if !matches!(g.exotic, Exotic::None) || g.props.shape() != *shape {
                            return None;
                        }
                        return g.props.entry_at(*slot)?.obj_ptr();
                    }
                };
                return match &binding.value {
                    Value::Obj(o) if binding.initialized && binding.import_ref.is_none() => {
                        Some(Gc::as_ptr(o))
                    }
                    _ => None,
                };
            }
            pointer = Rc::as_ptr(scope.parent.as_ref()?);
        }
        None
    }

    fn read_holder(
        &self,
        interp: &Interp,
        scope_pointer: *const RefCell<Scope>,
        scope: &Scope,
    ) -> Option<Value> {
        let binding = match &self.holder {
            Holder::Binding(pointer) => unsafe { &**pointer },
            Holder::Slot(slot) => scope.vars.template_binding(*slot)?,
            Holder::Global {
                object,
                shape,
                slot,
            } => {
                if scope_pointer != Rc::as_ptr(&interp.global_env)
                    || object.as_ptr() != Gc::as_ptr(&interp.global)
                {
                    return None;
                }
                return global_value(interp, &interp.global, *shape, *slot);
            }
        };
        (binding.initialized && binding.import_ref.is_none()).then(|| binding.value.clone())
    }

    fn build(interp: &Interp, env: &Env, name: &str) -> Option<(Self, Value)> {
        let mut guards = Vec::new();
        let mut current = env.clone();
        for depth in 0..MAX_DEPTH {
            let scope = current.borrow();
            if scope.with_obj.is_some() {
                return None;
            }
            guards.push(ScopeGuard::new(&current, &scope));
            if let Some(binding) = scope.vars.get(name) {
                // Direct bindings already have a native NameIc path. This is the fallback for
                // deeper resolutions, including depth one when the reader has no activation.
                if depth == 0 || !binding.initialized || binding.import_ref.is_some() {
                    return None;
                }
                let holder = match scope.vars.template_layout() {
                    Some(layout) => Holder::Slot(layout.slot(name)?),
                    None => Holder::Binding(binding as *const Binding),
                };
                return Some((Self { guards, holder }, binding.value.clone()));
            }
            if Rc::ptr_eq(&current, &interp.global_env) {
                if depth == 0 {
                    return None;
                }
                let global = interp.global.borrow();
                let slot = global.props.slot_of(name)?;
                let shape = global.props.shape();
                drop(global);
                let value = global_value(interp, &interp.global, shape, slot)?;
                let holder = Holder::Global {
                    object: Gc::downgrade(&interp.global),
                    shape,
                    slot,
                };
                return Some((Self { guards, holder }, value));
            }
            let parent = scope.parent.clone()?;
            drop(scope);
            current = parent;
        }
        None
    }
}

fn global_value(interp: &Interp, object: &Gc, shape: u32, slot: usize) -> Option<Value> {
    if !interp.ordinary_get_ptr(Gc::as_ptr(object) as usize) {
        return None;
    }
    let object = object.borrow();
    if !matches!(object.exotic, Exotic::None) || object.props.shape() != shape {
        return None;
    }
    let property = object.props.entry_at(slot)?;
    (!property.accessor()).then(|| property.value())
}

/// Write `value` to the global object's entry `slot` when it is a writable data property of an
/// ordinary global; `Err(value)` otherwise.
pub(super) fn global_store(interp: &Interp, slot: usize, value: Value) -> Result<(), Value> {
    if !interp.ordinary_get_ptr(Gc::as_ptr(&interp.global) as usize) {
        return Err(value);
    }
    let Ok(mut g) = interp.global.try_borrow_mut() else {
        return Err(value);
    };
    if !matches!(g.exotic, Exotic::None) {
        return Err(value);
    }
    match g.props.entry_at_mut(slot) {
        Some(p) if !p.accessor() && p.writable() => {
            p.set_value(value);
            Ok(())
        }
        _ => Err(value),
    }
}

impl Chunk {
    /// A cached free-name write through the guarded path (see [`Chunk::name_path_hit`]);
    /// `Err(value)` when the path does not prove a plain writable target.
    pub(super) fn name_path_store(
        &self,
        interp: &Interp,
        env: &Env,
        cache: u32,
        value: Value,
    ) -> Result<(), Value> {
        let paths = self.name_paths[cache as usize].borrow();
        let Some(path) = paths.as_ref() else {
            return Err(value);
        };
        if let Some(b) = path.binding_ptr(env, true) {
            drop(paths);
            // SAFETY: the path's guards prove the binding live and unmoved.
            unsafe { (*b).value = value };
            return Ok(());
        }
        match path.global_slot(interp, env) {
            Some(slot) => {
                drop(paths);
                global_store(interp, slot, value)
            }
            None => Err(value),
        }
    }

    /// [`NamePath::binding_ptr`] of cache `cache`'s path.
    pub(crate) fn name_path_binding(
        &self,
        env: &Env,
        cache: u32,
        write: bool,
    ) -> Option<*mut Binding> {
        self.name_paths.get(cache as usize)?.borrow().as_ref()?.binding_ptr(env, write)
    }

    /// [`NamePath::global_slot`] of cache `cache`'s path.
    pub(crate) fn name_path_global(&self, interp: &Interp, env: &Env, cache: u32) -> Option<usize> {
        self.name_paths.get(cache as usize)?.borrow().as_ref()?.global_slot(interp, env)
    }

    /// Whether cache `cache` holds a path, and whether it ends at the global object.
    pub(crate) fn name_path_kind(&self, cache: u32) -> Option<bool> {
        let paths = self.name_paths.get(cache as usize)?.borrow();
        Some(matches!(paths.as_ref()?.holder, Holder::Global { .. }))
    }

    pub(super) fn name_path_hit(&self, interp: &Interp, env: &Env, cache: u32) -> Option<Value> {
        self.name_paths[cache as usize]
            .borrow()
            .as_ref()?
            .read(interp, env)
    }

    pub(super) fn name_path_obj_ptr(
        &self,
        interp: &Interp,
        env: &Env,
        cache: u32,
    ) -> Option<*const RefCell<crate::value::Object>> {
        self.name_paths[cache as usize]
            .borrow()
            .as_ref()?
            .read_obj_ptr(interp, env)
    }

    pub(super) fn name_path_fill(
        &self,
        interp: &Interp,
        env: &Env,
        name: u32,
        cache: u32,
    ) -> Option<Value> {
        let (path, value) = NamePath::build(interp, env, &self.names[name as usize])?;
        *self.name_paths[cache as usize].borrow_mut() = Some(path);
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::NamePath;
    use crate::interpreter::{
        new_scope, new_var_scope_with_bindings, Binding, BindingLayout, VarMap,
    };
    use crate::value::Value;
    use crate::Engine;
    use std::rc::Rc;

    #[test]
    fn paths_follow_live_values_and_reject_shadowing_and_different_ancestors() {
        let engine = Engine::new();
        let parent = new_scope(Some(engine.interp.global_env.clone()));
        parent
            .borrow_mut()
            .vars
            .insert("x", Binding::data(Value::Num(7.0), true, true));
        let layout = BindingLayout::new([Rc::from("local")]);
        let first =
            new_var_scope_with_bindings(Some(parent.clone()), VarMap::from_layout(layout.clone()));
        let (path, value) = NamePath::build(&engine.interp, &first, "x").unwrap();
        assert!(matches!(value, Value::Num(7.0)));
        let second =
            new_var_scope_with_bindings(Some(parent.clone()), VarMap::from_layout(layout.clone()));
        parent.borrow_mut().vars.get_mut("x").unwrap().value = Value::Num(9.0);
        assert!(matches!(
            path.read(&engine.interp, &second),
            Some(Value::Num(9.0))
        ));
        let other = new_scope(Some(engine.interp.global_env.clone()));
        other
            .borrow_mut()
            .vars
            .insert("x", Binding::data(Value::Num(11.0), true, true));
        let third = new_var_scope_with_bindings(Some(other), VarMap::from_layout(layout));
        assert!(path.read(&engine.interp, &third).is_none());
        second
            .borrow_mut()
            .vars
            .insert("x", Binding::data(Value::Num(13.0), true, true));
        assert!(path.read(&engine.interp, &second).is_none());
        parent.borrow_mut().vars.remove("x");
        assert!(path.read(&engine.interp, &first).is_none());
    }

    #[test]
    fn layout_holders_read_the_current_activation_and_tdz_flag() {
        let engine = Engine::new();
        let layout = BindingLayout::new([Rc::from("x")]);
        let parent = new_var_scope_with_bindings(
            Some(engine.interp.global_env.clone()),
            VarMap::from_layout(layout.clone()),
        );
        let child_layout = BindingLayout::new([Rc::from("local")]);
        let child =
            new_var_scope_with_bindings(Some(parent), VarMap::from_layout(child_layout.clone()));
        let (path, _) = NamePath::build(&engine.interp, &child, "x").unwrap();
        let fresh = new_var_scope_with_bindings(
            Some(engine.interp.global_env.clone()),
            VarMap::from_layout(layout.clone()),
        );
        fresh
            .borrow_mut()
            .vars
            .layout_binding_mut(&layout, 0)
            .unwrap()
            .value = Value::Num(17.0);
        let reader =
            new_var_scope_with_bindings(Some(fresh.clone()), VarMap::from_layout(child_layout));
        assert!(matches!(
            path.read(&engine.interp, &reader),
            Some(Value::Num(17.0))
        ));
        fresh
            .borrow_mut()
            .vars
            .layout_binding_mut(&layout, 0)
            .unwrap()
            .initialized = false;
        assert!(path.read(&engine.interp, &reader).is_none());
        assert!(NamePath::build(&engine.interp, &reader, "x").is_none());
    }

    #[test]
    fn global_paths_reject_accessors_deletion_and_other_realms() {
        let mut engine = Engine::new();
        engine.eval("globalThis.pathValue=3", false).unwrap();
        let env = new_scope(Some(engine.interp.global_env.clone()));
        let (path, _) = NamePath::build(&engine.interp, &env, "pathValue").unwrap();
        engine.eval("globalThis.pathValue=5", false).unwrap();
        assert!(matches!(
            path.read(&engine.interp, &env),
            Some(Value::Num(5.0))
        ));
        let other = Engine::new();
        assert!(path.read(&other.interp, &env).is_none());
        engine.eval("globalThis.getterCalls=0; Object.defineProperty(globalThis,'pathValue',{configurable:true,get(){getterCalls++;return 7}})", false).unwrap();
        assert!(path.read(&engine.interp, &env).is_none());
        assert!(NamePath::build(&engine.interp, &env, "pathValue").is_none());
        assert!(
            matches!(engine.eval("getterCalls", false).unwrap(), crate::Completion::Value(v) if v == "0")
        );
        engine.eval("delete globalThis.pathValue", false).unwrap();
        assert!(path.read(&engine.interp, &env).is_none());
    }
}
