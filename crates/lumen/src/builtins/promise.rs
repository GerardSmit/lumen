//! Split out of builtins/mod.rs (behavior-preserving move).

use super::*;

pub(super) fn install_promise(it: &mut Interp) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("Promise", proto.clone());
    if let Some(key) = to_string_tag_key(it) {
        proto.borrow_mut().props.insert(
            key,
            Property::data(Value::str("Promise"), false, false, true),
        );
    }
    it.def_method(&proto, "then", 2, |i, this, a| {
        if !crate::eval::promise_fast::is_promise(&this) {
            return Err(i.make_error(
                "TypeError",
                "Promise.prototype.then called on a non-Promise",
            ));
        }
        // SpeciesConstructor(this, %Promise%) on an unmodified promise is %Promise% with no
        // observable step: the result is just a fresh promise.
        if promise_then_is_silent(i, &this) {
            let result = i.new_promise();
            i.promise_then_into(&this, arg(a, 0), arg(a, 1), result.clone());
            return Ok(result);
        }
        // The result promise is built by SpeciesConstructor(this, %Promise%) via NewPromiseCapability.
        let default_ctor = ab(i.get_member(&Value::Obj(i.global.clone()), "Promise"))?;
        let c = species_constructor(i, &this, &default_ctor)?;
        // NewPromiseCapability(%Promise%) is unobservable (its executor and resolving functions
        // never escape, and %Promise%.prototype is a non-writable, non-configurable data
        // property): the result is just a fresh promise.
        if is_intrinsic_promise_ctor(i, &c) {
            let result = i.new_promise();
            i.promise_then_into(&this, arg(a, 0), arg(a, 1), result.clone());
            return Ok(result);
        }
        let (result, res_f, rej_f) = new_promise_capability_full(i, &c)?;
        if crate::eval::promise_fast::is_promise(&result) {
            i.promise_then_into(&this, arg(a, 0), arg(a, 1), result.clone());
        } else {
            // The constructor produced a promise we don't manage: settle it through the
            // capability's own resolve/reject functions via a native shadow promise.
            let shadow = i.new_promise();
            i.promise_then_into(&this, arg(a, 0), arg(a, 1), shadow.clone());
            let dummy = i.new_promise();
            i.promise_then_into(&shadow, res_f, rej_f, dummy);
        }
        Ok(result)
    });
    it.def_method(&proto, "catch", 1, |i, this, a| {
        // Catch is `this.then(undefined, onRejected)` through the actual then method.
        let then = ab(i.get_member(&this, "then"))?;
        ab(i.call(then, this.clone(), &[Value::Undefined, arg(a, 0)]))
    });
    it.def_method(&proto, "finally", 1, |i, this, a| {
        if !matches!(this, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "Promise.prototype.finally on non-object"));
        }
        let default_ctor = ab(i.get_member(&Value::Obj(i.global.clone()), "Promise"))?;
        let c = species_constructor(i, &this, &default_ctor)?;
        let then = ab(i.get_member(&this, "then"))?;
        if !then.is_callable() {
            return Err(i.make_error("TypeError", "then is not callable"));
        }
        let on_finally = arg(a, 0);
        // A non-callable onFinally is passed to `then` on both paths unchanged.
        if !on_finally.is_callable() {
            return ab(i.call(then, this.clone(), &[on_finally.clone(), on_finally]));
        }
        // Otherwise wrap it so the original value/reason passes through after onFinally runs.
        let then_finally = make_bound(i, pf_then_finally, vec![on_finally.clone(), c.clone()]);
        let catch_finally = make_bound(i, pf_catch_finally, vec![on_finally, c]);
        ab(i.call(then, this.clone(), &[then_finally, catch_finally]))
    });

    let ctor = it.make_native("Promise", 1, |i, _t, a| {
        if !i.constructing {
            return Err(i.make_error("TypeError", "Promise constructor requires 'new'"));
        }
        let executor = arg(a, 0);
        if !executor.is_callable() {
            return Err(i.make_error("TypeError", "Promise resolver is not a function"));
        }
        let promise = i.new_promise();
        // OrdinaryCreateFromConstructor: newTarget's prototype (its getter may throw). For
        // %Promise% itself that is its non-writable, non-configurable `prototype`: the realm's
        // %Promise.prototype%, which `new_promise` already used.
        let nt_is_ctor = is_intrinsic_promise_ctor(i, &i.new_target);
        if let (Value::Obj(p), nt @ Value::Obj(_), false) = (&promise, &i.new_target.clone(), nt_is_ctor) {
            match ab(i.get_member(nt, "prototype"))? {
                Value::Obj(proto) => p.borrow_mut().proto = Some(proto),
                _ => {
                    if let Some(proto) = ctor_realm_proto(i, &nt.clone(), "Promise")? {
                        p.borrow_mut().proto = Some(proto);
                    }
                }
            }
        }
        let (res, rej) = i.make_resolver_pair(&promise);
        if let Err(Abrupt::Throw(e)) = i.call(executor, Value::Undefined, &[res, rej.clone()]) {
            // The catch goes through the reject RESOLVER: a no-op once resolve/reject ran.
            let _ = i.call(rej, Value::Undefined, &[e]);
        }
        Ok(promise)
    });
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(proto.clone()), false, false, false),
    );
    it.extra_protos.insert("%PromiseCtor%", ctor.clone());
    if let Some(Value::Obj(then)) = proto.borrow().props.get("then").map(|p| p.value()) {
        it.extra_protos.insert("%Promise.prototype.then%", then);
    }
    proto
        .borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    it.def_method(&ctor, "withResolvers", 0, |i, t, _a| {
        if !matches!(t, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "Promise.withResolvers called on a non-object"));
        }
        let promise = new_promise_capability(i, &t)?;
        let (resolve, reject) = i.make_resolver_pair(&promise);
        let obj = i.new_object();
        set_data(&obj, "promise", promise);
        set_data(&obj, "resolve", resolve);
        set_data(&obj, "reject", reject);
        Ok(Value::Obj(obj))
    });
    it.def_method(&ctor, "resolve", 1, |i, t, a| {
        // `this` must be a constructor object; PromiseResolve(this, v).
        if !matches!(t, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "Promise.resolve called on a non-object"));
        }
        promise_resolve(i, &t, arg(a, 0))
    });
    it.def_method(&ctor, "reject", 1, |i, t, a| {
        if !matches!(t, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "Promise.reject called on a non-object"));
        }
        // NewPromiseCapability(%Promise%) is unobservable: reject a fresh promise directly.
        if is_intrinsic_promise_ctor(i, &t) {
            let p = i.new_promise();
            i.reject_promise(&p, arg(a, 0));
            return Ok(p);
        }
        // NewPromiseCapability(C): rejection goes through the capability's own reject.
        let (promise, _resolve_fn, reject_fn) = new_promise_capability_full(i, &t)?;
        ab(i.call(reject_fn, Value::Undefined, &[arg(a, 0)]))?;
        Ok(promise)
    });
    it.def_method(&ctor, "all", 1, |i, t, a| {
        if combinator_gate(i, &t)? {
            return combinator_fast(i, &t, arg(a, 0), crate::eval::promise_fast::REACT_ALL);
        }
        // NewPromiseCapability(C): resolve/reject route through C's own capability so a subclass or
        // foreign Promise constructor works, not just the native machinery.
        let (result, resolve_fn, reject_fn) = match new_promise_capability_full(i, &t) {
            Ok(c) => c,
            Err(e) => return Err(e),
        };
        // GetPromiseResolve and the iteration may throw — those reject via the capability.
        let promise_resolve = match get_promise_resolve(i, &t) {
            Ok(r) => r,
            Err(e) => {
                let _ = i.call(reject_fn, Value::Undefined, &[e]);
                return Ok(result);
            }
        };
        // Iterate lazily so a throwing C.resolve / `.then` closes the iterator (IteratorClose).
        let (iter, next) = match i.get_iterator(&arg(a, 0)) {
            Ok(it) => it,
            Err(e) => {
                let reason = crate::interpreter::abrupt_value(e);
                let _ = i.call(reject_fn, Value::Undefined, &[reason]);
                return Ok(result);
            }
        };
        let results = i.make_array(vec![]);
        let state = i.new_object();
        set_internal(&state, "__results", results.clone());
        // remainingElementsCount starts at 1; each element increments, the loop end decrements once.
        set_internal(&state, "__remaining", Value::Num(1.0));
        let mut idx = 0usize;
        loop {
            let item = match i.iterator_step(&iter, &next) {
                Ok(Some(v)) => v,
                Ok(None) => break,
                Err(e) => {
                    // A throw from the iterator marks it done — no IteratorClose.
                    let _ = i.call(
                        reject_fn.clone(),
                        Value::Undefined,
                        &[crate::interpreter::abrupt_value(e)],
                    );
                    return Ok(result);
                }
            };
            let rem_v = ab(i.get_member(&Value::Obj(state.clone()), "__remaining"))?;
            let rem = ab(i.to_number(&rem_v))?;
            set_internal(&state, "__remaining", Value::Num(rem + 1.0));
            let p = match i.call(promise_resolve.clone(), t.clone(), &[item]) {
                Ok(p) => p,
                Err(e) => {
                    i.iterator_close(&iter);
                    let _ = i.call(
                        reject_fn.clone(),
                        Value::Undefined,
                        &[crate::interpreter::abrupt_value(e)],
                    );
                    return Ok(result);
                }
            };
            let already = i.new_object();
            set_internal(&already, "__called", Value::Bool(false));
            let on_f = make_bound(
                i,
                promise_all_element,
                vec![
                    Value::Obj(state.clone()),
                    Value::Num(idx as f64),
                    Value::Obj(already),
                    resolve_fn.clone(),
                ],
            );
            // Subscribe via the resolved value's `.then`; a throwing getter/call closes the iterator.
            let then = match i.get_member(&p, "then") {
                Ok(t) => t,
                Err(e) => {
                    i.iterator_close(&iter);
                    let _ = i.call(
                        reject_fn.clone(),
                        Value::Undefined,
                        &[crate::interpreter::abrupt_value(e)],
                    );
                    return Ok(result);
                }
            };
            if let Err(e) = i.call(then, p, &[on_f, reject_fn.clone()]) {
                i.iterator_close(&iter);
                let _ = i.call(
                    reject_fn.clone(),
                    Value::Undefined,
                    &[crate::interpreter::abrupt_value(e)],
                );
                return Ok(result);
            }
            idx += 1;
        }
        // The values array's length is the element count (CreateDataProperty set each index).
        ab(i.set_member(&results, "length", Value::Num(idx as f64)))?;
        let rem_v = ab(i.get_member(&Value::Obj(state.clone()), "__remaining"))?;
        let rem = ab(i.to_number(&rem_v))?;
        set_internal(&state, "__remaining", Value::Num(rem - 1.0));
        if rem - 1.0 == 0.0 {
            capability_resolve_or_reject(i, resolve_fn, reject_fn, results);
        }
        Ok(result)
    });
    it.def_method(&ctor, "race", 1, |i, t, a| {
        if combinator_gate(i, &t)? {
            return combinator_fast(i, &t, arg(a, 0), crate::eval::promise_fast::REACT_RACE);
        }
        // Race settles the result promise with the first element to settle, through C's capability.
        let (result, resolve_fn, reject_fn) = match new_promise_capability_full(i, &t) {
            Ok(c) => c,
            Err(e) => return Err(e),
        };
        let promise_resolve = match get_promise_resolve(i, &t) {
            Ok(r) => r,
            Err(e) => {
                let _ = i.call(reject_fn, Value::Undefined, &[e]);
                return Ok(result);
            }
        };
        let (iter, next) = match i.get_iterator(&arg(a, 0)) {
            Ok(it) => it,
            Err(e) => {
                let reason = crate::interpreter::abrupt_value(e);
                let _ = i.call(reject_fn, Value::Undefined, &[reason]);
                return Ok(result);
            }
        };
        loop {
            let item = match i.iterator_step(&iter, &next) {
                Ok(Some(v)) => v,
                Ok(None) => break,
                Err(e) => {
                    let _ = i.call(
                        reject_fn.clone(),
                        Value::Undefined,
                        &[crate::interpreter::abrupt_value(e)],
                    );
                    return Ok(result);
                }
            };
            let p = match i.call(promise_resolve.clone(), t.clone(), &[item]) {
                Ok(p) => p,
                Err(e) => {
                    i.iterator_close(&iter);
                    let _ = i.call(
                        reject_fn.clone(),
                        Value::Undefined,
                        &[crate::interpreter::abrupt_value(e)],
                    );
                    return Ok(result);
                }
            };
            let then = match i.get_member(&p, "then") {
                Ok(t) => t,
                Err(e) => {
                    i.iterator_close(&iter);
                    let _ = i.call(
                        reject_fn.clone(),
                        Value::Undefined,
                        &[crate::interpreter::abrupt_value(e)],
                    );
                    return Ok(result);
                }
            };
            if let Err(e) = i.call(then, p, &[resolve_fn.clone(), reject_fn.clone()]) {
                i.iterator_close(&iter);
                let _ = i.call(
                    reject_fn.clone(),
                    Value::Undefined,
                    &[crate::interpreter::abrupt_value(e)],
                );
                return Ok(result);
            }
        }
        Ok(result)
    });
    it.def_method(&ctor, "allSettled", 1, |i, t, a| {
        if combinator_gate(i, &t)? {
            return combinator_fast(i, &t, arg(a, 0), crate::eval::promise_fast::REACT_ALL_SETTLED);
        }
        let (result, resolve_fn, reject_fn) = match new_promise_capability_full(i, &t) {
            Ok(c) => c,
            Err(e) => return Err(e),
        };
        let promise_resolve = match get_promise_resolve(i, &t) {
            Ok(r) => r,
            Err(e) => {
                let _ = i.call(reject_fn, Value::Undefined, &[e]);
                return Ok(result);
            }
        };
        let (iter, next) = match i.get_iterator(&arg(a, 0)) {
            Ok(it) => it,
            Err(e) => {
                let reason = crate::interpreter::abrupt_value(e);
                let _ = i.call(reject_fn, Value::Undefined, &[reason]);
                return Ok(result);
            }
        };
        let results = i.make_array(vec![]);
        let state = i.new_object();
        set_internal(&state, "__results", results.clone());
        set_internal(&state, "__remaining", Value::Num(1.0));
        let mut idx = 0usize;
        loop {
            let item = match i.iterator_step(&iter, &next) {
                Ok(Some(v)) => v,
                Ok(None) => break,
                Err(e) => {
                    let _ = i.call(
                        reject_fn.clone(),
                        Value::Undefined,
                        &[crate::interpreter::abrupt_value(e)],
                    );
                    return Ok(result);
                }
            };
            let rem_v = ab(i.get_member(&Value::Obj(state.clone()), "__remaining"))?;
            let rem = ab(i.to_number(&rem_v))?;
            set_internal(&state, "__remaining", Value::Num(rem + 1.0));
            let p = match i.call(promise_resolve.clone(), t.clone(), &[item]) {
                Ok(p) => p,
                Err(e) => {
                    i.iterator_close(&iter);
                    let _ = i.call(
                        reject_fn.clone(),
                        Value::Undefined,
                        &[crate::interpreter::abrupt_value(e)],
                    );
                    return Ok(result);
                }
            };
            // The fulfill and reject element functions for one index share one [[AlreadyCalled]].
            let already = i.new_object();
            set_internal(&already, "__called", Value::Bool(false));
            let on_f = make_bound(
                i,
                promise_settled_fulfill,
                vec![
                    Value::Obj(state.clone()),
                    Value::Num(idx as f64),
                    Value::Obj(already.clone()),
                    resolve_fn.clone(),
                ],
            );
            let on_r = make_bound(
                i,
                promise_settled_reject,
                vec![
                    Value::Obj(state.clone()),
                    Value::Num(idx as f64),
                    Value::Obj(already),
                    resolve_fn.clone(),
                ],
            );
            let then = match i.get_member(&p, "then") {
                Ok(t) => t,
                Err(e) => {
                    i.iterator_close(&iter);
                    let _ = i.call(
                        reject_fn.clone(),
                        Value::Undefined,
                        &[crate::interpreter::abrupt_value(e)],
                    );
                    return Ok(result);
                }
            };
            if let Err(e) = i.call(then, p, &[on_f, on_r]) {
                i.iterator_close(&iter);
                let _ = i.call(
                    reject_fn.clone(),
                    Value::Undefined,
                    &[crate::interpreter::abrupt_value(e)],
                );
                return Ok(result);
            }
            idx += 1;
        }
        ab(i.set_member(&results, "length", Value::Num(idx as f64)))?;
        let rem_v = ab(i.get_member(&Value::Obj(state.clone()), "__remaining"))?;
        let rem = ab(i.to_number(&rem_v))?;
        set_internal(&state, "__remaining", Value::Num(rem - 1.0));
        if rem - 1.0 == 0.0 {
            capability_resolve_or_reject(i, resolve_fn, reject_fn, results);
        }
        Ok(result)
    });
    it.def_method(&ctor, "any", 1, |i, t, a| {
        if combinator_gate(i, &t)? {
            return combinator_fast(i, &t, arg(a, 0), crate::eval::promise_fast::REACT_ANY);
        }
        let (result, resolve_fn, reject_fn) = match new_promise_capability_full(i, &t) {
            Ok(c) => c,
            Err(e) => return Err(e),
        };
        let promise_resolve = match get_promise_resolve(i, &t) {
            Ok(r) => r,
            Err(e) => {
                let _ = i.call(reject_fn, Value::Undefined, &[e]);
                return Ok(result);
            }
        };
        let (iter, next) = match i.get_iterator(&arg(a, 0)) {
            Ok(it) => it,
            Err(e) => {
                let reason = crate::interpreter::abrupt_value(e);
                let _ = i.call(reject_fn, Value::Undefined, &[reason]);
                return Ok(result);
            }
        };
        let errors = i.make_array(vec![]);
        let state = i.new_object();
        set_internal(&state, "__errors", errors.clone());
        set_internal(&state, "__remaining", Value::Num(1.0));
        let mut idx = 0usize;
        loop {
            let item = match i.iterator_step(&iter, &next) {
                Ok(Some(v)) => v,
                Ok(None) => break,
                Err(e) => {
                    let _ = i.call(
                        reject_fn.clone(),
                        Value::Undefined,
                        &[crate::interpreter::abrupt_value(e)],
                    );
                    return Ok(result);
                }
            };
            let rem_v = ab(i.get_member(&Value::Obj(state.clone()), "__remaining"))?;
            let rem = ab(i.to_number(&rem_v))?;
            set_internal(&state, "__remaining", Value::Num(rem + 1.0));
            let p = match i.call(promise_resolve.clone(), t.clone(), &[item]) {
                Ok(p) => p,
                Err(e) => {
                    i.iterator_close(&iter);
                    let _ = i.call(
                        reject_fn.clone(),
                        Value::Undefined,
                        &[crate::interpreter::abrupt_value(e)],
                    );
                    return Ok(result);
                }
            };
            let already = i.new_object();
            set_internal(&already, "__called", Value::Bool(false));
            let on_r = make_bound(
                i,
                promise_any_reject,
                vec![
                    Value::Obj(state.clone()),
                    Value::Num(idx as f64),
                    Value::Obj(already),
                    reject_fn.clone(),
                ],
            );
            let then = match i.get_member(&p, "then") {
                Ok(t) => t,
                Err(e) => {
                    i.iterator_close(&iter);
                    let _ = i.call(
                        reject_fn.clone(),
                        Value::Undefined,
                        &[crate::interpreter::abrupt_value(e)],
                    );
                    return Ok(result);
                }
            };
            // First fulfillment resolves the result (its [[AlreadyResolved]] lives in resolve_fn).
            if let Err(e) = i.call(then, p, &[resolve_fn.clone(), on_r]) {
                i.iterator_close(&iter);
                let _ = i.call(
                    reject_fn.clone(),
                    Value::Undefined,
                    &[crate::interpreter::abrupt_value(e)],
                );
                return Ok(result);
            }
            idx += 1;
        }
        ab(i.set_member(&errors, "length", Value::Num(idx as f64)))?;
        let rem_v = ab(i.get_member(&Value::Obj(state.clone()), "__remaining"))?;
        let rem = ab(i.to_number(&rem_v))?;
        set_internal(&state, "__remaining", Value::Num(rem - 1.0));
        if rem - 1.0 == 0.0 {
            let agg = make_aggregate_error(i, errors)?;
            let _ = i.call(reject_fn, Value::Undefined, &[agg]);
        }
        Ok(result)
    });
    it.def_method(&ctor, "allKeyed", 1, |i, t, a| {
        promise_keyed_combinator(i, t, arg(a, 0), false)
    });
    it.def_method(&ctor, "allSettledKeyed", 1, |i, t, a| {
        promise_keyed_combinator(i, t, arg(a, 0), true)
    });
    it.def_method(&ctor, "try", 1, |i, t, a| {
        // Promise.try(fn, ...args): call fn synchronously; a throw rejects a fresh
        // NewPromiseCapability(this), a return goes through PromiseResolve(this, v) — so a
        // returned promise already of this constructor comes back unwrapped.
        if !matches!(t, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "Promise.try called on a non-object"));
        }
        let func = arg(a, 0);
        let rest: Vec<Value> = a.iter().skip(1).cloned().collect();
        match ab(i.call(func, Value::Undefined, &rest)) {
            Ok(v) => promise_resolve(i, &t, v),
            Err(e) => {
                let (promise, _resolve_fn, reject_fn) = new_promise_capability_full(i, &t)?;
                ab(i.call(reject_fn, Value::Undefined, &[e]))?;
                Ok(promise)
            }
        }
    });
    if let Some(Value::Obj(resolve)) = ctor.borrow().props.get("resolve").map(|p| p.value()) {
        it.extra_protos.insert("%Promise.resolve%", resolve);
    }
    install_species(it, &ctor);
    set_builtin(&it.global, "Promise", Value::Obj(ctor));
}

