//! `_thread` on the interpreter's threads (see `crate::threads`): locks queue the threads that
//! wait for them on `lumen_common::wait::Waiter`s and give the GIL up while they wait.

/// This module provides primitive operations to write multi-threaded programs.
/// The 'threading' module provides a more convenient interface.
#[lumen_bind::module(name = "_thread")]
pub mod _thread {
    use crate::bind::{type_object, KwArgs, Py, This};
    use crate::builtins::sysextra::structseq_type;
    use crate::object::*;
    use crate::vm::{dict_get_str, dict_set_str, Interp};
    use lumen_common::wait::Waiter;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::rc::{Rc, Weak};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// Longest timeout, in seconds, a lock accepts.
    const TIMEOUT_MAX: f64 = 9_223_372_036.0;

    /// How long a wait on the main thread goes without looking at signals.
    const SLICE: Duration = Duration::from_millis(10);

    fn ident() -> u64 {
        lumen_os::thread::ident()
    }

    /// `(blocking, timeout)` checked as CPython's `lock_acquire_parse_args`; the timeout is -1
    /// for none.
    fn acquire_args(it: &mut Interp, blocking: bool, timeout: Option<&Value>) -> R<(bool, f64)> {
        let timeout = match timeout {
            None => -1.0,
            Some(Value::Float(f)) => seconds(it, *f)?,
            Some(Value::Obj(o)) if matches!(o.kind, Kind::Float(_)) => match o.kind {
                Kind::Float(f) => seconds(it, f)?,
                _ => -1.0,
            },
            Some(v) => match it.index_of(v) {
                Ok(n) => n as f64,
                Err(e) if it.exc_is(&e, "OverflowError") => {
                    return Err(it.overflow_err("timestamp too large to convert to C _PyTime_t"))
                }
                Err(e) => return Err(e),
            },
        };
        if !blocking && timeout != -1.0 {
            return Err(it.value_error("can't specify a timeout for a non-blocking call"));
        }
        if timeout < 0.0 && timeout != -1.0 {
            return Err(it.value_error("timeout value must be positive"));
        }
        if !blocking {
            return Ok((false, 0.0));
        }
        if timeout > TIMEOUT_MAX {
            return Err(it.overflow_err("timeout value is too large"));
        }
        Ok((true, timeout))
    }

    fn seconds(it: &mut Interp, f: f64) -> R<f64> {
        if f.is_nan() {
            return Err(it.value_error("Invalid value NaN (not a number)"));
        }
        if f.is_infinite() && f > 0.0 {
            return Err(it.overflow_err("timestamp too large to convert to C _PyTime_t"));
        }
        Ok(f)
    }

    /// The state of a lock: whether it is held and the threads queued for it. Held in an `Rc` so a
    /// thread can wait on it without keeping its object borrowed.
    #[derive(Default)]
    pub struct LockCore {
        locked: Cell<bool>,
        waiters: RefCell<Vec<Arc<Waiter>>>,
    }

    impl LockCore {
        fn try_take(&self) -> bool {
            !self.locked.replace(true)
        }

        fn wake_one(&self) {
            let first = {
                let mut q = self.waiters.borrow_mut();
                if q.is_empty() {
                    None
                } else {
                    Some(q.remove(0))
                }
            };
            if let Some(w) = first {
                w.wake();
            }
        }

