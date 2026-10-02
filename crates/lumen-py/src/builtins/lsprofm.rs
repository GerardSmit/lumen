//! `_lsprof`: the profiler behind `cProfile`, a port of CPython's `Modules/_lsprof.c` on top of
//! `sys.setprofile` (the call, return and C-function events of the monitoring engine).

use super::sysextra::{structseq_full, structseq_type};
use crate::bind::Py;
use crate::object::*;
use crate::trace::ev;
use crate::vm::{dict_set_str, Interp};
use std::collections::HashMap;

/// The profiler counts nanoseconds.
const TIME_UNIT: f64 = 1e-9;
const PROFILER_ID: i64 = 2;

const CALLBACKS: [(u32, &str); 9] = [
    (ev::PY_START, "_pystart_callback"),
    (ev::PY_RESUME, "_pystart_callback"),
    (ev::PY_THROW, "_pythrow_callback"),
    (ev::PY_RETURN, "_pyreturn_callback"),
    (ev::PY_YIELD, "_pyreturn_callback"),
    (ev::PY_UNWIND, "_pyreturn_callback"),
    (ev::CALL, "_ccall_callback"),
    (ev::C_RETURN, "_creturn_callback"),
    (ev::C_RAISE, "_creturn_callback"),
];

type Key = (usize, usize);

struct SubEntry {
    callee: usize,
    callcount: i64,
    recursive: i64,
    total: i64,
    inline: i64,
    level: i64,
}

struct Entry {
    user: Value,
    callcount: i64,
    recursive: i64,
    total: i64,
    inline: i64,
    level: i64,
    calls: Vec<SubEntry>,
}

struct Context {
    entry: usize,
    start: i64,
    sub_time: i64,
}

struct Stats;
struct SubStats;

impl Entry {
    fn new(user: Value) -> Entry {
        Entry { user, callcount: 0, recursive: 0, total: 0, inline: 0, level: 0, calls: Vec::new() }
    }

    fn call_to(&mut self, callee: usize) -> &mut SubEntry {
        let at = match self.calls.iter().position(|c| c.callee == callee) {
            Some(i) => i,
            None => {
                self.calls.push(SubEntry { callee, callcount: 0, recursive: 0, total: 0, inline: 0, level: 0 });
                self.calls.len() - 1
            }
        };
        &mut self.calls[at]
    }
}

fn code_key(code: &Value) -> Option<Key> {
    match code {
        Value::Obj(o) if matches!(o.kind, Kind::Code(_)) => Some((std::rc::Rc::as_ptr(o) as usize, 0)),
        _ => None,
    }
}

/// The identity of a built-in function or bound built-in method, shared by every bound copy.
fn function_key(callable: &Value) -> Option<Key> {
    let Value::Obj(o) = callable else { return None };
    let native = match &o.kind {
        Kind::Native(n) => n,
        Kind::Method(Value::Obj(f), _) => match &f.kind {
            Kind::Native(n) => n,
            _ => return None,
        },
        _ => return None,
    };
    Some((native.f as usize, native.name.as_ptr() as usize))
}

pub struct State {
    timer: Value,
    timeunit: f64,
    subcalls: bool,
    builtins: bool,
    enabled: bool,
    in_timer: bool,
    entries: Vec<Entry>,
    index: HashMap<Key, usize>,
    stack: Vec<Context>,
}

impl State {
    fn new() -> State {
        State {
            timer: Value::None,
            timeunit: 0.0,
            subcalls: true,
            builtins: true,
            enabled: false,
            in_timer: false,
            entries: Vec::new(),
            index: HashMap::new(),
            stack: Vec::new(),
        }
    }

    fn entry_for(&mut self, key: Key, user: &Value) -> usize {
        if let Some(&i) = self.index.get(&key) {
            return i;
        }
        self.entries.push(Entry::new(user.clone()));
        let i = self.entries.len() - 1;
        self.index.insert(key, i);
        i
    }

