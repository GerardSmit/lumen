//! `_lsprof`: the profiler behind `cProfile`, a port of CPython's `Modules/_lsprof.c` on top of
//! `sys.setprofile` (the call, return and C-function events of the monitoring engine).

use super::sysextra::{frame_code, structseq_full, structseq_type};
use crate::bind::Py;
use crate::bytecode::Code;
use crate::object::*;
use crate::vm::Interp;
use std::collections::HashMap;
use std::rc::Rc;

/// The default timer's unit: it counts nanoseconds.
const DEFAULT_TIME_UNIT: f64 = 1e-9;
/// Seconds returned by an external timer without a `timeunit` are kept as fixed-point numbers.
const DOUBLE_TIMER_PRECISION: f64 = 4294967296.0;

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

fn code_key(code: &Rc<Code>) -> Key {
    (Rc::as_ptr(code) as usize, 0)
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

struct State {
    timer: Value,
    timeunit: f64,
    subcalls: bool,
    builtins: bool,
    enabled: bool,
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

    fn flush_unmatched(&mut self, now: i64) {
        while let Some(ctx) = self.stack.pop() {
            let idx = ctx.entry;
            self.stop(ctx, idx, now);
        }
    }

    fn factor(&self) -> f64 {
        if self.timer.is_none() {
            DEFAULT_TIME_UNIT
        } else if self.timeunit > 0.0 {
            self.timeunit
        } else {
            1.0 / DOUBLE_TIMER_PRECISION
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

    /// The current time in the profiler's units; a failing timer reads as 0.
    fn clock(it: &mut Interp, p: &Py<Profiler>) -> R<i64> {
        let (timer, unit) = p.with(it, |s| (s.timer.clone(), s.timeunit))?;
        if timer.is_none() {
            return Ok(it.platform.borrow().monotonic_ns() as i64);
        }
        let Ok(r) = it.call(&timer, Vec::new(), Vec::new()) else { return Ok(0) };
        Ok(match (&r, unit > 0.0) {
            (Value::Int(i), true) => *i,
            (Value::Bool(b), true) => *b as i64,
            (Value::Int(i), false) => (*i as f64 * DOUBLE_TIMER_PRECISION) as i64,
            (Value::Float(f), false) => (*f * DOUBLE_TIMER_PRECISION) as i64,
            _ => 0,
        })
    }

    fn profiler_callback(it: &mut Interp, args: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
        let [this, frame, what, arg] = args else { return Ok(Value::None) };
        let Some(p) = Py::<Profiler>::from_value(it, this) else { return Ok(Value::None) };
        let what = what.as_str().unwrap_or("").to_string();
        let (key, user) = match what.as_str() {
            "call" | "return" => {
                let Some(code) = frame_code(frame) else { return Ok(Value::None) };
                (code_key(&code), Value::Obj(Code::object(&code)))
            }
            "c_call" | "c_return" | "c_exception" => {
                if !p.with(it, |s| s.builtins)? {
                    return Ok(Value::None);
                }
                let Some(key) = function_key(arg) else { return Ok(Value::None) };
                (key, arg.clone())
            }
            _ => return Ok(Value::None),
        };
        let entering = matches!(what.as_str(), "call" | "c_call");
        if !entering && p.with(it, |s| s.stack.is_empty())? {
            return Ok(Value::None);
        }
        let now = clock(it, &p)?;
        p.with(it, |s| match what.as_str() {
            "call" | "c_call" => s.enter(key, &user, now),
            _ => s.leave(key, now),
        })?;
        Ok(Value::None)
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
            if timeunit < 0.0 {
                return Err(it.value_error("timeunit must not be negative"));
            }
            let timer = timer.cloned().unwrap_or(Value::None);
            let was_enabled = slf.0.with(it, |s| s.enabled)?;
            if was_enabled {
                return Err(it.new_exc_str("RuntimeError", "Cannot reconfigure the profiler: profiling is enabled"));
            }
            slf.0.with(it, |s| {
                s.timer = timer;
                s.timeunit = timeunit;
                s.subcalls = subcalls;
                s.builtins = builtins;
                s.entries.clear();
                s.index.clear();
                s.stack.clear();
            })
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
                s.enabled = true;
            })?;
            let native = it.new_native("profiler_callback", profiler_callback, false);
            let callback = Value::Obj(Object::new(Kind::Method(native, slf.0.value().clone())));
            it.set_profile_func(callback);
            Ok(())
        }

        /// Stop collecting profiling information.
        fn disable(slf: This<Py<Self>>, it: &mut Interp) -> R<()> {
            if slf.0.with(it, |s| s.enabled)? {
                it.set_profile_func(Value::None);
            }
            let now = clock(it, &slf.0)?;
            slf.0.with(it, |s| {
                s.flush_unmatched(now);
                s.enabled = false;
            })
        }

        /// Clear all profiling information collected so far.
        fn clear(&mut self) {
            self.entries.clear();
            self.index.clear();
            self.stack.clear();
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
            let entry_ty = structseq_type::<Stats>(it, "_lsprof", "profiler_entry", &["code", "callcount", "reccallcount", "totaltime", "inlinetime", "calls"], 6);
            let sub_ty = structseq_type::<SubStats>(it, "_lsprof", "profiler_subentry", &["code", "callcount", "reccallcount", "totaltime", "inlinetime"], 5);
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