        /// Takes the lock, waiting up to `timeout` seconds (-1: for ever) when `blocking`.
        fn acquire(&self, it: &mut Interp, blocking: bool, timeout: f64) -> R<bool> {
            if self.try_take() {
                return Ok(true);
            }
            if !blocking {
                return Ok(false);
            }
            if timeout < 0.0 && !it.other_threads() {
                it.poll()?;
                if self.try_take() {
                    return Ok(true);
                }
                return Err(it.runtime_error("deadlock: lock is already held and there is no other thread to release it"));
            }
            let deadline = (timeout >= 0.0).then(|| Instant::now() + Duration::from_secs_f64(timeout));
            let main = it.is_main_thread();
            loop {
                let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
                let wait = match (left, main) {
                    (Some(l), true) => Some(l.min(SLICE)),
                    (Some(l), false) => Some(l),
                    (None, true) => Some(SLICE),
                    (None, false) => None,
                };
                let w = Waiter::new();
                self.waiters.borrow_mut().push(w.clone());
                it.unlocked(|| w.block(wait, None));
                self.waiters.borrow_mut().retain(|x| !Arc::ptr_eq(x, &w));
                if self.try_take() {
                    return Ok(true);
                }
                let polled = it.poll();
                let expired = deadline.is_some_and(|d| Instant::now() >= d);
                if polled.is_err() || expired {
                    if w.is_woken() && !self.locked.get() {
                        self.wake_one();
                    }
                    polled?;
                    return Ok(false);
                }
            }
        }

        fn release(&self, it: &mut Interp) -> R<()> {
            if !self.locked.replace(false) {
                return Err(it.runtime_error("release unlocked lock"));
            }
            self.wake_one();
            Ok(())
        }

        fn reinit(&self) {
            self.locked.set(false);
            self.waiters.borrow_mut().clear();
        }
    }

    /// A lock object is a synchronization primitive.  To create a lock,
    /// call threading.Lock().  Methods are:
    ///
    /// acquire() -- lock the lock, possibly blocking until it can be obtained
    /// release() -- unlock of the lock
    /// locked() -- test whether the lock is currently locked
    ///
    /// A lock is not owned by the thread that locked it; another thread may
    /// unlock it.  A thread attempting to lock a lock that it has already locked
    /// will block until another thread unlocks it.  Deadlocks may ensue.
    #[class(name = "lock", skip(py), hint(py(final)))]
    pub struct Lock {
        core: Rc<LockCore>,
    }

    fn new_lock(it: &mut Interp) -> Value {
        Py::new(it, Lock { core: Rc::new(LockCore::default()) }).into_value()
    }

    fn lock_core(it: &mut Interp, slf: &Py<Lock>) -> R<Rc<LockCore>> {
        Ok(slf.borrow(it)?.core.clone())
    }

    #[methods]
    impl Lock {
        /// Lock the lock.  Without argument, this blocks if the lock is already
        /// locked (even by the same thread), waiting for another thread to release
        /// the lock, and return True once the lock is acquired.
        /// With an argument, this will only block if the argument is true,
        /// and the return value reflects whether the lock is acquired.
        /// The blocking operation is interruptible.
        #[method(hint(py(aliases = "acquire_lock")))]
        fn acquire(slf: This<Py<Self>>, it: &mut Interp, #[kw] #[default(true)] blocking: bool, #[kw] timeout: Option<&Value>) -> R<bool> {
            let (blocking, timeout) = acquire_args(it, blocking, timeout)?;
            lock_core(it, &slf.0)?.acquire(it, blocking, timeout)
        }

        /// Release the lock, allowing another thread that is blocked waiting for
        /// the lock to acquire the lock.  The lock must be in the locked state,
        /// but it needn't be locked by the same thread that unlocks it.
        #[method(hint(py(aliases = "release_lock")))]
        fn release(slf: This<Py<Self>>, it: &mut Interp) -> R<()> {
            lock_core(it, &slf.0)?.release(it)
        }

        /// Return whether the lock is in the locked state.
        #[method(hint(py(aliases = "locked_lock")))]
        fn locked(&self) -> bool {
            self.core.locked.get()
        }

        #[proto(enter)]
        fn enter(slf: This<Py<Self>>, it: &mut Interp) -> R<bool> {
            lock_core(it, &slf.0)?.acquire(it, true, -1.0)
        }

        #[proto(exit)]
        fn exit(slf: This<Py<Self>>, it: &mut Interp, #[varargs] args: &[Value]) -> R<()> {
            let _ = args;
            lock_core(it, &slf.0)?.release(it)
        }

        fn _at_fork_reinit(&self) {
            self.core.reinit();
        }