    fn enter(&mut self, key: Key, user: &Value, now: i64) {
        let idx = self.entry_for(key, user);
        let caller = self.stack.last().map(|c| c.entry);
        self.entries[idx].level += 1;
        if let (true, Some(caller)) = (self.subcalls, caller) {
            self.entries[caller].call_to(idx).level += 1;
        }
        self.stack.push(Context { entry: idx, start: now, sub_time: 0 });
    }

    fn leave(&mut self, key: Key, now: i64) {
        let Some(ctx) = self.stack.pop() else { return };
        if let Some(idx) = self.index.get(&key).copied() {
            self.stop(ctx, idx, now);
        }
    }

    fn stop(&mut self, ctx: Context, idx: usize, now: i64) {
        let total = now - ctx.start;
        let inline = total - ctx.sub_time;
        let caller = self.stack.last_mut().map(|c| {
            c.sub_time += total;
            c.entry
        });
        let e = &mut self.entries[idx];
        e.level -= 1;
        if e.level == 0 {
            e.total += total;
        } else {
            e.recursive += 1;
        }
        e.inline += inline;
        e.callcount += 1;
        if let (true, Some(caller)) = (self.subcalls, caller) {
            let sub = self.entries[caller].call_to(idx);
            sub.level -= 1;
            if sub.level == 0 {
                sub.total += total;
            } else {
                sub.recursive += 1;
            }
            sub.inline += inline;
            sub.callcount += 1;
        }
    }

    fn flush_one(&mut self, now: i64) {
        if let Some(ctx) = self.stack.pop() {
            let idx = ctx.entry;
            self.stop(ctx, idx, now);
        }
    }

    fn factor(&self) -> f64 {
        if self.timer.is_none() || self.timeunit <= 0.0 {
            TIME_UNIT
        } else {
            self.timeunit
        }
    }
}

#[lumen_bind::module(name = "_lsprof")]
pub mod _lsprof {
    use super::*;
    use crate::bind::{KwArgs, This};

    /// Builds a profiler object.
    ///
    ///   timer
    ///     A callable that returns the current time.  If not given, a
    ///     built-in time source is used.
    ///   timeunit
    ///     The unit of the values `timer` returns.  If 0, the values are
    ///     seconds as floats.
    ///   subcalls
    ///     If True, also records for each function statistics separated
    ///     according to its current caller.
    ///   builtins
    ///     If True, records the time spent in built-in functions
    ///     separately from their caller.
    #[class(name = "Profiler")]
    pub struct Profiler {
        pub(super) st: State,
    }

    /// The current time in nanoseconds; a failing timer reads as 0 and is reported as unraisable.
    fn clock(it: &mut Interp, p: &Py<Profiler>) -> R<i64> {
        let (timer, unit) = p.with(it, |s| (s.timer.clone(), s.timeunit))?;
        if timer.is_none() {
            return Ok(it.platform.borrow().monotonic_ns() as i64);
        }
        p.with(it, |s| s.in_timer = true)?;
        let r = it.call(&timer, Vec::new(), Vec::new());
        p.with(it, |s| s.in_timer = false)?;
        match r.and_then(|r| timer_ns(it, &r, unit > 0.0)) {
            Ok(ns) => Ok(ns),
            Err(e) => {
                let r = it.repr_of(&timer).unwrap_or_default();
                it.write_unraisable(&e, Some(&format!("Exception ignored while calling _lsprof timer {r}")), None);
                Ok(0)
            }
        }
    }

    fn timer_ns(it: &mut Interp, r: &Value, integral: bool) -> R<i64> {
        if integral {
            if !r.is_int_like() {
                let t = it.type_name_of(r);
                return Err(it.type_error(&format!("'{t}' object cannot be interpreted as an integer")));
            }
            return it.index_of(r);
        }
        let secs = match r {
            Value::Float(f) => *f,
            _ if r.is_int_like() => it.index_of(r)? as f64,
            _ => {
                let t = it.type_name_of(r);
                return Err(it.type_error(&format!("'{t}' object cannot be interpreted as an integer or float")));
            }
        };
        if secs.is_nan() {
            return Err(it.value_error("Invalid value NaN (not a number)"));
        }
        let ns = (secs * 1e9).floor();
        if !(i64::MIN as f64..i64::MAX as f64).contains(&ns) {
            return Err(it.overflow_err("timestamp too large to convert to C PyTime_t"));
        }
        Ok(ns as i64)
    }

