//! Event delivery for `sys.monitoring` (PEP 669) and, on top of the same events, `sys.settrace`
//! and `sys.setprofile`, as CPython 3.12 builds them: the legacy functions are the two reserved
//! tools (6 and 7) of the monitoring machinery.
//!
//! The VM checks one flag ([`Monitor::active`]) per instruction; everything else here runs only
//! while some tool listens. Events fire from the dispatch loop (`instrument`: line and
//! instruction events; jump arms: jump and branch), from `push_frame` (start, resume, throw), the
//! return and yield ops, the exception unwinder (raise, reraise, handled, unwind) and the call
//! ops (call, C return, C raise).

use crate::bytecode::Code;
use crate::object::*;
use crate::vm::*;
use std::collections::HashSet;
use std::rc::Rc;

/// The event bits of `sys.monitoring.events`.
pub mod ev {
    pub const PY_START: u32 = 1 << 0;
    pub const PY_RESUME: u32 = 1 << 1;
    pub const PY_RETURN: u32 = 1 << 2;
    pub const PY_YIELD: u32 = 1 << 3;
    pub const CALL: u32 = 1 << 4;
    pub const LINE: u32 = 1 << 5;
    pub const INSTRUCTION: u32 = 1 << 6;
    pub const JUMP: u32 = 1 << 7;
    pub const BRANCH: u32 = 1 << 8;
    pub const STOP_ITERATION: u32 = 1 << 9;
    pub const RAISE: u32 = 1 << 10;
    pub const EXCEPTION_HANDLED: u32 = 1 << 11;
    pub const PY_UNWIND: u32 = 1 << 12;
    pub const PY_THROW: u32 = 1 << 13;
    pub const RERAISE: u32 = 1 << 14;
    pub const C_RETURN: u32 = 1 << 15;
    pub const C_RAISE: u32 = 1 << 16;
}

/// The event names in bit order (`sys.monitoring.events` attributes).
pub const EVENT_NAMES: [&str; 17] = [
    "PY_START",
    "PY_RESUME",
    "PY_RETURN",
    "PY_YIELD",
    "CALL",
    "LINE",
    "INSTRUCTION",
    "JUMP",
    "BRANCH",
    "STOP_ITERATION",
    "RAISE",
    "EXCEPTION_HANDLED",
    "PY_UNWIND",
    "PY_THROW",
    "RERAISE",
    "C_RETURN",
    "C_RAISE",
];

pub const EVENT_COUNT: usize = EVENT_NAMES.len();
/// Events a code object can enable locally (`set_local_events`) and a callback can `DISABLE`.
pub const LOCAL_EVENTS: u32 = (1 << 10) - 1;
pub const C_RETURN_EVENTS: u32 = ev::C_RETURN | ev::C_RAISE;
pub const TOOLS: usize = 8;
/// The tool ids `sys.setprofile` and `sys.settrace` occupy.
pub const PROFILE_TOOL: usize = 6;
pub const TRACE_TOOL: usize = 7;
/// Tool ids available to `use_tool_id`.
pub const USER_TOOLS: usize = 6;

pub const ENTRY_START: u8 = 0;
pub const ENTRY_RESUME: u8 = 1;
pub const ENTRY_THROW: u8 = 2;

const TRACE_EVENTS: u32 =
    ev::PY_START | ev::PY_RESUME | ev::PY_RETURN | ev::PY_YIELD | ev::PY_UNWIND | ev::PY_THROW | ev::RAISE | ev::RERAISE | ev::LINE;
const PROFILE_EVENTS: u32 =
    ev::PY_START | ev::PY_RESUME | ev::PY_RETURN | ev::PY_YIELD | ev::PY_UNWIND | ev::PY_THROW | ev::CALL | ev::C_RETURN | ev::C_RAISE;

pub struct ToolState {
    /// The name given to `use_tool_id`; `None` while the tool is free.
    pub name: Option<Rc<str>>,
    /// The events enabled for every code object.
    pub events: u32,
    /// The callback registered for each event bit (`None` when unset).
    pub callbacks: [Value; EVENT_COUNT],
}

impl ToolState {
    fn new() -> ToolState {
        ToolState { name: None, events: 0, callbacks: std::array::from_fn(|_| Value::None) }
    }
}

/// The events enabled for one code object by each tool.
pub struct LocalEvents {
    pub code: Rc<Code>,
    pub events: [u32; TOOLS],
}

