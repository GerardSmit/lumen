//! `_testcapi` thread-state, pending-call, sub-interpreter and cross-interpreter helpers. The
//! interpreter runs on one thread, so a callback "run in another C thread" runs on this thread
//! under another thread identifier; sub-interpreters are the real ones of `_xxsubinterpreters`.

#![allow(non_snake_case)]

use super::getargsm::{parse_tuple_and_keywords, Slot};
use crate::bind::KwArgs;
use crate::builtins::native::with_opaque;
use crate::builtins::subinterpm::{set_settings, Settings, FLAG_DAEMON_THREADS, FLAG_EXEC, FLAG_EXTENSIONS, FLAG_FORK, FLAG_OBMALLOC, FLAG_THREADS};
use crate::builtins::xid::{share, unshare, Shared};
use super::call_builtin;
use crate::object::*;
use crate::vm::Interp;
use std::sync::mpsc::{channel, Sender};
use std::sync::Mutex;

static WAITER: Mutex<Option<Sender<()>>> = Mutex::new(None);

fn callable_arg(it: &mut Interp, v: &Value, func: &str) -> R<()> {
    if it.is_callable(v) {
        Ok(())
    } else {
        let t = it.tp_name_of(v);
        let _ = func;
        Err(it.type_error(&format!("'{t}' object is not callable")))
    }
}

/// Runs `callback` as the thread `ident`; the identity is restored afterwards.
fn call_as_thread(it: &mut Interp, callback: &Value, ident: i64) -> R<Value> {
    let saved = std::mem::replace(&mut it.thread_ident, ident);
    let r = it.call(callback, Vec::new(), Vec::new());
    it.thread_ident = saved;
    r
}

fn report_thread_error(it: &mut Interp, e: &Obj) {
    it.flush_out();
    let text = it.format_exception(e);
    it.write_stderr(&text);
}

#[lumen_bind::class(module = "builtins", name = "PyCapsule")]
pub struct Capsule {
    data: Shared,
}

#[lumen_bind::methods]
impl Capsule {
    #[constructor]
    fn new(it: &mut Interp) -> R<Capsule> {
        Err(it.type_error("cannot create 'PyCapsule' instances"))
    }
}

/// Runs `code` in a new sub-interpreter; 0 on success, -1 (after reporting) on an uncaught error.
fn run_in_new_interpreter(it: &mut Interp, code: &str, settings: Settings) -> R<i64> {
    let m = it.import_module("_xxsubinterpreters")?;
    let m = Value::Obj(m);
    let create = it.get_attr_str(&m, "create")?;
    let key = it.str_obj("isolated");
    let id = match it.call(&create, Vec::new(), vec![(key, Value::Bool(false))]) {
        Ok(id) => id,
        Err(e) => return Err(creation_failed(it, e)),
    };
    let number = call_builtin(it, "int", vec![id.clone()])?;
    if let Value::Int(n) = number {
        set_settings(n, settings);
    }
    let run = it.get_attr_str(&m, "run_string")?;
    let outcome = it.call(&run, vec![id.clone(), Value::str(code)], Vec::new());
    let destroy = it.get_attr_str(&m, "destroy")?;
    let _ = it.call(&destroy, vec![id], Vec::new());
    match outcome {
        Ok(_) => Ok(0),
        Err(e) => {
            report_thread_error(it, &e);
            Ok(-1)
        }
    }
}

fn creation_failed(it: &mut Interp, cause: Obj) -> Obj {
    let e = it.new_exc_str("RuntimeError", "sub-interpreter creation failed");
    let _ = it.set_attr_str(&Value::Obj(e.clone()), "__context__", Value::Obj(cause));
    e
}

fn config_flag(slots: &[Slot], i: usize) -> i64 {
    match slots.get(i).and_then(|s| s.out.clone()) {
        Some(Value::Bool(b)) => i64::from(b),
        Some(Value::Int(n)) => n,
        _ => -1,
    }
}

#[lumen_bind::module(name = "_testcapi")]
pub mod threadsm {
    use super::*;

