//! `_xxsubinterpreters`: interpreters that are isolated from each other. Each one is its own
//! [`Interp`] (modules, `sys`, builtins) running on its own OS thread, where `run_string` hands it
//! a script and the data it shares (see [`super::xid`]) and waits for the outcome.

use super::interpchanm;
use super::xid::{self, Shared};
use crate::object::*;
use crate::vm::{dict_set_str, Interp};
use lumen_os::channel::{Pop, Queue};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{JoinHandle, ThreadId};

const STACK_SIZE: usize = 1 << 27;

enum Job {
    Run { code: String, shared: Vec<(String, Shared)>, reply: Arc<Queue<Reply>> },
    Quit,
}

enum Reply {
    Done,
    Failed { name: Option<String>, msg: Option<String> },
}

struct Entry {
    id: i64,
    inbox: Arc<Queue<Job>>,
    running: Arc<AtomicBool>,
    /// Live ID objects; the interpreter is destroyed when the last one goes.
    refs: i64,
    thread: Option<JoinHandle<()>>,
    thread_id: ThreadId,
}

static INTERPRETERS: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
static SETTINGS: Mutex<Vec<(i64, Settings)>> = Mutex::new(Vec::new());

pub(crate) const FLAG_OBMALLOC: u32 = 1 << 5;
pub(crate) const FLAG_EXTENSIONS: u32 = 1 << 8;
pub(crate) const FLAG_THREADS: u32 = 1 << 10;
pub(crate) const FLAG_DAEMON_THREADS: u32 = 1 << 11;
pub(crate) const FLAG_FORK: u32 = 1 << 15;
pub(crate) const FLAG_EXEC: u32 = 1 << 16;

/// The `PyInterpreterConfig` an interpreter was created with, as `feature_flags` and `own_gil`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Settings {
    pub flags: u32,
    pub own_gil: bool,
}

impl Settings {
    /// The legacy configuration of the main interpreter and of `create(isolated=False)`.
    pub(crate) const LEGACY: Settings = Settings { flags: FLAG_OBMALLOC | FLAG_FORK | FLAG_EXEC | FLAG_THREADS | FLAG_DAEMON_THREADS, own_gil: false };
    /// `PyInterpreterConfig_OWN_GIL`: everything isolated, extension checks on.
    pub(crate) const ISOLATED: Settings = Settings { flags: FLAG_THREADS | FLAG_EXTENSIONS, own_gil: true };
}

thread_local! {
    static EXTENSIONS_OVERRIDE: Cell<i64> = const { Cell::new(0) };
}

/// The settings of interpreter `id`.
pub(crate) fn settings_of(id: i64) -> Settings {
    let all = SETTINGS.lock().unwrap_or_else(|e| e.into_inner());
    all.iter().find(|(i, _)| *i == id).map_or(Settings::LEGACY, |(_, s)| *s)
}

pub(crate) fn set_settings(id: i64, settings: Settings) {
    let mut all = SETTINGS.lock().unwrap_or_else(|e| e.into_inner());
    all.retain(|(i, _)| *i != id);
    all.push((id, settings));
}

/// `_imp._override_multi_interp_extensions_check`: sets the override of this interpreter
/// (-1 never, 1 always, 0 use the configuration) and returns the previous one.
pub(crate) fn override_extensions_check(flag: i64) -> i64 {
    EXTENSIONS_OVERRIDE.with(|c| c.replace(flag))
}

/// Whether this interpreter refuses extension modules that are not multi-phase.
pub(crate) fn extensions_check_enabled() -> bool {
    match EXTENSIONS_OVERRIDE.with(Cell::get) {
        0 => settings_of(current_interp_id()).flags & FLAG_EXTENSIONS != 0,
        n => n > 0,
    }
}
static NEXT_ID: AtomicI64 = AtomicI64::new(1);

thread_local! {
    static CURRENT: Cell<i64> = const { Cell::new(0) };
}

fn interpreters() -> MutexGuard<'static, Vec<Entry>> {
    INTERPRETERS.lock().unwrap_or_else(|e| e.into_inner())
}

/// The ID of the interpreter running on this thread (the main one is 0).
pub(crate) fn current_interp_id() -> i64 {
    CURRENT.with(Cell::get)
}

/// The IDs of every interpreter, oldest first (the main interpreter, 0, first).
pub(crate) fn interpreter_ids() -> Vec<i64> {
    let mut ids = vec![0];
    ids.extend(interpreters().iter().map(|e| e.id));
    ids
}

fn exists(id: i64) -> bool {
    id == 0 || interpreters().iter().any(|e| e.id == id)
}

fn incref(id: i64) -> bool {
    if id == 0 {
        return true;
    }
    match interpreters().iter_mut().find(|e| e.id == id) {
        Some(e) => {
            e.refs += 1;
            true
        }
        None => false,
    }
}