    fn missing(it: &mut Interp) -> Value {
        it.monitoring_sentinels().1
    }

    /// The profiled object of a CALL / C_RETURN / C_RAISE event: a built-in function or method;
    /// a method descriptor is bound to the first argument (the receiver).
    fn cfunction(it: &mut Interp, callable: &Value, self_arg: &Value) -> Option<(Key, Value)> {
        match it.type_name_of(callable).as_str() {
            "builtin_function_or_method" => function_key(callable).map(|k| (k, callable.clone())),
            "method_descriptor" => {
                if self_arg.is(&missing(it)) {
                    return None;
                }
                let bound = Value::Obj(Object::new(Kind::Method(callable.clone(), self_arg.clone())));
                function_key(&bound).map(|k| (k, bound))
            }
            _ => None,
        }
    }

    fn enter_call(it: &mut Interp, p: &Py<Profiler>, key: Key, user: &Value) -> R<()> {
        if p.with(it, |s| s.in_timer)? {
            return Ok(());
        }
        let now = clock(it, p)?;
        p.with(it, |s| s.enter(key, user, now))
    }

    fn leave_call(it: &mut Interp, p: &Py<Profiler>, key: Key) -> R<()> {
        if p.with(it, |s| s.in_timer || s.stack.is_empty())? {
            return Ok(());
        }
        let now = clock(it, p)?;
        p.with(it, |s| s.leave(key, now))
    }

    /// The label `getstats` reports for a profiled object: the code object itself, or a string
    /// describing a built-in function or method.
    fn describe(it: &mut Interp, user: &Value) -> R<Value> {
        let Value::Obj(o) = user else { return Ok(user.clone()) };
        let (native, this) = match &o.kind {
            Kind::Native(n) => (n, None),
            Kind::Method(Value::Obj(f), this) => match &f.kind {
                Kind::Native(n) => (n, Some(this.clone())),
                _ => return Ok(user.clone()),
            },
            _ => return Ok(user.clone()),
        };
        if let Some(this) = this {
            let ty = it.type_of(&this);
            if let Some(m) = it.lookup_mro(&ty, native.name) {
                if let Ok(r) = it.repr_of(&m) {
                    return Ok(Value::string(r));
                }
            }
            return Ok(Value::string(format!("<built-in method {}>", native.name)));
        }
        let module = native
            .desc
            .and_then(|d| d.module())
            .map(|m| m.to_string())
            .or_else(|| match &native.owner {
                Some(NativeOwner::Module(m)) => Some(m.to_string()),
                _ => None,
            })
            .unwrap_or_else(|| "builtins".to_string());
        Ok(Value::string(format!("<built-in method {}.{}>", module, native.name)))
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) -> R<()> {
        let Value::Obj(m) = m else { return Ok(()) };
        let d = it.module_dict(m);
        let (entry, sub) = stats_types(it);
        dict_set_str(&d, "profiler_entry", Value::Obj(entry));
        dict_set_str(&d, "profiler_subentry", Value::Obj(sub));
        Ok(())
    }

    fn stats_types(it: &mut Interp) -> (Obj, Obj) {
        let entry = structseq_type::<Stats>(it, "_lsprof", "profiler_entry", &["code", "callcount", "reccallcount", "totaltime", "inlinetime", "calls"], 6);
        let sub = structseq_type::<SubStats>(it, "_lsprof", "profiler_subentry", &["code", "callcount", "reccallcount", "totaltime", "inlinetime"], 5);
        (entry, sub)
    }

    #[methods]
    impl Profiler {
        #[constructor]
        fn new(#[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> Profiler {
            let _ = (args, kwargs);
            Profiler { st: State::new() }
        }

