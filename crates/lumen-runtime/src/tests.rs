use super::*;
use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

/// A console sink the test can read back after the loop runs.
#[derive(Clone, Default)]
struct Captured(Rc<RefCell<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn lines(&self) -> Vec<String> {
        String::from_utf8(self.0.borrow().clone())
            .expect("utf8 console output")
            .lines()
            .map(str::to_string)
            .collect()
    }
}

/// A runtime with console captured: (runtime, stdout sink, stderr sink).
fn test_runtime() -> (Runtime, Captured, Captured) {
    let mut rt = Runtime::new();
    let out = Captured::default();
    let err = Captured::default();
    rt.engine().ctx().op_state().put(console::ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(err.clone()),
    });
    (rt, out, err)
}

fn eval_ok(rt: &mut Runtime, src: &str) {
    match rt.eval(src).expect("parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
}

#[test]
fn browser_host_realm_installs_isolated_globals_on_shared_runtime_state() {
    let (mut runtime, _, _) = test_runtime();
    let _queued_parent_timer = runtime
        .engine()
        .eval_value(
            "globalThis.parentTimerRan = false; setTimeout(() => { parentTimerRan = true; }, 0)",
        )
        .expect("parent timer script parses")
        .unwrap_or_else(|_| panic!("parent timer script runs"));

    let child = runtime.engine().ctx().create_host_realm();
    runtime
        .install_browser_realm(&child)
        .expect("browser providers install in child realm");

    let child_capabilities = runtime
        .engine()
        .eval_value_in_host_realm(
            &child,
            "typeof URL === 'function' && typeof fetch === 'function' && typeof TextEncoder === 'function' && typeof setTimeout === 'function' && typeof process === 'undefined' && typeof require === 'undefined' && typeof Worker === 'undefined'",
            false,
        )
        .expect("child browser capability script parses")
        .unwrap_or_else(|_| panic!("child browser capability script runs"));
    assert!(matches!(child_capabilities, Value::Bool(true)));

    let parent_has_node = runtime
        .engine()
        .eval_value("typeof process === 'object' && typeof URL === 'function'")
        .expect("parent capability script parses")
        .unwrap_or_else(|_| panic!("parent capability script runs"));
    assert!(matches!(parent_has_node, Value::Bool(true)));

    let queued_without_checkpoint = runtime
        .engine()
        .eval_value_in_host_realm(
            &child,
            "globalThis.childMicrotaskRan = false; globalThis.childTimerRan = false; const childGlobal = globalThis; queueMicrotask(() => { childMicrotaskRan = globalThis === childGlobal; }); setTimeout(() => { childTimerRan = globalThis === childGlobal; }, 0); true",
            false,
        )
        .expect("child callback script parses")
        .unwrap_or_else(|_| panic!("child callback script runs"));
    assert!(matches!(queued_without_checkpoint, Value::Bool(true)));
    let callbacks_are_still_queued = runtime
        .engine()
        .eval_value_in_host_realm(&child, "!childMicrotaskRan && !childTimerRan", false)
        .expect("child pending result script parses")
        .unwrap_or_else(|_| panic!("child pending result script runs"));
    assert!(matches!(callbacks_are_still_queued, Value::Bool(true)));

    // The child callbacks and the parent's pre-existing timer all share this Runtime's event
    // loop, but each JS closure still executes in the realm where it was created.
    runtime.run_until_idle();
    let child_callbacks_ran_in_child = runtime
        .engine()
        .eval_value_in_host_realm(&child, "childMicrotaskRan && childTimerRan", false)
        .expect("child result script parses")
        .unwrap_or_else(|_| panic!("child result script runs"));
    assert!(matches!(child_callbacks_ran_in_child, Value::Bool(true)));
    let parent_timer_ran = runtime
        .engine()
        .eval_value("parentTimerRan")
        .expect("parent result script parses")
        .unwrap_or_else(|_| panic!("parent result script runs"));
    assert!(matches!(parent_timer_ran, Value::Bool(true)));
}

fn eval_host_value(
    runtime: &mut Runtime,
    realm: &lumen::embed::RealmHandle,
    source: &str,
) -> Value {
    runtime
        .engine()
        .eval_value_in_host_realm(realm, source, false)
        .unwrap_or_else(|_| panic!("host-realm script parses"))
        .unwrap_or_else(|_| panic!("host-realm script runs"))
}

#[test]
fn browser_timers_are_owned_by_their_realm_and_retained_callbacks_survive_cancel() {
    let (mut runtime, _, stderr) = test_runtime();
    let parent = runtime.engine().ctx().current_host_realm();
    let child = runtime.engine().ctx().create_host_realm();
    runtime
        .install_browser_realm(&child)
        .expect("browser providers install in the child realm");

    let child_setup = eval_host_value(
        &mut runtime,
        &child,
        "globalThis.intervalCalls = 0; globalThis.timeoutRan = false; globalThis.retainedCalls = 0; true",
    );
    assert!(matches!(child_setup, Value::Bool(true)));
    let child_interval = eval_host_value(&mut runtime, &child, "setInterval");
    let child_timeout = eval_host_value(&mut runtime, &child, "setTimeout");
    let child_interval_callback =
        eval_host_value(&mut runtime, &child, "() => { intervalCalls++; }");
    let child_timeout_callback =
        eval_host_value(&mut runtime, &child, "() => { timeoutRan = true; }");
    let retained_callback = eval_host_value(&mut runtime, &child, "() => { retainedCalls++; }");

    // Schedule through child-realm native functions while the parent is the original caller.
    // The timer belongs to the function's target realm, not the invocation/security actor.
    let interval_id = runtime
        .engine()
        .call_function(
            &child_interval,
            Value::Undefined,
            &[child_interval_callback, Value::Num(0.0)],
        )
        .unwrap_or_else(|_| panic!("child interval registration succeeds"));
    assert!(matches!(interval_id, Value::Num(_)));
    assert!(runtime
        .engine()
        .ctx()
        .current_host_realm()
        .same_realm(&parent));
    let timeout_id = runtime
        .engine()
        .call_function(
            &child_timeout,
            Value::Undefined,
            &[child_timeout_callback, Value::Num(0.0)],
        )
        .unwrap_or_else(|_| panic!("child timeout registration succeeds"));
    assert!(matches!(timeout_id, Value::Num(_)));
    assert!(runtime
        .engine()
        .ctx()
        .current_host_realm()
        .same_realm(&parent));

    // Clearing an ID through a different realm does not remove the child's timer entry.
    let parent_clear_timeout = runtime
        .engine()
        .eval_value("clearTimeout")
        .unwrap_or_else(|_| panic!("parent clearTimeout parses"))
        .unwrap_or_else(|_| panic!("parent clearTimeout exists"));
    runtime
        .engine()
        .call_function(
            &parent_clear_timeout,
            Value::Undefined,
            std::slice::from_ref(&timeout_id),
        )
        .unwrap_or_else(|_| panic!("parent clearTimeout call succeeds"));
    assert!(runtime
        .engine()
        .ctx()
        .current_host_realm()
        .same_realm(&parent));

    let _parent_timer = runtime
        .engine()
        .eval_value(
            "globalThis.parentTimerRan = false; setTimeout(() => { parentTimerRan = true; }, 0)",
        )
        .unwrap_or_else(|_| panic!("parent timer parses"))
        .unwrap_or_else(|_| panic!("parent timer registers"));
    assert!(runtime
        .engine()
        .ctx()
        .current_host_realm()
        .same_realm(&parent));

    {
        let timers = runtime
            .engine()
            .ctx()
            .host_mut::<lumen_timers::Timers>()
            .expect("runtime timer registry");
        assert_eq!(
            timers.pending_for_realm(&parent),
            1,
            "parent owns its timer"
        );
        assert_eq!(
            timers.pending_for_realm(&child),
            2,
            "child owns both timers"
        );
    }

    // Navigation cancellation removes both child-owned timers while preserving the parent's
    // timer. Dropping the queued callbacks does not invalidate a separately retained function
    // from the old document.
    assert_eq!(runtime.cancel_timers_for_realm(&child), 2);
    // Node's wrapper clamps a zero-delay timer to 1 ms; run_to_completion waits for the
    // surviving parent timer, whereas run_until_idle may return before its deadline.
    runtime.run_to_completion();
    assert!(
        stderr.lines().is_empty(),
        "queued callbacks must not error: {:?}",
        stderr.lines()
    );
    let child_timeout_state = eval_host_value(
        &mut runtime,
        &child,
        "!timeoutRan && intervalCalls === 0 && retainedCalls === 0",
    );
    assert!(matches!(child_timeout_state, Value::Bool(true)));
    let parent_timer_ran = runtime
        .engine()
        .eval_value("parentTimerRan")
        .unwrap_or_else(|_| panic!("parent result parses"))
        .unwrap_or_else(|_| panic!("parent result runs"));
    assert!(matches!(parent_timer_ran, Value::Bool(true)));

    runtime
        .engine()
        .ctx()
        .dispose_host_realm(&child)
        .expect("retire the old child realm");
    runtime
        .engine()
        .call_function(&retained_callback, Value::Undefined, &[])
        .unwrap_or_else(|_| panic!("retained callback remains callable after retirement"));
    let retained_callback_state = eval_host_value(
        &mut runtime,
        &child,
        "retainedCalls === 1 && intervalCalls === 0",
    );
    assert!(matches!(retained_callback_state, Value::Bool(true)));
}

#[test]
fn browser_timer_callback_uses_owner_global_as_this_without_changing_its_lexical_realm() {
    let (mut runtime, _, stderr) = test_runtime();
    let child = runtime.engine().ctx().create_host_realm();
    runtime
        .install_browser_realm(&child)
        .expect("browser providers install in the child realm");

    let parent_setup = runtime
        .engine()
        .eval_value(
            "globalThis.parentTimerLexicalGlobal = globalThis; globalThis.makeTimerCallback = expectedOwner => function() { globalThis.timerCallbackLexicalRealm = globalThis === parentTimerLexicalGlobal; globalThis.timerCallbackOwnerThis = this === expectedOwner; }",
        )
        .expect("parent callback factory parses")
        .unwrap_or_else(|_| panic!("parent callback factory runs"));
    assert!(matches!(parent_setup, Value::Obj(_)));

    let child_global_this = runtime
        .engine()
        .ctx()
        .with_host_realm(&child, |ctx| ctx.global_this())
        .expect("child realm remains registered");
    let callback_factory = runtime
        .engine()
        .eval_value("makeTimerCallback")
        .expect("callback factory lookup parses")
        .unwrap_or_else(|_| panic!("callback factory exists"));
    let callback = runtime
        .engine()
        .call_function(
            &callback_factory,
            Value::Undefined,
            std::slice::from_ref(&child_global_this),
        )
        .unwrap_or_else(|_| panic!("parent-realm callback creation succeeds"));
    let child_set_timeout = eval_host_value(&mut runtime, &child, "setTimeout");
    runtime
        .engine()
        .call_function(
            &child_set_timeout,
            Value::Undefined,
            &[callback, Value::Num(0.0)],
        )
        .unwrap_or_else(|_| panic!("child-realm timer registration succeeds"));

    runtime.run_until_idle();
    assert!(
        stderr.lines().is_empty(),
        "timer callback errors: {:?}",
        stderr.lines()
    );
    let result = runtime
        .engine()
        .eval_value("timerCallbackOwnerThis && timerCallbackLexicalRealm")
        .expect("timer callback assertions parse")
        .unwrap_or_else(|_| panic!("timer callback assertions run"));
    assert!(matches!(result, Value::Bool(true)));
}

fn decode_unit_task(
    _ctx: &mut lumen_host::Ctx,
    payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    payload
        .downcast::<()>()
        .map(|_| Vec::new())
        .map_err(|_| Value::Undefined)
}

#[test]
fn browser_realm_cancellation_drops_async_settlements_but_keeps_other_realms() {
    let (mut runtime, _, stderr) = test_runtime();
    let parent = runtime.engine().ctx().current_host_realm();
    let child = runtime.engine().ctx().create_host_realm();

    runtime
        .engine()
        .eval_value("globalThis.parentTaskSettled = false")
        .expect("parent setup parses")
        .unwrap_or_else(|_| panic!("parent setup runs"));
    let parent_callback = runtime
        .engine()
        .eval_value("() => { parentTaskSettled = true; }")
        .expect("parent callback parses")
        .unwrap_or_else(|_| panic!("parent callback is created"));
    let child_setup = eval_host_value(
        &mut runtime,
        &child,
        "globalThis.childTaskSettled = false; true",
    );
    assert!(matches!(child_setup, Value::Bool(true)));
    let child_callback =
        eval_host_value(&mut runtime, &child, "() => { childTaskSettled = true; }");

    let parent_task = lumen_host::register_task(
        runtime.engine().ctx(),
        parent_callback,
        None,
        decode_unit_task,
    );
    let child_task = runtime
        .engine()
        .ctx()
        .with_host_realm(&child, |ctx| {
            lumen_host::register_task(ctx, child_callback, None, decode_unit_task)
        })
        .expect("child task registration returns to parent realm");
    assert!(runtime
        .engine()
        .ctx()
        .current_host_realm()
        .same_realm(&parent));
    {
        let tasks = runtime
            .engine()
            .ctx()
            .host_mut::<TaskRegistry>()
            .expect("runtime task registry");
        assert_eq!(tasks.pending_for_realm(&parent), 1);
        assert_eq!(tasks.pending_for_realm(&child), 1);
    }

    let completions = runtime.completion_sender();
    completions.send(child_task, Box::new(()));
    assert_eq!(runtime.cancel_tasks_for_realm(&child), 1);
    completions.send(parent_task, Box::new(()));
    runtime.run_until_idle();

    let parent_settled = runtime
        .engine()
        .eval_value("parentTaskSettled")
        .expect("parent result parses")
        .unwrap_or_else(|_| panic!("parent result runs"));
    assert!(matches!(parent_settled, Value::Bool(true)));
    let child_settled = eval_host_value(&mut runtime, &child, "childTaskSettled");
    assert!(matches!(child_settled, Value::Bool(false)));
    assert!(
        stderr.lines().is_empty(),
        "settlement errors: {:?}",
        stderr.lines()
    );
}

#[test]
fn repeated_discarded_realms_release_their_interval_entries() {
    let (mut runtime, _, _) = test_runtime();
    for _ in 0..24 {
        let realm = runtime.engine().ctx().create_host_realm();
        runtime
            .install_browser_realm(&realm)
            .expect("browser providers install in each fresh realm");
        let interval = eval_host_value(&mut runtime, &realm, "setInterval(() => {}, 0)");
        assert!(matches!(interval, Value::Num(_)));

        assert_eq!(runtime.cancel_timers_for_realm(&realm), 1);
        assert!(!runtime
            .engine()
            .ctx()
            .op_state()
            .get::<lumen_timers::Timers>()
            .is_some_and(lumen_timers::Timers::has_pending));
        runtime
            .engine()
            .ctx()
            .dispose_host_realm(&realm)
            .expect("discard the realm after its timers are cancelled");
    }
    assert!(!runtime
        .engine()
        .ctx()
        .op_state()
        .get::<lumen_timers::Timers>()
        .is_some_and(lumen_timers::Timers::has_pending));
}

#[test]
fn browser_timer_omitted_delays_and_cancel_ids_use_optional_idl_defaults() {
    let mut runtime = Runtime::new_browser();
    runtime.set_deadline(std::time::Duration::from_secs(1));
    let result = runtime.engine().eval_value(r#"
        if (setTimeout.length !== 1 || setInterval.length !== 1 ||
            clearTimeout.length !== 0 || clearInterval.length !== 0) {
            throw new Error('timer optional arguments must be reflected in function length');
        }
        clearTimeout(); clearInterval();
        globalThis.timerDefaultTrace = [];
        setTimeout(() => timerDefaultTrace.push('timeout'));
        const interval = setInterval(() => {
            timerDefaultTrace.push('interval'); clearInterval(interval);
        });
        timerDefaultTrace.push('sync');
        timerDefaultTrace.join(',');
    "#).expect("timer defaults source parses").unwrap_or_else(|_| panic!("timer defaults source runs"));
    assert!(matches!(result, Value::Str(value) if value.as_str() == "sync"));
    runtime.run_until_idle();
    let result = runtime.engine().eval_value("timerDefaultTrace.join(',')")
        .expect("timer result parses").unwrap_or_else(|_| panic!("timer result runs"));
    assert!(matches!(result, Value::Str(value) if value.as_str() == "sync,timeout,interval"));
}

/// The Phase-2 acceptance test: setTimeout + queueMicrotask + console.log complete in the
/// right order and the loop exits by itself.
#[test]
fn acceptance_timers_microtasks_console() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        console.log("start");
        setTimeout(() => console.log("timeout"), 10);
        setTimeout(() => console.log("timeout-late"), 20);
        queueMicrotask(() => console.log("micro"));
        Promise.resolve().then(() => console.log("promise"));
        console.log("end");
        "#,
    );
    assert_eq!(
        out.lines(),
        [
            "start",
            "end",
            "micro",
            "promise",
            "timeout",
            "timeout-late"
        ]
    );
}

