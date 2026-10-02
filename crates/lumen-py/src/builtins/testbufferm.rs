//! `_testbuffer` (`Modules/_testbuffer.c`): `ndarray`, an exporter whose layout (shape, strides,
//! offset, format, flags) the tests choose freely, `staticarray`, a deliberately non-compliant
//! exporter, and the helpers that check `PyBuffer_*` against it.
//!
//! The data lives in a `lumen_common::buffer::ByteStore` and every layout is a [`ViewDesc`]
//! over it; what the C module expresses with raw pointers is a byte offset into the store.
//! PIL-style arrays (`ND_PIL`: a pointer in the first dimension, described by suboffsets) need
//! indirection that [`ViewDesc`] does not have, so they report `NotImplementedError`;
//! `add_suboffsets` records its redundant (negative) suboffsets on the base instead.

use super::memview::{export_flags, is_buffer_object, slice_parts, view_from_parts, Source, PYBUF_FULL_RO};
use crate::bind::{buffer_error, type_object, Py, This};
use crate::object::*;
use crate::vm::{dict_set_str, Interp};
use lumen_common::buffer::{adjust_slice_bounds, c_strides, f_strides, struct_code, BufferError, ByteOrder, ByteStore, StructMode, ViewDesc};
use std::rc::Rc;

const ND_MAX_NDIM: usize = 128;

const ND_VAREXPORT: i64 = 0x001;
const ND_WRITABLE: i64 = 0x002;
const ND_FORTRAN: i64 = 0x004;
const ND_SCALAR: i64 = 0x008;
const ND_PIL: i64 = 0x010;
const ND_REDIRECT: i64 = 0x020;
const ND_GETBUF_FAIL: i64 = 0x040;
const ND_GETBUF_UNDEFINED: i64 = 0x080;
const ND_C: i64 = 0x100;

const PYBUF_WRITABLE: i64 = 0x0001;
const PYBUF_FORMAT: i64 = 0x0004;
const PYBUF_ND: i64 = 0x0008;
const PYBUF_STRIDES: i64 = 0x0010 | PYBUF_ND;
const PYBUF_C_CONTIGUOUS: i64 = 0x0020 | PYBUF_STRIDES;
const PYBUF_F_CONTIGUOUS: i64 = 0x0040 | PYBUF_STRIDES;
const PYBUF_ANY_CONTIGUOUS: i64 = 0x0080 | PYBUF_STRIDES;
const PYBUF_INDIRECT: i64 = 0x0100 | PYBUF_STRIDES;
const PYBUF_CONTIG: i64 = PYBUF_ND | PYBUF_WRITABLE;
const PYBUF_CONTIG_RO: i64 = PYBUF_ND;
const PYBUF_STRIDED: i64 = PYBUF_STRIDES | PYBUF_WRITABLE;
const PYBUF_STRIDED_RO: i64 = PYBUF_STRIDES;
const PYBUF_RECORDS: i64 = PYBUF_STRIDES | PYBUF_WRITABLE | PYBUF_FORMAT;
const PYBUF_RECORDS_RO: i64 = PYBUF_STRIDES | PYBUF_FORMAT;
const PYBUF_FULL: i64 = PYBUF_INDIRECT | PYBUF_WRITABLE | PYBUF_FORMAT;
const PYBUF_READ: i64 = 0x100;
const PYBUF_WRITE: i64 = 0x200;

fn req(flags: i64, mask: i64) -> bool {
    flags & mask == mask
}

fn c_contiguous_flag(flags: i64) -> bool {
    flags & (ND_SCALAR | ND_C) != 0
}

fn f_contiguous_flag(flags: i64) -> bool {
    flags & (ND_SCALAR | ND_FORTRAN) != 0
}

fn any_contiguous_flag(flags: i64) -> bool {
    flags & (ND_SCALAR | ND_C | ND_FORTRAN) != 0
}

fn buffer_exc(it: &mut Interp, msg: &str) -> Obj {
    it.new_exc_str("BufferError", msg)
}

/// A `Py_buffer`: a view of `src` as an exporter reports it, where a missing shape, strides or
/// format are the NULL pointers of a request that did not ask for them.
struct Buf {
    src: Source,
    offset: usize,
    len: usize,
    itemsize: usize,
    readonly: bool,
    ndim: usize,
    format: Option<String>,
    shape: Option<Vec<usize>>,
    strides: Option<Vec<isize>>,
    suboffsets: bool,
    obj: Option<Value>,
}

impl Buf {
    fn dup(&self) -> Result<Buf, BufferError> {
        Ok(Buf {
            src: self.src.reexport()?,
            offset: self.offset,
            len: self.len,
            itemsize: self.itemsize,
            readonly: self.readonly,
            ndim: self.ndim,
            format: self.format.clone(),
            shape: self.shape.clone(),
            strides: self.strides.clone(),
            suboffsets: self.suboffsets,
            obj: self.obj.clone(),
        })
    }

    fn shape_vec(&self) -> Vec<usize> {
        match &self.shape {
            Some(s) => s.clone(),
            None if self.ndim == 0 => Vec::new(),
            None => vec![self.len / self.itemsize.max(1)],
        }
    }

    fn strides_vec(&self) -> Vec<isize> {
        match &self.strides {
            Some(s) => s.clone(),
            None => c_strides(&self.shape_vec(), self.itemsize),
        }
    }

    fn fmt(&self) -> &str {
        self.format.as_deref().unwrap_or("B")
    }

    fn desc(&self) -> ViewDesc {
        let fmt = self.fmt();
        let elem = match fmt.as_bytes() {
            [c] => struct_code(*c, StructMode::Native).and_then(|c| c.kind),
            _ => None,
        };
        ViewDesc {
            offset: self.offset,
            itemsize: self.itemsize,
            elem,
            order: ByteOrder::NATIVE,
            shape: self.shape_vec(),
            strides: self.strides_vec(),
            readonly: self.readonly,
        }
    }

    fn first_dim(&self) -> usize {
        match &self.shape {
            Some(s) => s.first().copied().unwrap_or(0),
            None => self.len,
        }
    }

    fn read(&self, it: &mut Interp, pos: isize, n: usize) -> R<Vec<u8>> {
        let out_of_bounds = |it: &mut Interp| it.new_exc_str("IndexError", "index out of bounds");
        if pos < 0 {
            return Err(out_of_bounds(it));
        }
        let pos = pos as usize;
        match self.src.with(|b| b.get(pos..pos + n).map(<[u8]>::to_vec)) {
            Ok(Some(v)) => Ok(v),
            Ok(None) => Err(out_of_bounds(it)),
            Err(e) => Err(buffer_error(it, e)),
        }
    }

    fn write(&self, it: &mut Interp, pos: isize, data: &[u8]) -> R<()> {
        let out_of_bounds = |it: &mut Interp| it.new_exc_str("IndexError", "index out of bounds");
        if pos < 0 {
            return Err(out_of_bounds(it));
        }
        let pos = pos as usize;
        let r = self.src.with_mut(|b| match b.get_mut(pos..pos + data.len()) {
            Some(d) => {
                d.copy_from_slice(data);
                true
            }
            None => false,
        });
        match r {
            Ok(true) => Ok(()),
            Ok(false) => Err(out_of_bounds(it)),
            Err(e) => Err(buffer_error(it, e)),
        }
    }