fn decref(id: i64) {
    let entry = {
        let mut all = interpreters();
        let Some(at) = all.iter().position(|e| e.id == id) else { return };
        all[at].refs -= 1;
        if all[at].refs > 0 {
            return;
        }
        all.remove(at)
    };
    finish(entry);
}

/// Ends the interpreter's thread (run outside the registry lock: the thread may need it).
fn finish(mut entry: Entry) {
    SETTINGS.lock().unwrap_or_else(|e| e.into_inner()).retain(|(i, _)| *i != entry.id);
    let _ = entry.inbox.push(Job::Quit);
    entry.inbox.close();
    if let Some(handle) = entry.thread.take() {
        if std::thread::current().id() != entry.thread_id {
            let _ = handle.join();
        }
    }
}

fn failure(it: &mut Interp, exc: &Obj) -> Reply {
    let ty = Value::Obj(it.type_of_obj(exc));
    let name = it.str_of(&ty).ok();
    let msg = it.str_of(&Value::Obj(exc.clone())).ok();
    Reply::Failed { name, msg }
}

fn run_job(it: &mut Interp, globals: &Obj, code: &str, shared: &[(String, Shared)]) -> Reply {
    for (name, data) in shared {
        match xid::unshare(it, data) {
            Ok(v) => dict_set_str(globals, name, v),
            Err(e) => return failure(it, &e),
        }
    }
    let outcome = match it.compile_source(code, "<string>") {
        Ok(c) => it.run_code(c, globals.clone(), globals.clone()),
        Err(e) => Err(e),
    };
    super::iom::flush_std_streams(it);
    it.flush_out();
    match outcome {
        Ok(_) => Reply::Done,
        Err(e) => failure(it, &e),
    }
}

fn interpreter_main(id: i64, inbox: Arc<Queue<Job>>, running: Arc<AtomicBool>, path: Vec<String>, ready: Arc<Queue<Reply>>) {
    CURRENT.with(|c| c.set(id));
    let mut it = Interp::new();
    it.set_path(&path);
    it.set_argv(&[String::new()]);
    let main = it.new_module("__main__");
    let globals = it.module_dict(&main);
    dict_set_str(&globals, "__builtins__", Value::Obj(it.builtins.clone()));
    it.register_module("__main__", &main);
    it.main_globals = Some(globals.clone());
    let _ = ready.push(Reply::Done);
    loop {
        match inbox.pop_wait(None) {
            Pop::Message(Job::Run { code, shared, reply }) => {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_job(&mut it, &globals, &code, &shared)));
                let outcome = outcome.unwrap_or(Reply::Failed { name: None, msg: Some("interpreter crashed".to_string()) });
                running.store(false, Ordering::SeqCst);
                let _ = reply.push(outcome);
            }
            Pop::Message(Job::Quit) | Pop::Closed => break,
            Pop::Empty => {}
        }
    }
    it.run_atexit();
    super::iom::flush_std_streams(&mut it);
    it.flush_out();
    drop(globals);
    drop(main);
    interpchanm::drop_interpreter(id);
}

