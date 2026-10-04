//! The bytecode VM: interpreter state, frames and the dispatch loop.

use crate::ast::{BinOp, CmpOp};
use crate::bytecode::*;
use crate::dict::PyDict;
use crate::object::*;
use crate::platform::{Platform, PlatformRef, StdPlatform};
use lumen_common::limits::{HeapBudget, InterruptHandle};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

pub struct Block {
    pub handler: u32,
    pub depth: u32,
}

pub struct Frame {
    pub code: Rc<Code>,
    pub pc: usize,
    pub stack: Vec<Value>,
    pub locals: Vec<Option<Value>>,
    pub cells: Vec<Obj>,
    pub globals: Obj,
    pub names: Option<Obj>,
    pub blocks: Vec<Block>,
    pub func: Option<Obj>,
}

macro_rules! types {
    ($($f:ident : $name:expr, $layout:expr;)*) => {
        pub struct Types { $(pub $f: Obj,)* }
        impl Types {
            pub fn create() -> Types {
                Types { $($f: new_type_raw($name, $layout),)* }
            }
        }
    };
}

pub fn new_type_raw(name: &str, layout: Layout) -> Obj {
    Rc::new(Object {
        cls: None,
        id: std::cell::Cell::new(0),
        dict: RefCell::new(Some(Object::new(Kind::Dict(RefCell::new(PyDict::new()))))),
        kind: Kind::Type(TypeData {
            name: RefCell::new(name.into()),
            qualname: RefCell::new(None),
            bases: RefCell::new(Vec::new()),
            mro: RefCell::new(Vec::new()),
            layout: std::cell::Cell::new(layout),
            flags: std::cell::Cell::new(TF_IMMUTABLE),
            hooks: std::cell::Cell::new((u64::MAX, 0)),
            slots: RefCell::new(None),
        }),
    })
}

types! {
    object: "object", Layout::Object;
    type_: "type", Layout::Type;
    int: "int", Layout::Int;
    bool_: "bool", Layout::Int;
    float: "float", Layout::Float;
    complex: "complex", Layout::Complex;
    str_: "str", Layout::Str;
    list: "list", Layout::List;
    tuple: "tuple", Layout::Tuple;
    dict: "dict", Layout::Dict;
    set: "set", Layout::Set;
    frozenset: "frozenset", Layout::FrozenSet;
    bytes: "bytes", Layout::Bytes;
    bytearray: "bytearray", Layout::ByteArray;
    none_type: "NoneType", Layout::Other;
    notimpl_type: "NotImplementedType", Layout::Other;
    ellipsis_type: "ellipsis", Layout::Other;
    function: "function", Layout::Function;
    method: "method", Layout::Other;
    builtin_function: "builtin_function_or_method", Layout::Other;
    module: "module", Layout::Module;
    cell: "cell", Layout::Other;
    code: "code", Layout::Other;
    generator: "generator", Layout::Other;
    coroutine: "coroutine", Layout::Other;
    async_generator: "async_generator", Layout::Other;
    asend: "async_generator_asend", Layout::Other;
    traceback: "traceback", Layout::Other;
    slice: "slice", Layout::Other;
    range: "range", Layout::Other;
    property: "property", Layout::Other;
    staticmethod: "staticmethod", Layout::Other;
    classmethod: "classmethod", Layout::Other;
    super_: "super", Layout::Other;
    dict_keys: "dict_keys", Layout::Other;
    dict_values: "dict_values", Layout::Other;
    dict_items: "dict_items", Layout::Other;
    list_iterator: "list_iterator", Layout::Other;
    list_reverseiterator: "list_reverseiterator", Layout::Other;
    tuple_iterator: "tuple_iterator", Layout::Other;
    str_iterator: "str_iterator", Layout::Other;
    bytes_iterator: "bytes_iterator", Layout::Other;
    range_iterator: "range_iterator", Layout::Other;
    dict_keyiterator: "dict_keyiterator", Layout::Other;
    dict_valueiterator: "dict_valueiterator", Layout::Other;
    dict_itemiterator: "dict_itemiterator", Layout::Other;
    set_iterator: "set_iterator", Layout::Other;
    iterator: "iterator", Layout::Other;
    callable_iterator: "callable_iterator", Layout::Other;
    reversed: "reversed", Layout::Other;
    enumerate: "enumerate", Layout::Other;
    zip: "zip", Layout::Other;
    map: "map", Layout::Other;
    filter: "filter", Layout::Other;
    frame: "frame", Layout::Other;
}

/// Destination for the interpreter's stdout and stderr; embedders replace the default
/// (the process's own streams) to capture or redirect output.
pub trait Output {
    fn write_stdout(&mut self, bytes: &[u8]);
    fn write_stderr(&mut self, bytes: &[u8]);
    fn flush(&mut self) {}
}

pub struct ProcessOutput;

impl Output for ProcessOutput {
    fn write_stdout(&mut self, bytes: &[u8]) {
        use std::io::Write;
        let _ = std::io::stdout().lock().write_all(bytes);
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        use std::io::Write;
        let _ = std::io::stderr().write_all(bytes);
    }

    fn flush(&mut self) {
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
}

pub struct Interp {
    pub frames: Vec<Frame>,
    pub types: Types,
    pub exc_types: BTreeMap<&'static str, Obj>,
    pub builtins: Obj,
    pub modules: Obj,
    pub sys_module: Option<Obj>,
    pub handled: Option<Obj>,
    pub recursion_limit: usize,
    pub yielded: Option<Frame>,
    pub no_tb: bool,
    pub repr_stack: Vec<usize>,
    pub out: Vec<u8>,
    pub sink: Option<Box<dyn Output>>,
    pub platform: PlatformRef,
    pub path_set: bool,
    pub script_dir: String,
    pub argv: Vec<String>,
    pub sources: BTreeMap<String, Vec<String>>,
    pub type_epoch: u64,
    pub main_globals: Option<Obj>,
    pub exit_code: Option<i32>,
    pub start_ns: u64,
    pub next_id: usize,
    pub obj_new: Option<Obj>,
    pub obj_init: Option<Obj>,
    pub subclass_registry: Vec<std::rc::Weak<Object>>,
    pub ret_val: Value,
    pub atexit: Vec<(Value, Vec<Value>, Vec<(Obj, Value)>)>,
    pub gc_enabled: bool,
    pub simple_namespace: Option<Obj>,
    pub alias_types: Option<Rc<crate::builtins::alias::AliasTypes>>,
    pub interrupt: InterruptHandle,
    pub interrupted: bool,
    /// Runs Python signal handlers at `poll` (the interpreter of the thread that owns signals).
    pub handles_signals: bool,
    pub heap: HeapBudget,
    pub int_max_str_digits: usize,
    pub codecs: crate::codecs::CodecState,
    /// Type objects of the native classes bound with `#[class]` (see `bind::type_object`).
    pub native_types: std::collections::HashMap<std::any::TypeId, Obj>,
    /// Per-interpreter state of native modules (CPython's module state), keyed by its type.
    pub native_state: std::collections::HashMap<std::any::TypeId, Box<dyn std::any::Any>>,
    /// The current `contextvars.Context`, created on first use.
    pub context: Option<Value>,
}

pub enum GenResult {
    Yield(Value),
    Return(Value),
}

impl Interp {
    /// The native-module state `T`, created on first use.
    pub fn native_state<T: Default + 'static>(&mut self) -> &mut T {
        let slot = self
            .native_state
            .entry(std::any::TypeId::of::<T>())
            .or_insert_with(|| Box::new(T::default()));
        slot.downcast_mut::<T>()
            .expect("native state keyed by its own type")
    }