pub struct Monitor {
    /// Some tool listens to some event: the one flag the dispatch loop tests.
    pub active: bool,
    /// A callback is running; nothing fires meanwhile (CPython's `tstate->tracing`).
    pub in_callback: bool,
    /// The event whose callback is running (`0` outside callbacks).
    pub cur_event: u32,
    pub tools: [ToolState; TOOLS],
    pub locals: Vec<LocalEvents>,
    /// The union of every tool's global events.
    pub union: u32,
    /// `(tool, code, instruction offset, event)` whose callback returned `DISABLE`.
    pub disabled: HashSet<(u8, usize, u32, u32)>,
    /// Keeps the codes in `disabled` and `locals` alive so their addresses stay unique.
    pub pinned: Vec<Rc<Code>>,
    pub trace_func: Value,
    pub profile_func: Value,
    /// Some frame asked for opcode events (`f_trace_opcodes`).
    pub trace_opcodes: bool,
    /// `(DISABLE, MISSING)`, created on first use.
    pub sentinels: Option<(Value, Value)>,
}

impl Monitor {
    pub fn new() -> Monitor {
        Monitor {
            active: false,
            in_callback: false,
            cur_event: 0,
            tools: std::array::from_fn(|_| ToolState::new()),
            locals: Vec::new(),
            union: 0,
            disabled: HashSet::new(),
            pinned: Vec::new(),
            trace_func: Value::None,
            profile_func: Value::None,
            trace_opcodes: false,
            sentinels: None,
        }
    }

    /// The events `tool` has enabled for `code`, globally or locally.
    pub fn tool_events(&self, tool: usize, code: &Rc<Code>) -> u32 {
        let mut ev = self.tools[tool].events;
        if !self.locals.is_empty() {
            if let Some(l) = self.locals.iter().find(|l| Rc::ptr_eq(&l.code, code)) {
                ev |= l.events[tool];
            }
        }
        ev
    }

    pub fn local_slot(&mut self, code: &Rc<Code>) -> &mut LocalEvents {
        let at = match self.locals.iter().position(|l| Rc::ptr_eq(&l.code, code)) {
            Some(i) => i,
            None => {
                self.locals.push(LocalEvents { code: code.clone(), events: [0; TOOLS] });
                self.locals.len() - 1
            }
        };
        &mut self.locals[at]
    }

    pub fn pin(&mut self, code: &Rc<Code>) {
        if !self.pinned.iter().any(|c| Rc::ptr_eq(c, code)) {
            self.pinned.push(code.clone());
        }
    }

    /// Recomputes `union` and `active` after any tool's events changed.
    pub fn refresh(&mut self) {
        self.union = self.tools.iter().fold(0, |a, t| a | t.events);
        self.locals.retain(|l| l.events.iter().any(|&e| e != 0));
        self.active = self.union != 0 || !self.locals.is_empty();
    }
}

impl Default for Monitor {
    fn default() -> Monitor {
        Monitor::new()
    }
}

/// The event bits as the 3.12 `sys.monitoring` reports them: `CALL` implies its two ancillary
/// events.
pub fn expand_call(events: u32) -> u32 {
    if events & ev::CALL != 0 {
        events | C_RETURN_EVENTS
    } else {
        events & !C_RETURN_EVENTS
    }
}

impl Interp {
    /// `(DISABLE, MISSING)`, the two sentinel objects of `sys.monitoring`.
    pub fn monitoring_sentinels(&mut self) -> (Value, Value) {
        if let Some(s) = &self.mon.sentinels {
            return s.clone();
        }
        let object = self.types.object.clone();
        let disable = Value::Obj(Object::with_cls(object.clone(), Kind::Instance));
        let missing = Value::Obj(Object::with_cls(object, Kind::Instance));
        self.mon.sentinels = Some((disable.clone(), missing.clone()));
        (disable, missing)
    }

    /// Every event enabled for `code` by any tool.
    pub(crate) fn events_for(&self, code: &Rc<Code>) -> u32 {
        let mut events = self.mon.union;
        if !self.mon.locals.is_empty() {
            if let Some(l) = self.mon.locals.iter().find(|l| Rc::ptr_eq(&l.code, code)) {
                events |= l.events.iter().fold(0, |a, e| a | e);
            }
        }
        events
    }

    pub fn set_trace_func(&mut self, f: Value) {
        let on = !f.is_none();
        self.mon.trace_func = f;
        let mut events = if on { TRACE_EVENTS } else { 0 };
        if on && self.mon.trace_opcodes {
            events |= ev::INSTRUCTION;
        }
        self.mon.tools[TRACE_TOOL].events = events;
        self.mon.refresh();
    }