#[test]
fn interval_fires_until_cleared_and_loop_exits() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        let n = 0;
        const id = setInterval(() => {
            n++;
            console.log("tick", n);
            if (n === 3) clearInterval(id);
        }, 5);
        "#,
    );
    // run_to_completion returned, so the cleared interval no longer holds the loop open.
    assert_eq!(out.lines(), ["tick 1", "tick 2", "tick 3"]);
}

#[test]
fn clear_timeout_cancels() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const id = setTimeout(() => console.log("no"), 5);
        clearTimeout(id);
        setTimeout(() => console.log("yes"), 10);
        "#,
    );
    assert_eq!(out.lines(), ["yes"]);
}

#[test]
fn next_tick_and_set_immediate_run_before_timers() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        setTimeout(() => console.log("timer"), 0);
        setImmediate(() => console.log("immediate"));
        process.nextTick((tag) => console.log("tick", tag), 42);
        "#,
    );
    assert_eq!(out.lines(), ["tick 42", "immediate", "timer"]);
}

#[test]
fn timer_args_pass_through_and_nested_timers_work() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        setTimeout((a, b) => {
            console.log("outer", a + b);
            setTimeout(() => console.log("inner"), 5);
        }, 5, 20, 22);
        "#,
    );
    assert_eq!(out.lines(), ["outer 42", "inner"]);
}

#[test]
fn uncaught_callback_error_is_fatal_unless_a_listener_owns_it() {
    // Node: an exception nobody catches ends the process with code 1 — later timers never run,
    // 'exit' fires (no 'beforeExit'), and the error is printed.
    let (mut rt, out, err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        process.on("beforeExit", () => console.log("beforeExit must not run"));
        process.on("exit", (code) => console.log("exit", code));
        setTimeout(() => { throw new TypeError("boom") }, 5);
        setTimeout(() => console.log("must not run"), 10);
        "#,
    );
    assert_eq!(rt.fatal_exit_code(), Some(1));
    assert_eq!(rt.finish_process(), 1);
    assert_eq!(out.lines(), ["exit 1"]);
    // Reported as Node reports it: the inspected error, then the version line.
    let err = err.lines();
    assert_eq!(
        err.first().map(String::as_str),
        Some("TypeError: boom"),
        "{err:?}"
    );
    assert_eq!(
        err.last().map(String::as_str),
        Some("Node.js v20.11.0"),
        "{err:?}"
    );

    // A 'uncaughtException' listener owns the error and the loop carries on; an unhandled
    // rejection with no 'unhandledRejection' listener is raised through the same hook.
    let (mut rt, out, err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        process.on("uncaughtException", (e, origin) => console.log("caught", e.message, origin));
        setTimeout(() => { throw new TypeError("boom") }, 5);
        setTimeout(() => console.log("still running"), 10);
        Promise.reject(new Error("rejected"));
        "#,
    );
    assert_eq!(rt.fatal_exit_code(), None);
    assert_eq!(rt.finish_process(), 0);
    assert_eq!(
        out.lines(),
        [
            "caught rejected unhandledRejection",
            "caught boom uncaughtException",
            "still running"
        ]
    );
    assert_eq!(err.lines(), Vec::<String>::new());
}

#[test]
fn browser_rejection_collection_is_opt_in_and_preserves_late_handled_reason() {
    let (mut rt, _, _) = test_runtime();
    rt.enable_browser_rejection_events();
    assert!(matches!(
        rt.engine()
            .eval(
                "globalThis.__browserRejected = Promise.reject('browser-reason')",
                false
            )
            .expect("rejection script parses"),
        Completion::Value(_)
    ));
    rt.run_until_idle();
    assert_eq!(rt.fatal_exit_code(), None);
    let first = rt.take_browser_rejection_events();
    assert_eq!(first.len(), 1);
    let promise = match first.into_iter().next().unwrap() {
        BrowserRejectionEvent::Unhandled {
            owner, promise, reason, ..
        } => {
            assert!(matches!(reason, Value::Str(ref value) if value.as_str() == "browser-reason"));
            // Collection queues notification; simulate its actual UA delivery now.
            let delivery = rt.browser_rejection_delivery();
            assert!(delivery.should_dispatch(rt.engine().ctx(), &owner, true, &promise));
            delivery.did_dispatch_unhandled(rt.engine().ctx(), &owner, &promise);
            promise
        }
        BrowserRejectionEvent::Handled { .. } => panic!("first browser notification was handled"),
    };

    assert!(matches!(
        rt.engine()
            .eval("__browserRejected.catch(() => {})", false)
            .expect("late handler parses"),
        Completion::Value(_)
    ));
    rt.run_until_idle();
    assert_eq!(rt.fatal_exit_code(), None);
    let handled = rt.take_browser_rejection_events();
    assert_eq!(handled.len(), 1);
    match handled.into_iter().next().unwrap() {
        BrowserRejectionEvent::Handled {
            promise: handled_promise,
            reason,
            ..
        } => {
            assert!(rt.engine().ctx().object_addr(&promise).is_some());
            assert_eq!(
                rt.engine().ctx().object_addr(&handled_promise),
                rt.engine().ctx().object_addr(&promise)
            );
            assert!(matches!(reason, Value::Str(ref value) if value.as_str() == "browser-reason"));
        }
        BrowserRejectionEvent::Unhandled { .. } => panic!("late notification was unhandled"),
    }

    eval_ok(&mut rt, "globalThis.__caughtBeforeDelivery = Promise.reject('early-reason')");
    rt.run_until_idle();
    let mut pending = rt.take_browser_rejection_events();
    assert_eq!(pending.len(), 1);
    let (owner, promise) = match pending.pop().unwrap() {
        BrowserRejectionEvent::Unhandled { owner, promise, reason, .. } => {
            assert!(matches!(reason, Value::Str(ref value) if value.as_str() == "early-reason"));
            (owner, promise)
        }
        BrowserRejectionEvent::Handled { .. } => panic!("early promise was already handled"),
    };
    eval_ok(&mut rt, "__caughtBeforeDelivery.catch(() => {})");
    rt.run_until_idle();
    assert!(!rt.browser_rejection_delivery().should_dispatch(
        rt.engine().ctx(), &owner, true, &promise));
    assert!(rt.take_browser_rejection_events().is_empty(),
        "catch before actual delivery must not emit rejectionhandled");
}

#[test]
fn browser_rejection_events_keep_realm_owner_and_cancel_retired_realm() {
    let (mut rt, _, _) = test_runtime();
    rt.enable_browser_rejection_events();
    let root = rt.engine().ctx().current_host_realm();
    let child = rt.engine().ctx().create_host_realm();

    assert!(rt
        .engine()
        .eval_value_in_host_realm(&root, "Promise.reject('root-reason')", false)
        .expect("root rejection evaluates")
        .is_ok());
    rt.run_until_idle();
    assert!(rt
        .engine()
        .eval_value_in_host_realm(
            &child,
            "globalThis.late=Promise.reject('child-reason')",
            false
        )
        .expect("child rejection evaluates")
        .is_ok());
    rt.run_until_idle();
    assert!(rt
        .engine()
        .eval_value_in_host_realm(&root, "Promise.reject('root-second')", false)
        .expect("second root rejection evaluates")
        .is_ok());
    rt.run_until_idle();

    let child_events = rt.take_browser_rejection_events_for_realm(&child);
    assert_eq!(child_events.len(), 1);
    match child_events.into_iter().next().unwrap() {
        BrowserRejectionEvent::Unhandled { owner, promise, reason, .. } => {
            assert!(owner.same_realm(&child));
            assert!(matches!(reason, Value::Str(ref value) if value.as_str() == "child-reason"));
            let delivery = rt.browser_rejection_delivery();
            assert!(delivery.should_dispatch(rt.engine().ctx(), &owner, true, &promise));
            delivery.did_dispatch_unhandled(rt.engine().ctx(), &owner, &promise);
        }
        BrowserRejectionEvent::Handled { .. } => panic!("child rejection was already handled"),
    }
    let root_events = rt.take_browser_rejection_events_for_realm(&root);
    assert_eq!(root_events.len(), 2);
    let root_reasons = root_events
        .into_iter()
        .map(|event| match event {
            BrowserRejectionEvent::Unhandled { owner, promise, reason, .. } => {
                assert!(owner.same_realm(&root));
                let delivery = rt.browser_rejection_delivery();
                assert!(delivery.should_dispatch(rt.engine().ctx(), &owner, true, &promise));
                delivery.did_dispatch_unhandled(rt.engine().ctx(), &owner, &promise);
                match reason {
                    Value::Str(value) => value.as_str().to_owned(),
                    _ => panic!("root rejection reason is a string"),
                }
            }
            BrowserRejectionEvent::Handled { .. } => panic!("root rejection was already handled"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        root_reasons,
        vec!["root-reason".to_owned(), "root-second".to_owned()]
    );

    assert!(rt
        .engine()
        .eval_value_in_host_realm(&child, "late.catch(() => {})", false)
        .expect("late handler evaluates")
        .is_ok());
    rt.run_until_idle();
    let handled = rt.take_browser_rejection_events_for_realm(&child);
    assert_eq!(handled.len(), 1);
    match handled.into_iter().next().unwrap() {
        BrowserRejectionEvent::Handled { owner, reason, .. } => {
            assert!(owner.same_realm(&child));
            assert!(matches!(reason, Value::Str(ref value) if value.as_str() == "child-reason"));
        }
        BrowserRejectionEvent::Unhandled { .. } => {
            panic!("late handler produced another unhandled event")
        }
    }

    assert!(rt
        .engine()
        .eval_value_in_host_realm(
            &child,
            "globalThis.retiring=Promise.reject('retiring')",
            false
        )
        .expect("retiring rejection evaluates")
        .is_ok());
    rt.run_until_idle();
    assert!(rt.cancel_browser_rejection_events_for_realm(&child) > 0);
    assert!(rt
        .engine()
        .eval_value_in_host_realm(
            &child,
            "retiring.catch(() => {}); Promise.reject('after-retirement')",
            false
        )
        .expect("post-retirement settlement evaluates")
        .is_ok());
    rt.run_until_idle();
    assert!(rt
        .take_browser_rejection_events_for_realm(&child)
        .is_empty());

    assert!(rt
        .engine()
        .eval_value_in_host_realm(&root, "Promise.reject('root-still-live')", false)
        .expect("root rejection after child retirement evaluates")
        .is_ok());
    rt.run_until_idle();
    let root_after = rt.take_browser_rejection_events_for_realm(&root);
    assert_eq!(root_after.len(), 1);
    assert!(
        matches!(root_after[0], BrowserRejectionEvent::Unhandled { ref owner, .. } if owner.same_realm(&root))
    );
}

#[test]
fn process_emitter_materializes_on_first_use_with_the_same_observable_state() {
    let (mut rt, out, err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        console.log(process.listenerCount("exit"), process.listenerCount("warning"), process.emit("exit"), process.emit("beforeExit"));
        let thrown = "";
        try { process.emit("error", new Error("unheard")); } catch (e) { thrown = e.message; }
        console.log(thrown);
        console.log(Object.keys(process).filter((k) => k.startsWith("_event") || k === "_maxListeners").join());
        console.log(process.on("SIGINT", () => {}) === process, process.listenerCount("SIGINT"));
        console.log(process.eventNames().map(String).join());
        const EE = require("events");
        console.log(require("node:events") === EE, process.emit === EE.prototype.emit, process._eventsCount);
        process.on("warning", (w) => console.log("user", w.message));
        const listeners = process.listeners("warning");
        console.log(listeners.length, listeners[0].name === "", listeners[0] !== listeners[1]);
        process.emitWarning("careful");
        "#,
    );
    rt.run_to_completion();
    assert_eq!(
        out.lines(),
        [
            "0 1 false false",
            "unheard",
            "_events,_eventsCount,_maxListeners",
            "true 1",
            "warning,SIGINT",
            "true true 2",
            "2 true true",
            "user careful",
        ]
    );
    assert_eq!(err.lines().len(), 2, "{:?}", err.lines());
    assert!(
        err.lines()[0].ends_with("Warning: careful"),
        "{:?}",
        err.lines()
    );

    let (mut rt, out, err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        process.emitWarning("alone");
        process.on("exit", (c) => console.log("exit", c));
        "#,
    );
    rt.run_to_completion();
    assert_eq!(rt.finish_process(), 0);
    assert_eq!(out.lines(), ["exit 0"]);
    assert!(
        err.lines()[0].ends_with("Warning: alone"),
        "{:?}",
        err.lines()
    );
}

#[test]
fn module_loader_path_math_matches_node_path() {
    let dir = TempDir::new("loader-path");
    std::fs::create_dir_all(std::path::Path::new(&dir.path("a/b"))).unwrap();
    std::fs::write(
        dir.path("a/b/c.js"),
        "module.exports = __filename + '|' + __dirname;",
    )
    .unwrap();
    std::fs::write(dir.path("a/d.json"), "{}").unwrap();
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        &format!(
            r#"
            const path = require("path");
            const Module = require("module");
            const root = require("fs").realpathSync({root:?});
            const from = path.join(root, "a", "x//y", "..", "..");
            const req = Module.createRequire(path.join(from, "entry.js"));
            console.log(req.resolve("./b//c.js") === path.join(root, "a", "b", "c.js"));
            console.log(req.resolve("./b/./../b/c") === path.join(root, "a", "b", "c.js"));
            console.log(req.resolve("../a/d") === path.join(root, "a", "d.json"));
            console.log(req(path.join(root, "a/b/c.js")) === [path.join(root, "a/b/c.js"), path.join(root, "a/b")].join("|"));
            const walk = (start) => {{
                const expected = [];
                for (let d = path.resolve(start); ; d = path.dirname(d)) {{
                    if (path.basename(d) !== "node_modules") expected.push(path.join(d, "node_modules"));
                    if (path.dirname(d) === d) break;
                }}
                return expected;
            }};
            for (const start of [from, path.join(root, "a", "node_modules", "p"), "rel//x/../y", "/", "/a/"]) {{
                console.log(JSON.stringify(Module._nodeModulePaths(start)) === JSON.stringify(walk(start)));
            }}
            console.log(Module.globalPaths.every((p) => typeof p === "string"));
            "#,
            root = dir.path("")
        ),
    );
    assert_eq!(out.lines(), vec!["true"; 10]);
}

#[test]
fn spawn_blocking_completion_settles_on_the_loop() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        "globalThis.onDone = (n) => console.log('got', n); 0",
    );
    let g = rt.engine().global_this();
    let cb = rt
        .engine()
        .ctx()
        .get_member(&g, "onDone")
        .map_err(|_| ())
        .expect("defined above");
    rt.spawn_blocking(
        || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            Box::new(21u64)
        },
        cb,
        |_ctx, payload| {
            let n = *payload.downcast::<u64>().expect("u64 payload");
            Ok(vec![Value::Num((n * 2) as f64)])
        },
    );
    // The in-flight task must hold the loop open until its completion arrives.
    rt.run_to_completion();
    assert_eq!(out.lines(), ["got 42"]);
}