    #[allow(clippy::new_without_default)]
    pub fn new() -> Interp {
        Interp::with_platform(Box::new(StdPlatform::new()))
    }

    pub fn with_platform(platform: Box<dyn Platform>) -> Interp {
        let platform: Box<dyn Platform> = Box::new(crate::platform::MemPlatform::new(
            platform,
            crate::frozen::fs(),
        ));
        let start_ns = platform.monotonic_ns();
        let types = Types::create();
        let builtins = Object::new(Kind::Dict(RefCell::new(PyDict::new())));
        let modules = Object::new(Kind::Dict(RefCell::new(PyDict::new())));
        let mut it = Interp {
            frames: Vec::new(),
            types,
            exc_types: BTreeMap::new(),
            builtins,
            modules,
            sys_module: None,
            handled: None,
            recursion_limit: 1000,
            yielded: None,
            no_tb: false,
            repr_stack: Vec::new(),
            out: Vec::new(),
            sink: None,
            platform: Rc::new(RefCell::new(platform)),
            path_set: false,
            script_dir: ".".into(),
            argv: Vec::new(),
            sources: BTreeMap::new(),
            type_epoch: 0,
            main_globals: None,
            exit_code: None,
            start_ns,
            next_id: 1,
            obj_new: None,
            obj_init: None,
            subclass_registry: Vec::new(),
            ret_val: Value::None,
            atexit: Vec::new(),
            gc_enabled: true,
            simple_namespace: None,
            alias_types: None,
            interrupt: InterruptHandle::new(),
            interrupted: false,
            handles_signals: false,
            heap: HeapBudget::NONE,
            int_max_str_digits: crate::limits::DEFAULT_INT_MAX_STR_DIGITS,
            codecs: Default::default(),
            native_types: std::collections::HashMap::new(),
            native_state: std::collections::HashMap::new(),
            context: None,
        };
        it.bootstrap_types();
        crate::builtins::init(&mut it);
        it
    }

    pub fn set_output(&mut self, sink: Box<dyn Output>) {
        self.flush_out();
        self.sink = Some(sink);
    }

    pub fn flush_out(&mut self) {
        if !self.out.is_empty() {
            match &mut self.sink {
                Some(s) => {
                    s.write_stdout(&self.out);
                    s.flush();
                }
                None => {
                    let mut p = self.platform.borrow_mut();
                    p.write_stdout(&self.out);
                    p.flush_stdout();
                }
            }
            self.out.clear();
        }
    }

    pub fn write_stdout(&mut self, s: &str) {
        self.out
            .extend_from_slice(lumen_common::smuggle::unescape_text(s).as_bytes());
        if self.out.len() > 1 << 16 {
            self.flush_out();
        }
    }

    /// `write(2)` through the platform; descriptors 1 and 2 go to the output sink when one is
    /// set, after the buffered print output.
    pub fn fd_write(&mut self, fd: i32, data: &[u8]) -> crate::platform::PResult<usize> {
        if fd == 1 || fd == 2 {
            self.flush_out();
            if let Some(s) = &mut self.sink {
                if fd == 1 {
                    s.write_stdout(data);
                    s.flush();
                } else {
                    s.write_stderr(data);
                }
                return Ok(data.len());
            }
        }
        self.platform.borrow_mut().fd_write(fd, data, None)
    }

    pub fn write_stderr(&mut self, s: &str) {
        self.flush_out();
        let s = lumen_common::smuggle::unescape_text(s);
        match &mut self.sink {
            Some(k) => k.write_stderr(s.as_bytes()),
            None => self.platform.borrow_mut().write_stderr(s.as_bytes()),
        }
    }

    // ---- frames ----------------------------------------------------------------------------

    pub fn new_frame(
        &self,
        code: Rc<Code>,
        globals: Obj,
        names: Option<Obj>,
        func: Option<Obj>,
        closure: &[Obj],
    ) -> Frame {
        let mut cells: Vec<Obj> = Vec::with_capacity(code.cellvars.len() + closure.len());
        for _ in 0..code.cellvars.len() {
            cells.push(Object::new(Kind::Cell(RefCell::new(None))));
        }
        cells.extend(closure.iter().cloned());
        Frame {
            locals: vec![None; code.varnames.len()],
            stack: Vec::with_capacity(8),
            pc: 0,
            blocks: Vec::new(),
            code,
            cells,
            globals,
            names,
            func,
        }
    }

    pub fn push_frame(&mut self, frame: Frame) -> R<()> {
        if self.frames.len() >= self.recursion_limit || lumen_common::stack::exhausted() {
            return Err(self.new_exc_str("RecursionError", "maximum recursion depth exceeded"));
        }
        self.poll()?;
        self.frames.push(frame);
        Ok(())
    }

    /// Runs `code` as a module-style frame: names resolve through `locals` then `globals`.
    pub fn run_code(&mut self, code: Rc<Code>, globals: Obj, locals: Obj) -> R<Value> {
        let frame = self.new_frame(code, globals, Some(locals), None, &[]);
        self.push_frame(frame)?;
        let entry = self.frames.len() - 1;
        self.run(entry, None)
    }

    // ---- main loop -------------------------------------------------------------------------

    pub fn run(&mut self, entry: usize, mut pending: Option<Obj>) -> R<Value> {
        loop {
            if let Some(exc) = pending.take() {
                let add_tb = !std::mem::replace(&mut self.no_tb, false);
                self.unwind(exc, entry, add_tb)?;
            }
            match self.exec(entry) {
                Ok(v) => return Ok(v),
                Err(e) => pending = Some(self.supersede_by_interrupt(e)),
            }
        }
    }

    /// While an interrupt is pending, whatever a script was in the middle of (possibly a
    /// consequence of an aborted big-integer operation) is replaced by `KeyboardInterrupt`, so
    /// handlers for ordinary exceptions never see it.
    pub(crate) fn supersede_by_interrupt(&mut self, e: Obj) -> Obj {
        if self.interrupt.is_interrupted() && !self.exc_is(&e, "KeyboardInterrupt") {
            return self.interrupt_exc();
        }
        e
    }

