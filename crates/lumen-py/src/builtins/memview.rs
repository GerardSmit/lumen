//! `memoryview`: shaped, typed views over the bytes of `bytes` and `bytearray`, a facade over
//! `lumen_common::buffer`. A view of a `bytearray` holds an [`Export`] of its store, so the
//! bytearray cannot be resized while the view lives; element access goes through
//! [`ViewDesc`] and the shared element codecs in `lumen_common::buffer::format`.

use crate::bind::{buffer_error, Py, This};
use crate::object::*;
use crate::pyint::BigInt;
use crate::vm::{dict_set_str, Interp};
use lumen_common::buffer::{
    adjust_slice, load, store, struct_code, BufferError, ByteOrder, CastError, ElemKind, Export, PackError, Scalar, StructMode,
    ViewDesc,
};
use std::rc::Rc;

/// The bytes a view (or another buffer consumer) reads: an immutable `bytes` object, or an
/// exported store.
pub enum Source {
    Bytes(Obj),
    Store(Export),
}

impl Source {
    /// Runs `f` on the source's bytes.
    pub fn with<T>(&self, f: impl FnOnce(&[u8]) -> T) -> Result<T, BufferError> {
        match self {
            Source::Bytes(o) => match &o.kind {
                Kind::Bytes(b) => Ok(f(b)),
                _ => Err(BufferError::Detached),
            },
            Source::Store(e) => e.store().try_bytes().map(|b| f(&b)),
        }
    }

    /// Runs `f` on the source's bytes for an in-place write.
    pub fn with_mut<T>(&self, f: impl FnOnce(&mut [u8]) -> T) -> Result<T, BufferError> {
        match self {
            Source::Bytes(_) => Err(BufferError::ReadOnly),
            Source::Store(e) => e.store().try_bytes_mut().map(|mut b| f(&mut b)),
        }
    }

    fn reexport(&self) -> Result<Source, BufferError> {
        Ok(match self {
            Source::Bytes(o) => Source::Bytes(o.clone()),
            Source::Store(e) => Source::Store(e.store().export()?),
        })
    }
}

/// A buffer exported from a bytes-like object: its source, the view, the exporting object and
/// the view's format. `None` when `v` does not support the buffer protocol.
pub struct Exported {
    pub src: Source,
    pub view: ViewDesc,
    pub obj: Value,
    pub fmt: Rc<str>,
}

pub fn export(it: &mut Interp, v: &Value) -> R<Option<Exported>> {
    let Value::Obj(o) = v else { return Ok(None) };
    match &o.kind {
        Kind::Bytes(b) => Ok(Some(Exported {
            src: Source::Bytes(o.clone()),
            view: ViewDesc::bytes(0, b.len(), true),
            obj: v.clone(),
            fmt: "B".into(),
        })),
        Kind::ByteArray(s) => {
            let e = s.export().map_err(|e| buffer_error(it, e))?;
            Ok(Some(Exported { view: ViewDesc::bytes(0, s.len(), false), src: Source::Store(e), obj: v.clone(), fmt: "B".into() }))
        }
        Kind::Opaque(_) => match Py::<MemoryView>::from_value(it, v) {
            Some(p) => {
                let m = p.borrow(it)?;
                let Some(src) = &m.src else { return Err(released(it)) };
                let src = src.reexport().map_err(|e| buffer_error(it, e))?;
                Ok(Some(Exported { src, view: m.view.clone(), obj: m.obj.clone(), fmt: m.fmt.clone() }))
            }
            None => match super::arraym::array::parts(it, v) {
                Some((spec, store)) => {
                    let e = store.export().map_err(|e| buffer_error(it, e))?;
                    let n = store.len() / spec.size;
                    let view = ViewDesc::contiguous(0, Some(spec.kind), spec.size, ByteOrder::NATIVE, vec![n], false);
                    Ok(Some(Exported { src: Source::Store(e), view, obj: v.clone(), fmt: spec.format().into() }))
                }
                None => match super::mmapm::mmap::buffer_of(it, v)? {
                    Some((store, readonly)) => {
                        let e = store.export().map_err(|e| buffer_error(it, e))?;
                        let view = ViewDesc::bytes(0, store.len(), readonly);
                        Ok(Some(Exported { src: Source::Store(e), view, obj: v.clone(), fmt: "B".into() }))
                    }
                    None => Ok(None),
                },
            },
        },
        _ => Ok(None),
    }
}