    fn all_bytes(&self, it: &mut Interp) -> R<Vec<u8>> {
        self.src.with(<[u8]>::to_vec).map_err(|e| buffer_error(it, e))
    }
}

fn packed<'a>(itemsize: usize, dims: impl Iterator<Item = (&'a usize, &'a isize)>) -> bool {
    let mut step = itemsize as isize;
    for (&n, &s) in dims {
        if n > 1 && s != step {
            return false;
        }
        step = step.saturating_mul(n as isize);
    }
    true
}

/// `PyBuffer_IsContiguous`.
fn is_contig(b: &Buf, order: char) -> bool {
    if b.suboffsets {
        return false;
    }
    let c = || {
        if b.len == 0 {
            return true;
        }
        let Some(strides) = &b.strides else { return true };
        if b.ndim == 0 {
            return true;
        }
        let shape = b.shape_vec();
        packed(b.itemsize, shape.iter().zip(strides).rev())
    };
    let f = || {
        if b.len == 0 {
            return true;
        }
        let Some(strides) = &b.strides else { return b.ndim <= 1 };
        if b.ndim == 0 {
            return true;
        }
        let shape = b.shape_vec();
        packed(b.itemsize, shape.iter().zip(strides))
    };
    match order {
        'C' => c(),
        'F' => f(),
        _ => c() || f(),
    }
}

fn init_flags(b: &Buf, flags: &mut i64) {
    if b.ndim == 0 {
        *flags |= ND_SCALAR;
    }
    if is_contig(b, 'C') {
        *flags |= ND_C;
    }
    if is_contig(b, 'F') {
        *flags |= ND_FORTRAN;
    }
}

fn type_name(it: &mut Interp, v: &Value) -> String {
    it.type_name_of(v)
}

fn seq_items(v: &Value) -> Option<Vec<Value>> {
    if let Some(t) = v.tuple_items() {
        return Some(t.to_vec());
    }
    list_of(v).map(|l| l.borrow().clone())
}

fn is_scalar_item(v: &Value) -> bool {
    match v {
        Value::Int(_) | Value::Bool(_) | Value::Float(_) => true,
        Value::Obj(o) => matches!(o.kind, Kind::Bytes(_) | Kind::Int(_) | Kind::Float(_)),
        _ => false,
    }
}

fn index_arg(it: &mut Interp, v: &Value) -> R<isize> {
    Ok(it.seq_index(v)? as isize)
}

fn struct_attr(it: &mut Interp, name: &str) -> R<Value> {
    let m = it.import_module("struct")?;
    it.get_attr_str(&Value::Obj(m), name)
}

fn bytes_of(it: &mut Interp, v: &Value) -> R<Vec<u8>> {
    match v {
        Value::Obj(o) => match &o.kind {
            Kind::Bytes(b) => Ok(b.clone()),
            _ => Err(it.type_error("expected bytes")),
        },
        _ => Err(it.type_error("expected bytes")),
    }
}

/// `unpack_single`: one element as `struct.unpack_from` returns it.
fn unpack_single(it: &mut Interp, data: Vec<u8>, fmt: Option<&str>) -> R<Value> {
    let fmt = fmt.unwrap_or("B");
    let f = struct_attr(it, "unpack_from")?;
    let x = it.call(&f, vec![Value::str(fmt), Value::bytes(data)], Vec::new())?;
    match x.tuple_items() {
        Some([one]) => Ok(one.clone()),
        _ => Ok(x),
    }
}

/// A `struct.Struct` and the number of members one of its items packs.
fn make_struct(it: &mut Interp, format: &Value) -> R<(Value, usize)> {
    let cls = struct_attr(it, "Struct")?;
    let st = it.call(&cls, vec![format.clone()], Vec::new())?;
    let size = it.get_attr_str(&st, "size")?.as_i64().unwrap_or(0).max(0) as usize;
    let zeros = it.call_method(&st, "unpack", vec![Value::bytes(vec![0; size])])?;
    let nmemb = zeros.tuple_items().map_or(0, <[Value]>::len);
    Ok((st, nmemb))
}

/// `pack_single` / `pack_from_list`: the bytes of one item.
fn pack_item(it: &mut Interp, st: &Value, nmemb: usize, item: &Value) -> R<Vec<u8>> {
    let args = if is_scalar_item(item) && nmemb == 1 {
        vec![item.clone()]
    } else {
        match seq_items(item) {
            Some(members) if members.len() == nmemb => members,
            _ => return Err(it.value_error("mismatch between initializer element and format string")),
        }
    };
    let packed = it.call_method(st, "pack", args)?;
    bytes_of(it, &packed)
}

fn pack_single(it: &mut Interp, buf: &Buf, pos: isize, item: &Value) -> R<()> {
    let fmt = Value::str(buf.fmt());
    let (st, nmemb) = make_struct(it, &fmt)?;
    let data = pack_item(it, &st, nmemb, item)?;
    buf.write(it, pos, &data)
}

fn unpack_at(it: &mut Interp, buf: &Buf, pos: isize) -> R<Value> {
    let size = if buf.format.is_some() { buf.itemsize } else { 1 };
    let data = buf.read(it, pos, size)?;
    unpack_single(it, data, buf.format.as_deref())
}

struct Base {
    buf: Buf,
    flags: i64,
    offset: i64,
    store: Option<Rc<ByteStore>>,
}

impl Base {
    fn exports(&self) -> usize {
        self.store.as_ref().map_or(0, |s| s.pins().saturating_sub(1))
    }
}

#[lumen_bind::class(module = "builtins", name = "ndarray")]
pub struct NDArray {
    flags: i64,
    bases: Vec<Base>,
}

impl NDArray {
    fn head(&self) -> &Base {
        self.bases.last().expect("an ndarray always has a base")
    }

    fn head_mut(&mut self) -> &mut Base {
        self.bases.last_mut().expect("an ndarray always has a base")
    }

    fn is_consumer(&self) -> bool {
        self.head().store.is_none()
    }
}

fn nd_of(it: &Interp, v: &Value) -> Option<Py<NDArray>> {
    Py::<NDArray>::from_value(it, v)
}

fn has_buffer(it: &mut Interp, v: &Value) -> bool {
    if nd_of(it, v).is_some() || is_buffer_object(it, v) {
        return true;
    }
    let Value::Obj(o) = v else { return false };
    if matches!(o.kind, Kind::Bytes(_) | Kind::ByteArray(_)) {
        return true;
    }
    if o.cls.is_none() {
        return false;
    }
    let cls = it.type_of_obj(o);
    it.lookup_mro(&cls, "__buffer__").is_some()
}

