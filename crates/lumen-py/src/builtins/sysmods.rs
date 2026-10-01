//! Small native modules for a single-threaded interpreter: `builtins`, `_thread`, `gc`, `atexit`.

use super::native::*;
use crate::object::*;
use crate::vm::*;

pub fn make_builtins(it: &mut Interp) -> Obj {
    Object::with_dict(Kind::Module, it.builtins.clone())
}

// ---- _thread ------------------------------------------------------------------------------------

struct LockState {
    locked: bool,
}

struct RLockState {
    count: usize,
}

const MAIN_THREAD: i64 = 0x1000;

fn lock_new(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    match a.first() {
        Some(Value::Obj(c)) => Ok(new_opaque(c, LockState { locked: false })),
        _ => Err(it.type_error("lock.__new__(X): X is not a type object")),
    }
}

fn allocate_lock(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    let ty = thread_type(it, "lock");
    Ok(new_opaque(&ty, LockState { locked: false }))
}

fn thread_type(it: &mut Interp, name: &str) -> Obj {
    let m = match dict_get_str(&it.modules, "_thread") {
        Some(Value::Obj(m)) => m,
        _ => unreachable!("_thread is loaded before its objects exist"),
    };
    let d = it.module_dict(&m);
    match dict_get_str(&d, name) {
        Some(Value::Obj(t)) => t,
        _ => unreachable!(),
    }
}

fn acquire_args(it: &mut Interp, a: &[Value], kw: Kw) -> R<(bool, f64)> {
    let b = it.bind_args("acquire", &a[1.min(a.len())..], kw, &["blocking", "timeout"], 0)?;
    let blocking = match &b[0] {
        Some(v) => it.truthy(v)?,
        None => true,
    };
    let timeout = match &b[1] {
        Some(v) => it.float_arg(v)?,
        None => -1.0,
    };
    if !blocking && timeout != -1.0 {
        return Err(it.value_error("can't specify a timeout for a non-blocking call"));
    }
    if timeout < 0.0 && timeout != -1.0 {
        return Err(it.value_error("timeout value must be a non-negative number"));
    }
    Ok((blocking, timeout))
}

fn lock_acquire(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let (blocking, timeout) = acquire_args(it, a, kw)?;
    let taken = with_opaque::<LockState, _>(&a[0], |l| {
        if l.locked {
            false
        } else {
            l.locked = true;
            true
        }
    });
    match taken {
        Some(true) => Ok(Value::Bool(true)),
        Some(false) => {
            if !blocking {
                return Ok(Value::Bool(false));
            }
            if timeout >= 0.0 {
                it.flush_out();
                it.platform.borrow_mut().sleep(timeout);
                return Ok(Value::Bool(false));
            }
            Err(it.new_exc_str("RuntimeError", "deadlock: lock is already held and there is no other thread to release it"))
        }
        None => Err(it.self_state_err("lock")),
    }
}

fn lock_release(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    match with_opaque::<LockState, _>(&a[0], |l| std::mem::replace(&mut l.locked, false)) {
        Some(true) => Ok(Value::None),
        Some(false) => Err(it.new_exc_str("RuntimeError", "release unlocked lock")),
        None => Err(it.self_state_err("lock")),
    }
}

fn lock_locked(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    match with_opaque::<LockState, _>(&a[0], |l| l.locked) {
        Some(b) => Ok(Value::Bool(b)),
        None => Err(it.self_state_err("lock")),
    }
}

fn lock_exit(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    lock_release(it, &a[..1], kw)
}

fn lock_enter(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    lock_acquire(it, &a[..1], &[])
}

fn lock_reinit(_it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    with_opaque::<LockState, _>(&a[0], |l| l.locked = false);
    Ok(Value::None)
}

fn lock_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let locked = with_opaque::<LockState, _>(&a[0], |l| l.locked).unwrap_or(false);
    let t = it.type_of(&a[0]);
    Ok(Value::string(format!("<{} {} object at {:#x}>", if locked { "locked" } else { "unlocked" }, it.type_display(&t), it.id_of(&a[0]))))
}

fn rlock_new(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    match a.first() {
        Some(Value::Obj(c)) => Ok(new_opaque(c, RLockState { count: 0 })),
        _ => Err(it.type_error("RLock.__new__(X): X is not a type object")),
    }
}

fn rlock_acquire(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    acquire_args(it, a, kw)?;
    match with_opaque::<RLockState, _>(&a[0], |l| l.count += 1) {
        Some(()) => Ok(Value::Bool(true)),
        None => Err(it.self_state_err("RLock")),
    }
}

fn rlock_release(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    match with_opaque::<RLockState, _>(&a[0], |l| {
        if l.count == 0 {
            false
        } else {
            l.count -= 1;
            true
        }
    }) {
        Some(true) => Ok(Value::None),
        Some(false) => Err(it.new_exc_str("RuntimeError", "cannot release un-acquired lock")),
        None => Err(it.self_state_err("RLock")),
    }
}

