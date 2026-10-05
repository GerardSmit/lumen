//! Typed host services scoped to the active JavaScript realm.
//!
//! `OpState` belongs to an interpreter and is shared while Lumen swaps realm intrinsics. Browser
//! adapters can use this store for state that belongs to one global (for example a document's
//! animation timeline) without creating a second interpreter or looking up mutable JS globals.

use lumen::embed::{Ctx, Value, WeakValue};
use std::{cell::RefCell, collections::HashMap, rc::Rc};

struct Entry<T> {
    // Do not make the realm global a hidden host root. The pointer key remains unique while the
    // global is alive; stale entries are pruned before lookup/replacement, so a reused address
    // cannot select a service belonging to a collected realm.
    global: WeakValue,
    service: Rc<T>,
}

/// One typed service slot per active realm in the current interpreter.
pub(crate) struct RealmServices<T> {
    entries: HashMap<usize, Entry<T>>,
}

impl<T> Default for RealmServices<T> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }
}

impl<T: 'static> RealmServices<T> {
    /// Replace the current realm's service and return its shared handle.
    pub(crate) fn replace_current(ctx: &mut Ctx, service: T) -> Rc<T> {
        Self::replace_shared_current(ctx, Rc::new(service))
    }

    /// Register an already shared service under T, preserving its identity.
    pub(crate) fn replace_shared_current(ctx: &mut Ctx, service: Rc<T>) -> Rc<T> {
        let global = ctx.global_object();
        let key = global
            .object_identity()
            .expect("the active JavaScript realm has an object global");
        let weak_global = ctx
            .weak_value(&global)
            .expect("the active JavaScript realm global is an object");
        if !ctx.op_state().has::<Self>() {
            ctx.op_state().put(Self::default());
        }
        let entries = &mut ctx
            .op_state()
            .get_mut::<Self>()
            .expect("realm service store was just installed")
            .entries;
        // Stale weak keys are swept on writes, not on every native lookup.
        entries.retain(|_, entry| entry.global.upgrade().is_some());
        entries.insert(
            key,
            Entry {
                global: weak_global,
                service: service.clone(),
            },
        );
        service
    }

    /// Clone the current realm's service handle, if it has been installed.
    pub(crate) fn current(ctx: &mut Ctx) -> Option<Rc<T>> {
        let global = ctx.global_object();
        let key = global.object_identity()?;
        let services = ctx.op_state().get_mut::<Self>()?;
        let is_live = services
            .entries
            .get(&key)
            .and_then(|entry| entry.global.upgrade())
            .is_some_and(|registered_global| registered_global.object_identity() == Some(key));
        if is_live {
            return services
                .entries
                .get(&key)
                .map(|entry| entry.service.clone());
        }
        services.entries.remove(&key);
        None
    }

    /// Remove this service's entry for a retired realm. A host can call this while another realm
    /// is active because the identity comes from the handle, not the current global.
    pub(crate) fn remove_for_global(ctx: &mut Ctx, global: &Value) -> Option<Rc<T>> {
        let key = global.object_identity()?;
        ctx.op_state()
            .get_mut::<Self>()?
            .entries
            .remove(&key)
            .map(|entry| entry.service)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;

    #[test]
    fn typed_service_slots_are_isolated_and_persist_per_realm() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let parent =
            RealmServices::<RefCell<Vec<u32>>>::replace_current(ctx, RefCell::new(vec![1]));
        parent.borrow_mut().push(2);

        let child = ctx.create_host_realm();
        let child_service = ctx
            .with_host_realm(&child, |ctx| {
                assert!(RealmServices::<RefCell<Vec<u32>>>::current(ctx).is_none());
                let service = RealmServices::replace_current(ctx, RefCell::new(vec![9]));
                assert_eq!(*service.borrow(), [9]);
                service
            })
            .expect("enter the child realm");

        let parent_again =
            RealmServices::<RefCell<Vec<u32>>>::current(ctx).expect("parent service");
        assert!(Rc::ptr_eq(&parent, &parent_again));
        assert_eq!(*parent_again.borrow(), [1, 2]);

        let child_again = ctx
            .with_host_realm(&child, RealmServices::<RefCell<Vec<u32>>>::current)
            .expect("re-enter the child realm")
            .expect("child service");
        assert!(Rc::ptr_eq(&child_service, &child_again));
        assert_eq!(*child_again.borrow(), [9]);
        assert!(Rc::ptr_eq(
            &parent_again,
            &RealmServices::<RefCell<Vec<u32>>>::current(ctx)
                .expect("parent service remains installed")
        ));
    }

    #[test]
    fn stale_or_mismatched_weak_global_keys_never_select_another_realm_service() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let first = ctx.create_host_realm();
        let second = ctx.create_host_realm();
        let second_global = second.global();
        let first_service = ctx
            .with_host_realm(&first, |ctx| RealmServices::<u32>::replace_current(ctx, 17))
            .expect("register first realm service");
        assert_eq!(*first_service, 17);

        let first_global = first.global();
        let first_key = first_global.object_identity().expect("object global");
        let mismatched_weak = ctx
            .weak_value(&second_global)
            .expect("second realm global is weak-referenceable");
        ctx.op_state()
            .get_mut::<RealmServices<u32>>()
            .expect("service store")
            .entries
            .get_mut(&first_key)
            .expect("first realm entry")
            .global = mismatched_weak;

        let observed = ctx
            .with_host_realm(&first, RealmServices::<u32>::current)
            .expect("inspect first realm service");
        assert!(
            observed.is_none(),
            "a reused identity key must be validated"
        );
    }

    #[test]
    fn retired_host_realm_weak_service_key_is_pruned_on_the_next_write() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        RealmServices::<u32>::replace_current(ctx, 1);
        let child = ctx.create_host_realm();
        let child_service = ctx
            .with_host_realm(&child, |ctx| RealmServices::<u32>::replace_current(ctx, 2))
            .expect("register child realm service");
        drop(child_service);

        ctx.dispose_host_realm(&child).expect("retire child realm");
        drop(child);
        ctx.collect_garbage_for_host();
        RealmServices::<u32>::replace_current(ctx, 3);

        let store = ctx
            .op_state()
            .get::<RealmServices<u32>>()
            .expect("service store remains installed");
        assert_eq!(store.entries.len(), 1);
        assert_eq!(
            *RealmServices::<u32>::current(ctx).expect("current root service"),
            3
        );
    }
}