    pub fn set_profile_func(&mut self, f: Value) {
        let on = !f.is_none();
        self.mon.profile_func = f;
        self.mon.tools[PROFILE_TOOL].events = if on { PROFILE_EVENTS } else { 0 };
        self.mon.refresh();
    }

    /// A frame turned `f_trace_opcodes` on: opcode events join the trace function's events.
    pub(crate) fn want_opcode_events(&mut self) {
        self.mon.trace_opcodes = true;
        if !self.mon.trace_func.is_none() {
            self.mon.tools[TRACE_TOOL].events |= ev::INSTRUCTION;
            self.mon.refresh();
        }
    }

    // ---- delivery -------------------------------------------------------------------------

    /// Calls the callback of every tool that listens to `event` for `code`. `args` follow the
    /// code object in the callback's arguments.
    pub(crate) fn fire(&mut self, event: u32, code: &Rc<Code>, offset: usize, args: Vec<Value>) -> R<()> {
        if self.mon.in_callback {
            return Ok(());
        }
        for tool in 0..TOOLS {
            if self.mon.tool_events(tool, code) & event == 0 {
                continue;
            }
            let key = (tool as u8, Rc::as_ptr(code) as usize, offset as u32, event);
            if !self.mon.disabled.is_empty() && self.mon.disabled.contains(&key) {
                continue;
            }
            let outer = std::mem::replace(&mut self.mon.cur_event, event);
            self.mon.in_callback = true;
            let r = self.deliver(tool, event, code, &args);
            self.mon.in_callback = false;
            self.mon.cur_event = outer;
            if r? {
                self.mon.pin(code);
                self.mon.disabled.insert(key);
            }
        }
        Ok(())
    }

    /// Runs tool `tool`'s handler for `event`; `true` asks to disable the event at this location.
    fn deliver(&mut self, tool: usize, event: u32, code: &Rc<Code>, args: &[Value]) -> R<bool> {
        match tool {
            TRACE_TOOL => self.legacy_trace(event, args).map(|_| false),
            PROFILE_TOOL => self.legacy_profile(event, args).map(|_| false),
            _ => {
                let idx = event.trailing_zeros() as usize;
                let cb = self.mon.tools[tool].callbacks[idx].clone();
                if cb.is_none() {
                    return Ok(false);
                }
                let mut a = Vec::with_capacity(args.len() + 1);
                a.push(Value::Obj(Code::object(code)));
                a.extend(args.iter().cloned());
                let r = self.call(&cb, a, Vec::new())?;
                let disable = matches!(&self.mon.sentinels, Some((d, _)) if d.is(&r));
                if !disable {
                    return Ok(false);
                }
                if event & LOCAL_EVENTS != 0 {
                    return Ok(true);
                }
                self.mon.tools[tool].callbacks[idx] = Value::None;
                let msg = format!("Cannot disable {} events. Callback removed.", EVENT_NAMES[idx]);
                Err(self.value_error(&msg))
            }
        }
    }

    fn top_location(&self) -> (Rc<Code>, usize) {
        let f = self.frames.last().expect("event outside a frame");
        (f.code.clone(), f.pc)
    }

    // ---- the VM's event points ------------------------------------------------------------

    /// A frame was just pushed: its start, resume or throw event.
    pub(crate) fn frame_started(&mut self) -> R<()> {
        let (code, entry, pc) = {
            let f = self.frames.last().unwrap();
            (f.code.clone(), f.entry, f.pc)
        };
        let event = match entry {
            ENTRY_START => ev::PY_START,
            ENTRY_RESUME => ev::PY_RESUME,
            _ => ev::PY_THROW,
        };
        if self.events_for(&code) & event == 0 {
            return Ok(());
        }
        let offset = 2 * pc;
        self.fire(event, &code, offset, vec![Value::Int(offset as i64)])
    }

