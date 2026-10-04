use super::{
    runtime::{Event, Link, Reservation},
    CancelReason, CpuHint, Job, Limits, ParallelHost, Parcel, TaskHandle,
};
use crate::{
    interpreter::Interp,
    value::{set_data, Exotic, Object, Value},
    Engine,
};
use std::{collections::HashMap, sync::Arc, time::Duration};

pub(crate) struct Realm {
    host: Arc<dyn ParallelHost>,
    limits: Limits,
    pub(crate) tasks: HashMap<u64, TaskHandle>,
    reservations: usize,
    pub worker: Option<TaskHandle>,
    pub(crate) migration: Option<(CpuHint, Parcel)>,
    migration_reserved: bool,
    #[cfg(feature = "aot-native")]
    native_glue: Option<&'static [u8]>,
}
#[cfg(test)]
impl Realm {
    pub(crate) fn task_count(&self) -> usize {
        self.tasks.len()
    }
    pub(crate) fn first_task(&self) -> TaskHandle {
        self.tasks.values().next().unwrap().clone()
    }
}
impl Drop for Realm {
    fn drop(&mut self) {
        for task in self.tasks.values() {
            task.0.parent_closed();
        }
    }
}

pub fn install(engine: &mut Engine, host: Arc<dyn ParallelHost>) {
    install_with_limits(engine, host, Limits::default());
}
/// Abort this realm's lifetime signal and cancel every child task.
/// Call on the realm's owner thread before the host tears down its services.
pub fn shutdown(engine: &mut Engine, grace: Duration) {
    if !engine.interp.host_state.has::<Realm>() {
        return;
    }
    let tasks = engine
        .interp
        .host_mut::<Realm>()
        .unwrap()
        .tasks
        .values()
        .cloned()
        .collect::<Vec<_>>();
    for task in tasks {
        task.cancel(CancelReason::Shutdown, grace);
    }
    let reason = abort_error(&engine.interp, "system shutdown", "ERR_SHUTDOWN");
    let _ = invoke(&mut engine.interp, "__parallelRealmAbort", &[reason]);
}

impl Engine {
    /// Abort the realm lifetime and cancel its children with the same reason.
    pub fn abort_realm(&mut self, reason: Value, grace: Duration) -> Result<(), Value> {
        let Some(realm) = self.interp.host_mut::<Realm>() else {
            return Ok(());
        };
        let tasks = realm.tasks.values().cloned().collect::<Vec<_>>();
        let parcels = tasks
            .iter()
            .map(|task| Parcel::build(&mut self.interp, &reason, task.0.limits))
            .collect::<Result<Vec<_>, _>>()?;
        for (task, parcel) in tasks.into_iter().zip(parcels) {
            task.cancel(CancelReason::User(parcel), grace);
        }
        invoke(&mut self.interp, "__parallelRealmAbort", &[reason]).map(|_| ())
    }
}
pub fn install_with_limits(engine: &mut Engine, host: Arc<dyn ParallelHost>, limits: Limits) {
    if !register(engine, host, limits) {
        return;
    }
    match engine
        .eval_snapshot(include_bytes!("glue.bin"), include_str!("glue.js"), false)
        .expect("parallel glue snapshot decodes")
    {
        crate::Completion::Value(_) => {}
        crate::Completion::Throw { name, message } => {
            panic!("parallel installation: {name}: {message}")
        }
    }
    #[cfg(feature = "aot-native")]
    register_namespace(engine).unwrap_or_else(|error| panic!("parallel namespace: {error}"));
}

#[cfg(feature = "aot-native")]
fn register_namespace(engine: &mut Engine) -> Result<(), String> {
    let global = engine.interp.global_env.clone();
    let lumen = engine
        .interp
        .get_var("Lumen", &global)
        .map_err(|_| "parallel glue did not define Lumen".to_string())?;
    let parallel = engine
        .interp
        .get_member(&lumen, "parallel")
        .map_err(|_| "parallel glue did not define Lumen.parallel".to_string())?;
    let namespace = Object::new_bare(None);
    for name in ["run", "spawn"] {
        let value = engine
            .interp
            .get_member(&parallel, name)
            .map_err(|_| format!("parallel glue did not define {name}"))?;
        namespace.borrow_mut().props.insert(
            name,
            crate::value::Property::data(value, false, true, false),
        );
    }
    namespace.borrow_mut().extensible = false;
    engine
        .register_native_module("lumen:parallel", Value::Obj(namespace))
        .map_err(str::to_string)
}