        #[proto(init)]
        fn __init__(
            slf: This<Py<Self>>,
            it: &mut Interp,
            #[kw] timer: Option<&Value>,
            #[kw]
            #[default(0.0)]
            timeunit: f64,
            #[kw]
            #[default(true)]
            subcalls: bool,
            #[kw]
            #[default(true)]
            builtins: bool,
        ) -> R<()> {
            let timer = timer.cloned().unwrap_or(Value::None);
            slf.0.with(it, |s| {
                s.timer = timer;
                s.timeunit = timeunit;
                s.subcalls = subcalls;
                s.builtins = builtins;
            })
        }

        fn _pystart_callback(slf: This<Py<Self>>, it: &mut Interp, code: &Value, instruction_offset: &Value) -> R<()> {
            let _ = instruction_offset;
            if let Some(key) = code_key(code) {
                enter_call(it, &slf.0, key, code)?;
            }
            Ok(())
        }

        fn _pythrow_callback(slf: This<Py<Self>>, it: &mut Interp, code: &Value, instruction_offset: &Value, exception: &Value) -> R<()> {
            let _ = (instruction_offset, exception);
            if let Some(key) = code_key(code) {
                enter_call(it, &slf.0, key, code)?;
            }
            Ok(())
        }

        fn _pyreturn_callback(slf: This<Py<Self>>, it: &mut Interp, code: &Value, instruction_offset: &Value, retval: &Value) -> R<()> {
            let _ = (instruction_offset, retval);
            if let Some(key) = code_key(code) {
                leave_call(it, &slf.0, key)?;
            }
            Ok(())
        }

        fn _ccall_callback(slf: This<Py<Self>>, it: &mut Interp, code: &Value, instruction_offset: &Value, callable: &Value, self_arg: &Value) -> R<()> {
            let _ = (code, instruction_offset);
            if slf.0.with(it, |s| s.builtins)? {
                if let Some((key, user)) = cfunction(it, callable, self_arg) {
                    enter_call(it, &slf.0, key, &user)?;
                }
            }
            Ok(())
        }

        fn _creturn_callback(slf: This<Py<Self>>, it: &mut Interp, code: &Value, instruction_offset: &Value, callable: &Value, self_arg: &Value) -> R<()> {
            let _ = (code, instruction_offset);
            if slf.0.with(it, |s| s.builtins)? {
                if let Some((key, _)) = cfunction(it, callable, self_arg) {
                    leave_call(it, &slf.0, key)?;
                }
            }
            Ok(())
        }

        /// Start collecting profiling information.
        ///
        ///   subcalls
        ///     If True, also records for each function
        ///     statistics separated according to its current caller.
        ///   builtins
        ///     If True, records the time spent in
        ///     built-in functions separately from their caller.
        fn enable(
            slf: This<Py<Self>>,
            it: &mut Interp,
            #[kw]
            #[default(true)]
            subcalls: bool,
            #[kw]
            #[default(true)]
            builtins: bool,
        ) -> R<()> {
            slf.0.with(it, |s| {
                s.subcalls = subcalls;
                s.builtins = builtins;
            })?;
            let sys = Value::Obj(it.import_module("sys")?);
            let mon = it.get_attr_str(&sys, "monitoring")?;
            it.call_method(&mon, "use_tool_id", vec![Value::Int(PROFILER_ID), Value::str("cProfile")])?;
            let mut all_events = 0;
            for (event, name) in CALLBACKS {
                let callback = it.get_attr_str(slf.0.value(), name)?;
                it.call_method(&mon, "register_callback", vec![Value::Int(PROFILER_ID), Value::Int(event as i64), callback])?;
                all_events |= event;
            }
            it.call_method(&mon, "set_events", vec![Value::Int(PROFILER_ID), Value::Int(all_events as i64)])?;
            slf.0.with(it, |s| s.enabled = true)
        }