// An inbox/task may be silent for arbitrarily long after loading. Collection must happen
// before its completion, rather than only when the next JS callback allocates objects.
struct IdleCollectionProbe {
    before: i64,
    reclaimed: Rc<RefCell<i64>>,
}

fn idle_loop_reclaims_setup_cycles(worker_loop: bool) {
    let (mut rt, out, _) = test_runtime();
    let source = r#"
        const retained = { offset: 2 };
        globalThis.onDone = n => console.log('retained', n + retained.offset);
        for (let n = 0; n < 12000; n++) {
            const garbage = {};
            garbage.self = garbage;
        }
        0;
    "#;
    match rt.engine().eval(source, false).expect("setup parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    let before = rt.engine().ctx().live_object_count();
    let global = rt.engine().global_this();
    let callback = rt
        .engine()
        .ctx()
        .get_member(&global, "onDone")
        .unwrap_or_else(|_| panic!("callback exists"));
    let reclaimed = Rc::new(RefCell::new(0));
    rt.engine().ctx().op_state().put(IdleCollectionProbe {
        before,
        reclaimed: reclaimed.clone(),
    });
    rt.spawn_blocking(
        || {
            std::thread::sleep(Duration::from_millis(2500));
            Box::new(())
        },
        callback,
        |ctx, _| {
            let live = ctx.live_object_count();
            let probe = ctx
                .op_state()
                .get::<IdleCollectionProbe>()
                .expect("probe installed");
            *probe.reclaimed.borrow_mut() = probe.before - live;
            Ok(vec![Value::Num(40.)])
        },
    );
    if worker_loop {
        rt.run_worker_loop(&std::sync::atomic::AtomicBool::new(false));
    } else {
        rt.run_to_completion();
    }
    assert!(
        *reclaimed.borrow() >= 12000,
        "setup cycles remained rooted until completion"
    );
    assert_eq!(out.lines(), ["retained 42"]);
}

#[test]
fn idle_main_loop_reclaims_before_silent_task_completion() {
    idle_loop_reclaims_setup_cycles(false);
}

#[test]
fn idle_worker_loop_reclaims_before_silent_task_completion() {
    idle_loop_reclaims_setup_cycles(true);
}

#[test]
fn console_streams_and_renders_common_values() {
    let (mut rt, out, err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        console.log("s", 1.5, true, null, undefined, Symbol("sym"), [1, 2], { a: 1 });
        console.warn("careful");
        console.error("bad");
        "#,
    );
    assert_eq!(
        out.lines(),
        ["s 1.5 true null undefined Symbol(sym) [ 1, 2 ] { a: 1 }"]
    );
    assert_eq!(err.lines(), ["careful", "bad"]);
}

#[test]
fn process_basics() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        console.log(typeof process.cwd(), process.cwd().length > 0);
        console.log(Array.isArray(process.argv), typeof process.argv[0]);
        console.log(typeof process.env, typeof process.platform);
        console.log(typeof process.setuid, typeof process.setgid,
          typeof process.getgroups === "function" && Array.isArray(process.getgroups()));
        "#,
    );
    assert_eq!(
        out.lines(),
        [
            "string true",
            "true string",
            "object string",
            if cfg!(windows) {
                "undefined undefined false"
            } else {
                "function function true"
            }
        ]
    );
}

#[test]
fn process_reports_native_cpu_and_memory_metrics() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const memory = process.memoryUsage();
        const cpu = process.cpuUsage();
        const resources = process.resourceUsage();
        console.log(memory.rss > 0, process.memoryUsage.rss() > 0);
        console.log(cpu.user >= 0, cpu.system >= 0, cpu.user + cpu.system > 0);
        console.log(resources.maxRSS > 0, resources.userCPUTime >= 0, resources.minorPageFault >= 0);
        console.log(process.availableMemory() > 0, process.constrainedMemory() === undefined || process.constrainedMemory() > 0);
        "#,
    );
    assert_eq!(
        out.lines(),
        ["true true", "true true true", "true true true", "true true"]
    );
}

#[test]
fn process_diagnostic_reports_and_finalization_are_functional() {
    let dir = TempDir::new("process-report");
    let report_path = dir.path("report.json");
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        &format!(
            r#"
            const fs = require("node:fs");
            const generated = process.report.getReport(new Error("boom"));
            console.log(generated.header.processId === process.pid, generated.javascriptStack.message.includes("boom"));
            console.log(process.report.writeReport({report_path:?}).endsWith("report.json"));
            const saved = JSON.parse(fs.readFileSync({report_path:?}, "utf8"));
            console.log(saved.header.processId === process.pid, saved.resourceUsage.maxRSS > 0);

            const ref = {{ tag: 1 }};
            process.finalization.registerBeforeExit(ref, (value, event) => console.log(value.tag, event));
            process.emit("beforeExit");
            console.log(process.finalization.unregister(ref));
            "#,
        ),
    );
    assert_eq!(
        out.lines(),
        ["true true", "true", "true true", "1 beforeExit", "true"]
    );
}

#[test]
fn process_execve_validates_and_reports_os_errors() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        console.log(typeof process.execve);
        try { process.execve(1, [], {}); } catch (error) { console.log(error.name); }
        try { process.execve("/definitely/not/a/lumen/executable", ["missing"], {}); }
        catch (error) {
          console.log(error.message.startsWith(process.platform === "win32"
            ? "process.execve is not supported" : "execve failed:"));
        }
        "#,
    );
    assert_eq!(out.lines(), ["function", "TypeError", "true"]);
}

#[test]
fn node_crypto_argon2_sync_and_async_match_rfc9106() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const { argon2, argon2Sync } = require("node:crypto");
        const parameters = {
          message: Buffer.alloc(32, 1), nonce: Buffer.alloc(16, 2),
          parallelism: 4, tagLength: 32, memory: 32, passes: 3,
          secret: Buffer.alloc(8, 3), associatedData: Buffer.alloc(12, 4),
        };
        const expected = "0d640df58d78766c08c037a34a8b53c9d01ef0452d75b65eb52520e96b01e659";
        console.log(Object.keys(require("node:crypto")).length, argon2Sync("argon2id", parameters).toString("hex") === expected);
        argon2("argon2id", parameters, (error, key) => console.log(error === null, key.toString("hex") === expected));
        "#,
    );
    assert_eq!(out.lines(), ["67 true", "true true"]);
}

#[test]
fn node_util_types_detects_proxies_and_key_objects() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const types = require("node:util/types");
        const { createSecretKey } = require("node:crypto");
        console.log(types.isProxy(new Proxy({}, {})), types.isProxy({}));
        console.log(types.isKeyObject(createSecretKey("secret")), types.isKeyObject(Buffer.from("secret")));
        "#,
    );
    assert_eq!(out.lines(), ["true false", "true false"]);
}

#[test]
fn bun_jsc_uses_live_heap_gc_and_microtask_instrumentation() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const jsc = require("bun:jsc");
        const stats = jsc.heapStats(), memory = jsc.memoryUsage();
        console.log(stats.objectCount > 0, stats.heapSize > 0, memory.current > 0);
        console.log(jsc.estimateShallowMemoryUsageOf("hello") === 10, jsc.estimateShallowMemoryUsageOf(new Uint8Array(20)) >= 20);
        const fn = () => 42;
        console.log(jsc.noFTL(fn) === fn, jsc.noInline(fn)());
        let drained = false;
        Promise.resolve().then(() => { drained = true; });
        jsc.drainMicrotasks();
        console.log(drained);
        const profile = jsc.profile((a, b) => a + b, 100, 2, 3);
        console.log(typeof profile.functions, Array.isArray(profile.stackTraces));
        let cycle = {}; cycle.self = cycle; cycle = null;
        console.log(jsc.fullGC() >= 0, Array.isArray(jsc.getProtectedObjects()));
        const v8Snapshot = Bun.generateHeapSnapshot("v8");
        const parsed = JSON.parse(v8Snapshot);
        const jscSnapshot = Bun.generateHeapSnapshot();
        console.log(parsed.snapshot.node_count === 1, parsed.snapshot.lumen_object_count > 0, jscSnapshot.objectCount > 0);
        console.log(Bun.gc() >= 0, Bun.shrink() >= 0);
        "#,
    );
    assert_eq!(
        out.lines(),
        [
            "true true true",
            "true true",
            "true 42",
            "true",
            "string true",
            "true true",
            "true true true",
            "true true",
        ]
    );
}

#[test]
fn bun_transpiler_transforms_typescript_jsx_and_scans_modules() {
    std::thread::Builder::new()
        .stack_size(lumen::THREAD_STACK_SIZE)
        .spawn(bun_transpiler_transforms_typescript_jsx_and_scans_modules_body)
        .unwrap()
        .join()
        .unwrap();
}

fn bun_transpiler_transforms_typescript_jsx_and_scans_modules_body() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const { Transpiler } = require("bun");
        const ts = new Transpiler({ loader: "ts", define: { DEBUG: "false" } });
        const js = ts.transformSync("const answer: number = 42; if (DEBUG) throw 1;");
        console.log(js.includes(": number"), js.includes("if (false)"), Function(`${js}; return answer`)());
        const jsx = new Transpiler({ loader: "jsx" }).transformSync("const el = <div id=\"x\">hello</div>;");
        console.log(jsx.includes('React.createElement("div"'), jsx.includes('"hello"'));
        const tsx = new Transpiler({ loader: "tsx" }).transformSync("const id=<T,>(x:T):T=>x; const n:number=3; const el=<a value={id<number>(n)}>{n+1}</a>;");
        const el = Function("React", `${tsx}; return el;`)({ createElement(type, props, ...children) { return { type, props, children }; } });
        console.log(el.type, el.props.value, el.children[0]);
        const scan = ts.scan('import value from "pkg"; export { value as result }; const lazy = import("later");');
        console.log(scan.exports.join(","), scan.imports.map(item => `${item.kind}:${item.path}`).join(","));
        ts.transform("const value: string = 'ok'").then(code => console.log(!code.includes(": string")));
        "#,
    );
    assert_eq!(
        out.lines(),
        [
            "false true 42",
            "true true",
            "a 3 4",
            "result import-statement:pkg,dynamic-import:later",
            "true",
        ]
    );
}

#[test]
fn jsx_tsx_entry_dependencies_commonjs_and_tsconfig() {
    let dir = TempDir::new("native-jsx");
    std::fs::create_dir_all(dir.0.join("runtime")).unwrap();
    std::fs::write(
        dir.0.join("runtime/jsx-runtime.mjs"),
        "export function jsx(type,props,key){return {type,props,key};} export const jsxs=jsx;",
    )
    .unwrap();
    std::fs::write(
        dir.0.join("tsconfig.json"),
        r#"{"compilerOptions":{"jsx":"react-jsx","jsxImportSource":"./runtime"}}"#,
    )
    .unwrap();
    std::fs::write(dir.0.join("label.jsx"), "export const label=<b>&copy;</b>;").unwrap();
    std::fs::write(dir.0.join("app.tsx"), "import {label} from './label.jsx'; const id=<T,>(x:T):T=>x; const n:number=3; console.log(JSON.stringify(<section n={id<number>(n)}>{label}</section>));").unwrap();
    std::fs::write(dir.0.join("common.tsx"), "/** @jsxRuntime classic @jsx h */ const id=<T,>(x:T):T=>x; function h(type,props,...children){return {type,props,children};} module.exports=<a n={id<number>(5)}>{6}</a>;").unwrap();
    let (mut rt, out, _) = test_runtime();
    rt.run_module(&dir.path("app.tsx")).unwrap();
    eval_ok(
        &mut rt,
        &format!(
            "console.log(JSON.stringify(require({:?})));",
            dir.path("common.tsx")
        ),
    );
    assert_eq!(
        out.lines(),
        [
            r#"{"type":"section","props":{"n":3,"children":{"type":"b","props":{"children":"©"}}}}"#,
            r#"{"type":"a","props":{"n":5},"children":[6]}"#,
        ]
    );
}

#[test]
fn bun_mmap_tracks_file_reads_and_shared_writes() {
    let dir = TempDir::new("bun-mmap");
    let path = dir.path("mapped.bin");
    std::fs::write(&path, [1_u8, 2, 3, 4]).unwrap();
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        &format!(
            r#"
            const fs = require("fs");
            const mapped = Bun.mmap({path:?});
            console.log(mapped instanceof Uint8Array, mapped.length, mapped[1]);
            fs.writeFileSync({path:?}, Uint8Array.of(9, 8, 7, 6));
            console.log(mapped[1]);
            mapped[2] = 42;
            mapped.close();
            console.log(fs.readFileSync({path:?})[2]);
            "#,
        ),
    );
    assert_eq!(out.lines(), ["true 4 2", "8", "42"]);
}

#[test]
fn bun_sqlite_file_control_reaches_native_sqlite() {
    let dir = TempDir::new("sqlite-file-control");
    let database = dir.path("control.sqlite");
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        &format!(
            r#"
            const {{ Database, constants }} = require("bun:sqlite");
            const db = new Database({database:?}, {{ create: true }});
            db.run("PRAGMA journal_mode=WAL");
            console.log(db.fileControl(constants.SQLITE_FCNTL_PERSIST_WAL, 0) === undefined);
            try {{ db.fileControl(constants.SQLITE_FCNTL_PERSIST_WAL, {{}}); }}
            catch (error) {{ console.log(error.name); }}
            db.close();
            "#,
        ),
    );
    assert_eq!(out.lines(), ["true", "TypeError"]);
}