        #[proto(repr)]
        fn repr(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
            let locked = slf.0.borrow(it)?.core.locked.get();
            let state = if locked { "locked" } else { "unlocked" };
            Ok(format!("<{state} {} object at {:#x}>", it.tp_name_of(slf.0.value()), it.id_of(slf.0.value())))
        }
    }

    /// A lock plus the thread that holds it and how many times it holds it.
    #[derive(Default)]
    pub struct RLockCore {
        lock: LockCore,
        owner: Cell<u64>,
        count: Cell<usize>,
    }

    impl RLockCore {
        fn owned(&self) -> bool {
            self.count.get() > 0 && self.owner.get() == ident()
        }

        fn acquire(&self, it: &mut Interp, blocking: bool, timeout: f64) -> R<bool> {
            if self.owned() {
                let Some(n) = self.count.get().checked_add(1) else {
                    return Err(it.overflow_err("Internal error: RLock recursion count overflow"));
                };
                self.count.set(n);
                return Ok(true);
            }
            if !self.lock.acquire(it, blocking, timeout)? {
                return Ok(false);
            }
            self.owner.set(ident());
            self.count.set(1);
            Ok(true)
        }

        fn release(&self, it: &mut Interp) -> R<()> {
            if !self.owned() {
                return Err(it.runtime_error("cannot release un-acquired lock"));
            }
            let left = self.count.get() - 1;
            self.count.set(left);
            if left == 0 {
                self.owner.set(0);
                self.lock.release(it)?;
            }
            Ok(())
        }
    }

    fn rlock_core(it: &mut Interp, slf: &Py<RLock>) -> R<Rc<RLockCore>> {
        Ok(slf.borrow(it)?.core.clone())
    }

    #[class(name = "RLock")]
    pub struct RLock {
        core: Rc<RLockCore>,
    }

