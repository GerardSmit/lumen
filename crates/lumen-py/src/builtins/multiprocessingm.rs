//! `_multiprocessing` (`SemLock`, `sem_unlink`) and `_posixshmem` on `lumen_os::ipc`
//! (`Modules/_multiprocessing/semaphore.c`, `Modules/_multiprocessing/posixshmem.c`).

/// The semaphore primitive behind `multiprocessing` locks, semaphores and conditions.
#[lumen_bind::module(name = "_multiprocessing")]
pub mod multiprocessing {
    use crate::bind::{type_object, Py, This};
    use crate::object::*;
    use crate::vm::{dict_set_str, Interp};
    use lumen_os::ipc::{self, Semaphore};
    use std::time::{Duration, Instant};

    const RECURSIVE_MUTEX: i32 = 0;
    const SEMAPHORE: i32 = 1;
    const SLICE: Duration = Duration::from_millis(20);
    const EINTR: i32 = 4;

    fn os_err(it: &mut Interp, e: lumen_os::FsError) -> Obj {
        it.os_error_errno(e.errno(), None, None)
    }

    /// A named POSIX semaphore with a recursive-mutex or counting flavour.
    #[class(name = "SemLock", module = "_multiprocessing")]
    pub struct SemLock {
        sem: Semaphore,
        kind: i32,
        maxvalue: i32,
        name: Option<String>,
        count: i32,
        owner: bool,
    }

    impl SemLock {
        fn is_mine(&self) -> bool {
            self.count > 0 && self.owner
        }

        fn borrowed(&self) -> Semaphore {
            Semaphore::from_raw(self.sem.raw())
        }
    }

