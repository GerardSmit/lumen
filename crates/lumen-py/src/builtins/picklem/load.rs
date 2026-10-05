//! The unpickler: the input buffering and opcode loop of `_pickle.c`, on the stack and opcode
//! tables of `lumen_common::pickle`.

use super::shared::{attr_opt, call_bound, getattribute, new_dict, unpickling_error, MethodRef, Shared};
use crate::builtins::memview;
use crate::builtins::numeric::{parse_float_str, parse_int_str, IntParseError};
use crate::codecs;
use crate::object::*;
use crate::pyint::BigInt;
use crate::vm::{dict_get_str, Interp};
use lumen_common::pickle::{self as pk, op as opc, Stack, StackError};
use std::cell::{Cell, RefCell};

#[derive(Default)]
pub struct Input {
    buf: Vec<u8>,
    next: usize,
    /// Bytes of `buf` before this index have been consumed from the file; those after it were
    /// only peeked.
    prefetched: usize,
    read: Option<Value>,
    readinto: Option<Value>,
    readline: Option<Value>,
    peek: Option<Value>,
}

#[derive(Default)]
pub struct UnpicklerCore {
    pub stack: RefCell<Stack<Value>>,
    pub memo: RefCell<Vec<Option<Value>>>,
    pub memo_len: Cell<usize>,
    pub input: RefCell<Input>,
    pub buffers: RefCell<Option<Value>>,
    pub encoding: RefCell<String>,
    pub errors: RefCell<String>,
    pub pers: RefCell<Option<MethodRef>>,
    pub proto: Cell<i64>,
    pub fix_imports: Cell<bool>,
}

impl Input {
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

const MEMO_INITIAL: usize = 32;

impl UnpicklerCore {
    /// Whether the input has been set (`Unpickler.__init__` ran).
    pub fn is_initialised(&self) -> bool {
        self.input.borrow().read.is_some()
    }

    pub fn clear(&self) {
        *self.input.borrow_mut() = Input::default();
        self.stack.borrow_mut().reset();
        *self.buffers.borrow_mut() = None;
        *self.pers.borrow_mut() = None;
        self.memo.borrow_mut().clear();
        self.memo_len.set(0);
    }

    pub fn reset_memo(&self) {
        let mut m = self.memo.borrow_mut();
        m.clear();
        m.resize(MEMO_INITIAL, None);
        self.memo_len.set(0);
    }

    pub fn memo_entries(&self) -> Vec<(usize, Value)> {
        self.memo.borrow().iter().enumerate().filter_map(|(i, v)| v.clone().map(|v| (i, v))).collect()
    }

    pub fn memo_size(&self) -> usize {
        self.memo.borrow().len()
    }

    pub fn set_encoding(&self, encoding: &str, errors: &str) {
        *self.encoding.borrow_mut() = encoding.to_string();
        *self.errors.borrow_mut() = errors.to_string();
    }

    pub fn memo_put(&self, it: &mut Interp, idx: usize, value: Value) -> R<()> {
        let len = self.memo.borrow().len();
        if idx >= len {
            let new_size = idx.saturating_mul(2).max(MEMO_INITIAL);
            it.check_alloc(new_size, std::mem::size_of::<Option<Value>>(), usize::MAX / 64)?;
            self.memo.borrow_mut().resize(new_size, None);
        }
        let old = self.memo.borrow_mut()[idx].replace(value);
        if old.is_none() {
            self.memo_len.set(self.memo_len.get() + 1);
        }
        Ok(())
    }

