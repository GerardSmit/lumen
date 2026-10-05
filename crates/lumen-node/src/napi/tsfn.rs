//! Native producers enqueue opaque payloads; only the owning event loop touches JS. A pending
//! TaskRegistry entry is the wakeup/ref token, so callbacks need no polling or relay threads.
use super::*;
use lumen_host::{CompletionSender, TaskId, TaskRegistry};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

type CallJs = unsafe extern "C" fn(napi_env, napi_value, *mut c_void, *mut c_void);
type Finalize = unsafe extern "C" fn(napi_env, *mut c_void, *mut c_void);
const CLOSING: napi_status = 16;
const QUEUE_FULL: napi_status = 15;
thread_local! {static ON_LOOP:std::cell::Cell<bool>=const {std::cell::Cell::new(false)};}
pub(super) struct LoopGuard(bool);
impl LoopGuard {
    pub(super) fn enter() -> Self {
        Self(ON_LOOP.with(|slot| slot.replace(true)))
    }
}
impl Drop for LoopGuard {
    fn drop(&mut self) {
        ON_LOOP.with(|slot| slot.set(self.0));
    }
}
struct Queue {
    data: VecDeque<usize>,
    threads: usize,
    closing: bool,
    abort: bool,
    task: Option<TaskId>,
    woken: bool,
    referenced: bool,
}
struct Shared {
    queue: Mutex<Queue>,
    available: Condvar,
    sender: CompletionSender,
    capacity: usize,
    context: usize,
}
impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn wake(&self, queue: &mut Queue) {
        if !queue.woken {
            if let Some(id) = queue.task {
                queue.woken = true;
                self.sender.send(id, Box::new(()));
            }
        }
    }
}
pub(super) struct Main {
    shared: Arc<Shared>,
    callback: std::cell::RefCell<Value>,
    call_js: Option<CallJs>,
    finalize: Option<Finalize>,
    final_data: usize,
    finished: std::cell::Cell<bool>,
    handle: usize,
}
fn handles() -> &'static Mutex<HashMap<usize, Arc<Shared>>> {
    static TABLE: OnceLock<Mutex<HashMap<usize, Arc<Shared>>>> = OnceLock::new();
    TABLE.get_or_init(Default::default)
}
fn find(handle: napi_threadsafe_function) -> Option<Arc<Shared>> {
    handles()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(handle as usize))
        .cloned()
}
fn decode(_ctx: &mut Ctx, _payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    Ok(vec![])
}
fn arm(ctx: &mut Ctx, main: Rc<Main>) {
    let next = main.clone();
    let callback = ctx.new_native_fn(
        "nativeThreadsafeDelivery",
        0,
        Rc::new(move |ctx: &mut Ctx, _this: Value, _args: &[Value]| {
            pump(ctx, next.clone());
            Ok(Value::Undefined)
        }),
    );
    let id = lumen_host::register_task(ctx, callback, None, decode);
    let registry = ctx
        .host_mut::<TaskRegistry>()
        .expect("native callback task registry");
    let mut queue = main.shared.lock();
    if !queue.referenced {
        registry.set_unref(id);
    }
    queue.task = Some(id);
    queue.woken = false;
    if !queue.data.is_empty() || queue.closing && queue.threads == 0 {
        main.shared.wake(&mut queue);
    }
}
fn finish(ctx: Option<&mut Ctx>, main: &Main) {
    if main.finished.replace(true) {
        return;
    }
    handles()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&main.handle);
    let (remaining, task, threads) = {
        let mut queue = main.shared.lock();
        queue.closing = true;
        queue.abort = true;
        (
            queue.data.drain(..).collect::<Vec<_>>(),
            queue.task.take(),
            queue.threads,
        )
    };
    main.shared.available.notify_all();
    if let Some(call) = main.call_js {
        for data in remaining {
            unsafe {
                call(
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    main.shared.context as *mut c_void,
                    data as *mut c_void,
                )
            };
        }
    }
    if let Some(ctx) = ctx {
        if let Some(task) = task {
            ctx.host_mut::<TaskRegistry>().map(|r| r.take(task));
        }
        // A producer that outlives realm teardown still owns its native context. Do not
        // free it under that thread: its finalizer remains unreclaimed rather than unsafe.
        if let Some(finalize) = main.finalize.filter(|_| threads == 0) {
            let call = EnvCall::new(ctx);
            unsafe {
                finalize(
                    call.env,
                    main.final_data as *mut c_void,
                    main.shared.context as *mut c_void,
                )
            };
        }
    }
    *main.callback.borrow_mut() = Value::Undefined;
}
fn pump(ctx: &mut Ctx, main: Rc<Main>) {
    let _loop_guard = LoopGuard::enter();
    if main.finished.get() {
        return;
    }
    {
        let mut queue = main.shared.lock();
        queue.task = None;
        queue.woken = false;
    }
    for _ in 0..64 {
        let (data, abort) = {
            let mut queue = main.shared.lock();
            (queue.data.pop_front(), queue.abort)
        };
        let Some(data) = data else {
            break;
        };
        main.shared.available.notify_all();
        if abort {
            if let Some(call) = main.call_js {
                unsafe {
                    call(
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        main.shared.context as *mut c_void,
                        data as *mut c_void,
                    )
                };
            }
        } else {
            let scope = EnvCall::new(ctx);
            let callback = main.callback.borrow().clone();
            let value = unsafe { (*scope.env).handle(callback) };
            if let Some(call) = main.call_js {
                unsafe {
                    call(
                        scope.env,
                        value,
                        main.shared.context as *mut c_void,
                        data as *mut c_void,
                    )
                };
            } else {
                let value = value_of_safe(value);
                if let Err(error) = ctx.invoke(value, Value::Undefined, &[]) {
                    unsafe { (*scope.env).pending = Some(error) };
                }
            }
            if let Some(error) = unsafe { (*scope.env).pending.take() } {
                let thrower = ctx.new_native_fn(
                    "nativeCallbackError",
                    0,
                    Rc::new(
                        move |_ctx: &mut Ctx, _this: Value, _args: &[Value]| Err(error.clone()),
                    ),
                );
                lumen_host::CallbackQueue::enqueue(ctx.op_state(), thrower, vec![]);
            }
        }
    }
    let done = {
        let queue = main.shared.lock();
        queue.closing && queue.threads == 0 && queue.data.is_empty()
    };
    if done {
        finish(Some(ctx), &main);
        unsafe {
            let env = Env::new(ctx);
            env.napi_state().tsfns.remove(&main.handle);
        }
    } else {
        arm(ctx, main);
    }
}
fn value_of_safe(value: napi_value) -> Value {
    unsafe { value_of(value) }
}

