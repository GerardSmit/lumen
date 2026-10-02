//! `array`: typed arrays of numbers over a growable `lumen_common::buffer::ByteStore`, with
//! elements encoded by the shared codecs in `lumen_common::buffer::format`. The store is
//! exported to `memoryview`, `struct` and every bytes-like consumer through `memview::export`.

#[lumen_bind::module(name = "array")]
pub mod array {
    #![allow(clippy::new_ret_no_self)]

    use super::super::memview::{index_i128, scalar_value};
    use crate::ast::CmpOp;
    use crate::bind::{buffer_error, opaque_instance, type_object, KwArgs, Py, This};
    use crate::object::*;
    use crate::vm::{dict_set_str, Interp};
    use lumen_common::buffer::{load, store_f64, store_int_wrapping, struct_code, BufferError, ByteOrder, ByteStore, ElemKind, StructMode};
    use lumen_common::smuggle;
    use std::rc::Rc;

    const TYPECODES: &str = "bBuhHiIlLqQfd";

    /// One element type: its typecode, codec kind and size.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub struct Spec {
        pub tc: u8,
        pub kind: ElemKind,
        pub size: usize,
    }

    impl Spec {
        pub fn of(tc: u8) -> Option<Spec> {
            if tc == b'u' {
                return Some(Spec { tc, kind: ElemKind::U32, size: 4 });
            }
            if !TYPECODES.as_bytes().contains(&tc) {
                return None;
            }
            let c = struct_code(tc, StructMode::Native)?;
            Some(Spec { tc, kind: c.kind?, size: c.size })
        }

        /// The buffer format a `memoryview` of the array reports.
        pub fn format(self) -> &'static str {
            match self.tc {
                b'b' => "b",
                b'B' => "B",
                b'u' => "w",
                b'h' => "h",
                b'H' => "H",
                b'i' => "i",
                b'I' => "I",
                b'l' => "l",
                b'L' => "L",
                b'q' => "q",
                b'Q' => "Q",
                b'f' => "f",
                _ => "d",
            }
        }

        fn is_float(self) -> bool {
            self.kind.is_float()
        }

        /// The machine format code `__reduce_ex__` pickles (little-endian, IEEE floats).
        fn mformat(self) -> i64 {
            match (self.tc, self.size) {
                (b'u', _) => 20,
                (b'f', _) => 14,
                (b'd', _) => 16,
                (tc, size) => {
                    let signed = matches!(tc, b'b' | b'h' | b'i' | b'l' | b'q');
                    let base = match size {
                        1 => return if signed { 1 } else { 0 },
                        2 => 2,
                        4 => 6,
                        _ => 10,
                    };
                    base + if signed { 2 } else { 0 }
                }
            }
        }
    }

    fn overflow(it: &mut Interp, msg: &str) -> Obj {
        it.overflow_err(msg)
    }

    /// An integer item, range-checked with the messages of CPython's per-typecode setters.
    fn int_item(it: &mut Interp, tc: u8, v: &Value) -> R<i128> {
        if !it.has_index(v) {
            let t = it.type_name_of(v);
            return Err(it.type_error(&format!("'{}' object cannot be interpreted as an integer", t)));
        }
        let n = index_i128(it, v)?;
        let neg = match n {
            Some(n) => n < 0,
            None => match v.as_bigint() {
                Some(b) => b.is_negative(),
                None => {
                    let r = it.call_special(v, "__index__", Vec::new())?;
                    r.as_bigint().is_some_and(|b| b.is_negative())
                }
            },
        };
        let n = n.filter(|n| i64::try_from(*n).is_ok() || u64::try_from(*n).is_ok());
        let range = |it: &mut Interp, n: i128, lo: i128, hi: i128, what: &str| -> R<()> {
            if n < lo {
                return Err(overflow(it, &format!("{} is less than minimum", what)));
            }
            if n > hi {
                return Err(overflow(it, &format!("{} is greater than maximum", what)));
            }
            Ok(())
        };
        match tc {
            b'b' | b'B' | b'h' | b'H' | b'i' | b'l' => {
                let Some(n) = n.filter(|n| i64::try_from(*n).is_ok()) else {
                    return Err(overflow(it, "Python int too large to convert to C long"));
                };
                match tc {
                    b'b' => {
                        range(it, n, i16::MIN as i128, i16::MAX as i128, "signed short integer")?;
                        range(it, n, i8::MIN as i128, i8::MAX as i128, "signed char")?;
                    }
                    b'B' => range(it, n, 0, 255, "unsigned byte integer")?,
                    b'h' => range(it, n, i16::MIN as i128, i16::MAX as i128, "signed short integer")?,
                    b'H' => {
                        range(it, n, i32::MIN as i128, i32::MAX as i128, "signed integer")?;
                        range(it, n, 0, u16::MAX as i128, "unsigned short")?;
                    }
                    b'i' => range(it, n, i32::MIN as i128, i32::MAX as i128, "signed integer")?,
                    _ => {}
                }
                Ok(n)
            }
            b'I' | b'L' => {
                if neg {
                    return Err(overflow(it, "can't convert negative value to unsigned int"));
                }
                let Some(n) = n.filter(|n| u64::try_from(*n).is_ok()) else {
                    return Err(overflow(it, "Python int too large to convert to C unsigned long"));
                };
                if tc == b'I' && n > u32::MAX as i128 {
                    return Err(overflow(it, "unsigned int is greater than maximum"));
                }
                Ok(n)
            }
            b'q' => match n.filter(|n| i64::try_from(*n).is_ok()) {
                Some(n) => Ok(n),
                None => Err(overflow(it, "int too big to convert")),
            },
            _ => {
                if neg {
                    return Err(overflow(it, "can't convert negative int to unsigned"));
                }
                match n.filter(|n| u64::try_from(*n).is_ok()) {
                    Some(n) => Ok(n),
                    None => Err(overflow(it, "int too big to convert")),
                }
            }
        }
    }

    fn float_item(it: &mut Interp, v: &Value) -> R<f64> {
        let is_float = matches!(v, Value::Float(_) | Value::Int(_) | Value::Bool(_))
            || matches!(v, Value::Obj(o) if matches!(o.kind, Kind::Float(_) | Kind::Int(_)));
        if !is_float {
            let cls = it.type_of(v);
            if it.lookup_mro(&cls, "__float__").is_none() {
                if it.has_index(v) {
                    let n = it.call_special(v, "__index__", Vec::new())?;
                    return it.float_arg(&n);
                }
                let t = it.type_name_of(v);
                return Err(it.type_error(&format!("must be real number, not {}", t)));
            }
        }
        it.float_arg(v)
    }

    /// The bytes of one item `v` (the first `spec.size` of the result).
    pub fn encode(it: &mut Interp, spec: Spec, v: &Value) -> R<[u8; 8]> {
        let mut out = [0u8; 8];
        let buf = &mut out[..spec.size];
        if spec.tc == b'u' {
            let cp = match v {
                Value::Obj(o) if matches!(o.kind, Kind::Str(_)) => {
                    let s = v.as_str().unwrap_or("");
                    let mut cps = smuggle::code_points(s);
                    match (cps.next(), cps.next()) {
                        (Some(c), None) => Some(c),
                        _ => None,
                    }
                }
                _ => None,
            };
            let Some(cp) = cp else { return Err(it.type_error("array item must be unicode character")) };
            buf.copy_from_slice(&cp.to_ne_bytes());
        } else if spec.is_float() {
            let x = float_item(it, v)?;
            store_f64(spec.kind, x, buf, ByteOrder::NATIVE);
        } else {
            let n = int_item(it, spec.tc, v)?;
            store_int_wrapping(spec.kind, n, buf, ByteOrder::NATIVE);
        }
        Ok(out)
    }

    /// The Python value of one encoded item.
    pub fn decode(it: &mut Interp, spec: Spec, b: &[u8]) -> R<Value> {
        if spec.tc == b'u' {
            let cp = u32::from_ne_bytes([b[0], b[1], b[2], b[3]]);
            let mut s = String::new();
            if cp > 0x10FFFF || !smuggle::push_code_point(&mut s, cp) {
                return Err(it.value_error(&format!("character U+{:x} is not in range [U+0000; U+10ffff]", cp)));
            }
            return Ok(Value::string(s));
        }
        Ok(scalar_value(load(spec.kind, b, ByteOrder::NATIVE)))
    }

    fn decode_all(it: &mut Interp, spec: Spec, bytes: &[u8]) -> R<Vec<Value>> {
        let mut out = Vec::with_capacity(bytes.len() / spec.size);
        for c in bytes.chunks_exact(spec.size) {
            out.push(decode(it, spec, c)?);
        }
        Ok(out)
    }

    fn resize_error(it: &mut Interp, e: BufferError) -> Obj {
        match e {
            BufferError::Pinned => it.new_exc_str("BufferError", "cannot resize an array that is exporting buffers"),
            e => buffer_error(it, e),
        }
    }

    fn edit<T>(it: &mut Interp, store: &ByteStore, f: impl FnOnce(&mut Vec<u8>) -> T) -> R<T> {
        store.edit(f).map_err(|e| resize_error(it, e))
    }

    fn index_in(it: &mut Interp, key: &Value, len: usize, msg: &str) -> R<usize> {
        let i = it.seq_index(key)?;
        let j = if i < 0 { i + len as i64 } else { i };
        if j < 0 || j >= len as i64 {
            return Err(it.new_exc_str("IndexError", msg));
        }
        Ok(j as usize)
    }

    fn slice_positions(start: i64, step: i64, n: usize) -> impl Iterator<Item = usize> {
        (0..n as i64).map(move |k| (start + k * step) as usize)
    }

    fn array_of(it: &mut Interp, v: &Value) -> Option<(Spec, Rc<ByteStore>)> {
        let p = Py::<Array>::from_value(it, v)?;
        let a = p.borrow(it).ok()?;
        Some((a.spec, a.store.clone()))
    }

    /// The element spec and store of array `v`; `None` when `v` is not an array.
    pub fn parts(it: &mut Interp, v: &Value) -> Option<(Spec, Rc<ByteStore>)> {
        array_of(it, v)
    }

    fn new_array(it: &mut Interp, spec: Spec, bytes: Vec<u8>) -> Value {
        Py::new(it, Array::with(spec, bytes)).into_value()
    }

    fn typecode_of(it: &mut Interp, v: &Value, what: &str) -> R<u8> {
        let s = match v.as_str() {
            Some(s) => s.to_string(),
            None => {
                let t = it.type_name_of(v);
                return Err(it.type_error(&format!("{} must be a unicode character, not {}", what, t)));
            }
        };
        let mut cps = smuggle::code_points(&s);
        match (cps.next(), cps.next()) {
            (Some(c), None) => Ok(if c < 128 { c as u8 } else { 0 }),
            _ => Err(it.type_error(&format!("{} must be a unicode character, not str", what))),
        }
    }

    fn bad_typecode(it: &mut Interp) -> Obj {
        it.value_error("bad typecode (must be b, B, u, h, H, i, I, l, L, q, Q, f or d)")
    }

    /// Appends `items` one by one (earlier items stay when a later one fails, as in CPython).
    fn append_values(it: &mut Interp, spec: Spec, store: &ByteStore, items: &[Value]) -> R<()> {
        for v in items {
            let b = encode(it, spec, v)?;
            edit(it, store, |vec| vec.extend_from_slice(&b[..spec.size]))?;
        }
        Ok(())
    }

    fn extend_iter(it: &mut Interp, spec: Spec, store: &ByteStore, iterable: &Value) -> R<()> {
        let iter = it.get_iter(iterable)?;
        while let Some(v) = it.iter_next(&iter)? {
            let b = encode(it, spec, &v)?;
            edit(it, store, |vec| vec.extend_from_slice(&b[..spec.size]))?;
        }
        Ok(())
    }

    fn from_bytes(it: &mut Interp, spec: Spec, store: &ByteStore, data: &[u8]) -> R<()> {
        if data.len() % spec.size != 0 {
            return Err(it.value_error("bytes length not a multiple of item size"));
        }
        if !data.is_empty() {
            edit(it, store, |v| v.extend_from_slice(data))?;
        }
        Ok(())
    }

    fn from_unicode(it: &mut Interp, spec: Spec, store: &ByteStore, s: &str) -> R<()> {
        if spec.tc != b'u' {
            return Err(it.value_error("fromunicode() may only be called on unicode type arrays"));
        }
        let mut data = Vec::new();
        for cp in smuggle::code_points(s) {
            data.extend_from_slice(&cp.to_ne_bytes());
        }
        from_bytes(it, spec, store, &data)
    }

    fn to_unicode(it: &mut Interp, spec: Spec, store: &ByteStore) -> R<Value> {
        if spec.tc != b'u' {
            return Err(it.value_error("tounicode() may only be called on unicode type arrays"));
        }
        let data = store.to_vec();
        let mut s = String::new();
        for c in data.chunks_exact(4) {
            let cp = u32::from_ne_bytes([c[0], c[1], c[2], c[3]]);
            if cp > 0x10FFFF || !smuggle::push_code_point(&mut s, cp) {
                return Err(it.value_error(&format!("character U+{:x} is not in range [U+0000; U+10ffff]", cp)));
            }
        }
        Ok(Value::string(s))
    }

    /// Fills a new array from a constructor initializer.
    fn initialize(it: &mut Interp, spec: Spec, store: &ByteStore, init: &Value) -> R<()> {
        if let Value::Obj(o) = init {
            match &o.kind {
                Kind::Str(_) => {
                    if spec.tc != b'u' {
                        return Err(it.type_error(&format!("cannot use a str to initialize an array with typecode '{}'", spec.tc as char)));
                    }
                    let s = init.as_str().unwrap_or("").to_string();
                    return from_unicode(it, spec, store, &s);
                }
                Kind::Bytes(b) => {
                    let b = b.clone();
                    return from_bytes(it, spec, store, &b);
                }
                Kind::ByteArray(b) => {
                    let b = b.to_vec();
                    return from_bytes(it, spec, store, &b);
                }
                Kind::List(_) | Kind::Tuple(_) => {
                    let items = it.iterate_to_vec(init)?;
                    return append_values(it, spec, store, &items);
                }
                _ => {}
            }
        }
        if let Some((other, data)) = array_of(it, init) {
            if other.tc == b'u' && spec.tc != b'u' {
                return Err(it.type_error(&format!("cannot use a unicode array to initialize an array with typecode '{}'", spec.tc as char)));
            }
            if other == spec {
                let data = data.to_vec();
                return from_bytes(it, spec, store, &data);
            }
            let data = data.to_vec();
            let items = decode_all(it, other, &data)?;
            return append_values(it, spec, store, &items);
        }
        extend_iter(it, spec, store, init)
    }

    fn instance_dict(it: &mut Interp, v: &Value) -> R<Value> {
        match it.get_attr_str(v, "__dict__") {
            Ok(d) => Ok(d),
            Err(e) if it.exc_is(&e, "AttributeError") => Ok(Value::None),
            Err(e) => Err(e),
        }
    }

    #[class(name = "array", module = "array", generic, hint(py(unhashable)))]
    pub struct Array {
        spec: Spec,
        store: Rc<ByteStore>,
    }

    impl Array {
        fn with(spec: Spec, bytes: Vec<u8>) -> Array {
            Array { spec, store: Rc::new(ByteStore::new(bytes).growable()) }
        }

        fn len(&self) -> usize {
            self.store.len() / self.spec.size
        }
    }

    fn get(it: &mut Interp, slf: &Py<Array>) -> R<(Spec, Rc<ByteStore>)> {
        let a = slf.borrow(it)?;
        Ok((a.spec, a.store.clone()))
    }

    fn item_at(it: &mut Interp, spec: Spec, store: &ByteStore, i: usize) -> R<Value> {
        let mut b = [0u8; 8];
        store.read_at(i * spec.size, &mut b[..spec.size]);
        decode(it, spec, &b[..spec.size])
    }

    /// The index of the first item equal to `v` in `start..stop`.
    fn find(it: &mut Interp, slf: &Py<Array>, v: &Value, start: usize, stop: usize) -> R<Option<usize>> {
        let (spec, store) = get(it, slf)?;
        let mut i = start;
        while i < stop.min(store.len() / spec.size) {
            let x = item_at(it, spec, &store, i)?;
            if equal(it, &x, v)? {
                return Ok(Some(i));
            }
            i += 1;
        }
        Ok(None)
    }

    fn remove_range(it: &mut Interp, spec: Spec, store: &ByteStore, start: usize, stop: usize) -> R<()> {
        if stop > start {
            edit(it, store, |v| drop(v.drain(start * spec.size..stop * spec.size)))?;
        }
        Ok(())
    }

    fn repeat(data: &[u8], n: i64) -> Option<Vec<u8>> {
        if n <= 0 || data.is_empty() {
            return Some(Vec::new());
        }
        let total = data.len().checked_mul(usize::try_from(n).ok()?)?;
        if total > isize::MAX as usize / 2 {
            return None;
        }
        Some(data.repeat(n as usize))
    }

    fn compare(it: &mut Interp, slf: &Py<Array>, other: &Value, op: CmpOp) -> R<Value> {
        let Some((ospec, ostore)) = array_of(it, other) else { return Ok(Value::NotImplemented) };
        let (spec, store) = get(it, slf)?;
        let (n, m) = (store.len() / spec.size, ostore.len() / ospec.size);
        if n != m && matches!(op, CmpOp::Eq | CmpOp::NotEq) {
            return Ok(Value::Bool(op == CmpOp::NotEq));
        }
        if spec == ospec && !spec.is_float() && matches!(op, CmpOp::Eq | CmpOp::NotEq) {
            let same = *store.bytes() == *ostore.bytes();
            return Ok(Value::Bool(same == (op == CmpOp::Eq)));
        }
        let a = decode_all(it, spec, &store.to_vec())?;
        let b = decode_all(it, ospec, &ostore.to_vec())?;
        for (x, y) in a.iter().zip(&b) {
            if !equal(it, x, y)? {
                return match op {
                    CmpOp::Eq => Ok(Value::Bool(false)),
                    CmpOp::NotEq => Ok(Value::Bool(true)),
                    _ => it.rich_compare(op, x, y),
                };
            }
        }
        let (n, m) = (a.len(), b.len());
        Ok(Value::Bool(match op {
            CmpOp::Eq => n == m,
            CmpOp::NotEq => n != m,
            CmpOp::Lt => n < m,
            CmpOp::LtE => n <= m,
            CmpOp::Gt => n > m,
            _ => n >= m,
        }))
    }

    /// `==` without the identity shortcut: items are fresh objects in CPython, so a NaN item
    /// never equals itself.
    fn equal(it: &mut Interp, x: &Value, y: &Value) -> R<bool> {
        let r = it.rich_compare(CmpOp::Eq, x, y)?;
        it.truthy(&r)
    }

    #[methods]
    impl Array {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
            let Value::Obj(cls) = cls.0 else { return Err(it.type_error("array.__new__(X): X is not a type object")) };
            let base = type_object::<Array>(it);
            if Rc::ptr_eq(&cls, &base) && !kw.is_empty() {
                return Err(it.type_error("array.array() takes no keyword arguments"));
            }
            if args.is_empty() || args.len() > 2 {
                return Err(it.type_error(&format!("array() takes at most 2 arguments ({} given)", args.len())));
            }
            let tc = typecode_of(it, &args[0], "array() argument 1")?;
            let Some(spec) = Spec::of(tc) else { return Err(bad_typecode(it)) };
            let arr = Array::with(spec, Vec::new());
            if let Some(init) = args.get(1) {
                initialize(it, spec, &arr.store, init)?;
            }
            Ok(opaque_instance(&cls, arr))
        }

        /// the typecode character used to create the array
        #[getter]
        fn typecode(&self) -> String {
            (self.spec.tc as char).to_string()
        }

        /// the size, in bytes, of one array item
        #[getter]
        fn itemsize(&self) -> usize {
            self.spec.size
        }

        fn append(slf: This<Py<Self>>, it: &mut Interp, v: &Value) -> R<()> {
            let (spec, store) = get(it, &slf.0)?;
            append_values(it, spec, &store, std::slice::from_ref(v))
        }

        fn buffer_info(&self) -> (usize, usize) {
            (self.store.as_ptr() as usize, self.len())
        }

        fn byteswap(&self, it: &mut Interp) -> R<()> {
            let size = self.spec.size;
            let mut b = self.store.try_bytes_mut().map_err(|e| buffer_error(it, e))?;
            for c in b.chunks_exact_mut(size) {
                c.reverse();
            }
            Ok(())
        }

        fn count(slf: This<Py<Self>>, it: &mut Interp, v: &Value) -> R<usize> {
            let mut n = 0;
            let mut i = 0;
            while let Some(j) = find(it, &slf.0, v, i, usize::MAX)? {
                n += 1;
                i = j + 1;
            }
            Ok(n)
        }

        fn extend(slf: This<Py<Self>>, it: &mut Interp, bb: &Value) -> R<()> {
            let (spec, store) = get(it, &slf.0)?;
            if let Some((ospec, ostore)) = array_of(it, bb) {
                if ospec != spec {
                    return Err(it.type_error("can only extend with array of same kind"));
                }
                let data = ostore.to_vec();
                return from_bytes(it, spec, &store, &data);
            }
            extend_iter(it, spec, &store, bb)
        }

        fn frombytes(&self, it: &mut Interp, buffer: &[u8]) -> R<()> {
            from_bytes(it, self.spec, &self.store, buffer)
        }

        fn fromfile(slf: This<Py<Self>>, it: &mut Interp, f: &Value, n: i64) -> R<()> {
            let (spec, store) = get(it, &slf.0)?;
            if n < 0 {
                return Err(it.value_error("negative count"));
            }
            let Some(nbytes) = (n as usize).checked_mul(spec.size).filter(|b| *b <= isize::MAX as usize) else {
                return Err(it.memory_error());
            };
            let b = it.call_method(f, "read", vec![Value::Int(nbytes as i64)])?;
            let data = match &b {
                Value::Obj(o) => match &o.kind {
                    Kind::Bytes(d) => d.clone(),
                    _ => return Err(it.type_error("read() didn't return bytes")),
                },
                _ => return Err(it.type_error("read() didn't return bytes")),
            };
            let whole = data.len() / spec.size * spec.size;
            from_bytes(it, spec, &store, &data[..whole])?;
            if data.len() != nbytes {
                return Err(it.new_exc_str("EOFError", "read() didn't return enough bytes"));
            }
            Ok(())
        }

        fn fromlist(slf: This<Py<Self>>, it: &mut Interp, list: &Value) -> R<()> {
            let is_list = matches!(list, Value::Obj(o) if matches!(o.kind, Kind::List(_)));
            if !is_list {
                return Err(it.type_error("arg must be list"));
            }
            let (spec, store) = get(it, &slf.0)?;
            let items = it.iterate_to_vec(list)?;
            let mut data = Vec::with_capacity(items.len() * spec.size);
            for v in &items {
                let b = encode(it, spec, v)?;
                data.extend_from_slice(&b[..spec.size]);
            }
            from_bytes(it, spec, &store, &data)
        }

        fn fromunicode(&self, it: &mut Interp, ustr: &Value) -> R<()> {
            let Some(s) = ustr.as_str() else {
                let t = it.type_name_of(ustr);
                return Err(it.type_error(&format!("fromunicode() argument must be str, not {}", t)));
            };
            let s = s.to_string();
            from_unicode(it, self.spec, &self.store, &s)
        }

        fn index(slf: This<Py<Self>>, it: &mut Interp, v: &Value, #[default(0)] start: isize, #[default(isize::MAX)] stop: isize) -> R<usize> {
            let n = { slf.0.borrow(it)?.len() } as isize;
            let clamp = |i: isize| if i < 0 { (i + n).max(0) as usize } else { i as usize };
            match find(it, &slf.0, v, clamp(start), clamp(stop))? {
                Some(i) => Ok(i),
                None => Err(it.value_error("array.index(x): x not in array")),
            }
        }

        fn insert(slf: This<Py<Self>>, it: &mut Interp, i: isize, v: &Value) -> R<()> {
            let (spec, store) = get(it, &slf.0)?;
            let b = encode(it, spec, v)?;
            let n = (store.len() / spec.size) as isize;
            let at = if i < 0 { (i + n).max(0) } else { i.min(n) } as usize;
            edit(it, &store, |vec| {
                let p = at * spec.size;
                vec.splice(p..p, b[..spec.size].iter().copied());
            })
        }

        fn pop(slf: This<Py<Self>>, it: &mut Interp, #[default(-1)] i: isize) -> R<Value> {
            let (spec, store) = get(it, &slf.0)?;
            let n = (store.len() / spec.size) as isize;
            if n == 0 {
                return Err(it.new_exc_str("IndexError", "pop from empty array"));
            }
            let j = if i < 0 { i + n } else { i };
            if j < 0 || j >= n {
                return Err(it.new_exc_str("IndexError", "pop index out of range"));
            }
            let v = item_at(it, spec, &store, j as usize)?;
            remove_range(it, spec, &store, j as usize, j as usize + 1)?;
            Ok(v)
        }

        fn remove(slf: This<Py<Self>>, it: &mut Interp, v: &Value) -> R<()> {
            match find(it, &slf.0, v, 0, usize::MAX)? {
                Some(i) => {
                    let (spec, store) = get(it, &slf.0)?;
                    remove_range(it, spec, &store, i, i + 1)
                }
                None => Err(it.value_error("array.remove(x): x not in array")),
            }
        }

        fn reverse(&self, it: &mut Interp) -> R<()> {
            let size = self.spec.size;
            let mut b = self.store.try_bytes_mut().map_err(|e| buffer_error(it, e))?;
            let n = b.len() / size;
            for k in 0..n / 2 {
                let (x, y) = (k * size, (n - 1 - k) * size);
                for q in 0..size {
                    b.swap(x + q, y + q);
                }
            }
            Ok(())
        }

        fn tobytes(&self) -> Value {
            Value::bytes(self.store.to_vec())
        }

        fn tofile(slf: This<Py<Self>>, it: &mut Interp, f: &Value) -> R<()> {
            const BLOCKSIZE: usize = 64 * 1024;
            let (_, store) = get(it, &slf.0)?;
            let data = store.to_vec();
            for chunk in data.chunks(BLOCKSIZE) {
                it.call_method(f, "write", vec![Value::bytes(chunk.to_vec())])?;
            }
            Ok(())
        }

        fn tolist(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            let (spec, store) = get(it, &slf.0)?;
            Ok(Value::list(decode_all(it, spec, &store.to_vec())?))
        }

        fn tounicode(&self, it: &mut Interp) -> R<Value> {
            to_unicode(it, self.spec, &self.store)
        }

        #[method(name = "__reduce_ex__")]
        fn reduce_ex(slf: This<Py<Self>>, it: &mut Interp, protocol: &Value) -> R<Value> {
            let slf = slf.0;
            let is_int = matches!(protocol, Value::Int(_) | Value::Bool(_)) || matches!(protocol, Value::Obj(o) if matches!(o.kind, Kind::Int(_)));
            if !is_int {
                return Err(it.type_error("__reduce_ex__ argument should be an integer"));
            }
            let protocol = it.index_of(protocol)?;
            let (spec, store) = get(it, &slf)?;
            let dict = instance_dict(it, slf.value())?;
            let ty = Value::Obj(it.type_of(slf.value()));
            let tc = Value::string((spec.tc as char).to_string());
            if protocol < 3 {
                let list = Value::list(decode_all(it, spec, &store.to_vec())?);
                return Ok(Value::tuple(vec![ty, Value::tuple(vec![tc, list]), dict]));
            }
            let module = Value::Obj(it.import_module("array")?);
            let recon = it.get_attr_str(&module, "_array_reconstructor")?;
            let args = Value::tuple(vec![ty, tc, Value::Int(spec.mformat()), Value::bytes(store.to_vec())]);
            Ok(Value::tuple(vec![recon, args, dict]))
        }

        #[proto(copy)]
        fn __copy__(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            let (spec, store) = get(it, &slf.0)?;
            Ok(new_array(it, spec, store.to_vec()))
        }

        #[proto(deepcopy)]
        fn __deepcopy__(slf: This<Py<Self>>, it: &mut Interp, unused: &Value) -> R<Value> {
            let _ = unused;
            let (spec, store) = get(it, &slf.0)?;
            Ok(new_array(it, spec, store.to_vec()))
        }

        #[proto(sizeof)]
        fn __sizeof__(&self) -> usize {
            64 + self.store.len()
        }

        #[proto(len)]
        fn __len__(&self) -> usize {
            self.len()
        }

        #[proto(getitem)]
        fn __getitem__(slf: This<Py<Self>>, it: &mut Interp, key: &Value) -> R<Value> {
            let (spec, store) = get(it, &slf.0)?;
            let n = store.len() / spec.size;
            if it.has_index(key) {
                let i = index_in(it, key, n, "array index out of range")?;
                return item_at(it, spec, &store, i);
            }
            if !it.is_slice(key) {
                return Err(it.type_error("array indices must be integers"));
            }
            let (start, stop, step) = it.slice_bounds(key, n)?;
            let count = crate::ops::slice_len(start, stop, step);
            let bytes = store.bytes();
            let out = if step == 1 {
                bytes[start as usize * spec.size..(start as usize + count) * spec.size].to_vec()
            } else {
                let mut out = Vec::with_capacity(count * spec.size);
                for p in slice_positions(start, step, count) {
                    out.extend_from_slice(&bytes[p * spec.size..(p + 1) * spec.size]);
                }
                out
            };
            drop(bytes);
            Ok(new_array(it, spec, out))
        }

        #[proto(setitem)]
        fn __setitem__(slf: This<Py<Self>>, it: &mut Interp, key: &Value, value: &Value) -> R<()> {
            let (spec, store) = get(it, &slf.0)?;
            let n = store.len() / spec.size;
            if it.has_index(key) {
                let i = index_in(it, key, n, "array assignment index out of range")?;
                let b = encode(it, spec, value)?;
                store.write_at(i * spec.size, &b[..spec.size]);
                return Ok(());
            }
            if !it.is_slice(key) {
                return Err(it.type_error("array indices must be integers"));
            }
            let Some((ospec, ostore)) = array_of(it, value) else {
                let t = it.type_name_of(value);
                return Err(it.type_error(&format!("can only assign array (not \"{}\") to array slice", t)));
            };
            if ospec != spec {
                return Err(it.type_error("bad argument type for built-in operation"));
            }
            let data = ostore.to_vec();
            let (start, stop, step) = it.slice_bounds(key, n)?;
            let count = crate::ops::slice_len(start, stop, step);
            let m = data.len() / spec.size;
            if step == 1 {
                let (a, b) = (start as usize * spec.size, (start as usize + count) * spec.size);
                if m == count {
                    store.write_at(a, &data);
                    return Ok(());
                }
                return edit(it, &store, |v| drop(v.splice(a..b, data)));
            }
            if m != count {
                return Err(it.value_error(&format!("attempt to assign array of size {} to extended slice of size {}", m, count)));
            }
            for (k, p) in slice_positions(start, step, count).enumerate() {
                store.write_at(p * spec.size, &data[k * spec.size..(k + 1) * spec.size]);
            }
            Ok(())
        }

        #[proto(delitem)]
        fn __delitem__(slf: This<Py<Self>>, it: &mut Interp, key: &Value) -> R<()> {
            let (spec, store) = get(it, &slf.0)?;
            let n = store.len() / spec.size;
            if it.has_index(key) {
                let i = index_in(it, key, n, "array assignment index out of range")?;
                return remove_range(it, spec, &store, i, i + 1);
            }
            if !it.is_slice(key) {
                return Err(it.type_error("array indices must be integers"));
            }
            let (start, stop, step) = it.slice_bounds(key, n)?;
            let count = crate::ops::slice_len(start, stop, step);
            if count == 0 {
                return Ok(());
            }
            if step == 1 {
                return remove_range(it, spec, &store, start as usize, start as usize + count);
            }
            let mut drop_item = vec![false; n];
            for p in slice_positions(start, step, count) {
                drop_item[p] = true;
            }
            edit(it, &store, |v| {
                let kept: Vec<u8> = v.chunks_exact(spec.size).zip(&drop_item).filter(|(_, d)| !**d).flat_map(|(c, _)| c.iter().copied()).collect();
                *v = kept;
            })
        }

        #[proto(contains)]
        fn __contains__(slf: This<Py<Self>>, it: &mut Interp, v: &Value) -> R<bool> {
            Ok(find(it, &slf.0, v, 0, usize::MAX)?.is_some())
        }

        #[proto(iter)]
        fn __iter__(slf: This<Py<Self>>) -> ArrayIter {
            ArrayIter { arr: Some(slf.0), index: 0 }
        }

        #[proto(repr)]
        fn __repr__(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            let slf = slf.0;
            let ty = it.type_of(slf.value());
            let name = it.type_name(&ty);
            let (spec, store) = get(it, &slf)?;
            let tc = spec.tc as char;
            if store.is_empty() {
                return Ok(Value::string(format!("{}('{}')", name, tc)));
            }
            let body = if spec.tc == b'u' { to_unicode(it, spec, &store)? } else { Value::list(decode_all(it, spec, &store.to_vec())?) };
            let r = it.repr_of(&body)?;
            Ok(Value::string(format!("{}('{}', {})", name, tc, r)))
        }

        #[proto(eq)]
        fn __eq__(slf: This<Py<Self>>, it: &mut Interp, other: &Value) -> R<Value> {
            compare(it, &slf.0, other, CmpOp::Eq)
        }

        #[proto(ne)]
        fn __ne__(slf: This<Py<Self>>, it: &mut Interp, other: &Value) -> R<Value> {
            compare(it, &slf.0, other, CmpOp::NotEq)
        }

        #[proto(lt)]
        fn __lt__(slf: This<Py<Self>>, it: &mut Interp, other: &Value) -> R<Value> {
            compare(it, &slf.0, other, CmpOp::Lt)
        }

        #[proto(le)]
        fn __le__(slf: This<Py<Self>>, it: &mut Interp, other: &Value) -> R<Value> {
            compare(it, &slf.0, other, CmpOp::LtE)
        }

        #[proto(gt)]
        fn __gt__(slf: This<Py<Self>>, it: &mut Interp, other: &Value) -> R<Value> {
            compare(it, &slf.0, other, CmpOp::Gt)
        }

        #[proto(ge)]
        fn __ge__(slf: This<Py<Self>>, it: &mut Interp, other: &Value) -> R<Value> {
            compare(it, &slf.0, other, CmpOp::GtE)
        }

        #[proto(add)]
        fn __add__(slf: This<Py<Self>>, it: &mut Interp, other: &Value) -> R<Value> {
            let (spec, store) = get(it, &slf.0)?;
            let Some((ospec, ostore)) = array_of(it, other) else {
                let t = it.type_name_of(other);
                return Err(it.type_error(&format!("can only append array (not \"{}\") to array", t)));
            };
            if ospec != spec {
                return Err(it.type_error("bad argument type for built-in operation"));
            }
            let mut data = store.to_vec();
            data.extend_from_slice(&ostore.bytes());
            Ok(new_array(it, spec, data))
        }

        #[proto(iadd)]
        fn __iadd__(slf: This<Py<Self>>, it: &mut Interp, other: &Value) -> R<Py<Self>> {
            let (spec, store) = get(it, &slf.0)?;
            let Some((ospec, ostore)) = array_of(it, other) else {
                let t = it.type_name_of(other);
                return Err(it.type_error(&format!("can only extend array with array (not \"{}\")", t)));
            };
            if ospec != spec {
                return Err(it.type_error("can only extend with array of same kind"));
            }
            let data = ostore.to_vec();
            from_bytes(it, spec, &store, &data)?;
            Ok(slf.0)
        }

        #[proto(mul)]
        #[method(hint(py(aliases = "__rmul__")))]
        fn __mul__(slf: This<Py<Self>>, it: &mut Interp, n: &Value) -> R<Value> {
            let n = it.index_or(n, "OverflowError")?;
            let (spec, store) = get(it, &slf.0)?;
            let data = repeat(&store.bytes(), n);
            match data {
                Some(data) => Ok(new_array(it, spec, data)),
                None => Err(it.memory_error()),
            }
        }

        #[proto(imul)]
        fn __imul__(slf: This<Py<Self>>, it: &mut Interp, n: &Value) -> R<Value> {
            let n = it.index_or(n, "OverflowError")?;
            let (_, store) = get(it, &slf.0)?;
            let Some(data) = repeat(&store.bytes(), n) else { return Err(it.memory_error()) };
            if data.len() != store.len() {
                edit(it, &store, |v| *v = data)?;
            }
            Ok(slf.0.into_value())
        }
    }

    #[class(name = "arrayiterator", module = "array", hint(py(final)))]
    pub struct ArrayIter {
        arr: Option<Py<Array>>,
        index: usize,
    }

    #[methods]
    impl ArrayIter {
        #[proto(iter)]
        fn __iter__(slf: This<Py<Self>>) -> Py<Self> {
            slf.0
        }

        #[proto(next)]
        fn __next__(&mut self, it: &mut Interp) -> R<Option<Value>> {
            let Some(arr) = &self.arr else { return Ok(None) };
            let (spec, store) = get(it, arr)?;
            if self.index < store.len() / spec.size {
                let v = item_at(it, spec, &store, self.index)?;
                self.index += 1;
                return Ok(Some(v));
            }
            self.arr = None;
            Ok(None)
        }

        #[proto(reduce)]
        fn __reduce__(&self, it: &mut Interp) -> R<Value> {
            let builtins = Value::Obj(it.import_module("builtins")?);
            let iter = it.get_attr_str(&builtins, "iter")?;
            Ok(match &self.arr {
                Some(a) => Value::tuple(vec![iter, Value::tuple(vec![a.value().clone()]), Value::Int(self.index as i64)]),
                None => Value::tuple(vec![iter, Value::tuple(vec![Value::tuple(Vec::new())])]),
            })
        }

        fn __setstate__(&mut self, it: &mut Interp, state: &Value) -> R<()> {
            let i = it.index_of(state)?;
            if let Some(a) = &self.arr {
                let n = a.borrow(it)?.len() as i64;
                self.index = i.clamp(0, n) as usize;
            }
            Ok(())
        }
    }

    /// Internal. Used for pickling support.
    #[op]
    fn _array_reconstructor(it: &mut Interp, arraytype: &Value, typecode: &Value, mformat_code: i64, items: &Value) -> R<Value> {
        let Value::Obj(cls) = arraytype else {
            let t = it.type_name_of(arraytype);
            return Err(it.type_error(&format!("first argument must be a type object, not {}", t)));
        };
        if !matches!(cls.kind, Kind::Type(_)) {
            let t = it.type_name_of(arraytype);
            return Err(it.type_error(&format!("first argument must be a type object, not {}", t)));
        }
        let base = type_object::<Array>(it);
        if !it.is_subtype(cls, &base) {
            let n = it.type_name(cls);
            return Err(it.type_error(&format!("{} is not a subtype of array.array", n)));
        }
        let tc = typecode_of(it, typecode, "_array_reconstructor() argument 2")?;
        let Some(mut spec) = Spec::of(tc) else { return Err(it.value_error("second argument must be a valid type code")) };
        if !(0..=21).contains(&mformat_code) {
            return Err(it.value_error("third argument must be a valid machine format code."));
        }
        let data = match items {
            Value::Obj(o) => match &o.kind {
                Kind::Bytes(b) => b.clone(),
                _ => {
                    let t = it.type_name_of(items);
                    return Err(it.type_error(&format!("fourth argument should be bytes, not {}", t)));
                }
            },
            _ => {
                let t = it.type_name_of(items);
                return Err(it.type_error(&format!("fourth argument should be bytes, not {}", t)));
            }
        };
        let arr = Array::with(spec, Vec::new());
        if mformat_code == spec.mformat() {
            from_bytes(it, spec, &arr.store, &data)?;
            return Ok(opaque_instance(cls, arr));
        }
        let size = [1, 1, 2, 2, 2, 2, 4, 4, 4, 4, 8, 8, 8, 8, 4, 4, 8, 8, 2, 2, 4, 4][mformat_code as usize];
        if data.len() % size != 0 {
            return Err(it.value_error("string length not a multiple of item size"));
        }
        let big = mformat_code % 2 == 1 && mformat_code > 1;
        let order = if big { ByteOrder::Big } else { ByteOrder::Little };
        let converted = match mformat_code {
            14..=17 => {
                let kind = if size == 4 { ElemKind::F32 } else { ElemKind::F64 };
                Value::list(data.chunks_exact(size).map(|c| Value::Float(lumen_common::buffer::load_f64(kind, c, order))).collect())
            }
            18..=21 => {
                let enc = match mformat_code {
                    18 => "utf-16-le",
                    19 => "utf-16-be",
                    20 => "utf-32-le",
                    _ => "utf-32-be",
                };
                it.call_method(items, "decode", vec![Value::str(enc)])?
            }
            _ => {
                let signed = mformat_code == 1 || (mformat_code >= 2 && (mformat_code - 2) % 4 >= 2);
                let kind = lumen_common::buffer::format::int_kind(size, signed);
                for c in TYPECODES.bytes() {
                    if let Some(s) = Spec::of(c) {
                        let s_signed = matches!(c, b'b' | b'h' | b'i' | b'l' | b'q');
                        if c != b'u' && !s.is_float() && s.size == size && s_signed == signed {
                            spec = s;
                        }
                    }
                }
                Value::list(data.chunks_exact(size).map(|c| scalar_value(load(kind, c, order))).collect())
            }
        };
        let arr = Array::with(spec, Vec::new());
        initialize(it, spec, &arr.store, &converted)?;
        Ok(opaque_instance(cls, arr))
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) -> R<()> {
        let Value::Obj(m) = m else { return Ok(()) };
        let d = it.module_dict(m);
        let ty = Value::Obj(type_object::<Array>(it));
        dict_set_str(&d, "ArrayType", ty.clone());
        dict_set_str(&d, "typecodes", Value::str(TYPECODES));
        let abc = Value::Obj(it.import_module("collections.abc")?);
        let seq = it.get_attr_str(&abc, "MutableSequence")?;
        it.call_method(&seq, "register", vec![ty])?;
        Ok(())
    }
}