/// `PyObject_GetBuffer`.
fn get_buffer(it: &mut Interp, obj: &Value, flags: i64) -> R<Buf> {
    if let Some(nd) = nd_of(it, obj) {
        return ndarray_getbuf(it, &nd, flags);
    }
    let Some(e) = export_flags(it, obj, flags)? else {
        let t = type_name(it, obj);
        return Err(it.type_error(&format!("a bytes-like object is required, not '{t}'")));
    };
    let d = e.view;
    let mut buf = Buf {
        src: e.src,
        offset: d.offset,
        len: d.nbytes(),
        itemsize: d.itemsize,
        readonly: d.readonly,
        ndim: d.ndim(),
        format: Some(e.fmt.to_string()),
        shape: Some(d.shape.clone()),
        strides: Some(d.strides.clone()),
        suboffsets: false,
        obj: Some(e.obj),
    };
    let (c, f) = (is_contig(&buf, 'C'), is_contig(&buf, 'F'));
    if flags & PYBUF_WRITABLE != 0 && buf.readonly {
        return Err(buffer_exc(it, "memoryview: underlying buffer is not writable"));
    }
    if flags & PYBUF_FORMAT == 0 {
        buf.format = None;
    }
    if req(flags, PYBUF_C_CONTIGUOUS) && !c {
        return Err(buffer_exc(it, "memoryview: underlying buffer is not C-contiguous"));
    }
    if req(flags, PYBUF_F_CONTIGUOUS) && !f {
        return Err(buffer_exc(it, "memoryview: underlying buffer is not Fortran contiguous"));
    }
    if req(flags, PYBUF_ANY_CONTIGUOUS) && !(c || f) {
        return Err(buffer_exc(it, "memoryview: underlying buffer is not contiguous"));
    }
    if !req(flags, PYBUF_STRIDES) {
        if !c {
            return Err(buffer_exc(it, "memoryview: underlying buffer is not C-contiguous"));
        }
        buf.strides = None;
    }
    if !req(flags, PYBUF_ND) {
        if flags & PYBUF_FORMAT != 0 {
            return Err(buffer_exc(it, "memoryview: cannot cast to unsigned bytes if the format flag is present"));
        }
        buf.ndim = 1;
        buf.shape = None;
    }
    Ok(buf)
}

fn ndarray_getbuf(it: &mut Interp, py: &Py<NDArray>, flags: i64) -> R<Buf> {
    let nd = py.borrow(it)?;
    let head = nd.head();
    let baseflags = head.flags;
    if let (Some(obj), true) = (&head.buf.obj, baseflags & ND_REDIRECT != 0) {
        let obj = obj.clone();
        drop(nd);
        return get_buffer(it, &obj, flags);
    }
    let base = &head.buf;
    let mut view = base.dup().map_err(|e| buffer_error(it, e))?;
    view.obj = None;
    if view.format.is_none() {
        view.format = Some("B".to_string());
    }
    if base.ndim != 0 && ((req(flags, PYBUF_ND) && base.shape.is_none()) || (req(flags, PYBUF_STRIDES) && base.strides.is_none())) {
        return Err(buffer_exc(it, "re-exporter does not provide format, shape or strides"));
    }
    if baseflags & ND_GETBUF_FAIL != 0 {
        return Err(buffer_exc(it, "ND_GETBUF_FAIL: forced test exception"));
    }
    if flags & PYBUF_WRITABLE != 0 && base.readonly {
        return Err(buffer_exc(it, "ndarray is not writable"));
    }
    if flags & PYBUF_FORMAT == 0 {
        view.format = None;
    }
    if req(flags, PYBUF_C_CONTIGUOUS) && !c_contiguous_flag(baseflags) {
        return Err(buffer_exc(it, "ndarray is not C-contiguous"));
    }
    if req(flags, PYBUF_F_CONTIGUOUS) && !f_contiguous_flag(baseflags) {
        return Err(buffer_exc(it, "ndarray is not Fortran contiguous"));
    }
    if req(flags, PYBUF_ANY_CONTIGUOUS) && !any_contiguous_flag(baseflags) {
        return Err(buffer_exc(it, "ndarray is not contiguous"));
    }
    if !req(flags, PYBUF_INDIRECT) && baseflags & ND_PIL != 0 {
        return Err(buffer_exc(it, "ndarray cannot be represented without suboffsets"));
    }
    if !req(flags, PYBUF_STRIDES) {
        if !c_contiguous_flag(baseflags) {
            return Err(buffer_exc(it, "ndarray is not C-contiguous"));
        }
        view.strides = None;
    }
    if !req(flags, PYBUF_ND) {
        if view.format.is_some() {
            return Err(buffer_exc(it, "ndarray: cannot cast to unsigned bytes if the format flag is present"));
        }
        view.ndim = 1;
        view.shape = None;
    }
    let mismatch = c_contiguous_flag(baseflags) != is_contig(&view, 'C')
        || (view.format.is_some() && view.shape.is_some() && f_contiguous_flag(baseflags) != is_contig(&view, 'F'))
        || (view.format.is_none() && view.shape.is_none() && !is_contig(&view, 'F'));
    if mismatch {
        return Err(buffer_exc(it, "ndarray: contiguity mismatch in getbuf()"));
    }
    view.obj = Some(py.value().clone());
    Ok(view)
}

fn buf_view(it: &mut Interp, buf: Buf) -> Value {
    let obj = buf.obj.clone().unwrap_or(Value::None);
    let fmt: Rc<str> = buf.fmt().into();
    let desc = buf.desc();
    view_from_parts(it, obj, buf.src, desc, fmt)
}

fn seq_as_ssize_array(it: &mut Interp, items: &[Value], is_shape: bool) -> R<Vec<i64>> {
    let what = if is_shape { "shape" } else { "strides" };
    let mut out = Vec::with_capacity(items.len());
    for v in items {
        if !v.is_int_like() {
            return Err(it.value_error(&format!("elements of {what} must be integers")));
        }
        let Some(x) = v.as_i64().or_else(|| v.as_bigint().and_then(|b| b.to_i64())) else {
            return Err(it.overflow_err("Python int too large to convert to C ssize_t"));
        };
        if is_shape && x < 0 {
            return Err(it.value_error("elements of shape must be integers >= 0"));
        }
        out.push(x);
    }
    Ok(out)
}

fn verify_structure(it: &mut Interp, len: usize, itemsize: usize, offset: usize, shape: &[usize], strides: &[isize]) -> R<()> {
    for &s in strides {
        if s as i128 % itemsize as i128 != 0 {
            return Err(it.value_error("strides must be a multiple of itemsize"));
        }
    }
    if shape.contains(&0) {
        return Ok(());
    }
    let (mut imin, mut imax) = (0i128, 0i128);
    for (&n, &s) in shape.iter().zip(strides) {
        let span = (n as i128 - 1) * s as i128;
        if s <= 0 {
            imin += span;
        } else {
            imax += span;
        }
    }
    if imin + (offset as i128) < 0 || imax + offset as i128 + itemsize as i128 > len as i128 {
        return Err(it.value_error("invalid combination of buffer, shape and strides"));
    }
    Ok(())
}