#[test]
fn async_await_settles_before_loop_exit() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
        (async () => {
            console.log("before");
            await delay(10);
            console.log("after");
        })();
        "#,
    );
    assert_eq!(out.lines(), ["before", "after"]);
}

// ---- fs (node:fs as the runtime assembles it) ----

/// A unique temp dir per test, cleaned up on drop.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static NEXT_TEMP_DIR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT_TEMP_DIR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("lumen-fs-test-{tag}-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        TempDir(dir)
    }
    fn path(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().into_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The Phase-4 acceptance test: sync and async read/write both round-trip a file from JS.
#[test]
fn fs_sync_and_async_roundtrip() {
    let dir = TempDir::new("roundtrip");
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        &format!(
            r#"
            const fs = require("node:fs");
            const sync = {sync:?}, asyncPath = {async_:?};
            fs.writeFileSync(sync, "hello sync");
            console.log("sync:", fs.readFileSync(sync, "utf8"));
            (async () => {{
                await fs.promises.writeFile(asyncPath, "hello async");
                console.log("async:", await fs.promises.readFile(asyncPath, "utf8"));
            }})();
            "#,
            sync = dir.path("s.txt"),
            async_ = dir.path("a.txt"),
        ),
    );
    assert_eq!(out.lines(), ["sync: hello sync", "async: hello async"]);
}

#[test]
fn fs_exists_readdir_unlink_append() {
    let dir = TempDir::new("meta");
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        &format!(
            r#"
            const fs = require("node:fs");
            const d = {dir:?}, f = {file:?};
            console.log(fs.existsSync(f));
            fs.writeFileSync(f, "a");
            fs.appendFileSync(f, "b");
            console.log(fs.existsSync(f), fs.readFileSync(f, "utf8"));
            console.log(fs.readdirSync(d).join(","));
            fs.unlinkSync(f);
            console.log(fs.existsSync(f));
            "#,
            dir = dir.path(""),
            file = dir.path("x.txt"),
        ),
    );
    assert_eq!(out.lines(), ["false", "true ab", "x.txt", "false"]);
}

#[test]
fn fs_descriptors_read_write_and_close() {
    let dir = TempDir::new("handles");
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        &format!(
            r#"
            const fs = require("node:fs");
            const f = {file:?};
            const w = fs.openSync(f, "w");
            fs.writeSync(w, "line one\n");
            fs.writeSync(w, "line two\n");
            fs.closeSync(w);
            const r = fs.openSync(f, "r");
            const buf = Buffer.alloc(64);
            const n = fs.readSync(r, buf);
            console.log(JSON.stringify(buf.toString("utf8", 0, n)));
            fs.closeSync(r);
            try {{ fs.readSync(r, buf) }} catch (e) {{ console.log("stale:", e.code) }}
            "#,
            file = dir.path("h.txt"),
        ),
    );
    assert_eq!(out.lines(), [r#""line one\nline two\n""#, "stale: EBADF"]);
}

#[test]
fn fs_promise_rejection_is_catchable() {
    let dir = TempDir::new("reject");
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        &format!(
            r#"
            const fs = require("node:fs");
            (async () => {{
                try {{
                    await fs.promises.readFile({missing:?});
                    console.log("unexpected success");
                }} catch (e) {{
                    console.log("caught:", e.code === "ENOENT", e.message.includes("nope.txt"));
                }}
            }})();
            "#,
            missing = dir.path("nope.txt"),
        ),
    );
    assert_eq!(out.lines(), ["caught: true true"]);
}

#[test]
fn fs_sync_error_throws_catchable_error() {
    let dir = TempDir::new("syncerr");
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        &format!(
            "try {{ require('node:fs').readFileSync({missing:?}) }} catch (e) {{ console.log('caught', e instanceof Error) }}",
            missing = dir.path("gone.txt"),
        ),
    );
    assert_eq!(out.lines(), ["caught true"]);
}

// ---- lumen-web (WinterTC minimum common API; the runtime assembles it) ----

#[test]
fn web_base64_edge_cases() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const t = (f) => { try { return JSON.stringify(f()); } catch (e) { return e.name; } };
        const r = [];
        for (const s of ["", "a", "ab", "abc", "h\xe9llo\xff\0", "\u0100", "\ud800", undefined, 12]) r.push(t(() => btoa(s)));
        for (const s of ["", "YQ==", "YQ", "YWI=", " Y W J j ", "YQ=", "YQ===", "Y", "=", "\u00e9", "Y-_=", "\u00ff"]) r.push(t(() => atob(s)));
        r.push(atob(btoa("x".repeat(100000))).length);
        console.log(r.join("|"));
        "#,
    );
    assert_eq!(
        out.lines(),
        [
            r#"""|"YQ=="|"YWI="|"YWJj"|"aOlsbG//AA=="|InvalidCharacterError|InvalidCharacterError|"dW5kZWZpbmVk"|"MTI="|""|"a"|"a"|"ab"|"abc"|InvalidCharacterError|InvalidCharacterError|InvalidCharacterError|InvalidCharacterError|InvalidCharacterError|InvalidCharacterError|InvalidCharacterError|100000"#
        ]
    );
}

#[test]
fn web_encoding_and_base64() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const enc = new TextEncoder().encode("hi \u{1F600}");
        console.log(enc.length, new TextDecoder().decode(enc));
        console.log(btoa("Man"), atob("TWFu"));
        try { new TextDecoder().decode(new Uint8Array([0xff]), undefined) } catch { console.log("nonfatal-ok") }
        console.log(new TextDecoder("utf-8", { fatal: true }).constructor.name);
        "#,
    );
    assert_eq!(out.lines(), ["7 hi \u{1F600}", "TWFu Man", "TextDecoder"]);
}

#[test]
fn web_legacy_encoding_streams_reset_and_preserve_real_buffer_views() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(&mut rt, r#"
        const assert = (ok) => { if (!ok) throw new Error('decoder contract'); };
        const decoder = new TextDecoder('  Shift_JIS\n', {fatal:true});
        assert(decoder.encoding === 'shift_jis' && decoder.fatal && !decoder.ignoreBOM);
        assert(decoder.decode(Uint8Array.of(0x82), {stream:true}) === '');
        assert(decoder.decode(Uint8Array.of(0xa0,0x82), {stream:true}) === 'あ');
        assert(decoder.decode(Uint8Array.of(0xa2)) === 'い');
        let fatal = false;
        try { decoder.decode(Uint8Array.of(0x82)); } catch (e) { fatal = e instanceof TypeError; }
        assert(fatal && decoder.decode(Uint8Array.of(65)) === 'A');
        const source = Uint8Array.of(0,0xc4,0xe3,0);
        assert(new TextDecoder('gb18030').decode(new DataView(source.buffer,1,2)) === '你');
        assert(new TextDecoder('big5').decode(Uint8Array.of(0xa7,0x41)) === '你');
        assert(new TextDecoder('euc-kr').decode(Uint8Array.of(0xb0,0xa1)) === '가');
        assert(new TextDecoder('windows-1251').decode(Uint8Array.of(0xcf,0xf0)) === 'Пр');
        const iso = new TextDecoder('iso-2022-jp');
        assert(iso.decode(Uint8Array.of(27,36),{stream:true}) === '');
        assert(iso.decode(Uint8Array.of(66,36,34),{stream:true}) === 'あ');
        assert(iso.decode(Uint8Array.of(27,40,66,33)) === '!');
        assert(iso.decode(Uint8Array.of(65)) === 'A');
        console.log('legacy-streams-ok');
    "#);
    assert_eq!(out.lines(), ["legacy-streams-ok"]);
}

#[test]
fn web_encoding_encoder_and_decoder_expose_real_webidl_brands_and_arities() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(&mut rt, r#"
        const assert = (ok) => { if (!ok) throw new Error('encoding IDL contract'); };
        const fails = (fn) => { try {fn();return false} catch(e){return e instanceof TypeError} };
        const encoder = new TextEncoder();
        assert(TextEncoder.length === 0 && TextDecoder.length === 0);
        assert(TextDecoder.prototype.decode.length === 0);
        assert(TextEncoder.prototype.encode.length === 0 && TextEncoder.prototype.encodeInto.length === 2);
        assert(Object.prototype.toString.call(encoder) === '[object TextEncoder]');
        assert(Object.getOwnPropertyDescriptor(TextEncoder.prototype,'encoding').enumerable);
        assert(Object.getOwnPropertyDescriptor(TextEncoder.prototype,'encode').enumerable);
        assert(fails(()=>TextEncoder.prototype.encode.call({},'a')));
        assert(fails(()=>Object.getOwnPropertyDescriptor(TextEncoder.prototype,'encoding').get.call({})));
        assert(fails(()=>encoder.encode(Symbol('input'))));
        assert(fails(()=>new TextDecoder(Symbol('label'))));
        const empty = encoder.encodeInto('abc', new Uint8Array(0));
        assert(empty.read === 0 && empty.written === 0);
        const detached = new Uint8Array(4);
        structuredClone(detached.buffer,{transfer:[detached.buffer]});
        const result = encoder.encodeInto('',detached);
        assert(result.read === 0 && result.written === 0);
        const fake = Object.create(Uint8Array.prototype);
        assert(fails(()=>encoder.encodeInto('a',fake)));
        console.log('encoding-idl-ok');
    "#);
    assert_eq!(out.lines(), ["encoding-idl-ok"]);
}

#[test]
fn web_encoding_bom_options_order_and_brand_share_native_stream_state() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(&mut rt, r#"
        const assert = (ok) => { if (!ok) throw new Error('decoder contract'); };
        const seen = [];
        const decoder = new TextDecoder('utf-8', {
            get fatal(){seen.push('fatal');return false},
            get ignoreBOM(){seen.push('ignoreBOM');return false}
        });
        assert(seen.join(',') === 'fatal,ignoreBOM');
        assert(decoder.decode(Uint8Array.of(0xef),{stream:true}) === '');
        assert(decoder.decode(Uint8Array.of(0xbb,0xbf,65)) === 'A');
        assert(decoder.decode(Uint8Array.of(0xef,0xbb,0xbf,66)) === 'B');
        assert(new TextDecoder('utf-8',{ignoreBOM:true}).decode(Uint8Array.of(0xef,0xbb,0xbf)) === '\ufeff');
        const le = new TextDecoder('utf-16le');
        assert(le.decode(Uint8Array.of(0xff),{stream:true}) === '');
        assert(le.decode(Uint8Array.of(0xfe,65,0)) === 'A');
        assert(new TextDecoder('windows-1252',{fatal:true}).decode(Uint8Array.of(0x80,0x81)) === '\u20ac\u0081');
        let invalid = false;
        try { new TextDecoder('replacement',{get fatal(){throw new Error('must not read')}}); }
        catch(e){invalid = e instanceof RangeError;}
        assert(invalid);
        let branded = false;
        try { TextDecoder.prototype.decode.call({},new Uint8Array()); }
        catch(e){branded=e instanceof TypeError;}
        assert(branded);
        assert(new TextDecoder('utf-8').decode(Uint8Array.of(0xff)) === '\ufffd');
        const input = Uint8Array.of(90,65,66,90).subarray(1,3);
        for (const name of ['buffer','byteOffset','byteLength']) {
            Object.defineProperty(input,name,{get(){throw new Error('must use intrinsic view range')}});
        }
        assert(decoder.decode(input) === 'AB');
        const data = new DataView(Uint8Array.of(90,67,90).buffer,1,1);
        for (const name of ['buffer','byteOffset','byteLength']) {
            Object.defineProperty(data,name,{get(){throw new Error('must use intrinsic DataView range')}});
        }
        assert(decoder.decode(data) === 'C');
        const detaching = Uint8Array.of(65);
        assert(decoder.decode(detaching,{get stream(){
            structuredClone(detaching.buffer,{transfer:[detaching.buffer]});return false;
        }}) === '');
        console.log('bom-and-brand-ok');
    "#);
    assert_eq!(out.lines(), ["bom-and-brand-ok"]);
}

#[test]
fn web_url_and_search_params() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const u = new URL("http://user@ex.com/a/b?x=1&y=2#f");
        console.log(u.protocol, u.hostname, u.pathname, u.hash);
        console.log(u.searchParams.get("x"), u.searchParams.getAll("y").length);
        u.searchParams.set("x", "9");
        console.log(u.search);
        console.log(new URL("../c", "http://ex.com/a/b/").href);
        console.log(URL.canParse("nope"), URL.canParse("http://ok.com"));
        const sp = new URLSearchParams("a=1&a=2&b=3");
        console.log([...sp.keys()].join(","), sp.toString());
        "#,
    );
    assert_eq!(
        out.lines(),
        [
            "http: ex.com /a/b #f",
            "1 1",
            "?x=9&y=2",
            "http://ex.com/a/c",
            "false true",
            "a,a,b a=1&a=2&b=3",
        ]
    );
}

/// The native parser answers with Node's URLContext record, not an object of parts; every getter
/// has to read that record. When it did not, `protocol` was "undefined:" for every URL and the
/// browser driver refused http, https and about:blank alike.
#[test]
fn web_url_reads_the_native_record() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        for (const s of ["https://example.com", "http://localhost", "about:blank", "HTTP://EX.com"]) {
          const u = new URL(s);
          console.log(u.protocol, JSON.stringify(u.host), u.pathname, u.href);
        }
        const u = new URL("https://us:pw@ex.com:8443/p/q?a=1#h");
        console.log(u.username, u.password, u.hostname, u.port, u.host, u.origin);
        console.log(u.pathname, u.search, u.hash);
        console.log(new URL("http://ex.com:80/").port === "", new URL("file:///c/d").origin);
        console.log(new URL("blob:https://ex.com/id").origin, new URL("data:,x").origin);
        try { new URL("not a url"); } catch (e) { console.log(e.name, e.code); }
        u.hostname = "other.org"; u.port = "9"; u.pathname = "/z"; u.search = "b=2"; u.hash = "k";
        console.log(u.href);
        u.protocol = "http"; u.search = ""; u.hash = "";
        console.log(u.href);
        const w = new URL("http://ex.com/?x=1");
        w.searchParams.append("y", "2");
        console.log(w.href, w.search);
        w.href = "https://new.example/";
        console.log(w.protocol, w.searchParams.toString() === "");
        "#,
    );
    assert_eq!(
        out.lines(),
        [
            "https: \"example.com\" / https://example.com/",
            "http: \"localhost\" / http://localhost/",
            "about: \"\" blank about:blank",
            "http: \"ex.com\" / http://ex.com/",
            "us pw ex.com 8443 ex.com:8443 https://ex.com:8443",
            "/p/q ?a=1 #h",
            "true null",
            "https://ex.com null",
            "TypeError ERR_INVALID_URL",
            "https://us:pw@other.org:9/z?b=2#k",
            "http://us:pw@other.org:9/z",
            "http://ex.com/?x=1&y=2 ?x=1&y=2",
            "https: true",
        ]
    );
}