/// PromiseResolve(C, x): a promise whose own `constructor` is C is returned unchanged; anything
/// else resolves a fresh NewPromiseCapability(C) through the capability's own resolve.
fn promise_resolve(i: &mut Interp, ctor: &Value, v: Value) -> Result<Value, Value> {
    if let Value::Obj(o) = &v {
        if crate::eval::promise_fast::is_promise_obj(o) {
            // `constructor` inherited as a data property from an untouched %Promise.prototype%:
            // the read runs no user code.
            if let Some(intr) = i.promise_intr() {
                if is_plain_native_promise(o, &intr) {
                    if let Some(c) = proto_data(&intr.proto, "constructor") {
                        if same_value(&c, ctor) {
                            return Ok(v);
                        }
                    }
                }
            }
            let c = ab(i.get_member(&v, "constructor"))?;
            if same_value(&c, ctor) {
                return Ok(v);
            }
        }
    }
    // NewPromiseCapability(%Promise%) is unobservable (see `then`): resolve a fresh promise
    // directly instead of through an executor and a resolving-function pair.
    if is_intrinsic_promise_ctor(i, ctor) {
        let p = i.new_promise();
        i.resolve_promise(&p, v);
        return Ok(p);
    }
    let (promise, resolve_fn, _reject_fn) = new_promise_capability_full(i, ctor)?;
    ab(i.call(resolve_fn, Value::Undefined, &[v]))?;
    Ok(promise)
}

