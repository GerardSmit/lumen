//! Host-driven rendering callbacks. A rendering opportunity is supplied by the
//! embedder; requesting a callback never manufactures a timer or a frame.
use super::*;
use lumen::embed::{JsFunction, JsObject};
use lumen_host::time::Instant;
use std::{
    collections::{BTreeMap, VecDeque},
    time::Duration,
};

struct HtmlTask {
    realm: lumen::embed::RealmHandle,
    callback: Box<dyn FnOnce(&mut Ctx) -> OpResult<()>>,
}

#[derive(Default)]
struct TaskQueue {
    tasks: VecDeque<HtmlTask>,
}

/// Queue a user-agent task separately from Promise and mutation microtasks.
/// Captured JS values stay retained until invocation or realm teardown.
pub fn queue_task(
    ctx: &mut Ctx,
    task: impl FnOnce(&mut Ctx) -> OpResult<()> + 'static,
) -> OpResult<()> {
    let realm = ctx.current_host_realm();
    if ctx.host_mut::<TaskQueue>().is_none() {
        ctx.op_state().put(TaskQueue::default());
    }
    ctx.host_mut::<TaskQueue>()
        .expect("HTML task queue initialized")
        .tasks
        .push_back(HtmlTask {
            realm,
            callback: Box::new(task),
        });
    Ok(())
}

pub fn task_pending(ctx: &mut Ctx) -> bool {
    ctx.host_mut::<TaskQueue>()
        .is_some_and(|queue| !queue.tasks.is_empty())
}

/// Run admitted tasks with checkpoints between callbacks. New tasks wait for
/// a later host turn, and one callback's exception does not drop other tasks.
pub fn run_tasks(engine: &mut lumen::Engine, budget: usize) -> Vec<Value> {
    while engine.run_one_job() {}
    let count = engine
        .ctx()
        .host_mut::<TaskQueue>()
        .map_or(0, |queue| queue.tasks.len().min(budget));
    let mut errors = Vec::new();
    for _ in 0..count {
        let task = engine
            .ctx()
            .host_mut::<TaskQueue>()
            .and_then(|queue| queue.tasks.pop_front());
        if let Some(task) = task {
            match engine.ctx().with_host_realm(&task.realm, |ctx| {
                (task.callback)(ctx).map_err(|error| error.to_value(ctx))
            }) {
                Ok(Ok(())) => {}
                Ok(Err(error)) => errors.push(error),
                Err(error) => errors.push(engine.ctx().make_error("Error", error.to_string())),
            }
            while engine.run_one_job() {}
        }
    }
    errors
}

struct IdleRequest {
    callback: JsFunction,
    timeout: Option<Instant>,
}

struct Scheduler {
    next: u32,
    frames: BTreeMap<u32, JsFunction>,
    frame_order: Vec<u32>,
    idle: BTreeMap<u32, IdleRequest>,
    idle_order: Vec<u32>,
}

impl Scheduler {
    fn allocate(&mut self) -> u32 {
        loop {
            self.next = self.next.wrapping_add(1);
            if self.next != 0
                && !self.frames.contains_key(&self.next)
                && !self.idle.contains_key(&self.next)
            {
                return self.next;
            }
        }
    }
}

#[lumen_bind::class(name = "IdleDeadline", hint(js(webidl)))]
struct IdleDeadline {
    timeout: bool,
    deadline: Instant,
}

#[lumen_bind::methods]
impl IdleDeadline {
    #[getter]
    fn did_timeout(&self) -> bool {
        self.timeout
    }

    fn time_remaining(&self) -> f64 {
        self.deadline
            .saturating_duration_since(Instant::now())
            .as_secs_f64()
            * 1000.0
    }
}

#[lumen_bind::module(name = "rendering_callbacks")]
mod globals {
    use super::*;

    #[op(rename(js = "requestAnimationFrame"))]
    fn request_animation_frame(ctx: &mut Ctx, callback: JsFunction) -> u32 {
        let state = ctx
            .host_mut::<Scheduler>()
            .expect("rendering callbacks installed");
        let id = state.allocate();
        state.frames.insert(id, callback);
        state.frame_order.push(id);
        id
    }

    #[op(rename(js = "cancelAnimationFrame"))]
    fn cancel_animation_frame(ctx: &mut Ctx, handle: u32) {
        let state = ctx
            .host_mut::<Scheduler>()
            .expect("rendering callbacks installed");
        state.frames.remove(&handle);
        state.frame_order.retain(|id| *id != handle);
    }

