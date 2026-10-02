//! `_thread` for a single-threaded interpreter: locks that no other thread can release, and no
//! new threads.

/// This module provides primitive operations to write multi-threaded programs.
/// The 'threading' module provides a more convenient interface.
#[lumen_bind::module(name = "_thread")]
pub mod _thread {
    use crate::bind::{type_object, KwArgs, Py, This};
    use crate::object::*;
    use crate::vm::{dict_set_str, Interp};

    pub const MAIN_THREAD: i64 = 0x1000;

    /// `(blocking, timeout)` checked as CPython's `lock_acquire_parse_args`.
    fn acquire_args(it: &mut Interp, blocking: bool, timeout: Option<&Value>) -> R<(bool, f64)> {
        let timeout = match timeout {
            None => -1.0,
            Some(Value::Float(f)) => *f,
            Some(v) => it.index_of(v)? as f64,
        };
        if !blocking && timeout != -1.0 {
            return Err(it.value_error("can't specify a timeout for a non-blocking call"));
        }
        if timeout < 0.0 && timeout != -1.0 {
            return Err(it.value_error("timeout value must be positive"));
        }
        Ok((blocking, timeout))
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
        locked: bool,
    }

    fn new_lock(it: &mut Interp) -> Value {
        Py::new(it, Lock { locked: false }).into_value()
    }

    impl Lock {
        fn acquire_impl(&mut self, it: &mut Interp, blocking: bool, timeout: Option<&Value>) -> R<bool> {
            let (blocking, timeout) = acquire_args(it, blocking, timeout)?;
            if !self.locked {
                self.locked = true;
                return Ok(true);
            }
            if !blocking {
                return Ok(false);
            }
            if timeout >= 0.0 {
                it.flush_out();
                it.platform.borrow_mut().sleep(timeout);
                return Ok(false);
            }
            Err(it.runtime_error("deadlock: lock is already held and there is no other thread to release it"))
        }

        fn release_impl(&mut self, it: &mut Interp) -> R<()> {
            if !std::mem::replace(&mut self.locked, false) {
                return Err(it.runtime_error("release unlocked lock"));
            }
            Ok(())
        }
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
        fn acquire(&mut self, it: &mut Interp, #[kw] #[default(true)] blocking: bool, #[kw] timeout: Option<&Value>) -> R<bool> {
            self.acquire_impl(it, blocking, timeout)
        }

        /// Release the lock, allowing another thread that is blocked waiting for
        /// the lock to acquire the lock.  The lock must be in the locked state,
        /// but it needn't be locked by the same thread that unlocks it.
        #[method(hint(py(aliases = "release_lock")))]
        fn release(&mut self, it: &mut Interp) -> R<()> {
            self.release_impl(it)
        }

        /// Return whether the lock is in the locked state.
        #[method(hint(py(aliases = "locked_lock")))]
        fn locked(&self) -> bool {
            self.locked
        }

        #[proto(enter)]
        fn enter(&mut self, it: &mut Interp) -> R<bool> {
            self.acquire_impl(it, true, None)
        }

        #[proto(exit)]
        fn exit(&mut self, it: &mut Interp, #[varargs] args: &[Value]) -> R<()> {
            let _ = args;
            self.release_impl(it)
        }

        fn _at_fork_reinit(&mut self) {
            self.locked = false;
        }

        #[proto(repr)]
        fn repr(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
            let locked = slf.0.borrow(it)?.locked;
            let state = if locked { "locked" } else { "unlocked" };
            Ok(format!("<{state} {} object at {:#x}>", it.tp_name_of(slf.0.value()), it.id_of(slf.0.value())))
        }
    }

    #[class(name = "RLock")]
    pub struct RLock {
        count: usize,
    }

    impl RLock {
        fn release_impl(&mut self, it: &mut Interp) -> R<()> {
            if self.count == 0 {
                return Err(it.runtime_error("cannot release un-acquired lock"));
            }
            self.count -= 1;
            Ok(())
        }
    }

