//! `marshal`: CPython's value serialization format (version 5) for the data types. Code objects
//! are not supported, since lumen-py's bytecode is not CPython's. Writing never emits
//! back-references (CPython's depend on reference counts); reading accepts them. Set elements
//! are written in the order of their encodings, as CPython does, so output is reproducible.

/// This module contains functions that can read and write Python values in
/// a binary format. The format is specific to Python, but independent of
/// machine architecture issues.
///
/// Not all Python object types are supported; in general, only objects
/// whose value is independent from a particular invocation of Python can be
/// written and read by this module. The following types are supported:
/// None, integers, floating-point numbers, strings, bytes, bytearrays,
/// tuples, lists, sets, dictionaries, and code objects, where it
/// should be understood that tuples, lists and dictionaries are only
/// supported as long as the values contained therein are themselves
/// supported; and recursive lists and dictionaries should not be written
/// (they will cause infinite loops).
///
/// Variables:
///
/// version -- indicates the format that the module uses. Version 0 is the
///     historical format, version 1 shares interned strings and version 2
///     uses a binary format for floating-point numbers.
///     Version 3 shares common object references (New in version 3.4).
///     Version 4 supports small tuples and short strings (New in version 3.4).
///     Version 5 supports slice objects (New in version 3.14).
///
/// Functions:
///
/// dump() -- write value to a file
/// load() -- read value from a file
/// dumps() -- marshal value as a bytes object
/// loads() -- read value from a bytes-like object
#[lumen_bind::module(name = "marshal")]
pub mod marshal {
    use crate::object::*;
    use crate::pyint::BigInt;
    use crate::vm::{dict_set_str, Interp};
    use std::cell::RefCell;
    use std::collections::HashMap;

    const VERSION: i64 = 5;
    const MAX_DEPTH: usize = 2000;
    const FLAG_REF: u8 = 0x80;

    struct Writer {
        out: Vec<u8>,
        version: i64,
        depth: usize,
        /// Mutable containers written so far and their reference index (version 3 and up), so
        /// shared and recursive containers are written once and referred to afterwards.
        seen: HashMap<*const Object, i32>,
    }

    fn unmarshallable(it: &mut Interp) -> Obj {
        it.value_error("unmarshallable object")
    }

    impl Writer {
        fn long(&mut self, n: i32) {
            self.out.extend_from_slice(&n.to_le_bytes());
        }

        fn sized(&mut self, it: &mut Interp, code: u8, data: &[u8]) -> R<()> {
            let Ok(n) = i32::try_from(data.len()) else {
                return Err(unmarshallable(it));
            };
            self.out.push(code);
            self.long(n);
            self.out.extend_from_slice(data);
            Ok(())
        }

        fn float(&mut self, f: f64) {
            if self.version > 1 {
                self.out.push(b'g');
                self.out.extend_from_slice(&f.to_le_bytes());
            } else {
                let s = crate::num::float_repr(f);
                self.out.push(b'f');
                self.out.push(s.len() as u8);
                self.out.extend_from_slice(s.as_bytes());
            }
        }

        /// `w_PyLong`: 15-bit digits, least significant first, signed count.
        fn big(&mut self, b: &BigInt) {
            let (neg, words) = b.words();
            let mut digits: Vec<u16> = Vec::new();
            let bits = words.len() * 64;
            let mut at = 0;
            while at < bits {
                let (w, o) = (at / 64, at % 64);
                let mut d = words[w] >> o;
                if o > 49 && w + 1 < words.len() {
                    d |= words[w + 1] << (64 - o);
                }
                digits.push((d & 0x7fff) as u16);
                at += 15;
            }
            while digits.last() == Some(&0) {
                digits.pop();
            }
            self.out.push(b'l');
            let n = digits.len() as i32;
            self.long(if neg { -n } else { n });
            for d in digits {
                self.out.extend_from_slice(&d.to_le_bytes());
            }
        }