    fn acquire_sem(it: &mut Interp, sem: &Semaphore, block: bool, deadline: Option<Instant>) -> R<bool> {
        loop {
            match sem.try_wait() {
                Ok(true) => return Ok(true),
                Ok(false) => break,
                Err(e) if e.errno() == EINTR => it.poll()?,
                Err(e) => return Err(os_err(it, e)),
            }
        }
        if !block {
            return Ok(false);
        }
        it.flush_out();
        loop {
            let slice = match deadline {
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        return Ok(false);
                    }
                    (d - now).min(SLICE)
                }
                None => SLICE,
            };
            match sem.wait_ms(slice.as_millis().max(1) as u64) {
                Ok(true) => return Ok(true),
                Ok(false) => {}
                Err(e) if e.errno() == EINTR => {}
                Err(e) => return Err(os_err(it, e)),
            }
            it.poll()?;
        }
    }

    #[methods]
    impl SemLock {
        #[constructor]
        fn new(it: &mut Interp, #[kw] kind: i32, #[kw] value: i32, #[kw] maxvalue: i32, #[kw] name: &str, #[kw] unlink: bool) -> R<SemLock> {
            if kind != RECURSIVE_MUTEX && kind != SEMAPHORE {
                return Err(it.value_error("unrecognized kind"));
            }
            let sem = Semaphore::create(name, value as u32).map_err(|e| os_err(it, e))?;
            if unlink {
                ipc::sem_unlink(name).map_err(|e| os_err(it, e))?;
            }
            Ok(SemLock { sem, kind, maxvalue, name: (!unlink).then(|| name.to_string()), count: 0, owner: false })
        }

        /// Acquire the semaphore/lock.
        fn acquire(slf: This<Py<Self>>, it: &mut Interp, #[kw] #[default(true)] block: bool, #[kw] timeout: Option<&Value>) -> R<bool> {
            acquire_lock(it, &slf.0, block, timeout)
        }

        /// Release the semaphore/lock.
        fn release(slf: This<Py<Self>>, it: &mut Interp) -> R<()> {
            release_lock(it, &slf.0)
        }

        #[proto(enter)]
        fn enter(slf: This<Py<Self>>, it: &mut Interp) -> R<bool> {
            acquire_lock(it, &slf.0, true, None)
        }

        #[proto(exit)]
        fn exit(slf: This<Py<Self>>, it: &mut Interp, #[varargs] args: &[Value]) -> R<()> {
            let _ = args;
            release_lock(it, &slf.0)
        }

        /// Rezero the net acquisition count after fork().
        fn _after_fork(&mut self) {
            self.count = 0;
        }

        /// Num of `acquire()`s minus num of `release()`s for this process.
        fn _count(&self) -> i32 {
            self.count
        }

        /// Whether the lock is held by the current thread.
        fn _is_mine(&self) -> bool {
            self.is_mine()
        }

        /// Get the value of the semaphore.
        fn _get_value(&self, it: &mut Interp) -> R<i32> {
            match self.sem.value().map_err(|e| os_err(it, e))? {
                Some(v) => Ok(v),
                None => Err(it.new_exc_str("NotImplementedError", "")),
            }
        }

        /// Return whether the semaphore has value zero.
        fn _is_zero(&self, it: &mut Interp) -> R<bool> {
            match self.sem.value().map_err(|e| os_err(it, e))? {
                Some(v) => Ok(v == 0),
                None => {
                    if self.sem.try_wait().map_err(|e| os_err(it, e))? {
                        self.sem.post().map_err(|e| os_err(it, e))?;
                        Ok(false)
                    } else {
                        Ok(true)
                    }
                }
            }
        }

        #[classmethod]
        fn _rebuild(cls: This<Value>, it: &mut Interp, handle: usize, kind: i32, maxvalue: i32, name: Option<&Value>) -> R<Value> {
            let _ = cls;
            let name = match name {
                None | Some(Value::None) => None,
                Some(v) => match v.as_str() {
                    Some(s) => Some(s.to_string()),
                    None => return Err(it.type_error("argument 4 must be str or None")),
                },
            };
            let sem = match &name {
                Some(n) => Semaphore::open(n).map_err(|e| os_err(it, e))?,
                None => Semaphore::from_raw(handle),
            };
            Ok(Py::new(it, SemLock { sem, kind, maxvalue, name, count: 0, owner: false }).value().clone())
        }

        #[getter]
        fn handle(&self) -> i64 {
            self.sem.raw() as i64
        }

        #[getter]
        fn kind(&self) -> i32 {
            self.kind
        }

        #[getter]
        fn maxvalue(&self) -> i32 {
            self.maxvalue
        }

        #[getter]
        fn name(&self) -> Value {
            self.name.as_deref().map_or(Value::None, Value::str)
        }
    }

    fn acquire_lock(it: &mut Interp, slf: &Py<SemLock>, block: bool, timeout: Option<&Value>) -> R<bool> {
        let (kind, mine) = {
            let s = slf.borrow(it)?;
            (s.kind, s.is_mine())
        };
        if kind == RECURSIVE_MUTEX && mine {
            slf.with(it, |s| s.count += 1)?;
            return Ok(true);
        }
        let deadline = match timeout {
            None | Some(Value::None) => None,
            Some(v) => {
                let secs = match v {
                    Value::Float(f) => *f,
                    v if v.is_int_like() => it.index_of(v)? as f64,
                    v => {
                        let name = it.type_name_of(v);
                        return Err(it.type_error(&format!("must be real number, not {name}")));
                    }
                };
                let secs = if secs.is_nan() || secs < 0.0 { 0.0 } else { secs.min(1e9) };
                Some(Instant::now() + Duration::from_secs_f64(secs))
            }
        };
        let sem = slf.borrow(it)?.borrowed();
        if !acquire_sem(it, &sem, block, deadline)? {
            return Ok(false);
        }
        slf.with(it, |s| {
            s.count += 1;
            s.owner = true;
        })?;
        Ok(true)
    }

    fn release_lock(it: &mut Interp, slf: &Py<SemLock>) -> R<()> {
        let (kind, maxvalue, count, mine, sem) = {
            let s = slf.borrow(it)?;
            (s.kind, s.maxvalue, s.count, s.is_mine(), s.borrowed())
        };
        if kind == RECURSIVE_MUTEX {
            if !mine {
                return Err(it.new_exc_str("AssertionError", "attempt to release recursive lock not owned by thread"));
            }
            if count > 1 {
                return slf.with(it, |s| s.count -= 1);
            }
        } else {
            match sem.value().map_err(|e| os_err(it, e))? {
                Some(v) if v >= maxvalue => return Err(it.value_error("semaphore or lock released too many times")),
                Some(_) => {}
                None => {
                    if maxvalue == 1 && sem.try_wait().map_err(|e| os_err(it, e))? {
                        sem.post().map_err(|e| os_err(it, e))?;
                        return Err(it.value_error("semaphore or lock released too many times"));
                    }
                }
            }
        }
        sem.post().map_err(|e| os_err(it, e))?;
        slf.with(it, |s| s.count -= 1)
    }

    /// Unlink a named semaphore.
    #[op]
    fn sem_unlink(it: &mut Interp, name: &str) -> R<()> {
        ipc::sem_unlink(name).map_err(|e| os_err(it, e))
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        let flags = it.new_dict();
        let mut set = |name: &str, v: i64| {
            let _ = it.dict_set(&flags, Value::str(name), Value::Int(v));
        };
        set("HAVE_SEM_OPEN", 1);
        set("HAVE_SEM_TIMEDWAIT", cfg!(any(target_os = "linux", target_os = "android")) as i64);
        if !ipc::HAVE_SEM_GETVALUE {
            set("HAVE_BROKEN_SEM_GETVALUE", 1);
        }
        dict_set_str(&d, "flags", Value::Obj(flags));
        let cls = type_object::<SemLock>(it);
        let cd = cls.dict.borrow().clone();
        if let Some(cd) = cd.as_ref() {
            dict_set_str(cd, "SEM_VALUE_MAX", Value::Int(ipc::sem_value_max() as i64));
        }
    }
}

/// POSIX shared memory objects.
#[lumen_bind::module(name = "_posixshmem")]
pub mod posixshmem {
    use crate::object::*;
    use crate::vm::Interp;
    use lumen_os::ipc;

    /// Open a shared memory object.  Returns a file descriptor (integer).
    #[op]
    fn shm_open(it: &mut Interp, #[kw] path: &Value, #[kw] flags: i32, #[kw] #[default(0o777)] mode: i32) -> R<i32> {
        let Some(name) = path.as_str().map(str::to_string) else {
            let t = it.type_name_of(path);
            return Err(it.type_error(&format!("shm_open() argument 'path' must be str, not {t}")));
        };
        ipc::shm_open(&name, flags, mode as u32).map_err(|e| it.os_error_errno(e.errno(), Some(path), None))
    }

    /// Remove a shared memory object (similar to unlink()).
    ///
    /// Remove a shared memory object name, and, once all processes  have  unmapped
    /// the object, de-allocates and destroys the contents of the associated memory
    /// region.
    #[op]
    fn shm_unlink(it: &mut Interp, #[kw] path: &Value) -> R<()> {
        let Some(name) = path.as_str().map(str::to_string) else {
            let t = it.type_name_of(path);
            return Err(it.type_error(&format!("shm_unlink() argument 'path' must be str, not {t}")));
        };
        ipc::shm_unlink(&name).map_err(|e| it.os_error_errno(e.errno(), Some(path), None))
    }
}