/// Runs `f` on the bytes of the writable, C-contiguous buffer `v`; `None` when `v` is not one.
pub fn with_writable<T>(it: &mut Interp, v: &Value, f: impl FnOnce(&mut [u8]) -> T) -> R<Option<T>> {
    let Some(e) = export(it, v)? else { return Ok(None) };
    if e.view.readonly || !e.view.is_c_contiguous() {
        return Ok(None);
    }
    let (offset, n) = (e.view.offset, e.view.nbytes());
    e.src.with_mut(|b| f(&mut b[offset..offset + n])).map(Some).map_err(|e| inaccessible(it, e))
}

/// A writable `memoryview` of `store`, exported by `obj`.
pub fn view_of_store(it: &mut Interp, obj: Value, store: &Rc<lumen_common::buffer::ByteStore>) -> R<Value> {
    let e = store.export().map_err(|e| buffer_error(it, e))?;
    let view = ViewDesc::bytes(0, store.len(), false);
    Ok(Py::new(it, MemoryView { obj, src: Some(Source::Store(e)), view, fmt: "B".into() }).into_value())
}

/// The bytes of any bytes-like object, in C order (`bytes(x)` for a buffer).
pub fn contiguous_bytes(it: &mut Interp, v: &Value) -> R<Option<Vec<u8>>> {
    let Some(e) = export(it, v)? else { return Ok(None) };
    gather(it, &e.src, &e.view).map(Some)
}

fn released(it: &mut Interp) -> Obj {
    it.value_error("operation forbidden on released memoryview object")
}

fn inaccessible(it: &mut Interp, e: BufferError) -> Obj {
    match e {
        BufferError::OutOfBounds | BufferError::Detached => it.new_exc_str("BufferError", "memoryview: underlying buffer is not accessible"),
        e => buffer_error(it, e),
    }
}

fn gather(it: &mut Interp, src: &Source, view: &ViewDesc) -> R<Vec<u8>> {
    src.with(|b| view.check(b.len()).map(|()| view.gather(b))).and_then(|r| r).map_err(|e| inaccessible(it, e))
}

/// The element kind of a native single-character format (`"B"`, `"@i"`), as memoryview accepts.
fn parse_fmt(fmt: &str) -> Option<(ElemKind, usize)> {
    let b = fmt.strip_prefix('@').unwrap_or(fmt).as_bytes();
    if b.len() != 1 {
        return None;
    }
    let c = struct_code(b[0], StructMode::Native)?;
    Some((c.kind?, c.size))
}

fn is_byte_fmt(fmt: &str) -> bool {
    matches!(parse_fmt(fmt), Some((ElemKind::U8 | ElemKind::I8 | ElemKind::Char, _)))
}

/// A decoded element as a Python value (shared with `_struct`).
pub fn scalar_value(s: Scalar) -> Value {
    match s {
        Scalar::Int(n) => match i64::try_from(n) {
            Ok(i) => Value::Int(i),
            Err(_) => Value::big(BigInt::from_i128(n)),
        },
        Scalar::Float(x) => Value::Float(x),
        Scalar::Bool(b) => Value::Bool(b),
        Scalar::Char(c) => Value::bytes(vec![c]),
    }
}

/// The value of an object with `__index__` as an `i128`; `None` when it does not fit.
pub fn index_i128(it: &mut Interp, v: &Value) -> R<Option<i128>> {
    match v {
        Value::Int(i) => Ok(Some(*i as i128)),
        Value::Bool(b) => Ok(Some(*b as i128)),
        _ => {
            if let Some(b) = v.as_bigint() {
                return Ok(b.to_i128());
            }
            let n = it.call_special(v, "__index__", Vec::new())?;
            match (n.as_i64(), n.as_bigint()) {
                (Some(i), _) => Ok(Some(i as i128)),
                (None, Some(b)) => Ok(b.to_i128()),
                _ => {
                    let t = it.type_name_of(&n);
                    Err(it.type_error(&format!("__index__ returned non-int (type {})", t)))
                }
            }
        }
    }
}