        /// Starts a mutable container: writes a back-reference and returns true if `o` was
        /// written before, else its type byte with `FLAG_REF` (version 3 and up).
        fn start(&mut self, o: &Obj, code: u8) -> bool {
            if self.version < 3 {
                self.out.push(code);
                return false;
            }
            let key = std::rc::Rc::as_ptr(o);
            if let Some(&i) = self.seen.get(&key) {
                self.out.push(b'r');
                self.long(i);
                return true;
            }
            let i = self.seen.len() as i32;
            self.seen.insert(key, i);
            self.out.push(code | FLAG_REF);
            false
        }

        fn seq(&mut self, it: &mut Interp, o: Option<&Obj>, code: u8, items: &[Value]) -> R<()> {
            let Ok(n) = i32::try_from(items.len()) else {
                return Err(unmarshallable(it));
            };
            match o {
                Some(o) => {
                    if self.start(o, code) {
                        return Ok(());
                    }
                }
                None => self.out.push(code),
            }
            self.long(n);
            for v in items {
                self.value(it, v)?;
            }
            Ok(())
        }

        fn value(&mut self, it: &mut Interp, v: &Value) -> R<()> {
            self.depth += 1;
            if self.depth > MAX_DEPTH {
                self.depth -= 1;
                return Err(it.value_error("object too deeply nested to marshal"));
            }
            let r = self.value_inner(it, v);
            self.depth -= 1;
            r
        }

        fn value_inner(&mut self, it: &mut Interp, v: &Value) -> R<()> {
            match v {
                Value::None => self.out.push(b'N'),
                Value::Ellipsis => self.out.push(b'.'),
                Value::Bool(false) => self.out.push(b'F'),
                Value::Bool(true) => self.out.push(b'T'),
                Value::Int(i) => match i32::try_from(*i) {
                    Ok(n) => {
                        self.out.push(b'i');
                        self.long(n);
                    }
                    Err(_) => self.big(&BigInt::from_i64(*i)),
                },
                Value::Float(f) => self.float(*f),
                Value::NotImplemented => return Err(unmarshallable(it)),
                Value::Obj(o) => {
                    let stop = it.exc_type("StopIteration");
                    if std::rc::Rc::ptr_eq(o, &stop) {
                        self.out.push(b'S');
                        return Ok(());
                    }
                    if o.cls.is_some() {
                        return self.buffer(it, v);
                    }
                    match &o.kind {
                        Kind::Int(b) => match b.to_i64().and_then(|i| i32::try_from(i).ok()) {
                            Some(n) => {
                                self.out.push(b'i');
                                self.long(n);
                            }
                            None => self.big(b),
                        },
                        Kind::Float(f) => self.float(*f),
                        Kind::Complex(re, im) => {
                            if self.version > 1 {
                                self.out.push(b'y');
                                self.out.extend_from_slice(&re.to_le_bytes());
                                self.out.extend_from_slice(&im.to_le_bytes());
                            } else {
                                self.out.push(b'x');
                                for f in [re, im] {
                                    let s = crate::num::float_repr(*f);
                                    self.out.push(s.len() as u8);
                                    self.out.extend_from_slice(s.as_bytes());
                                }
                            }
                        }
                        Kind::Str(s) => {
                            if self.version >= 4 && s.ascii {
                                let b = s.s.as_bytes();
                                if b.len() < 256 {
                                    self.out.push(b'z');
                                    self.out.push(b.len() as u8);
                                    self.out.extend_from_slice(b);
                                } else {
                                    self.sized(it, b'a', b)?;
                                }
                            } else {
                                let bytes = crate::codecs::utf8_encode(it, &s.s, "surrogatepass")?;
                                self.sized(it, b'u', &bytes)?;
                            }
                        }
                        Kind::Tuple(items) => {
                            if self.version >= 4 && items.len() < 256 {
                                self.out.push(b')');
                                self.out.push(items.len() as u8);
                                for x in items {
                                    self.value(it, x)?;
                                }
                            } else {
                                self.seq(it, None, b'(', items)?;
                            }
                        }
                        Kind::List(l) => {
                            let items = l.borrow().clone();
                            self.seq(it, Some(o), b'[', &items)?;
                        }
                        Kind::Dict(d) => {
                            let pairs: Vec<(Value, Value)> = d
                                .borrow()
                                .iter()
                                .map(|e| (e.key.clone(), e.val.clone()))
                                .collect();
                            if self.start(o, b'{') {
                                return Ok(());
                            }
                            for (k, x) in pairs {
                                self.value(it, &k)?;
                                self.value(it, &x)?;
                            }
                            self.out.push(b'0');
                        }
                        Kind::Set(d) | Kind::FrozenSet(d) => {
                            let code = if matches!(o.kind, Kind::Set(_)) {
                                b'<'
                            } else {
                                b'>'
                            };
                            let items = d.borrow().keys();
                            let Ok(n) = i32::try_from(items.len()) else {
                                return Err(unmarshallable(it));
                            };
                            let mut encoded = Vec::with_capacity(items.len());
                            for x in &items {
                                let mut w = Writer {
                                    out: Vec::new(),
                                    version: self.version,
                                    depth: self.depth,
                                    seen: HashMap::new(),
                                };
                                w.value(it, x)?;
                                encoded.push(w.out);
                            }
                            encoded.sort();
                            if code == b'<' {
                                if self.start(o, code) {
                                    return Ok(());
                                }
                            } else {
                                self.out.push(code);
                            }
                            self.long(n);
                            for e in encoded {
                                self.out.extend_from_slice(&e);
                            }
                        }
                        Kind::Slice(a, b, c) => {
                            if self.version < 5 {
                                return Err(unmarshallable(it));
                            }
                            self.out.push(b':');
                            for x in [a, b, c] {
                                self.value(it, x)?;
                            }
                        }
                        _ => return self.buffer(it, v),
                    }
                }
            }
            Ok(())
        }

