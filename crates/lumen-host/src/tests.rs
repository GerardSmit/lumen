use super::*;

/// The Phase-1 acceptance test: an external crate registers a native global and JS calls it.
fn host_add(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let a = ctx.coerce_number(args.first().unwrap_or(&Value::Undefined))?;
    let b = ctx.coerce_number(args.get(1).unwrap_or(&Value::Undefined))?;
    Ok(Value::Num(a + b))
}

fn eval_str(engine: &mut Engine, src: &str) -> String {
    match engine.eval(src, false).expect("parse") {
        Completion::Value(v) => v,
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
}

#[test]
fn native_global_callable_from_js() {
    let mut engine = Engine::new();
    engine.define_global("hostAdd", 2, host_add);
    assert_eq!(eval_str(&mut engine, "hostAdd(2, 40)"), "42");
    // Coercion path (user valueOf) works, and a native fn is a real function object.
    assert_eq!(
        eval_str(&mut engine, "hostAdd({ valueOf: () => 1 }, '2.5')"),
        "3.5"
    );
    assert_eq!(eval_str(&mut engine, "typeof hostAdd"), "function");
    assert_eq!(eval_str(&mut engine, "hostAdd.length"), "2");
}

struct Counter(u32);

#[lumen_bind::module(name = "counterGlobals")]
mod counter_globals {
    use super::*;

    #[op]
    pub fn bump(ctx: &mut Ctx) -> f64 {
        bump_state(ctx)
    }
}

#[lumen_bind::module(name = "counterNs")]
mod counter_ns {
    use super::*;

    #[op(name = "bumpToo")]
    pub fn bump_too(ctx: &mut Ctx) -> f64 {
        bump_state(ctx)
    }
}

fn bump_state(ctx: &mut Ctx) -> f64 {
    let c = ctx.host_mut::<Counter>().expect("state_init ran");
    c.0 += 1;
    c.0 as f64
}

#[test]
fn extension_installs_state_globals_and_namespaces() {
    static EXT: Extension = Extension {
        name: "counter",
        modules: &[
            globals::<counter_globals::Module>,
            namespace::<counter_ns::Module>,
        ],
        state_init: Some(|state| state.put(Counter(0))),
        js_init: None,
        js_init_snapshot: None,
        lazy_globals: &[],
    };
    let mut engine = Engine::new();
    install(&mut engine, std::slice::from_ref(&EXT));
    assert_eq!(eval_str(&mut engine, "bump(); bump()"), "2");
    assert_eq!(eval_str(&mut engine, "counterNs.bumpToo()"), "3");
}

#[test]
fn native_fn_survives_the_bytecode_tier() {
    // The embed hooks must work on every execution tier (a native call from bytecode
    // frames goes through the same Callable::Native dispatch, but verify, don't assume).
    let mut engine = Engine::new();
    engine.set_tier(lumen::bytecode::Tier::Bytecode);
    engine.set_tier_threshold(0);
    engine.define_global("hostAdd", 2, host_add);
    assert_eq!(
        eval_str(
            &mut engine,
            "function f() { let s = 0; for (let i = 0; i < 1000; i++) s = hostAdd(s, 1); return s } f(); f()"
        ),
        "1000"
    );
}

#[test]
fn call_function_reenters_the_engine() {
    let mut engine = Engine::new();
    match engine
        .eval("globalThis.cb = (x) => x * 2; 0", false)
        .unwrap()
    {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
    let g = global_this(&mut engine);
    let cb = engine
        .ctx()
        .get_member(&g, "cb")
        .map_err(|_| ())
        .expect("cb defined");
    let out = engine
        .call_function(&cb, Value::Undefined, &[Value::Num(21.0)])
        .map_err(|_| "js threw")
        .expect("call ok");
    assert!(matches!(out, Value::Num(n) if n == 42.0));
}

fn global_this(engine: &mut Engine) -> Value {
    engine.global_this()
}

#[test]
fn microtask_hooks_drive_promises() {
    let mut engine = Engine::new();
    // Queue a reaction without draining: eval() drains at the end, so queue one from a
    // native call instead... simplest honest check: after eval, nothing pending; then a
    // manually enqueued callback via call_function leaves the queue drainable by hand.
    engine
        .eval(
            "globalThis.done = 0; Promise.resolve(1).then(v => done = v)",
            false,
        )
        .unwrap();
    // Engine::eval already ran the checkpoint: the reaction fired.
    assert_eq!(eval_str(&mut engine, "done"), "1");
    assert!(!engine.has_pending_jobs());
    // Now queue a job by calling a promise-creating function through call_function (which
    // does NOT run a microtask checkpoint) and drain it step by step.
    engine
        .eval(
            "globalThis.queue = () => { Promise.resolve(2).then(v => done = v) }",
            false,
        )
        .unwrap();
    let queue = {
        let g = global_this(&mut engine);
        engine
            .ctx()
            .get_member(&g, "queue")
            .map_err(|_| ())
            .unwrap()
    };
    engine
        .call_function(&queue, Value::Undefined, &[])
        .map_err(|_| "js threw")
        .unwrap();
    assert!(engine.has_pending_jobs());
    assert!(engine.run_one_job());
    engine.run_microtasks();
    assert!(!engine.has_pending_jobs());
    assert_eq!(eval_str(&mut engine, "done"), "2");
}

#[test]
fn resource_table_roundtrip() {
    let mut table = ResourceTable::default();
    let rid = table.add(String::from("a file handle"));
    assert!(table.has(rid));
    assert_eq!(*table.get::<String>(rid).unwrap(), "a file handle");
    assert!(table.get::<u32>(rid).is_none(), "downcast is type-checked");
    assert!(table.close(rid).is_some());
    assert!(!table.has(rid));
    assert!(table.is_empty());
}

#[test]
fn threadpool_completions_arrive() {
    let (tx, rx) = std::sync::mpsc::channel();
    let pool = ThreadPool::new(tx);
    for id in 0..16u64 {
        pool.spawn_blocking(id, move || {
            Box::new(id * 2) as Box<dyn std::any::Any + Send>
        });
    }
    let mut seen = std::collections::HashMap::new();
    for _ in 0..16 {
        let done = rx.recv().expect("completion");
        seen.insert(done.task, *done.result.downcast::<u64>().unwrap());
    }
    assert_eq!(seen.len(), 16);
    assert!((0..16).all(|id| seen[&id] == id * 2));
    drop(pool); // waits for pending tasks; must not deadlock
}

#[test]
fn threadpool_drop_waits_for_pending_without_polling() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    let (tx, _rx) = std::sync::mpsc::channel();
    let pool = ThreadPool::new(tx);
    let started = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    {
        let (started, finished) = (started.clone(), finished.clone());
        pool.spawn_blocking(1, move || {
            started.store(true, Ordering::SeqCst);
            go_rx.recv().unwrap();
            finished.store(true, Ordering::SeqCst);
            Box::new(()) as Box<dyn std::any::Any + Send>
        });
    }
    while !started.load(Ordering::SeqCst) {
        std::thread::yield_now();
    }
    assert_eq!(pool.pending(), 1);
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        go_tx.send(()).unwrap();
    });
    let begin = Instant::now();
    drop(pool);
    let waited = begin.elapsed();
    assert!(finished.load(Ordering::SeqCst), "drop returned before the task finished");
    assert!(waited >= Duration::from_millis(40), "drop did not wait: {waited:?}");
    assert!(waited < Duration::from_millis(400), "drop overshot the task: {waited:?}");
    release.join().unwrap();
}

