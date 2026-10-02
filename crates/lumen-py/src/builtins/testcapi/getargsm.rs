#![allow(non_snake_case)]
//! `_testcapi` ports of `PyArg_ParseTuple`, `PyArg_ParseTupleAndKeywords`, `PyArg_Parse` and
//! `Py_BuildValue`: the format-string interpreters of CPython's `Python/getargs.c` and
//! `Python/modsupport.c`, with the same error messages. Output variables are [`Slot`]s: a unit of
//! the format takes the next slot and stores the Python value of what C would have stored.

use super::unicodem::utf8_bytes;
use super::{call_builtin, int_value, system_error};
use crate::builtins::memview::{self, Exported};
use crate::bind::{index, KwArgs};
use crate::object::*;
use crate::pyint::BigInt;
use crate::vm::Interp;

const CONV_UNICODE: &str = "(unicode conversion error)";
const SIZE_CLEAN: &str = "PY_SSIZE_T_CLEAN macro must be defined for '#' formats";

/// What one format unit reads and writes.
#[derive(Default, Clone)]
pub(super) struct Slot {
    pub encoding: Option<String>,
    pub buffer: Option<Value>,
    pub pytype: Option<Value>,
    pub converter: Option<Value>,
    pub out: Option<Value>,
}

enum Fail {
    Exc(Obj),
    Msg(String),
}

impl From<Obj> for Fail {
    fn from(e: Obj) -> Fail {
        Fail::Exc(e)
    }
}

type Conv<T> = Result<T, Fail>;

struct Fmt<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Fmt<'a> {
    fn new(s: &'a str) -> Fmt<'a> {
        Fmt { s: s.as_bytes(), i: 0 }
    }

    fn peek(&self) -> u8 {
        self.s.get(self.i).copied().unwrap_or(0)
    }

    fn bump(&mut self) -> u8 {
        let c = self.peek();
        self.i += 1;
        c
    }

    fn rest(&self) -> String {
        String::from_utf8_lossy(self.s.get(self.i..).unwrap_or(&[])).into_owned()
    }

    fn end(&self) -> bool {
        matches!(self.peek(), 0 | b'|' | b'$' | b':' | b';')
    }
}

struct State<'s> {
    slots: &'s mut Vec<Slot>,
    next: usize,
    size_clean: bool,
}

impl State<'_> {
    fn slot(&mut self) -> &mut Slot {
        if self.next >= self.slots.len() {
            self.slots.resize(self.next + 1, Slot::default());
        }
        self.next += 1;
        &mut self.slots[self.next - 1]
    }
}

fn converterr(it: &mut Interp, expected: &str, arg: &Value) -> Fail {
    if expected.starts_with('(') {
        return Fail::Msg(expected.to_string());
    }
    let t = if arg.is_none() { "None".to_string() } else { it.tp_name_of(arg) };
    Fail::Msg(format!("must be {expected}, not {t}"))
}

fn is_bytes(v: &Value) -> bool {
    matches!(v, Value::Obj(o) if matches!(o.kind, Kind::Bytes(_)))
}

fn is_bytearray(v: &Value) -> bool {
    matches!(v, Value::Obj(o) if matches!(o.kind, Kind::ByteArray(_)))
}

fn float_argument_error(it: &mut Interp, arg: &Value) -> Conv<()> {
    let is_float = matches!(arg, Value::Float(_)) || matches!(arg, Value::Obj(o) if matches!(o.kind, Kind::Float(_)));
    if is_float {
        return Err(Fail::Exc(it.type_error("integer argument expected, got float")));
    }
    Ok(())
}

pub(super) fn as_integer(it: &mut Interp, arg: &Value) -> R<BigInt> {
    let i = index(it, arg)?;
    Ok(i.as_bigint().unwrap_or_else(BigInt::zero))
}

pub(super) fn long_like(it: &mut Interp, arg: &Value, min: i128, max: i128, c_type: &str) -> R<i128> {
    let b = as_integer(it, arg)?;
    match b.to_i128() {
        Some(n) if n >= min && n <= max => Ok(n),
        _ => Err(it.overflow_err(&format!("Python int too large to convert to C {c_type}"))),
    }
}

fn long_signed(it: &mut Interp, arg: &Value) -> R<i128> {
    long_like(it, arg, i64::MIN as i128, i64::MAX as i128, "long")
}

/// `PyLong_AsUnsignedLongMask` and friends: wrap an integer (`__index__` accepted) to 64 bits.
fn unsigned_mask(it: &mut Interp, arg: &Value) -> R<u64> {
    let b = as_integer(it, arg)?;
    Ok(b.to_i128_wrapping() as u64)
}

fn utf8_of(it: &mut Interp, s: &str) -> Conv<Vec<u8>> {
    Ok(utf8_bytes(it, s)?)
}

fn read_only_buffer(it: &mut Interp, arg: &Value) -> Conv<Vec<u8>> {
    if let Value::Obj(o) = arg {
        if let Kind::Bytes(b) = &o.kind {
            return Ok(b.clone());
        }
    }
    match memview::export(it, arg)? {
        Some(_) => Err(Fail::Msg("read-only bytes-like object".into())),
        None => Err(Fail::Msg("bytes-like object".into())),
    }
}

fn simple_buffer(it: &mut Interp, arg: &Value) -> Conv<Vec<u8>> {
    let Some(e) = memview::export(it, arg)? else {
        return Err(Fail::Msg("bytes-like object".into()));
    };
    if !e.view.is_c_contiguous() {
        return Err(Fail::Exc(it.new_exc_str("BufferError", "memoryview: underlying buffer is not C-contiguous")));
    }
    Ok(memview::contiguous_bytes(it, arg)?.unwrap_or_default())
}

fn writable_buffer(it: &mut Interp, arg: &Value) -> Conv<Exported> {
    let e = match memview::export(it, arg) {
        Ok(Some(e)) => e,
        _ => return Err(Fail::Msg("read-write bytes-like object".into())),
    };
    if e.view.readonly || !e.view.is_c_contiguous() {
        return Err(Fail::Msg("read-write bytes-like object".into()));
    }
    Ok(e)
}

fn warn_deprecated_format(it: &mut Interp, c: u8) -> Conv<()> {
    let msg = format!("getargs: The '{}' format is deprecated. Use 'U' instead.", c as char);
    crate::builtins::warningsm::warn_category(it, "DeprecationWarning", &msg, 1)?;
    Ok(())
}