/// A data property's value on `o` (`None` when absent or an accessor).
fn proto_data(o: &Gc, key: &str) -> Option<Value> {
    match o.borrow().props.get(key) {
        Some(p) if !p.accessor() => Some(p.value()),
        _ => None,
    }
}

/// Whether native promise `o` inherits straight from this realm's untouched-at-the-instance
/// %Promise.prototype%: its prototype is that object and it has no own `then`/`constructor`.
fn is_plain_native_promise(o: &Gc, intr: &crate::eval::promise_fast::PromiseIntr) -> bool {
    let b = o.borrow();
    matches!(b.call, Callable::Promise(_))
        && matches!(&b.proto, Some(p) if Gc::ptr_eq(p, &intr.proto))
        && !b.props.contains("then")
        && !b.props.contains("constructor")
}

/// Whether %Promise%[@@species] is still the original getter (so SpeciesConstructor of a
/// promise whose `constructor` is %Promise% is %Promise% with no user code).
fn species_is_pristine(i: &Interp, intr: &crate::eval::promise_fast::PromiseIntr) -> bool {
    let mut c = intr.slots.get();
    let cb = intr.ctor.borrow();
    let shape = cb.props.shape();
    if c.ctor_shape != Some(shape) {
        let Some(key) = well_known_key(i, "species") else { return false };
        let Some(slot) = cb.props.slot_of(&key) else { return false };
        c.species_slot = slot as u32;
        c.ctor_shape = Some(shape);
        intr.slots.set(c);
    }
    let getter = match cb.props.entry_at(c.species_slot as usize) {
        Some(p) if p.accessor() => p.getter().cloned(),
        _ => None,
    };
    drop(cb);
    matches!(getter, Some(Value::Obj(g))
        if matches!(g.borrow().call, Callable::Native(f) if f as usize == nf_species_getter as NativeFn as usize))
}