#[test]
fn web_response_status_defaults() {
    // An explicit `undefined` status/statusText counts as absent (WebIDL) and takes the default,
    // rather than coercing to `Number(undefined)` → NaN / `String(undefined)` → "undefined". This
    // is the path Hono's `c.json()` hits (its internal status is left undefined).
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        console.log(new Response("x").status);
        console.log(new Response("x", { status: undefined }).status);
        console.log(new Response("x", { status: 201 }).status);
        const r = new Response("x", { status: undefined, statusText: undefined });
        console.log(JSON.stringify(r.statusText), r.ok);
        "#,
    );
    assert_eq!(out.lines(), ["200", "200", "201", "\"\" true"]);
}

#[test]
fn web_readable_stream_body() {
    // `Response`/`Request` expose their buffered body as a `ReadableStream` via `.body`, and the
    // constructors accept a stream body — so `new Response(res.body, res)` (Hono's `c.header()`
    // rebuild) round-trips the payload instead of dropping it. Also covers reader reads, async
    // iteration, a user-authored stream as a body, and `null` for an empty body.
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        (async () => {
            const a = new Response('{"x":1}', { headers: { "content-type": "application/json" } });
            const rebuilt = new Response(a.body, a);            // c.header() rebuild pattern
            console.log(await rebuilt.text());
            console.log(new Response("hi").body instanceof ReadableStream, new Response(null).body);
            const rd = new Response("hello").body.getReader();
            const c = await rd.read();
            console.log(new TextDecoder().decode(c.value), (await rd.read()).done);
            let acc = "";
            for await (const ch of new Response("abc").body) acc += new TextDecoder().decode(ch);
            console.log(acc);
            const us = new ReadableStream({ start(ctrl) { ctrl.enqueue(new TextEncoder().encode("strm")); ctrl.close(); } });
            console.log(await new Response(us).text());
        })();
        "#,
    );
    assert_eq!(
        out.lines(),
        ["{\"x\":1}", "true null", "hello true", "abc", "strm"]
    );
}

#[test]
fn web_events_and_abort() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const et = new EventTarget();
        let count = 0;
        const cb = (e) => { count += e.detail; };
        et.addEventListener("ping", cb);
        et.dispatchEvent(new CustomEvent("ping", { detail: 5 }));
        et.dispatchEvent(new CustomEvent("ping", { detail: 5 }));
        et.removeEventListener("ping", cb);
        et.dispatchEvent(new CustomEvent("ping", { detail: 5 }));
        console.log("count", count);

        let onceCount = 0;
        et.addEventListener("x", () => onceCount++, { once: true });
        et.dispatchEvent(new Event("x"));
        et.dispatchEvent(new Event("x"));
        console.log("once", onceCount);

        const ac = new AbortController();
        let aborted = false;
        ac.signal.addEventListener("abort", () => { aborted = true; });
        console.log("pre", ac.signal.aborted);
        ac.abort();
        console.log("post", ac.signal.aborted, aborted, ac.signal.reason.name);
        console.log("static", AbortSignal.abort().aborted);
        "#,
    );
    assert_eq!(
        out.lines(),
        [
            "count 10",
            "once 1",
            "pre false",
            "post true true AbortError",
            "static true"
        ]
    );
}

#[test]
fn web_structured_clone() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const orig = { a: [1, 2, { deep: true }], m: new Map([["k", 1]]), d: new Date(1000) };
        orig.self = orig;
        const c = structuredClone(orig);
        console.log(c !== orig, c.a[2].deep, c.m.get("k"), c.d.getTime());
        console.log(c.self === c, c.a !== orig.a);
        try { structuredClone(() => {}); } catch (e) { console.log("fn", e.name); }
        "#,
    );
    assert_eq!(
        out.lines(),
        ["true true 1 1000", "true true", "fn DataCloneError"]
    );
}

#[test]
fn web_crypto() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const a = new Uint8Array(16), b = new Uint8Array(16);
        crypto.getRandomValues(a); crypto.getRandomValues(b);
        // Astronomically unlikely to be equal: a real randomness source.
        console.log(a.some((v, i) => v !== b[i]));
        console.log(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(crypto.randomUUID()));
        try { crypto.getRandomValues(new Uint8Array(70000)); } catch (e) { console.log("quota", e.name); }
        crypto.subtle.digest("SHA-256", new TextEncoder().encode("abc")).then((d) => {
            const hex = [...new Uint8Array(d)].map((x) => x.toString(16).padStart(2, "0")).join("");
            console.log(hex);
        });
        "#,
    );
    assert_eq!(
        out.lines(),
        [
            "true",
            "true",
            "quota QuotaExceededError",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        ]
    );
}

#[test]
fn web_fetch_roundtrip_over_local_http() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    // One-shot server on a background thread: read the request, reply with a JSON body.
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf);
        let body = br#"{"ok":true,"n":42}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nx-test: yes\r\ncontent-length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(resp.as_bytes());
        let _ = stream.write_all(body);
    });

    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        &format!(
            r#"
            (async () => {{
                const r = await fetch("http://{addr}/data");
                console.log(r.status, r.ok, r.headers.get("x-test"));
                const j = await r.json();
                console.log(j.ok, j.n);
            }})();
            "#,
        ),
    );
    server.join().ok();
    assert_eq!(out.lines(), ["200 true yes", "true 42"]);
}

// ---- WinterTC Minimum Common API conformance ----
//
// The tracked score for the WinterTC "Minimum Common API" global surface. `SUPPORTED` are the
// interfaces implemented today; `NOT_YET` are the remaining ones. The test asserts every SUPPORTED
// global is present AND every NOT_YET global is absent — so implementing an interface fails the
// test until its name is moved across, keeping the score honest. Total = the full spec surface.
const WINTERTC_SUPPORTED: &[&str] = &[
    "globalThis",
    "queueMicrotask",
    "structuredClone",
    "atob",
    "btoa",
    "fetch",
    "console",
    "setTimeout",
    "clearTimeout",
    "setInterval",
    "clearInterval",
    "Event",
    "EventTarget",
    "CustomEvent",
    "DOMException",
    "AbortController",
    "AbortSignal",
    "TextEncoder",
    "TextDecoder",
    "URL",
    "URLSearchParams",
    "Headers",
    "Request",
    "Response",
    "ReadableStream",
    "ReadableStreamDefaultReader",
    "ReadableStreamDefaultController",
    "crypto",
    "Crypto",
    "SubtleCrypto",
    "performance",
    "Performance",
    "navigator",
    "self",
    "Blob",
    "File",
    "FormData",
    "WritableStream",
    "WritableStreamDefaultWriter",
    "WritableStreamDefaultController",
    "TransformStream",
    "TransformStreamDefaultController",
    "ByteLengthQueuingStrategy",
    "CountQueuingStrategy",
    "TextEncoderStream",
    "TextDecoderStream",
    "URLPattern",
    "CompressionStream",
    "DecompressionStream",
    "ReadableStreamBYOBReader",
    "ReadableByteStreamController",
    "ReadableStreamBYOBRequest",
    "WebAssembly",
    "reportError",
    "onerror",
    "onunhandledrejection",
];
const WINTERTC_NOT_YET: &[&str] = &[];

/// Web-platform interfaces the runtime ships *beyond* the WinterTC Minimum Common API. Not part
/// of the tracked 56/56 score, but presence-guarded the same way so they can't silently regress.
const BEYOND_MINIMUM: &[&str] = &[
    "Worker",
    "WebSocket",
    "EventSource",
    "MessageChannel",
    "MessagePort",
    "BroadcastChannel",
    "MessageEvent",
    "CloseEvent",
    "ErrorEvent",
    "PromiseRejectionEvent",
];

#[test]
fn wintertc_minimum_common_api() {
    let (mut rt, out, _err) = test_runtime();
    let all = [WINTERTC_SUPPORTED, WINTERTC_NOT_YET, BEYOND_MINIMUM]
        .concat()
        .join(",");
    eval_ok(
        &mut rt,
        &format!(
            r#"{{
                const names = "{all}".split(",");
                const present = names.filter((n) => typeof globalThis[n] !== "undefined");
                console.log("PRESENT:" + present.join(","));
            }}"#,
        ),
    );
    let line = out.lines().into_iter().next().unwrap_or_default();
    let present: std::collections::HashSet<&str> = line
        .strip_prefix("PRESENT:")
        .unwrap_or("")
        .split(',')
        .collect();

    let missing_supported: Vec<&str> = WINTERTC_SUPPORTED
        .iter()
        .copied()
        .filter(|n| !present.contains(n))
        .collect();
    assert!(
        missing_supported.is_empty(),
        "WinterTC regression — these SUPPORTED globals went missing: {missing_supported:?}"
    );
    let unexpected: Vec<&str> = WINTERTC_NOT_YET
        .iter()
        .copied()
        .filter(|n| present.contains(n))
        .collect();
    assert!(
        unexpected.is_empty(),
        "WinterTC globals now present but still listed NOT_YET — move them to SUPPORTED: {unexpected:?}"
    );

    let missing_extra: Vec<&str> = BEYOND_MINIMUM
        .iter()
        .copied()
        .filter(|n| !present.contains(n))
        .collect();
    assert!(
        missing_extra.is_empty(),
        "beyond-minimum web interfaces went missing: {missing_extra:?}"
    );

    let total = WINTERTC_SUPPORTED.len() + WINTERTC_NOT_YET.len();
    println!(
        "WinterTC Minimum Common API: {}/{} globals implemented (+{} beyond-minimum interfaces)",
        WINTERTC_SUPPORTED.len(),
        total,
        BEYOND_MINIMUM.len()
    );
}

#[test]
fn error_reporting_globals() {
    // HTML error reporting (WinterTC §5.2): `reportError` fires the global `onerror` handler
    // (returning `true` suppresses the default report); an unhandled rejection fires
    // `onunhandledrejection` whose `event.preventDefault()` suppresses the default line. The
    // default reports land on the error sink in the loop's `Uncaught …` format.
    use std::fs;
    let dir = TempDir::new("error-reporting");
    let root = dir.0.clone();
    fs::write(
        root.join("app.mjs"),
        r#"
        reportError(new TypeError("boom-default"));
        onerror = (message, source, lineno, colno, error) => {
            console.log("onerror", message, error.message, source === "" && lineno === 0);
            return true;
        };
        reportError(new RangeError("boom-suppressed"));
        onerror = null;
        try { reportError(); } catch (e) { console.log("zero-arg", e.constructor.name); }
        onunhandledrejection = (event) => {
            console.log("rejection", event.type, event.reason, event.promise instanceof Promise);
            event.preventDefault();
        };
        Promise.reject("quiet");
        setTimeout(() => {
            onunhandledrejection = null;
            Promise.reject("loud");
        }, 1);
        "#,
    )
    .unwrap();
    let (mut rt, out, err) = test_runtime();
    rt.run_module(&root.join("app.mjs").to_string_lossy())
        .expect("module runs");
    assert_eq!(
        out.lines(),
        [
            "onerror Uncaught RangeError: boom-suppressed boom-suppressed true",
            "zero-arg TypeError",
            "rejection unhandledrejection quiet true",
        ]
    );
    assert_eq!(
        err.lines(),
        [
            "Uncaught TypeError: boom-default",
            "[UnhandledPromiseRejection: This error originated either by throwing inside of an async function without a catch block, or by rejecting a promise which was not handled with .catch(). The promise rejected with the reason \"loud\".] {",
            "  code: 'ERR_UNHANDLED_REJECTION'",
            "}",
            "",
            "Node.js v20.11.0",
        ]
    );
}

#[test]
fn message_channel_semantics() {
    // Channel messaging: data serializes synchronously at postMessage (later mutations are
    // invisible), delivery queues until the receiving port starts (assigning onmessage starts
    // it), the receiver gets a CLONE, and the event is a real MessageEvent.
    let (mut rt, out, _err) = test_runtime();
    rt.eval(
        r#"
        const { port1, port2 } = new MessageChannel();
        const msg = { n: 1 };
        port2.postMessage(msg);       // port1 not started yet: queued
        msg.n = 999;                  // must not be observable (sync serialize)
        port1.onmessage = (e) => {
            console.log(e.data.n, e.data !== msg, e instanceof MessageEvent, e.type);
            port1.close();
        };
        "#,
    )
    .unwrap();
    assert_eq!(out.lines(), ["1 true true message"]);
}

#[test]
fn message_port_close_and_guards() {
    let (mut rt, out, _err) = test_runtime();
    rt.eval(
        r#"
        const { port1, port2 } = new MessageChannel();
        port1.onmessage = () => console.log("received (wrong: port closed)");
        port1.close();
        port2.postMessage("x");       // dropped: peer closed
        try { new MessagePort(); } catch (e) { console.log("ctor:", e.constructor.name); }
        setTimeout(() => console.log("done"), 5);
        "#,
    )
    .unwrap();
    assert_eq!(out.lines(), ["ctor: TypeError", "done"]);
}

#[test]
fn broadcast_channel_fanout() {
    // BroadcastChannel: every same-name channel EXCEPT the sender receives its own clone;
    // posting on a closed channel is an InvalidStateError DOMException.
    let (mut rt, out, _err) = test_runtime();
    rt.eval(
        r#"
        const a = new BroadcastChannel("chan");
        const b = new BroadcastChannel("chan");
        const c = new BroadcastChannel("chan");
        const other = new BroadcastChannel("elsewhere");
        a.onmessage = () => console.log("self (wrong!)");
        other.onmessage = () => console.log("cross-name (wrong!)");
        b.onmessage = (e) => { e.data.x++; console.log("b", e.data.x); };
        c.onmessage = (e) => console.log("c", e.data.x); // b's mutation must not leak here
        a.postMessage({ x: 7 });
        setTimeout(() => {
            c.close();
            try { c.postMessage(1); } catch (e) { console.log("closed:", e.name); }
            a.close(); b.close(); other.close();
        }, 5);
        "#,
    )
    .unwrap();
    assert_eq!(out.lines(), ["b 8", "c 7", "closed: InvalidStateError"]);
}

#[test]
fn abort_signal_any_and_event_classes() {
    let (mut rt, out, _err) = test_runtime();
    rt.eval(
        r#"
        // AbortSignal.any: first abort wins; a pre-aborted input short-circuits.
        const c1 = new AbortController();
        const c2 = new AbortController();
        const s = AbortSignal.any([c1.signal, c2.signal]);
        s.addEventListener("abort", () => console.log("any:", s.reason.message));
        c1.abort(new Error("first"));
        c2.abort(new Error("second (must lose)"));
        console.log("pre:", AbortSignal.any([AbortSignal.abort("done")]).aborted);
        // Event classes: CloseEvent code is ToUint16; ErrorEvent coerces positions; the
        // PromiseRejectionEvent init requires a promise.
        const ce = new CloseEvent("close", { code: 70000, reason: "bye", wasClean: true });
        console.log("close:", ce.code, ce.reason, ce.wasClean);
        const ee = new ErrorEvent("error", { message: "m", lineno: 3.7, error: 42 });
        console.log("errev:", ee.message, ee.lineno, ee.error);
        const pre = new PromiseRejectionEvent("unhandledrejection", {
            promise: Promise.resolve(),
            reason: "r",
        });
        console.log("prev:", pre.reason, pre.promise instanceof Promise);
        try { new PromiseRejectionEvent("x", {}); } catch (e) { console.log("guard:", e.constructor.name); }
        "#,
    )
    .unwrap();
    assert_eq!(
        out.lines(),
        [
            "any: first",
            "pre: true",
            "close: 4464 bye true",
            "errev: m 3 42",
            "prev: r true",
            "guard: TypeError",
        ]
    );
}