fn pack_item(it: &mut Interp, fmt: &str, kind: ElemKind, v: &Value, out: &mut [u8]) -> R<()> {
    let bad_type = |it: &mut Interp| it.type_error(&format!("memoryview: invalid type for format '{}'", fmt));
    let bad_value = |it: &mut Interp| it.value_error(&format!("memoryview: invalid value for format '{}'", fmt));
    let scalar = match kind {
        ElemKind::Char => match v {
            Value::Obj(o) => match &o.kind {
                Kind::Bytes(b) if b.len() == 1 => Scalar::Char(b[0]),
                Kind::Bytes(_) => return Err(bad_value(it)),
                _ => return Err(bad_type(it)),
            },
            _ => return Err(bad_type(it)),
        },
        ElemKind::Bool => Scalar::Bool(it.truthy(v)?),
        k if k.is_float() => {
            let is_num = matches!(v, Value::Float(_) | Value::Int(_) | Value::Bool(_))
                || matches!(v, Value::Obj(o) if matches!(o.kind, Kind::Float(_) | Kind::Int(_)));
            if !is_num {
                return Err(bad_type(it));
            }
            let x = it.float_arg(v)?;
            if k == ElemKind::F32 {
                // A C cast, as CPython's memoryview does: no overflow check.
                lumen_common::buffer::store_f64(k, x, out, ByteOrder::NATIVE);
                return Ok(());
            }
            Scalar::Float(x)
        }
        _ => {
            if !it.has_index(v) {
                return Err(bad_type(it));
            }
            match index_i128(it, v)? {
                Some(n) => Scalar::Int(n),
                None => return Err(bad_value(it)),
            }
        }
    };
    match store(kind, scalar, out, ByteOrder::NATIVE) {
        Ok(()) => Ok(()),
        Err(PackError::WrongType) => Err(bad_type(it)),
        Err(_) => Err(bad_value(it)),
    }
}

/// CPython's `PyBUF_MAX_NDIM`.
const MAX_NDIM: usize = 64;

#[lumen_bind::class(name = "memoryview", module = "builtins")]
pub struct MemoryView {
    obj: Value,
    /// `None` once released.
    src: Option<Source>,
    view: ViewDesc,
    fmt: Rc<str>,
}

enum Key {
    Elem(usize),
    View(ViewDesc),
    Whole,
}

impl MemoryView {
    fn of(it: &mut Interp, v: &Value) -> R<MemoryView> {
        match export(it, v)? {
            Some(e) => Ok(MemoryView { obj: e.obj, src: Some(e.src), view: e.view, fmt: e.fmt }),
            None => {
                let t = it.type_name_of(v);
                Err(it.type_error(&format!("memoryview: a bytes-like object is required, not '{}'", t)))
            }
        }
    }

    fn src(&self, it: &mut Interp) -> R<&Source> {
        match &self.src {
            Some(s) => Ok(s),
            None => Err(released(it)),
        }
    }

    /// A new view of the same source.
    fn derive(&self, it: &mut Interp, view: ViewDesc, fmt: Rc<str>) -> R<Value> {
        let src = self.src(it)?.reexport().map_err(|e| buffer_error(it, e))?;
        Ok(Py::new(it, MemoryView { obj: self.obj.clone(), src: Some(src), view, fmt }).into_value())
    }

    fn kind(&self, it: &mut Interp) -> R<ElemKind> {
        match parse_fmt(&self.fmt) {
            Some((k, _)) => Ok(k),
            None => Err(it.new_exc_str("NotImplementedError", &format!("memoryview: unsupported format {}", self.fmt))),
        }
    }

    fn element(&self, it: &mut Interp, pos: usize) -> R<Value> {
        let kind = self.kind(it)?;
        let size = self.view.itemsize;
        let src = self.src(it)?;
        match src.with(|b| b.get(pos..pos + size).map(|s| load(kind, s, ByteOrder::NATIVE))) {
            Ok(Some(s)) => Ok(scalar_value(s)),
            Ok(None) => Err(it.new_exc_str("IndexError", "index out of bounds on dimension 1")),
            Err(e) => Err(inaccessible(it, e)),
        }
    }

    fn bytes(&self, it: &mut Interp) -> R<Vec<u8>> {
        let src = self.src(it)?;
        gather(it, src, &self.view)
    }

    fn values(&self, it: &mut Interp) -> R<Vec<Value>> {
        let kind = self.kind(it)?;
        let data = self.bytes(it)?;
        Ok(data.chunks(self.view.itemsize.max(1)).map(|c| scalar_value(load(kind, c, ByteOrder::NATIVE))).collect())
    }

    fn index_pos(&self, it: &mut Interp, dim: usize, key: &Value) -> R<usize> {
        let i = it.seq_index(key)?;
        match self.view.index(dim, i as isize) {
            Some(j) => Ok(j),
            None => Err(it.new_exc_str("IndexError", &format!("index out of bounds on dimension {}", dim + 1))),
        }
    }