    fn unwind(&mut self, exc: Obj, entry: usize, mut add_tb: bool) -> R<()> {
        self.link_context(&exc);
        loop {
            let fr = self.frames.last_mut().unwrap();
            if add_tb {
                let lasti = fr.pc.saturating_sub(1);
                let line = fr.code.line_at(lasti);
                let entry_tb = TbEntry {
                    file: fr.code.filename.clone(),
                    line,
                    lasti: lasti as u32,
                    name: fr.code.name.clone(),
                    code: fr.code.clone(),
                    globals: fr.globals.clone(),
                };
                if let Kind::Exception(d) = &exc.kind {
                    d.borrow_mut().tb.push(entry_tb);
                }
            }
            if let Some(block) = fr.blocks.pop() {
                fr.stack.truncate(block.depth as usize);
                fr.stack.push(Value::Obj(exc));
                fr.pc = block.handler as usize;
                return Ok(());
            }
            let idx = self.frames.len() - 1;
            self.frames.pop();
            if idx == entry {
                return Err(exc);
            }
            add_tb = true;
        }
    }

    fn link_context(&mut self, exc: &Obj) {
        if let Kind::Exception(d) = &exc.kind {
            let mut d = d.borrow_mut();
            if d.ctx_set {
                return;
            }
            d.ctx_set = true;
            if let Some(h) = &self.handled {
                if Rc::ptr_eq(h, exc) {
                    return;
                }
                let mut cur = Some(h.clone());
                while let Some(c) = cur {
                    if Rc::ptr_eq(&c, exc) {
                        return;
                    }
                    cur = match &c.kind {
                        Kind::Exception(cd) => cd.borrow().context.clone(),
                        _ => None,
                    };
                }
                d.context = Some(h.clone());
            }
        }
    }

    fn name_err(&mut self, name: &Obj) -> Obj {
        let n = name.as_str_kind().unwrap_or("?").to_string();
        let e = self.new_exc_str("NameError", &format!("name '{}' is not defined", n));
        self.set_exc_attr(&e, "name", Value::str(&n));
        e
    }