fn encode_slice(it: &mut Interp, buffer: &Value, data: &[u8]) -> R<()> {
    let slice = Value::Obj(Object::new(Kind::Slice(Value::Int(0), Value::Int(data.len() as i64), Value::None)));
    it.call_method(buffer, "__setitem__", vec![slice, Value::bytes(data.to_vec())])?;
    Ok(())
}

fn convertsimple(it: &mut Interp, st: &mut State, arg: &Value, fmt: &mut Fmt) -> Conv<()> {
    let c = fmt.bump();
    let out: Value = match c {
        b'b' => {
            float_argument_error(it, arg)?;
            let n = long_signed(it, arg)?;
            if n < 0 {
                return Err(Fail::Exc(it.overflow_err("unsigned byte integer is less than minimum")));
            }
            if n > i128::from(u8::MAX) {
                return Err(Fail::Exc(it.overflow_err("unsigned byte integer is greater than maximum")));
            }
            int_value(n)
        }
        b'B' => int_value(i128::from(unsigned_mask(it, arg)? as u8)),
        b'h' => {
            float_argument_error(it, arg)?;
            let n = long_signed(it, arg)?;
            if n < i128::from(i16::MIN) {
                return Err(Fail::Exc(it.overflow_err("signed short integer is less than minimum")));
            }
            if n > i128::from(i16::MAX) {
                return Err(Fail::Exc(it.overflow_err("signed short integer is greater than maximum")));
            }
            int_value(n)
        }
        b'H' => int_value(i128::from(unsigned_mask(it, arg)? as u16)),
        b'i' => {
            float_argument_error(it, arg)?;
            let n = long_signed(it, arg)?;
            if n > i128::from(i32::MAX) {
                return Err(Fail::Exc(it.overflow_err("signed integer is greater than maximum")));
            }
            if n < i128::from(i32::MIN) {
                return Err(Fail::Exc(it.overflow_err("signed integer is less than minimum")));
            }
            int_value(n)
        }
        b'I' => int_value(i128::from(unsigned_mask(it, arg)? as u32)),
        b'n' => {
            float_argument_error(it, arg)?;
            int_value(long_like(it, arg, i64::MIN as i128, i64::MAX as i128, "ssize_t")?)
        }
        b'l' => {
            float_argument_error(it, arg)?;
            int_value(long_signed(it, arg)?)
        }
        b'k' => {
            if !arg.is_int_like() {
                return Err(converterr(it, "int", arg));
            }
            int_value(i128::from(unsigned_mask(it, arg)?))
        }
        b'L' => {
            float_argument_error(it, arg)?;
            int_value(long_like(it, arg, i64::MIN as i128, i64::MAX as i128, "long long")?)
        }
        b'K' => {
            if !arg.is_int_like() {
                return Err(converterr(it, "int", arg));
            }
            int_value(i128::from(unsigned_mask(it, arg)?))
        }
        b'f' => Value::Float(it.float_arg(arg)? as f32 as f64),
        b'd' => Value::Float(it.float_arg(arg)?),
        b'D' => {
            let (re, im) = it.complex_arg(arg)?;
            Value::Obj(Object::new(Kind::Complex(re, im)))
        }
        b'c' => {
            let data = if is_bytes(arg) || is_bytearray(arg) { it.bytes_of(arg)? } else { Vec::new() };
            if data.len() != 1 {
                return Err(converterr(it, "a byte string of length 1", arg));
            }
            Value::Int(i64::from(data[0]))
        }
        b'C' => {
            let one = arg.as_str().and_then(|s| {
                let mut cps = lumen_common::smuggle::code_points(s);
                match (cps.next(), cps.next()) {
                    (Some(c), None) => Some(c),
                    _ => None,
                }
            });
            match one {
                Some(c) => Value::Int(i64::from(c)),
                None => return Err(converterr(it, "a unicode character", arg)),
            }
        }
        b'p' => Value::Int(i64::from(it.truthy(arg)?)),
        b'y' => convert_y(it, st, arg, fmt)?,
        b's' | b'z' => convert_s(it, st, c, arg, fmt)?,
        b'u' | b'Z' => convert_u(it, st, c, arg, fmt)?,
        b'e' => convert_e(it, st, arg, fmt)?,
        b'S' => {
            if !is_bytes(arg) {
                return Err(converterr(it, "bytes", arg));
            }
            arg.clone()
        }
        b'Y' => {
            if !is_bytearray(arg) {
                return Err(converterr(it, "bytearray", arg));
            }
            arg.clone()
        }
        b'U' => {
            if arg.as_str().is_none() {
                return Err(converterr(it, "str", arg));
            }
            arg.clone()
        }
        b'O' => {
            if fmt.peek() == b'!' {
                fmt.bump();
                let slot = st.slot();
                let ty = slot.pytype.clone().unwrap_or(Value::None);
                let ok = match &ty {
                    Value::Obj(t) => {
                        let at = it.type_of(arg);
                        it.is_subtype(&at, t)
                    }
                    _ => false,
                };
                if !ok {
                    let name = it.get_attr_str(&ty, "__name__").ok().and_then(|n| n.as_str().map(str::to_string)).unwrap_or_default();
                    return Err(converterr(it, &name, arg));
                }
                slot.out = Some(arg.clone());
                return Ok(());
            }
            if fmt.peek() == b'&' {
                fmt.bump();
                let conv = st.slot().converter.clone().unwrap_or(Value::None);
                let ok = it.call(&conv, vec![arg.clone()], Vec::new())?;
                if !it.truthy(&ok)? {
                    return Err(Fail::Msg("(unspecified)".into()));
                }
                return Ok(());
            }
            arg.clone()
        }
        b'w' => {
            if fmt.peek() != b'*' {
                return Err(Fail::Msg("(invalid use of 'w' format character)".into()));
            }
            fmt.bump();
            writable_buffer(it, arg)?;
            arg.clone()
        }
        _ => return Err(Fail::Msg("(impossible<bad format char>)".into())),
    };
    if c == b'e' {
        let n = st.next - 1;
        st.slots[n].out = Some(out);
        return Ok(());
    }
    st.slot().out = Some(out);
    Ok(())
}