// ---- EventSource (SSE) against an in-process event-stream server (lumen_web::sse_testing) ----

use lumen_web::sse_testing::{spawn as spawn_sse, Mode as SseMode};

fn sse_drive(mode: SseMode, conns: usize, script: &str) -> Vec<String> {
    let port = spawn_sse(mode, conns);
    let (mut rt, out, _err) = test_runtime();
    let src = script.replace("{PORT}", &port.to_string());
    rt.eval(&src)
        .expect("sse script parses and runs to quiescence");
    out.lines()
}

#[test]
fn eventsource_parses_events() {
    // Unnamed `message`, a named `event: tick`, and a multi-line `data` (joined with \n) with an
    // `id` — the three canned events, then the stream ends (readyState -> CONNECTING as it would
    // reconnect; we close on the first error).
    let lines = sse_drive(
        SseMode::Events,
        1,
        r#"
        const es = new EventSource("http://127.0.0.1:{PORT}/stream");
        console.log("initial", es.readyState, es.url.endsWith("/stream"));
        es.onopen = () => console.log("open", es.readyState);
        es.onmessage = (e) => console.log("message", JSON.stringify(e.data), e.lastEventId, e instanceof MessageEvent);
        es.addEventListener("tick", (e) => console.log("tick", e.data));
        es.onerror = () => { console.log("error", es.readyState); es.close(); };
        "#,
    );
    assert_eq!(
        lines,
        [
            "initial 0 true",
            "open 1",
            r#"message "hello"  true"#,
            "tick 42",
            r#"message "line one\nline two" 9 true"#,
            "error 0",
        ]
    );
}

#[test]
fn eventsource_reconnects_with_last_event_id() {
    // First connection yields `id: 5` then drops; the client reconnects (default 3s retry is too
    // slow for a test, so the server's event sets retry). We shorten via a `retry:` field is not
    // sent here — instead the test tolerates the reconnect by checking the resumed event, which
    // the server builds from the Last-Event-ID header. Two connections are served.
    let lines = sse_drive(
        SseMode::Reconnect,
        2,
        r#"
        const es = new EventSource("http://127.0.0.1:{PORT}/stream");
        let seen = 0;
        es.onmessage = (e) => {
            console.log("msg", e.data, e.lastEventId);
            if (++seen === 2) es.close();
        };
        "#,
    );
    assert_eq!(lines, ["msg first 5", "msg resumed-from-5 5"]);
}

#[test]
fn eventsource_wrong_content_type_is_fatal() {
    let lines = sse_drive(
        SseMode::WrongContentType,
        1,
        r#"
        const es = new EventSource("http://127.0.0.1:{PORT}/stream");
        es.onopen = () => console.log("open (wrong!)");
        es.onerror = () => console.log("error", es.readyState);
        "#,
    );
    // A fatal error sets readyState CLOSED (2) and does not reconnect.
    assert_eq!(lines, ["error 2"]);
}

#[test]
fn eventsource_204_stops() {
    let lines = sse_drive(
        SseMode::NoContent,
        1,
        r#"
        const es = new EventSource("http://127.0.0.1:{PORT}/stream");
        es.onerror = () => console.log("error", es.readyState);
        "#,
    );
    assert_eq!(lines, ["error 2"]);
}

#[test]
fn eventsource_field_parser_units() {
    // Exercise the line parser directly against a synthetic stream via a data: URL is not
    // supported; instead validate comment-skipping, colon-less field, leading-space stripping,
    // and retry parsing through the state the events produce. Server Events mode already covers
    // the happy path; here we assert the readyState constants + url/withCredentials surface.
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        console.log(EventSource.CONNECTING, EventSource.OPEN, EventSource.CLOSED);
        const es = new EventSource("http://127.0.0.1:1/nope", { withCredentials: true });
        console.log(es.withCredentials, es.url.endsWith("/nope"));
        es.close();
        console.log(es.readyState);
        "#,
    );
    assert_eq!(out.lines(), ["0 1 2", "true true", "2"]);
}

// ---- structured-clone wire format (cross-thread serialize/deserialize) ----

#[test]
fn structured_clone_wire_round_trips() {
    // The JS wire format (__serializeForClone/__deserializeClone) round-trips the structured-clone
    // subset, including cycles, shared subgraphs, typed arrays, and DataCloneError. This is what
    // carries a Worker message across the thread boundary.
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const rt = (v) => __deserializeClone(__serializeForClone(v));
        console.log("prims", rt(undefined), rt(null), rt(true), rt(-3.5), rt("x\u{1f600}"));
        console.log("bigint", rt(90071992547409910n) === 90071992547409910n);
        console.log("nested", JSON.stringify(rt({ a: [1, { b: 2 }], c: "y" })));
        console.log("date", rt(new Date(123)).getTime());
        console.log("regexp", String(rt(/a.c/gi)));
        const m = rt(new Map([["k", 1]])); const s = rt(new Set([1, 2, 2]));
        console.log("mapset", m.get("k"), [...s].join(","));
        const ta = rt(new Float64Array([1.5, 2.5]));
        console.log("typedarray", ta instanceof Float64Array, ta.join(","));
        const c = { x: 1 }; c.self = c; const rc = rt(c);
        console.log("cycle", rc.self === rc, rc.x);
        const shared = { v: 9 }; const g = rt({ a: shared, b: shared });
        console.log("shared", g.a === g.b);
        const e = rt(new TypeError("boom")); console.log("error", e.name, e.message);
        try { rt(Symbol("s")); } catch (ex) { console.log("throws", ex.name); }
        "#,
    );
    assert_eq!(
        out.lines(),
        [
            "prims undefined null true -3.5 x\u{1f600}",
            "bigint true",
            r#"nested {"a":[1,{"b":2}],"c":"y"}"#,
            "date 123",
            "regexp /a.c/gi",
            "mapset 1 1,2",
            "typedarray true 1.5,2.5",
            "cycle true 1",
            "shared true",
            "error TypeError boom",
            "throws DataCloneError",
        ]
    );
}

// ---- Web Workers (realm-per-thread + structured messaging) ----

/// Write worker scripts into a temp dir and drive `main_src` against them; returns stdout lines.
/// Every case must arrange for the worker(s) to exit (close/terminate) so the loop finishes.
fn worker_drive(files: &[(&str, &str)], main_src: &str) -> Vec<String> {
    let dir = TempDir::new("worker");
    for (name, body) in files {
        std::fs::write(dir.0.join(name), body).unwrap();
    }
    let (mut rt, out, _err) = test_runtime();
    let src = main_src.replace("{DIR}", &dir.0.to_string_lossy().replace('\\', "/"));
    rt.eval(&src)
        .expect("worker main parses and runs to quiescence");
    out.lines()
}

#[test]
fn worker_message_round_trip_and_structured_clone() {
    // A structured payload (nested object, array, Date, Map) survives the cross-thread clone in
    // both directions; the worker computes a reply and then closes itself.
    let lines = worker_drive(
        &[(
            "echo.mjs",
            r#"
            onmessage = (e) => {
                const d = e.data;
                postMessage({
                    sum: d.nums.reduce((a, b) => a + b, 0),
                    ts: d.when.getTime(),
                    tag: d.meta.get("tag"),
                });
                close();
            };
            "#,
        )],
        r#"
        const w = new Worker("{DIR}/echo.mjs", { type: "module" });
        w.onmessage = (e) => console.log("reply", e.data.sum, e.data.ts, e.data.tag);
        w.postMessage({ nums: [1, 2, 3, 4], when: new Date(1000), meta: new Map([["tag", "hi"]]) });
        "#,
    );
    assert_eq!(lines, ["reply 10 1000 hi"]);
}

#[test]
fn browser_worker_installs_shared_css_typed_om_interfaces() {
    let lines = worker_drive(
        &[(
            "typed-om.mjs",
            r#"
            const parsed = CSSNumericValue.parse("calc(2 * 3s)");
            const inverse = new CSSMathInvert(CSS.px(2));
            postMessage([
                typeof CSSNumericValue,
                parsed instanceof CSSMathProduct,
                parsed.toString(),
                parsed.type().time,
                inverse.type().length,
            ].join("|"));
            close();
            "#,
        )],
        r#"
        const worker = new Worker("{DIR}/typed-om.mjs", { type: "module" });
        worker.onmessage = event => console.log(event.data);
        "#,
    );
    assert_eq!(lines, ["function|true|calc(2 * 3s)|1|-1"]);
}

#[test]
fn worker_bidirectional_conversation() {
    // Several messages each way, in order; the worker closes after the third.
    let lines = worker_drive(
        &[(
            "counter.mjs",
            r#"
            let total = 0;
            onmessage = (e) => {
                total += e.data;
                postMessage(total);
                if (total >= 6) close();
            };
            "#,
        )],
        r#"
        const w = new Worker("{DIR}/counter.mjs", { type: "module" });
        const send = [1, 2, 3];
        let i = 0;
        w.onmessage = (e) => {
            console.log("running total", e.data);
            if (i < send.length) w.postMessage(send[i++]);
        };
        w.postMessage(send[i++]);
        "#,
    );
    assert_eq!(
        lines,
        ["running total 1", "running total 3", "running total 6"]
    );
}

#[test]
fn worker_error_propagates_to_onerror() {
    let lines = worker_drive(
        &[(
            "boom.mjs",
            r#"onmessage = (e) => { throw new RangeError("bad: " + e.data); };"#,
        )],
        r#"
        const w = new Worker("{DIR}/boom.mjs", { type: "module" });
        w.onerror = (e) => { console.log("onerror", e.message, e instanceof ErrorEvent); w.terminate(); };
        w.postMessage(42);
        "#,
    );
    assert_eq!(lines, ["onerror Uncaught RangeError: bad: 42 true"]);
}

#[test]
fn worker_terminate_stops_a_running_worker() {
    // A worker posting on an interval is terminated after 3 messages; no further messages arrive,
    // and the main loop exits promptly.
    let lines = worker_drive(
        &[(
            "ticker.mjs",
            r#"
            let n = 0;
            setInterval(() => { n++; postMessage(n); }, 3);
            "#,
        )],
        r#"
        const w = new Worker("{DIR}/ticker.mjs", { type: "module" });
        let count = 0;
        w.onmessage = (e) => {
            count++;
            if (count === 3) {
                w.terminate();
                console.log("terminated at", e.data);
                setTimeout(() => console.log("no leak"), 60);
            } else if (count > 3) {
                console.log("LEAK", e.data);
            }
        };
        "#,
    );
    assert_eq!(lines, ["terminated at 3", "no leak"]);
}

#[test]
fn worker_load_error_reports() {
    // A missing worker script surfaces as an error event, not a hang.
    let lines = worker_drive(
        &[],
        r#"
        const w = new Worker("{DIR}/does-not-exist.mjs", { type: "module" });
        w.onerror = (e) => { console.log("load-error", e.message.includes("cannot load")); w.terminate(); };
        "#,
    );
    assert_eq!(lines, ["load-error true"]);
}

#[test]
fn worker_datacloneerror_on_unserializable() {
    // Posting a function is a DataCloneError, synchronously, on the sending side.
    let lines = worker_drive(
        &[("noop.mjs", "onmessage = () => close();")],
        r#"
        const w = new Worker("{DIR}/noop.mjs", { type: "module" });
        try { w.postMessage(() => 1); } catch (e) { console.log("clone-error", e.name); }
        w.terminate();
        "#,
    );
    assert_eq!(lines, ["clone-error DataCloneError"]);
}

// ---- WebSocket (RFC 6455) against an in-process echo server (lumen_web::ws_testing) ----

use lumen_web::ws_testing::{spawn_echo, Mode as WsMode};

/// Drive a WebSocket against `mode`'s echo server and return the captured stdout lines.
fn ws_drive(mode: WsMode, script: &str) -> Vec<String> {
    let port = spawn_echo(mode, 1);
    let (mut rt, out, _err) = test_runtime();
    let src = script.replace("{PORT}", &port.to_string());
    rt.eval(&src)
        .expect("ws script parses and runs to quiescence");
    out.lines()
}

#[test]
fn websocket_text_and_binary_round_trip() {
    let lines = ws_drive(
        WsMode::Echo,
        r#"
        const ws = new WebSocket("ws://127.0.0.1:{PORT}/", ["chat", "v2"]);
        ws.binaryType = "arraybuffer";
        ws.onopen = () => {
            console.log("open", ws.readyState, ws.protocol);
            ws.send("hello");
        };
        ws.onmessage = (e) => {
            if (typeof e.data === "string") {
                console.log("text", e.data, e instanceof MessageEvent, e.origin);
                ws.send(new Uint8Array([1, 2, 254, 255]));
            } else {
                console.log("binary", Array.from(new Uint8Array(e.data)).join(","));
                ws.close(1000, "done");
            }
        };
        ws.onclose = (e) => console.log("close", e.code, e.reason, e.wasClean, ws.readyState);
        "#,
    );
    let port = lines[0].is_empty();
    let _ = port;
    // The message-event origin is the socket URL (host:port varies), so match it structurally.
    assert_eq!(lines[0], "open 1 chat");
    assert!(
        lines[1].starts_with("text hello true ws://127.0.0.1:") && lines[1].ends_with('/'),
        "message event shape/origin: {}",
        lines[1]
    );
    assert_eq!(lines[2], "binary 1,2,254,255");
    assert_eq!(lines[3], "close 1000 done true 3");
    assert_eq!(lines.len(), 4);
}

#[test]
fn websocket_transparent_ping_pong() {
    // The server pings on connect; the client must answer with a pong transparently (no user
    // event), which the server then reports back as a text message.
    let lines = ws_drive(
        WsMode::PingThenEcho,
        r#"
        const ws = new WebSocket("ws://127.0.0.1:{PORT}/");
        ws.onmessage = (e) => { console.log("msg", e.data); ws.close(); };
        ws.onclose = () => console.log("closed");
        "#,
    );
    assert_eq!(lines, ["msg pong:marco", "closed"]);
}

#[test]
fn websocket_reassembles_fragments() {
    // A message split across a data frame + two continuation frames arrives whole.
    let lines = ws_drive(
        WsMode::FragmentedHello,
        r#"
        const ws = new WebSocket("ws://127.0.0.1:{PORT}/");
        ws.onmessage = (e) => { console.log("got", e.data, e.data.length); ws.close(); };
        ws.onclose = () => console.log("closed");
        "#,
    );
    assert_eq!(lines, ["got fragment 8", "closed"]);
}

#[test]
fn websocket_server_initiated_close() {
    let lines = ws_drive(
        WsMode::CloseImmediately,
        r#"
        const ws = new WebSocket("ws://127.0.0.1:{PORT}/");
        ws.onclose = (e) => console.log("close", e.code, e.reason, e.wasClean);
        ws.onopen = () => console.log("open");
        "#,
    );
    assert_eq!(lines, ["open", "close 4001 going away true"]);
}