/// Whether %Promise.prototype% still has the original `then` and `constructor` as data
/// properties and %Promise% the original species getter: `Invoke(p, "then", ...)` on a plain
/// native promise `p` then runs exactly PerformPromiseThen with an unobservable derived promise.
fn proto_then_is_pristine(i: &Interp, intr: &crate::eval::promise_fast::PromiseIntr) -> bool {
    let Some((ctor, then)) = proto_slots(intr) else { return false };
    let pb = intr.proto.borrow();
    let holds = |slot: u32, want: &Gc| {
        matches!(pb.props.entry_at(slot as usize),
            Some(p) if !p.accessor() && matches!(p.value(), Value::Obj(f) if Gc::ptr_eq(&f, want)))
    };
    let ok = holds(ctor, &intr.ctor) && holds(then, &intr.then);
    drop(pb);
    ok && species_is_pristine(i, intr)
}

/// The slots of `constructor` and `then` on %Promise.prototype% (see [`PristineSlots`]).
///
/// [`PristineSlots`]: crate::eval::promise_fast::PristineSlots
fn proto_slots(intr: &crate::eval::promise_fast::PromiseIntr) -> Option<(u32, u32)> {
    let mut c = intr.slots.get();
    let pb = intr.proto.borrow();
    let shape = pb.props.shape();
    if c.proto_shape != Some(shape) {
        c.ctor_slot = pb.props.slot_of("constructor")? as u32;
        c.then_slot = pb.props.slot_of("then")? as u32;
        c.proto_shape = Some(shape);
        intr.slots.set(c);
    }
    Some((c.ctor_slot, c.then_slot))
}