fn convert_y(it: &mut Interp, st: &mut State, arg: &Value, fmt: &mut Fmt) -> Conv<Value> {
    if fmt.peek() == b'*' {
        fmt.bump();
        let data = simple_buffer(it, arg).map_err(|f| match f {
            Fail::Msg(m) => converterr(it, &m, arg),
            e => e,
        })?;
        return Ok(Value::bytes(data));
    }
    let data = read_only_buffer(it, arg).map_err(|f| match f {
        Fail::Msg(m) => converterr(it, &m, arg),
        e => e,
    })?;
    if fmt.peek() == b'#' {
        if !st.size_clean {
            return Err(Fail::Exc(system_error(it, SIZE_CLEAN)));
        }
        fmt.bump();
    } else if data.contains(&0) {
        return Err(Fail::Exc(it.value_error("embedded null byte")));
    }
    Ok(Value::bytes(data))
}

fn convert_s(it: &mut Interp, st: &mut State, c: u8, arg: &Value, fmt: &mut Fmt) -> Conv<Value> {
    match fmt.peek() {
        b'*' => {
            fmt.bump();
            if c == b'z' && arg.is_none() {
                return Ok(Value::None);
            }
            if let Some(s) = arg.as_str() {
                return Ok(Value::bytes(utf8_of(it, s)?));
            }
            let data = simple_buffer(it, arg).map_err(|f| match f {
                Fail::Msg(m) => converterr(it, &m, arg),
                e => e,
            })?;
            Ok(Value::bytes(data))
        }
        b'#' => {
            if !st.size_clean {
                return Err(Fail::Exc(system_error(it, SIZE_CLEAN)));
            }
            fmt.bump();
            if c == b'z' && arg.is_none() {
                return Ok(Value::None);
            }
            if let Some(s) = arg.as_str() {
                return Ok(Value::bytes(utf8_of(it, s)?));
            }
            let data = read_only_buffer(it, arg).map_err(|f| match f {
                Fail::Msg(m) => converterr(it, &m, arg),
                e => e,
            })?;
            Ok(Value::bytes(data))
        }
        _ => {
            if c == b'z' && arg.is_none() {
                return Ok(Value::None);
            }
            let Some(s) = arg.as_str() else {
                return Err(converterr(it, if c == b'z' { "str or None" } else { "str" }, arg));
            };
            let data = utf8_of(it, s)?;
            if data.contains(&0) {
                return Err(Fail::Exc(it.value_error("embedded null character")));
            }
            Ok(Value::bytes(data))
        }
    }
}

fn convert_u(it: &mut Interp, st: &mut State, c: u8, arg: &Value, fmt: &mut Fmt) -> Conv<Value> {
    warn_deprecated_format(it, c)?;
    let hash = fmt.peek() == b'#';
    if hash {
        if !st.size_clean {
            return Err(Fail::Exc(system_error(it, SIZE_CLEAN)));
        }
        fmt.bump();
    }
    if c == b'Z' && arg.is_none() {
        return Ok(Value::None);
    }
    let Some(s) = arg.as_str() else {
        return Err(converterr(it, if c == b'Z' { "str or None" } else { "str" }, arg));
    };
    if !hash && s.contains('\0') {
        return Err(Fail::Exc(it.value_error("embedded null character")));
    }
    Ok(Value::str(s))
}

fn convert_e(it: &mut Interp, st: &mut State, arg: &Value, fmt: &mut Fmt) -> Conv<Value> {
    let slot = st.slot().clone();
    let encoding = slot.encoding.clone().unwrap_or_else(|| "utf-8".to_string());
    let recode_strings = match fmt.peek() {
        b's' => true,
        b't' => false,
        _ => return Err(converterr(it, "(unknown parser marker combination)", arg)),
    };
    fmt.bump();
    let data: Vec<u8> = if !recode_strings && (is_bytes(arg) || is_bytearray(arg)) {
        it.bytes_of(arg)?
    } else if let Some(s) = arg.as_str() {
        let enc = it.call_method(&Value::str(s), "encode", vec![Value::str(&encoding)]);
        match enc {
            Ok(b) => it.bytes_of(&b)?,
            Err(_) => return Err(converterr(it, "(encoding failed)", arg)),
        }
    } else {
        return Err(converterr(it, if recode_strings { "str" } else { "str, bytes or bytearray" }, arg));
    };
    if fmt.peek() == b'#' {
        if !st.size_clean {
            return Err(Fail::Exc(system_error(it, SIZE_CLEAN)));
        }
        fmt.bump();
        if let Some(buffer) = &slot.buffer {
            let capacity = it.call_method(buffer, "__len__", Vec::new())?.as_i64().unwrap_or(0) as usize;
            if data.len() + 1 > capacity {
                let msg = format!("encoded string too long ({}, maximum length {})", data.len(), capacity as i64 - 1);
                return Err(Fail::Exc(it.value_error(&msg)));
            }
            let mut with_nul = data.clone();
            with_nul.push(0);
            encode_slice(it, buffer, &with_nul)?;
        }
        Ok(Value::bytes(data))
    } else {
        if data.contains(&0) {
            return Err(converterr(it, "encoded string without null bytes", arg));
        }
        Ok(Value::bytes(data))
    }
}

fn skipitem(st: &mut State, it: &mut Interp, fmt: &mut Fmt) -> Result<(), SkipFail> {
    let c = fmt.bump();
    match c {
        b'b' | b'B' | b'h' | b'H' | b'i' | b'I' | b'l' | b'k' | b'L' | b'K' | b'n' | b'f' | b'd' | b'D' | b'c' | b'C' | b'p' | b'S' | b'Y'
        | b'U' => {
            st.slot();
        }
        b'e' | b's' | b'z' | b'y' | b'w' => {
            st.slot();
            if c == b'e' {
                if !matches!(fmt.peek(), b's' | b't') {
                    return Err(SkipFail::Msg("impossible<bad format char>".into()));
                }
                fmt.bump();
            }
            match fmt.peek() {
                b'*' => {
                    fmt.bump();
                }
                b'#' => {
                    if !st.size_clean {
                        return Err(SkipFail::Exc(system_error(it, SIZE_CLEAN)));
                    }
                    fmt.bump();
                }
                _ => {}
            }
        }
        b'u' | b'Z' => {
            st.slot();
            if fmt.peek() == b'#' {
                if !st.size_clean {
                    return Err(SkipFail::Exc(system_error(it, SIZE_CLEAN)));
                }
                fmt.bump();
            }
        }
        b'O' => {
            if matches!(fmt.peek(), b'!' | b'&') {
                fmt.bump();
            }
            st.slot();
        }
        b'(' => {
            loop {
                if fmt.peek() == b')' {
                    break;
                }
                if matches!(fmt.peek(), 0 | b':' | b';') {
                    return Err(SkipFail::Msg("Unmatched left paren in format string".into()));
                }
                skipitem(st, it, fmt)?;
            }
            fmt.bump();
        }
        b')' => return Err(SkipFail::Msg("Unmatched right paren in format string".into())),
        _ => return Err(SkipFail::Msg("impossible<bad format char>".into())),
    }
    Ok(())
}