    #[inline]
    fn exec(&mut self, entry: usize) -> R<Value> {
        macro_rules! fr {
            () => {
                self.frames.last_mut().unwrap()
            };
        }
        macro_rules! push {
            ($v:expr) => {{
                let v = $v;
                fr!().stack.push(v);
            }};
        }
        macro_rules! pop {
            () => {
                fr!().stack.pop().unwrap()
            };
        }
        loop {
            let op = {
                let fr = fr!();
                let op = fr.code.ops[fr.pc];
                fr.pc += 1;
                op
            };
            match op {
                Op::Nop => {}
                Op::Pop => {
                    pop!();
                }
                Op::Dup => {
                    let fr = fr!();
                    let v = fr.stack.last().unwrap().clone();
                    fr.stack.push(v);
                }
                Op::DupTwo => {
                    let fr = fr!();
                    let n = fr.stack.len();
                    let a = fr.stack[n - 2].clone();
                    let b = fr.stack[n - 1].clone();
                    fr.stack.push(a);
                    fr.stack.push(b);
                }
                Op::Swap(n) => {
                    let fr = fr!();
                    let l = fr.stack.len();
                    fr.stack.swap(l - 1, l - n as usize);
                }
                Op::Rot3 => {
                    let fr = fr!();
                    let c = fr.stack.pop().unwrap();
                    let l = fr.stack.len();
                    fr.stack.insert(l - 2, c);
                }
                Op::Rot4 => {
                    let fr = fr!();
                    let d = fr.stack.pop().unwrap();
                    let l = fr.stack.len();
                    fr.stack.insert(l - 3, d);
                }
                Op::LoadConst(i) => {
                    let fr = fr!();
                    let v = fr.code.consts[i as usize].clone();
                    fr.stack.push(v);
                }
                Op::LoadFast(i) => {
                    let fr = fr!();
                    match &fr.locals[i as usize] {
                        Some(v) => {
                            let v = v.clone();
                            fr.stack.push(v);
                        }
                        None => {
                            let n = fr.code.varnames[i as usize].to_string();
                            return Err(self.new_exc_str(
                                "UnboundLocalError",
                                &format!("cannot access local variable '{}' where it is not associated with a value", n),
                            ));
                        }
                    }
                }
                Op::StoreFast(i) => {
                    let fr = fr!();
                    let v = fr.stack.pop().unwrap();
                    fr.locals[i as usize] = Some(v);
                }
                Op::DelFast(i) => {
                    let fr = fr!();
                    if fr.locals[i as usize].take().is_none() {
                        let n = fr.code.varnames[i as usize].to_string();
                        return Err(self.new_exc_str(
                            "UnboundLocalError",
                            &format!("cannot access local variable '{}' where it is not associated with a value", n),
                        ));
                    }
                    if crate::weak::has_pending() {
                        self.run_weak_callbacks();
                    }
                }
                Op::LoadName(i) => {
                    let fr = fr!();
                    let name = fr.code.names[i as usize].clone();
                    let mut found = None;
                    if let Some(ns) = &fr.names {
                        found = dict_get_name(ns, &name);
                    }
                    if found.is_none() {
                        found = dict_get_name(&fr.globals, &name);
                    }
                    if found.is_none() {
                        found = dict_get_name(&self.builtins, &name);
                    }
                    match found {
                        Some(v) => push!(v),
                        None => return Err(self.name_err(&name)),
                    }
                }
                Op::StoreName(i) => {
                    let fr = fr!();
                    let name = fr.code.names[i as usize].clone();
                    let v = fr.stack.pop().unwrap();
                    let ns = fr.names.clone().unwrap_or_else(|| fr.globals.clone());
                    if ns.cls.is_some() {
                        self.setitem(&Value::Obj(ns), Value::Obj(name), v)?;
                    } else {
                        dict_set_name(&ns, &name, v);
                    }
                }
                Op::DelName(i) => {
                    let fr = fr!();
                    let name = fr.code.names[i as usize].clone();
                    let ns = fr.names.clone().unwrap_or_else(|| fr.globals.clone());
                    if dict_del_name(&ns, &name).is_none() {
                        return Err(self.name_err(&name));
                    }
                    if crate::weak::has_pending() {
                        self.run_weak_callbacks();
                    }
                }
                Op::LoadGlobal(i) => {
                    let fr = fr!();
                    let name = fr.code.names[i as usize].clone();
                    let mut found = dict_get_name(&fr.globals, &name);
                    if found.is_none() {
                        found = dict_get_name(&self.builtins, &name);
                    }
                    match found {
                        Some(v) => push!(v),
                        None => return Err(self.name_err(&name)),
                    }
                }
                Op::StoreGlobal(i) => {
                    let fr = fr!();
                    let name = fr.code.names[i as usize].clone();
                    let v = fr.stack.pop().unwrap();
                    let g = fr.globals.clone();
                    dict_set_name(&g, &name, v);
                }
                Op::DelGlobal(i) => {
                    let fr = fr!();
                    let name = fr.code.names[i as usize].clone();
                    let g = fr.globals.clone();
                    if dict_del_name(&g, &name).is_none() {
                        return Err(self.name_err(&name));
                    }
                    if crate::weak::has_pending() {
                        self.run_weak_callbacks();
                    }
                }
                Op::LoadDeref(i) => {
                    let fr = fr!();
                    let v = match &fr.cells[i as usize].kind {
                        Kind::Cell(c) => c.borrow().clone(),
                        _ => None,
                    };
                    match v {
                        Some(v) => fr.stack.push(v),
                        None => {
                            let n = cell_name(&fr.code, i as usize);
                            let free = i as usize >= fr.code.cellvars.len();
                            let msg = if free {
                                format!("cannot access free variable '{}' where it is not associated with a value in enclosing scope", n)
                            } else {
                                format!("cannot access local variable '{}' where it is not associated with a value", n)
                            };
                            return Err(self.new_exc_str(
                                if free {
                                    "NameError"
                                } else {
                                    "UnboundLocalError"
                                },
                                &msg,
                            ));
                        }
                    }
                }
                Op::StoreDeref(i) => {
                    let fr = fr!();
                    let v = fr.stack.pop().unwrap();
                    if let Kind::Cell(c) = &fr.cells[i as usize].kind {
                        *c.borrow_mut() = Some(v);
                    }
                }
                Op::DelDeref(i) => {
                    let fr = fr!();
                    if let Kind::Cell(c) = &fr.cells[i as usize].kind {
                        *c.borrow_mut() = None;
                    }
                }
                Op::LoadClosure(i) => {
                    let fr = fr!();
                    let c = fr.cells[i as usize].clone();
                    fr.stack.push(Value::Obj(c));
                }
                Op::LoadClassDeref(i) => {
                    let fr = fr!();
                    let n = cell_name(&fr.code, i as usize);
                    let mut found = None;
                    if let Some(ns) = &fr.names {
                        found = dict_get_str(ns, &n);
                    }
                    if found.is_none() {
                        if let Kind::Cell(c) = &fr.cells[i as usize].kind {
                            found = c.borrow().clone();
                        }
                    }
                    match found {
                        Some(v) => fr.stack.push(v),
                        None => {
                            return Err(self.new_exc_str(
                                "NameError",
                                &format!("cannot access free variable '{}' where it is not associated with a value in enclosing scope", n),
                            ))
                        }
                    }
                }
                Op::LoadAttr(i) => {
                    let name = fr!().code.names[i as usize].clone();
                    let obj = pop!();
                    let v = self.get_attr(&obj, &name)?;
                    push!(v);
                }
                Op::StoreAttr(i) => {
                    let name = fr!().code.names[i as usize].clone();
                    let obj = pop!();
                    let v = pop!();
                    self.set_attr(&obj, &name, v)?;
                }
                Op::DelAttr(i) => {
                    let name = fr!().code.names[i as usize].clone();
                    let obj = pop!();
                    self.del_attr(&obj, &name)?;
                }
                Op::LoadMethod(_) | Op::CallMethod(_) => {}
                Op::Subscr => {
                    let key = pop!();
                    let obj = pop!();
                    let v = self.getitem(&obj, &key)?;
                    push!(v);
                }
                Op::StoreSubscr => {
                    let key = pop!();
                    let obj = pop!();
                    let v = pop!();
                    self.setitem(&obj, key, v)?;
                }
                Op::DelSubscr => {
                    let key = pop!();
                    let obj = pop!();
                    self.delitem(&obj, &key)?;
                }
                Op::Binary(op) => {
                    let b = pop!();
                    let a = pop!();
                    let v = self.binary_op(op, &a, &b)?;
                    push!(v);
                }
                Op::Inplace(op) => {
                    let b = pop!();
                    let a = pop!();
                    let v = self.inplace_op(op, a, &b)?;
                    push!(v);
                }
                Op::Unary(op) => {
                    let a = pop!();
                    let v = self.unary_op(op, &a)?;
                    push!(v);
                }
                Op::Compare(op) => {
                    let b = pop!();
                    let a = pop!();
                    let v = self.compare_op(op, &a, &b)?;
                    push!(v);
                }
                Op::Jump(t) => {
                    let fr = fr!();
                    let back = (t as usize) < fr.pc;
                    fr.pc = t as usize;
                    if back {
                        self.poll()?;
                    }
                }
                Op::JumpIfFalse(t) => {
                    let v = pop!();
                    let b = match v {
                        Value::Bool(b) => b,
                        v => self.truthy(&v)?,
                    };
                    if !b {
                        fr!().pc = t as usize;
                    }
                }
                Op::JumpIfTrue(t) => {
                    let v = pop!();
                    let b = match v {
                        Value::Bool(b) => b,
                        v => self.truthy(&v)?,
                    };
                    if b {
                        fr!().pc = t as usize;
                    }
                }
                Op::JumpIfFalseKeep(t) => {
                    let v = fr!().stack.last().unwrap().clone();
                    let b = match v {
                        Value::Bool(b) => b,
                        v => self.truthy(&v)?,
                    };
                    if !b {
                        fr!().pc = t as usize;
                    } else {
                        pop!();
                    }
                }
                Op::JumpIfTrueKeep(t) => {
                    let v = fr!().stack.last().unwrap().clone();
                    let b = match v {
                        Value::Bool(b) => b,
                        v => self.truthy(&v)?,
                    };
                    if b {
                        fr!().pc = t as usize;
                    } else {
                        pop!();
                    }
                }
                Op::GetIter => {
                    let v = pop!();
                    let it = self.get_iter(&v)?;
                    push!(it);
                }
                Op::ForIter(t) => {
                    let it = fr!().stack.last().unwrap().clone();
                    match self.iter_step(&it)? {
                        Some(v) => push!(v),
                        None => {
                            let fr = fr!();
                            fr.stack.pop();
                            fr.pc = t as usize;
                        }
                    }
                }
                Op::BuildTuple(n) => {
                    let fr = fr!();
                    let at = fr.stack.len() - n as usize;
                    let items: Vec<Value> = fr.stack.drain(at..).collect();
                    fr.stack.push(Value::tuple(items));
                }
                Op::BuildList(n) => {
                    let fr = fr!();
                    let at = fr.stack.len() - n as usize;
                    let items: Vec<Value> = fr.stack.drain(at..).collect();
                    fr.stack.push(Value::list(items));
                }
                Op::BuildSet(n) => {
                    let at = fr!().stack.len() - n as usize;
                    let items: Vec<Value> = fr!().stack.drain(at..).collect();
                    let s = self.new_set(items)?;
                    push!(s);
                }
                Op::BuildSetConst(n) => {
                    let at = fr!().stack.len() - n as usize;
                    let items: Vec<Value> = fr!().stack.drain(at..).collect();
                    let tmp = self.new_set(items)?;
                    let res = self.new_set(Vec::new())?;
                    if let (Value::Obj(t), Value::Obj(r)) = (&tmp, &res) {
                        if let (Kind::Set(td), Kind::Set(rd)) = (&t.kind, &r.kind) {
                            rd.borrow_mut().merge_set(&td.borrow());
                        }
                    }
                    push!(res);
                }
                Op::BuildMap(n) => {
                    let at = fr!().stack.len() - 2 * n as usize;
                    let items: Vec<Value> = fr!().stack.drain(at..).collect();
                    let d = Object::new(Kind::Dict(RefCell::new(PyDict::new())));
                    let mut it = items.into_iter();
                    while let (Some(k), Some(v)) = (it.next(), it.next()) {
                        self.dict_set(&d, k, v)?;
                    }
                    push!(Value::Obj(d));
                }
                Op::BuildSlice(n) => {
                    let fr = fr!();
                    let step = if n == 3 {
                        fr.stack.pop().unwrap()
                    } else {
                        Value::None
                    };
                    let stop = fr.stack.pop().unwrap();
                    let start = fr.stack.pop().unwrap();
                    fr.stack
                        .push(Value::Obj(Object::new(Kind::Slice(start, stop, step))));
                }
                Op::BuildString(n) => {
                    let at = fr!().stack.len() - n as usize;
                    let items: Vec<Value> = fr!().stack.drain(at..).collect();
                    let mut s = String::new();
                    for it in &items {
                        s.push_str(it.as_str().unwrap_or(""));
                    }
                    push!(Value::string(s));
                }
                Op::ListAppend(n) => {
                    let fr = fr!();
                    let v = fr.stack.pop().unwrap();
                    let idx = fr.stack.len() - n as usize;
                    if let Some(l) = list_of(&fr.stack[idx]) {
                        l.borrow_mut().push(v);
                    }
                }
                Op::SetAdd(n) => {
                    let v = pop!();
                    let idx = fr!().stack.len() - n as usize;
                    let s = fr!().stack[idx].clone();
                    self.set_add(&s, v)?;
                }
                Op::MapAdd(n) => {
                    let v = pop!();
                    let k = pop!();
                    let idx = fr!().stack.len() - n as usize;
                    let d = fr!().stack[idx].clone();
                    if let Value::Obj(d) = d {
                        self.dict_set(&d, k, v)?;
                    }
                }
                Op::ListExtend(n) => {
                    let v = pop!();
                    let idx = fr!().stack.len() - n as usize;
                    let l = fr!().stack[idx].clone();
                    let items = self.iterate_to_vec(&v)?;
                    if let Some(l) = list_of(&l) {
                        l.borrow_mut().extend(items);
                    }
                }
                Op::SetUpdate(n) => {
                    let v = pop!();
                    let idx = fr!().stack.len() - n as usize;
                    let s = fr!().stack[idx].clone();
                    let items = self.iterate_to_vec(&v)?;
                    for i in items {
                        self.set_add(&s, i)?;
                    }
                }
                Op::DictUpdate(n) => {
                    let v = pop!();
                    let idx = fr!().stack.len() - n as usize;
                    let d = fr!().stack[idx].clone();
                    if let Value::Obj(d) = d {
                        self.dict_update_from(&d, &v)?;
                    }
                }
                Op::KwMerge => {
                    let v = pop!();
                    let st = &fr!().stack;
                    let d = st[st.len() - 1].clone();
                    let f = st[st.len() - 3].clone();
                    if let Value::Obj(d) = d {
                        self.kw_merge(&d, &v, &f)?;
                    }
                }
                Op::ListToTuple => {
                    let v = pop!();
                    let items = match list_of(&v) {
                        Some(l) => l.borrow().clone(),
                        None => Vec::new(),
                    };
                    push!(Value::tuple(items));
                }
                Op::UnpackSequence(n) => {
                    let v = pop!();
                    let items = self.unpack_items(&v, n as usize, None)?;
                    let fr = fr!();
                    for x in items.into_iter().rev() {
                        fr.stack.push(x);
                    }
                }
                Op::UnpackEx(before, after) => {
                    let v = pop!();
                    let items = self.unpack_items(&v, before as usize, Some(after as usize))?;
                    let fr = fr!();
                    for x in items.into_iter().rev() {
                        fr.stack.push(x);
                    }
                }
                Op::FormatValue(conv, has_spec) => {
                    let spec = if has_spec { Some(pop!()) } else { None };
                    let v = pop!();
                    let v = match conv {
                        1 => Value::string(self.str_of(&v)?),
                        2 => Value::string(self.repr_of(&v)?),
                        3 => {
                            let r = self.repr_of(&v)?;
                            Value::string(crate::builtins::format::ascii_escape(&r))
                        }
                        _ => v,
                    };
                    let spec_s = match &spec {
                        Some(s) => s.as_str().unwrap_or("").to_string(),
                        None => String::new(),
                    };
                    let out = if spec_s.is_empty()
                        && v.as_str().is_some()
                        && matches!(&v, Value::Obj(o) if o.cls.is_none())
                    {
                        v
                    } else {
                        Value::string(self.format_value(&v, &spec_s)?)
                    };
                    push!(out);
                }
                Op::MakeFunction(flags) => {
                    let code = pop!();
                    let closure = if flags & MF_CLOSURE != 0 {
                        Some(pop!())
                    } else {
                        None
                    };
                    let ann = if flags & MF_ANNOTATIONS != 0 {
                        Some(pop!())
                    } else {
                        None
                    };
                    let kwd = if flags & MF_KWDEFAULTS != 0 {
                        Some(pop!())
                    } else {
                        None
                    };
                    let defaults = if flags & MF_DEFAULTS != 0 {
                        Some(pop!())
                    } else {
                        None
                    };
                    let f = self.make_function(code, closure, ann, kwd, defaults)?;
                    push!(f);
                }
                Op::Call(argc) => {
                    if crate::weak::has_pending() {
                        self.run_weak_callbacks();
                    }
                    let fr = fr!();
                    let at = fr.stack.len() - argc as usize;
                    let args: Vec<Value> = fr.stack.drain(at..).collect();
                    let f = fr.stack.pop().unwrap();
                    if self.call_inline(&f, args, Vec::new())? {
                        continue;
                    }
                }
                Op::CallKw(argc) => {
                    if crate::weak::has_pending() {
                        self.run_weak_callbacks();
                    }
                    let names = pop!();
                    let fr = fr!();
                    let at = fr.stack.len() - argc as usize;
                    let mut args: Vec<Value> = fr.stack.drain(at..).collect();
                    let f = fr.stack.pop().unwrap();
                    let names = names.tuple_items().unwrap_or(&[]).to_vec();
                    let npos = args.len() - names.len();
                    let kwvals = args.split_off(npos);
                    let kw: Vec<(Obj, Value)> = names
                        .into_iter()
                        .zip(kwvals)
                        .filter_map(|(n, v)| n.as_obj().cloned().map(|o| (o, v)))
                        .collect();
                    if self.call_inline(&f, args, kw)? {
                        continue;
                    }
                }
                Op::CallEx(flags) => {
                    if crate::weak::has_pending() {
                        self.run_weak_callbacks();
                    }
                    let kwd = if flags & 1 != 0 { Some(pop!()) } else { None };
                    let args = pop!();
                    let f = pop!();
                    let args: Vec<Value> = match args.tuple_items() {
                        Some(t) => t.to_vec(),
                        None => self.iterate_to_vec(&args)?,
                    };
                    let kw = match kwd {
                        Some(d) => self.dict_to_kwargs(&d)?,
                        None => Vec::new(),
                    };
                    if self.call_inline(&f, args, kw)? {
                        continue;
                    }
                }
                Op::ReturnValue => {
                    let v = pop!();
                    self.frames.pop();
                    if crate::weak::has_pending() {
                        self.run_weak_callbacks();
                    }
                    if self.frames.len() == entry {
                        return Ok(v);
                    }
                    push!(v);
                }
                Op::YieldValue => {
                    let v = pop!();
                    let frame = self.frames.pop().unwrap();
                    self.yielded = Some(frame);
                    if self.frames.len() == entry {
                        return Ok(v);
                    }
                    return Err(self.new_exc_str("RuntimeError", "yield outside generator context"));
                }
                Op::YieldFrom => {
                    let v = pop!();
                    let it = fr!().stack.last().unwrap().clone();
                    match self.send_to_iter(&it, v)? {
                        GenResult::Yield(y) => {
                            let fr = fr!();
                            fr.pc -= 1;
                            let frame = self.frames.pop().unwrap();
                            self.yielded = Some(frame);
                            if self.frames.len() == entry {
                                return Ok(y);
                            }
                            return Err(
                                self.new_exc_str("RuntimeError", "yield outside generator context")
                            );
                        }
                        GenResult::Return(r) => {
                            let fr = fr!();
                            fr.stack.pop();
                            fr.stack.push(r);
                        }
                    }
                }
                Op::GetAwaitable => {
                    let v = pop!();
                    let it = self.get_awaitable(&v)?;
                    push!(it);
                }
                Op::GetYieldFromIter => {
                    let v = pop!();
                    let is_gen =
                        matches!(&v, Value::Obj(o) if matches!(o.kind, Kind::Generator(_)));
                    let it = if is_gen { v } else { self.get_iter(&v)? };
                    push!(it);
                }
                Op::GetAIter => {
                    let v = pop!();
                    let r = self.call_special(&v, "__aiter__", Vec::new())?;
                    push!(r);
                }
                Op::GetANext => {
                    let it = fr!().stack.last().unwrap().clone();
                    let r = self.call_special(&it, "__anext__", Vec::new())?;
                    push!(r);
                }
                Op::EndAsyncFor(t) => {
                    let exc = pop!();
                    let is_stop = match &exc {
                        Value::Obj(e) => self.exc_is(e, "StopAsyncIteration"),
                        _ => false,
                    };
                    if is_stop {
                        let fr = fr!();
                        fr.stack.pop();
                        fr.pc = t as usize;
                    } else if let Value::Obj(e) = exc {
                        self.no_tb = true;
                        return Err(e);
                    }
                }
                Op::AsyncGenWrap => {
                    let v = pop!();
                    push!(Value::Obj(Object::new(Kind::AsyncGenValue(v))));
                }
                Op::Raise(n) => {
                    let cause = if n == 2 { Some(pop!()) } else { None };
                    let exc = if n >= 1 { Some(pop!()) } else { None };
                    return Err(self.do_raise(exc, cause)?);
                }
                Op::SetupBlock(t) => {
                    let fr = fr!();
                    let depth = fr.stack.len() as u32;
                    fr.blocks.push(Block { handler: t, depth });
                }
                Op::PopBlock => {
                    fr!().blocks.pop();
                }
                Op::PushExcInfo => {
                    let exc = pop!();
                    let prev = match self.handled.take() {
                        Some(p) => Value::Obj(p),
                        None => Value::None,
                    };
                    self.handled = exc.as_obj().cloned();
                    let fr = fr!();
                    fr.stack.push(prev);
                    fr.stack.push(exc);
                }
                Op::PopExcInfo(keep) => {
                    let fr = fr!();
                    let idx = fr.stack.len() - 1 - keep as usize;
                    let v = fr.stack.remove(idx);
                    self.handled = v.as_obj().cloned();
                }
                Op::UnwindExc(keep) => {
                    let fr = fr!();
                    let idx = fr.stack.len() - 2 - keep as usize;
                    let prev = fr.stack.remove(idx);
                    fr.stack.remove(idx);
                    self.handled = prev.as_obj().cloned();
                }
                Op::CheckExcMatch => {
                    let ty = pop!();
                    let exc = fr!().stack.last().unwrap().clone();
                    let m = self.exc_matches(&exc, &ty)?;
                    push!(Value::Bool(m));
                }
                Op::ExcStarWrap => {
                    let exc = pop!();
                    let g = crate::builtins::excgroup::star_wrap(self, &exc)?;
                    push!(g);
                }
                Op::ExcStarSplit => {
                    let ty = pop!();
                    let rem = pop!();
                    let (rest, m) = crate::builtins::excgroup::star_split(self, &rem, &ty)?;
                    push!(rest);
                    push!(m);
                }
                Op::ExcStarEnd => {
                    let rest = pop!();
                    let rest = if rest.is_none() {
                        rest
                    } else {
                        crate::builtins::excgroup::star_unwrap(rest)
                    };
                    push!(rest);
                }
                Op::Reraise => {
                    let exc = pop!();
                    self.no_tb = true;
                    if let Value::Obj(e) = exc {
                        return Err(e);
                    }
                }
                Op::CleanupReraise => {
                    let newexc = pop!();
                    pop!();
                    let prev = pop!();
                    self.handled = prev.as_obj().cloned();
                    self.no_tb = true;
                    if let Value::Obj(e) = newexc {
                        return Err(e);
                    }
                }
                Op::EndFinally => {
                    let exc = pop!();
                    let prev = pop!();
                    self.handled = prev.as_obj().cloned();
                    self.no_tb = true;
                    if let Value::Obj(e) = exc {
                        return Err(e);
                    }
                }
                Op::SetupWith(t) => {
                    let mgr = pop!();
                    let (enter, exit) = self.lookup_context_methods(&mgr, false)?;
                    fr!().stack.push(exit);
                    let r = self.call(&enter, Vec::new(), Vec::new())?;
                    let fr = fr!();
                    let depth = fr.stack.len() as u32;
                    fr.blocks.push(Block { handler: t, depth });
                    fr.stack.push(r);
                }
                Op::BeforeAsyncWith => {
                    let mgr = pop!();
                    let (enter, exit) = self.lookup_context_methods(&mgr, true)?;
                    push!(exit);
                    let r = self.call(&enter, Vec::new(), Vec::new())?;
                    push!(r);
                }
                Op::WithCallExit => {
                    let exit = pop!();
                    let r = self.call(
                        &exit,
                        vec![Value::None, Value::None, Value::None],
                        Vec::new(),
                    )?;
                    push!(r);
                }
                Op::WithExceptStart => {
                    let fr = fr!();
                    let n = fr.stack.len();
                    let exit = fr.stack[n - 2].clone();
                    let exc = fr.stack[n - 1].clone();
                    let ty = match &exc {
                        Value::Obj(e) => Value::Obj(self.type_of_obj(e)),
                        _ => Value::None,
                    };
                    let prev = self.handled.replace(exc.as_obj().cloned().unwrap());
                    let tb = match &exc {
                        Value::Obj(e) => match &e.kind {
                            Kind::Exception(d) => self.make_tb(&d.borrow().tb),
                            _ => Value::None,
                        },
                        _ => Value::None,
                    };
                    let r = self.call(&exit, vec![ty, exc, tb], Vec::new());
                    self.handled = prev;
                    push!(r?);
                }
                Op::WithExceptEnd(t) => {
                    let res = pop!();
                    let exc = pop!();
                    pop!();
                    if self.truthy(&res)? {
                        fr!().pc = t as usize;
                    } else {
                        self.no_tb = true;
                        if let Value::Obj(e) = exc {
                            return Err(e);
                        }
                    }
                }
                Op::ImportName(i) => {
                    let fromlist = pop!();
                    let level = pop!();
                    let name = fr!().code.names[i as usize]
                        .as_str_kind()
                        .unwrap_or("")
                        .to_string();
                    let m =
                        self.import_name(&name, level.as_i64().unwrap_or(0) as usize, &fromlist)?;
                    push!(m);
                }
                Op::ImportFrom(i) => {
                    let name = fr!().code.names[i as usize].clone();
                    let m = fr!().stack.last().unwrap().clone();
                    let v = self.import_from(&m, &name)?;
                    push!(v);
                }
                Op::ImportStar => {
                    let m = pop!();
                    let ns = fr!().names.clone().unwrap_or_else(|| fr!().globals.clone());
                    self.import_star(&m, &ns)?;
                }
                Op::LoadBuildClass => {
                    let v = dict_get_str(&self.builtins, "__build_class__").unwrap_or(Value::None);
                    push!(v);
                }
                Op::LoadAssertionError => {
                    let c = self.exc_type("AssertionError");
                    push!(Value::Obj(c));
                }
                Op::LoadLocals => {
                    let fr = fr!();
                    let ns = fr.names.clone().unwrap_or_else(|| fr.globals.clone());
                    push!(Value::Obj(ns));
                }
                Op::LoadFromDictOrDeref(i) => {
                    let mapping = pop!();
                    let n = cell_name(&fr!().code, i as usize);
                    let key = self.str_obj(&n);
                    let found = match self.mapping_lookup(&mapping, &key)? {
                        Some(v) => Some(v),
                        None => match &fr!().cells[i as usize].kind {
                            Kind::Cell(c) => c.borrow().clone(),
                            _ => None,
                        },
                    };
                    match found {
                        Some(v) => push!(v),
                        None => {
                            return Err(self.new_exc_str(
                                "NameError",
                                &format!("cannot access free variable '{}' where it is not associated with a value in enclosing scope", n),
                            ))
                        }
                    }
                }
                Op::LoadFromDictOrGlobals(i) => {
                    let mapping = pop!();
                    let name = fr!().code.names[i as usize].clone();
                    let mut found = self.mapping_lookup(&mapping, &name)?;
                    if found.is_none() {
                        found = dict_get_name(&fr!().globals, &name);
                    }
                    if found.is_none() {
                        found = dict_get_name(&self.builtins, &name);
                    }
                    match found {
                        Some(v) => push!(v),
                        None => return Err(self.name_err(&name)),
                    }
                }
                Op::CallIntrinsic1(k) => {
                    let v = pop!();
                    let r = if k == crate::bytecode::INTRINSIC1_PRINT {
                        self.display_hook(v)?
                    } else {
                        crate::builtins::typingm::intrinsic1(self, k, v)?
                    };
                    push!(r);
                }
                Op::CallIntrinsic2(k) => {
                    let b = pop!();
                    let a = pop!();
                    let r = crate::builtins::typingm::intrinsic2(self, k, a, b)?;
                    push!(r);
                }
                Op::SetupAnnotations => {
                    let fr = fr!();
                    let ns = fr.names.clone().unwrap_or_else(|| fr.globals.clone());
                    if dict_get_str(&ns, "__annotations__").is_none() {
                        dict_set_str(&ns, "__annotations__", Value::dict(PyDict::new()));
                    }
                }
                Op::MatchSequence => {
                    let v = fr!().stack.last().unwrap().clone();
                    let r = self.match_is_sequence(&v);
                    push!(Value::Bool(r));
                }
                Op::MatchLen(n, star) => {
                    let v = fr!().stack.last().unwrap().clone();
                    let l = self.len_of(&v)?;
                    let ok = if star {
                        l >= n as usize
                    } else {
                        l == n as usize
                    };
                    push!(Value::Bool(ok));
                }
                Op::MatchSeqItem(i) => {
                    let v = pop!();
                    let items = self.iterate_to_vec(&v)?;
                    let idx = if i < 0 {
                        (items.len() as i32 + i) as usize
                    } else {
                        i as usize
                    };
                    push!(items.get(idx).cloned().unwrap_or(Value::None));
                }
                Op::MatchStarSlice(before, after) => {
                    let v = pop!();
                    let items = self.iterate_to_vec(&v)?;
                    let end = items.len() - after as usize;
                    push!(Value::list(items[before as usize..end].to_vec()));
                }
                Op::MatchMapping => {
                    let v = fr!().stack.last().unwrap().clone();
                    let r = self.match_is_mapping(&v);
                    push!(Value::Bool(r));
                }
                Op::MatchKeys(n) => {
                    let at = fr!().stack.len() - n as usize;
                    let keys: Vec<Value> = fr!().stack.drain(at..).collect();
                    let subj = fr!().stack.last().unwrap().clone();
                    let r = self.match_keys(&subj, &keys)?;
                    push!(r);
                }
                Op::MatchRest(n) => {
                    let at = fr!().stack.len() - n as usize;
                    let keys: Vec<Value> = fr!().stack.drain(at..).collect();
                    let n = fr!().stack.len();
                    let subj = fr!().stack[n - 2].clone();
                    let r = self.match_rest(&subj, &keys)?;
                    push!(r);
                }
                Op::MatchClass(npos, nkw) => {
                    let names = pop!();
                    let cls = pop!();
                    let subj = fr!().stack.last().unwrap().clone();
                    let r = self.match_class(&subj, &cls, npos as usize, nkw as usize, &names)?;
                    push!(r);
                }
            }
        }
    }