fn current_sys_path(it: &mut Interp) -> Vec<String> {
    let Some(sys) = it.sys_module.clone() else { return Vec::new() };
    let d = it.module_dict(&sys);
    match crate::vm::dict_get_str(&d, "path") {
        Some(p) => list_of(&p).map(|l| l.borrow().iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default(),
        None => Vec::new(),
    }
}

/// An ID object for interpreter `id`, which keeps the interpreter alive.
pub(crate) fn new_interp_id(it: &mut Interp, id: i64) -> R<Value> {
    if !incref(id) {
        return Err(it.runtime_error(&format!("unrecognized interpreter ID {id}")));
    }
    Ok(crate::bind::Py::new(it, _xxsubinterpreters::InterpreterID { id, counted: true }).into_value())
}

/// This module provides primitive operations to manage Python interpreters.
/// The 'interpreters' module provides a more convenient interface.
#[lumen_bind::module(name = "_xxsubinterpreters")]
pub mod _xxsubinterpreters {
    use super::*;
    use crate::bind::type_object;
    use crate::builtins::native::{new_type, with_opaque};

    #[derive(Default)]
    struct State {
        run_failed: Option<Obj>,
    }

    /// The interpreter ID an `id` argument names (an InterpreterID or an int).
    fn interp_id(it: &mut Interp, v: &Value) -> R<i64> {
        if let Some(id) = with_opaque::<InterpreterID, _>(v, |c| c.id) {
            return Ok(id);
        }
        if it.has_index(v) {
            let n = it.index_of(v)?;
            if n < 0 {
                let r = it.repr_of(v)?;
                return Err(it.value_error(&format!("interpreter ID must be a non-negative int, got {r}")));
            }
            return Ok(n);
        }
        let t = it.tp_name_of(v);
        Err(it.type_error(&format!("interpreter ID must be an int, got {t}")))
    }

    fn lookup(it: &mut Interp, v: &Value) -> R<i64> {
        let id = interp_id(it, v)?;
        if !exists(id) {
            return Err(it.runtime_error(&format!("unrecognized interpreter ID {id}")));
        }
        Ok(id)
    }

    /// A unique interpreter ID object. Its last reference destroys the interpreter.
    #[class(name = "InterpreterID", module = "_xxsubinterpreters")]
    pub struct InterpreterID {
        pub(crate) id: i64,
        pub(crate) counted: bool,
    }

    impl Drop for InterpreterID {
        fn drop(&mut self) {
            if self.counted {
                decref(self.id);
            }
        }
    }

    impl InterpreterID {
        fn equals(&self, it: &mut Interp, other: &Value, op: crate::ast::CmpOp) -> R<Value> {
            use crate::ast::CmpOp;
            let equal = if let Some(id) = with_opaque::<InterpreterID, _>(other, |c| c.id) {
                self.id == id
            } else {
                match other {
                    Value::Int(i) => *i == self.id,
                    Value::Bool(b) => i64::from(*b) == self.id,
                    Value::Obj(o) if matches!(o.kind, Kind::Int(_)) => false,
                    Value::Float(_) => return it.rich_compare(op, &Value::Int(self.id), other),
                    Value::Obj(o) if matches!(o.kind, Kind::Complex(..)) => return it.rich_compare(op, &Value::Int(self.id), other),
                    _ => return Ok(Value::NotImplemented),
                }
            };
            Ok(Value::Bool(equal == (op == CmpOp::Eq)))
        }
    }

    #[methods]
    impl InterpreterID {
        #[constructor]
        fn new(it: &mut Interp, #[kw] id: &Value, #[kwonly] #[default(false)] force: bool) -> R<InterpreterID> {
            let id = interp_id(it, id)?;
            if !force && !incref(id) {
                return Err(it.runtime_error(&format!("unrecognized interpreter ID {id}")));
            }
            Ok(InterpreterID { id, counted: !force })
        }

        #[proto(repr)]
        fn __repr__(&self) -> String {
            format!("InterpreterID({})", self.id)
        }

        #[proto(str)]
        fn __str__(&self) -> String {
            self.id.to_string()
        }

        #[proto(hash)]
        fn __hash__(&self) -> i64 {
            self.id
        }

        #[proto(int)]
        fn __int__(&self) -> i64 {
            self.id
        }

        #[proto(index)]
        fn __index__(&self) -> i64 {
            self.id
        }

        #[proto(eq)]
        fn __eq__(&self, it: &mut Interp, other: &Value) -> R<Value> {
            self.equals(it, other, crate::ast::CmpOp::Eq)
        }

        #[proto(ne)]
        fn __ne__(&self, it: &mut Interp, other: &Value) -> R<Value> {
            self.equals(it, other, crate::ast::CmpOp::NotEq)
        }

        #[getter]
        fn id(&self) -> i64 {
            self.id
        }
    }

    /// create() -> ID
    ///
    /// Create a new interpreter and return a unique generated ID.
    #[op]
    fn create(it: &mut Interp, #[kwonly] #[default(true)] isolated: bool) -> R<Value> {
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        set_settings(id, if isolated { Settings::ISOLATED } else { Settings::LEGACY });
        let inbox = Arc::new(Queue::new());
        let running = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(Queue::new());
        let path = current_sys_path(it);
        let spawned = std::thread::Builder::new().stack_size(STACK_SIZE).spawn({
            let (inbox, running, ready) = (inbox.clone(), running.clone(), ready.clone());
            move || interpreter_main(id, inbox, running, path, ready)
        });
        let handle = match spawned {
            Ok(h) => h,
            Err(_) => return Err(it.runtime_error("interpreter creation failed")),
        };
        let _ = ready.pop_wait(None);
        let thread_id = handle.thread().id();
        interpreters().push(Entry { id, inbox, running, refs: 0, thread: Some(handle), thread_id });
        new_interp_id(it, id)
    }

    /// destroy(id)
    ///
    /// Destroy the identified interpreter.
    ///
    /// Attempting to destroy the current interpreter results in a RuntimeError.
    /// So does an unrecognized ID.
    #[op]
    fn destroy(it: &mut Interp, #[kw] id: &Value) -> R<()> {
        let id = lookup(it, id)?;
        if id == current_interp_id() {
            return Err(it.runtime_error("cannot destroy the current interpreter"));
        }
        let entry = {
            let mut all = interpreters();
            let Some(at) = all.iter().position(|e| e.id == id) else { return Err(it.runtime_error("interpreter running")) };
            if all[at].running.load(Ordering::SeqCst) {
                return Err(it.runtime_error("interpreter running"));
            }
            all.remove(at)
        };
        finish(entry);
        Ok(())
    }

    /// list_all() -> [ID]
    ///
    /// Return a list containing the ID of every existing interpreter.
    #[op]
    fn list_all(it: &mut Interp) -> R<Value> {
        let mut out = Vec::new();
        for id in interpreter_ids() {
            out.push(new_interp_id(it, id)?);
        }
        Ok(Value::list(out))
    }

    /// get_current() -> ID
    ///
    /// Return the ID of current interpreter.
    #[op]
    fn get_current(it: &mut Interp) -> R<Value> {
        new_interp_id(it, current_interp_id())
    }

    /// get_main() -> ID
    ///
    /// Return the ID of main interpreter.
    #[op]
    fn get_main(it: &mut Interp) -> R<Value> {
        new_interp_id(it, 0)
    }

    /// is_running(id) -> bool
    ///
    /// Return whether or not the identified interpreter is running.
    #[op]
    fn is_running(it: &mut Interp, #[kw] id: &Value) -> R<bool> {
        let id = lookup(it, id)?;
        if id == 0 {
            return Ok(true);
        }
        Ok(interpreters().iter().find(|e| e.id == id).is_some_and(|e| e.running.load(Ordering::SeqCst)))
    }

    /// run_string(id, script, shared)
    ///
    /// Execute the provided string in the identified interpreter.
    ///
    /// See PyRun_SimpleStrings.
    #[op]
    fn run_string(it: &mut Interp, #[kw] id: &Value, #[kw] script: &str, #[kw] shared: Option<&Value>) -> R<()> {
        let id = lookup(it, id)?;
        if script.contains('\0') {
            return Err(it.value_error("source code string cannot contain null bytes"));
        }
        let mut data = Vec::new();
        if let Some(ns) = shared.filter(|s| !s.is_none()) {
            let items = it.call_method(ns, "items", Vec::new())?;
            for pair in it.iterate_to_vec(&items)? {
                let (Some(key), Some(value)) = (pair.tuple_items().and_then(|t| t.first()), pair.tuple_items().and_then(|t| t.get(1))) else { continue };
                let Some(name) = key.as_str() else {
                    let t = it.tp_name_of(key);
                    return Err(it.type_error(&format!("bad argument type for built-in operation: {t}")));
                };
                data.push((name.to_string(), xid::share(it, value)?));
            }
        }
        let target = {
            let all = interpreters();
            all.iter().find(|e| e.id == id).filter(|e| e.running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok()).map(|e| e.inbox.clone())
        };
        let Some(inbox) = target else { return Err(it.runtime_error("interpreter already running")) };
        let reply = Arc::new(Queue::new());
        let job = Job::Run { code: script.to_string(), shared: data, reply: reply.clone() };
        if inbox.push(job).is_err() {
            return Err(it.runtime_error("interpreter already running"));
        }
        let outcome = loop {
            match reply.pop_wait(None) {
                Pop::Message(r) => break r,
                Pop::Closed => break Reply::Failed { name: None, msg: Some("interpreter crashed".to_string()) },
                Pop::Empty => {}
            }
        };
        match outcome {
            Reply::Done => Ok(()),
            Reply::Failed { name, msg } => {
                let cls = it.native_state::<State>().run_failed.clone().unwrap_or_else(|| it.exc_type("RuntimeError"));
                let args = match (name, msg) {
                    (Some(n), Some(m)) => vec![Value::string(format!("{n}: {m}"))],
                    (Some(s), None) | (None, Some(s)) => vec![Value::string(s)],
                    (None, None) => Vec::new(),
                };
                Err(it.new_exc(&cls, args))
            }
        }
    }

    /// is_shareable(obj) -> bool
    ///
    /// Return True if the object's data may be shared between interpreters and
    /// False otherwise.
    #[op]
    fn is_shareable(it: &mut Interp, #[kw] obj: &Value) -> bool {
        xid::share(it, obj).is_ok()
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        let runtime = it.exc_type("RuntimeError");
        let failed = new_type(it, "_xxsubinterpreters", "RunFailedError", Some(&runtime), Layout::Exception);
        it.native_state::<State>().run_failed = Some(failed.clone());
        dict_set_str(&d, "RunFailedError", Value::Obj(failed));
        let id = type_object::<InterpreterID>(it);
        dict_set_str(&d, "InterpreterID", Value::Obj(id));
    }
}