/// Whether SpeciesConstructor(`this`, %Promise%) is %Promise% without any observable step:
/// `this` inherits `constructor` straight from an untouched %Promise.prototype% (a data
/// property holding %Promise%), whose `@@species` is still the original getter.
pub(crate) fn promise_then_is_silent(i: &Interp, this: &Value) -> bool {
    let Value::Obj(o) = this else { return false };
    let Some(intr) = i.promise_intr() else { return false };
    {
        let b = o.borrow();
        if !matches!(&b.proto, Some(p) if Gc::ptr_eq(p, &intr.proto))
            || b.props.contains("constructor")
        {
            return false;
        }
    }
    let Some((ctor, _)) = proto_slots(&intr) else { return false };
    let ctor_ok = matches!(intr.proto.borrow().props.entry_at(ctor as usize),
        Some(p) if !p.accessor() && matches!(p.value(), Value::Obj(c) if Gc::ptr_eq(&c, &intr.ctor)));
    ctor_ok && species_is_pristine(i, &intr)
}

/// Whether `c` is this realm's %Promise%.
fn is_intrinsic_promise_ctor(i: &Interp, c: &Value) -> bool {
    match (c, i.promise_intr()) {
        (Value::Obj(c), Some(intr)) => Gc::ptr_eq(c, &intr.ctor),
        _ => false,
    }
}