    #[op(rename(js = "requestIdleCallback"))]
    fn request_idle_callback(
        ctx: &mut Ctx,
        callback: JsFunction,
        options: Option<JsObject>,
    ) -> OpResult<u32> {
        let timeout = if let Some(options) = options {
            match options.get(ctx, "timeout")? {
                Value::Undefined => None,
                value => {
                    let millis = ctx.coerce_number(&value).map_err(OpError::thrown)?;
                    // WebIDL unsigned-long conversion, including wrapping.
                    let millis = if millis.is_finite() {
                        millis.trunc().rem_euclid(4294967296.0) as u32
                    } else {
                        0
                    };
                    Some(Instant::now() + Duration::from_millis(u64::from(millis)))
                }
            }
        } else {
            None
        };
        let state = ctx
            .host_mut::<Scheduler>()
            .expect("rendering callbacks installed");
        let id = state.allocate();
        state.idle.insert(id, IdleRequest { callback, timeout });
        state.idle_order.push(id);
        Ok(id)
    }

    #[op(rename(js = "cancelIdleCallback"))]
    fn cancel_idle_callback(ctx: &mut Ctx, handle: u32) {
        let state = ctx
            .host_mut::<Scheduler>()
            .expect("rendering callbacks installed");
        state.idle.remove(&handle);
        state.idle_order.retain(|id| *id != handle);
    }
}

pub(crate) fn install(ctx: &mut Ctx) -> Result<(), Value> {
    lumen_host::perf::start_clock();
    ctx.op_state().put(Scheduler {
        next: 0,
        frames: BTreeMap::new(),
        frame_order: Vec::new(),
        idle: BTreeMap::new(),
        idle_order: Vec::new(),
    });
    let constructor = ctx.class_constructor::<IdleDeadline>();
    let global = ctx.global_object();
    crate::install_interface(ctx, &global, "IdleDeadline", constructor)
        .map_err(|_| ctx.make_error("Error", "IdleDeadline install failed"))?;
    ctx.install_module::<globals::Module>(&global)
}

/// Whether a rendering opportunity must wake this realm.
pub fn animation_frame_pending(ctx: &mut Ctx) -> bool {
    ctx.host_mut::<Scheduler>()
        .is_some_and(|state| !state.frames.is_empty())
        || super::animations::pending(ctx)
}

/// Earliest idle timeout, or zero when an idle opportunity could do useful work.
pub fn idle_delay_ms(ctx: &mut Ctx) -> Option<u64> {
    if task_pending(ctx) {
        return Some(0);
    }
    let state = ctx.host_mut::<Scheduler>()?;
    if state.idle.is_empty() {
        None
    } else {
        Some(0)
    }
}

/// Timeout wakeups remain necessary while the host is busy rendering, whereas
/// ordinary idle work waits for a genuine idle opportunity.
pub fn idle_timeout_delay_ms(ctx: &mut Ctx) -> Option<u64> {
    if task_pending(ctx) {
        return Some(0);
    }
    let now = Instant::now();
    ctx.host_mut::<Scheduler>()?
        .idle
        .values()
        .filter_map(|request| request.timeout)
        .map(|timeout| {
            let delay = timeout.saturating_duration_since(now);
            delay.as_millis().min(u128::from(u64::MAX)) as u64
                + u64::from(delay.subsec_nanos() % 1_000_000 != 0)
        })
        .min()
}

/// Run one rendering opportunity. Snapshot handles, but remove each entry only
/// immediately before invocation: an earlier callback may cancel a later one.
/// Requests made by callbacks are left for the next opportunity.
pub fn run_animation_frame(engine: &mut lumen::Engine) -> Vec<Value> {
    let timestamp = lumen_host::perf::now_ms();
    let mut errors = Vec::new();
    if let Err(error) = super::animations::advance(engine.ctx(), timestamp) {
        errors.push(error.to_value(engine.ctx()));
    }
    while engine.run_one_job() {}
    let Some(state) = engine.ctx().host_mut::<Scheduler>() else {
        return errors;
    };
    let handles = state.frame_order.clone();
    for handle in handles {
        let callback = engine.ctx().host_mut::<Scheduler>().and_then(|state| {
            state.frame_order.retain(|id| *id != handle);
            state.frames.remove(&handle)
        });
        if let Some(callback) = callback {
            if let Err(error) =
                callback.call(engine.ctx(), Value::Undefined, &[Value::Num(timestamp)])
            {
                errors.push(error.to_value(engine.ctx()));
            }
            // The HTML callback invocation performs a microtask checkpoint.
            while engine.run_one_job() {}
        }
    }
    errors
}