enum SkipFail {
    Msg(String),
    Exc(Obj),
}

fn seterror(it: &mut Interp, iarg: usize, msg: &str, levels: &[i32], fname: Option<&str>, message: Option<&str>) -> Obj {
    let text = match message {
        Some(m) => m.to_string(),
        None => {
            let mut buf = String::new();
            if let Some(f) = fname {
                buf.push_str(&format!("{f}() "));
            }
            if iarg != 0 {
                buf.push_str(&format!("argument {iarg}"));
                for &l in levels.iter().take(32) {
                    if l <= 0 {
                        break;
                    }
                    buf.push_str(&format!(", item {}", l - 1));
                }
            } else {
                buf.push_str("argument");
            }
            buf.push(' ');
            buf.push_str(msg);
            buf
        }
    };
    if msg.starts_with('(') {
        system_error(it, &text)
    } else {
        it.type_error(&text)
    }
}

fn converttuple(it: &mut Interp, st: &mut State, arg: &Value, fmt: &mut Fmt, levels: &mut [i32], toplevel: bool) -> Conv<()> {
    let mut probe = Fmt { s: fmt.s, i: fmt.i };
    if !toplevel {
        probe.bump();
    }
    let mut level = 0;
    let mut n = 0;
    loop {
        let c = probe.bump();
        match c {
            b'(' => {
                if level == 0 {
                    n += 1;
                }
                level += 1;
            }
            b')' => {
                if level == 0 {
                    break;
                }
                level -= 1;
            }
            b':' | b';' | 0 => break,
            c if level == 0 && c.is_ascii_alphabetic() => n += 1,
            _ => {}
        }
    }
    if !toplevel {
        fmt.bump();
    }
    let is_sequence = !is_bytes(arg) && (arg.tuple_items().is_some() || it.get_attr_str(arg, "__getitem__").is_ok());
    if !is_sequence {
        levels[0] = 0;
        let t = if arg.is_none() { "None".to_string() } else { it.tp_name_of(arg) };
        let m = if toplevel { format!("expected {n} arguments, not {t}") } else { format!("must be {n}-item sequence, not {t}") };
        return Err(Fail::Msg(m));
    }
    let len = it.len_of(arg)?;
    if len != n as usize {
        levels[0] = 0;
        let m = if toplevel { format!("expected {n} arguments, not {len}") } else { format!("must be sequence of length {n}, not {len}") };
        return Err(Fail::Msg(m));
    }
    for i in 0..n {
        let item = match it.getitem(arg, &Value::Int(i64::from(i))) {
            Ok(v) => v,
            Err(_) => {
                levels[0] = i + 1;
                levels[1] = 0;
                return Err(Fail::Msg("is not retrievable".into()));
            }
        };
        if let Err(f) = convertitem(it, st, &item, fmt, &mut levels[1..]) {
            levels[0] = i + 1;
            return Err(f);
        }
    }
    Ok(())
}

fn convertitem(it: &mut Interp, st: &mut State, arg: &Value, fmt: &mut Fmt, levels: &mut [i32]) -> Conv<()> {
    if fmt.peek() == b'(' {
        converttuple(it, st, arg, fmt, levels, false)?;
        fmt.bump();
        return Ok(());
    }
    let r = convertsimple(it, st, arg, fmt);
    if r.is_err() {
        levels[0] = 0;
    }
    r
}

fn fail_to_exc(it: &mut Interp, f: Fail, iarg: usize, levels: &[i32], fname: Option<&str>, message: Option<&str>) -> Obj {
    match f {
        Fail::Exc(e) => e,
        Fail::Msg(m) => seterror(it, iarg, &m, levels, fname, message),
    }
}

fn split_name(format: &str) -> (&str, Option<&str>, Option<&str>) {
    if let Some(i) = format.find(':') {
        return (&format[..i], Some(&format[i + 1..]), None);
    }
    if let Some(i) = format.find(';') {
        return (&format[..i], None, Some(&format[i + 1..]));
    }
    (format, None, None)
}

fn function_name(fname: Option<&str>) -> (String, &'static str) {
    match fname {
        Some(f) => (f.to_string(), "()"),
        None => ("function".to_string(), ""),
    }
}

fn plural(n: i64) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// `PyArg_ParseTuple`.
pub(super) fn parse_tuple(it: &mut Interp, args: &[Value], format: &str, slots: &mut Vec<Slot>, size_clean: bool) -> R<()> {
    let (units, fname, message) = split_name(format);
    let mut min: i64 = -1;
    let mut max: i64 = 0;
    let mut level = 0;
    for c in units.bytes() {
        match c {
            b'(' => {
                if level == 0 {
                    max += 1;
                }
                level += 1;
            }
            b')' => {
                if level == 0 {
                    return Err(system_error(it, "excess ')' in getargs format"));
                }
                level -= 1;
            }
            _ if level != 0 => {}
            c if c.is_ascii_alphabetic() => {
                if c != b'e' {
                    max += 1;
                }
            }
            b'|' => min = max,
            _ => {}
        }
    }
    if level != 0 {
        return Err(system_error(it, "missing ')' in getargs format"));
    }
    if min < 0 {
        min = max;
    }
    let nargs = args.len() as i64;
    if nargs < min || max < nargs {
        let text = match message {
            Some(m) => m.to_string(),
            None => {
                let (name, paren) = function_name(fname);
                let which = if min == max { "exactly" } else if nargs < min { "at least" } else { "at most" };
                let count = if nargs < min { min } else { max };
                format!("{name}{paren} takes {which} {count} argument{} ({nargs} given)", plural(count))
            }
        };
        return Err(it.type_error(&text));
    }
    let mut st = State { slots, next: 0, size_clean };
    let mut fmt = Fmt::new(units);
    let mut levels = [0i32; 32];
    for (i, arg) in args.iter().enumerate() {
        if fmt.peek() == b'|' {
            fmt.bump();
        }
        if let Err(f) = convertitem(it, &mut st, arg, &mut fmt, &mut levels) {
            return Err(fail_to_exc(it, f, i + 1, &levels, fname, message));
        }
    }
    if fmt.peek() == b'|' {
        fmt.bump();
    }
    if !fmt.end() && fmt.peek() != b'(' && !fmt.peek().is_ascii_alphabetic() {
        return Err(system_error(it, &format!("bad format string: {units}")));
    }
    Ok(())
}