    #[methods]
    impl RLock {
        #[constructor]
        fn new(cls: This<Value>, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> Value {
            let _ = (args, kwargs);
            let Value::Obj(cls) = &cls.0 else { unreachable!("a type") };
            crate::bind::opaque_instance(cls, RLock { core: Rc::new(RLockCore::default()) })
        }

        /// Lock the lock.  `blocking` indicates whether we should wait
        /// for the lock to be available or not.  If `blocking` is False
        /// and another thread holds the lock, the method will return False
        /// immediately.  If `blocking` is True and another thread holds
        /// the lock, the method will wait for the lock to be released,
        /// take it and then return True.
        /// (note: the blocking operation is interruptible.)
        ///
        /// In all other cases, the method will return True immediately.
        /// Precisely, if the current thread already holds the lock, its
        /// internal counter is simply incremented. If nobody holds the lock,
        /// the lock is taken and its internal counter initialized to 1.
        fn acquire(slf: This<Py<Self>>, it: &mut Interp, #[kw] #[default(true)] blocking: bool, #[kw] timeout: Option<&Value>) -> R<bool> {
            let (blocking, timeout) = acquire_args(it, blocking, timeout)?;
            rlock_core(it, &slf.0)?.acquire(it, blocking, timeout)
        }

        /// Release the lock, allowing another thread that is blocked waiting for
        /// the lock to acquire the lock.  The lock must be in the locked state,
        /// and must be locked by the same thread that unlocks it; otherwise a
        /// `RuntimeError` is raised.
        ///
        /// Do note that if the lock was acquire()d several times in a row by the
        /// current thread, release() needs to be called as many times for the lock
        /// to be available for other threads.
        fn release(slf: This<Py<Self>>, it: &mut Interp) -> R<()> {
            rlock_core(it, &slf.0)?.release(it)
        }

        /// acquire(blocking=True) -> bool
        ///
        /// Lock the lock.  `blocking` indicates whether we should wait
        /// for the lock to be available or not.  If `blocking` is False
        /// and another thread holds the lock, the method will return False
        /// immediately.  If `blocking` is True and another thread holds
        /// the lock, the method will wait for the lock to be released,
        /// take it and then return True.
        /// (note: the blocking operation is interruptible.)
        ///
        /// In all other cases, the method will return True immediately.
        /// Precisely, if the current thread already holds the lock, its
        /// internal counter is simply incremented. If nobody holds the lock,
        /// the lock is taken and its internal counter initialized to 1.
        #[proto(enter, hint(py(text_signature = "")))]
        fn enter(slf: This<Py<Self>>, it: &mut Interp) -> R<bool> {
            rlock_core(it, &slf.0)?.acquire(it, true, -1.0)
        }

        /// release()
        ///
        /// Release the lock, allowing another thread that is blocked waiting for
        /// the lock to acquire the lock.  The lock must be in the locked state,
        /// and must be locked by the same thread that unlocks it; otherwise a
        /// `RuntimeError` is raised.
        ///
        /// Do note that if the lock was acquire()d several times in a row by the
        /// current thread, release() needs to be called as many times for the lock
        /// to be available for other threads.
        #[proto(exit, hint(py(text_signature = "")))]
        fn exit(slf: This<Py<Self>>, it: &mut Interp, #[varargs] args: &[Value]) -> R<()> {
            let _ = args;
            rlock_core(it, &slf.0)?.release(it)
        }

        /// _is_owned() -> bool
        ///
        /// For internal use by `threading.Condition`.
        #[method(hint(py(text_signature = "")))]
        fn _is_owned(&self) -> bool {
            self.core.owned()
        }

        /// _recursion_count() -> int
        ///
        /// For internal use by reentrancy checks.
        #[method(hint(py(text_signature = "")))]
        fn _recursion_count(&self) -> usize {
            if self.core.owned() {
                self.core.count.get()
            } else {
                0
            }
        }

        /// _release_save() -> tuple
        ///
        /// For internal use by `threading.Condition`.
        #[method(hint(py(text_signature = "")))]
        fn _release_save(slf: This<Py<Self>>, it: &mut Interp) -> R<(usize, i64)> {
            let core = rlock_core(it, &slf.0)?;
            if core.count.get() == 0 {
                return Err(it.runtime_error("cannot release un-acquired lock"));
            }
            let state = (core.count.replace(0), core.owner.replace(0) as i64);
            core.lock.release(it)?;
            Ok(state)
        }

        /// _acquire_restore(state) -> None
        ///
        /// For internal use by `threading.Condition`.
        #[method(hint(py(text_signature = "")))]
        fn _acquire_restore(slf: This<Py<Self>>, it: &mut Interp, state: &Value) -> R<()> {
            let (count, owner) = match state.tuple_items() {
                Some([c, o]) => (it.index_of(c)?, it.index_of(o)?),
                _ => return Err(it.type_error("_acquire_restore() argument 1 must be tuple of 2 items")),
            };
            let core = rlock_core(it, &slf.0)?;
            core.lock.acquire(it, true, -1.0)?;
            core.owner.set(owner as u64);
            core.count.set(count.max(0) as usize);
            Ok(())
        }

        #[method(hint(py(text_signature = "")))]
        fn _at_fork_reinit(&self) {
            self.core.lock.reinit();
            self.core.owner.set(0);
            self.core.count.set(0);
        }

        #[proto(repr)]
        fn repr(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
            let core = rlock_core(it, &slf.0)?;
            let count = core.count.get();
            let state = if count > 0 { "locked" } else { "unlocked" };
            let (name, id) = (it.tp_name_of(slf.0.value()), it.id_of(slf.0.value()));
            Ok(format!("<{state} {name} object owner={} count={count} at {id:#x}>", core.owner.get()))
        }
    }

    /// The `_local` objects alive, so a finishing thread can drop its dicts.
    #[derive(Default)]
    struct LocalRegistry {
        all: Vec<Weak<Object>>,
        prune_at: usize,
    }

    fn init_overridden(it: &mut Interp, cls: &Obj) -> bool {
        let base = type_object::<Local>(it);
        match (it.lookup_mro(&base, "__init__"), it.lookup_mro(cls, "__init__")) {
            (Some(a), Some(b)) => !a.is(&b),
            _ => false,
        }
    }