fn rlock_enter(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    rlock_acquire(it, &a[..1], &[])
}

fn rlock_exit(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    rlock_release(it, &a[..1], &[])
}

fn rlock_is_owned(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    match with_opaque::<RLockState, _>(&a[0], |l| l.count > 0) {
        Some(b) => Ok(Value::Bool(b)),
        None => Err(it.self_state_err("RLock")),
    }
}

fn rlock_release_save(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    match with_opaque::<RLockState, _>(&a[0], |l| std::mem::take(&mut l.count)) {
        Some(0) => Err(it.new_exc_str("RuntimeError", "cannot release un-acquired lock")),
        Some(n) => Ok(Value::tuple(vec![Value::Int(n as i64), Value::Int(MAIN_THREAD)])),
        None => Err(it.self_state_err("RLock")),
    }
}

fn rlock_acquire_restore(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("_acquire_restore", a, 2, 2)?;
    let n = match a[1].tuple_items() {
        Some([c, _]) => it.index_of(c)?,
        _ => return Err(it.type_error("_acquire_restore() argument must be a (count, owner) tuple")),
    };
    with_opaque::<RLockState, _>(&a[0], |l| l.count = n as usize);
    Ok(Value::None)
}

fn rlock_reinit(_it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    with_opaque::<RLockState, _>(&a[0], |l| l.count = 0);
    Ok(Value::None)
}

fn rlock_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let count = with_opaque::<RLockState, _>(&a[0], |l| l.count).unwrap_or(0);
    let t = it.type_of(&a[0]);
    let owner = if count > 0 { MAIN_THREAD } else { 0 };
    Ok(Value::string(format!(
        "<{} {} object owner={} count={} at {:#x}>",
        if count > 0 { "locked" } else { "unlocked" },
        it.type_display(&t),
        owner,
        count,
        it.id_of(&a[0])
    )))
}

fn local_init(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::None)
}

fn get_ident(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Int(MAIN_THREAD))
}

fn get_native_id(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Int(std::process::id() as i64))
}

fn thread_count(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Int(0))
}

fn start_new_thread(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Err(it.new_exc_str("RuntimeError", "can't start new thread"))
}

fn stack_size(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Int(0))
}

fn interrupt_main(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Err(it.new_exc_str("KeyboardInterrupt", ""))
}

pub fn make_thread(it: &mut Interp) -> Obj {
    let m = it.new_module("_thread");
    let d = it.module_dict(&m);
    it.register_module("_thread", &m);

    let lock = new_type(it, "_thread", "lock", None, Layout::Other);
    it.reg_new(&lock, lock_new);
    it.reg(&lock, "acquire", lock_acquire);
    it.reg(&lock, "acquire_lock", lock_acquire);
    it.reg(&lock, "release", lock_release);
    it.reg(&lock, "release_lock", lock_release);
    it.reg(&lock, "locked", lock_locked);
    it.reg(&lock, "locked_lock", lock_locked);
    it.reg(&lock, "__enter__", lock_enter);
    it.reg(&lock, "__exit__", lock_exit);
    it.reg(&lock, "_at_fork_reinit", lock_reinit);
    it.reg(&lock, "__repr__", lock_repr);
    set_type(&d, "lock", &lock);
    set_type(&d, "LockType", &lock);

    let rlock = new_type(it, "_thread", "RLock", None, Layout::Other);
    it.reg_new(&rlock, rlock_new);
    it.reg(&rlock, "acquire", rlock_acquire);
    it.reg(&rlock, "release", rlock_release);
    it.reg(&rlock, "__enter__", rlock_enter);
    it.reg(&rlock, "__exit__", rlock_exit);
    it.reg(&rlock, "_is_owned", rlock_is_owned);
    it.reg(&rlock, "_release_save", rlock_release_save);
    it.reg(&rlock, "_acquire_restore", rlock_acquire_restore);
    it.reg(&rlock, "_at_fork_reinit", rlock_reinit);
    it.reg(&rlock, "__repr__", rlock_repr);
    set_type(&d, "RLock", &rlock);

    let local = new_type(it, "_thread", "_local", None, Layout::Object);
    it.reg(&local, "__init__", local_init);
    set_type(&d, "_local", &local);

    set_fn(it, &d, "allocate_lock", allocate_lock);
    set_fn(it, &d, "allocate", allocate_lock);
    set_fn(it, &d, "get_ident", get_ident);
    set_fn(it, &d, "get_native_id", get_native_id);
    set_fn(it, &d, "_count", thread_count);
    set_fn(it, &d, "start_new_thread", start_new_thread);
    set_fn(it, &d, "start_new", start_new_thread);
    set_fn(it, &d, "stack_size", stack_size);
    set_fn(it, &d, "interrupt_main", interrupt_main);
    dict_set_str(&d, "error", Value::Obj(it.exc_type("RuntimeError")));
    dict_set_str(&d, "TIMEOUT_MAX", Value::Float(9_223_372_036.0));
    m
}