/// `PyArg_Parse`: one argument that is not a tuple.
fn parse_single(it: &mut Interp, arg: &Value, format: &str, slots: &mut Vec<Slot>) -> R<()> {
    let (units, fname, message) = split_name(format);
    let mut st = State { slots, next: 0, size_clean: true };
    let mut fmt = Fmt::new(units);
    let mut levels = [0i32; 32];
    if let Err(f) = convertitem(it, &mut st, arg, &mut fmt, &mut levels) {
        let l0 = levels[0].max(0) as usize;
        return Err(fail_to_exc(it, f, l0, &levels[1..], fname, message));
    }
    Ok(())
}

/// `PyArg_ParseTupleAndKeywords`; a keyword name is one of `kwlist`, `""` for positional-only.
pub(super) fn parse_tuple_and_keywords(
    it: &mut Interp,
    args: &[Value],
    kwargs: Option<&Obj>,
    format: &str,
    kwlist: &[String],
    slots: &mut Vec<Slot>,
    size_clean: bool,
) -> R<()> {
    let (units, fname, custom) = split_name(format);
    let (name, paren) = function_name(fname);
    let pos = kwlist.iter().take_while(|k| k.is_empty()).count();
    for k in &kwlist[pos..] {
        if k.is_empty() {
            return Err(system_error(it, "Empty keyword parameter name"));
        }
    }
    let len = kwlist.len() as i64;
    let nargs = args.len() as i64;
    let keys = match kwargs {
        Some(d) => it.iterate_to_vec(&Value::Obj(d.clone()))?,
        None => Vec::new(),
    };
    let mut nkwargs = keys.len() as i64;
    if nargs + nkwargs > len {
        let msg = format!(
            "{name}{paren} takes at most {len} {}argument{} ({} given)",
            if nargs == 0 { "keyword " } else { "" },
            plural(len),
            nargs + nkwargs
        );
        return Err(it.type_error(&msg));
    }
    let mut st = State { slots, next: 0, size_clean };
    let mut fmt = Fmt::new(units);
    let mut levels = [0i32; 32];
    let mut min = i64::MAX;
    let mut max = i64::MAX;
    let mut skip = false;
    let mut i: i64 = 0;
    while i < len {
        if fmt.peek() == b'|' {
            if min != i64::MAX {
                return Err(system_error(it, "Invalid format string (| specified twice)"));
            }
            min = i;
            fmt.bump();
            if max != i64::MAX {
                return Err(system_error(it, "Invalid format string ($ before |)"));
            }
        }
        if fmt.peek() == b'$' {
            if max != i64::MAX {
                return Err(system_error(it, "Invalid format string ($ specified twice)"));
            }
            max = i;
            fmt.bump();
            if max < pos as i64 {
                return Err(system_error(it, "Empty parameter name after $"));
            }
            if skip {
                break;
            }
            if max < nargs {
                let msg = if max == 0 {
                    format!("{name}{paren} takes no positional arguments")
                } else {
                    format!(
                        "{name}{paren} takes {} {max} positional argument{} ({nargs} given)",
                        if min < max { "at most" } else { "exactly" },
                        plural(max)
                    )
                };
                return Err(it.type_error(&msg));
            }
        }
        if fmt.end() {
            return Err(system_error(it, &format!("More keyword list entries ({len}) than format specifiers ({i})")));
        }
        if !skip {
            let current = if i < nargs {
                Some(args[i as usize].clone())
            } else if nkwargs > 0 && i >= pos as i64 {
                let found = match kwargs {
                    Some(d) => it.dict_get(d, &Value::str(&kwlist[i as usize]))?,
                    None => None,
                };
                if found.is_some() {
                    nkwargs -= 1;
                }
                found
            } else {
                None
            };
            if let Some(arg) = current {
                if let Err(f) = convertitem(it, &mut st, &arg, &mut fmt, &mut levels) {
                    return Err(fail_to_exc(it, f, i as usize + 1, &levels, fname, custom));
                }
                i += 1;
                continue;
            }
            if i < min {
                if i < pos as i64 {
                    skip = true;
                } else {
                    let msg = format!("{name}{paren} missing required argument '{}' (pos {})", kwlist[i as usize], i + 1);
                    return Err(it.type_error(&msg));
                }
            }
            if nkwargs == 0 && !skip {
                return Ok(());
            }
        }
        if let Err(f) = skipitem(&mut st, it, &mut fmt) {
            return Err(match f {
                SkipFail::Exc(e) => e,
                SkipFail::Msg(m) => system_error(it, &format!("{m}: '{}'", fmt.rest())),
            });
        }
        i += 1;
    }
    if skip {
        let least = (pos as i64).min(min);
        let msg = format!(
            "{name}{paren} takes {} {least} positional argument{} ({nargs} given)",
            if least < i { "at least" } else { "exactly" },
            plural(least)
        );
        return Err(it.type_error(&msg));
    }
    if !fmt.end() {
        return Err(system_error(it, &format!("more argument specifiers than keyword list entries (remaining format:'{}')", fmt.rest())));
    }
    if nkwargs > 0 {
        let d = kwargs.expect("keywords are left");
        for i in pos as i64..nargs {
            if it.dict_get(d, &Value::str(&kwlist[i as usize]))?.is_some() {
                let msg = format!("argument for {name}{paren} given by name ('{}') and position ({})", kwlist[i as usize], i + 1);
                return Err(it.type_error(&msg));
            }
        }
        for key in &keys {
            let Some(text) = key.as_str() else {
                return Err(it.type_error("keywords must be strings"));
            };
            let exact = matches!(key, Value::Obj(o) if o.cls.is_none());
            let matched = exact && kwlist[pos..].iter().any(|k| k == text);
            if !matched {
                let target = match fname {
                    Some(f) => format!("{f}()"),
                    None => "this function".to_string(),
                };
                return Err(it.type_error(&format!("'{text}' is an invalid keyword argument for {target}")));
            }
        }
    }
    Ok(())
}