    fn resolve(&self, it: &mut Interp, key: &Value) -> R<Key> {
        let v = &self.view;
        if matches!(key, Value::Ellipsis) {
            return Ok(Key::Whole);
        }
        if let Some(items) = key.tuple_items() {
            if items.is_empty() && v.ndim() == 0 {
                return Ok(Key::Elem(v.offset));
            }
            if items.iter().all(|k| it.has_index(k)) {
                if items.len() < v.ndim() {
                    return Err(it.new_exc_str("NotImplementedError", "sub-views are not implemented"));
                }
                if items.len() > v.ndim() {
                    return Err(it.type_error(&format!("cannot index {}-dimension view with {}-element tuple", v.ndim(), items.len())));
                }
                let mut idx = Vec::with_capacity(items.len());
                for (d, k) in items.iter().enumerate() {
                    idx.push(self.index_pos(it, d, k)?);
                }
                return Ok(Key::Elem(v.item_offset(&idx)));
            }
            if items.iter().all(|k| it.is_slice(k)) {
                return Err(it.new_exc_str("NotImplementedError", "multi-dimensional slicing is not implemented"));
            }
            return Err(it.type_error("memoryview: invalid slice key"));
        }
        if v.ndim() == 0 {
            return Err(it.type_error("invalid indexing of 0-dim memory"));
        }
        if it.is_slice(key) {
            let (start, stop, step) = slice_parts(it, key)?;
            let (start, n) = adjust_slice(v.shape[0], start, stop, step);
            return Ok(Key::View(v.slice(0, start, step, n)));
        }
        if it.has_index(key) {
            if v.ndim() != 1 {
                return Err(it.new_exc_str("NotImplementedError", "multi-dimensional sub-views are not implemented"));
            }
            let j = self.index_pos(it, 0, key)?;
            return Ok(Key::Elem(v.item_offset(&[j])));
        }
        Err(it.type_error("memoryview: invalid slice key"))
    }

    fn equal(&self, it: &mut Interp, other: &Value) -> R<Value> {
        let Some(y) = export(it, other)? else { return Ok(Value::NotImplemented) };
        if y.view.shape != self.view.shape {
            return Ok(Value::Bool(false));
        }
        let (fx, fy) = (parse_fmt(&self.fmt), parse_fmt(&y.fmt));
        let (Some((kx, _)), Some((ky, _))) = (fx, fy) else { return Ok(Value::Bool(false)) };
        if kx == ky && !kx.is_float() {
            let a = self.bytes(it)?;
            let b = gather(it, &y.src, &y.view)?;
            return Ok(Value::Bool(a == b));
        }
        let xs = self.values(it)?;
        let data = gather(it, &y.src, &y.view)?;
        let ys: Vec<Value> = data.chunks(y.view.itemsize.max(1)).map(|c| scalar_value(load(ky, c, ByteOrder::NATIVE))).collect();
        for (p, q) in xs.iter().zip(&ys) {
            if !it.values_eq(p, q)? {
                return Ok(Value::Bool(false));
            }
        }
        Ok(Value::Bool(true))
    }
}

/// `(start, stop, step)` of a slice object, `None` for omitted bounds.
fn slice_parts(it: &mut Interp, s: &Value) -> R<(Option<isize>, Option<isize>, isize)> {
    let (a, b, c) = match s {
        Value::Obj(o) => match &o.kind {
            Kind::Slice(a, b, c) => (a.clone(), b.clone(), c.clone()),
            _ => return Err(it.type_error("slice expected")),
        },
        _ => return Err(it.type_error("slice expected")),
    };
    let step = if c.is_none() { 1 } else { it.slice_index(&c)? as isize };
    if step == 0 {
        return Err(it.value_error("slice step cannot be zero"));
    }
    let bound = |it: &mut Interp, v: &Value| -> R<Option<isize>> { if v.is_none() { Ok(None) } else { Ok(Some(it.slice_index(v)? as isize)) } };
    Ok((bound(it, &a)?, bound(it, &b)?, step))
}

#[lumen_bind::methods]
impl MemoryView {
    #[constructor]
    fn new(it: &mut Interp, #[kw] object: &Value) -> R<MemoryView> {
        MemoryView::of(it, object)
    }