    fn memo_get(&self, idx: i64) -> Option<Value> {
        let idx = usize::try_from(idx).ok()?;
        self.memo.borrow().get(idx).cloned().flatten()
    }
}

fn bad_readline(it: &mut Interp, sh: &Shared) -> Obj {
    unpickling_error(it, sh, "pickle data was truncated")
}

/// The bytes of a bytes-like object, as `PyObject_GetBuffer(.., PyBUF_CONTIG_RO)` yields them.
pub fn buffer_bytes(it: &mut Interp, v: &Value) -> R<Vec<u8>> {
    if let Value::Obj(o) = v {
        if let Kind::Bytes(b) = &o.kind {
            return Ok(b.clone());
        }
    }
    match memview::contiguous_bytes(it, v)? {
        Some(b) => Ok(b),
        None => {
            let t = it.type_name_of(v);
            Err(it.type_error(&format!("a bytes-like object is required, not '{}'", t)))
        }
    }
}

pub fn set_string_input(it: &mut Interp, core: &UnpicklerCore, data: &Value) -> R<usize> {
    let bytes = buffer_bytes(it, data)?;
    let n = bytes.len();
    let mut inp = core.input.borrow_mut();
    inp.buf = bytes;
    inp.next = 0;
    inp.prefetched = n;
    Ok(n)
}

pub fn set_input_stream(it: &mut Interp, core: &UnpicklerCore, file: &Value) -> R<()> {
    let peek = attr_opt(it, file, "peek")?;
    let readinto = attr_opt(it, file, "readinto")?;
    let read = attr_opt(it, file, "read")?;
    let readline = attr_opt(it, file, "readline")?;
    if read.is_none() || readline.is_none() {
        return Err(it.type_error("file must have 'read' and 'readline' attributes"));
    }
    let mut inp = core.input.borrow_mut();
    inp.peek = peek;
    inp.readinto = readinto;
    inp.read = read;
    inp.readline = readline;
    Ok(())
}

pub fn set_buffers(it: &mut Interp, core: &UnpicklerCore, buffers: Option<&Value>) -> R<()> {
    *core.buffers.borrow_mut() = match buffers.filter(|v| !v.is_none()) {
        None => None,
        Some(b) => Some(it.get_iter(b)?),
    };
    Ok(())
}

pub fn find_class_impl(it: &mut Interp, sh: &Shared, proto: i64, fix_imports: bool, module_name: &Value, global_name: &Value) -> R<Value> {
    let mut module_name = module_name.clone();
    let mut global_name = global_name.clone();
    if proto < 3 && fix_imports {
        let key = Value::tuple(vec![module_name.clone(), global_name.clone()]);
        if let Some(item) = it.dict_get(&sh.name_2to3, &key)? {
            let pair = match item.tuple_items() {
                Some(t) if t.len() == 2 => t.to_vec(),
                _ => {
                    let t = it.tp_name_of(&item);
                    return Err(it.runtime_error(&format!("_compat_pickle.NAME_MAPPING values should be 2-tuples, not {}", t)));
                }
            };
            if pair[0].as_str().is_none() || pair[1].as_str().is_none() {
                let (a, b) = (it.tp_name_of(&pair[0]), it.tp_name_of(&pair[1]));
                return Err(it.runtime_error(&format!("_compat_pickle.NAME_MAPPING values should be pairs of str, not ({}, {})", a, b)));
            }
            module_name = pair[0].clone();
            global_name = pair[1].clone();
        } else if let Some(item) = it.dict_get(&sh.import_2to3, &module_name)? {
            if item.as_str().is_none() {
                let t = it.tp_name_of(&item);
                return Err(it.runtime_error(&format!("_compat_pickle.IMPORT_MAPPING values should be strings, not {}", t)));
            }
            module_name = item;
        }
    }
    let Some(name) = module_name.as_str() else {
        let t = it.tp_name_of(&module_name);
        return Err(it.type_error(&format!("module name must be str, not {}", t)));
    };
    let module = Value::Obj(it.import_module(name)?);
    getattribute(it, &module, &global_name, proto >= 4)
}

pub struct Ld<'a> {
    pub u: &'a UnpicklerCore,
    pub sh: &'a Shared,
    pub slf: Option<&'a Value>,
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

impl<'a> Ld<'a> {
    fn uerr(&self, it: &mut Interp, msg: &str) -> Obj {
        unpickling_error(it, self.sh, msg)
    }

    fn stack_err(&self, it: &mut Interp, e: StackError) -> Obj {
        self.uerr(it, e.message())
    }

    fn underflow(&self, it: &mut Interp) -> Obj {
        let e = self.u.stack.borrow().underflow();
        self.stack_err(it, e)
    }

    fn push(&self, v: Value) {
        self.u.stack.borrow_mut().push(v);
    }

    fn pop(&self, it: &mut Interp) -> R<Value> {
        let r = self.u.stack.borrow_mut().pop();
        r.map_err(|e| self.stack_err(it, e))
    }

    fn top(&self, it: &mut Interp) -> R<Value> {
        let r = self.u.stack.borrow().top().cloned();
        r.map_err(|e| self.stack_err(it, e))
    }

    fn marker(&self, it: &mut Interp) -> R<usize> {
        let r = self.u.stack.borrow_mut().marker();
        r.map_err(|e| self.stack_err(it, e))
    }

    fn pop_tuple(&self, it: &mut Interp, start: usize) -> R<Vec<Value>> {
        let r = self.u.stack.borrow_mut().take_from(start);
        r.map_err(|e| self.stack_err(it, e))
    }

    fn skip_consumed(&self, it: &mut Interp) -> R<()> {
        let (consumed, read) = {
            let inp = self.u.input.borrow();
            (inp.next.saturating_sub(inp.prefetched), inp.read.clone())
        };
        if consumed == 0 {
            return Ok(());
        }
        if let Some(read) = read {
            it.call(&read, vec![Value::Int(consumed as i64)], Vec::new())?;
        }
        let mut inp = self.u.input.borrow_mut();
        inp.prefetched = inp.next;
        Ok(())
    }

    /// Replaces the buffer with fresh data from the file; `None` reads a whole line. Returns the
    /// number of bytes now buffered.
    fn read_from_file(&self, it: &mut Interp, n: Option<usize>) -> R<usize> {
        self.skip_consumed(it)?;
        let data = match n {
            None => {
                let readline = self.u.input.borrow().readline.clone().expect("readline is set with read");
                it.call(&readline, Vec::new(), Vec::new())?
            }
            Some(n) => {
                let peek = self.u.input.borrow().peek.clone();
                if let Some(peek) = peek.filter(|_| n < pk::PREFETCH) {
                    match it.call(&peek, vec![Value::Int(pk::PREFETCH as i64)], Vec::new()) {
                        Err(e) if it.exc_is(&e, "NotImplementedError") => {
                            self.u.input.borrow_mut().peek = None;
                        }
                        Err(e) => return Err(e),
                        Ok(d) => {
                            let size = set_string_input(it, self.u, &d)?;
                            self.u.input.borrow_mut().prefetched = 0;
                            if n <= size {
                                return Ok(n);
                            }
                        }
                    }
                }
                let read = self.u.input.borrow().read.clone().expect("read is set");
                it.call(&read, vec![Value::Int(n as i64)], Vec::new())?
            }
        };
        set_string_input(it, self.u, &data)
    }

