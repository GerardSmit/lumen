//! `_struct`: format-string driven packing and unpacking of C-like binary data, on the shared
//! element table and codecs of `lumen_common::buffer` (`struct_code`, `load`, `store_*`).

/// Functions to convert between Python values and C structs.
/// Python bytes objects are used to hold the data representing the C struct
/// and also as format strings (explained below) to describe the layout of data
/// in the C struct.
///
/// The optional first format char indicates byte order, size and alignment:
///   @: native order, size & alignment (default)
///   =: native order, std. size & alignment
///   <: little-endian, std. size & alignment
///   >: big-endian, std. size & alignment
///   !: same as >
///
/// The remaining chars indicate types of args and must match exactly;
/// these can be preceded by a decimal repeat count:
///   x: pad byte (no data); c:char; b:signed byte; B:unsigned byte;
///   ?: _Bool (requires C99; if not available, char is used instead)
///   h:short; H:unsigned short; i:int; I:unsigned int;
///   l:long; L:unsigned long; f:float; d:double; e:half-float.
/// Special cases (preceding decimal count indicates length):
///   s:string (array of char); p: pascal string (with count byte).
/// Special cases (only available in native format):
///   n:ssize_t; N:size_t;
///   P:an integer type that is wide enough to hold a pointer.
/// Special case (not in native mode unless 'long long' in platform C):
///   q:long long; Q:unsigned long long
/// Whitespace between formats is ignored.
///
/// The variable struct.error is an exception raised on errors.
#[lumen_bind::module(name = "_struct")]
pub mod _struct {
    use super::super::memview::{self, index_i128, scalar_value, Source};
    use super::super::native::new_type;
    use crate::bind::{buffer_error, Py, This};
    use crate::object::*;
    use crate::vm::{dict_get_str, dict_set_str, Interp};
    use lumen_common::buffer::{load, store_f64, store_float_checked, store_int_checked, store_int_wrapping, struct_code};
    use lumen_common::buffer::{ElemKind, PackError, StructMode, ViewDesc};
    use std::cell::RefCell;
    use lumen_common::fasthash::FastMap;
    use std::rc::Rc;

    const MAX_CACHE: usize = 100;

    #[derive(Clone, Copy)]
    struct Code {
        ch: u8,
        /// `None` for `s` and `p`.
        kind: Option<ElemKind>,
        offset: usize,
        size: usize,
        repeat: usize,
    }

    pub struct Fmt {
        text: String,
        codes: Vec<Code>,
        size: usize,
        nvalues: usize,
        mode: StructMode,
        uninit: bool,
    }

    impl Fmt {
        /// The format of a `Struct` created by `__new__` without `__init__`: CPython reports its
        /// size and item count as -1.
        fn uninitialized() -> Fmt {
            Fmt { text: String::new(), codes: Vec::new(), size: 0, nvalues: 0, mode: StructMode::Native, uninit: true }
        }
    }

    fn shown(f: &Fmt, n: usize) -> i64 {
        if f.uninit { -1 } else { n as i64 }
    }

    thread_local! {
        static CACHE: RefCell<FastMap<Vec<u8>, Rc<Fmt>>> = RefCell::new(FastMap::default());
    }

    /// The format cache, moved out of this thread's slot when the thread gives up the GIL (its
    /// `Rc`s are not atomic, so they must follow the interpreter, not the OS thread).
    pub(crate) fn tls_take() -> Box<dyn std::any::Any> {
        Box::new(CACHE.with(|c| std::mem::take(&mut *c.borrow_mut())))
    }

    pub(crate) fn tls_put(state: Box<dyn std::any::Any>) {
        if let Ok(v) = state.downcast::<FastMap<Vec<u8>, Rc<Fmt>>>() {
            let old = CACHE.with(|c| std::mem::replace(&mut *c.borrow_mut(), *v));
            drop(old);
        }
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let exc = it.exc_type("Exception");
        let error = new_type(it, "struct", "error", Some(&exc), Layout::Exception);
        let d = it.module_dict(m);
        dict_set_str(&d, "error", Value::Obj(error));
    }