fn out_of(slots: &[Slot], i: usize) -> Value {
    slots.get(i).and_then(|s| s.out.clone()).unwrap_or(Value::None)
}

fn ints_or_default(slots: &[Slot], n: usize, default: i64) -> Value {
    let items = (0..n).map(|i| slots.get(i).and_then(|s| s.out.clone()).unwrap_or(Value::Int(default))).collect();
    Value::tuple(items)
}

fn single(it: &mut Interp, args: &[Value], format: &str) -> R<Value> {
    let mut slots = Vec::new();
    parse_tuple(it, args, format, &mut slots, true)?;
    Ok(out_of(&slots, 0))
}

fn keyword_names(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| (*s).to_string()).collect()
}

fn kwargs_dict(it: &mut Interp, kw: &KwArgs) -> R<Option<Obj>> {
    let pairs = kw.to_vec();
    if pairs.is_empty() {
        return Ok(None);
    }
    Ok(Some(it.kwargs_to_dict(&pairs)?))
}

fn keywords_call(it: &mut Interp, args: &[Value], kw: &KwArgs, format: &str, names: &[&str], slots: &mut Vec<Slot>, size_clean: bool) -> R<()> {
    let d = kwargs_dict(it, kw)?;
    parse_tuple_and_keywords(it, args, d.as_ref(), format, &keyword_names(names), slots, size_clean)
}

fn c_string_len(data: &[u8]) -> usize {
    data.iter().position(|&b| b == 0).unwrap_or(data.len())
}

#[derive(Clone)]
enum Built {
    Obj(Option<Value>),
    Uint(u32),
}

struct Build<'a> {
    fmt: &'a [u8],
    i: usize,
    args: &'a [Built],
    next: usize,
}

impl Build<'_> {
    fn peek(&self) -> u8 {
        self.fmt.get(self.i).copied().unwrap_or(0)
    }

    fn take(&mut self) -> Built {
        let a = self.args.get(self.next).cloned().unwrap_or(Built::Obj(None));
        self.next += 1;
        a
    }
}

fn count_format(it: &mut Interp, fmt: &[u8], mut i: usize, end: u8) -> R<i64> {
    let mut count = 0;
    let mut level = 0;
    loop {
        let c = fmt.get(i).copied().unwrap_or(0);
        if level == 0 && c == end {
            return Ok(count);
        }
        match c {
            0 => return Err(system_error(it, "unmatched paren in format")),
            b'(' | b'[' | b'{' => {
                if level == 0 {
                    count += 1;
                }
                level += 1;
            }
            b')' | b']' | b'}' => level -= 1,
            b'#' | b'&' | b',' | b':' | b' ' | b'\t' => {}
            _ => {
                if level == 0 {
                    count += 1;
                }
            }
        }
        i += 1;
    }
}

fn build_items(it: &mut Interp, b: &mut Build, end: u8, n: i64) -> R<Vec<Value>> {
    let mut items = Vec::new();
    let mut failure = None;
    for _ in 0..n {
        match build_value(it, b) {
            Ok(v) => items.push(v),
            Err(e) => {
                if failure.is_none() {
                    failure = Some(e);
                }
            }
        }
    }
    if let Some(e) = failure {
        return Err(e);
    }
    if b.peek() != end {
        return Err(system_error(it, "Unmatched paren in format"));
    }
    if end != 0 {
        b.i += 1;
    }
    Ok(items)
}

fn build_value(it: &mut Interp, b: &mut Build) -> R<Value> {
    loop {
        let c = b.peek();
        b.i += 1;
        match c {
            b'(' => {
                let n = count_format(it, b.fmt, b.i, b')')?;
                return Ok(Value::tuple(build_items(it, b, b')', n)?));
            }
            b'[' => {
                let n = count_format(it, b.fmt, b.i, b']')?;
                return Ok(Value::list(build_items(it, b, b']', n)?));
            }
            b'{' => {
                let n = count_format(it, b.fmt, b.i, b'}')?;
                if n % 2 != 0 {
                    return Err(system_error(it, "Bad dict format"));
                }
                let items = build_items(it, b, b'}', n)?;
                let d = it.new_dict();
                for pair in items.chunks(2) {
                    it.dict_set(&d, pair[0].clone(), pair[1].clone())?;
                }
                return Ok(Value::Obj(d));
            }
            b'b' | b'B' | b'h' | b'i' => {
                return Ok(match b.take() {
                    Built::Uint(u) => Value::Int(i64::from(u as i32)),
                    Built::Obj(_) => Value::None,
                });
            }
            b'H' | b'I' => {
                return Ok(match b.take() {
                    Built::Uint(u) => Value::Int(i64::from(u)),
                    Built::Obj(_) => Value::None,
                });
            }
            b'c' => {
                let byte = match b.take() {
                    Built::Uint(u) => u as u8,
                    Built::Obj(_) => 0,
                };
                return Ok(Value::bytes(vec![byte]));
            }
            b'C' => {
                let n = match b.take() {
                    Built::Uint(u) => i64::from(u as i32),
                    Built::Obj(_) => 0,
                };
                if !(0..=0x10ffff).contains(&n) {
                    return Err(it.value_error("%c arg not in range(0x110000)"));
                }
                return call_builtin(it, "chr", vec![Value::Int(n)]);
            }
            b'N' | b'S' | b'O' => {
                return match b.take() {
                    Built::Obj(Some(v)) => Ok(v),
                    Built::Obj(None) => Err(system_error(it, "NULL object passed to Py_BuildValue")),
                    Built::Uint(_) => Err(system_error(it, "NULL object passed to Py_BuildValue")),
                };
            }
            b':' | b',' | b' ' | b'\t' => {}
            _ => return Err(system_error(it, "bad format char passed to Py_BuildValue")),
        }
    }
}

fn py_build_value(it: &mut Interp, fmt: &str, args: &[Built]) -> R<Value> {
    let mut b = Build { fmt: fmt.as_bytes(), i: 0, args, next: 0 };
    let n = count_format(it, b.fmt, 0, 0)?;
    let items = build_items(it, &mut b, 0, n)?;
    Ok(match items.len() {
        0 => Value::None,
        1 => items.into_iter().next().unwrap_or(Value::None),
        _ => Value::tuple(items),
    })
}