#[test]
fn websocket_handshake_rejection_is_error_then_close() {
    // A wrong Sec-WebSocket-Accept fails the connection: an error event, then a 1006 close.
    let lines = ws_drive(
        WsMode::BadAccept,
        r#"
        const ws = new WebSocket("ws://127.0.0.1:{PORT}/");
        ws.onerror = () => console.log("error", ws.readyState);
        ws.onclose = (e) => console.log("close", e.code, e.wasClean);
        ws.onopen = () => console.log("open (wrong!)");
        "#,
    );
    assert_eq!(lines, ["error 3", "close 1006 false"]);
}

#[test]
fn websocket_constructor_validation() {
    // Bad scheme, fragment, and duplicate subprotocol all throw synchronously (no server needed).
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const bad = (fn) => { try { fn(); console.log("no throw"); } catch (e) { console.log(e.name); } };
        bad(() => new WebSocket("http://x/"));
        bad(() => new WebSocket("ws://x/#frag"));
        bad(() => new WebSocket("ws://x/", ["a", "a"]));
        bad(() => new WebSocket("ws://x/", ["bad proto"]));
        console.log(WebSocket.CONNECTING, WebSocket.OPEN, WebSocket.CLOSING, WebSocket.CLOSED);
        "#,
    );
    assert_eq!(
        out.lines(),
        [
            "SyntaxError",
            "SyntaxError",
            "SyntaxError",
            "SyntaxError",
            "0 1 2 3"
        ]
    );
}

#[test]
fn wintertc_functional_smoke() {
    // Presence is not correctness: exercise the core interfaces end-to-end.
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const ok = [];
        ok.push(JSON.stringify(structuredClone({a:[1,2]})) === '{"a":[1,2]}');
        ok.push(new TextDecoder().decode(new TextEncoder().encode("héllo")) === "héllo");
        ok.push(new URL("http://x/y?a=1").searchParams.get("a") === "1");
        ok.push(atob(btoa("hi")) === "hi");
        ok.push(new AbortController().signal.aborted === false);
        ok.push(/^[0-9a-f-]{36}$/.test(crypto.randomUUID()));
        ok.push(typeof performance.now() === "number");
        ok.push(new Headers({a:"1"}).get("a") === "1");
        console.log(ok.every(Boolean) ? "ALL_OK" : "FAIL:" + ok.join(","));
        "#,
    );
    assert_eq!(out.lines(), ["ALL_OK"]);
}

// Cold-boot cost breakdown (realm intrinsics + per-extension install), for tracking the
// startup floor. `#[ignore]`d (timing, not correctness):
//   cargo test -p lumen-runtime perf_boot_breakdown --release -- --ignored --nocapture
#[test]
#[ignore]
fn perf_boot_breakdown() {
    use std::time::Instant;

    fn median(mut xs: Vec<f64>) -> f64 {
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        xs[xs.len() / 2]
    }
    // Time a closure's median over N runs, in microseconds.
    fn bench(n: usize, mut f: impl FnMut() -> f64) -> f64 {
        median((0..n).map(|_| f()).collect())
    }

    let engine_us = bench(30, || {
        let t = Instant::now();
        let _e = lumen_host::Engine::new();
        t.elapsed().as_secs_f64() * 1e6
    });
    println!("Engine::new (realm intrinsics)   {engine_us:8.1} us");

    // Per-extension install (state + ops + js_init parse+eval), in the real order.
    let names = ["timers", "console", "process", "web", "node"];
    let mut totals = vec![Vec::new(); names.len()];
    for _ in 0..30 {
        let (tx, _rx) = std::sync::mpsc::channel();
        let pool = ThreadPool::new(4, tx);
        let mut engine = lumen_host::Engine::new();
        engine.ctx().op_state().put(pool.handle());
        engine.ctx().op_state().put(TaskRegistry::default());
        let exts = [
            lumen_timers::extension(),
            console::extension(),
            process::extension(),
            lumen_web::extension(),
            lumen_node::extension(),
        ];
        for (i, ext) in exts.into_iter().enumerate() {
            let t = Instant::now();
            install(&mut engine, std::slice::from_ref(&ext));
            totals[i].push(t.elapsed().as_secs_f64() * 1e6);
        }
    }
    let mut sum = 0.0;
    for (i, name) in names.iter().enumerate() {
        let m = median(std::mem::take(&mut totals[i]));
        sum += m;
        println!("install {name:<8}                 {m:8.1} us");
    }
    println!("---\nextensions total                 {sum:8.1} us");
}

#[test]
fn web_serve_roundtrip_over_loopback() {
    // A whole HTTP server + client on one loop: Lumen.serve binds, the same runtime fetches
    // itself through the loopback socket (server accept, client request, and response write all
    // run concurrently on the threadpool), then shutdown() lets the loop go idle so eval returns.
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        (async () => {
            const server = Lumen.serve(async (req) => {
                const u = new URL(req.url);
                if (u.pathname === "/json") {
                    return Response.json({ ok: true, id: u.searchParams.get("id") });
                }
                if (req.method === "POST") return new Response("got:" + (await req.text()));
                return new Response("pong\n", { headers: { "x-cta": "hi" } });
            }, { hostname: "127.0.0.1", port: 0 });

            const base = `http://127.0.0.1:${server.port}`;
            const r1 = await fetch(base + "/");
            console.log(r1.status, r1.headers.get("x-cta"), (await r1.text()).trim());

            const r2 = await fetch(base + "/json?id=42");
            const j = await r2.json();
            console.log(r2.status, j.ok, j.id);

            const r3 = await fetch(base + "/echo", { method: "POST", body: "hey" });
            console.log(r3.status, await r3.text());

            await server.shutdown();
            console.log("closed");
        })();
        "#,
    );
    assert_eq!(
        out.lines(),
        ["200 hi pong", "200 true 42", "200 got:hey", "closed"]
    );
}

// ---- lumen-node (node: compat; the runtime assembles it) ----

#[test]
fn node_path_and_os_builtins() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const path = require("node:path");
        console.log(path.join("a", "b", "..", "c"), path.extname("x.tar.gz"), path.basename("/p/q.js", ".js"));
        console.log(path.dirname("/a/b/c"), path.isAbsolute("/x"), path.isAbsolute("x"));
        const os = require("os");
        console.log(typeof os.platform(), typeof os.homedir(), os.EOL === "\n" || os.EOL === "\r\n");
        console.log(require("path") === require("node:path"));
        "#,
    );
    assert_eq!(
        out.lines(),
        if cfg!(windows) {
            [
                "a\\c .gz q",
                "\\a\\b true false",
                "string string true",
                "true",
            ]
        } else {
            ["a/c .gz q", "/a/b true false", "string string true", "true"]
        }
    );
}

#[test]
fn node_buffer() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        console.log(Buffer.from("hé").length, Buffer.from("hé").toString("hex"));
        console.log(Buffer.from("6869", "hex").toString());
        console.log(Buffer.from("aGVsbG8=", "base64").toString());
        console.log(Buffer.alloc(3, 7).toString("hex"));
        console.log(Buffer.concat([Buffer.from("ab"), Buffer.from("cd")]).toString());
        console.log(Buffer.isBuffer(Buffer.from("x")), Buffer.isBuffer(new Uint8Array(1)));
        console.log(Buffer.from("abc").equals(Buffer.from("abc")), Buffer.from("abc").compare(Buffer.from("abd")));
        const b = Buffer.alloc(4); b.writeUInt32LE(0x01020304, 0);
        console.log(b.toString("hex"), b.readUInt32BE(0).toString(16));
        "#,
    );
    assert_eq!(
        out.lines(),
        [
            "3 68c3a9",
            "hi",
            "hello",
            "070707",
            "abcd",
            "true false",
            "true -1",
            "04030201 4030201",
        ]
    );
}

#[test]
fn node_require_resolution() {
    use std::fs;
    let dir = TempDir::new("require");
    let root = dir.0.clone();
    fs::write(
        root.join("lib.js"),
        "module.exports = { v: require('./data.json').v + 1 };",
    )
    .unwrap();
    fs::write(root.join("data.json"), r#"{ "v": 41 }"#).unwrap();
    let pkg = root.join("node_modules").join("widget");
    fs::create_dir_all(pkg.join("lib")).unwrap();
    fs::write(
        pkg.join("package.json"),
        r#"{ "name": "widget", "main": "lib/main.js" }"#,
    )
    .unwrap();
    fs::write(
        pkg.join("lib").join("main.js"),
        "module.exports = () => 'widget-ok';",
    )
    .unwrap();
    fs::write(
        root.join("entry.js"),
        r#"
        const lib = require("./lib");
        const widget = require("widget");
        console.log(lib.v, widget());
        console.log(require.resolve("./lib").endsWith("lib.js"));
        // cache: requiring twice yields the same exports object
        console.log(require("./lib") === require("./lib"));
        "#,
    )
    .unwrap();

    let (mut rt, out, _err) = test_runtime();
    let entry = root.join("entry.js").to_string_lossy().into_owned();
    rt.run_main(&entry).expect("main runs");
    assert_eq!(out.lines(), ["42 widget-ok", "true", "true"]);
}

/// Runs `body` on a thread with the stack real engine hosts use: `run_main` nests the CommonJS
/// loader, `require` and the lazy node glue's first evaluation, which overflows a default 2 MiB
/// debug-build test thread.
fn on_engine_stack(body: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(lumen::THREAD_STACK_SIZE)
        .spawn(body)
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn node_run_main_dirname_and_module() {
    on_engine_stack(node_run_main_dirname_and_module_body);
}

fn node_run_main_dirname_and_module_body() {
    use std::fs;
    let dir = TempDir::new("main");
    let entry = dir.0.join("prog.js");
    fs::write(
        &entry,
        r#"
        const path = require("node:path");
        console.log(__filename.endsWith("prog.js"));
        console.log(__dirname === path.dirname(__filename));
        console.log(require.main.filename === __filename);
        module.exports = { ran: true };
        "#,
    )
    .unwrap();
    let (mut rt, out, _err) = test_runtime();
    rt.run_main(&entry.to_string_lossy()).expect("runs");
    assert_eq!(out.lines(), ["true", "true", "true"], "stderr: {:?}", _err.lines());
}

#[test]
fn node_fs_module_round_trips() {
    use std::fs;
    let dir = TempDir::new("nodefs");
    let target = dir.path("f.txt");
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        &format!(
            r#"
            const nfs = require("node:fs");
            nfs.writeFileSync({target:?}, "node fs content");
            console.log(nfs.readFileSync({target:?}, "utf8"));
            console.log(nfs.readFileSync({target:?}) instanceof Buffer);
            console.log(nfs.existsSync({target:?}), nfs.statSync({target:?}).isFile());
            "#,
        ),
    );
    let _ = fs::remove_file(&target);
    assert_eq!(out.lines(), ["node fs content", "true", "true true"]);
}

#[test]
fn node_require_missing_module_throws() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        try { require("./does-not-exist"); }
        catch (e) { console.log(e.code, e.message.includes("Cannot find module")); }
        "#,
    );
    assert_eq!(out.lines(), ["MODULE_NOT_FOUND true"]);
}

// ---- ESM (run_module: the import graph resolves against disk + node_modules) ----