        /// Stop collecting profiling information.
        fn disable(slf: This<Py<Self>>, it: &mut Interp) -> R<()> {
            if slf.0.with(it, |s| s.in_timer)? {
                return Err(it.new_exc_str("RuntimeError", "cannot disable profiler in external timer"));
            }
            if slf.0.with(it, |s| s.enabled)? {
                let sys = Value::Obj(it.import_module("sys")?);
                let mon = it.get_attr_str(&sys, "monitoring")?;
                for (event, _) in CALLBACKS {
                    it.call_method(&mon, "register_callback", vec![Value::Int(PROFILER_ID), Value::Int(event as i64), Value::None])?;
                }
                it.call_method(&mon, "set_events", vec![Value::Int(PROFILER_ID), Value::Int(0)])?;
                it.call_method(&mon, "free_tool_id", vec![Value::Int(PROFILER_ID)])?;
                slf.0.with(it, |s| s.enabled = false)?;
                while !slf.0.with(it, |s| s.stack.is_empty())? {
                    let now = clock(it, &slf.0)?;
                    slf.0.with(it, |s| s.flush_one(now))?;
                }
            }
            Ok(())
        }

        /// Clear all profiling information collected so far.
        fn clear(slf: This<Py<Self>>, it: &mut Interp) -> R<()> {
            if slf.0.with(it, |s| s.in_timer)? {
                return Err(it.new_exc_str("RuntimeError", "cannot clear profiler in external timer"));
            }
            slf.0.with(it, |s| {
                s.entries.clear();
                s.index.clear();
                s.stack.clear();
            })
        }

        /// list of profiler_entry objects.
        ///
        /// getstats() -> list of profiler_entry objects
        ///
        /// Return all information collected by the profiler.
        /// Each profiler_entry is a tuple-like object with the
        /// following attributes:
        ///
        ///     code          code object
        ///     callcount     how many times this was called
        ///     reccallcount  how many times called recursively
        ///     totaltime     total time in this entry
        ///     inlinetime    inline time in this entry (not in subcalls)
        ///     calls         details of the calls
        ///
        /// The calls attribute is either None or a list of
        /// profiler_subentry objects:
        ///
        ///     code          called code object
        ///     callcount     how many times this is called
        ///     reccallcount  how many times this is called recursively
        ///     totaltime     total time spent in this call
        ///     inlinetime    inline time (not in further subcalls)
        fn getstats(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            type Row = (Value, i64, i64, i64, i64, Vec<(usize, i64, i64, i64, i64)>);
            let (factor, rows): (f64, Vec<Row>) = slf.0.with(it, |s| {
                let rows = s
                    .entries
                    .iter()
                    .map(|e| {
                        let calls = e.calls.iter().map(|c| (c.callee, c.callcount, c.recursive, c.total, c.inline)).collect();
                        (e.user.clone(), e.callcount, e.recursive, e.total, e.inline, calls)
                    })
                    .collect();
                (s.factor(), rows)
            })?;
            let users: Vec<Value> = rows.iter().map(|r| r.0.clone()).collect();
            let (entry_ty, sub_ty) = stats_types(it);
            let mut out = Vec::with_capacity(rows.len());
            for (user, callcount, recursive, total, inline, calls) in rows {
                let subs = if calls.is_empty() {
                    Value::None
                } else {
                    let mut list = Vec::with_capacity(calls.len());
                    for (callee, n, rec, tt, it_time) in calls {
                        let code = describe(it, &users[callee])?;
                        let vals = vec![code, Value::Int(n), Value::Int(rec), Value::Float(tt as f64 * factor), Value::Float(it_time as f64 * factor)];
                        list.push(structseq_full(&sub_ty, vals));
                    }
                    Value::list(list)
                };
                let code = describe(it, &user)?;
                let vals = vec![code, Value::Int(callcount), Value::Int(recursive), Value::Float(total as f64 * factor), Value::Float(inline as f64 * factor), subs];
                out.push(structseq_full(&entry_ty, vals));
            }
            Ok(Value::list(out))
        }
    }
}

impl std::ops::Deref for _lsprof::Profiler {
    type Target = State;
    fn deref(&self) -> &State {
        &self.st
    }
}

impl std::ops::DerefMut for _lsprof::Profiler {
    fn deref_mut(&mut self) -> &mut State {
        &mut self.st
    }
}