fn list_or_tuple(it: &mut Interp, v: &Value, name: &str) -> R<Vec<Value>> {
    match seq_items(v) {
        Some(items) => Ok(items),
        None => Err(it.type_error(&format!("{name} must be a list or a tuple"))),
    }
}

fn init_ndbuf(it: &mut Interp, items: &Value, shape: &Value, strides: Option<&Value>, offset: i64, format: &Value, flags: i64) -> R<Base> {
    let shape_items = list_or_tuple(it, shape, "shape")?;
    let ndim = shape_items.len();
    if ndim > ND_MAX_NDIM {
        return Err(it.value_error(&format!("ndim must not exceed {ND_MAX_NDIM}")));
    }
    let mut stride_items = None;
    if let Some(s) = strides {
        let v = list_or_tuple(it, s, "strides")?;
        if v.is_empty() {
        } else if flags & ND_FORTRAN != 0 {
            return Err(it.type_error("ND_FORTRAN cannot be used together with strides"));
        } else if v.len() != ndim {
            return Err(it.value_error("len(shape) != len(strides)"));
        } else {
            stride_items = Some(v);
        }
    }

    let calcsize = struct_attr(it, "calcsize")?;
    let size = it.call(&calcsize, vec![format.clone()], Vec::new())?;
    let itemsize = size.as_i64().unwrap_or(0);
    if itemsize <= 0 {
        return Err(it.value_error("itemsize must not be zero"));
    }
    let itemsize = itemsize as usize;

    let items: Vec<Value> = if ndim == 0 { vec![items.clone()] } else { list_or_tuple(it, items, "items")? };
    if items.is_empty() {
        return Err(it.value_error("initializer list or tuple must not be empty"));
    }
    let len = items.len() * itemsize;
    if offset % itemsize as i64 != 0 {
        return Err(it.value_error("offset must be a multiple of itemsize"));
    }
    if offset < 0 || offset as usize + itemsize > len {
        return Err(it.value_error("offset out of bounds"));
    }

    let (st, nmemb) = make_struct(it, format)?;
    let mut data = vec![0u8; len];
    for (i, item) in items.iter().enumerate() {
        let packed = pack_item(it, &st, nmemb, item)?;
        data[i * itemsize..(i + 1) * itemsize].copy_from_slice(&packed);
    }
    let Some(fmt) = format.as_str().map(str::to_string) else {
        return Err(it.type_error("bad argument type for built-in operation"));
    };

    let store = Rc::new(ByteStore::new(data));
    let export = store.export().map_err(|e| buffer_error(it, e))?;
    let mut buf = Buf {
        src: Source::Store(export),
        offset: 0,
        len,
        itemsize,
        readonly: flags & ND_WRITABLE == 0,
        ndim,
        format: Some(fmt),
        shape: None,
        strides: None,
        suboffsets: false,
        obj: None,
    };
    let mut flags = flags;

    if ndim == 0 {
        if flags & ND_PIL != 0 {
            return Err(it.type_error("ndim = 0 cannot be used in conjunction with ND_PIL"));
        }
        flags |= ND_SCALAR | ND_C | ND_FORTRAN;
        return Ok(Base { buf, flags, offset, store: Some(store) });
    }

    let shape_v: Vec<usize> = seq_as_ssize_array(it, &shape_items, true)?.into_iter().map(|n| n as usize).collect();
    let strides_v: Vec<isize> = match stride_items {
        Some(v) => seq_as_ssize_array(it, &v, false)?.into_iter().map(|n| n as isize).collect(),
        None if flags & ND_FORTRAN != 0 => f_strides(&shape_v, itemsize),
        None => c_strides(&shape_v, itemsize),
    };
    verify_structure(it, len, itemsize, offset as usize, &shape_v, &strides_v)?;
    buf.offset = offset as usize;
    buf.len = shape_v.iter().fold(itemsize, |a, &n| a.saturating_mul(n));
    buf.shape = Some(shape_v);
    buf.strides = Some(strides_v);
    init_flags(&buf, &mut flags);
    if flags & ND_PIL != 0 {
        return Err(it.new_exc_str("NotImplementedError", "ND_PIL: arrays with suboffsets are not supported"));
    }
    Ok(Base { buf, flags, offset, store: Some(store) })
}

fn consumer_base(it: &mut Interp, exporter: &Value, getbuf: i64) -> R<Base> {
    let buf = get_buffer(it, exporter, getbuf)?;
    let flags = if buf.readonly { 0 } else { ND_WRITABLE };
    Ok(Base { buf, flags, offset: -1, store: None })
}

fn new_consumer(it: &mut Interp, exporter: &Py<NDArray>, mut edit: impl FnMut(&mut Interp, &mut Buf) -> R<()>) -> R<Py<NDArray>> {
    let mut base = consumer_base(it, exporter.value(), PYBUF_FULL_RO)?;
    edit(it, &mut base.buf)?;
    let mut flags = base.flags;
    init_flags(&base.buf, &mut flags);
    base.flags = flags;
    Ok(Py::new(it, NDArray { flags: 0, bases: vec![base] }))
}

fn ptr_from_index(it: &mut Interp, base: &Buf, index: isize) -> R<isize> {
    let nitems = base.first_dim() as isize;
    let index = if index < 0 { index + nitems } else { index };
    if index < 0 || index >= nitems {
        return Err(it.new_exc_str("IndexError", "index out of bounds"));
    }
    let step = match &base.strides {
        Some(s) => s[0],
        None => base.itemsize as isize,
    };
    Ok(base.offset as isize + step * index)
}

fn nd_item(it: &mut Interp, py: &Py<NDArray>, index: isize) -> R<Value> {
    let nd = py.borrow(it)?;
    let base = &nd.head().buf;
    if base.ndim == 0 {
        return Err(it.type_error("invalid indexing of scalar"));
    }
    let pos = ptr_from_index(it, base, index)?;
    if base.ndim == 1 {
        return unpack_at(it, base, pos);
    }
    drop(nd);
    let sub = new_consumer(it, py, |_, b| {
        b.offset = pos as usize;
        let first = b.first_dim().max(1);
        b.len /= first;
        b.ndim -= 1;
        if let Some(s) = &mut b.shape {
            s.remove(0);
        }
        if let Some(s) = &mut b.strides {
            s.remove(0);
        }
        Ok(())
    })?;
    Ok(sub.into_value())
}

fn init_slice(it: &mut Interp, base: &mut Buf, key: &Value, dim: usize) -> R<()> {
    let (start, stop, step) = slice_parts(it, key)?;
    let (Some(shape), Some(strides)) = (&mut base.shape, &mut base.strides) else {
        return Err(buffer_exc(it, "re-exporter does not provide format, shape or strides"));
    };
    if dim >= shape.len() {
        return Err(it.new_exc_str("IndexError", "too many indices"));
    }
    let (start, _, n) = adjust_slice_bounds(shape[dim], start, stop, step);
    if n > 0 {
        base.offset = (base.offset as isize + strides[dim] * start) as usize;
    }
    shape[dim] = n;
    strides[dim] *= step;
    Ok(())
}