    /// Per instruction while a tool listens: line and instruction events. `true` when a callback
    /// moved the frame (`f_lineno` assignment) and the instruction must be fetched again.
    #[cold]
    #[inline(never)]
    pub(crate) fn instrument(&mut self) -> R<bool> {
        if self.mon.in_callback {
            return Ok(false);
        }
        let top = self.frames.len() - 1;
        let (code, pc) = {
            let f = &self.frames[top];
            (f.code.clone(), f.pc - 1)
        };
        let events = self.events_for(&code);
        if events & (ev::LINE | ev::INSTRUCTION) == 0 {
            return Ok(false);
        }
        if events & ev::LINE != 0 {
            let cur = code.line_at(pc);
            let candidate = code.info().line_starts.get(pc).copied().unwrap_or(false);
            let (prev, back) = {
                let f = &mut self.frames[top];
                let state = (f.prev_line, f.jump_back);
                f.prev_line = cur;
                f.jump_back = false;
                state
            };
            if lumen_common::lineno::line_event_due(candidate, prev, cur, back) {
                self.fire(ev::LINE, &code, 2 * pc, vec![Value::Int(cur as i64)])?;
                if self.frames[top].pc != pc + 1 {
                    return Ok(true);
                }
            }
        }
        if events & ev::INSTRUCTION != 0 {
            self.fire(ev::INSTRUCTION, &code, 2 * pc, vec![Value::Int(2 * pc as i64)])?;
            if self.frames[top].pc != pc + 1 {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// A jump from instruction `src` to `dst` happened (the frame's pc is already `dst`).
    pub(crate) fn jump_event(&mut self, event: u32, src: usize, dst: usize) -> R<()> {
        let top = self.frames.len() - 1;
        if dst <= src {
            self.frames[top].jump_back = true;
        }
        let code = self.frames[top].code.clone();
        if self.events_for(&code) & event == 0 {
            return Ok(());
        }
        self.fire(event, &code, 2 * src, vec![Value::Int(2 * src as i64), Value::Int(2 * dst as i64)])
    }

    /// A conditional jump at `src` either went to `target` (`taken`) or fell through.
    pub(crate) fn branch_event(&mut self, src: usize, target: usize, taken: bool) -> R<()> {
        let dst = if taken { target } else { src + 1 };
        self.jump_event(ev::BRANCH, src, dst)
    }

    /// The running frame returns or yields `value`.
    pub(crate) fn return_event(&mut self, event: u32, value: &Value) -> R<()> {
        let (code, pc) = self.top_location();
        if self.events_for(&code) & event == 0 {
            return Ok(());
        }
        let offset = 2 * pc.saturating_sub(1);
        self.fire(event, &code, offset, vec![Value::Int(offset as i64), value.clone()])
    }

    /// An exception event in the running frame. A failing callback's exception is returned so
    /// the unwinder can replace the one being raised.
    pub(crate) fn exception_event(&mut self, event: u32, exc: &Obj) -> Option<Obj> {
        let (code, pc) = {
            let f = self.frames.last()?;
            (f.code.clone(), f.pc)
        };
        if self.events_for(&code) & event == 0 {
            return None;
        }
        let at = if event == ev::EXCEPTION_HANDLED { pc } else { pc.saturating_sub(1) };
        let offset = 2 * at;
        self.fire(event, &code, offset, vec![Value::Int(offset as i64), Value::Obj(exc.clone())]).err()
    }

    /// `call_inline` while a tool listens: the call event, then the C return or C raise event of
    /// a callable that is not a Python function.
    pub(crate) fn call_inline_monitored(&mut self, f: &Value, args: Vec<Value>, kw: Vec<(Obj, Value)>) -> R<bool> {
        let (code, pc) = self.top_location();
        let offset = 2 * pc.saturating_sub(1);
        let (_, missing) = self.monitoring_sentinels();
        let (callable, arg0) = match f {
            Value::Obj(o) => match &o.kind {
                Kind::Method(func, this) => (func.clone(), this.clone()),
                _ => (f.clone(), args.first().cloned().unwrap_or(missing)),
            },
            _ => (f.clone(), args.first().cloned().unwrap_or(missing)),
        };
        let wants = self.events_for(&code);
        if wants & ev::CALL != 0 {
            self.fire(ev::CALL, &code, offset, vec![Value::Int(offset as i64), callable.clone(), arg0.clone()])?;
        }
        let is_python = match f {
            Value::Obj(o) => match &o.kind {
                Kind::Function(_) => true,
                Kind::Method(Value::Obj(func), _) => matches!(func.kind, Kind::Function(_)),
                _ => false,
            },
            _ => false,
        };
        if is_python {
            return self.call_inline_plain(f, args, kw);
        }
        match self.call(f, args, kw) {
            Ok(r) => {
                if wants & ev::C_RETURN != 0 {
                    self.fire(ev::C_RETURN, &code, offset, vec![Value::Int(offset as i64), callable, arg0])?;
                }
                self.frames.last_mut().unwrap().stack.push(r);
                Ok(false)
            }
            Err(e) => {
                if wants & ev::C_RAISE != 0 {
                    if let Err(e2) = self.fire(ev::C_RAISE, &code, offset, vec![Value::Int(offset as i64), callable, arg0]) {
                        return Err(e2);
                    }
                }
                Err(e)
            }
        }
    }

    // ---- sys.settrace -----------------------------------------------------------------------

    fn top_frame_value(&mut self) -> Value {
        let top = self.frames.len() - 1;
        self.frame_object(top)
    }

    fn legacy_trace(&mut self, event: u32, args: &[Value]) -> R<()> {
        match event {
            ev::PY_START | ev::PY_RESUME | ev::PY_THROW => self.trace_call("call", Value::None),
            ev::PY_RETURN | ev::PY_YIELD => self.trace_call("return", args.get(1).cloned().unwrap_or(Value::None)),
            ev::PY_UNWIND => self.trace_call("return", Value::None),
            ev::RAISE | ev::RERAISE => {
                let Some(Value::Obj(e)) = args.get(1) else { return Ok(()) };
                let ty = Value::Obj(self.type_of_obj(e));
                let tb = match &e.kind {
                    Kind::Exception(d) => self.make_tb(&d.borrow().tb),
                    _ => Value::None,
                };
                self.trace_call("exception", Value::tuple(vec![ty, Value::Obj(e.clone()), tb]))
            }
            ev::LINE => {
                if self.frames.last().is_some_and(|f| f.trace_lines) {
                    self.trace_call("line", Value::None)?;
                }
                Ok(())
            }
            ev::INSTRUCTION => {
                if self.frames.last().is_some_and(|f| f.trace_opcodes) {
                    self.trace_call("opcode", Value::None)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// CPython's `trace_trampoline`: the global trace function takes `call` events, the frame's
    /// `f_trace` all others; a result other than `None` becomes the frame's `f_trace`, an error
    /// turns tracing off.
    fn trace_call(&mut self, what: &str, arg: Value) -> R<()> {
        let top = self.frames.len() - 1;
        let callback = if what == "call" { self.mon.trace_func.clone() } else { self.frames[top].trace.clone().unwrap_or(Value::None) };
        if callback.is_none() {
            return Ok(());
        }
        let frame = self.top_frame_value();
        match self.call(&callback, vec![frame, Value::str(what), arg], Vec::new()) {
            Err(e) => {
                self.set_trace_func(Value::None);
                if let Some(f) = self.frames.get_mut(top) {
                    f.trace = None;
                }
                Err(e)
            }
            Ok(v) => {
                if !v.is_none() {
                    if let Some(f) = self.frames.get_mut(top) {
                        f.trace = Some(v);
                    }
                }
                Ok(())
            }
        }
    }

    // ---- sys.setprofile ---------------------------------------------------------------------

    /// The `arg` of a profile call event for `callable`, when it is a built-in function.
    fn profile_c_function(callable: &Value) -> bool {
        match callable {
            Value::Obj(o) => match &o.kind {
                Kind::Native(_) => true,
                Kind::Method(Value::Obj(func), _) => matches!(func.kind, Kind::Native(_)),
                _ => false,
            },
            _ => false,
        }
    }

    fn legacy_profile(&mut self, event: u32, args: &[Value]) -> R<()> {
        match event {
            ev::PY_START | ev::PY_RESUME | ev::PY_THROW => self.profile_call("call", Value::None),
            ev::PY_RETURN | ev::PY_YIELD => self.profile_call("return", args.get(1).cloned().unwrap_or(Value::None)),
            ev::PY_UNWIND => self.profile_call("return", Value::None),
            ev::CALL | ev::C_RETURN | ev::C_RAISE => {
                let callable = args.get(1).cloned().unwrap_or(Value::None);
                if !Self::profile_c_function(&callable) {
                    return Ok(());
                }
                let what = match event {
                    ev::CALL => "c_call",
                    ev::C_RETURN => "c_return",
                    _ => "c_exception",
                };
                self.profile_call(what, callable)
            }
            _ => Ok(()),
        }
    }

    fn profile_call(&mut self, what: &str, arg: Value) -> R<()> {
        let callback = self.mon.profile_func.clone();
        if callback.is_none() {
            return Ok(());
        }
        let frame = self.top_frame_value();
        match self.call(&callback, vec![frame, Value::str(what), arg], Vec::new()) {
            Ok(_) => Ok(()),
            Err(e) => {
                self.set_profile_func(Value::None);
                Err(e)
            }
        }
    }

    /// `sys.call_tracing(func, args)`: calls `func(*args)` with tracing enabled.
    pub fn call_tracing(&mut self, func: &Value, args: Vec<Value>) -> R<Value> {
        let saved = std::mem::replace(&mut self.mon.in_callback, false);
        let r = self.call(func, args, Vec::new());
        self.mon.in_callback = saved;
        r
    }
}