// ---- gc -----------------------------------------------------------------------------------------

fn gc_collect(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    it.run_weak_callbacks();
    Ok(Value::Int(0))
}

fn gc_enable(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    it.gc_enabled = true;
    Ok(Value::None)
}

fn gc_disable(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    it.gc_enabled = false;
    Ok(Value::None)
}

fn gc_isenabled(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Bool(it.gc_enabled))
}

fn gc_get_count(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::tuple(vec![Value::Int(0), Value::Int(0), Value::Int(0)]))
}

fn gc_get_threshold(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::tuple(vec![Value::Int(700), Value::Int(10), Value::Int(10)]))
}

fn gc_none(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::None)
}

fn gc_empty_list(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::list(Vec::new()))
}

fn gc_zero(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Int(0))
}

fn gc_false(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Bool(false))
}

fn gc_get_stats(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    let mut out = Vec::new();
    for _ in 0..3 {
        let d = it.new_dict();
        for k in ["collections", "collected", "uncollectable"] {
            dict_set_str(&d, k, Value::Int(0));
        }
        out.push(Value::Obj(d));
    }
    Ok(Value::list(out))
}

pub fn make_gc(it: &mut Interp) -> Obj {
    let m = it.new_module("gc");
    let d = it.module_dict(&m);
    let fns: &[(&'static str, NativeFn)] = &[
        ("collect", gc_collect),
        ("enable", gc_enable),
        ("disable", gc_disable),
        ("isenabled", gc_isenabled),
        ("get_count", gc_get_count),
        ("get_threshold", gc_get_threshold),
        ("set_threshold", gc_none),
        ("set_debug", gc_none),
        ("get_debug", gc_zero),
        ("freeze", gc_none),
        ("unfreeze", gc_none),
        ("get_freeze_count", gc_zero),
        ("get_objects", gc_empty_list),
        ("get_referrers", gc_empty_list),
        ("get_referents", gc_empty_list),
        ("get_stats", gc_get_stats),
        ("is_tracked", gc_false),
        ("is_finalized", gc_false),
    ];
    for (n, f) in fns {
        set_fn(it, &d, n, *f);
    }
    dict_set_str(&d, "garbage", Value::list(Vec::new()));
    dict_set_str(&d, "callbacks", Value::list(Vec::new()));
    for (n, v) in [("DEBUG_STATS", 1), ("DEBUG_COLLECTABLE", 2), ("DEBUG_UNCOLLECTABLE", 4), ("DEBUG_SAVEALL", 32), ("DEBUG_LEAK", 38)] {
        dict_set_str(&d, n, Value::Int(v));
    }
    m
}

// ---- atexit -------------------------------------------------------------------------------------

fn atexit_register(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    if a.is_empty() {
        return Err(it.type_error("register() takes at least 1 argument (0 given)"));
    }
    if !it.is_callable(&a[0]) {
        return Err(it.type_error("the first argument must be callable"));
    }
    it.atexit.push((a[0].clone(), a[1..].to_vec(), kw.to_vec()));
    Ok(a[0].clone())
}

fn atexit_unregister(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("unregister", a, 1, 1)?;
    let mut keep = Vec::new();
    for entry in std::mem::take(&mut it.atexit) {
        if !it.values_eq(&entry.0, &a[0])? {
            keep.push(entry);
        }
    }
    it.atexit = keep;
    Ok(Value::None)
}

fn atexit_clear(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    it.atexit.clear();
    Ok(Value::None)
}

fn atexit_ncallbacks(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Int(it.atexit.len() as i64))
}

fn atexit_run(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    it.run_atexit();
    Ok(Value::None)
}

impl Interp {
    /// Runs the registered exit functions, last registered first.
    pub fn run_atexit(&mut self) {
        while let Some((f, args, kw)) = self.atexit.pop() {
            if let Err(e) = self.call(&f, args, kw) {
                if self.exc_is(&e, "SystemExit") {
                    continue;
                }
                self.flush_out();
                let repr = self.repr_of(&f).unwrap_or_default();
                self.write_stderr(&format!("Exception ignored in atexit callback {}:\n", repr));
                let text = self.format_exception(&e);
                self.write_stderr(&text);
            }
        }
    }
}

pub fn make_atexit(it: &mut Interp) -> Obj {
    let m = it.new_module("atexit");
    let d = it.module_dict(&m);
    set_fn(it, &d, "register", atexit_register);
    set_fn(it, &d, "unregister", atexit_unregister);
    set_fn(it, &d, "_clear", atexit_clear);
    set_fn(it, &d, "_ncallbacks", atexit_ncallbacks);
    set_fn(it, &d, "_run_exitfuncs", atexit_run);
    m
}