    /// Makes `n` more bytes available, leaving the cursor just past them.
    fn advance(&self, it: &mut Interp, n: usize) -> R<()> {
        {
            let mut inp = self.u.input.borrow_mut();
            if n <= inp.buf.len() - inp.next {
                inp.next += n;
                return Ok(());
            }
            if inp.next.checked_add(n).is_none_or(|t| t > isize::MAX as usize) {
                drop(inp);
                return Err(self.uerr(it, "read would overflow (invalid bytecode)"));
            }
            if inp.read.is_none() {
                drop(inp);
                return Err(bad_readline(it, self.sh));
            }
        }
        let got = self.read_from_file(it, Some(n))?;
        if got < n {
            return Err(bad_readline(it, self.sh));
        }
        self.u.input.borrow_mut().next = n;
        Ok(())
    }

    fn read_byte(&self, it: &mut Interp) -> R<u8> {
        {
            let mut inp = self.u.input.borrow_mut();
            if inp.next < inp.buf.len() {
                let b = inp.buf[inp.next];
                inp.next += 1;
                return Ok(b);
            }
        }
        self.advance(it, 1)?;
        let inp = self.u.input.borrow();
        Ok(inp.buf[inp.next - 1])
    }

    fn read_n(&self, it: &mut Interp, n: usize) -> R<Vec<u8>> {
        self.advance(it, n)?;
        let inp = self.u.input.borrow();
        Ok(inp.buf[inp.next - n..inp.next].to_vec())
    }

    /// A payload read straight into its destination, past the prefetch buffer.
    fn read_into(&self, it: &mut Interp, n: usize) -> R<Vec<u8>> {
        it.check_alloc(n, 1, isize::MAX as usize)?;
        let mut out: Vec<u8> = Vec::new();
        out.try_reserve_exact(n.min(1 << 26)).map_err(|_| it.memory_error())?;
        let mut remaining = n;
        {
            let mut inp = self.u.input.borrow_mut();
            let in_buffer = inp.buf.len() - inp.next;
            if in_buffer > 0 {
                let take = in_buffer.min(remaining);
                let start = inp.next;
                out.extend_from_slice(&inp.buf[start..start + take]);
                inp.next += take;
                remaining -= take;
                if remaining == 0 {
                    return Ok(out);
                }
            }
        }
        let (read, readinto) = {
            let inp = self.u.input.borrow();
            (inp.read.clone(), inp.readinto.clone())
        };
        let Some(read) = read else { return Err(bad_readline(it, self.sh)) };
        self.skip_consumed(it)?;
        let Some(readinto) = readinto else {
            let data = it.call(&read, vec![Value::Int(remaining as i64)], Vec::new())?;
            let Value::Obj(o) = &data else {
                let t = it.repr_of(&Value::Obj(it.type_of(&data)))?;
                return Err(it.value_error(&format!("read() returned non-bytes object ({})", t)));
            };
            let Kind::Bytes(b) = &o.kind else {
                let t = it.repr_of(&Value::Obj(it.type_of(&data)))?;
                return Err(it.value_error(&format!("read() returned non-bytes object ({})", t)));
            };
            if b.len() < remaining {
                return Err(bad_readline(it, self.sh));
            }
            out.extend_from_slice(&b[..remaining]);
            return Ok(out);
        };
        let dest = Value::bytearray(vec![0u8; remaining]);
        let mv_ty = dict_get_str(&it.builtins, "memoryview").unwrap_or(Value::None);
        let view = it.call(&mv_ty, vec![dest.clone()], Vec::new())?;
        let r = it.call(&readinto, vec![view], Vec::new())?;
        let got = it.index_of(&r)?;
        if got < 0 {
            return Err(it.value_error("readinto() returned negative size"));
        }
        if (got as usize) < remaining {
            return Err(bad_readline(it, self.sh));
        }
        if let Value::Obj(o) = &dest {
            if let Kind::ByteArray(store) = &o.kind {
                out.extend_from_slice(&store.to_vec());
            }
        }
        Ok(out)
    }

    /// A line including its newline.
    fn readline(&self, it: &mut Interp) -> R<Vec<u8>> {
        {
            let mut inp = self.u.input.borrow_mut();
            let start = inp.next;
            if let Some(off) = inp.buf[start..].iter().position(|&b| b == b'\n') {
                let end = start + off + 1;
                inp.next = end;
                return Ok(inp.buf[start..end].to_vec());
            }
            if inp.read.is_none() {
                drop(inp);
                return Err(bad_readline(it, self.sh));
            }
        }
        let got = self.read_from_file(it, None)?;
        let mut inp = self.u.input.borrow_mut();
        if got == 0 || inp.buf[got - 1] != b'\n' {
            drop(inp);
            return Err(bad_readline(it, self.sh));
        }
        inp.next = got;
        Ok(inp.buf[..got].to_vec())
    }

    fn decode_string(&self, it: &mut Interp, data: Vec<u8>) -> R<Value> {
        let (enc, errors) = (self.u.encoding.borrow().clone(), self.u.errors.borrow().clone());
        if enc == "bytes" {
            return Ok(Value::bytes(data));
        }
        Ok(Value::string(codecs::decode(it, &data, &enc, &errors)?))
    }

    fn read_size(&self, it: &mut Interp, nbytes: usize, what: &str) -> R<usize> {
        let b = self.read_n(it, nbytes)?;
        match pk::calc_binsize(&b) {
            Some(n) => Ok(n),
            None => Err(it.overflow_err(&format!("{} exceeds system's maximum size of {} bytes", what, isize::MAX))),
        }
    }

    fn parse_int(&self, it: &mut Interp, text: &str, base: u32) -> R<Option<BigInt>> {
        match parse_int_str(text, base, it.int_max_str_digits()) {
            Ok(v) => Ok(Some(v)),
            Err(IntParseError::Invalid) => Ok(None),
            Err(IntParseError::TooManyDigits(n)) => {
                it.check_parse_digits(n)?;
                Ok(None)
            }
        }
    }