/// `new AggregateError(errors)` (for `Promise.any`).
pub(crate) fn make_aggregate_error_value(i: &mut Interp, errors: Value) -> Result<Value, Value> {
    make_aggregate_error(i, errors)
}

/// `Promise.all` / `allSettled` / `any` / `race` when C is %Promise% and `C.resolve` is the
/// original: NewPromiseCapability(%Promise%) is unobservable, so the result is a native promise
/// carrying the combinator state (see `promise_fast::Combinator`). Each element that is a plain
/// native promise (or a primitive) under an untouched %Promise.prototype% subscribes with a
/// compact combinator reaction - no element function objects, no derived promise - since
/// PromiseResolve and `Invoke(p, "then")` then run no user code and the element functions never
/// escape. Any other element (a thenable, a patched promise) takes the full observable path
/// with real element functions over the same state.
fn combinator_fast(i: &mut Interp, t: &Value, iterable: Value, mode: u8) -> Result<Value, Value> {
    use crate::eval::promise_fast::{Combinator, Reaction, REACT_ANY, REACT_RACE};
    let result = i.new_promise();
    if let Value::Obj(ro) = &result {
        if let Callable::Promise(s) = &mut ro.borrow_mut().call {
            s.comb = Some(Box::new(Combinator { remaining: 1, ..Default::default() }));
        }
    }
    let (iter, next) = match i.get_iterator(&iterable) {
        Ok(it) => it,
        Err(e) => {
            i.combinator_resolve(&result, false, crate::interpreter::abrupt_value(e));
            return Ok(result);
        }
    };
    let mut idx: u32 = 0;
    loop {
        let item = match i.iterator_step(&iter, &next) {
            Ok(Some(v)) => v,
            Ok(None) => break,
            Err(e) => {
                // A throw from the iterator marks it done — no IteratorClose.
                i.combinator_resolve(&result, false, crate::interpreter::abrupt_value(e));
                return Ok(result);
            }
        };
        if mode != REACT_RACE {
            // Append an empty element and bump remainingElementsCount.
            if let Value::Obj(ro) = &result {
                if let Callable::Promise(s) = &mut ro.borrow_mut().call {
                    if let Some(c) = s.comb.as_mut() {
                        c.values.push(Value::Undefined);
                        c.remaining += 1;
                    }
                }
            }
        }
        let silent = match i.promise_intr() {
            Some(intr) => {
                let plain = match &item {
                    Value::Obj(o) => is_plain_native_promise(o, &intr),
                    _ => true,
                };
                plain && proto_then_is_pristine(i, &intr)
            }
            None => false,
        };
        if silent {
            let reaction = Reaction {
                on_f: Value::Undefined,
                on_r: Value::Undefined,
                result: result.clone(),
                context: i.async_context.clone(),
                kind: mode,
                idx,
            };
            match &item {
                Value::Obj(o) => i.perform_then(o, reaction),
                // PromiseResolve made a fresh fulfilled promise: its reaction queues at once.
                _ => {
                    let job = Interp::reaction_job(reaction, true, item);
                    i.microtasks.push_back(job);
                }
            }
            idx += 1;
            continue;
        }
        // The observable path: Call(C.resolve, C, «item»), then Invoke(p, "then", «on_f, on_r»).
        if let Err(e) = combinator_slow_step(i, t, &result, mode, idx, item) {
            i.iterator_close(&iter);
            i.combinator_resolve(&result, false, e);
            return Ok(result);
        }
        idx += 1;
    }
    if mode != REACT_RACE {
        i.combinator_record(&result, mode == REACT_ANY, None);
    }
    Ok(result)
}