fn nd_subscript(it: &mut Interp, py: &Py<NDArray>, key: &Value) -> R<Value> {
    let ndim = py.borrow(it)?.head().buf.ndim;
    if ndim == 0 {
        if key.tuple_items().is_some_and(<[Value]>::is_empty) {
            let nd = py.borrow(it)?;
            let b = &nd.head().buf;
            return unpack_at(it, b, b.offset as isize);
        }
        if matches!(key, Value::Ellipsis) {
            return Ok(py.value().clone());
        }
        return Err(it.type_error("invalid indexing of scalar"));
    }
    if it.has_index(key) {
        let index = index_arg(it, key)?;
        return nd_item(it, py, index);
    }
    let sub = new_consumer(it, py, |it, b| {
        if it.is_slice(key) {
            init_slice(it, b, key, 0)?;
        } else if let Some(items) = key.tuple_items() {
            for (i, k) in items.iter().enumerate() {
                if !it.is_slice(k) {
                    let t = type_name(it, k);
                    return Err(it.type_error(&format!("cannot index memory using \"{t}\"")));
                }
                init_slice(it, b, k, i)?;
            }
        } else {
            let t = type_name(it, key);
            return Err(it.type_error(&format!("cannot index memory using \"{t}\"")));
        }
        init_len(b);
        Ok(())
    })?;
    Ok(sub.into_value())
}

fn same_structure(dest: &Buf, src: &Buf) -> bool {
    if dest.fmt() != src.fmt() || dest.itemsize != src.itemsize || dest.ndim != src.ndim {
        return false;
    }
    let (a, b) = (dest.shape_vec(), src.shape_vec());
    for (x, y) in a.iter().zip(&b) {
        if x != y {
            return false;
        }
        if *x == 0 {
            break;
        }
    }
    true
}

/// `copy_buffer`: all of `src` is read before `dest` is written.
fn copy_buffer(it: &mut Interp, dest: &Buf, src: &Buf) -> R<()> {
    if !same_structure(dest, src) {
        return Err(it.value_error("ndarray assignment: lvalue and rvalue have different structures"));
    }
    let sdesc = src.desc();
    let ddesc = dest.desc();
    let data = src.src.with(|b| sdesc.check(b.len()).map(|()| sdesc.gather(b))).map_err(|e| buffer_error(it, e))?;
    let data = data.map_err(|e| buffer_error(it, e))?;
    let r = dest.src.with_mut(|b| ddesc.check(b.len()).map(|()| ddesc.scatter(b, &data)));
    r.and_then(|r| r).map_err(|e| buffer_error(it, e))
}

fn nd_tobytes(it: &mut Interp, nd: &NDArray) -> R<Vec<u8>> {
    let head = nd.head();
    let buf = &head.buf;
    let all = buf.all_bytes(it)?;
    let desc = buf.desc();
    if c_contiguous_flag(head.flags) {
        return match all.get(buf.offset..buf.offset + buf.len) {
            Some(s) => Ok(s.to_vec()),
            None => Err(buffer_error(it, BufferError::OutOfBounds)),
        };
    }
    desc.check(all.len()).map_err(|e| buffer_error(it, e))?;
    Ok(desc.gather(&all))
}

fn unpack_rec(it: &mut Interp, unpack_from: &Value, all: &[u8], pos: isize, shape: &[usize], strides: &[isize], itemsize: usize) -> R<Value> {
    if shape.is_empty() {
        let item = usize::try_from(pos).ok().and_then(|p| all.get(p..p + itemsize));
        let Some(item) = item else { return Err(it.new_exc_str("IndexError", "index out of bounds")) };
        let x = it.call(unpack_from, vec![Value::bytes(item.to_vec())], Vec::new())?;
        return Ok(match x.tuple_items() {
            Some([one]) => one.clone(),
            _ => x,
        });
    }
    let mut out = Vec::with_capacity(shape[0]);
    for i in 0..shape[0] {
        let p = pos + strides[0] * i as isize;
        out.push(unpack_rec(it, unpack_from, all, p, &shape[1..], &strides[1..], itemsize)?);
    }
    Ok(Value::list(out))
}

fn nd_tolist(it: &mut Interp, py: &Py<NDArray>) -> R<Value> {
    let nd = py.borrow(it)?;
    let base = &nd.head().buf;
    let Some(fmt) = &base.format else {
        return Err(it.value_error("ndarray: tolist() does not support format=NULL, use tobytes()"));
    };
    let (shape, strides) = (base.shape_vec(), base.strides_vec());
    let cls = struct_attr(it, "Struct")?;
    let st = it.call(&cls, vec![Value::str(fmt)], Vec::new())?;
    let unpack_from = it.get_attr_str(&st, "unpack_from")?;
    let all = base.all_bytes(it)?;
    unpack_rec(it, &unpack_from, &all, base.offset as isize, &shape, &strides, base.itemsize)
}

fn tuple_of(items: &[i64]) -> Value {
    Value::tuple(items.iter().map(|&n| Value::Int(n)).collect())
}

fn init_len(b: &mut Buf) {
    b.len = b.shape_vec().iter().fold(b.itemsize, |a, &n| a.saturating_mul(n));
}

fn push_base(it: &mut Interp, nd: &Py<NDArray>, items: &Value, shape: &Value, strides: Option<&Value>, offset: i64, format: &Value, flags: i64) -> R<()> {
    let base = init_ndbuf(it, items, shape, strides, offset, format, flags)?;
    nd.borrow_mut(it)?.bases.push(base);
    Ok(())
}

fn exports_error(it: &mut Interp, n: usize) -> Obj {
    buffer_exc(it, &format!("cannot change structure: {n} exported buffer{}", if n == 1 { "" } else { "s" }))
}

fn simple_format() -> Value {
    Value::str("B")
}

#[lumen_bind::methods]
impl NDArray {
    #[constructor]
    fn new(
        it: &mut Interp,
        obj: &Value,
        shape: Option<&Value>,
        strides: Option<&Value>,
        offset: Option<i64>,
        format: Option<&Value>,
        flags: Option<i64>,
        getbuf: Option<i64>,
    ) -> R<NDArray> {
        let offset = offset.unwrap_or(0);
        let mut flags = flags.unwrap_or(0);
        if shape.is_none() && has_buffer(it, obj) {
            if strides.is_some() || offset != 0 || format.is_some() || !(flags == 0 || flags == ND_REDIRECT) {
                return Err(it.type_error("construction from exporter object only takes 'obj', 'getbuf' and 'flags' arguments"));
            }
            let mut base = consumer_base(it, obj, getbuf.unwrap_or(PYBUF_FULL_RO))?;
            init_flags(&base.buf, &mut base.flags);
            base.flags |= flags;
            return Ok(NDArray { flags: 0, bases: vec![base] });
        }
        if getbuf.is_some() {
            return Err(it.type_error("getbuf argument only valid for construction from exporter object"));
        }
        let Some(shape) = shape else {
            return Err(it.type_error("shape is a required argument when constructing from list, tuple or scalar"));
        };
        let mut nd_flags = 0;
        if flags & ND_VAREXPORT != 0 {
            nd_flags |= ND_VAREXPORT;
            flags &= !ND_VAREXPORT;
        }
        let format = format.cloned().unwrap_or_else(simple_format);
        let base = init_ndbuf(it, obj, shape, strides, offset, &format, flags)?;
        Ok(NDArray { flags: nd_flags, bases: vec![base] })
    }