/// Load firmware-authenticated native glue after registering its Rust operations.
#[cfg(feature = "aot-native")]
pub fn install_native_with_limits(
    engine: &mut Engine,
    host: Arc<dyn ParallelHost>,
    limits: Limits,
    bytes: &'static [u8],
) -> Result<(), String> {
    if !register(engine, host, limits) {
        return Ok(());
    }
    engine.interp.host_mut::<Realm>().unwrap().native_glue = Some(bytes);
    let result = engine
        .load_native_glue_value(bytes)
        .map(|_| crate::Completion::Value(String::new()));
    match result {
        Ok(crate::Completion::Value(_)) => {
            let result = register_namespace(engine);
            if result.is_err() {
                engine.interp.host_state.take::<Realm>();
            }
            result
        }
        Ok(crate::Completion::Throw { name, message }) => {
            engine.interp.host_state.take::<Realm>();
            Err(format!("parallel installation: {name}: {message}"))
        }
        Err(error) => {
            engine.interp.host_state.take::<Realm>();
            Err(error)
        }
    }
}

fn register(engine: &mut Engine, host: Arc<dyn ParallelHost>, limits: Limits) -> bool {
    if engine.interp.host_state.has::<Realm>() {
        return false;
    }
    engine.interp.host_state.put(Realm {
        host,
        limits,
        tasks: HashMap::new(),
        reservations: 0,
        worker: None,
        migration: None,
        migration_reserved: false,
        #[cfg(feature = "aot-native")]
        native_glue: None,
    });
    for (name, length, function) in [
        ("__parallelStart", 5, start as crate::value::NativeFn),
        ("__parallelPost", 3, post),
        ("__parallelClose", 1, close),
        ("__parallelCancel", 3, cancel),
        ("__parallelComplete", 2, done),
        ("__parallelConsumed", 1, consumed),
    ] {
        engine.define_global(name, length, function);
    }
    assert!(
        engine.define_globals::<migration_ops::Module>().is_ok(),
        "parallel migration globals"
    );
    true
}

fn argument(args: &[Value], index: usize) -> &Value {
    args.get(index).unwrap_or(&Value::Undefined)
}
fn elements(interp: &mut Interp, value: &Value) -> Result<Vec<Value>, Value> {
    let Some(array) = value.as_obj() else {
        return Err(interp.make_error("TypeError", "parallel args and transfer must be arrays"));
    };
    if array.borrow().exotic != Exotic::Array {
        return Err(interp.make_error("TypeError", "parallel args and transfer must be arrays"));
    }
    let length = array
        .borrow()
        .props
        .get("length")
        .map(|property| property.value())
        .unwrap_or(Value::Num(0.0));
    let Value::Num(length) = length else {
        return Err(interp.make_error("TypeError", "invalid array length"));
    };
    if length > 2_000_000.0 {
        return Err(interp.make_error("RangeError", "parallel argument array limit exceeded"));
    }
    (0..length as usize)
        .map(|index| {
            interp
                .get_member(value, &index.to_string())
                .map_err(|error| match error {
                    crate::interpreter::Abrupt::Throw(value) => value,
                    _ => interp.make_error("TypeError", "array property read failed"),
                })
        })
        .collect()
}
pub(crate) fn invoke(interp: &mut Interp, name: &str, args: &[Value]) -> Result<Value, Value> {
    let function = interp
        .global
        .borrow()
        .props
        .get(name)
        .map(|property| property.value())
        .unwrap_or(Value::Undefined);
    interp.invoke(function, Value::Undefined, args)
}
pub(crate) fn abort_error(interp: &Interp, message: &str, code: &str) -> Value {
    let object = Object::new(Some(interp.error_protos["Error"].clone()));
    object
        .borrow_mut()
        .set_exotic(Exotic::Error, Some(Value::Undefined));
    set_data(&object, "message", Value::str(message));
    set_data(&object, "name", Value::str("AbortError"));
    if !code.is_empty() {
        set_data(&object, "code", Value::str(code));
    }
    Value::Obj(object)
}
fn endpoint(interp: &mut Interp, value: &Value) -> Result<(TaskHandle, bool), Value> {
    let Value::Num(id) = value else {
        return Err(interp.make_error("TypeError", "invalid task handle"));
    };
    let task = interp.host_mut::<Realm>().and_then(|realm| {
        if *id == 0.0 {
            realm.worker.clone()
        } else {
            realm.tasks.get(&(*id as u64)).cloned()
        }
    });
    task.map(|task| (task, *id == 0.0))
        .ok_or_else(|| interp.make_error("TypeError", "task is closed"))
}