#[test]
fn esm_named_default_json_and_dynamic_import() {
    use std::fs;
    let dir = TempDir::new("esm-basic");
    let root = dir.0.clone();
    fs::write(
        root.join("util.mjs"),
        "export const double = (n) => n * 2;\nexport default { tag: 'd' };",
    )
    .unwrap();
    fs::write(root.join("data.json"), r#"{ "answer": 42 }"#).unwrap();
    fs::write(
        root.join("app.mjs"),
        r#"
        import def, { double } from "./util.mjs";
        import data from "./data.json";
        console.log(double(21), def.tag, data.answer);
        const dyn = await import("./util.mjs");
        console.log(dyn.double(4));
        "#,
    )
    .unwrap();
    let (mut rt, out, _err) = test_runtime();
    rt.run_module(&root.join("app.mjs").to_string_lossy())
        .expect("module runs");
    assert_eq!(out.lines(), ["42 d 42", "8"]);
}

#[test]
fn esm_import_text_attribute() {
    // TC39 import-text: `with { type: "text" }` default-exports the file's contents, decoded
    // as UTF-8 with a leading BOM stripped — for any extension, including a .js file (which
    // must not execute). Dynamic import with the attribute resolves the same record.
    use std::fs;
    let dir = TempDir::new("esm-import-text");
    let root = dir.0.clone();
    fs::write(root.join("note.txt"), b"\xef\xbb\xbfHello \xc3\xa9!\n").unwrap();
    fs::write(root.join("side.js"), "globalThis.__ranSide = true;").unwrap();
    fs::write(
        root.join("app.mjs"),
        r#"
        import note from "./note.txt" with { type: "text" };
        import source from "./side.js" with { type: "text" };
        console.log(JSON.stringify(note));
        console.log(source === "globalThis.__ranSide = true;", typeof globalThis.__ranSide);
        const dyn = await import("./note.txt", { with: { type: "text" } });
        console.log(dyn.default === note);
        "#,
    )
    .unwrap();
    let (mut rt, out, _err) = test_runtime();
    rt.run_module(&root.join("app.mjs").to_string_lossy())
        .expect("module runs");
    assert_eq!(
        out.lines(),
        ["\"Hello \u{e9}!\\n\"", "true undefined", "true"]
    );
}

#[test]
fn esm_import_bytes_attribute() {
    // TC39 import-bytes: `with { type: "bytes" }` default-exports a Uint8Array over an
    // immutable buffer, byte-exact for arbitrary binary content (here deliberately invalid
    // UTF-8). Strict (module) writes throw TypeError and leave the bytes untouched.
    use std::fs;
    let dir = TempDir::new("esm-import-bytes");
    let root = dir.0.clone();
    fs::write(root.join("blob.bin"), [0u8, 1, 0xfe, 0xff, 0x80, 65]).unwrap();
    fs::write(
        root.join("app.mjs"),
        r#"
        import b from "./blob.bin" with { type: "bytes" };
        console.log(b instanceof Uint8Array, b.length, Array.from(b).join(","));
        console.log(b.buffer.immutable);
        let wrote = "no-throw";
        try { b[0] = 9; } catch (e) { wrote = e.constructor.name; }
        console.log(wrote, b[0]);
        const dyn = await import("./blob.bin", { with: { type: "bytes" } });
        console.log(dyn.default === b);
        "#,
    )
    .unwrap();
    let (mut rt, out, _err) = test_runtime();
    rt.run_module(&root.join("app.mjs").to_string_lossy())
        .expect("module runs");
    assert_eq!(
        out.lines(),
        ["true 6 0,1,254,255,128,65", "true", "TypeError 0", "true"]
    );
}

#[test]
fn esm_import_json_attribute() {
    // JSON modules: `with { type: "json" }` JSON.parses the file for ANY extension; the legacy
    // attribute-less `.json` import gets the same JSON.parse semantics (`__proto__` becomes a
    // plain own data property — never prototype-setting) but stays a distinct module record.
    use std::fs;
    let dir = TempDir::new("esm-import-json");
    let root = dir.0.clone();
    fs::write(
        root.join("data.json"),
        r#"{ "answer": 42, "__proto__": { "evil": true } }"#,
    )
    .unwrap();
    fs::write(root.join("data.txt"), r#"{ "fromTxt": true }"#).unwrap();
    fs::write(root.join("bad.json"), "{ bad").unwrap();
    fs::write(
        root.join("app.mjs"),
        r#"
        import data from "./data.json" with { type: "json" };
        import legacy from "./data.json";
        import txt from "./data.txt" with { type: "json" };
        console.log(data.answer, txt.fromTxt, data === legacy);
        const safe = (o) =>
            Object.getPrototypeOf(o) === Object.prototype &&
            o.evil === undefined &&
            Object.getOwnPropertyNames(o).includes("__proto__");
        console.log(safe(data), safe(legacy));
        const dyn = await import("./data.json", { with: { type: "json" } });
        console.log(dyn.default === data);
        try {
            await import("./bad.json", { with: { type: "json" } });
            console.log("bad: resolved");
        } catch (e) {
            console.log("bad:", e.constructor.name);
        }
        "#,
    )
    .unwrap();
    let (mut rt, out, _err) = test_runtime();
    rt.run_module(&root.join("app.mjs").to_string_lossy())
        .expect("module runs");
    assert_eq!(
        out.lines(),
        ["42 true false", "true true", "true", "bad: SyntaxError"]
    );
}

#[test]
fn esm_imports_node_builtins_named_and_default() {
    use std::fs;
    let dir = TempDir::new("esm-builtin");
    let entry = dir.0.join("b.mjs");
    fs::write(
        &entry,
        r#"
        import { readFileSync, writeFileSync } from "node:fs";
        import path, { join } from "node:path";
        import os from "os";
        console.log(typeof readFileSync, typeof writeFileSync);
        console.log(path.basename("/a/b.js"), join("x", "y"));
        console.log(typeof os.platform());
        "#,
    )
    .unwrap();
    let (mut rt, out, _err) = test_runtime();
    rt.run_module(&entry.to_string_lossy()).expect("runs");
    let joined = if cfg!(windows) {
        "b.js x\\y"
    } else {
        "b.js x/y"
    };
    assert_eq!(out.lines(), ["function function", joined, "string"]);
}

#[test]
fn esm_resolves_node_modules_packages() {
    use std::fs;
    let dir = TempDir::new("esm-pkg");
    let root = dir.0.clone();
    let esm = root.join("node_modules").join("esmpkg");
    fs::create_dir_all(&esm).unwrap();
    fs::write(
        esm.join("package.json"),
        r#"{ "name":"esmpkg", "type":"module", "main":"index.js" }"#,
    )
    .unwrap();
    fs::write(esm.join("index.js"), "export const from = 'esm-pkg';").unwrap();
    let cjs = root.join("node_modules").join("cjspkg");
    fs::create_dir_all(&cjs).unwrap();
    fs::write(
        cjs.join("package.json"),
        r#"{ "name":"cjspkg", "main":"index.js" }"#,
    )
    .unwrap();
    fs::write(
        cjs.join("index.js"),
        "module.exports = { from: 'cjs-pkg' };",
    )
    .unwrap();
    fs::write(
        root.join("app.mjs"),
        r#"
        import { from as e } from "esmpkg";
        import cjs from "cjspkg";
        console.log(e, cjs.from);
        "#,
    )
    .unwrap();
    let (mut rt, out, _err) = test_runtime();
    rt.run_module(&root.join("app.mjs").to_string_lossy())
        .expect("runs");
    assert_eq!(out.lines(), ["esm-pkg cjs-pkg"]);
}

#[test]
fn esm_prefers_exports_import_over_cjs_main() {
    // A package shaped like hono: `main` is a CJS build, but `type:module` + the exports `import`
    // condition point at an ESM build with real named exports. The bare import must resolve the
    // ESM entry (named `Hono` works), not fall through to `main` (CJS, default-only).
    use std::fs;
    let dir = TempDir::new("esm-exports");
    let root = dir.0.clone();
    let pkg = root.join("node_modules").join("dual");
    fs::create_dir_all(pkg.join("dist").join("cjs")).unwrap();
    fs::write(
        pkg.join("package.json"),
        r#"{
            "name": "dual",
            "main": "dist/cjs/index.js",
            "type": "module",
            "module": "dist/index.js",
            "exports": { ".": {
                "import": "./dist/index.js",
                "require": "./dist/cjs/index.js"
            }, "./dist/named.js": "./dist/named.js" }
        }"#,
    )
    .unwrap();
    fs::write(
        pkg.join("dist").join("index.js"),
        "export class Widget { hi() { return 'esm'; } }\nexport const kind = 'named';",
    )
    .unwrap();
    // If resolution wrongly picked this CJS build, loading it as ESM would fail (`module` is not
    // defined) or expose no named `Widget`.
    fs::write(
        pkg.join("dist").join("cjs").join("index.js"),
        "module.exports = { Widget: null, kind: 'cjs' };",
    )
    .unwrap();
    // An explicitly exported subpath must inherit the package's `type:module`.
    fs::write(
        pkg.join("dist").join("named.js"),
        "export const sub = 'subpath-esm';",
    )
    .unwrap();
    fs::write(
        root.join("app.mjs"),
        r#"
        import { Widget, kind } from "dual";
        import { sub } from "dual/dist/named.js";
        console.log(new Widget().hi(), kind, sub);
        "#,
    )
    .unwrap();
    let (mut rt, out, _err) = test_runtime();
    rt.run_module(&root.join("app.mjs").to_string_lossy())
        .expect("runs");
    assert_eq!(out.lines(), ["esm named subpath-esm"]);
}

#[test]
fn esm_top_level_await_on_timer_settles() {
    use std::fs;
    let dir = TempDir::new("esm-tla");
    let entry = dir.0.join("tla.mjs");
    fs::write(
        &entry,
        r#"
        const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
        console.log("before");
        await sleep(10);
        console.log("after");
        "#,
    )
    .unwrap();
    let (mut rt, out, _err) = test_runtime();
    rt.run_module(&entry.to_string_lossy()).expect("runs");
    assert_eq!(out.lines(), ["before", "after"]);
}

#[test]
fn esm_module_not_found_is_an_error() {
    use std::fs;
    let dir = TempDir::new("esm-missing");
    let entry = dir.0.join("bad.mjs");
    fs::write(&entry, "import x from './nope.mjs';\n").unwrap();
    let (mut rt, _out, _err) = test_runtime();
    let err = rt.run_module(&entry.to_string_lossy()).unwrap_err();
    assert!(err.contains("not found") || err.contains("nope"), "{err}");
}

/// Bun.hash through the whole JS glue stack: values are Bun 1.2.21 outputs (the exhaustive
/// oracle-matrix test lives in lumen-node's `bunhash::tests`; this covers the glue — return
/// types, input coercion, and seed coercion edge cases).
#[test]
fn bun_hash_matches_bun_through_the_glue() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const h = Bun.hash;
        console.log(`${typeof h("hello")} ${h("hello")}`);
        console.log(`${h.wyhash("hello", 1)}`);
        console.log(`${typeof h.cityHash32("hello")} ${h.cityHash32("hello")}`);
        console.log(`${h.cityHash64("hello")}`);
        console.log(`${h.xxHash32("hello")} ${h.xxHash64("hello")} ${h.xxHash3("hello")}`);
        console.log(`${h.murmur32v3("hello")} ${h.murmur32v2("hello")} ${h.murmur64v2("hello")}`);
        console.log(`${h.rapidhash("hello")} ${h.crc32("hello")} ${h.adler32("hello")}`);
        // seed coercion: int32 sign-extends; 2^51 clamps to 0; bigints wrap mod 2^64
        console.log(`${h.wyhash("x", -1)} ${h.wyhash("x", 2 ** 51) === h.wyhash("x", 0)}`);
        console.log(`${h.wyhash("hello world test", 2n ** 64n - 1n)}`);
        // input coercion: string === its utf8 bytes === offset subarray; null → "null"
        const bytes = new TextEncoder().encode("hello");
        const sub = new Uint8Array(new TextEncoder().encode("XXhelloYY").buffer, 2, 5);
        console.log(`${h.wyhash(bytes) === h.wyhash("hello")} ${h.wyhash(sub) === h.wyhash("hello")}`);
        console.log(`${h.wyhash(null) === h.wyhash("null")}`);
        "#,
    );
    rt.run_to_completion();
    assert_eq!(
        out.lines(),
        [
            "bigint 1019145960556548909",
            "15802777309726279454",
            "number 2039911270",
            "16172099214758459231",
            "4211111929 2794345569481354659 10760762337991515389",
            "613153351 3848350155 2191231550387646743",
            "9166712279701818032 907060870 103547413",
            "12979056674793561970 true",
            "5531584226709605751",
            "true true",
            "true",
        ]
    );
}

#[test]
fn node_url_module_uses_native_url_classes() {
    let (mut rt, out, err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const url = require('url');
        const util = require('util');
        const buffer = require('buffer');
        console.log(url.URL === URL, url.URLSearchParams === URLSearchParams);
        console.log(url.domainToASCII('español.com'), url.domainToUnicode('xn--espaol-zwa.com'));
        console.log(url.format(new URL('https://a:b@測試.com/p?q=1#h'), { fragment: false, unicode: true, auth: false, search: false }));
        console.log(url.fileURLToPath('file:///tmp/a%20b'), url.pathToFileURL('/tmp/a b').href);
        console.log(util.inspect(new URL('http://u@a.com:81/p?x=1#f')).split('\n')[0]);
        console.log(util.inspect(new URLSearchParams('a=1&b=2')));
        console.log(util.inspect(new URLSearchParams('a=1').keys()));
        const blob = new Blob(['x']);
        const id = URL.createObjectURL(blob);
        console.log(buffer.resolveObjectURL(id) !== undefined);
        URL.revokeObjectURL(id);
        console.log(buffer.resolveObjectURL(id) === undefined);
        try { new URL('bad'); } catch (e) { console.log(e.code, e.input); }
        try { url.fileURLToPath('http://a/'); } catch (e) { console.log(e.code); }
        "#,
    );
    assert!(err.lines().is_empty(), "stderr: {:?}", err.lines());
    assert_eq!(
        out.lines(),
        [
            "true true",
            "xn--espaol-zwa.com español.com",
            "https://測試.com/p",
            "/tmp/a b file:///tmp/a%20b",
            "URL {",
            "URLSearchParams { 'a' => '1', 'b' => '2' }",
            "URLSearchParams Iterator { 'a' }",
            "true",
            "true",
            "ERR_INVALID_URL bad",
            "ERR_INVALID_URL_SCHEME",
        ]
    );
}

#[test]
fn node_blob_keeps_files_on_disk_and_resolves_object_urls() {
    let (mut rt, out, err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const fs = require('fs');
        const os = require('os');
        const path = require('path');
        const buffer = require('buffer');
        const file = path.join(os.tmpdir(), `lumen-blob-${process.pid}-${Date.now()}.txt`);
        fs.writeFileSync(file, 'hello file');
        (async () => {
          const blob = await fs.openAsBlob(file, { type: 'Text/Plain' });
          console.log(blob instanceof Blob, blob.size, blob.type);
          console.log(await blob.slice(6, 10).text());
          console.log((await blob.arrayBuffer()).byteLength);
          const url = URL.createObjectURL(blob);
          const resolved = buffer.resolveObjectURL(url);
          console.log(resolved instanceof Blob, resolved.size, resolved.type, await resolved.text());
          try { structuredClone(blob); } catch (error) { console.log(error.code); }
          const reader = blob.slice(0, 5).stream().getReader();
          console.log(Buffer.from((await reader.read()).value).toString());
          console.log(new Blob([blob, '!']).size);
          const memory = new Blob(['abc'], { type: 'a/b' });
          const copy = structuredClone(new File([memory], 'n.txt', { lastModified: 9 }));
          console.log(copy instanceof File, copy.name, copy.lastModified, copy.size);
          URL.revokeObjectURL(url);
          console.log(buffer.resolveObjectURL(url));
          fs.writeFileSync(file, 'a different, longer body');
          try { await blob.text(); } catch (error) { console.log(error.name); }
          fs.unlinkSync(file);
        })().catch((error) => console.log('failed', error));
        "#,
    );
    assert!(err.lines().is_empty(), "stderr: {:?}", err.lines());
    assert_eq!(
        out.lines(),
        [
            "true 10 text/plain",
            "file",
            "10",
            "true 10 text/plain hello file",
            "ERR_INVALID_STATE",
            "hello",
            "11",
            "true n.txt 9 3",
            "undefined",
            "NotReadableError",
        ]
    );
}

#[test]
fn native_event_target_follows_node_event_target_semantics() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const events = require("events");
        const failures = [];
        const check = (ok, name) => { if (!ok) failures.push(name); };

        const target = new EventTarget();
        const first = () => {};
        const second = { handleEvent() {} };
        target.addEventListener("a", first);
        target.addEventListener("a", second);
        target.addEventListener("b", first);
        const listeners = events.getEventListeners(target, "a");
        check(listeners.length === 2 && listeners[0] === first && listeners[1] === second, "listeners");

        check(target[Symbol.for("lumen.kEvents")] instanceof Map &&
            target[Symbol.for("lumen.kEvents")].size === 2, "kEvents-snapshot");

        const warnings = [];
        process.on("warning", (warning) => { warnings.push(warning); });
        events.setMaxListeners(1, target);
        target.addEventListener("c", () => {});
        target.addEventListener("c", () => {});

        const trusted = new Event("t");
        check(trusted.isTrusted === false, "untrusted");
        check(Object.keys(Object.getOwnPropertyDescriptors(trusted)).includes("isTrusted"), "own-isTrusted");

        target.addEventListener("d", null);
        check(events.getEventListeners(target, "d").length === 0, "null-listener-ignored");

        let reported = null;
        process.once("uncaughtException", (error) => { reported = error; });
        const failing = new EventTarget();
        const boom = new Error("listener failure");
        failing.addEventListener("x", () => { throw boom; });
        let afterFailure = false;
        failing.addEventListener("x", () => { afterFailure = true; });
        check(failing.dispatchEvent(new Event("x")) === true && afterFailure, "failing-listener-does-not-stop-dispatch");

        const signal = AbortSignal.abort();
        check(signal.reason instanceof DOMException && signal.reason.name === "AbortError", "abort-reason");
        check(typeof events.once === "function", "once");

        process.nextTick(() => {
            setImmediate(() => {
                const warned = warnings.find((w) => w.name === "MaxListenersExceededWarning");
                const nullWarning = warnings.find((w) => w.name === "AddEventListenerArgumentTypeWarning");
                check(warned && warned.name === "MaxListenersExceededWarning" && warned.count === 2 && warned.type === "c", "max-listeners-warning");
                check(nullWarning && nullWarning.name === "AddEventListenerArgumentTypeWarning", "null-warning");
                check(reported === boom, "reported-through-uncaughtException");
                console.log(failures.length === 0 ? "ok" : failures.join("|"));
            });
        });
        "#,
    );
    rt.run_to_completion();
    assert_eq!(out.lines(), ["ok"]);
}