    /// _test_thread_state(callback): call `callback` from this thread and from threads of its own.
    #[op(hint(py(arg_style = "parse", arg_name = "test_thread_state")))]
    fn _test_thread_state(it: &mut Interp, callback: &Value) -> R<()> {
        callable_arg(it, callback, "test_thread_state")?;
        let main = it.thread_ident;
        let mut failure: Option<Obj> = None;
        if let Err(e) = call_as_thread(it, callback, main + 1) {
            report_thread_error(it, &e);
        }
        for _ in 0..2 {
            if let Err(e) = call_as_thread(it, callback, main) {
                failure.get_or_insert(e);
            }
        }
        if let Err(e) = call_as_thread(it, callback, main + 2) {
            report_thread_error(it, &e);
        }
        if let Err(e) = call_as_thread(it, callback, main) {
            failure.get_or_insert(e);
        }
        match failure {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    #[op]
    fn gilstate_ensure_release() {}

    /// crash_no_current_thread(): `PyThreadState_Get()` with the GIL released.
    #[op]
    fn crash_no_current_thread(it: &mut Interp) {
        it.flush_out();
        it.write_stderr(
            "Fatal Python error: PyThreadState_Get: the function must be called with the GIL held, after Python initialization and before Python finalization, but the GIL is released (the current Python thread state is NULL)\nPython runtime state: initialized\n",
        );
        std::process::abort()
    }

    /// _pending_threadfunc(callable): schedule `callable()` as a pending call.
    #[op]
    fn _pending_threadfunc(it: &mut Interp, callable: &Value) -> R<bool> {
        it.call(callable, Vec::new(), Vec::new())?;
        Ok(true)
    }

    /// call_in_temporary_c_thread(callback, wait=True): call `callback` from a new C thread.
    #[op(hint(py(arg_style = "parse", arg_name = "call_in_temporary_c_thread")))]
    fn call_in_temporary_c_thread(it: &mut Interp, callback: &Value, wait: Option<i64>) -> R<()> {
        let _ = wait;
        let ident = it.thread_ident + 1;
        if let Err(e) = call_as_thread(it, callback, ident) {
            report_thread_error(it, &e);
        }
        Ok(())
    }

    #[op]
    fn join_temporary_c_thread() {}

    /// _spawn_pthread_waiter(): start a thread the `threading` module does not know about.
    #[op]
    fn _spawn_pthread_waiter(it: &mut Interp) -> R<()> {
        let mut slot = WAITER.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_some() {
            return Err(it.runtime_error("thread already running"));
        }
        let (tx, rx) = channel::<()>();
        std::thread::spawn(move || {
            let _ = rx.recv();
        });
        *slot = Some(tx);
        Ok(())
    }

    #[op]
    fn _end_spawned_pthread(it: &mut Interp) -> R<()> {
        let mut slot = WAITER.lock().unwrap_or_else(|p| p.into_inner());
        match slot.take() {
            Some(tx) => {
                let _ = tx.send(());
                Ok(())
            }
            None => Err(it.runtime_error("call _spawn_pthread_waiter 1st")),
        }
    }

    /// run_in_subinterp(code): `Py_NewInterpreter`, `PyRun_SimpleString`, `Py_EndInterpreter`.
    #[op(hint(py(arg_style = "parse", arg_name = "run_in_subinterp")))]
    fn run_in_subinterp(it: &mut Interp, code: &str) -> R<i64> {
        run_in_new_interpreter(it, code, Settings::LEGACY)
    }

    /// run_in_subinterp_with_config(code, *, use_main_obmalloc, allow_fork, allow_exec,
    /// allow_threads, allow_daemon_threads, check_multi_interp_extensions, gil)
    #[op(hint(py(arg_style = "parse", arg_name = "run_in_subinterp_with_config")))]
    fn run_in_subinterp_with_config(it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<i64> {
        let names = ["code", "use_main_obmalloc", "allow_fork", "allow_exec", "allow_threads", "allow_daemon_threads", "check_multi_interp_extensions", "gil"];
        let kwlist: Vec<String> = names.iter().map(|n| (*n).to_string()).collect();
        let kw = if kwargs.is_empty() { None } else { Some(it.kwargs_to_dict(&kwargs.to_vec())?) };
        let mut slots: Vec<Slot> = Vec::new();
        parse_tuple_and_keywords(it, args, kw.as_ref(), "s$ppppppi:run_in_subinterp_with_config", &kwlist, &mut slots, false)?;
        let code = match slots.first().and_then(|s| s.out.clone()) {
            Some(v) => v.as_str().unwrap_or("").to_string(),
            None => String::new(),
        };
        for i in [1, 2, 3, 4, 7, 5, 6] {
            let name = names[i];
            if config_flag(&slots, i) < 0 {
                return Err(it.value_error(&format!("missing {name}")));
            }
        }
        let mut flags = 0;
        for (i, flag) in [(1, FLAG_OBMALLOC), (2, FLAG_FORK), (3, FLAG_EXEC), (4, FLAG_THREADS), (5, FLAG_DAEMON_THREADS), (6, FLAG_EXTENSIONS)] {
            if config_flag(&slots, i) != 0 {
                flags |= flag;
            }
        }
        if flags & FLAG_OBMALLOC == 0 && flags & FLAG_EXTENSIONS == 0 {
            let cause = it.new_exc_str("RuntimeError", "per-interpreter obmalloc does not support single-phase init extension modules");
            return Err(creation_failed(it, cause));
        }
        run_in_new_interpreter(it, &code, Settings { flags, own_gil: config_flag(&slots, 7) == 2 })
    }

    /// get_crossinterp_data(obj) -> capsule
    #[op(hint(py(arg_style = "parse", arg_name = "get_crossinterp_data")))]
    fn get_crossinterp_data(it: &mut Interp, obj: &Value) -> R<Value> {
        let data = share(it, obj)?;
        let ty = crate::bind::type_object::<Capsule>(it);
        Ok(crate::bind::opaque_instance(&ty, Capsule { data }))
    }

    /// restore_crossinterp_data(capsule) -> obj
    #[op(hint(py(arg_style = "parse", arg_name = "restore_crossinterp_data")))]
    fn restore_crossinterp_data(it: &mut Interp, capsule: &Value) -> R<Value> {
        match with_opaque::<Capsule, _>(capsule, |c| c.data.clone()) {
            Some(data) => unshare(it, &data),
            None => Err(it.value_error("PyCapsule_GetPointer called with invalid PyCapsule object")),
        }
    }
}