fn start(interp: &mut Interp, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let function = argument(args, 0);
    if !function
        .as_obj()
        .is_some_and(|object| object.borrow().call.is_fn())
    {
        return Err(interp.make_error("TypeError", "parallel function must be callable"));
    }
    if !argument(args, 1)
        .as_obj()
        .is_some_and(|array| array.borrow().exotic == Exotic::Array)
    {
        return Err(interp.make_error("TypeError", "parallel args must be an array"));
    }
    let transfer = elements(interp, argument(args, 3))?;
    let cpu = match argument(args, 2) {
        Value::Str(value) if &**value == "any" => CpuHint::Any,
        Value::Str(value) if &**value == "efficiency" => CpuHint::Efficiency,
        Value::Str(value) if &**value == "performance" => CpuHint::Performance,
        _ => {
            return Err(
                interp.make_error("TypeError", "cpu must be any, efficiency or performance")
            );
        }
    };
    let (host, limits) = {
        let realm = interp.host_mut::<Realm>().unwrap();
        if realm.tasks.len() + realm.reservations >= realm.limits.active_tasks {
            return Err(interp.make_error("RangeError", "parallel active task limit exceeded"));
        }
        realm.reservations += 1;
        (realm.host.clone(), realm.limits)
    };
    let root = Value::Obj(Object::new_array_from_vec(
        Some(interp.array_proto.clone()),
        vec![function.clone(), argument(args, 1).clone()],
    ));
    let parcel = Parcel::build_with_transfer(interp, &root, &transfer, limits);
    interp.host_mut::<Realm>().unwrap().reservations -= 1;
    let parcel = parcel?;
    let link = Link::new(host.clone(), limits);
    #[cfg(feature = "aot-native")]
    {
        *link.native_glue.lock().unwrap() = interp.host_mut::<Realm>().unwrap().native_glue.clone();
    }
    let job = Job {
        jit_mode: interp.jit_mode,
        link: link.clone(),
        parcel,
        spawn: matches!(argument(args, 4), Value::Bool(true)),
        previous_core: None,
    };
    let (placement, handle) = host
        .spawn(cpu, job)
        .map_err(|error| interp.make_error("Error", error.0))?;
    assert!(
        Arc::ptr_eq(&link, &handle.0),
        "host returned a handle for a different job"
    );
    interp
        .host_mut::<Realm>()
        .unwrap()
        .tasks
        .insert(link.id, handle);
    arm(interp, &link);
    let result = Object::new(Some(interp.object_proto.clone()));
    for (name, value) in [
        ("id", Value::Num(link.id as f64)),
        ("core", Value::Num(placement.core as f64)),
        ("class", Value::str(placement.class.name())),
        ("fallback", Value::Bool(placement.fallback)),
    ] {
        set_data(&result, name, value);
    }
    Ok(Value::Obj(result))
}
#[lumen_bind::module(name = "parallel_migration")]
mod migration_ops {
    use super::*;
    #[op(name = "__parallelMigrate")]
    fn checkpoint(
        ctx: &mut crate::embed::Ctx,
        function: Value,
        args: Value,
        cpu: Value,
        transfer: Value,
        messages: Value,
    ) -> Result<Value, Value> {
        migrate(ctx, &[function, args, cpu, transfer, messages])
    }
}
fn migrate(interp: &mut Interp, args: &[Value]) -> Result<Value, Value> {
    let realm = interp.host_mut::<Realm>().unwrap();
    let Some(task) = realm.worker.clone() else {
        return Err(interp.make_error("TypeError", "migration requires a worker realm"));
    };
    if !realm.tasks.is_empty() || realm.migration.is_some() || realm.migration_reserved {
        return Err(interp.make_error("TypeError", "realm has child tasks or a pending migration"));
    }
    let cpu = match argument(args, 2) {
        Value::Str(value) if &**value == "any" => CpuHint::Any,
        Value::Str(value) if &**value == "efficiency" => CpuHint::Efficiency,
        Value::Str(value) if &**value == "performance" => CpuHint::Performance,
        _ => {
            return Err(
                interp.make_error("TypeError", "cpu must be any, efficiency or performance")
            );
        }
    };
    interp.host_mut::<Realm>().unwrap().migration_reserved = true;
    let result = (|| {
        let transfer = elements(interp, argument(args, 3))?;
        if !task.0.host.can_migrate(interp)
            || interp.has_pending_async()
            || !interp.pending_async_waits.is_empty()
            || !interp.pending_timers.is_empty()
        {
            return Err(interp.make_error("TypeError", "realm has pending host resources"));
        }
        if !argument(args, 0).is_callable()
            || !argument(args, 1)
                .as_obj()
                .is_some_and(|a| a.borrow().exotic == Exotic::Array)
        {
            return Err(interp.make_error(
                "TypeError",
                "migration requires a function and argument array",
            ));
        }
        let root = Value::Obj(Object::new_array_from_vec(
            Some(interp.array_proto.clone()),
            vec![
                argument(args, 0).clone(),
                argument(args, 1).clone(),
                argument(args, 4).clone(),
            ],
        ));
        let parcel = Parcel::build_checked(interp, &root, &transfer, task.0.limits, |interp| {
            if !task.0.host.can_migrate(interp)
                || interp.has_pending_async()
                || !interp.pending_async_waits.is_empty()
                || !interp.pending_timers.is_empty()
                || !interp.host_mut::<Realm>().unwrap().tasks.is_empty()
            {
                return Err(interp.make_error("TypeError", "realm has pending host resources"));
            }
            let queues = task.0.queues.lock().unwrap();
            if queues.cancelled || task.is_finished() || task.is_interrupted() {
                Err(interp.make_error("TypeError", "task is cancelled"))
            } else {
                Ok(())
            }
        })?;
        interp.host_mut::<Realm>().unwrap().migration = Some((cpu, parcel));
        Ok(Value::Undefined)
    })();
    interp.host_mut::<Realm>().unwrap().migration_reserved = false;
    result
}
fn post(interp: &mut Interp, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let (task, outgoing) = endpoint(interp, argument(args, 0))?;
    let transfer = elements(interp, argument(args, 2))?;
    let reservation = Reservation::new(&task.0, outgoing).map_err(|message| {
        interp.make_error(
            if message.contains("limit") {
                "RangeError"
            } else {
                "TypeError"
            },
            message,
        )
    })?;
    let parcel = Parcel::build_checked(
        interp,
        argument(args, 1),
        &transfer,
        task.0.limits,
        |interp| {
            reservation
                .validate()
                .map_err(|message| interp.make_error("TypeError", message))
        },
    )?;
    reservation.send(parcel);
    Ok(Value::Undefined)
}
fn close(interp: &mut Interp, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let (task, outgoing) = endpoint(interp, argument(args, 0))?;
    {
        let mut queues = task.0.queues.lock().unwrap();
        queues.incoming_closed = true;
        if outgoing {
            queues.outgoing_closed = true;
        }
    }
    task.0.wake();
    if outgoing {
        task.0.push(Event::Closed);
    }
    Ok(Value::Undefined)
}
fn consumed(interp: &mut Interp, _this: Value, args: &[Value]) -> Result<Value, Value> {
    if let Ok((task, worker)) = endpoint(interp, argument(args, 0)) {
        let mut queues = task.0.queues.lock().unwrap();
        if worker {
            queues.incoming_count = queues.incoming_count.saturating_sub(1);
        } else {
            queues.outgoing_count = queues.outgoing_count.saturating_sub(1);
        }
    }
    Ok(Value::Undefined)
}
fn cancel(interp: &mut Interp, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let (task, _) = endpoint(interp, argument(args, 0))?;
    let reason = match Parcel::build(interp, argument(args, 1), task.0.limits) {
        Ok(reason) => reason,
        Err(_) => {
            let text = interp
                .coerce_string(argument(args, 1))
                .map(|s| s.to_string())
                .unwrap_or_else(|_| "unclonable abort reason".into());
            let reason = abort_error(interp, &text, "");
            Parcel::build(interp, &reason, Limits::default()).unwrap_or_else(|error| {
                panic!(
                    "AbortError clone failed: {}",
                    interp
                        .coerce_string(&error)
                        .map(|s| s.to_string())
                        .unwrap_or_default()
                )
            })
        }
    };
    let grace = match argument(args, 2) {
        Value::Num(value) => Duration::try_from_secs_f64(*value / 1000.0).map_err(|_| {
            interp.make_error("RangeError", "grace must be a finite nonnegative duration")
        })?,
        _ => {
            return Err(
                interp.make_error("RangeError", "grace must be a finite nonnegative duration")
            );
        }
    };
    task.cancel(CancelReason::User(reason), grace);
    Ok(Value::Undefined)
}
fn done(interp: &mut Interp, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let (task, _) = endpoint(interp, &Value::Num(0.0))?;
    complete(
        interp,
        &task.0,
        argument(args, 0).clone(),
        matches!(argument(args, 1), Value::Bool(true)),
    );
    Ok(Value::Undefined)
}
pub(crate) fn complete(interp: &mut Interp, link: &Arc<Link>, value: Value, failed: bool) {
    let (parcel, failed) = match Parcel::build(interp, &value, link.limits) {
        Ok(parcel) => (parcel, failed),
        Err(error) => (
            Parcel::build(interp, &error, Limits::default())
                .unwrap_or_else(|_| panic!("clone error is cloneable")),
            true,
        ),
    };
    link.queues.lock().unwrap().pending_result = Some((parcel, failed));
}
fn arm(interp: &mut Interp, link: &Arc<Link>) {
    if !link.parent_alive.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let deferred = interp.new_deferred();
    let completer = interp.completer(deferred);
    let ready = {
        let mut queues = link.queues.lock().unwrap();
        assert!(
            queues.completer.is_none(),
            "duplicate parent wake registration"
        );
        queues.completer = Some(completer);
        !queues.outgoing.is_empty()
    };
    if ready {
        link.notify();
    }
}
pub(crate) fn drain(interp: &mut Interp, link: &Arc<Link>) {
    let events = link
        .queues
        .lock()
        .unwrap()
        .outgoing
        .drain(..)
        .collect::<Vec<_>>();
    let mut terminal = false;
    for event in events {
        let (kind, value) = match event {
            Event::Message(parcel) => match interp.try_adopt(parcel) {
                Ok(value) => (0, value),
                Err(error) => (2, error),
            },
            Event::Result(parcel, failed) => {
                terminal = true;
                if link.queues.lock().unwrap().cancelled {
                    drop(parcel);
                    (5, Value::Undefined)
                } else {
                    match interp.try_adopt(parcel) {
                        Ok(value) => (if failed { 2 } else { 1 }, value),
                        Err(error) => (2, error),
                    }
                }
            }
            Event::Cancel(reason) => {
                let value = match reason {
                    CancelReason::User(parcel) => {
                        let value = interp.try_adopt(parcel).unwrap_or_else(|error| error);
                        if let Ok(parcel) = Parcel::build(interp, &value, Limits::default()) {
                            let mut queues = link.queues.lock().unwrap();
                            queues.abort = Some(parcel);
                            queues.cancel_reason = None;
                        }
                        link.wake();
                        value
                    }
                    reason => {
                        let (message, code) = reason.description();
                        abort_error(interp, message, code)
                    }
                };
                (3, value)
            }
            Event::Closed => {
                terminal = link.finished.load(std::sync::atomic::Ordering::Acquire);
                (if terminal { 5 } else { 4 }, Value::Undefined)
            }
            Event::Placement(placement) => {
                let value = Object::new(Some(interp.object_proto.clone()));
                set_data(&value, "core", Value::Num(placement.core as f64));
                set_data(&value, "class", Value::str(placement.class.name()));
                set_data(&value, "fallback", Value::Bool(placement.fallback));
                (6, Value::Obj(value))
            }
        };
        let _ = invoke(
            interp,
            "__parallelDispatch",
            &[Value::Num(link.id as f64), Value::Num(kind as f64), value],
        );
    }
    if terminal {
        if interp
            .host_mut::<Realm>()
            .unwrap()
            .tasks
            .remove(&link.id)
            .is_some()
        {
            let mut stats = interp.jit_stats.get();
            stats += link.queues.lock().unwrap().jit_stats;
            interp.jit_stats.set(stats);
        }
    } else {
        arm(interp, link);
    }
}