#[no_mangle]
pub unsafe extern "C" fn napi_create_threadsafe_function(
    env: napi_env,
    func: napi_value,
    _resource: napi_value,
    _name: napi_value,
    max_queue_size: usize,
    initial_thread_count: usize,
    final_data: *mut c_void,
    finalize: *mut c_void,
    context: *mut c_void,
    call_js: *mut c_void,
    result: *mut napi_threadsafe_function,
) -> napi_status {
    if env.is_null()
        || result.is_null()
        || initial_thread_count == 0
        || initial_thread_count > 16384
    {
        return NAPI_INVALID_ARG;
    }
    *result = std::ptr::null_mut();
    let env = &mut *env;
    if env.napi_state().shutting_down {
        return CLOSING;
    }
    if env.napi_state().tsfns.len() >= 128 || max_queue_size > 4096 {
        return 9;
    }
    let callback = value_of(func);
    if call_js.is_null() && callback.type_of() != "function" {
        return NAPI_INVALID_ARG;
    }
    let Some(sender) = env.interp().host_mut::<CompletionSender>().cloned() else {
        return 9;
    };
    if env.interp().host_mut::<TaskRegistry>().is_none() {
        return 9;
    }
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    let handle = NEXT.fetch_add(1, Ordering::Relaxed);
    let shared = Arc::new(Shared {
        queue: Mutex::new(Queue {
            data: VecDeque::new(),
            threads: initial_thread_count,
            closing: false,
            abort: false,
            task: None,
            woken: false,
            referenced: true,
        }),
        available: Condvar::new(),
        sender,
        capacity: if max_queue_size == 0 {
            4096
        } else {
            max_queue_size
        },
        context: context as usize,
    });
    let main = Rc::new(Main {
        shared: shared.clone(),
        callback: std::cell::RefCell::new(callback),
        call_js: if call_js.is_null() {
            None
        } else {
            Some(std::mem::transmute::<*mut c_void, CallJs>(call_js))
        },
        finalize: if finalize.is_null() {
            None
        } else {
            Some(std::mem::transmute::<*mut c_void, Finalize>(finalize))
        },
        final_data: final_data as usize,
        finished: std::cell::Cell::new(false),
        handle,
    });
    handles()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(handle, shared);
    env.napi_state().tsfns.insert(handle, main.clone());
    arm(env.interp(), main);
    *result = handle as napi_threadsafe_function;
    NAPI_OK
}
#[no_mangle]
pub unsafe extern "C" fn napi_call_threadsafe_function(
    handle: napi_threadsafe_function,
    data: *mut c_void,
    mode: c_int,
) -> napi_status {
    if mode != 0 && mode != 1 {
        return NAPI_INVALID_ARG;
    }
    let Some(shared) = find(handle) else {
        return CLOSING;
    };
    let mut queue = shared.lock();
    while queue.data.len() >= shared.capacity && !queue.closing {
        if mode == 0 {
            return QUEUE_FULL;
        }
        if ON_LOOP.with(|slot| slot.get()) {
            return 21;
        } // napi_would_deadlock
        queue = shared
            .available
            .wait(queue)
            .unwrap_or_else(|e| e.into_inner());
    }
    if queue.closing {
        // napi_closing retires this caller's acquisition; the caller must never use the
        // handle again (including release). Otherwise abort can retain a referenced queue.
        if queue.threads > 0 {
            queue.threads -= 1;
            if queue.threads == 0 {
                shared.wake(&mut queue);
            }
        }
        shared.available.notify_all();
        return CLOSING;
    }
    queue.data.push_back(data as usize);
    shared.wake(&mut queue);
    NAPI_OK
}
#[no_mangle]
pub unsafe extern "C" fn napi_acquire_threadsafe_function(
    handle: napi_threadsafe_function,
) -> napi_status {
    let Some(shared) = find(handle) else {
        return CLOSING;
    };
    let mut queue = shared.lock();
    if queue.closing {
        return CLOSING;
    }
    if queue.threads >= 16384 {
        return 9;
    }
    queue.threads += 1;
    NAPI_OK
}
#[no_mangle]
pub unsafe extern "C" fn napi_release_threadsafe_function(
    handle: napi_threadsafe_function,
    mode: c_int,
) -> napi_status {
    if mode != 0 && mode != 1 {
        return NAPI_INVALID_ARG;
    }
    let Some(shared) = find(handle) else {
        return CLOSING;
    };
    let mut queue = shared.lock();
    if queue.threads == 0 {
        return CLOSING;
    }
    queue.threads -= 1;
    if mode == 1 {
        queue.abort = true;
        queue.closing = true;
    }
    if queue.threads == 0 {
        queue.closing = true;
    }
    shared.available.notify_all();
    shared.wake(&mut queue);
    NAPI_OK
}
#[no_mangle]
pub unsafe extern "C" fn napi_get_threadsafe_function_context(
    handle: napi_threadsafe_function,
    result: *mut *mut c_void,
) -> napi_status {
    if result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let Some(shared) = find(handle) else {
        return CLOSING;
    };
    *result = shared.context as *mut c_void;
    NAPI_OK
}
unsafe fn change_ref(
    env: napi_env,
    handle: napi_threadsafe_function,
    referenced: bool,
) -> napi_status {
    if env.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = &mut *env;
    let Some(main) = env.napi_state().tsfns.get(&(handle as usize)).cloned() else {
        return NAPI_INVALID_ARG;
    };
    let mut queue = main.shared.lock();
    queue.referenced = referenced;
    if let Some(id) = queue.task {
        if let Some(registry) = env.interp().host_mut::<TaskRegistry>() {
            if referenced {
                registry.set_ref(id)
            } else {
                registry.set_unref(id)
            }
        }
    }
    NAPI_OK
}
#[no_mangle]
pub unsafe extern "C" fn napi_unref_threadsafe_function(
    env: napi_env,
    handle: napi_threadsafe_function,
) -> napi_status {
    change_ref(env, handle, false)
}
#[no_mangle]
pub unsafe extern "C" fn napi_ref_threadsafe_function(
    env: napi_env,
    handle: napi_threadsafe_function,
) -> napi_status {
    change_ref(env, handle, true)
}
pub(super) fn shutdown(ctx: &mut Ctx) {
    let mains = unsafe {
        let env = Env::new(ctx);
        env.napi_state().shutting_down = true;
        std::mem::take(&mut env.napi_state().tsfns)
    };
    for (_, main) in mains {
        finish(Some(ctx), &main);
    }
}
impl Drop for Main {
    fn drop(&mut self) {
        finish(None, self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    unsafe extern "C" fn delivered(
        env: napi_env,
        _callback: napi_value,
        context: *mut c_void,
        data: *mut c_void,
    ) {
        let log = &mut *(context as *mut Vec<usize>);
        log.push(data as usize + if env.is_null() { 100 } else { 0 });
    }
    unsafe extern "C" fn finalized(_env: napi_env, _data: *mut c_void, context: *mut c_void) {
        (*(context as *mut Vec<usize>)).push(99);
    }
    fn engine() -> (
        lumen_host::Engine,
        std::sync::mpsc::Receiver<lumen_host::TaskCompletion>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut engine = lumen_host::Engine::new();
        engine.ctx().op_state().put(CompletionSender::new(tx));
        engine.ctx().op_state().put(TaskRegistry::default());
        (engine, rx)
    }
    fn create(
        engine: &mut lumen_host::Engine,
        log: &mut Vec<usize>,
        capacity: usize,
    ) -> napi_threadsafe_function {
        let mut env = Env::new(engine.ctx());
        let mut handle = std::ptr::null_mut();
        assert_eq!(
            unsafe {
                napi_create_threadsafe_function(
                    &mut env,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    capacity,
                    1,
                    std::ptr::null_mut(),
                    finalized as *const () as *mut c_void,
                    log as *mut _ as *mut c_void,
                    delivered as *const () as *mut c_void,
                    &mut handle,
                )
            },
            NAPI_OK
        );
        handle
    }
    fn tick(
        engine: &mut lumen_host::Engine,
        rx: &std::sync::mpsc::Receiver<lumen_host::TaskCompletion>,
    ) {
        let completion = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("native wakeup");
        let task = engine
            .ctx()
            .host_mut::<TaskRegistry>()
            .unwrap()
            .take(completion.task)
            .expect("pending callback");
        let args = (task.decode)(engine.ctx(), completion.result)
            .ok()
            .expect("decode");
        engine
            .ctx()
            .invoke(task.on_ok, Value::Undefined, &args)
            .ok()
            .expect("deliver");
    }
    #[test]
    fn producer_delivery_backpressure_refs_and_normal_finalization_use_owner_loop() {
        let (mut engine, rx) = engine();
        let mut log = Vec::new();
        let handle = create(&mut engine, &mut log, 1);
        let numeric = handle as usize;
        let producer = std::thread::spawn(move || unsafe {
            assert_eq!(
                napi_call_threadsafe_function(numeric as _, 1usize as _, 0),
                NAPI_OK
            );
            assert_eq!(
                napi_call_threadsafe_function(numeric as _, 2usize as _, 0),
                QUEUE_FULL
            );
        });
        producer.join().unwrap();
        let mut env = Env::new(engine.ctx());
        assert_eq!(
            unsafe { napi_unref_threadsafe_function(&mut env, handle) },
            NAPI_OK
        );
        assert!(!engine
            .ctx()
            .host_mut::<TaskRegistry>()
            .unwrap()
            .has_ref_pending());
        assert_eq!(
            unsafe { napi_ref_threadsafe_function(&mut env, handle) },
            NAPI_OK
        );
        assert!(engine
            .ctx()
            .host_mut::<TaskRegistry>()
            .unwrap()
            .has_ref_pending());
        tick(&mut engine, &rx);
        assert_eq!(log, [1]);
        assert_eq!(
            unsafe { napi_release_threadsafe_function(handle, 0) },
            NAPI_OK
        );
        tick(&mut engine, &rx);
        assert_eq!(log, [1, 99]);
        assert_eq!(
            unsafe { napi_call_threadsafe_function(handle, 3usize as _, 0) },
            CLOSING
        );
        assert!(engine.ctx().host_mut::<TaskRegistry>().unwrap().is_empty());
    }
    #[test]
    fn abort_discards_queued_payloads_but_waits_for_all_producers_before_finalizing() {
        let (mut engine, rx) = engine();
        let mut log = Vec::new();
        let handle = create(&mut engine, &mut log, 1);
        assert_eq!(unsafe { napi_acquire_threadsafe_function(handle) }, NAPI_OK);
        assert_eq!(
            unsafe { napi_call_threadsafe_function(handle, 1usize as _, 0) },
            NAPI_OK
        );
        assert_eq!(
            unsafe { napi_release_threadsafe_function(handle, 1) },
            NAPI_OK
        );
        tick(&mut engine, &rx);
        assert_eq!(log, [101]);
        assert!(rx.try_recv().is_err());
        assert_eq!(
            unsafe { napi_release_threadsafe_function(handle, 0) },
            NAPI_OK
        );
        tick(&mut engine, &rx);
        assert_eq!(log, [101, 99]);
    }
    #[test]
    fn closing_call_retires_remaining_producer_without_an_extra_release() {
        let (mut engine, rx) = engine();
        let mut log = Vec::new();
        let handle = create(&mut engine, &mut log, 1);
        assert_eq!(unsafe { napi_acquire_threadsafe_function(handle) }, NAPI_OK);
        assert_eq!(
            unsafe { napi_release_threadsafe_function(handle, 1) },
            NAPI_OK
        );
        tick(&mut engine, &rx);
        assert!(log.is_empty());
        let numeric = handle as usize;
        std::thread::spawn(move || {
            assert_eq!(
                unsafe { napi_call_threadsafe_function(numeric as _, 1usize as _, 0) },
                CLOSING
            );
            // Node-API forbids an additional release after napi_closing.
        })
        .join()
        .unwrap();
        tick(&mut engine, &rx);
        assert_eq!(log, [99]);
        assert!(engine.ctx().host_mut::<TaskRegistry>().unwrap().is_empty());
    }

    #[test]
    fn blocking_native_producer_wakes_after_loop_drains_without_touching_js_off_thread() {
        let (mut engine, rx) = engine();
        let mut log = Vec::new();
        let handle = create(&mut engine, &mut log, 1);
        let numeric = handle as usize;
        let producer = std::thread::spawn(move || unsafe {
            assert_eq!(
                napi_call_threadsafe_function(numeric as _, 1usize as _, 0),
                NAPI_OK
            );
            assert_eq!(
                napi_call_threadsafe_function(numeric as _, 2usize as _, 1),
                NAPI_OK
            );
            assert_eq!(napi_release_threadsafe_function(numeric as _, 0), NAPI_OK);
        });
        while !log.contains(&99) {
            tick(&mut engine, &rx);
        }
        producer.join().unwrap();
        assert_eq!(log, [1, 2, 99]);
    }
}