    fn struct_error(it: &mut Interp, msg: &str) -> Obj {
        let cls = match dict_get_str(&it.modules, "_struct") {
            Some(Value::Obj(m)) => {
                let d = it.module_dict(&m);
                match dict_get_str(&d, "error") {
                    Some(Value::Obj(c)) => c,
                    _ => it.exc_type("Exception"),
                }
            }
            _ => it.exc_type("Exception"),
        };
        it.new_exc(&cls, vec![Value::str(msg)])
    }

    fn is_space(c: u8) -> bool {
        matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
    }

    fn compile(it: &mut Interp, text: &str, spec: &[u8]) -> R<Fmt> {
        let too_long = |it: &mut Interp| struct_error(it, "total struct size too long");
        let (mode, rest) = match spec.first().and_then(|&c| StructMode::from_prefix(c)) {
            Some(m) => (m, &spec[1..]),
            None => (StructMode::Native, spec),
        };
        let max = isize::MAX as usize;
        let mut codes = Vec::new();
        let mut size = 0usize;
        let mut nvalues = 0usize;
        let mut i = 0;
        while i < rest.len() {
            let mut c = rest[i];
            i += 1;
            if is_space(c) {
                continue;
            }
            let mut num = 1usize;
            if c.is_ascii_digit() {
                num = (c - b'0') as usize;
                loop {
                    let Some(&d) = rest.get(i) else {
                        return Err(struct_error(it, "repeat count given without format specifier"));
                    };
                    i += 1;
                    if !d.is_ascii_digit() {
                        c = d;
                        break;
                    }
                    let d = (d - b'0') as usize;
                    if num > (max - d) / 10 {
                        return Err(too_long(it));
                    }
                    num = num * 10 + d;
                }
            }
            let Some(sc) = struct_code(c, mode) else {
                return Err(struct_error(it, "bad char in struct format"));
            };
            if sc.align > 1 {
                size = match size.checked_add(sc.align - 1) {
                    Some(s) => s / sc.align * sc.align,
                    None => return Err(too_long(it)),
                };
            }
            match c {
                b's' | b'p' => {
                    codes.push(Code { ch: c, kind: None, offset: size, size: num, repeat: 1 });
                    nvalues += 1;
                    size = size.checked_add(num).filter(|s| *s <= max).ok_or_else(|| too_long(it))?;
                }
                b'x' => size = size.checked_add(num).filter(|s| *s <= max).ok_or_else(|| too_long(it))?,
                _ if num > 0 => {
                    if num > (max - size) / sc.size {
                        return Err(too_long(it));
                    }
                    codes.push(Code { ch: c, kind: sc.kind, offset: size, size: sc.size, repeat: num });
                    nvalues += num;
                    size += sc.size * num;
                }
                _ => {}
            }
        }
        Ok(Fmt { text: text.to_string(), codes, size, nvalues, mode, uninit: false })
    }

    fn compile_value(it: &mut Interp, v: &Value) -> R<Fmt> {
        let (text, bytes): (String, Vec<u8>) = match v {
            Value::Obj(o) => match &o.kind {
                Kind::Str(s) => {
                    let b = it.encode_str(&s.s, "ascii", "strict")?;
                    (s.s.to_string(), b)
                }
                Kind::Bytes(b) => (b.iter().map(|&c| c as char).collect(), b.clone()),
                _ => return Err(format_type_error(it, v)),
            },
            _ => return Err(format_type_error(it, v)),
        };
        if bytes.contains(&0) {
            return Err(struct_error(it, "embedded null character"));
        }
        compile(it, &text, &bytes)
    }

    fn format_type_error(it: &mut Interp, v: &Value) -> Obj {
        let t = it.type_name_of(v);
        it.type_error(&format!("Struct() argument 1 must be a str or bytes object, not {}", t))
    }