    #[proto(getitem)]
    fn __getitem__(slf: This<Py<Self>>, it: &mut Interp, key: &Value) -> R<Value> {
        let m = slf.0.borrow(it)?;
        m.src(it)?;
        m.kind(it)?;
        match m.resolve(it, key)? {
            Key::Elem(pos) => m.element(it, pos),
            Key::View(v) => {
                let fmt = m.fmt.clone();
                m.derive(it, v, fmt)
            }
            Key::Whole => Ok(slf.0.value().clone()),
        }
    }

    #[proto(setitem)]
    fn __setitem__(&self, it: &mut Interp, key: &Value, value: &Value) -> R<()> {
        self.src(it)?;
        if self.view.readonly {
            return Err(it.type_error("cannot modify read-only memory"));
        }
        let kind = self.kind(it)?;
        let dst = match self.resolve(it, key)? {
            Key::Elem(pos) => {
                let mut buf = vec![0u8; self.view.itemsize];
                pack_item(it, &self.fmt, kind, value, &mut buf)?;
                let r = self.src(it)?.with_mut(|b| match b.get_mut(pos..pos + buf.len()) {
                    Some(d) => {
                        d.copy_from_slice(&buf);
                        true
                    }
                    None => false,
                });
                return match r {
                    Ok(true) => Ok(()),
                    Ok(false) => Err(it.new_exc_str("IndexError", "index out of bounds on dimension 1")),
                    Err(e) => Err(inaccessible(it, e)),
                };
            }
            Key::View(v) => v,
            Key::Whole => self.view.clone(),
        };
        let Some(src) = export(it, value)? else {
            let t = it.type_name_of(value);
            return Err(it.type_error(&format!("a bytes-like object is required, not '{}'", t)));
        };
        if parse_fmt(&src.fmt).map(|f| f.0) != Some(kind) || src.view.shape != dst.shape {
            return Err(it.value_error("memoryview assignment: lvalue and rvalue have different structures"));
        }
        let data = gather(it, &src.src, &src.view)?;
        drop(src);
        let r = self.src(it)?.with_mut(|b| dst.check(b.len()).map(|()| dst.scatter(b, &data)));
        r.and_then(|r| r).map_err(|e| inaccessible(it, e))
    }

    #[proto(delitem)]
    fn __delitem__(&self, it: &mut Interp, key: &Value) -> R<()> {
        let _ = key;
        self.src(it)?;
        if self.view.readonly {
            return Err(it.type_error("cannot modify read-only memory"));
        }
        Err(it.type_error("cannot delete memory"))
    }

    #[proto(len)]
    fn __len__(&self, it: &mut Interp) -> R<usize> {
        self.src(it)?;
        match self.view.shape.first() {
            Some(n) => Ok(*n),
            None => Err(it.type_error("0-dim memory has no length")),
        }
    }

    #[proto(iter)]
    fn __iter__(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let src = slf.0;
        {
            let m = src.borrow(it)?;
            m.src(it)?;
            if m.view.ndim() == 0 {
                return Err(it.type_error("invalid indexing of 0-dim memory"));
            }
            if m.view.ndim() != 1 {
                return Err(it.new_exc_str("NotImplementedError", "multi-dimensional sub-views are not implemented"));
            }
            m.kind(it)?;
        }
        let mut i = 0usize;
        Ok(it.native_iter(Box::new(move |it: &mut Interp| {
            let m = src.borrow(it)?;
            m.src(it)?;
            if i >= m.view.shape[0] {
                return Ok(None);
            }
            let pos = m.view.item_offset(&[i]);
            i += 1;
            m.element(it, pos).map(Some)
        })))
    }

    #[proto(eq)]
    fn __eq__(slf: This<Py<Self>>, it: &mut Interp, other: &Value) -> R<Value> {
        if slf.0.value().is(other) {
            return Ok(Value::Bool(true));
        }
        let m = slf.0.borrow(it)?;
        let other_released = Py::<MemoryView>::from_value(it, other).map(|p| p.borrow(it).map(|o| o.src.is_none())).transpose()?;
        if m.src.is_none() || other_released == Some(true) {
            return Ok(Value::Bool(false));
        }
        m.equal(it, other)
    }

    #[proto(ne)]
    fn __ne__(slf: This<Py<Self>>, it: &mut Interp, other: &Value) -> R<Value> {
        Ok(match Self::__eq__(slf, it, other)? {
            Value::Bool(b) => Value::Bool(!b),
            v => v,
        })
    }