    fn push(
        slf: This<Py<Self>>,
        it: &mut Interp,
        items: &Value,
        shape: &Value,
        strides: Option<&Value>,
        offset: Option<i64>,
        format: Option<&Value>,
        flags: Option<i64>,
    ) -> R<()> {
        let flags = flags.unwrap_or(0);
        if flags & ND_VAREXPORT != 0 {
            return Err(it.value_error("ND_VAREXPORT flag can only be used during object creation"));
        }
        {
            let nd = slf.0.borrow(it)?;
            if nd.is_consumer() {
                return Err(buffer_exc(it, "structure of re-exporting object is immutable"));
            }
            let exports = nd.head().exports();
            if nd.flags & ND_VAREXPORT == 0 && exports > 0 {
                return Err(exports_error(it, exports));
            }
        }
        let format = format.cloned().unwrap_or_else(simple_format);
        push_base(it, &slf.0, items, shape, strides, offset.unwrap_or(0), &format, flags)
    }

    fn pop(&mut self, it: &mut Interp) -> R<()> {
        if self.is_consumer() {
            return Err(buffer_exc(it, "structure of re-exporting object is immutable"));
        }
        let exports = self.head().exports();
        if exports > 0 {
            return Err(exports_error(it, exports));
        }
        if self.bases.len() == 1 {
            return Err(buffer_exc(it, "list only has a single base"));
        }
        self.bases.pop();
        Ok(())
    }

    fn tolist(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        nd_tolist(it, &slf.0)
    }

    fn tobytes(&self, it: &mut Interp) -> R<Value> {
        Ok(Value::bytes(nd_tobytes(it, self)?))
    }

    fn add_suboffsets(&mut self, it: &mut Interp) -> R<()> {
        let head = self.head_mut();
        if head.buf.suboffsets {
            return Err(it.type_error("cannot add suboffsets to PIL-style array"));
        }
        if head.buf.strides.is_none() {
            return Err(it.type_error("cannot add suboffsets to array without strides"));
        }
        head.buf.suboffsets = true;
        head.flags &= !(ND_C | ND_FORTRAN);
        Ok(())
    }

    fn memoryview_from_buffer(&self, it: &mut Interp) -> R<Value> {
        let head = self.head();
        let owner_store = match (&head.store, &head.buf.obj) {
            (Some(s), _) => Some(s.clone()),
            (None, Some(o)) => match nd_of(it, o) {
                Some(owner) => owner.borrow(it)?.head().store.clone(),
                None => None,
            },
            _ => None,
        };
        let Some(store) = owner_store else {
            return Err(it.type_error(
                "memoryview_from_buffer(): ndarray must be original exporter or consumer from ndarray/original exporter",
            ));
        };
        let view = &head.buf;
        if view.fmt().len() > ND_MAX_NDIM {
            return Err(it.type_error(&format!("memoryview_from_buffer: format is limited to {ND_MAX_NDIM} characters")));
        }
        if view.ndim > ND_MAX_NDIM {
            return Err(it.type_error(&format!("memoryview_from_buffer: ndim is limited to {ND_MAX_NDIM}")));
        }
        let copy = Rc::new(ByteStore::new(store.to_vec()));
        let export = copy.export().map_err(|e| buffer_error(it, e))?;
        let mut info = view.dup().map_err(|e| buffer_error(it, e))?;
        info.src = Source::Store(export);
        Ok(buf_view(it, info))
    }

    #[method(name = "__buffer__")]
    fn buffer(slf: This<Py<Self>>, it: &mut Interp, flags: i64) -> R<Value> {
        let buf = ndarray_getbuf(it, &slf.0, flags)?;
        Ok(buf_view(it, buf))
    }

    #[method(name = "__release_buffer__")]
    fn release_buffer(&self, view: &Value) {
        let _ = view;
    }

    #[proto(getitem)]
    fn getitem(slf: This<Py<Self>>, it: &mut Interp, key: &Value) -> R<Value> {
        nd_subscript(it, &slf.0, key)
    }

    #[proto(setitem)]
    fn setitem(slf: This<Py<Self>>, it: &mut Interp, key: &Value, value: &Value) -> R<()> {
        let nd = slf.0.borrow(it)?;
        let dest = &nd.head().buf;
        if dest.readonly {
            return Err(it.type_error("ndarray is not writable"));
        }
        if dest.ndim == 0 {
            if matches!(key, Value::Ellipsis) || key.tuple_items().is_some_and(<[Value]>::is_empty) {
                return pack_single(it, dest, dest.offset as isize, value);
            }
            return Err(it.type_error("invalid indexing of scalar"));
        }
        if dest.ndim == 1 && it.has_index(key) {
            let index = index_arg(it, key)?;
            let pos = ptr_from_index(it, dest, index)?;
            return pack_single(it, dest, pos, value);
        }
        drop(nd);
        let src = get_buffer(it, value, PYBUF_FULL_RO)?;
        let sub = nd_subscript(it, &slf.0, key)?;
        let Some(sub) = nd_of(it, &sub) else {
            return Err(it.type_error("ndarray assignment: expected an ndarray slice"));
        };
        let sub = sub.borrow(it)?;
        copy_buffer(it, &sub.head().buf, &src)
    }

    #[proto(delitem)]
    fn delitem(&self, it: &mut Interp, key: &Value) -> R<()> {
        let _ = key;
        if self.head().buf.readonly {
            return Err(it.type_error("ndarray is not writable"));
        }
        Err(it.type_error("ndarray data cannot be deleted"))
    }

    fn __hash__(slf: This<Py<Self>>, it: &mut Interp) -> R<i64> {
        let (readonly, obj) = {
            let nd = slf.0.borrow(it)?;
            (nd.head().buf.readonly, nd.head().buf.obj.clone())
        };
        if !readonly {
            return Err(it.value_error("cannot hash writable ndarray object"));
        }
        if let Some(o) = obj {
            it.hash_value(&o)?;
        }
        let nd = slf.0.borrow(it)?;
        let bytes = nd_tobytes(it, &nd)?;
        it.hash_value(&Value::bytes(bytes))
    }

    #[getter]
    fn flags(&self) -> i64 {
        self.head().flags
    }

    #[getter]
    fn offset(&self) -> i64 {
        self.head().offset
    }

    #[getter]
    fn obj(&self) -> Value {
        self.head().buf.obj.clone().unwrap_or(Value::None)
    }

    #[getter]
    fn nbytes(&self) -> i64 {
        self.head().buf.len as i64
    }

    #[getter]
    fn readonly(&self) -> bool {
        self.head().buf.readonly
    }