    fn cached_fmt(it: &mut Interp, v: &Value) -> R<Rc<Fmt>> {
        let key: Option<&[u8]> = match v {
            Value::Obj(o) => match &o.kind {
                Kind::Str(s) if s.ascii => Some(s.s.as_bytes()),
                Kind::Bytes(b) => Some(b),
                _ => None,
            },
            _ => None,
        };
        if let Some(k) = key {
            if let Some(f) = CACHE.with(|c| c.borrow().get(k).cloned()) {
                return Ok(f);
            }
        }
        let f = Rc::new(compile_value(it, v)?);
        if let Some(k) = key {
            CACHE.with(|c| {
                let mut c = c.borrow_mut();
                if c.len() >= MAX_CACHE {
                    c.clear();
                }
                c.insert(k.to_vec(), f.clone());
            });
        }
        Ok(f)
    }

    fn unpack_values(f: &Fmt, data: &[u8]) -> Vec<Value> {
        let order = f.mode.order();
        let mut out = Vec::with_capacity(f.nvalues);
        for c in &f.codes {
            match c.kind {
                None if c.ch == b's' => out.push(Value::bytes(data[c.offset..c.offset + c.size].to_vec())),
                None => {
                    let n = c.size;
                    let v = if n == 0 {
                        Vec::new()
                    } else {
                        let len = (data[c.offset] as usize).min(n - 1);
                        data[c.offset + 1..c.offset + 1 + len].to_vec()
                    };
                    out.push(Value::bytes(v));
                }
                Some(kind) => {
                    for k in 0..c.repeat {
                        let at = c.offset + k * c.size;
                        out.push(scalar_value(load(kind, &data[at..at + c.size], order)));
                    }
                }
            }
        }
        out
    }

    fn int_arg(it: &mut Interp, v: &Value) -> R<Option<i128>> {
        if !it.has_index(v) {
            return Err(struct_error(it, "required argument is not an integer"));
        }
        index_i128(it, v)
    }

    fn float_arg(it: &mut Interp, v: &Value) -> R<f64> {
        let cls = it.type_of(v);
        let ok = matches!(v, Value::Float(_) | Value::Int(_) | Value::Bool(_))
            || it.lookup_mro(&cls, "__float__").is_some()
            || it.has_index(v)
            || matches!(v, Value::Obj(o) if matches!(o.kind, Kind::Float(_)));
        if ok {
            if let Ok(f) = it.float_arg(v) {
                return Ok(f);
            }
        }
        Err(struct_error(it, "required argument is not a float"))
    }

    fn pack_one(it: &mut Interp, f: &Fmt, c: &Code, kind: ElemKind, v: &Value, out: &mut [u8]) -> R<()> {
        let order = f.mode.order();
        match kind {
            ElemKind::Char => match v {
                Value::Obj(o) if matches!(&o.kind, Kind::Bytes(b) if b.len() == 1) => {
                    out[0] = if let Kind::Bytes(b) = &o.kind { b[0] } else { 0 };
                    Ok(())
                }
                _ => Err(struct_error(it, "char format requires a bytes object of length 1")),
            },
            ElemKind::Bool => {
                out[0] = it.truthy(v)? as u8;
                Ok(())
            }
            k if k.is_float() => {
                let x = float_arg(it, v)?;
                if k == ElemKind::F32 && f.mode.is_native() {
                    // Native `f` is a C cast (CPython's `np_float`): overflow gives infinity.
                    store_f64(k, x, out, order);
                    return Ok(());
                }
                match store_float_checked(k, x, out, order) {
                    Ok(()) => Ok(()),
                    Err(_) => Err(it.overflow_err(&format!("float too large to pack with {} format", c.ch as char))),
                }
            }
            _ => {
                let x = int_arg(it, v)?;
                if c.ch == b'P' {
                    return match x {
                        Some(x) if (-(1i128 << 63)..=(1i128 << 64) - 1).contains(&x) => {
                            store_int_wrapping(kind, x, out, order);
                            Ok(())
                        }
                        _ => Err(struct_error(it, "argument out of range")),
                    };
                }
                let range = |kind: ElemKind| kind.int_range().unwrap_or((0, 0));
                let r = match x {
                    Some(x) => store_int_checked(kind, x, out, order),
                    None => {
                        let (lo, hi) = range(kind);
                        Err(PackError::OutOfRange { lo, hi })
                    }
                };
                match r {
                    Ok(()) => Ok(()),
                    Err(PackError::OutOfRange { lo, hi }) => {
                        Err(struct_error(it, &format!("'{}' format requires {} <= number <= {}", c.ch as char, lo, hi)))
                    }
                    Err(_) => Err(struct_error(it, "required argument is not an integer")),
                }
            }
        }
    }