    #[proto(hash)]
    fn __hash__(&self, it: &mut Interp) -> R<i64> {
        self.src(it)?;
        if !self.view.readonly {
            return Err(it.value_error("cannot hash writable memoryview object"));
        }
        if !is_byte_fmt(&self.fmt) {
            return Err(it.value_error("memoryview: hashing is restricted to formats 'B', 'b' or 'c'"));
        }
        let data = self.bytes(it)?;
        Ok(hash_bytes(&data))
    }

    #[proto(repr)]
    fn __repr__(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
        let released = slf.0.borrow(it)?.src.is_none();
        let id = it.id_of(slf.0.value());
        Ok(if released { format!("<released memory at {:#x}>", id) } else { format!("<memory at {:#x}>", id) })
    }

    #[proto(enter)]
    #[method(hint(py(text_signature = "")))]
    fn __enter__(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        slf.0.borrow(it)?.src(it)?;
        Ok(slf.0.into_value())
    }

    #[proto(exit)]
    #[method(hint(py(text_signature = "")))]
    fn __exit__(&mut self, #[varargs] _args: &[Value]) {
        self.src = None;
    }

    #[method(name = "__buffer__")]
    fn buffer(&self, it: &mut Interp, flags: &Value) -> R<Value> {
        let _ = flags;
        let fmt = self.fmt.clone();
        self.derive(it, self.view.clone(), fmt)
    }

    #[method(name = "__release_buffer__")]
    fn release_buffer(&self, buffer: &Value) {
        let _ = buffer;
    }

    #[method(hint(py(text_signature = "")))]
    fn __reduce__(&self, it: &mut Interp) -> R<Value> {
        Err(it.type_error("cannot pickle 'memoryview' object"))
    }

    #[method(hint(py(text_signature = "")))]
    fn __reduce_ex__(&self, it: &mut Interp, protocol: &Value) -> R<Value> {
        let _ = protocol;
        Err(it.type_error("cannot pickle 'memoryview' object"))
    }

    #[method(hint(py(text_signature = "($self, /)")))]
    fn tolist(&self, it: &mut Interp) -> R<Value> {
        self.src(it)?;
        let flat = self.values(it)?;
        if self.view.ndim() == 0 {
            return Ok(flat.into_iter().next().unwrap_or(Value::None));
        }
        fn nest(shape: &[usize], items: &mut std::vec::IntoIter<Value>) -> Value {
            if shape.len() == 1 {
                return Value::list(items.take(shape[0]).collect());
            }
            Value::list((0..shape[0]).map(|_| nest(&shape[1..], items)).collect())
        }
        Ok(nest(&self.view.shape, &mut flat.into_iter()))
    }

    #[method(hint(py(text_signature = "($self, /, order='C')")))]
    fn tobytes(&self, it: &mut Interp, #[kw] order: Option<&Value>) -> R<Value> {
        if let Some(o) = order.filter(|o| !o.is_none()) {
            let s = it.str_arg(o, "order")?;
            if !matches!(s.as_str(), "C" | "F" | "A") {
                return Err(it.value_error("order must be 'C', 'F' or 'A'"));
            }
        }
        Ok(Value::bytes(self.bytes(it)?))
    }

    #[method(hint(py(text_signature = "($self, /, sep=<unrepresentable>, bytes_per_sep=1)")))]
    fn hex(&self, it: &mut Interp, #[kw] sep: Option<&Value>, #[kw] bytes_per_sep: Option<&Value>) -> R<String> {
        let data = self.bytes(it)?;
        let sep = hex_sep_arg(it, sep)?;
        let per = match bytes_per_sep {
            Some(v) => it.index_of(v)?,
            None => 1,
        };
        Ok(lumen_common::codec::hex_encode_sep(&data, sep, per))
    }

    #[method(hint(py(text_signature = "($self, /)")))]
    fn release(&mut self) {
        self.src = None;
    }

    #[method(hint(py(text_signature = "($self, /)")))]
    fn toreadonly(&self, it: &mut Interp) -> R<Value> {
        let mut v = self.view.clone();
        v.readonly = true;
        let fmt = self.fmt.clone();
        self.derive(it, v, fmt)
    }