    fn parse_int_or_err(&self, it: &mut Interp, text: &str, base: u32) -> R<BigInt> {
        match self.parse_int(it, text, base)? {
            Some(v) => Ok(v),
            None => {
                let r = it.repr_of(&Value::str(text))?;
                Err(it.value_error(&format!("invalid literal for int() with base {}: {}", base, r)))
            }
        }
    }

    fn to_ssize(&self, it: &mut Interp, v: &BigInt) -> R<i64> {
        match v.to_i64() {
            Some(i) => Ok(i),
            None => Err(it.overflow_err("Python int too large to convert to C ssize_t")),
        }
    }

    pub fn load(&self, it: &mut Interp) -> R<Value> {
        self.u.stack.borrow_mut().reset();
        self.u.proto.set(0);
        loop {
            let op = match self.read_byte(it) {
                Ok(b) => b,
                Err(e) => {
                    let ty = it.type_of_obj(&e);
                    if it.is_subtype(&ty, &self.sh.unpickling_error) {
                        return Err(it.new_exc_str("EOFError", "Ran out of input"));
                    }
                    return Err(e);
                }
            };
            if op == opc::STOP {
                break;
            }
            self.dispatch(it, op)?;
        }
        self.skip_consumed(it)?;
        self.pop(it)
    }

    fn dispatch(&self, it: &mut Interp, op: u8) -> R<()> {
        match op {
            opc::NONE => self.push(Value::None),
            opc::BININT => self.load_binint(it, 4)?,
            opc::BININT1 => self.load_binint(it, 1)?,
            opc::BININT2 => self.load_binint(it, 2)?,
            opc::INT => self.load_int(it)?,
            opc::LONG => self.load_long(it)?,
            opc::LONG1 => self.load_counted_long(it, 1)?,
            opc::LONG4 => self.load_counted_long(it, 4)?,
            opc::FLOAT => self.load_float(it)?,
            opc::BINFLOAT => self.load_binfloat(it)?,
            opc::SHORT_BINBYTES => self.load_counted_binbytes(it, 1)?,
            opc::BINBYTES => self.load_counted_binbytes(it, 4)?,
            opc::BINBYTES8 => self.load_counted_binbytes(it, 8)?,
            opc::BYTEARRAY8 => self.load_counted_bytearray(it)?,
            opc::NEXT_BUFFER => self.load_next_buffer(it)?,
            opc::READONLY_BUFFER => self.load_readonly_buffer(it)?,
            opc::SHORT_BINSTRING => self.load_counted_binstring(it, 1)?,
            opc::BINSTRING => self.load_counted_binstring(it, 4)?,
            opc::STRING => self.load_string(it)?,
            opc::UNICODE => self.load_unicode(it)?,
            opc::SHORT_BINUNICODE => self.load_counted_binunicode(it, 1)?,
            opc::BINUNICODE => self.load_counted_binunicode(it, 4)?,
            opc::BINUNICODE8 => self.load_counted_binunicode(it, 8)?,
            opc::EMPTY_TUPLE => self.load_counted_tuple(it, 0)?,
            opc::TUPLE1 => self.load_counted_tuple(it, 1)?,
            opc::TUPLE2 => self.load_counted_tuple(it, 2)?,
            opc::TUPLE3 => self.load_counted_tuple(it, 3)?,
            opc::TUPLE => {
                let i = self.marker(it)?;
                let len = self.u.stack.borrow().len().saturating_sub(i);
                self.load_counted_tuple(it, len)?
            }
            opc::EMPTY_LIST => self.push(Value::list(Vec::new())),
            opc::LIST => {
                let i = self.marker(it)?;
                let items = self.u.stack.borrow_mut().drain_from(i);
                self.push(Value::list(items));
            }
            opc::EMPTY_DICT => self.push(Value::Obj(new_dict())),
            opc::DICT => self.load_dict(it)?,
            opc::EMPTY_SET => {
                let s = it.new_set(Vec::new())?;
                self.push(s);
            }
            opc::ADDITEMS => self.load_additems(it)?,
            opc::FROZENSET => {
                let i = self.marker(it)?;
                let items = self.pop_tuple(it, i)?;
                let f = it.new_frozenset_from(items)?;
                self.push(f);
            }
            opc::OBJ => self.load_obj(it)?,
            opc::INST => self.load_inst(it)?,
            opc::NEWOBJ => self.load_newobj(it, false)?,
            opc::NEWOBJ_EX => self.load_newobj(it, true)?,
            opc::GLOBAL => self.load_global(it)?,
            opc::STACK_GLOBAL => self.load_stack_global(it)?,
            opc::APPEND => {
                let len = self.u.stack.borrow().len() as i64;
                if len - 1 <= self.u.stack.borrow().fence() as i64 {
                    return Err(self.underflow(it));
                }
                self.do_append(it, len - 1)?
            }
            opc::APPENDS => {
                let i = self.marker(it)?;
                self.do_append(it, i as i64)?
            }
            opc::BUILD => self.load_build(it)?,
            opc::DUP => {
                let v = self.top(it)?;
                self.push(v);
            }
            opc::BINGET => {
                let idx = self.read_byte(it)? as i64;
                self.get_memo(it, idx)?
            }
            opc::LONG_BINGET => {
                let b = self.read_n(it, 4)?;
                let idx = pk::calc_binsize(&b).map_or(-1, |n| n as i64);
                self.get_memo(it, idx)?
            }
            opc::GET => self.load_get(it)?,
            opc::MARK => self.u.stack.borrow_mut().mark(),
            opc::BINPUT => {
                let idx = self.read_byte(it)? as usize;
                let v = self.top(it)?;
                self.u.memo_put(it, idx, v)?
            }
            opc::LONG_BINPUT => {
                let b = self.read_n(it, 4)?;
                let v = self.top(it)?;
                match pk::calc_binsize(&b) {
                    Some(idx) => self.u.memo_put(it, idx, v)?,
                    None => return Err(it.value_error("negative LONG_BINPUT argument")),
                }
            }
            opc::PUT => self.load_put(it)?,
            opc::MEMOIZE => {
                let v = self.top(it)?;
                self.u.memo_put(it, self.u.memo_len.get(), v)?
            }
            opc::POP => {
                let r = self.u.stack.borrow_mut().pop_or_unmark();
                r.map_err(|e| self.stack_err(it, e))?
            }
            opc::POP_MARK => {
                let i = self.marker(it)?;
                self.u.stack.borrow_mut().drain_from(i);
            }
            opc::SETITEM => {
                let len = self.u.stack.borrow().len() as i64;
                self.do_setitems(it, len - 2)?
            }
            opc::SETITEMS => {
                let i = self.marker(it)?;
                self.do_setitems(it, i as i64)?
            }
            opc::PERSID => self.load_persid(it)?,
            opc::BINPERSID => self.load_binpersid(it)?,
            opc::REDUCE => self.load_reduce(it)?,
            opc::PROTO => {
                let i = self.read_byte(it)?;
                if i > pk::HIGHEST_PROTOCOL {
                    return Err(it.value_error(&format!("unsupported pickle protocol: {}", i)));
                }
                self.u.proto.set(i as i64);
            }
            opc::FRAME => {
                let b = self.read_n(it, 8)?;
                let Some(frame_len) = pk::calc_binsize(&b) else {
                    return Err(it.overflow_err(&format!("FRAME length exceeds system's maximum of {} bytes", isize::MAX)));
                };
                self.advance(it, frame_len)?;
                self.u.input.borrow_mut().next -= frame_len;
            }
            opc::EXT1 => self.load_extension(it, 1)?,
            opc::EXT2 => self.load_extension(it, 2)?,
            opc::EXT4 => self.load_extension(it, 4)?,
            opc::NEWTRUE => self.push(Value::Bool(true)),
            opc::NEWFALSE => self.push(Value::Bool(false)),
            c => {
                let msg = if (0x20..=0x7e).contains(&c) && c != b'\'' && c != b'\\' {
                    format!("invalid load key, '{}'.", c as char)
                } else {
                    format!("invalid load key, '\\x{:02x}'.", c)
                };
                return Err(self.uerr(it, &msg));
            }
        }
        Ok(())
    }