    fn name_obj(it: &mut Interp, v: &Value) -> R<Obj> {
        match v {
            Value::Obj(o) if matches!(o.kind, Kind::Str(_)) => Ok(o.clone()),
            _ => {
                let t = it.type_name_of(v);
                Err(it.type_error(&format!("attribute name must be string, not '{t}'")))
            }
        }
    }

    /// Thread-local data: each thread sees its own instance dict. The dict of the thread that
    /// touched the object last is installed as the object's `__dict__`; a thread that finds
    /// another's installed swaps in its own (created, and `__init__` called with the original
    /// arguments, on its first access).
    #[class(name = "_local")]
    pub struct Local {
        args: Vec<Value>,
        kwargs: Vec<(Obj, Value)>,
        dicts: HashMap<u64, Obj>,
        current: Option<u64>,
    }

    fn install_dict(o: &Obj, d: Obj) {
        let old = o.dict.replace(Some(d));
        drop(old);
    }

    fn ensure_current(it: &mut Interp, slf: &Py<Local>) -> R<()> {
        let me = ident();
        if slf.borrow(it)?.current == Some(me) {
            return Ok(());
        }
        let Value::Obj(o) = slf.value() else { return Ok(()) };
        let existing = slf.borrow(it)?.dicts.get(&me).cloned();
        if let Some(d) = existing {
            install_dict(o, d);
            slf.borrow_mut(it)?.current = Some(me);
            return Ok(());
        }
        let d = it.new_dict();
        install_dict(o, d.clone());
        {
            let mut l = slf.borrow_mut(it)?;
            l.dicts.insert(me, d);
            l.current = Some(me);
        }
        let cls = it.type_of_obj(o);
        if init_overridden(it, &cls) {
            let (args, kwargs) = {
                let l = slf.borrow(it)?;
                (l.args.clone(), l.kwargs.clone())
            };
            let init = it.get_attr_str(slf.value(), "__init__")?;
            it.call(&init, args, kwargs)?;
        }
        Ok(())
    }

    #[methods]
    impl Local {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
            let Value::Obj(cls) = &cls.0 else { unreachable!("a type") };
            if (!args.is_empty() || !kwargs.is_empty()) && !init_overridden(it, cls) {
                return Err(it.type_error("Initialization arguments are not supported"));
            }
            let me = ident();
            let d = it.new_dict();
            let state = Local { args: args.to_vec(), kwargs: kwargs.to_vec(), dicts: HashMap::from([(me, d.clone())]), current: Some(me) };
            let v = crate::bind::opaque_instance(cls, state);
            if let Value::Obj(o) = &v {
                install_dict(o, d);
                let reg = it.native_state::<LocalRegistry>();
                if reg.all.len() >= reg.prune_at.max(64) {
                    reg.all.retain(|w| w.strong_count() > 0);
                    reg.prune_at = reg.all.len() * 2;
                }
                reg.all.push(Rc::downgrade(o));
            }
            Ok(v)
        }

        #[proto(init)]
        fn init(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<()> {
            let _ = (slf, it, args, kwargs);
            Ok(())
        }

        #[proto(getattribute)]
        fn getattribute(slf: This<Py<Self>>, it: &mut Interp, name: &Value) -> R<Value> {
            let n = name_obj(it, name)?;
            ensure_current(it, &slf.0)?;
            let cls = it.type_of(slf.0.value());
            it.generic_getattr(slf.0.value(), &cls, &n)
        }

        #[proto(setattr)]
        fn setattr(slf: This<Py<Self>>, it: &mut Interp, name: &Value, value: &Value) -> R<()> {
            let n = name_obj(it, name)?;
            ensure_current(it, &slf.0)?;
            let cls = it.type_of(slf.0.value());
            it.generic_setattr(slf.0.value(), &cls, &n, value.clone())
        }

        #[proto(delattr)]
        fn delattr(slf: This<Py<Self>>, it: &mut Interp, name: &Value) -> R<()> {
            let n = name_obj(it, name)?;
            ensure_current(it, &slf.0)?;
            let cls = it.type_of(slf.0.value());
            it.generic_delattr(slf.0.value(), &cls, &n)
        }
    }