    #[method(hint(py(text_signature = "($self, /, format, shape=<unrepresentable>)")))]
    fn cast(&self, it: &mut Interp, #[kw] format: &Value, #[kw] shape: Option<&Value>) -> R<Value> {
        self.src(it)?;
        if !self.view.is_c_contiguous() {
            return Err(it.type_error("memoryview: casts are restricted to C-contiguous views"));
        }
        let shape = shape.filter(|s| !s.is_none());
        if (shape.is_some() || self.view.ndim() != 1) && self.view.shape.contains(&0) {
            return Err(it.type_error("memoryview: cannot cast view with zeros in shape or strides"));
        }
        let dims = match shape {
            Some(s) => {
                let items = match s {
                    Value::Obj(o) if matches!(o.kind, Kind::List(_) | Kind::Tuple(_)) => it.iterate_to_vec(s)?,
                    _ => return Err(it.type_error("shape must be a list or a tuple")),
                };
                if items.len() > MAX_NDIM {
                    return Err(it.value_error("memoryview: number of dimensions must not exceed 64"));
                }
                if self.view.ndim() != 1 && items.len() != 1 {
                    return Err(it.type_error("memoryview: cast must be 1D -> ND or ND -> 1D"));
                }
                Some(items)
            }
            None => None,
        };
        let Some(fmt) = format.as_str().map(str::to_string) else {
            return Err(it.type_error("memoryview: format argument must be a string"));
        };
        let Some((kind, itemsize)) = parse_fmt(&fmt) else {
            return Err(it.value_error(
                "memoryview: destination format must be a native single character format prefixed with an optional '@'",
            ));
        };
        if !is_byte_fmt(&fmt) && !is_byte_fmt(&self.fmt) {
            return Err(it.type_error("memoryview: cannot cast between two non-byte formats"));
        }
        if self.view.nbytes() % itemsize != 0 {
            return Err(it.type_error("memoryview: length is not a multiple of itemsize"));
        }
        let shape = match dims {
            Some(items) => {
                let mut out = Vec::with_capacity(items.len());
                for v in &items {
                    if !matches!(v, Value::Int(_) | Value::Bool(_)) && v.as_bigint().is_none() {
                        return Err(it.type_error("memoryview.cast(): elements of shape must be integers"));
                    }
                    let n = it.index_of(v)?;
                    if n <= 0 {
                        return Err(it.value_error("memoryview.cast(): elements of shape must be integers > 0"));
                    }
                    out.push(n as usize);
                    if lumen_common::buffer::shape_product(&out).is_none() {
                        return Err(it.value_error("memoryview.cast(): product(shape) > SSIZE_MAX"));
                    }
                }
                Some(out)
            }
            None => None,
        };
        let v = match self.view.cast(Some(kind), itemsize, ByteOrder::NATIVE, shape) {
            Ok(v) => v,
            Err(CastError::TooLarge) => return Err(it.value_error("memoryview.cast(): product(shape) > SSIZE_MAX")),
            Err(CastError::NotContiguous) => return Err(it.type_error("memoryview: casts are restricted to C-contiguous views")),
            Err(CastError::SizeMismatch) => return Err(it.type_error("memoryview: product(shape) * itemsize != buffer size")),
        };
        self.derive(it, v, fmt.as_str().into())
    }

    #[getter]
    fn nbytes(&self, it: &mut Interp) -> R<usize> {
        self.src(it)?;
        Ok(self.view.nbytes())
    }

    #[getter]
    fn readonly(&self, it: &mut Interp) -> R<bool> {
        self.src(it)?;
        Ok(self.view.readonly)
    }

    #[getter]
    fn itemsize(&self, it: &mut Interp) -> R<usize> {
        self.src(it)?;
        Ok(self.view.itemsize)
    }

    #[getter]
    fn format(&self, it: &mut Interp) -> R<String> {
        self.src(it)?;
        Ok(self.fmt.to_string())
    }

    #[getter]
    fn ndim(&self, it: &mut Interp) -> R<usize> {
        self.src(it)?;
        Ok(self.view.ndim())
    }

    #[getter]
    fn shape(&self, it: &mut Interp) -> R<Value> {
        self.src(it)?;
        Ok(Value::tuple(self.view.shape.iter().map(|n| Value::Int(*n as i64)).collect()))
    }

    #[getter]
    fn strides(&self, it: &mut Interp) -> R<Value> {
        self.src(it)?;
        Ok(Value::tuple(self.view.strides.iter().map(|n| Value::Int(*n as i64)).collect()))
    }

    #[getter]
    fn suboffsets(&self, it: &mut Interp) -> R<Value> {
        self.src(it)?;
        Ok(Value::tuple(Vec::new()))
    }

    #[getter]
    fn obj(&self, it: &mut Interp) -> R<Value> {
        self.src(it)?;
        Ok(self.obj.clone())
    }

    #[getter]
    fn c_contiguous(&self, it: &mut Interp) -> R<bool> {
        self.src(it)?;
        Ok(self.view.is_c_contiguous())
    }