    #[methods]
    impl RLock {
        #[constructor]
        fn new(#[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> RLock {
            let _ = (args, kwargs);
            RLock { count: 0 }
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
        fn acquire(&mut self, it: &mut Interp, #[kw] #[default(true)] blocking: bool, #[kw] timeout: Option<&Value>) -> R<bool> {
            acquire_args(it, blocking, timeout)?;
            self.count += 1;
            Ok(true)
        }

        /// Release the lock, allowing another thread that is blocked waiting for
        /// the lock to acquire the lock.  The lock must be in the locked state,
        /// and must be locked by the same thread that unlocks it; otherwise a
        /// `RuntimeError` is raised.
        ///
        /// Do note that if the lock was acquire()d several times in a row by the
        /// current thread, release() needs to be called as many times for the lock
        /// to be available for other threads.
        fn release(&mut self, it: &mut Interp) -> R<()> {
            self.release_impl(it)
        }

        #[proto(enter)]
        fn enter(&mut self) -> bool {
            self.count += 1;
            true
        }

        #[proto(exit)]
        fn exit(&mut self, it: &mut Interp, #[varargs] args: &[Value]) -> R<()> {
            let _ = args;
            self.release_impl(it)
        }

        fn _is_owned(&self) -> bool {
            self.count > 0
        }

        fn _recursion_count(&self) -> usize {
            self.count
        }

        fn _release_save(&mut self, it: &mut Interp) -> R<(usize, i64)> {
            if self.count == 0 {
                return Err(it.runtime_error("cannot release un-acquired lock"));
            }
            Ok((std::mem::take(&mut self.count), MAIN_THREAD))
        }

        fn _acquire_restore(&mut self, it: &mut Interp, state: &Value) -> R<()> {
            let n = match state.tuple_items() {
                Some([c, _]) => it.index_of(c)?,
                _ => return Err(it.type_error("_acquire_restore() argument 1 must be tuple")),
            };
            self.count = n.max(0) as usize;
            Ok(())
        }

        fn _at_fork_reinit(&mut self) {
            self.count = 0;
        }

        #[proto(repr)]
        fn repr(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
            let count = slf.0.borrow(it)?.count;
            let state = if count > 0 { "locked" } else { "unlocked" };
            let owner = if count > 0 { MAIN_THREAD } else { 0 };
            let (name, id) = (it.tp_name_of(slf.0.value()), it.id_of(slf.0.value()));
            Ok(format!("<{state} {name} object owner={owner} count={count} at {id:#x}>"))
        }
    }

    /// Thread-local data
    #[class(name = "_local")]
    pub struct Local;

    #[methods]
    impl Local {
        #[constructor]
        fn new(#[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> Local {
            let _ = (args, kwargs);
            Local
        }

        #[proto(init)]
        fn init(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<()> {
            if (!args.is_empty() || !kwargs.is_empty()) && !it.is_heap(&it.type_of(&slf.0)) {
                return Err(it.type_error("Initialization arguments are not supported"));
            }
            Ok(())
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
        new_lock(it)
    }

    /// Return a non-zero integer that uniquely identifies the current thread
    /// amongst other threads that exist simultaneously.
    #[op]
    fn get_ident() -> i64 {
        MAIN_THREAD
    }

    /// Return a non-negative integer identifying the thread as reported
    /// by the OS (kernel). This may be used to uniquely identify a
    /// particular thread within a system.
    #[op]
    fn get_native_id(it: &mut Interp) -> i64 {
        it.platform.borrow().process_id() as i64
    }

    /// Return the number of currently running Python threads, excluding
    /// the main thread.
    #[op]
    fn _count() -> i64 {
        0
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
    #[op(hint(py(aliases = "start_new")))]
    fn start_new_thread(it: &mut Interp, function: &Value, args: &Value, kwargs: Option<&Value>) -> R<Value> {
        let _ = (function, args, kwargs);
        Err(it.runtime_error("can't start new thread"))
    }

    /// Return the thread stack size used when creating new threads.
    #[op]
    fn stack_size(#[default(0)] size: i64) -> i64 {
        let _ = size;
        0
    }

    /// Simulate the arrival of the given signal in the main thread,
    /// where the corresponding signal handler will be executed.
    #[op]
    fn interrupt_main(it: &mut Interp, #[default(2)] signum: i64) -> R<()> {
        crate::builtins::signalm::simulate(it, signum)
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        let lock = type_object::<Lock>(it);
        dict_set_str(&d, "LockType", Value::Obj(lock));
        dict_set_str(&d, "error", Value::Obj(it.exc_type("RuntimeError")));
        dict_set_str(&d, "TIMEOUT_MAX", Value::Float(9_223_372_036.0));
    }
}