/// One element of [`combinator_fast`] through the observable path, with real element
/// functions (the capability's resolve/reject carry no per-element [[AlreadyCalled]]).
fn combinator_slow_step(
    i: &mut Interp,
    t: &Value,
    result: &Value,
    mode: u8,
    idx: u32,
    item: Value,
) -> Result<(), Value> {
    use crate::eval::promise_fast::{REACT_ALL, REACT_ANY, REACT_RACE};
    let p = promise_resolve(i, t, item)?;
    let flag = Value::Obj(Object::new_bare(None));
    let none = Value::Undefined;
    let elem = |i: &mut Interp, fulfil: bool, flag: &Value| {
        make_bound(
            i,
            promise_comb_element,
            vec![
                result.clone(),
                Value::Num(mode as f64),
                Value::Bool(fulfil),
                Value::Num(idx as f64),
                flag.clone(),
            ],
        )
    };
    let (on_f, on_r) = match mode {
        REACT_RACE => (elem(i, true, &none), elem(i, false, &none)),
        REACT_ANY => (elem(i, true, &none), elem(i, false, &flag)),
        REACT_ALL => (elem(i, true, &flag), elem(i, false, &none)),
        _ => (elem(i, true, &flag), elem(i, false, &flag)),
    };
    let then = ab(i.get_member(&p, "then"))?;
    ab(i.call(then, p, &[on_f, on_r]))?;
    Ok(())
}