    fn bytes_arg(v: &Value) -> Option<Vec<u8>> {
        match v {
            Value::Obj(o) => match &o.kind {
                Kind::Bytes(b) => Some(b.clone()),
                Kind::ByteArray(b) => Some(b.to_vec()),
                _ => None,
            },
            _ => None,
        }
    }

    fn pack_values(it: &mut Interp, f: &Fmt, fname: &str, args: &[Value]) -> R<Vec<u8>> {
        if args.len() != f.nvalues || f.uninit {
            return Err(struct_error(it, &format!("{} expected {} items for packing (got {})", fname, shown(f, f.nvalues), args.len())));
        }
        let mut buf: Vec<u8> = it.vec_with_capacity(f.size, crate::limits::MAX_BYTES_LEN)?;
        buf.resize(f.size, 0);
        let mut next = 0;
        for c in &f.codes {
            match c.kind {
                None => {
                    let v = &args[next];
                    next += 1;
                    let Some(data) = bytes_arg(v) else {
                        return Err(struct_error(it, &format!("argument for '{}' must be a bytes object", c.ch as char)));
                    };
                    let dst = &mut buf[c.offset..c.offset + c.size];
                    if c.ch == b's' {
                        let n = data.len().min(c.size);
                        dst[..n].copy_from_slice(&data[..n]);
                    } else if c.size > 0 {
                        let n = data.len().min(c.size - 1).min(255);
                        dst[0] = n as u8;
                        dst[1..1 + n].copy_from_slice(&data[..n]);
                    }
                }
                Some(kind) => {
                    for k in 0..c.repeat {
                        let at = c.offset + k * c.size;
                        pack_one(it, f, c, kind, &args[next], &mut buf[at..at + c.size])?;
                        next += 1;
                    }
                }
            }
        }
        Ok(buf)
    }

    fn do_unpack(it: &mut Interp, f: &Fmt, data: &[u8]) -> R<Value> {
        if data.len() != f.size || f.uninit {
            return Err(struct_error(it, &format!("unpack requires a buffer of {} bytes", shown(f, f.size))));
        }
        Ok(Value::tuple(unpack_values(f, data)))
    }

    fn do_unpack_from(it: &mut Interp, f: &Fmt, data: &[u8], offset: isize) -> R<Value> {
        let mut off = offset as i64;
        let len = data.len() as i64;
        let size = f.size as i64;
        if off < 0 {
            if off.saturating_add(size) > 0 {
                return Err(struct_error(it, &format!("not enough data to unpack {} bytes at offset {}", size, off)));
            }
            if off.saturating_add(len) < 0 {
                return Err(struct_error(it, &format!("offset {} out of range for {}-byte buffer", off, len)));
            }
            off += len;
        }
        if len - off < size {
            let msg = format!(
                "unpack_from requires a buffer of at least {} bytes for unpacking {} bytes at offset {} (actual buffer size is {})",
                off.saturating_add(size),
                size,
                off,
                len
            );
            return Err(struct_error(it, &msg));
        }
        let at = off as usize;
        Ok(Value::tuple(unpack_values(f, &data[at..at + f.size])))
    }