/// Run idle work within a host supplied budget (at most 50ms). Expired timeout
/// requests run even with no remaining idle time. Reentrant requests wait for
/// the next idle opportunity.
pub fn run_idle_callbacks(engine: &mut lumen::Engine, budget_ms: u32) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_millis(u64::from(budget_ms.min(50)));
    let Some(state) = engine.ctx().host_mut::<Scheduler>() else {
        return Vec::new();
    };
    let handles = state.idle_order.clone();
    let mut errors = Vec::new();
    for handle in handles {
        let now = Instant::now();
        let state = engine
            .ctx()
            .host_mut::<Scheduler>()
            .expect("rendering callbacks installed");
        let Some(request) = state.idle.get(&handle) else {
            continue;
        };
        let timeout = request.timeout.is_some_and(|time| time <= now);
        if !timeout && now >= deadline {
            continue;
        }
        let request = state.idle.remove(&handle).unwrap();
        state.idle_order.retain(|id| *id != handle);
        let argument = engine.ctx().new_instance(IdleDeadline {
            timeout,
            deadline: if timeout { now } else { deadline },
        });
        if let Err(error) = request
            .callback
            .call(engine.ctx(), Value::Undefined, &[argument])
        {
            errors.push(error.to_value(engine.ctx()));
        }
        while engine.run_one_job() {}
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(engine: &mut lumen::Engine, source: &str) -> Value {
        match engine.eval_value(source) {
            Ok(Ok(value)) => value,
            Ok(Err(_)) => panic!("scheduling test script threw"),
            Err(error) => panic!("scheduling test script parse failed: {}", error.message),
        }
    }

    #[test]
    fn user_agent_tasks_enter_their_enqueue_realm_and_restore_the_host() {
        let mut engine = lumen::Engine::new();
        let parent = engine.ctx().current_host_realm();
        let child = engine.ctx().create_host_realm();
        let parent_global = parent.global();
        let child_global = child.global();
        engine
            .ctx()
            .with_host_realm(&child, |ctx| {
                queue_task(ctx, |ctx| {
                    let object = ctx.new_object();
                    let global = ctx.global_object();
                    ctx.member_set(&global, "taskObject", Value::Obj(object))
                        .map_err(OpError::thrown)?;
                    Err(OpError::new("Error", "child task error"))
                })
                .expect("queue child task");
            })
            .expect("enter child");
        queue_task(engine.ctx(), |ctx| {
            let global = ctx.global_object();
            ctx.member_set(&global, "parentTaskRan", Value::Bool(true))
                .map_err(OpError::thrown)
        })
        .expect("queue parent task");
        let errors = run_tasks(&mut engine, 2);
        assert_eq!(errors.len(), 1);
        let restored_global = engine.ctx().global_object();
        let restored_address = engine.ctx().object_addr(&restored_global);
        assert_eq!(restored_address, engine.ctx().object_addr(&parent_global));
        let child_object = engine
            .ctx()
            .member_get(&child_global, "taskObject")
            .ok()
            .expect("child object");
        let child_prototype = engine
            .ctx()
            .member_get(&child_global, "Object")
            .ok()
            .and_then(|constructor| engine.ctx().member_get(&constructor, "prototype").ok())
            .expect("child Object prototype");
        let object_prototype = engine.ctx().prototype_of(&child_object);
        let object_address = engine.ctx().object_addr(&object_prototype);
        assert_eq!(object_address, engine.ctx().object_addr(&child_prototype));
        assert!(matches!(
            engine.ctx().member_get(&parent_global, "parentTaskRan"),
            Ok(Value::Bool(true))
        ));
        assert!(matches!(
            engine.ctx().member_get(&parent_global, "taskObject"),
            Ok(Value::Undefined)
        ));
    }

    #[test]
    fn user_agent_tasks_checkpoint_snapshot_and_preserve_queue_after_errors() {
        let mut engine = lumen::Engine::new();
        assert!(install(engine.ctx()).is_ok());
        eval(
            &mut engine,
            "var log=[];Promise.resolve().then(()=>log.push('before'))",
        );
        let first = JsFunction::from_value(eval(
            &mut engine,
            "(()=>{log.push('first');Promise.resolve().then(()=>log.push('micro'))})",
        ))
        .unwrap();
        let later = JsFunction::from_value(eval(&mut engine, "(()=>log.push('later'))")).unwrap();
        let last = JsFunction::from_value(eval(&mut engine, "(()=>log.push('last'))")).unwrap();
        queue_task(engine.ctx(), move |ctx| {
            first.call(ctx, Value::Undefined, &[])?;
            queue_task(ctx, move |ctx| {
                later.call(ctx, Value::Undefined, &[]).map(|_| ())
            })
        })
        .unwrap();
        queue_task(engine.ctx(), |_| Err(OpError::new("Error", "task failure"))).unwrap();
        queue_task(engine.ctx(), move |ctx| {
            last.call(ctx, Value::Undefined, &[]).map(|_| ())
        })
        .unwrap();
        assert_eq!(idle_delay_ms(engine.ctx()), Some(0));
        assert_eq!(idle_timeout_delay_ms(engine.ctx()), Some(0));
        assert_eq!(run_tasks(&mut engine, 3).len(), 1);
        assert!(matches!(
            eval(&mut engine, "log.join(',')==='before,first,micro,last'"),
            Value::Bool(true)
        ));
        assert!(task_pending(engine.ctx()));
        assert!(run_tasks(&mut engine, 1).is_empty());
        assert!(matches!(
            eval(
                &mut engine,
                "log.join(',')==='before,first,micro,last,later'"
            ),
            Value::Bool(true)
        ));
        assert!(!task_pending(engine.ctx()));
        assert_eq!(idle_delay_ms(engine.ctx()), None);
    }

    #[test]
    fn wrapped_handles_preserve_registration_order() {
        let mut engine = lumen::Engine::new();
        assert!(install(engine.ctx()).is_ok());
        engine.ctx().host_mut::<Scheduler>().unwrap().next = u32::MAX - 1;
        eval(
            &mut engine,
            "var order=[];requestAnimationFrame(()=>order.push('first'));requestAnimationFrame(()=>order.push('second'))",
        );
        assert!(run_animation_frame(&mut engine).is_empty());
        assert!(matches!(
            eval(&mut engine, "order.join(',')==='first,second'"),
            Value::Bool(true)
        ));
        engine.ctx().host_mut::<Scheduler>().unwrap().next = u32::MAX - 1;
        eval(
            &mut engine,
            "order=[];requestIdleCallback(()=>order.push('first'),{timeout:0});requestIdleCallback(()=>order.push('second'),{timeout:0})",
        );
        assert!(run_idle_callbacks(&mut engine, 0).is_empty());
        assert!(matches!(
            eval(&mut engine, "order.join(',')==='first,second'"),
            Value::Bool(true)
        ));
    }

    #[test]
    fn animation_callbacks_snapshot_cancel_reenter_and_checkpoint() {
        let mut engine = lumen::Engine::new();
        assert!(install(engine.ctx()).is_ok());
        eval(
            &mut engine,
            "var log=[];var times=[];requestAnimationFrame(t=>{log.push('a');times.push(t);cancelAnimationFrame(cancelled);requestAnimationFrame(()=>log.push('next'));Promise.resolve().then(()=>log.push('micro'))});var cancelled=requestAnimationFrame(()=>log.push('cancelled'));requestAnimationFrame(t=>{log.push('b');times.push(t)})",
        );
        assert!(animation_frame_pending(engine.ctx()));
        assert!(run_animation_frame(&mut engine).is_empty());
        assert!(matches!(
            eval(
                &mut engine,
                "log.join(',')==='a,micro,b' && times[0]===times[1]"
            ),
            Value::Bool(true)
        ));
        assert!(run_animation_frame(&mut engine).is_empty());
        assert!(matches!(
            eval(&mut engine, "log.join(',')==='a,micro,b,next'"),
            Value::Bool(true)
        ));
        assert!(!animation_frame_pending(engine.ctx()));
    }

    #[test]
    fn callback_failure_does_not_drop_following_callbacks() {
        let mut engine = lumen::Engine::new();
        assert!(install(engine.ctx()).is_ok());
        eval(
            &mut engine,
            "var ran=false;requestAnimationFrame(()=>{throw new Error('frame')});requestAnimationFrame(()=>ran=true)",
        );
        assert_eq!(run_animation_frame(&mut engine).len(), 1);
        assert!(matches!(eval(&mut engine, "ran"), Value::Bool(true)));
    }

    #[test]
    fn idle_budget_timeout_cancel_and_reentrant_requests() {
        let mut engine = lumen::Engine::new();
        assert!(install(engine.ctx()).is_ok());
        eval(
            &mut engine,
            "var log=[];var cancelled=requestIdleCallback(()=>log.push('cancelled'));cancelIdleCallback(cancelled);requestIdleCallback(d=>{log.push('timeout:'+d.didTimeout+':'+d.timeRemaining());requestIdleCallback(()=>log.push('later'))},{timeout:0});requestIdleCallback(d=>log.push('idle:'+d.didTimeout))",
        );
        assert!(run_idle_callbacks(&mut engine, 0).is_empty());
        assert!(matches!(
            eval(&mut engine, "log.join(',')==='timeout:true:0'"),
            Value::Bool(true)
        ));
        assert!(run_idle_callbacks(&mut engine, 50).is_empty());
        assert!(matches!(
            eval(
                &mut engine,
                "log.join(',')==='timeout:true:0,idle:false,later'"
            ),
            Value::Bool(true)
        ));
    }
}