    /// What a finished thread leaves behind: its dicts in every live `_local` go, and its
    /// sentinel lock (see `_set_sentinel`) is released so that joiners wake up.
    pub fn thread_finished(it: &mut Interp, ident: u64, sentinel: Option<Value>) {
        let locals: Vec<Rc<Object>> = {
            let reg = it.native_state::<LocalRegistry>();
            reg.all.retain(|w| w.strong_count() > 0);
            reg.all.iter().filter_map(Weak::upgrade).collect()
        };
        for o in locals {
            let Some(local) = Py::<Local>::from_value(it, &Value::Obj(o.clone())) else { continue };
            let taken = match local.borrow_mut(it) {
                Ok(mut l) => {
                    if l.current == Some(ident) {
                        l.current = None;
                        let old = o.dict.replace(None);
                        drop(old);
                    }
                    l.dicts.remove(&ident)
                }
                Err(_) => None,
            };
            drop(taken);
        }
        if let Some(s) = sentinel {
            if let Some(lock) = Py::<Lock>::from_value(it, &s) {
                if let Ok(core) = lock_core(it, &lock) {
                    if core.locked.get() {
                        let _ = core.release(it);
                    }
                }
            }
        }
    }

    /// Create a new lock object. See help(type(threading.Lock())) for
    /// information about locks.
    #[op(hint(py(aliases = "allocate")))]
    fn allocate_lock(it: &mut Interp) -> Value {
        new_lock(it)
    }

    /// Set a sentinel lock that will be released when the current thread
    /// state is finalized (normally when the thread ends).
    #[op]
    fn _set_sentinel(it: &mut Interp) -> Value {
        let lock = new_lock(it);
        it.threads.sentinels.insert(ident(), lock.clone());
        lock
    }

    /// Return a non-zero integer that uniquely identifies the current thread
    /// amongst other threads that exist simultaneously.
    #[op]
    fn get_ident() -> i64 {
        ident() as i64
    }

    /// Return a non-negative integer identifying the thread as reported
    /// by the OS (kernel). This may be used to uniquely identify a
    /// particular thread within a system.
    #[op]
    fn get_native_id() -> i64 {
        lumen_os::thread::native_id() as i64
    }

    /// Return the number of currently running Python threads, excluding
    /// the main thread.
    #[op]
    fn _count(it: &mut Interp) -> i64 {
        it.threads.gil.as_ref().map_or(0, |g| g.running()) as i64
    }

    /// Return True if daemon threads are allowed in the current interpreter,
    /// and False otherwise.
    #[op]
    fn daemon_threads_allowed() -> bool {
        true
    }

    /// Return True if the current interpreter is the main Python interpreter.
    #[op]
    fn _is_main_interpreter() -> bool {
        true
    }

    /// Start a new thread and return its identifier.
    ///
    /// The thread will call the function with positional arguments from the
    /// tuple "args" and keyword arguments taken from the optional dictionary
    /// "kwargs".  The thread exits when the function returns; the return value
    /// is ignored.  The thread will also exit when the function raises an
    /// unhandled exception; a stack trace will be printed unless the exception
    /// is SystemExit.
    #[op(hint(py(aliases = "start_new")))]
    fn start_new_thread(it: &mut Interp, function: &Value, args: &Value, kwargs: Option<&Value>) -> R<i64> {
        if !it.is_callable(function) {
            return Err(it.type_error("first arg must be callable"));
        }
        let Some(items) = args.tuple_items() else {
            return Err(it.type_error("2nd arg must be a tuple"));
        };
        let items = items.to_vec();
        let kw = match kwargs {
            None => Vec::new(),
            Some(k) if dict_of(k).is_some() => it.dict_to_kwargs(k)?,
            Some(_) => return Err(it.type_error("optional 3rd arg must be a dictionary")),
        };
        let id = it.start_thread(function.clone(), items, kw)?;
        Ok(id as i64)
    }

    /// This is synonymous to ``raise SystemExit''.  It will cause the current
    /// thread to exit silently unless the exception is caught.
    #[op(hint(py(aliases = "exit_thread")))]
    fn exit(it: &mut Interp) -> R<()> {
        let cls = it.exc_type("SystemExit");
        Err(it.new_exc(&cls, Vec::new()))
    }