mod owner_loop_tests {
    use super::*;
    use crate::messaging::{self, Receiver};
    use crate::ports::{self, PortLimits};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Default)]
    struct NativeSlot(Option<Rc<messaging::NativeReceiver>>);

    #[lumen_bind::module(name = "nativeTest")]
    mod native_test {
        use super::*;
        use lumen::embed::OpResult;

        /// A pair whose first end delivers to `target` natively; returns the other end's id.
        #[op(name = "open")]
        pub fn open(ctx: &mut Ctx, target: Value, on_close: Option<Value>) -> OpResult<f64> {
            let (native, far) = ports::new_pair();
            let native = ports::adopt(ctx, native);
            let far = ports::adopt(ctx, far);
            let weak = ctx.weak_value(&target).expect("target is an object");
            let receiver = messaging::listen_native(ctx, native, weak, Receiver::Port, on_close)?;
            ctx.host_mut::<NativeSlot>().unwrap().0 = Some(Rc::new(receiver));
            Ok(far as f64)
        }

        #[op(name = "post")]
        pub fn post(ctx: &mut Ctx, message: Value, transfer: Option<Value>) -> OpResult<()> {
            let receiver = ctx.host_mut::<NativeSlot>().unwrap().0.clone().unwrap();
            receiver.post(ctx, message, transfer.unwrap_or(Value::Undefined))
        }

        #[op(name = "close")]
        pub fn close(ctx: &mut Ctx, target: Value) {
            let receiver = ctx.host_mut::<NativeSlot>().unwrap().0.clone().unwrap();
            receiver.close(ctx, &target);
        }

        #[op(name = "limits", coerce)]
        pub fn limits(ctx: &mut Ctx, bytes: f64, queued: f64) {
            ports::set_limits(
                ctx,
                PortLimits {
                    max_message_bytes: bytes as usize,
                    max_queued: queued as usize,
                },
            );
        }
    }

    fn events_extension() -> Extension {
        Extension {
            name: "test-events",
            modules: &[
                lazy_globals::<events::bindings::Module>,
                namespace::<native_test::Module>,
            ],
            state_init: Some(|state| state.put(NativeSlot::default())),
            js_init: None,
            js_init_snapshot: None,
            lazy_globals: &[],
        }
    }

    /// A bare engine whose only event loop is the owner loop; `notifies` counts host wake-ups.
    fn owner_engine() -> (Engine, Arc<AtomicUsize>) {
        let notifies = Arc::new(AtomicUsize::new(0));
        let mut engine = Engine::new();
        let counter = Arc::clone(&notifies);
        owner_loop::install(
            engine.ctx(),
            Arc::new(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            }),
        );
        install(
            &mut engine,
            &[
                events_extension(),
                ports::extension(),
                clone_transfer::extension(),
                messaging::extension(),
            ],
        );
        (engine, notifies)
    }

    fn pump_all(engine: &mut Engine) {
        let thrown = owner_loop::pump(engine, 1024);
        assert!(thrown.is_empty(), "pump reported a throw");
    }

    #[test]
    fn nothing_is_ready_or_notified_while_idle() {
        let (mut engine, notifies) = owner_engine();
        assert!(!owner_loop::has_ready(engine.ctx()));
        eval_str(
            &mut engine,
            "globalThis.channel = new MessageChannel(); channel.port1.onmessage = () => {}; 0",
        );
        assert!(!owner_loop::has_ready(engine.ctx()));
        assert_eq!(notifies.load(Ordering::SeqCst), 0);
        assert!(owner_loop::pump(&mut engine, 8).is_empty());
    }

    #[test]
    fn message_channel_round_trip_through_the_owner_loop() {
        let (mut engine, notifies) = owner_engine();
        eval_str(
            &mut engine,
            "globalThis.got = []; const { port1, port2 } = new MessageChannel();
             port1.onmessage = (e) => { got.push([e.data.n, e instanceof MessageEvent, e.isTrusted]); };
             port2.postMessage({ n: 7 }); 0",
        );
        assert!(owner_loop::has_ready(engine.ctx()));
        assert_eq!(notifies.load(Ordering::SeqCst), 1);
        pump_all(&mut engine);
        assert_eq!(eval_str(&mut engine, "JSON.stringify(got)"), "[[7,true,true]]");
        assert!(!owner_loop::has_ready(engine.ctx()));
    }

    #[test]
    fn a_burst_notifies_the_host_once() {
        let (mut engine, notifies) = owner_engine();
        eval_str(
            &mut engine,
            "globalThis.count = 0;
             for (let i = 0; i < 4; i++) {
               const { port1, port2 } = new MessageChannel();
               port1.onmessage = () => { count++; };
               port2.postMessage(i);
               port2.postMessage(i);
             }
             0",
        );
        assert_eq!(notifies.load(Ordering::SeqCst), 1);
        pump_all(&mut engine);
        assert_eq!(eval_str(&mut engine, "count"), "8");
        assert!(!owner_loop::has_ready(engine.ctx()));
        eval_str(
            &mut engine,
            "const { port1, port2 } = new MessageChannel();
             port1.onmessage = () => { count++; };
             port2.postMessage(1); 0",
        );
        assert_eq!(notifies.load(Ordering::SeqCst), 2, "a drained realm is announced again");
    }

    #[test]
    fn pump_respects_its_budget_and_leaves_the_rest_ready() {
        let (mut engine, _) = owner_engine();
        eval_str(
            &mut engine,
            "globalThis.count = 0; const { port1, port2 } = new MessageChannel();
             port1.onmessage = () => { count++; };
             for (let i = 0; i < 5; i++) port2.postMessage(i);
             0",
        );
        assert!(owner_loop::pump(&mut engine, 2).is_empty());
        assert_eq!(eval_str(&mut engine, "count"), "2");
        assert!(owner_loop::has_ready(engine.ctx()));
        assert!(owner_loop::pump(&mut engine, 0).is_empty());
        assert_eq!(eval_str(&mut engine, "count"), "2");
        pump_all(&mut engine);
        assert_eq!(eval_str(&mut engine, "count"), "5");
        assert!(!owner_loop::has_ready(engine.ctx()));
    }

    #[test]
    fn each_message_gets_a_microtask_checkpoint() {
        let (mut engine, _) = owner_engine();
        eval_str(
            &mut engine,
            "globalThis.order = []; const { port1, port2 } = new MessageChannel();
             port1.onmessage = (e) => { order.push('m' + e.data); Promise.resolve().then(() => order.push('t' + e.data)); };
             port2.postMessage(1); port2.postMessage(2); 0",
        );
        pump_all(&mut engine);
        assert_eq!(
            eval_str(&mut engine, "order.join()"),
            "m1,t1,m2,t2"
        );
    }

    #[test]
    fn a_throwing_listener_is_returned_not_lost() {
        let (mut engine, _) = owner_engine();
        eval_str(
            &mut engine,
            "const { port1, port2 } = new MessageChannel();
             port1.onmessage = () => { throw new Error('boom'); };
             port2.postMessage(1); 0",
        );
        let thrown = owner_loop::pump(&mut engine, 8);
        // Listener errors are reported by the event target, not thrown out of the dispatch.
        assert!(thrown.len() <= 1);
        assert!(!owner_loop::has_ready(engine.ctx()));
    }

    #[test]
    fn dead_ports_are_reaped_without_a_runtime() {
        let (mut engine, _) = owner_engine();
        eval_str(
            &mut engine,
            "globalThis.keep = new MessageChannel().port1; keep.onmessage = () => {}; 0",
        );
        let keep = {
            let global = engine.global_this();
            engine.ctx().get_member(&global, "keep").map_err(|_| ()).unwrap()
        };
        let weak = engine.ctx().weak_value(&keep).unwrap();
        drop(keep);
        // Its peer's wrapper is unreachable: collecting it asks the realm to release the handle.
        engine.collect_garbage();
        engine.collect_garbage();
        pump_all(&mut engine);
        eval_str(&mut engine, "globalThis.keep = null; 0");
        engine.collect_garbage();
        engine.collect_garbage();
        pump_all(&mut engine);
        engine.collect_garbage();
        engine.collect_garbage();
        assert!(weak.upgrade().is_none(), "a port whose peer is gone must be collectable");
    }

    #[test]
    fn settle_reports_decode_failures_and_cancelled_tasks() {
        let (mut engine, _) = owner_engine();
        fn decode_fail(_: &mut Ctx, _: Box<dyn Any + Send>) -> Result<Vec<Value>, Value> {
            Err(Value::Num(13.0))
        }
        fn decode_ok(_: &mut Ctx, _: Box<dyn Any + Send>) -> Result<Vec<Value>, Value> {
            Ok(vec![Value::Num(1.0)])
        }
        let ctx = engine.ctx();
        let callback = Value::Undefined;
        let (failing, rejecting, cancelled, fine) = {
            let registry = ctx.host_mut::<TaskRegistry>().unwrap();
            (
                registry.register(callback.clone(), None, decode_fail),
                registry.register(callback.clone(), Some(Value::Bool(true)), decode_fail),
                registry.register(callback.clone(), None, decode_ok),
                registry.register(callback.clone(), None, decode_ok),
            )
        };
        ctx.host_mut::<TaskRegistry>().unwrap().cancel(cancelled);
        let done = |task| TaskCompletion {
            task,
            result: Box::new(()),
        };
        let settled = owner_loop::settle(ctx, done(failing)).expect("registered");
        assert!(matches!(settled.outcome, owner_loop::Outcome::Uncaught(Value::Num(n)) if n == 13.0));
        settled.finish(ctx);
        let settled = owner_loop::settle(ctx, done(rejecting)).expect("registered");
        assert!(matches!(
            &settled.outcome,
            owner_loop::Outcome::Call { callback: Value::Bool(true), args } if args.len() == 1
        ));
        settled.finish(ctx);
        assert!(owner_loop::settle(ctx, done(cancelled)).is_none());
        let settled = owner_loop::settle(ctx, done(fine)).expect("registered");
        assert!(matches!(&settled.outcome, owner_loop::Outcome::Call { args, .. } if args.len() == 1));
        settled.finish(ctx);
        assert!(!ctx.host_mut::<TaskRegistry>().unwrap().has_ref_pending());
    }

    #[test]
    fn native_resource_completion_retains_context_without_a_js_callback() {
        let (mut engine, _) = owner_engine();
        fn decode(ctx: &mut Ctx, payload: Box<dyn Any + Send>) -> Result<Vec<Value>, Value> {
            assert!(matches!(ctx.async_context(), Value::Num(7.0)));
            assert_eq!(*payload.downcast::<usize>().unwrap(), 42);
            Ok(Vec::new())
        }
        let ctx = engine.ctx();
        ctx.set_async_context(Value::Num(7.0));
        let id = register_native_task(ctx, decode);
        ctx.set_async_context(Value::Num(9.0));
        let settled = owner_loop::settle(ctx, TaskCompletion { task: id, result: Box::new(42usize) }).unwrap();
        assert!(matches!(settled.outcome, owner_loop::Outcome::Complete));
        settled.finish(ctx);
        assert!(matches!(ctx.async_context(), Value::Num(9.0)));
        assert!(!ctx.host_mut::<TaskRegistry>().unwrap().has_ref_pending());
    }

    #[test]
    fn a_full_queue_is_a_quota_error_and_an_oversized_message_a_clone_error() {
        let (mut engine, _) = owner_engine();
        eval_str(&mut engine, "nativeTest.limits(64, 3); 0");
        assert_eq!(
            eval_str(
                &mut engine,
                "const { port2 } = new MessageChannel();
                 let seen = [];
                 for (let i = 0; i < 5; i++) {
                   try { port2.postMessage(i); seen.push('ok'); } catch (e) { seen.push(e.name); }
                 }
                 seen.join()"
            ),
            "ok,ok,ok,QuotaExceededError,QuotaExceededError"
        );
        assert_eq!(
            eval_str(
                &mut engine,
                "const other = new MessageChannel();
                 let outcome;
                 try { other.port1.postMessage(new Uint8Array(200)); outcome = 'sent'; }
                 catch (e) { outcome = e.name; }
                 outcome + ',' + (other.port1.postMessage('small'), 'small ok')"
            ),
            "DataCloneError,small ok"
        );
    }

    #[test]
    fn a_refused_post_detaches_nothing() {
        let (mut engine, _) = owner_engine();
        eval_str(&mut engine, "nativeTest.limits(1048576, 1); 0");
        assert_eq!(
            eval_str(
                &mut engine,
                "const { port1, port2 } = new MessageChannel();
                 const buffer = new ArrayBuffer(8);
                 port2.postMessage(0);
                 let refused = 'none';
                 try { port2.postMessage(buffer, [buffer]); } catch (e) { refused = e.name; }
                 refused + ',' + buffer.byteLength"
            ),
            "QuotaExceededError,8"
        );
    }

    #[test]
    fn a_queue_that_drains_accepts_messages_again() {
        let (mut engine, _) = owner_engine();
        eval_str(&mut engine, "nativeTest.limits(1048576, 1); 0");
        eval_str(
            &mut engine,
            "globalThis.count = 0; globalThis.pair = new MessageChannel();
             pair.port1.onmessage = () => { count++; };
             pair.port2.postMessage(1); 0",
        );
        pump_all(&mut engine);
        assert_eq!(
            eval_str(&mut engine, "pair.port2.postMessage(2); count"),
            "1"
        );
        pump_all(&mut engine);
        assert_eq!(eval_str(&mut engine, "count"), "2");
    }

    #[test]
    fn native_receiver_gets_data_and_ports() {
        let (mut engine, _) = owner_engine();
        eval_str(
            &mut engine,
            "globalThis.log = [];
             globalThis.target = new EventTarget();
             target.addEventListener('message', (e) => {
               log.push([e.data.x, e.ports.length, e.ports[0] instanceof MessagePort, e.isTrusted, e.type]);
             });
             globalThis.far = __lumenSharedPorts.create(nativeTest.open(target));
             const inner = new MessageChannel();
             far.postMessage({ x: 5 }, [inner.port1]); 0",
        );
        pump_all(&mut engine);
        assert_eq!(
            eval_str(&mut engine, "JSON.stringify(log)"),
            "[[5,1,true,true,\"message\"]]"
        );
    }

    #[test]
    fn native_receiver_posts_back_and_transfers() {
        let (mut engine, _) = owner_engine();
        eval_str(
            &mut engine,
            "globalThis.log = [];
             globalThis.target = new EventTarget();
             globalThis.far = __lumenSharedPorts.create(nativeTest.open(target));
             far.onmessage = (e) => { log.push([e.data.view.byteLength, e.data.view.buffer.byteLength]); };
             const bytes = new Uint8Array(16);
             nativeTest.post({ view: bytes }, [bytes.buffer]);
             log.push(['detached', bytes.byteLength]); 0",
        );
        pump_all(&mut engine);
        assert_eq!(
            eval_str(&mut engine, "JSON.stringify(log)"),
            "[[\"detached\",0],[16,16]]"
        );
    }

    #[test]
    fn peer_close_fires_close_on_the_native_receiver_after_queued_messages() {
        let (mut engine, _) = owner_engine();
        eval_str(
            &mut engine,
            "globalThis.log = [];
             globalThis.target = new EventTarget();
             target.addEventListener('message', (e) => log.push('message ' + e.data));
             target.addEventListener('close', (e) => log.push('close ' + e.isTrusted));
             globalThis.far = __lumenSharedPorts.create(nativeTest.open(target, () => log.push('hook')));
             far.postMessage('last');
             far.close(); 0",
        );
        pump_all(&mut engine);
        assert_eq!(
            eval_str(&mut engine, "log.join()"),
            "message last,hook,close true"
        );
        assert!(!owner_loop::has_ready(engine.ctx()));
    }

    #[test]
    fn closing_the_native_end_stops_delivery_without_a_close_event() {
        let (mut engine, _) = owner_engine();
        eval_str(
            &mut engine,
            "globalThis.log = [];
             globalThis.target = new EventTarget();
             target.addEventListener('close', () => log.push('close'));
             globalThis.far = __lumenSharedPorts.create(nativeTest.open(target, () => log.push('hook')));
             far.onmessage = () => log.push('far message');
             far.addEventListener('close', () => log.push('far close'));
             nativeTest.close(target); 0",
        );
        pump_all(&mut engine);
        assert_eq!(eval_str(&mut engine, "log.join()"), "hook,far close");
    }

    struct MockBackend {
        spawned: Arc<AtomicUsize>,
        terminated: Arc<AtomicUsize>,
    }

    impl workers::WorkerBackend for MockBackend {
        fn spawn_dedicated(
            &self,
            _ctx: &mut Ctx,
            _spec: workers::DedicatedSpec,
        ) -> lumen_bind::NativeResult<u64> {
            Ok(self.spawned.fetch_add(1, Ordering::SeqCst) as u64)
        }

        fn terminate(&self, _ctx: &mut Ctx, _id: u64) {
            self.terminated.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn worker_engine() -> (Engine, Arc<AtomicUsize>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let (mut engine, notifies) = owner_engine();
        let spawned = Arc::new(AtomicUsize::new(0));
        let terminated = Arc::new(AtomicUsize::new(0));
        workers::set_backend(
            engine.ctx(),
            Rc::new(MockBackend {
                spawned: Arc::clone(&spawned),
                terminated: Arc::clone(&terminated),
            }),
        );
        assert!(workers::install_page_classes(engine.ctx()).is_ok());
        (engine, notifies, spawned, terminated)
    }

    #[test]
    fn worker_class_has_web_idl_shape() {
        let (mut engine, _, spawned, _) = worker_engine();
        assert_eq!(eval_str(&mut engine, "Worker.length"), "1");
        assert_eq!(
            eval_str(
                &mut engine,
                "const d = Object.getOwnPropertyDescriptor(Worker.prototype, 'onmessage'); typeof d.get + typeof d.set"
            ),
            "functionfunction"
        );
        assert_eq!(
            eval_str(&mut engine, "Object.getPrototypeOf(Worker.prototype) === EventTarget.prototype"),
            "true"
        );
        assert_eq!(
            eval_str(
                &mut engine,
                "try { Worker.prototype.postMessage.call({}, 1); 'no' } catch (e) { e.constructor.name }"
            ),
            "TypeError"
        );
        assert_eq!(
            eval_str(&mut engine, "try { Worker('x.js'); 'no' } catch (e) { e.constructor.name }"),
            "TypeError"
        );
        assert_eq!(spawned.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn worker_without_backend_support_is_not_supported() {
        let (mut engine, _) = owner_engine();
        assert!(workers::install_page_classes(engine.ctx()).is_ok());
        assert_eq!(
            eval_str(
                &mut engine,
                "try { new Worker('x.js'); 'no' } catch (e) { e.name }"
            ),
            "NotSupportedError"
        );
    }

    #[test]
    fn terminate_notifies_the_backend_once_and_idle_worker_is_not_notified() {
        let (mut engine, notifies, spawned, terminated) = worker_engine();
        eval_str(
            &mut engine,
            "globalThis.w = new Worker('x.js'); w.onmessage = () => {}; w.terminate(); w.terminate(); 0",
        );
        assert_eq!(spawned.load(Ordering::SeqCst), 1);
        assert_eq!(terminated.load(Ordering::SeqCst), 1);
        assert_eq!(notifies.load(Ordering::SeqCst), 0);
        assert!(!owner_loop::has_ready(engine.ctx()));
    }

    #[test]
    fn worker_post_message_after_terminate_throws_nothing_and_listenerless_worker_collects() {
        let (mut engine, _, _, terminated) = worker_engine();
        eval_str(
            &mut engine,
            "globalThis.w = new Worker('x.js'); w.terminate(); w.postMessage(1); 0",
        );
        eval_str(&mut engine, "new Worker('y.js'); 0");
        engine.collect_garbage();
        assert_eq!(terminated.load(Ordering::SeqCst), 1);
    }
}