    fn load_binint(&self, it: &mut Interp, size: usize) -> R<()> {
        let b = self.read_n(it, size)?;
        self.push(Value::Int(pk::calc_binint(&b)));
        Ok(())
    }

    fn load_int(&self, it: &mut Interp) -> R<()> {
        let line = self.readline(it)?;
        if line.len() < 2 {
            return Err(bad_readline(it, self.sh));
        }
        let simple = match pk::strtol_base0(&line) {
            Some((x, end)) if end == line.len() || line[end] == b'\n' => Some(x),
            _ => None,
        };
        let value = match simple {
            Some(x) if line.len() == 3 && (x == 0 || x == 1) => Value::Bool(x == 1),
            Some(x) => Value::Int(x),
            None => match self.parse_int(it, &lossy(&line), 0)? {
                Some(v) => Value::big(v),
                None => return Err(it.value_error("could not convert string to int")),
            },
        };
        self.push(value);
        Ok(())
    }

    fn load_long(&self, it: &mut Interp) -> R<()> {
        let line = self.readline(it)?;
        if line.len() < 2 {
            return Err(bad_readline(it, self.sh));
        }
        let text = if line[line.len() - 2] == b'L' { &line[..line.len() - 2] } else { &line[..] };
        let text = lossy(text);
        let v = self.parse_int_or_err(it, &text, 0)?;
        self.push(Value::big(v));
        Ok(())
    }

    fn load_counted_long(&self, it: &mut Interp, size: usize) -> R<()> {
        let b = self.read_n(it, size)?;
        let n = pk::calc_binint(&b);
        if n < 0 {
            return Err(self.uerr(it, "LONG pickle has negative byte count"));
        }
        let v = if n == 0 {
            Value::Int(0)
        } else {
            let data = self.read_n(it, n as usize)?;
            Value::big(pk::decode_long(&data))
        };
        self.push(v);
        Ok(())
    }

    fn load_float(&self, it: &mut Interp) -> R<()> {
        let line = self.readline(it)?;
        if line.len() < 2 {
            return Err(bad_readline(it, self.sh));
        }
        let text = lossy(&line);
        match parse_float_str(&text) {
            Some(d) => {
                self.push(Value::Float(d));
                Ok(())
            }
            None => {
                let r = it.repr_of(&Value::string(text))?;
                Err(it.value_error(&format!("could not convert string to float: {}", r)))
            }
        }
    }