        /// Any other object exporting a buffer is written as `bytes`.
        fn buffer(&mut self, it: &mut Interp, v: &Value) -> R<()> {
            match crate::builtins::memview::contiguous_bytes(it, v)? {
                Some(b) => self.sized(it, b's', &b),
                None => Err(unmarshallable(it)),
            }
        }
    }

    struct Reader<'a> {
        data: &'a [u8],
        pos: usize,
        refs: Vec<Option<Value>>,
        depth: usize,
        allow_code: bool,
        file: bool,
    }

    fn bad(it: &mut Interp, what: &str) -> Obj {
        it.value_error(&format!("bad marshal data ({what})"))
    }

    fn eof(it: &mut Interp, file: bool) -> Obj {
        let msg = if file {
            "EOF read where not expected"
        } else {
            "marshal data too short"
        };
        it.new_exc_str("EOFError", msg)
    }

    impl Reader<'_> {
        fn bytes(&mut self, it: &mut Interp, n: usize) -> R<&[u8]> {
            if self.data.len() - self.pos < n {
                return Err(eof(it, self.file));
            }
            let s = &self.data[self.pos..self.pos + n];
            self.pos += n;
            Ok(s)
        }

        fn byte(&mut self, it: &mut Interp) -> R<u8> {
            match self.data.get(self.pos) {
                Some(&b) => {
                    self.pos += 1;
                    Ok(b)
                }
                None => Err(it.new_exc_str("EOFError", "EOF read where not expected")),
            }
        }

        fn long(&mut self, it: &mut Interp) -> R<i32> {
            let b = self.bytes(it, 4)?;
            Ok(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        }

        fn size(&mut self, it: &mut Interp, what: &str) -> R<usize> {
            let n = self.long(it)?;
            usize::try_from(n).map_err(|_| bad(it, &format!("{what} size out of range")))
        }

        fn f64(&mut self, it: &mut Interp) -> R<f64> {
            let b = self.bytes(it, 8)?;
            Ok(f64::from_le_bytes([
                b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
            ]))
        }

        fn text_float(&mut self, it: &mut Interp) -> R<f64> {
            let n = self.byte(it)? as usize;
            let s = String::from_utf8_lossy(self.bytes(it, n)?).into_owned();
            s.trim()
                .parse::<f64>()
                .map_err(|_| it.value_error(&format!("could not convert string to float: '{s}'")))
        }

        fn text(&mut self, it: &mut Interp, n: usize) -> R<Value> {
            let b = self.bytes(it, n)?.to_vec();
            let (s, _) = crate::codecs::utf8_decode(it, &b, "surrogatepass", true)?;
            Ok(Value::string(s))
        }

        fn object(&mut self, it: &mut Interp) -> R<Value> {
            self.depth += 1;
            if self.depth > MAX_DEPTH {
                self.depth -= 1;
                return Err(it.value_error("recursion limit exceeded"));
            }
            let r = self.object_inner(it);
            self.depth -= 1;
            r
        }

        fn object_inner(&mut self, it: &mut Interp) -> R<Value> {
            if self.pos >= self.data.len() {
                return Err(it.new_exc_str("EOFError", "EOF read where object expected"));
            }
            let raw = self.byte(it)?;
            let flag = raw & FLAG_REF != 0;
            let code = raw & !FLAG_REF;
            let slot = if flag {
                self.refs.push(None);
                Some(self.refs.len() - 1)
            } else {
                None
            };
            let v = match code {
                b'0' => return Err(it.type_error("NULL object in marshal data for object")),
                b'N' => Value::None,
                b'F' => Value::Bool(false),
                b'T' => Value::Bool(true),
                b'.' => Value::Ellipsis,
                b'S' => Value::Obj(it.exc_type("StopIteration")),
                b'i' => Value::Int(self.long(it)? as i64),
                b'I' => {
                    let b = self.bytes(it, 8)?;
                    Value::Int(i64::from_le_bytes([
                        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
                    ]))
                }
                b'l' => {
                    let n = self.long(it)?;
                    let count = n.unsigned_abs() as usize;
                    let mut words: Vec<u64> = vec![0; (count * 15).div_ceil(64)];
                    for i in 0..count {
                        let b = self.bytes(it, 2)?;
                        let d = u16::from_le_bytes([b[0], b[1]]);
                        if d > 0x7fff {
                            return Err(bad(it, "digit out of range in long"));
                        }
                        if i + 1 == count && d == 0 {
                            return Err(bad(it, "unnormalized long data"));
                        }
                        let at = i * 15;
                        let (w, o) = (at / 64, at % 64);
                        words[w] |= (d as u64) << o;
                        if o > 49 {
                            words[w + 1] |= (d as u64) >> (64 - o);
                        }
                    }
                    Value::big(BigInt::from_words(n < 0, words))
                }
                b'g' => Value::Float(self.f64(it)?),
                b'f' => Value::Float(self.text_float(it)?),
                b'y' => {
                    let (re, im) = (self.f64(it)?, self.f64(it)?);
                    Value::Obj(Object::new(Kind::Complex(re, im)))
                }
                b'x' => {
                    let (re, im) = (self.text_float(it)?, self.text_float(it)?);
                    Value::Obj(Object::new(Kind::Complex(re, im)))
                }
                b's' => {
                    let n = self.size(it, "bytes object")?;
                    Value::bytes(self.bytes(it, n)?.to_vec())
                }
                b'u' | b't' => {
                    let n = self.size(it, "string")?;
                    self.text(it, n)?
                }
                b'a' | b'A' => {
                    let n = self.size(it, "string")?;
                    self.text(it, n)?
                }
                b'z' | b'Z' => {
                    let n = self.byte(it)? as usize;
                    self.text(it, n)?
                }
                b')' => {
                    let n = self.byte(it)? as usize;
                    self.tuple(it, n)?
                }
                b'(' => {
                    let n = self.size(it, "tuple")?;
                    self.tuple(it, n)?
                }
                b'[' => {
                    let n = self.size(it, "list")?;
                    let list =
                        Object::new(Kind::List(RefCell::new(Vec::with_capacity(n.min(1 << 16)))));
                    if let Some(s) = slot {
                        self.refs[s] = Some(Value::Obj(list.clone()));
                    }
                    for _ in 0..n {
                        let x = self.object(it)?;
                        if let Kind::List(l) = &list.kind {
                            l.borrow_mut().push(x);
                        }
                    }
                    return Ok(Value::Obj(list));
                }
                b'{' => {
                    let d = it.new_dict();
                    if let Some(s) = slot {
                        self.refs[s] = Some(Value::Obj(d.clone()));
                    }
                    let dv = Value::Obj(d);
                    loop {
                        if self.data.get(self.pos) == Some(&b'0') {
                            self.pos += 1;
                            break;
                        }
                        let k = self.object(it)?;
                        let x = self.object(it)?;
                        it.setitem(&dv, k, x)?;
                    }
                    return Ok(dv);
                }
                b'<' if slot.is_some() => {
                    let n = self.size(it, "set")?;
                    let set = it.new_set(Vec::new())?;
                    if let Some(s) = slot {
                        self.refs[s] = Some(set.clone());
                    }
                    for _ in 0..n {
                        let x = self.object(it)?;
                        it.call_method(&set, "add", vec![x])?;
                    }
                    return Ok(set);
                }
                b'<' | b'>' => {
                    let n = self.size(it, "set")?;
                    let mut items = Vec::with_capacity(n.min(1 << 16));
                    for _ in 0..n {
                        items.push(self.object(it)?);
                    }
                    if code == b'<' {
                        it.new_set(items)?
                    } else {
                        it.new_frozenset_from(items)?
                    }
                }
                b':' => {
                    let a = self.object(it)?;
                    let b = self.object(it)?;
                    let c = self.object(it)?;
                    Value::Obj(Object::new(Kind::Slice(a, b, c)))
                }
                b'r' => {
                    let n = self.long(it)?;
                    return match usize::try_from(n).ok().and_then(|i| self.refs.get(i)) {
                        Some(Some(v)) => Ok(v.clone()),
                        _ => Err(bad(it, "invalid reference")),
                    };
                }
                b'c' if !self.allow_code => {
                    return Err(it.value_error("unmarshalling code objects is disallowed"));
                }
                b'c' => return Err(bad(it, "code objects are not supported")),
                _ => return Err(bad(it, "unknown type code")),
            };
            if let Some(s) = slot {
                self.refs[s] = Some(v.clone());
            }
            Ok(v)
        }

        fn tuple(&mut self, it: &mut Interp, n: usize) -> R<Value> {
            let mut items = Vec::with_capacity(n.min(1 << 16));
            for _ in 0..n {
                items.push(self.object(it)?);
            }
            Ok(Value::tuple(items))
        }
    }

    fn dump_bytes(it: &mut Interp, value: &Value, version: i64) -> R<Vec<u8>> {
        let mut w = Writer {
            out: Vec::new(),
            version,
            depth: 0,
            seen: HashMap::new(),
        };
        w.value(it, value)?;
        Ok(w.out)
    }

    fn load_bytes(it: &mut Interp, data: &[u8], allow_code: bool, file: bool) -> R<(Value, usize)> {
        let mut r = Reader {
            data,
            pos: 0,
            refs: Vec::new(),
            depth: 0,
            allow_code,
            file,
        };
        let v = r.object(it)?;
        Ok((v, r.pos))
    }

    /// Return the bytes object that would be written to a file by dump(value, file).
    ///
    ///   value
    ///     Must be a supported type.
    ///   version
    ///     Indicates the data format that dumps should use.
    ///
    /// Raise a ValueError exception if value has (or contains an object that has) an
    /// unsupported type.
    ///   allow_code
    ///     Allow to write code objects.
    #[op(hint(py(text_signature = "($module, value, version=version, /, *, allow_code=True)")))]
    fn dumps(
        it: &mut Interp,
        value: &Value,
        #[default(5)] version: i64,
        #[kwonly]
        #[default(true)]
        allow_code: bool,
    ) -> R<Value> {
        let _ = allow_code; // code objects are unmarshallable either way
        Ok(Value::bytes(dump_bytes(it, value, version)?))
    }

    /// Write the value on the open file.
    ///
    ///   value
    ///     Must be a supported type.
    ///   file
    ///     Must be a writeable binary file.
    ///   version
    ///     Indicates the data format that dump should use.
    ///   allow_code
    ///     Allow to write code objects.
    ///
    /// If the value has (or contains an object that has) an unsupported type, a
    /// ValueError exception is raised - but garbage data will also be written
    /// to the file. The object will not be properly read back by load().
    #[op(hint(py(
        text_signature = "($module, value, file, version=version, /, *, allow_code=True)"
    )))]
    fn dump(
        it: &mut Interp,
        value: &Value,
        file: &Value,
        #[default(5)] version: i64,
        #[kwonly]
        #[default(true)]
        allow_code: bool,
    ) -> R<()> {
        let _ = allow_code;
        let b = dump_bytes(it, value, version)?;
        it.call_method(file, "write", vec![Value::bytes(b)])?;
        Ok(())
    }

    /// Convert the bytes-like object to a value.
    ///
    /// If no valid value is found, raise EOFError, ValueError or TypeError.  Extra
    /// bytes in the input are ignored.
    ///   allow_code
    ///     Allow to load code objects.
    #[op(hint(py(text_signature = "($module, bytes, /, *, allow_code=True)")))]
    fn loads(
        it: &mut Interp,
        data: &Value,
        #[kwonly]
        #[default(true)]
        allow_code: bool,
    ) -> R<Value> {
        let Some(b) = crate::builtins::memview::contiguous_bytes(it, data)? else {
            let t = it.type_name_of(data);
            return Err(it.type_error(&format!("a bytes-like object is required, not '{t}'")));
        };
        Ok(load_bytes(it, &b, allow_code, false)?.0)
    }

    /// Read one value from the open file and return it.
    ///
    ///   file
    ///     Must be readable binary file.
    ///   allow_code
    ///     Allow to load code objects.
    ///
    /// If no valid value is read (e.g. because the data has a different Python
    /// version's incompatible marshal format), raise EOFError, ValueError or
    /// TypeError.
    ///
    /// Note: If an object containing an unsupported type was marshalled with
    /// dump(), load() will substitute None for the unmarshallable type.
    #[op(hint(py(text_signature = "($module, file, /, *, allow_code=True)")))]
    fn load(
        it: &mut Interp,
        file: &Value,
        #[kwonly]
        #[default(true)]
        allow_code: bool,
    ) -> R<Value> {
        let start = it.call_method(file, "tell", Vec::new())?;
        let rest = it.call_method(file, "read", Vec::new())?;
        let Some(b) = crate::builtins::memview::contiguous_bytes(it, &rest)? else {
            let t = it.type_name_of(&rest);
            return Err(it.type_error(&format!("file.read() returned not bytes but {t}")));
        };
        let (v, used) = load_bytes(it, &b, allow_code, true)?;
        let start = it.index_of(&start)?;
        it.call_method(file, "seek", vec![Value::Int(start + used as i64)])?;
        Ok(v)
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        dict_set_str(&d, "version", Value::Int(VERSION));
    }
}