#[lumen_bind::module(name = "_testcapi")]
pub mod getargsm {
    use super::*;

    #[op]
    fn get_args(#[varargs] args: &[Value]) -> Value {
        Value::tuple(args.to_vec())
    }

    #[op]
    fn get_kwargs(it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        let _ = args;
        match kwargs_dict(it, &kw)? {
            Some(d) => Ok(Value::Obj(d)),
            None => Ok(Value::None),
        }
    }

    #[op]
    fn parse_tuple_and_keywords(it: &mut Interp, sub_args: &Value, sub_kwargs: &Value, sub_format: &str, sub_keywords: &Value) -> R<Value> {
        let is_exact = |v: &Value| matches!(v, Value::Obj(o) if o.cls.is_none() && matches!(o.kind, Kind::List(_) | Kind::Tuple(_)));
        if !is_exact(sub_keywords) {
            return Err(it.value_error("parse_tuple_and_keywords: sub_keywords must be either list or tuple"));
        }
        let items = it.iterate_to_vec(sub_keywords)?;
        if items.len() > 8 {
            return Err(it.value_error("parse_tuple_and_keywords: too many keywords in sub_keywords"));
        }
        let mut names = Vec::new();
        for o in &items {
            if let Some(s) = o.as_str() {
                names.push(String::from_utf8_lossy(&utf8_bytes(it, s)?).into_owned());
            } else if let Value::Obj(b) = o {
                match &b.kind {
                    Kind::Bytes(data) => names.push(String::from_utf8_lossy(&data[..c_string_len(data)]).into_owned()),
                    _ => return Err(it.value_error("parse_tuple_and_keywords: keywords must be str or bytes")),
                }
            } else {
                return Err(it.value_error("parse_tuple_and_keywords: keywords must be str or bytes"));
            }
        }
        let Some(positional) = sub_args.tuple_items() else {
            return Err(system_error(it, "bad argument to internal function"));
        };
        let kwargs = match sub_kwargs {
            Value::Obj(o) if matches!(o.kind, Kind::Dict(_)) => Some(o.clone()),
            _ => None,
        };
        let mut slots = vec![Slot { encoding: Some(String::new()), ..Slot::default() }; 8];
        parse_tuple_and_keywords(it, positional, kwargs.as_ref(), sub_format, &names, &mut slots, true)?;
        let mut count = 0;
        for c in sub_format.bytes() {
            if c.is_ascii_alphanumeric() {
                if !b"OSUY".contains(&c) {
                    return Ok(Value::None);
                }
                count += 1;
            }
        }
        let outs = (0..count).map(|i| out_of(&slots, i)).collect();
        Ok(Value::tuple(outs))
    }

    #[op]
    fn getargs_w_star(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        let obj = single(it, args, "w*:getargs_w_star")?;
        let e = match memview::export(it, &obj)? {
            Some(e) => e,
            None => return Err(system_error(it, "bad argument to internal function")),
        };
        let len = e.view.nbytes();
        let off = e.view.offset;
        if len >= 2 {
            let _ = e.src.with_mut(|b| {
                b[off] = b'[';
                b[off + len - 1] = b']';
            });
        }
        let data = memview::contiguous_bytes(it, &obj)?.unwrap_or_default();
        Ok(Value::bytes(data))
    }

    #[op]
    fn test_empty_argparse(it: &mut Interp) -> R<()> {
        let mut slots = Vec::new();
        parse_tuple(it, &[], "|:test_empty_argparse", &mut slots, true)?;
        let d = it.new_dict();
        parse_tuple_and_keywords(it, &[], Some(&d), "|:test_empty_argparse", &[], &mut slots, true)
    }

    #[op]
    fn getargs_tuple(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        let mut slots = Vec::new();
        parse_tuple(it, args, "i(ii)", &mut slots, true)?;
        Ok(ints_or_default(&slots, 3, -1))
    }

    #[op]
    fn getargs_keywords(it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        let mut slots = Vec::new();
        keywords_call(it, args, &kw, "(ii)i|(i(ii))(iii)i", &["arg1", "arg2", "arg3", "arg4", "arg5"], &mut slots, true)?;
        Ok(ints_or_default(&slots, 10, -1))
    }

    #[op]
    fn getargs_keyword_only(it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        let mut slots = Vec::new();
        keywords_call(it, args, &kw, "i|i$i", &["required", "optional", "keyword_only"], &mut slots, true)?;
        Ok(ints_or_default(&slots, 3, -1))
    }

    #[op]
    fn getargs_positional_only_and_keywords(it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        let mut slots = Vec::new();
        keywords_call(it, args, &kw, "i|ii", &["", "", "keyword"], &mut slots, true)?;
        Ok(ints_or_default(&slots, 3, -1))
    }

    #[op]
    fn getargs_s_hash_int(it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<()> {
        let mut slots = Vec::new();
        keywords_call(it, args, &kw, "w*|s#i", &["", "", "x"], &mut slots, false)
    }

    #[op]
    fn getargs_s_hash_int2(it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<()> {
        let mut slots = Vec::new();
        keywords_call(it, args, &kw, "w*|(s#)i", &["", "", "x"], &mut slots, false)
    }

    #[op]
    fn getargs_b(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "b")
    }

    #[op]
    fn getargs_B(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "B")
    }