    pub fn exc_matches(&mut self, exc: &Value, ty: &Value) -> R<bool> {
        if let Some(items) = ty.tuple_items() {
            for t in items {
                if self.exc_matches(exc, t)? {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
        match ty {
            Value::Obj(c) if matches!(c.kind, Kind::Type(_)) => {
                if !self.is_subtype(c, &self.exc_type("BaseException")) {
                    return Err(self.new_exc_str(
                        "TypeError",
                        "catching classes that do not inherit from BaseException is not allowed",
                    ));
                }
                match exc {
                    Value::Obj(e) => {
                        let et = self.type_of_obj(e);
                        Ok(self.is_subtype(&et, c))
                    }
                    _ => Ok(false),
                }
            }
            _ => Err(self.new_exc_str(
                "TypeError",
                "catching classes that do not inherit from BaseException is not allowed",
            )),
        }
    }

    pub fn exc_is(&self, e: &Obj, name: &str) -> bool {
        let et = self.type_of_obj(e);
        let t = self.exc_type(name);
        self.is_subtype(&et, &t)
    }

    pub fn do_raise(&mut self, exc: Option<Value>, cause: Option<Value>) -> R<Obj> {
        let exc = match exc {
            None => match &self.handled {
                Some(h) => {
                    self.no_tb = true;
                    return Ok(h.clone());
                }
                None => {
                    return Err(self.new_exc_str("RuntimeError", "No active exception to reraise"))
                }
            },
            Some(e) => e,
        };
        let e = self.exc_from_value(exc)?;
        if let Some(c) = cause {
            let cv = match c {
                Value::None => None,
                v => Some(self.exc_from_value(v)?),
            };
            if let Kind::Exception(d) = &e.kind {
                let mut d = d.borrow_mut();
                d.cause = cv;
                d.suppress_context = true;
            }
        }
        Ok(e)
    }

    fn exc_from_value(&mut self, v: Value) -> R<Obj> {
        match &v {
            Value::Obj(o) if matches!(o.kind, Kind::Exception(_)) => Ok(o.clone()),
            Value::Obj(o)
                if matches!(o.kind, Kind::Type(_))
                    && self.is_subtype(o, &self.exc_type("BaseException")) =>
            {
                let r = self.call(&v, Vec::new(), Vec::new())?;
                match r {
                    Value::Obj(e) if matches!(e.kind, Kind::Exception(_)) => Ok(e),
                    _ => Err(self.new_exc_str(
                        "TypeError",
                        "calling exception class did not return an exception instance",
                    )),
                }
            }
            _ => Err(self.new_exc_str("TypeError", "exceptions must derive from BaseException")),
        }
    }

    pub fn int_binop_fast(op: BinOp, a: i64, b: i64) -> Option<Value> {
        match op {
            BinOp::Add => a.checked_add(b).map(Value::Int),
            BinOp::Sub => a.checked_sub(b).map(Value::Int),
            BinOp::Mult => a.checked_mul(b).map(Value::Int),
            _ => None,
        }
    }

    pub fn cmp_fast(op: CmpOp, a: i64, b: i64) -> Option<bool> {
        Some(match op {
            CmpOp::Eq => a == b,
            CmpOp::NotEq => a != b,
            CmpOp::Lt => a < b,
            CmpOp::LtE => a <= b,
            CmpOp::Gt => a > b,
            CmpOp::GtE => a >= b,
            _ => return None,
        })
    }
}

fn cell_name(code: &Code, i: usize) -> String {
    if i < code.cellvars.len() {
        code.cellvars[i].to_string()
    } else {
        code.freevars
            .get(i - code.cellvars.len())
            .map(|s| s.to_string())
            .unwrap_or_default()
    }
}

pub fn dict_get_name(d: &Obj, name: &Obj) -> Option<Value> {
    if let (Kind::Dict(dd), Kind::Str(s)) = (&d.kind, &name.kind) {
        let dd = dd.borrow();
        return dd
            .find_str(s.hash(), &s.s)
            .and_then(|i| dd.get(i).map(|e| e.val.clone()));
    }
    None
}

pub fn dict_set_name(d: &Obj, name: &Obj, v: Value) {
    if let (Kind::Dict(dd), Kind::Str(s)) = (&d.kind, &name.kind) {
        let mut dd = dd.borrow_mut();
        match dd.find_str(s.hash(), &s.s) {
            Some(i) => dd.set_val(i, v),
            None => {
                dd.insert_new(s.hash(), Value::Obj(name.clone()), v);
            }
        }
    }
}

pub fn dict_del_name(d: &Obj, name: &Obj) -> Option<Value> {
    if let (Kind::Dict(dd), Kind::Str(s)) = (&d.kind, &name.kind) {
        let mut dd = dd.borrow_mut();
        let i = dd.find_str(s.hash(), &s.s)?;
        return dd.remove(i).map(|e| e.val);
    }
    None
}

pub fn dict_get_str(d: &Obj, name: &str) -> Option<Value> {
    if let Kind::Dict(dd) = &d.kind {
        let dd = dd.borrow();
        return dd
            .find_str(hash_str(name), name)
            .and_then(|i| dd.get(i).map(|e| e.val.clone()));
    }
    None
}

pub fn dict_set_str(d: &Obj, name: &str, v: Value) {
    if let Kind::Dict(dd) = &d.kind {
        let mut dd = dd.borrow_mut();
        let h = hash_str(name);
        match dd.find_str(h, name) {
            Some(i) => dd.set_val(i, v),
            None => {
                dd.insert_new(h, Value::str(name), v);
            }
        }
    }
}

pub fn dict_del_str(d: &Obj, name: &str) -> Option<Value> {
    if let Kind::Dict(dd) = &d.kind {
        let mut dd = dd.borrow_mut();
        let i = dd.find_str(hash_str(name), name)?;
        return dd.remove(i).map(|e| e.val);
    }
    None
}

impl Object {
    pub fn as_str_kind(&self) -> Option<&str> {
        match &self.kind {
            Kind::Str(s) => Some(&s.s),
            _ => None,
        }
    }
}