    fn load_binfloat(&self, it: &mut Interp) -> R<()> {
        let b = self.read_n(it, 8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(&b);
        self.push(Value::Float(f64::from_be_bytes(a)));
        Ok(())
    }

    fn load_string(&self, it: &mut Interp) -> R<()> {
        let line = self.readline(it)?;
        let len = line.len() - 1;
        if !(len >= 2 && line[0] == line[len - 1] && (line[0] == b'\'' || line[0] == b'"')) {
            return Err(self.uerr(it, "the STRING opcode argument must be quoted"));
        }
        let decoded = match codecs::escape_decode(&line[1..len - 1], "strict") {
            Ok(d) => d,
            Err(msg) => return Err(it.value_error(&msg)),
        };
        let v = self.decode_string(it, decoded)?;
        self.push(v);
        Ok(())
    }

    fn load_counted_binstring(&self, it: &mut Interp, nbytes: usize) -> R<()> {
        let b = self.read_n(it, nbytes)?;
        let Some(size) = pk::calc_binsize(&b) else {
            return Err(self.uerr(it, &format!("BINSTRING exceeds system's maximum size of {} bytes", isize::MAX)));
        };
        let data = self.read_n(it, size)?;
        let v = self.decode_string(it, data)?;
        self.push(v);
        Ok(())
    }

    fn load_counted_binbytes(&self, it: &mut Interp, nbytes: usize) -> R<()> {
        let size = self.read_size(it, nbytes, "BINBYTES")?;
        let data = self.read_into(it, size)?;
        self.push(Value::bytes(data));
        Ok(())
    }

    fn load_counted_bytearray(&self, it: &mut Interp) -> R<()> {
        let size = self.read_size(it, 8, "BYTEARRAY8")?;
        let data = self.read_into(it, size)?;
        self.push(Value::bytearray(data));
        Ok(())
    }

    fn load_next_buffer(&self, it: &mut Interp) -> R<()> {
        let Some(buffers) = self.u.buffers.borrow().clone() else {
            return Err(self.uerr(it, "pickle stream refers to out-of-band data but no *buffers* argument was given"));
        };
        match it.iter_next(&buffers)? {
            Some(b) => {
                self.push(b);
                Ok(())
            }
            None => Err(self.uerr(it, "not enough out-of-band buffers")),
        }
    }

    fn load_readonly_buffer(&self, it: &mut Interp) -> R<()> {
        let obj = self.top(it)?;
        let mv_ty = dict_get_str(&it.builtins, "memoryview").unwrap_or(Value::None);
        let view = it.call(&mv_ty, vec![obj], Vec::new())?;
        let ro = it.get_attr_str(&view, "readonly")?;
        if !it.truthy(&ro)? {
            let view = it.call_method(&view, "toreadonly", Vec::new())?;
            let mut st = self.u.stack.borrow_mut();
            if let Ok(top) = st.top_mut() {
                *top = view;
            }
        }
        Ok(())
    }

    fn load_unicode(&self, it: &mut Interp) -> R<()> {
        let line = self.readline(it)?;
        let (s, _) = codecs::unicode_escape_decode(it, &line[..line.len() - 1], "strict", true, true)?;
        self.push(Value::string(s));
        Ok(())
    }

    fn load_counted_binunicode(&self, it: &mut Interp, nbytes: usize) -> R<()> {
        let size = self.read_size(it, nbytes, "BINUNICODE")?;
        let data = self.read_n(it, size)?;
        let (s, _) = codecs::utf8_decode(it, &data, "surrogatepass", true)?;
        self.push(Value::string(s));
        Ok(())
    }

    fn load_counted_tuple(&self, it: &mut Interp, len: usize) -> R<()> {
        let size = self.u.stack.borrow().len();
        if size < len {
            return Err(self.underflow(it));
        }
        let items = self.pop_tuple(it, size - len)?;
        self.push(Value::tuple(items));
        Ok(())
    }

    fn load_dict(&self, it: &mut Interp) -> R<()> {
        let i = self.marker(it)?;
        let len = self.u.stack.borrow().len();
        if (len - i) % 2 != 0 {
            return Err(self.uerr(it, "odd number of items for DICT"));
        }
        let items: Vec<Value> = self.u.stack.borrow().items()[i..].to_vec();
        let dict = new_dict();
        for pair in items.chunks(2) {
            it.dict_set(&dict, pair[0].clone(), pair[1].clone())?;
        }
        self.u.stack.borrow_mut().drain_from(i);
        self.push(Value::Obj(dict));
        Ok(())
    }

    fn instantiate(&self, it: &mut Interp, cls: &Value, args: Vec<Value>) -> R<Value> {
        if args.is_empty() && cls.is_type() && attr_opt(it, cls, "__getinitargs__")?.is_none() {
            return it.call_method(cls, "__new__", vec![cls.clone()]);
        }
        it.call(cls, args, Vec::new())
    }

    fn load_obj(&self, it: &mut Interp) -> R<()> {
        let i = self.marker(it)?;
        if self.u.stack.borrow().len() < i + 1 {
            return Err(self.underflow(it));
        }
        let args = self.pop_tuple(it, i + 1)?;
        let cls = self.pop(it)?;
        let obj = self.instantiate(it, &cls, args)?;
        self.push(obj);
        Ok(())
    }

    fn ascii_line(&self, it: &mut Interp, line: &[u8]) -> R<Value> {
        Ok(Value::string(codecs::ascii_decode(it, &line[..line.len() - 1], "strict")?))
    }

    fn load_inst(&self, it: &mut Interp) -> R<()> {
        let i = self.marker(it)?;
        let line = self.readline(it)?;
        if line.len() < 2 {
            return Err(bad_readline(it, self.sh));
        }
        let module_name = self.ascii_line(it, &line)?;
        let line = self.readline(it)?;
        if line.len() < 2 {
            return Err(bad_readline(it, self.sh));
        }
        let class_name = self.ascii_line(it, &line)?;
        let cls = self.find_class(it, &module_name, &class_name)?;
        let args = self.pop_tuple(it, i)?;
        let obj = self.instantiate(it, &cls, args)?;
        self.push(obj);
        Ok(())
    }

    fn newobj_error(&self, it: &mut Interp, what: &str, ex: bool, tail: &str, v: &Value) -> Obj {
        let t = it.tp_name_of(v);
        let name = if ex { "NEWOBJ_EX" } else { "NEWOBJ" };
        self.uerr(it, &format!("{} {} {}, not {}", name, what, tail, t))
    }

    fn load_newobj(&self, it: &mut Interp, use_kwargs: bool) -> R<()> {
        let kwargs = if use_kwargs { Some(self.pop(it)?) } else { None };
        let args = self.pop(it)?;
        let cls = self.pop(it)?;
        if !cls.is_type() {
            return Err(self.newobj_error(it, "class argument", use_kwargs, "must be a type", &cls));
        }
        let Some(arg_items) = args.tuple_items() else {
            return Err(self.newobj_error(it, "args argument", use_kwargs, "must be a tuple", &args));
        };
        let mut call_args = Vec::with_capacity(arg_items.len() + 1);
        call_args.push(cls.clone());
        call_args.extend_from_slice(arg_items);
        let kw = match &kwargs {
            Some(k) => {
                if dict_of(k).is_none() {
                    return Err(self.newobj_error(it, "kwargs argument", use_kwargs, "must be a dict", k));
                }
                super::shared::dict_to_kw(it, k)?
            }
            None => Vec::new(),
        };
        let new = it.get_attr_str(&cls, "__new__")?;
        let obj = it.call(&new, call_args, kw)?;
        self.push(obj);
        Ok(())
    }

    fn find_class(&self, it: &mut Interp, module_name: &Value, global_name: &Value) -> R<Value> {
        match self.slf {
            Some(s) => it.call_method(s, "find_class", vec![module_name.clone(), global_name.clone()]),
            None => find_class_impl(it, self.sh, self.u.proto.get(), self.u.fix_imports.get(), module_name, global_name),
        }
    }

    fn load_global(&self, it: &mut Interp) -> R<()> {
        let line = self.readline(it)?;
        if line.len() < 2 {
            return Err(bad_readline(it, self.sh));
        }
        let (m, _) = codecs::utf8_decode(it, &line[..line.len() - 1], "strict", true)?;
        let line = self.readline(it)?;
        if line.len() < 2 {
            return Err(bad_readline(it, self.sh));
        }
        let (g, _) = codecs::utf8_decode(it, &line[..line.len() - 1], "strict", true)?;
        let global = self.find_class(it, &Value::string(m), &Value::string(g))?;
        self.push(global);
        Ok(())
    }

    fn load_stack_global(&self, it: &mut Interp) -> R<()> {
        let global_name = self.pop(it)?;
        let module_name = self.pop(it)?;
        if module_name.as_exact_str().is_none() || global_name.as_exact_str().is_none() {
            return Err(self.uerr(it, "STACK_GLOBAL requires str"));
        }
        let global = self.find_class(it, &module_name, &global_name)?;
        self.push(global);
        Ok(())
    }

    fn no_pers_load(&self, it: &mut Interp) -> Obj {
        self.uerr(it, "A load persistent id instruction was encountered, but no persistent_load function was specified.")
    }

    fn load_persid(&self, it: &mut Interp) -> R<()> {
        let Some(pers) = self.u.pers.borrow().clone() else {
            return Err(self.no_pers_load(it));
        };
        let line = self.readline(it)?;
        let pid = match codecs::ascii_decode(it, &line[..line.len() - 1], "strict") {
            Ok(s) => Value::string(s),
            Err(e) if it.exc_is(&e, "UnicodeDecodeError") => {
                return Err(self.uerr(it, "persistent IDs in protocol 0 must be ASCII strings"));
            }
            Err(e) => return Err(e),
        };
        let obj = call_bound(it, &pers, self.slf, pid)?;
        self.push(obj);
        Ok(())
    }

    fn load_binpersid(&self, it: &mut Interp) -> R<()> {
        let Some(pers) = self.u.pers.borrow().clone() else {
            return Err(self.no_pers_load(it));
        };
        let pid = self.pop(it)?;
        let obj = call_bound(it, &pers, self.slf, pid)?;
        self.push(obj);
        Ok(())
    }

    fn get_memo(&self, it: &mut Interp, idx: i64) -> R<()> {
        match self.u.memo_get(idx) {
            Some(v) => {
                self.push(v);
                Ok(())
            }
            None => Err(self.uerr(it, &format!("Memo value not found at index {}", idx))),
        }
    }

    fn load_get(&self, it: &mut Interp) -> R<()> {
        let line = self.readline(it)?;
        if line.len() < 2 {
            return Err(bad_readline(it, self.sh));
        }
        let key = self.parse_int_or_err(it, &lossy(&line), 10)?;
        let idx = self.to_ssize(it, &key)?;
        self.get_memo(it, idx)
    }

    fn load_put(&self, it: &mut Interp) -> R<()> {
        let line = self.readline(it)?;
        if line.len() < 2 {
            return Err(bad_readline(it, self.sh));
        }
        let value = self.top(it)?;
        let key = self.parse_int_or_err(it, &lossy(&line), 10)?;
        let idx = self.to_ssize(it, &key)?;
        if idx < 0 {
            return Err(it.value_error("negative PUT argument"));
        }
        self.u.memo_put(it, idx as usize, value)
    }

    fn load_extension(&self, it: &mut Interp, nbytes: usize) -> R<()> {
        let b = self.read_n(it, nbytes)?;
        let code = pk::calc_binint(&b);
        if code <= 0 {
            return Err(self.uerr(it, "EXT specifies code <= 0"));
        }
        let py_code = Value::Int(code);
        if let Some(obj) = it.dict_get(&self.sh.ext_cache, &py_code)? {
            self.push(obj);
            return Ok(());
        }
        let Some(pair) = it.dict_get(&self.sh.inverted, &py_code)? else {
            return Err(it.value_error(&format!("unregistered extension code {}", code)));
        };
        let names = match pair.tuple_items() {
            Some(t) if t.len() == 2 && t[0].as_str().is_some() && t[1].as_str().is_some() => (t[0].clone(), t[1].clone()),
            _ => return Err(it.value_error(&format!("_inverted_registry[{}] isn't a 2-tuple of strings", code))),
        };
        let obj = self.find_class(it, &names.0, &names.1)?;
        it.dict_set(&self.sh.ext_cache, py_code, obj.clone())?;
        self.push(obj);
        Ok(())
    }

    fn do_append(&self, it: &mut Interp, x: i64) -> R<()> {
        let (len, fence) = {
            let st = self.u.stack.borrow();
            (st.len() as i64, st.fence() as i64)
        };
        if x > len || x <= fence {
            return Err(self.underflow(it));
        }
        if len == x {
            return Ok(());
        }
        let x = x as usize;
        let list = self.u.stack.borrow().get(x - 1).cloned().expect("index checked against the length");
        let items = self.u.stack.borrow_mut().drain_from(x);
        if let Value::Obj(o) = &list {
            if o.cls.is_none() {
                if let Kind::List(l) = &o.kind {
                    l.borrow_mut().extend(items);
                    return Ok(());
                }
            }
        }
        match attr_opt(it, &list, "extend")? {
            Some(extend) => {
                it.call(&extend, vec![Value::list(items)], Vec::new())?;
            }
            None => {
                let append = it.get_attr_str(&list, "append")?;
                for item in items {
                    it.call(&append, vec![item], Vec::new())?;
                }
            }
        }
        Ok(())
    }

    fn do_setitems(&self, it: &mut Interp, x: i64) -> R<()> {
        let (len, fence) = {
            let st = self.u.stack.borrow();
            (st.len() as i64, st.fence() as i64)
        };
        if x > len || x <= fence {
            return Err(self.underflow(it));
        }
        if len == x {
            return Ok(());
        }
        if (len - x) % 2 != 0 {
            return Err(self.uerr(it, "odd number of items for SETITEMS"));
        }
        let x = x as usize;
        let dict = self.u.stack.borrow().get(x - 1).cloned().expect("index checked against the length");
        let items = self.u.stack.borrow_mut().drain_from(x);
        for pair in items.chunks(2) {
            it.setitem(&dict, pair[0].clone(), pair[1].clone())?;
        }
        Ok(())
    }

    fn load_additems(&self, it: &mut Interp) -> R<()> {
        let mark = self.marker(it)?;
        let (len, fence) = {
            let st = self.u.stack.borrow();
            (st.len(), st.fence())
        };
        if mark > len || mark <= fence {
            return Err(self.underflow(it));
        }
        if len == mark {
            return Ok(());
        }
        let set = self.u.stack.borrow().get(mark - 1).cloned().expect("index checked against the length");
        let items = self.pop_tuple(it, mark)?;
        if let Value::Obj(o) = &set {
            if matches!(o.kind, Kind::Set(_)) {
                for item in items {
                    it.set_add_obj(o, item)?;
                }
                return Ok(());
            }
        }
        let add = it.get_attr_str(&set, "add")?;
        for item in items {
            it.call(&add, vec![item], Vec::new())?;
        }
        Ok(())
    }

    fn load_build(&self, it: &mut Interp) -> R<()> {
        {
            let st = self.u.stack.borrow();
            if (st.len() as i64) - 2 < st.fence() as i64 {
                drop(st);
                return Err(self.underflow(it));
            }
        }
        let state = self.pop(it)?;
        let inst = self.u.stack.borrow().items().last().cloned().expect("a second item is on the stack");

        if let Some(setstate) = attr_opt(it, &inst, "__setstate__")? {
            it.call(&setstate, vec![state], Vec::new())?;
            return Ok(());
        }

        let (state, slotstate) = match state.tuple_items() {
            Some(t) if t.len() == 2 => (t[0].clone(), Some(t[1].clone())),
            _ => (state, None),
        };

        if !state.is_none() {
            let Some(pd) = dict_of(&state) else {
                return Err(self.uerr(it, "state is not a dictionary"));
            };
            let entries: Vec<(Value, Value)> = pd.borrow().iter().map(|e| (e.key.clone(), e.val.clone())).collect();
            let dict = it.get_attr_str(&inst, "__dict__")?;
            for (k, v) in entries {
                it.setitem(&dict, k, v)?;
            }
        }

        if let Some(slots) = slotstate {
            let Some(pd) = dict_of(&slots) else {
                return Err(self.uerr(it, "slot state is not a dictionary"));
            };
            let entries: Vec<(Value, Value)> = pd.borrow().iter().map(|e| (e.key.clone(), e.val.clone())).collect();
            for (k, v) in entries {
                match k {
                    Value::Obj(ref name) if matches!(name.kind, Kind::Str(_)) => it.set_attr(&inst, name, v)?,
                    other => {
                        let t = it.tp_name_of(&other);
                        return Err(it.type_error(&format!("attribute name must be string, not '{}'", t)));
                    }
                }
            }
        }
        Ok(())
    }

    fn load_reduce(&self, it: &mut Interp) -> R<()> {
        let argtup = self.pop(it)?;
        let callable = self.pop(it)?;
        let Some(args) = argtup.tuple_items() else {
            return Err(it.type_error("argument list must be a tuple"));
        };
        let obj = it.call(&callable, args.to_vec(), Vec::new())?;
        self.push(obj);
        Ok(())
    }
}