    #[getter]
    fn f_contiguous(&self, it: &mut Interp) -> R<bool> {
        self.src(it)?;
        Ok(self.view.is_f_contiguous())
    }

    #[getter]
    fn contiguous(&self, it: &mut Interp) -> R<bool> {
        self.src(it)?;
        Ok(self.view.is_contiguous())
    }
}

/// The `sep` argument of `bytes.hex()` and friends: one ASCII character (from a str or a
/// bytes-like object), or `None`.
pub fn hex_sep_arg(it: &mut Interp, sep: Option<&Value>) -> R<Option<char>> {
    let Some(v) = sep.filter(|v| !v.is_none()) else { return Ok(None) };
    let chars: Vec<char> = match v.as_str() {
        Some(s) => s.chars().collect(),
        None => {
            it.len_of(v)?;
            it.bytes_of(v)?.iter().map(|&b| b as char).collect()
        }
    };
    let [c] = chars[..] else {
        return Err(it.value_error("sep must be length 1."));
    };
    if !c.is_ascii() {
        return Err(it.value_error("sep must be ASCII."));
    }
    Ok(Some(c))
}

/// Where a C-contiguous memoryview's bytes live, for zero-copy borrows.
pub enum Part {
    Bytes(Obj, std::ops::Range<usize>),
    Store(Rc<ByteStore>, std::ops::Range<usize>),
}

/// The bytes of memoryview `v` (`BufferError` unless it is C-contiguous) or of an array; `None`
/// for any other object.
pub fn contiguous_part(it: &mut Interp, v: &Value) -> R<Option<Part>> {
    let Some(p) = Py::<MemoryView>::from_value(it, v) else {
        let store = match super::arraym::array::parts(it, v) {
            Some((_, store)) => Some(store),
            None => super::mmapm::mmap::buffer_of(it, v)?.map(|(store, _)| store),
        };
        return Ok(store.map(|store| {
            let n = store.len();
            Part::Store(store, 0..n)
        }));
    };
    let m = p.borrow(it)?;
    let src = m.src(it)?;
    if !m.view.is_c_contiguous() {
        return Err(it.new_exc_str("BufferError", "memoryview: underlying buffer is not C-contiguous"));
    }
    let len = src.with(|b| b.len()).map_err(|e| inaccessible(it, e))?;
    m.view.check(len).map_err(|e| inaccessible(it, e))?;
    let r = m.view.offset..m.view.offset + m.view.nbytes();
    Ok(Some(match src {
        Source::Bytes(o) => Part::Bytes(o.clone(), r),
        Source::Store(e) => Part::Store(e.store().clone(), r),
    }))
}

/// The store range of a writable C-contiguous memoryview or of a writable `mmap`; `None` for any
/// other object.
pub fn writable_part(it: &mut Interp, v: &Value) -> R<Option<(Rc<ByteStore>, std::ops::Range<usize>)>> {
    if let Some((store, false)) = super::mmapm::mmap::buffer_of(it, v)? {
        let n = store.len();
        return Ok(Some((store, 0..n)));
    }
    let Some(p) = Py::<MemoryView>::from_value(it, v) else { return Ok(None) };
    let m = p.borrow(it)?;
    if m.view.readonly || !m.view.is_c_contiguous() {
        return Ok(None);
    }
    let src = m.src(it)?;
    let Source::Store(e) = src else { return Ok(None) };
    let len = src.with(|b| b.len()).map_err(|e| inaccessible(it, e))?;
    m.view.check(len).map_err(|e| inaccessible(it, e))?;
    Ok(Some((e.store().clone(), m.view.offset..m.view.offset + m.view.nbytes())))
}

pub fn is_memoryview(it: &Interp, v: &Value) -> bool {
    crate::bind::is_instance::<MemoryView>(it, v)
}

/// Whether `v` is a native object exporting a buffer (`memoryview`, `array.array`, `mmap.mmap`).
pub fn is_buffer_object(it: &Interp, v: &Value) -> bool {
    is_memoryview(it, v)
        || crate::bind::is_instance::<super::arraym::array::Array>(it, v)
        || crate::bind::is_instance::<super::mmapm::mmap::Mmap>(it, v)
}

/// Installs `memoryview` in `builtins`.
pub fn init(it: &mut Interp) {
    let ty = crate::bind::type_object::<MemoryView>(it);
    let b = it.builtins.clone();
    dict_set_str(&b, "memoryview", Value::Obj(ty));
}