    fn do_pack_into(it: &mut Interp, f: &Fmt, args: &[Value]) -> R<Value> {
        if args.is_empty() {
            return Err(struct_error(it, "pack_into expected buffer argument"));
        }
        if args.len() < 2 {
            return Err(struct_error(it, "pack_into expected offset argument"));
        }
        // A `bytearray` (the usual target) is exported directly, without a view descriptor.
        let (src, base, len) = match &args[0] {
            Value::Obj(o) if matches!(o.kind, Kind::ByteArray(_)) => {
                let Kind::ByteArray(store) = &o.kind else { unreachable!() };
                let e = store.export().map_err(|e| buffer_error(it, e))?;
                (memview::Source::Store(e), 0, store.len())
            }
            v => match memview::export(it, v)? {
                Some(e) if !e.view.readonly && e.view.is_c_contiguous() => (e.src, e.view.offset, e.view.nbytes()),
                _ => {
                    let t = it.type_name_of(v);
                    return Err(it.type_error(&format!("argument must be read-write bytes-like object, not {}", t)));
                }
            },
        };
        let mut off = match it.index_of(&args[1]) {
            Ok(o) => o,
            Err(e) if it.exc_is(&e, "OverflowError") => {
                let t = it.type_name_of(&args[1]);
                return Err(it.new_exc_str("IndexError", &format!("cannot fit '{}' into an index-sized integer", t)));
            }
            Err(e) => return Err(e),
        };
        let packed = pack_values(it, f, "pack_into", &args[2..])?;
        let len = len as i64;
        let size = f.size as i64;
        if off < 0 {
            if off.saturating_add(size) > 0 {
                return Err(struct_error(it, &format!("no space to pack {} bytes at offset {}", size, off)));
            }
            if off.saturating_add(len) < 0 {
                return Err(struct_error(it, &format!("offset {} out of range for {}-byte buffer", off, len)));
            }
            off += len;
        }
        if len - off < size {
            let msg = format!(
                "pack_into requires a buffer of at least {} bytes for packing {} bytes at offset {} (actual buffer size is {})",
                off.saturating_add(size),
                size,
                off,
                len
            );
            return Err(struct_error(it, &msg));
        }
        let at = base + off as usize;
        let r = src.with_mut(|b| b[at..at + packed.len()].copy_from_slice(&packed));
        r.map_err(|e| buffer_error(it, e))?;
        Ok(Value::None)
    }

    fn make_iter(it: &mut Interp, f: Rc<Fmt>, buffer: &Value) -> R<Value> {
        let src = match memview::export(it, buffer)? {
            Some(e) if e.view.is_c_contiguous() => e,
            _ => {
                let t = it.type_name_of(buffer);
                return Err(it.type_error(&format!("a bytes-like object is required, not '{}'", t)));
            }
        };
        if f.size == 0 {
            return Err(struct_error(it, "cannot iteratively unpack with a struct of length 0"));
        }
        let len = src.view.nbytes();
        if len % f.size != 0 {
            let msg = format!("iterative unpacking requires a buffer of a multiple of {} bytes", f.size);
            return Err(struct_error(it, &msg));
        }
        Ok(Py::new(it, UnpackIterator { fmt: f, src: Some(src.src), view: src.view, index: 0 }).into_value())
    }

    /// Struct(fmt) --> compiled struct object
    ///
    #[class(name = "Struct")]
    pub struct Struct {
        fmt: Option<Rc<Fmt>>,
    }

    impl Struct {
        fn fmt(&self, it: &mut Interp) -> R<Rc<Fmt>> {
            let _ = it;
            Ok(self.fmt.clone().unwrap_or_else(|| Rc::new(Fmt::uninitialized())))
        }
    }

    #[methods]
    impl Struct {
        #[constructor(hint(py(text_signature = "")))]
        fn new(#[varargs] args: &[Value], #[varkw] kwargs: crate::bind::KwArgs) -> Struct {
            let _ = (args, kwargs);
            Struct { fmt: None }
        }

        #[proto(init)]
        #[method(hint(py(text_signature = "($self, /, *args, **kwargs)")))]
        fn __init__(&mut self, it: &mut Interp, #[kw] format: &Value) -> R<()> {
            self.fmt = Some(Rc::new(compile_value(it, format)?));
            Ok(())
        }

        #[getter]
        fn format(&self) -> Value {
            self.fmt.as_ref().map_or(Value::None, |f| Value::string(f.text.clone()))
        }

        #[getter]
        fn size(&self) -> i64 {
            self.fmt.as_ref().map_or(-1, |f| f.size as i64)
        }