    #[getter]
    fn itemsize(&self) -> i64 {
        self.head().buf.itemsize as i64
    }

    #[getter]
    fn format(&self) -> String {
        self.head().buf.format.clone().unwrap_or_default()
    }

    #[getter]
    fn ndim(&self) -> i64 {
        self.head().buf.ndim as i64
    }

    #[getter]
    fn shape(&self) -> Value {
        match &self.head().buf.shape {
            Some(s) => tuple_of(&s.iter().map(|&n| n as i64).collect::<Vec<_>>()),
            None => Value::tuple(Vec::new()),
        }
    }

    #[getter]
    fn strides(&self) -> Value {
        match &self.head().buf.strides {
            Some(s) => tuple_of(&s.iter().map(|&n| n as i64).collect::<Vec<_>>()),
            None => Value::tuple(Vec::new()),
        }
    }

    #[getter]
    fn suboffsets(&self) -> Value {
        let b = &self.head().buf;
        if b.suboffsets {
            tuple_of(&vec![-1i64; b.ndim])
        } else {
            Value::tuple(Vec::new())
        }
    }

    #[getter]
    fn c_contiguous(&self, it: &mut Interp) -> R<bool> {
        let head = self.head();
        let ret = is_contig(&head.buf, 'C');
        if ret != c_contiguous_flag(head.flags) {
            return Err(it.new_exc_str("RuntimeError", "results from PyBuffer_IsContiguous() and flags differ"));
        }
        Ok(ret)
    }

    #[getter]
    fn f_contiguous(&self, it: &mut Interp) -> R<bool> {
        let head = self.head();
        let ret = is_contig(&head.buf, 'F');
        if ret != f_contiguous_flag(head.flags) {
            return Err(it.new_exc_str("RuntimeError", "results from PyBuffer_IsContiguous() and flags differ"));
        }
        Ok(ret)
    }

    #[getter]
    fn contiguous(&self, it: &mut Interp) -> R<bool> {
        let head = self.head();
        let ret = is_contig(&head.buf, 'A');
        if ret != any_contiguous_flag(head.flags) {
            return Err(it.new_exc_str("RuntimeError", "results from PyBuffer_IsContiguous() and flags differ"));
        }
        Ok(ret)
    }
}

/// `staticarray`: always exports the same twelve read-only bytes and ignores the request flags.
#[lumen_bind::class(module = "builtins", name = "staticarray")]
pub struct StaticArray {
    legacy_mode: bool,
    store: Rc<ByteStore>,
}

#[lumen_bind::methods]
impl StaticArray {
    #[constructor]
    fn new(legacy_mode: Option<&Value>) -> StaticArray {
        let legacy = legacy_mode.is_some_and(|v| !matches!(v, Value::Bool(false)));
        let store = Rc::new(ByteStore::new((0u8..12).collect()).readonly());
        StaticArray { legacy_mode: legacy, store }
    }

    #[method(name = "__buffer__")]
    fn buffer(slf: This<Py<Self>>, it: &mut Interp, flags: &Value) -> R<Value> {
        let _ = flags;
        let (legacy, store) = {
            let s = slf.0.borrow(it)?;
            (s.legacy_mode, s.store.clone())
        };
        let export = store.export().map_err(|e| buffer_error(it, e))?;
        let obj = if legacy { Value::None } else { slf.0.value().clone() };
        let view = ViewDesc::bytes(0, 12, true);
        Ok(view_from_parts(it, obj, Source::Store(export), view, "B".into()))
    }

    #[method(name = "__release_buffer__")]
    fn release_buffer(&self, view: &Value) {
        let _ = view;
    }
}

fn get_ascii_order(it: &mut Interp, order: &Value) -> R<char> {
    let Some(s) = order.as_str() else {
        return Err(it.type_error("order must be a string"));
    };
    let ascii = if s.is_ascii() {
        s.to_string()
    } else {
        let encoded = it.call_method(order, "encode", vec![Value::str("ascii")])?;
        String::from_utf8_lossy(&bytes_of(it, &encoded)?).into_owned()
    };
    match ascii.chars().next() {
        Some(c @ ('C' | 'F' | 'A')) => Ok(c),
        _ => Err(it.value_error("invalid order, must be C, F or A")),
    }
}

/// `PyBuffer_ToContiguous`: the elements in C or Fortran order.
fn to_contiguous(it: &mut Interp, buf: &Buf, order: char) -> R<Vec<u8>> {
    let all = buf.all_bytes(it)?;
    if is_contig(buf, order) {
        return match all.get(buf.offset..buf.offset + buf.len) {
            Some(s) => Ok(s.to_vec()),
            None => Err(buffer_error(it, BufferError::OutOfBounds)),
        };
    }
    let mut desc = buf.desc();
    desc.check(all.len()).map_err(|e| buffer_error(it, e))?;
    if order == 'F' {
        desc.shape.reverse();
        desc.strides.reverse();
    }
    Ok(desc.gather(&all))
}

fn fmt_equal(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (None, other) | (other, None) => other.as_deref().map_or(true, |s| s == "B"),
        (Some(x), Some(y)) => x == y,
    }
}

fn arrays_equal<T: PartialEq + Copy>(a: &[T], b: &[T], shape: Option<&[usize]>) -> bool {
    a.iter().zip(b).enumerate().all(|(i, (x, y))| shape.is_some_and(|s| s.get(i).is_some_and(|&n| n <= 1)) || x == y)
}

fn cmp_contig(it: &mut Interp, b1: &Value, b2: &Value) -> R<bool> {
    let v1 = get_buffer(it, b1, PYBUF_FULL_RO)
        .map_err(|_| it.type_error("cmp_contig: first argument does not implement the buffer protocol"))?;
    let v2 = get_buffer(it, b2, PYBUF_FULL_RO)
        .map_err(|_| it.type_error("cmp_contig: second argument does not implement the buffer protocol"))?;
    let both = |o: char| is_contig(&v1, o) && is_contig(&v2, o);
    if !both('C') && !both('F') {
        return Ok(false);
    }
    if v1.len != v2.len
        || v1.itemsize != v2.itemsize
        || v1.ndim != v2.ndim
        || !fmt_equal(&v1.format, &v2.format)
        || v1.shape.is_some() != v2.shape.is_some()
        || v1.strides.is_some() != v2.strides.is_some()
        || v1.suboffsets != v2.suboffsets
    {
        return Ok(false);
    }
    if let (Some(a), Some(b)) = (&v1.shape, &v2.shape) {
        if !arrays_equal(a, b, None) {
            return Ok(false);
        }
    }
    if let (Some(a), Some(b)) = (&v1.strides, &v2.strides) {
        if !arrays_equal(a, b, v1.shape.as_deref()) {
            return Ok(false);
        }
    }
    let (x, y) = (v1.all_bytes(it)?, v2.all_bytes(it)?);
    let (r1, r2) = (x.get(v1.offset..v1.offset + v1.len), y.get(v2.offset..v2.offset + v2.len));
    Ok(r1.is_some() && r1 == r2)
}