    /// Return the thread stack size used when creating new threads.  The
    /// optional size argument specifies the stack size (in bytes) to be used
    /// for subsequently created threads, and must be 0 (use platform or
    /// configured default) or a positive integer value of at least 32,768 (32k).
    #[op]
    fn stack_size(it: &mut Interp, #[default(0)] size: i64) -> R<i64> {
        if size < 0 {
            return Err(it.value_error("size must be 0 or a positive value"));
        }
        if size != 0 && size < 32768 {
            return Err(it.value_error(&format!("size not valid: {size} bytes")));
        }
        let old = std::mem::replace(&mut it.threads.stack_size, size as usize);
        Ok(old as i64)
    }

    /// Simulate the arrival of the given signal in the main thread,
    /// where the corresponding signal handler will be executed.
    /// If it is omitted, SIGINT is assumed.
    /// A subthread can use this function to interrupt the main thread.
    ///
    /// Note: the default signal handler for SIGINT raises ``KeyboardInterrupt``.
    #[op]
    fn interrupt_main(it: &mut Interp, #[default(2)] signum: i64) -> R<()> {
        crate::builtins::signalm::simulate(it, signum)
    }

    struct ExceptHookArgs;

    fn except_hook_args_type(it: &mut Interp) -> Obj {
        structseq_type::<ExceptHookArgs>(it, "_thread", "_ExceptHookArgs", &["exc_type", "exc_value", "exc_traceback", "thread"], 4)
    }

    /// Handle uncaught Thread.run() exception.
    #[op]
    fn _excepthook(it: &mut Interp, args: &Value) -> R<()> {
        let expected = except_hook_args_type(it);
        let is_args = matches!(args, Value::Obj(_)) && std::rc::Rc::ptr_eq(&it.type_of(args), &expected);
        if !is_args {
            return Err(it.type_error("_thread._excepthook argument type must be ExceptHookArgs"));
        }
        let (exc_type, exc_value, thread) = match args.tuple_items() {
            Some([t, v, _, th]) => (t.clone(), v.clone(), th.clone()),
            _ => return Err(it.type_error("_thread._excepthook argument type must be ExceptHookArgs")),
        };
        let system_exit = it.exc_type("SystemExit");
        if matches!(&exc_type, Value::Obj(t) if Rc::ptr_eq(t, &system_exit)) {
            return Ok(());
        }
        let sys_stderr = it.sys_module.clone().and_then(|m| {
            let d = it.module_dict(&m);
            dict_get_str(&d, "stderr")
        });
        let file = match sys_stderr.filter(|f| !f.is_none()) {
            Some(f) => f,
            None => {
                if thread.is_none() {
                    return Ok(());
                }
                let f = it.get_attr_str(&thread, "_stderr")?;
                if f.is_none() {
                    return Ok(());
                }
                f
            }
        };
        let who = if thread.is_none() {
            ident().to_string()
        } else {
            let name = it.get_attr_str(&thread, "name")?;
            it.str_of(&name)?
        };
        let body = match &exc_value {
            Value::Obj(e) if matches!(e.kind, Kind::Exception(_)) => it.format_exception(e),
            v => format!("{}\n", it.str_of(v)?),
        };
        it.call_method(&file, "write", vec![Value::string(format!("Exception in thread {who}:\n"))])?;
        it.call_method(&file, "write", vec![Value::string(body)])?;
        it.call_method(&file, "flush", Vec::new())?;
        Ok(())
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        let lock = type_object::<Lock>(it);
        dict_set_str(&d, "LockType", Value::Obj(lock));
        dict_set_str(&d, "error", Value::Obj(it.exc_type("RuntimeError")));
        dict_set_str(&d, "TIMEOUT_MAX", Value::Float(TIMEOUT_MAX));
        let hook_args = except_hook_args_type(it);
        dict_set_str(&d, "_ExceptHookArgs", Value::Obj(hook_args));
    }
}