        #[method(hint(py(text_signature = "")))]
        fn pack(&self, it: &mut Interp, #[varargs] values: &[Value]) -> R<Value> {
            let f = self.fmt(it)?;
            Ok(Value::bytes(pack_values(it, &f, "pack", values)?))
        }

        #[method(hint(py(text_signature = "")))]
        fn pack_into(&self, it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
            let f = self.fmt(it)?;
            do_pack_into(it, &f, args)
        }

        fn unpack(&self, it: &mut Interp, buffer: &[u8]) -> R<Value> {
            let f = self.fmt(it)?;
            do_unpack(it, &f, buffer)
        }

        fn unpack_from(&self, it: &mut Interp, #[kw] buffer: &[u8], #[kw] #[default(0)] offset: isize) -> R<Value> {
            let f = self.fmt(it)?;
            do_unpack_from(it, &f, buffer, offset)
        }

        fn iter_unpack(&self, it: &mut Interp, buffer: &Value) -> R<Value> {
            let f = self.fmt(it)?;
            make_iter(it, f, buffer)
        }

        #[method(hint(py(text_signature = "")))]
        fn __sizeof__(&self) -> i64 {
            56 + 32 * (self.fmt.as_ref().map_or(0, |f| f.codes.len()) as i64 + 1)
        }
    }

    #[class(name = "unpack_iterator", skip(py))]
    pub struct UnpackIterator {
        fmt: Rc<Fmt>,
        /// Holds an export of the buffer (a `bytearray` cannot be resized meanwhile); `None`
        /// once exhausted.
        src: Option<Source>,
        view: ViewDesc,
        index: usize,
    }

    #[methods]
    impl UnpackIterator {
        #[proto(iter)]
        fn __iter__(slf: This<Py<Self>>) -> Py<Self> {
            slf.0
        }

        #[proto(next)]
        fn __next__(&mut self, it: &mut Interp) -> R<Option<Value>> {
            let size = self.fmt.size;
            let Some(src) = &self.src else { return Ok(None) };
            if self.index + size > self.view.nbytes() {
                self.src = None;
                return Ok(None);
            }
            let at = self.view.offset + self.index;
            let fmt = self.fmt.clone();
            let r = src.with(|b| unpack_values(&fmt, &b[at..at + size]));
            let items = r.map_err(|e| buffer_error(it, e))?;
            self.index += size;
            Ok(Some(Value::tuple(items)))
        }

        #[method(hint(py(text_signature = "")))]
        fn __length_hint__(&self) -> usize {
            match self.src {
                Some(_) => (self.view.nbytes().saturating_sub(self.index)) / self.fmt.size.max(1),
                None => 0,
            }
        }
    }

    #[op]
    fn calcsize(it: &mut Interp, format: &Value) -> R<i64> {
        Ok(cached_fmt(it, format)?.size as i64)
    }

    #[op(hint(py(text_signature = "")))]
    fn pack(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        let Some(fv) = args.first() else { return Err(it.type_error("missing format argument")) };
        let f = cached_fmt(it, fv)?;
        Ok(Value::bytes(pack_values(it, &f, "pack", &args[1..])?))
    }

    #[op(hint(py(text_signature = "")))]
    fn pack_into(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        let Some(fv) = args.first() else { return Err(it.type_error("missing format argument")) };
        let f = cached_fmt(it, fv)?;
        do_pack_into(it, &f, &args[1..])
    }

    #[op]
    fn unpack(it: &mut Interp, format: &Value, buffer: &[u8]) -> R<Value> {
        let f = cached_fmt(it, format)?;
        do_unpack(it, &f, buffer)
    }

    #[op]
    fn unpack_from(it: &mut Interp, format: &Value, #[kw] buffer: &[u8], #[kw] #[default(0)] offset: isize) -> R<Value> {
        let f = cached_fmt(it, format)?;
        do_unpack_from(it, &f, buffer, offset)
    }

    #[op]
    fn iter_unpack(it: &mut Interp, format: &Value, buffer: &Value) -> R<Value> {
        let f = cached_fmt(it, format)?;
        make_iter(it, f, buffer)
    }

    #[op]
    fn _clearcache() {
        CACHE.with(|c| c.borrow_mut().clear());
    }
}