/// The element / capability function of the observable combinator path. Bound args:
/// `[result, mode, fulfil, idx, alreadyCalled]` (`alreadyCalled` undefined for the capability
/// functions, whose [[AlreadyResolved]] lives in the combinator state), then the value.
fn promise_comb_element(i: &mut Interp, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let result = arg(args, 0);
    let mode = match arg(args, 1) {
        Value::Num(n) => n as u8,
        _ => return Ok(Value::Undefined),
    };
    let fulfil = matches!(arg(args, 2), Value::Bool(true));
    let idx = match arg(args, 3) {
        Value::Num(n) => n as u32,
        _ => 0,
    };
    // [[AlreadyCalled]]: the private record's extensible bit.
    if let Value::Obj(flag) = arg(args, 4) {
        let mut b = flag.borrow_mut();
        if !b.extensible {
            return Ok(Value::Undefined);
        }
        b.extensible = false;
    }
    i.combinator_settle(&result, mode, fulfil, idx, arg(args, 5));
    Ok(Value::Undefined)
}

/// The combinator fast path applies: `t` is %Promise% and `t.resolve` (read once, as
/// GetPromiseResolve does) is the original. `Err`: the read threw; `Ok(None)`: take the
/// general path.
fn combinator_gate(i: &mut Interp, t: &Value) -> Result<bool, Value> {
    if !is_intrinsic_promise_ctor(i, t) {
        return Ok(false);
    }
    let Some(intr) = i.promise_intr() else { return Ok(false) };
    // A data property holding the original: reading it is unobservable, and the general path
    // would read it again (the read happens after NewPromiseCapability there, which is
    // unobservable for %Promise%).
    Ok(matches!(proto_data(&intr.ctor, "resolve"), Some(Value::Obj(r)) if Gc::ptr_eq(&r, &intr.resolve)))
}