    #[op]
    fn getargs_h(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "h")
    }

    #[op]
    fn getargs_H(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "H")
    }

    #[op]
    fn getargs_I(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "I")
    }

    #[op]
    fn getargs_k(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "k")
    }

    #[op]
    fn getargs_i(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "i")
    }

    #[op]
    fn getargs_l(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "l")
    }

    #[op]
    fn getargs_n(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "n")
    }

    #[op]
    fn getargs_p(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "p")
    }

    #[op]
    fn getargs_L(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "L")
    }

    #[op]
    fn getargs_K(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "K")
    }

    #[op]
    fn getargs_f(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "f")
    }

    #[op]
    fn getargs_d(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "d")
    }

    #[op]
    fn getargs_D(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "D")
    }

    #[op]
    fn getargs_S(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "S")
    }

    #[op]
    fn getargs_Y(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "Y")
    }

    #[op]
    fn getargs_U(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "U")
    }

    #[op]
    fn getargs_c(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "c")
    }

    #[op]
    fn getargs_C(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "C")
    }

    #[op]
    fn getargs_s(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "s")
    }

    #[op]
    fn getargs_s_star(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "s*")
    }

    #[op]
    fn getargs_s_hash(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "s#")
    }

    #[op]
    fn getargs_z(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "z")
    }

    #[op]
    fn getargs_z_star(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "z*")
    }

    #[op]
    fn getargs_z_hash(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "z#")
    }

    #[op]
    fn getargs_y(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "y")
    }

    #[op]
    fn getargs_y_star(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "y*")
    }

    #[op]
    fn getargs_y_hash(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "y#")
    }

    #[op]
    fn getargs_u(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "u")
    }

    #[op]
    fn getargs_u_hash(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "u#")
    }

    #[op]
    fn getargs_Z(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "Z")
    }

    #[op]
    fn getargs_Z_hash(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        single(it, args, "Z#")
    }

    fn encoded(it: &mut Interp, args: &[Value], marker: &str, hash: bool) -> R<Value> {
        let mut head = Vec::new();
        parse_tuple(it, args, if hash { "O|sY" } else { "O|s" }, &mut head, true)?;
        let arg = out_of(&head, 0);
        let encoding = head.get(1).and_then(|s| s.out.as_ref()).and_then(|v| v.as_str()).map(|s| String::from_utf8_lossy(&utf8_bytes_lossy(s)).into_owned());
        let buffer = head.get(2).and_then(|s| s.out.clone());
        let format = format!("e{marker}{}", if hash { "#" } else { "" });
        let mut slots = vec![Slot { encoding, buffer, ..Slot::default() }];
        parse_single(it, &arg, &format, &mut slots)?;
        Ok(out_of(&slots, 0))
    }

    #[op]
    fn getargs_es(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        encoded(it, args, "s", false)
    }

    #[op]
    fn getargs_et(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        encoded(it, args, "t", false)
    }

    #[op]
    fn getargs_es_hash(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        encoded(it, args, "s", true)
    }

    #[op]
    fn getargs_et_hash(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        encoded(it, args, "t", true)
    }

    #[op]
    fn gh_99240_clear_args(it: &mut Interp, #[varargs] args: &[Value]) -> R<()> {
        let mut slots = vec![
            Slot { encoding: Some("idna".into()), ..Slot::default() },
            Slot { encoding: Some("idna".into()), ..Slot::default() },
        ];
        parse_tuple(it, args, "eses", &mut slots, true)
    }

    #[op]
    fn test_L_code(it: &mut Interp) -> R<()> {
        let mut slots = Vec::new();
        parse_tuple(it, &[Value::Int(42)], "L:test_L_code", &mut slots, true)?;
        if out_of(&slots, 0).as_i64() != Some(42) {
            return Err(it.new_exc_str("AssertionError", "test_L_code: L code returned wrong value for long 42"));
        }
        let big = call_builtin(it, "int", vec![Value::str("-FFFFFFFF000000000000000042"), Value::Int(16)])?;
        let mut slots = Vec::new();
        parse_tuple(it, &[big], "L:test_L_code", &mut slots, true)?;
        Ok(())
    }

    #[op]
    fn test_k_code(it: &mut Interp) -> R<()> {
        let num = call_builtin(it, "int", vec![Value::str("FFFFFFFF000000000000000042"), Value::Int(16)])?;
        let mut slots = Vec::new();
        parse_tuple(it, &[num], "k:test_k_code", &mut slots, true)?;
        if out_of(&slots, 0).as_i64() != Some(0x42) {
            return Err(it.new_exc_str("AssertionError", "test_k_code: k code returned wrong value for long 0xFFF...FFF"));
        }
        let num = call_builtin(it, "int", vec![Value::str("-FFFFFFFF000000000000000042"), Value::Int(16)])?;
        let mut slots = Vec::new();
        parse_tuple(it, &[num], "k:test_k_code", &mut slots, true)?;
        let expected = int_value(i128::from((-0x42i64) as u64));
        if !it.values_eq(&out_of(&slots, 0), &expected)? {
            return Err(it.new_exc_str("AssertionError", "test_k_code: k code returned wrong value for long -0xFFF..000042"));
        }
        Ok(())
    }

    #[op]
    fn test_s_code(it: &mut Interp) -> R<()> {
        let obj = Value::str("t\u{ea}te");
        let mut slots = Vec::new();
        parse_tuple(it, std::slice::from_ref(&obj), "s:test_s_code1", &mut slots, true)?;
        parse_tuple(it, std::slice::from_ref(&obj), "z:test_s_code2", &mut slots, true)
    }

    #[op]
    fn argparsing(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        if args.len() != 2 {
            let mut slots = Vec::new();
            parse_tuple(it, args, "O&O&", &mut slots, true)?;
        }
        let os = it.import_module("os")?;
        let fsencode = it.get_attr_str(&Value::Obj(os), "fsencode")?;
        it.call(&fsencode, vec![args[0].clone()], Vec::new())?;
        Ok(Value::Int(1))
    }

    #[op]
    fn py_buildvalue(it: &mut Interp, fmt: &str, #[varargs] objs: &[Value]) -> R<Value> {
        let built: Vec<Built> = (0..10)
            .map(|i| match objs.get(i) {
                Some(v) if !v.is_none() => Built::Obj(Some(v.clone())),
                _ => Built::Obj(None),
            })
            .collect();
        if objs.len() > 10 {
            return Err(it.type_error("function takes at most 11 arguments"));
        }
        py_build_value(it, fmt, &built)
    }

    #[op]
    fn py_buildvalue_ints(it: &mut Interp, fmt: &str, #[varargs] values: &[Value]) -> R<Value> {
        if values.len() > 10 {
            return Err(it.type_error("function takes at most 11 arguments"));
        }
        let mut built = Vec::new();
        for v in values {
            built.push(Built::Uint(unsigned_mask(it, v)? as u32));
        }
        built.resize(10, Built::Uint(0));
        py_build_value(it, fmt, &built)
    }

    #[op]
    fn test_buildvalue_N() {}
}

fn utf8_bytes_lossy(s: &str) -> Vec<u8> {
    lumen_common::smuggle::unescape_text(s).into_owned().into_bytes()
}