fn get_contiguous(it: &mut Interp, obj: &Value, buffertype: &Value, order: &Value) -> R<Value> {
    if !buffertype.is_int_like() {
        return Err(it.type_error("buffertype must be PyBUF_READ or PyBUF_WRITE"));
    }
    let kind = it.seq_index(buffertype)?;
    if kind != PYBUF_READ && kind != PYBUF_WRITE {
        return Err(it.value_error("invalid buffer type"));
    }
    let ord = get_ascii_order(it, order)?;
    let buf = get_buffer(it, obj, PYBUF_FULL_RO)?;
    if kind == PYBUF_WRITE && buf.readonly {
        return Err(it.type_error("underlying buffer is not writable"));
    }
    if is_contig(&buf, ord) {
        if super::memview::is_memoryview(it, obj) {
            return Ok(obj.clone());
        }
        return Ok(buf_view(it, buf));
    }
    if kind == PYBUF_WRITE {
        return Err(it.type_error("writable contiguous buffer requested for a non-contiguous object."));
    }
    let data = to_contiguous(it, &buf, ord)?;
    let shape = buf.shape_vec();
    let strides = if ord == 'F' { f_strides(&shape, buf.itemsize) } else { c_strides(&shape, buf.itemsize) };
    let store = Rc::new(ByteStore::new(data).readonly());
    let export = store.export().map_err(|e| buffer_error(it, e))?;
    let mut desc = buf.desc();
    desc.offset = 0;
    desc.strides = strides;
    desc.readonly = true;
    let fmt: Rc<str> = buf.fmt().into();
    Ok(view_from_parts(it, buf.obj.unwrap_or(Value::None), Source::Store(export), desc, fmt))
}

#[lumen_bind::module(name = "_testbuffer")]
pub mod testbuffer {
    use super::*;

    #[op]
    fn slice_indices(it: &mut Interp, key: &Value, len: i64) -> R<Value> {
        if !it.is_slice(key) {
            return Err(it.type_error("first argument must be a slice object"));
        }
        let (start, stop, step) = slice_parts(it, key)?;
        let (start, stop, n) = adjust_slice_bounds(len.max(0) as usize, start, stop, step);
        Ok(tuple_of(&[start as i64, stop as i64, step as i64, n as i64]))
    }

    #[op]
    fn get_pointer(it: &mut Interp, bufobj: &Value, seq: &Value) -> R<Value> {
        let indices = list_or_tuple(it, seq, "seq")?;
        let view = get_buffer(it, bufobj, PYBUF_FULL_RO)?;
        if view.ndim > ND_MAX_NDIM {
            return Err(it.value_error(&format!("get_pointer(): ndim > {ND_MAX_NDIM}")));
        }
        if indices.len() != view.ndim {
            return Err(it.value_error("get_pointer(): len(indices) != ndim"));
        }
        let shape = view.shape_vec();
        let strides = view.strides_vec();
        let mut pos = view.offset as isize;
        for (i, x) in indices.iter().enumerate() {
            let n = index_arg(it, x)?;
            if n < 0 || n as usize >= shape[i] {
                return Err(it.value_error(&format!("get_pointer(): invalid index {n} at position {i}")));
            }
            pos += strides[i] * n;
        }
        unpack_at(it, &view, pos)
    }

    #[op]
    fn get_sizeof_void_p() -> i64 {
        std::mem::size_of::<*const u8>() as i64
    }

    #[op]
    fn get_contiguous(it: &mut Interp, obj: &Value, buffertype: &Value, order: &Value) -> R<Value> {
        super::get_contiguous(it, obj, buffertype, order)
    }

    #[op]
    fn py_buffer_to_contiguous(it: &mut Interp, obj: &Value, order: &Value, flags: i64) -> R<Value> {
        let view = get_buffer(it, obj, flags)?;
        let ord = get_ascii_order(it, order)?;
        Ok(Value::bytes(to_contiguous(it, &view, ord)?))
    }

    #[op]
    fn is_contiguous(it: &mut Interp, obj: &Value, order: &Value) -> R<bool> {
        let ord = get_ascii_order(it, order)?;
        if let Some(nd) = nd_of(it, obj) {
            return Ok(is_contig(&nd.borrow(it)?.head().buf, ord));
        }
        match get_buffer(it, obj, PYBUF_FULL_RO) {
            Ok(view) => Ok(is_contig(&view, ord)),
            Err(_) => Err(it.type_error("is_contiguous: object does not implement the buffer protocol")),
        }
    }

    #[op]
    fn cmp_contig(it: &mut Interp, b1: &Value, b2: &Value) -> R<bool> {
        super::cmp_contig(it, b1, b2)
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        dict_set_str(&d, "ndarray", Value::Obj(type_object::<NDArray>(it)));
        dict_set_str(&d, "staticarray", Value::Obj(type_object::<StaticArray>(it)));
        let ints: [(&str, i64); 28] = [
            ("ND_MAX_NDIM", ND_MAX_NDIM as i64),
            ("ND_VAREXPORT", ND_VAREXPORT),
            ("ND_WRITABLE", ND_WRITABLE),
            ("ND_FORTRAN", ND_FORTRAN),
            ("ND_SCALAR", ND_SCALAR),
            ("ND_PIL", ND_PIL),
            ("ND_GETBUF_FAIL", ND_GETBUF_FAIL),
            ("ND_GETBUF_UNDEFINED", ND_GETBUF_UNDEFINED),
            ("ND_REDIRECT", ND_REDIRECT),
            ("PyBUF_SIMPLE", 0),
            ("PyBUF_WRITABLE", PYBUF_WRITABLE),
            ("PyBUF_FORMAT", PYBUF_FORMAT),
            ("PyBUF_ND", PYBUF_ND),
            ("PyBUF_STRIDES", PYBUF_STRIDES),
            ("PyBUF_INDIRECT", PYBUF_INDIRECT),
            ("PyBUF_C_CONTIGUOUS", PYBUF_C_CONTIGUOUS),
            ("PyBUF_F_CONTIGUOUS", PYBUF_F_CONTIGUOUS),
            ("PyBUF_ANY_CONTIGUOUS", PYBUF_ANY_CONTIGUOUS),
            ("PyBUF_FULL", PYBUF_FULL),
            ("PyBUF_FULL_RO", PYBUF_FULL_RO),
            ("PyBUF_RECORDS", PYBUF_RECORDS),
            ("PyBUF_RECORDS_RO", PYBUF_RECORDS_RO),
            ("PyBUF_STRIDED", PYBUF_STRIDED),
            ("PyBUF_STRIDED_RO", PYBUF_STRIDED_RO),
            ("PyBUF_CONTIG", PYBUF_CONTIG),
            ("PyBUF_CONTIG_RO", PYBUF_CONTIG_RO),
            ("PyBUF_READ", PYBUF_READ),
            ("PyBUF_WRITE", PYBUF_WRITE),
        ];
        for (name, value) in ints {
            dict_set_str(&d, name, Value::Int(value));
        }
    }
}
