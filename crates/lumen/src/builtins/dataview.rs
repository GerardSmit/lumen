//! Split out of builtins/mod.rs (behavior-preserving move).

use super::*;

fn dv_info(i: &mut Interp, this: &Value) -> Result<(usize, usize, usize, bool), Value> {
    let ptr =
        map_ptr(this).ok_or_else(|| i.make_error("TypeError", "receiver is not a DataView"))?;
    i.data_views
        .get(&ptr)
        .copied()
        .ok_or_else(|| i.make_error("TypeError", "receiver is not a DataView"))
}

/// The DataView's current byte length, or `None` when its buffer is detached or (over a resizable
/// buffer) the view is now out of bounds.
/// Read `n` raw bytes of a (possibly shared) buffer.
fn dv_bytes(i: &Interp, buf: usize, start: usize, n: usize) -> Option<Vec<u8>> {
    if let Some(&id) = i.shared_buffers.get(&buf) {
        return crate::interpreter::shared_mem_get(id).and_then(|m| {
            let g = m.lock().unwrap();
            (start + n <= g.len()).then(|| g[start..start + n].to_vec())
        });
    }
    i.array_buffers
        .get(&buf)
        .and_then(|b| (start + n <= b.len()).then(|| b[start..start + n].to_vec()))
}

/// Write raw bytes into a (possibly shared) buffer (bounds-checked, silently dropped otherwise).
fn dv_put(i: &mut Interp, buf: usize, start: usize, bytes: &[u8]) {
    if let Some(&id) = i.shared_buffers.get(&buf) {
        if let Some(m) = crate::interpreter::shared_mem_get(id) {
            let mut g = m.lock().unwrap();
            if start + bytes.len() <= g.len() {
                g[start..start + bytes.len()].copy_from_slice(bytes);
            }
        }
        return;
    }
    if let Some(b) = i.array_buffers.get_mut(&buf) {
        if start + bytes.len() <= b.len() {
            b[start..start + bytes.len()].copy_from_slice(bytes);
        }
    }
}

fn dv_view_len(i: &Interp, buf: usize, off: usize, len: usize, track: bool) -> Option<usize> {
    let blen = i.array_buffers.get(&buf)?.len();
    if track {
        if off > blen {
            None
        } else {
            Some(blen - off)
        }
    } else if off + len > blen {
        None
    } else {
        Some(len)
    }
}
fn dv_buffer_get(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    dv_info(i, &this)?;
    ab(i.get_member(&this, "__dv_buffer"))
}
fn dv_bytelength_get(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let (buf, off, len, track) = dv_info(i, &this)?;
    match dv_view_len(i, buf, off, len, track) {
        Some(l) => Ok(Value::Num(l as f64)),
        None => Err(i.make_error(
            "TypeError",
            "DataView's buffer is detached or out of bounds",
        )),
    }
}
fn dv_byteoffset_get(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let (buf, off, len, track) = dv_info(i, &this)?;
    if dv_view_len(i, buf, off, len, track).is_none() {
        return Err(i.make_error(
            "TypeError",
            "DataView's buffer is detached or out of bounds",
        ));
    }
    Ok(Value::Num(off as f64))
}

fn dv_get(i: &mut Interp, this: &Value, args: &[Value], kind: TaKind) -> Result<Value, Value> {
    let ptr = map_ptr(this).ok_or_else(|| i.make_error("TypeError", "not a DataView"))?;
    let (buf, off, len, track) = *i
        .data_views
        .get(&ptr)
        .ok_or_else(|| i.make_error("TypeError", "not a DataView"))?;
    let byte_off = to_index(i, &arg(args, 0))?;
    let little = i.to_boolean(&arg(args, 1));
    let es = kind.elsize();
    // GetViewByteLength: re-derived after coercion (a detached/out-of-bounds view throws TypeError).
    let vlen = dv_view_len(i, buf, off, len, track).ok_or_else(|| {
        i.make_error(
            "TypeError",
            "DataView's buffer is detached or out of bounds",
        )
    })?;
    if byte_off.checked_add(es).is_none_or(|e| e > vlen) {
        return Err(i.make_error("RangeError", "Offset is outside the bounds of the DataView"));
    }
    let start = off + byte_off;
    let mut b = match dv_bytes(i, buf, start, es) {
        Some(b) => b,
        None => return Err(i.make_error("TypeError", "detached buffer")),
    };
    // `kind.read` is little-endian; DataView defaults to big-endian, so reverse unless little.
    if !little {
        b.reverse();
    }
    Ok(Value::Num(kind.read(&b)))
}

fn dv_set(i: &mut Interp, this: &Value, args: &[Value], kind: TaKind) -> Result<Value, Value> {
    let ptr = map_ptr(this).ok_or_else(|| i.make_error("TypeError", "not a DataView"))?;
    let (buf, off, len, track) = *i
        .data_views
        .get(&ptr)
        .ok_or_else(|| i.make_error("TypeError", "not a DataView"))?;
    // IsImmutableBuffer is checked before ToIndex(byteOffset)/ToNumber(value) read any arguments.
    if i.immutable_buffers.contains(&buf) {
        return Err(i.make_error("TypeError", "Cannot write to an immutable ArrayBuffer"));
    }
    let byte_off = to_index(i, &arg(args, 0))?;
    let value = ab(i.to_number(&arg(args, 1)))?;
    let little = i.to_boolean(&arg(args, 2));
    // Coercing the index/value can detach or resize the buffer — re-derive the view length.
    let vlen = dv_view_len(i, buf, off, len, track).ok_or_else(|| {
        i.make_error(
            "TypeError",
            "DataView's buffer is detached or out of bounds",
        )
    })?;
    let es = kind.elsize();
    if byte_off.checked_add(es).is_none_or(|e| e > vlen) {
        return Err(i.make_error("RangeError", "Offset is outside the bounds of the DataView"));
    }
    let mut bytes = kind.write(value);
    if !little {
        bytes.reverse();
    }
    let start = off + byte_off;
    dv_put(i, buf, start, &bytes);
    Ok(Value::Undefined)
}

fn dv_get_big(i: &mut Interp, this: &Value, args: &[Value], signed: bool) -> Result<Value, Value> {
    let ptr = map_ptr(this).ok_or_else(|| i.make_error("TypeError", "not a DataView"))?;
    let (buf, off, len, track) = *i
        .data_views
        .get(&ptr)
        .ok_or_else(|| i.make_error("TypeError", "not a DataView"))?;
    let byte_off = to_index(i, &arg(args, 0))?;
    let little = i.to_boolean(&arg(args, 1));
    let vlen = dv_view_len(i, buf, off, len, track).ok_or_else(|| {
        i.make_error(
            "TypeError",
            "DataView's buffer is detached or out of bounds",
        )
    })?;
    if byte_off.checked_add(8).is_none_or(|e| e > vlen) {
        return Err(i.make_error("RangeError", "Offset is outside the bounds of the DataView"));
    }
    let start = off + byte_off;
    let mut b = match dv_bytes(i, buf, start, 8) {
        Some(b) => b,
        None => return Err(i.make_error("TypeError", "detached buffer")),
    };
    if !little {
        b.reverse();
    }
    let raw = u64::from_le_bytes(b.try_into().unwrap());
    Ok(Value::BigInt(if signed {
        crate::bigint::JsBigInt::from_i128(raw as i64 as i128)
    } else {
        crate::bigint::JsBigInt::from_u64(raw)
    }))
}

fn dv_set_big(i: &mut Interp, this: &Value, args: &[Value]) -> Result<Value, Value> {
    let ptr = map_ptr(this).ok_or_else(|| i.make_error("TypeError", "not a DataView"))?;
    let (buf, off, len, track) = *i
        .data_views
        .get(&ptr)
        .ok_or_else(|| i.make_error("TypeError", "not a DataView"))?;
    if i.immutable_buffers.contains(&buf) {
        return Err(i.make_error("TypeError", "Cannot write to an immutable ArrayBuffer"));
    }
    let byte_off = to_index(i, &arg(args, 0))?;
    let value = ab(i.to_bigint(&arg(args, 1)))?;
    let little = i.to_boolean(&arg(args, 2));
    let vlen = dv_view_len(i, buf, off, len, track).ok_or_else(|| {
        i.make_error(
            "TypeError",
            "DataView's buffer is detached or out of bounds",
        )
    })?;
    if byte_off.checked_add(8).is_none_or(|e| e > vlen) {
        return Err(i.make_error("RangeError", "Offset is outside the bounds of the DataView"));
    }
    let mut bytes = (value.to_i128_wrapping() as u64).to_le_bytes().to_vec();
    if !little {
        bytes.reverse();
    }
    let start = off + byte_off;
    dv_put(i, buf, start, &bytes);
    Ok(Value::Undefined)
}

macro_rules! dv_natives {
    ($(($get:ident, $set:ident, $kind:expr)),* $(,)?) => {
        $(
            fn $get(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
                dv_get(i, &this, a, $kind)
            }
            fn $set(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
                dv_set(i, &this, a, $kind)
            }
        )*
    };
}
dv_natives!(
    (dv_get_i8, dv_set_i8, TaKind::I8),
    (dv_get_u8, dv_set_u8, TaKind::U8),
    (dv_get_i16, dv_set_i16, TaKind::I16),
    (dv_get_u16, dv_set_u16, TaKind::U16),
    (dv_get_i32, dv_set_i32, TaKind::I32),
    (dv_get_u32, dv_set_u32, TaKind::U32),
    (dv_get_f32, dv_set_f32, TaKind::F32),
    (dv_get_f64, dv_set_f64, TaKind::F64),
);

/// The `DataView.prototype` get/set methods compiled code runs inline (every numeric kind but
/// Float16): name, native code, element kind, whether it stores.
const DV_METHODS: &[(&str, crate::value::NativeFn, TaKind, bool)] = &[
    ("getInt8", dv_get_i8, TaKind::I8, false),
    ("setInt8", dv_set_i8, TaKind::I8, true),
    ("getUint8", dv_get_u8, TaKind::U8, false),
    ("setUint8", dv_set_u8, TaKind::U8, true),
    ("getInt16", dv_get_i16, TaKind::I16, false),
    ("setInt16", dv_set_i16, TaKind::I16, true),
    ("getUint16", dv_get_u16, TaKind::U16, false),
    ("setUint16", dv_set_u16, TaKind::U16, true),
    ("getInt32", dv_get_i32, TaKind::I32, false),
    ("setInt32", dv_set_i32, TaKind::I32, true),
    ("getUint32", dv_get_u32, TaKind::U32, false),
    ("setUint32", dv_set_u32, TaKind::U32, true),
    ("getFloat32", dv_get_f32, TaKind::F32, false),
    ("setFloat32", dv_set_f32, TaKind::F32, true),
    ("getFloat64", dv_get_f64, TaKind::F64, false),
    ("setFloat64", dv_set_f64, TaKind::F64, true),
];

/// For a method name compiled code may run inline (see [`DV_METHODS`]): its element kind and
/// whether it stores.
pub(crate) fn dv_inline_method(name: &str) -> Option<(TaKind, bool)> {
    DV_METHODS
        .iter()
        .find(|m| m.0 == name)
        .map(|&(_, _, k, set)| (k, set))
}

/// The bytes compiled code may access for `obj.<name>(...)` where `name` is one of
/// [`DV_METHODS`]: when `obj` is a DataView over an unshared (for a store, also mutable),
/// attached buffer, in bounds, and reading `name` on it finds the intrinsic method as a plain
/// data property of an ordinary prototype (none on `obj` itself) in a one-realm engine — then
/// calling the method with a Number offset in bounds (and a Number value) is exactly a load or
/// store at the view. Returns the address of the view's byte 0 and its byte length. Pure. The
/// view stays valid until JS runs (only JS can detach, resize or transfer the buffer, or change
/// the prototype chain) or the object is collected.
pub(crate) fn dv_jit_view(i: &mut Interp, obj: &Gc, name: &str) -> Option<(*mut u8, usize)> {
    if i.multi_realm() {
        return None;
    }
    let &(_, want, _, set) = DV_METHODS.iter().find(|m| m.0 == name)?;
    let &(buf, off, len, track) = i.data_views.get(&(Gc::as_ptr(obj) as usize))?;
    if !i.shared_buffers.is_empty() && i.shared_buffers.contains_key(&buf) {
        return None;
    }
    if set && !i.immutable_buffers.is_empty() && i.immutable_buffers.contains(&buf) {
        return None;
    }
    // The method: an inherited data property holding the intrinsic native.
    let mut cur = obj.clone();
    let mut own = true;
    loop {
        let next = {
            let b = cur.try_borrow().ok()?;
            if !matches!(b.exotic, crate::value::Exotic::None) {
                return None;
            }
            if let Some(k) = b.props.slot_of(name) {
                if own {
                    return None;
                }
                let p = b.props.entry_at(k)?;
                if p.accessor() {
                    return None;
                }
                let Value::Obj(fo) = p.value() else { return None };
                let fb = fo.try_borrow().ok()?;
                match fb.call {
                    crate::value::Callable::Native(fp) if fp as usize == want as usize => break,
                    _ => return None,
                }
            }
            b.proto.clone()?
        };
        cur = next;
        own = false;
    }
    let vlen = dv_view_len(i, buf, off, len, track)?;
    let bytes: &mut [u8] = i.array_buffers.get_mut(&buf)?;
    if off.checked_add(vlen).is_none_or(|e| e > bytes.len()) {
        return None;
    }
    // SAFETY: `off + vlen <= bytes.len()`.
    Some((unsafe { bytes.as_mut_ptr().add(off) }, vlen))
}

/// `obj.byteLength` without running JS, when reading it calls the intrinsic ArrayBuffer or
/// DataView getter (an inherited accessor of an ordinary prototype chain, none on `obj` itself,
/// in a one-realm engine) and that getter returns without throwing. Pure. Only JS can change the
/// result (resize, transfer, detach, or a prototype change).
pub(crate) fn jit_byte_length(i: &Interp, obj: &Gc) -> Option<usize> {
    if i.multi_realm() {
        return None;
    }
    let mut cur = obj.clone();
    let mut own = true;
    let fp = loop {
        let next = {
            let b = cur.try_borrow().ok()?;
            if !matches!(b.exotic, crate::value::Exotic::None) {
                return None;
            }
            if let Some(k) = b.props.slot_of("byteLength") {
                if own {
                    return None;
                }
                let p = b.props.entry_at(k)?;
                if !p.accessor() {
                    return None;
                }
                let Some(Value::Obj(g)) = p.getter() else { return None };
                let gb = g.try_borrow().ok()?;
                match gb.call {
                    crate::value::Callable::Native(fp) => break fp as usize,
                    _ => return None,
                }
            }
            b.proto.clone()?
        };
        cur = next;
        own = false;
    };
    let p = Gc::as_ptr(obj) as usize;
    if fp == super::typedarray::ab_bytelength_get as usize {
        if i.shared_buffers.contains_key(&p) || !obj.try_borrow().ok()?.props.contains("__abMaxByteLength") {
            return None;
        }
        Some(i.array_buffers.get(&p).map_or(0, |b| b.len()))
    } else if fp == dv_bytelength_get as usize {
        let &(buf, off, len, track) = i.data_views.get(&p)?;
        dv_view_len(i, buf, off, len, track)
    } else {
        None
    }
}

pub(super) fn install_dataview(it: &mut Interp) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("DataView", proto.clone());
    // buffer / byteLength / byteOffset are brand-checked accessor getters; byteLength/byteOffset
    // additionally throw if the backing buffer has been detached.
    for (name, getter) in [
        (
            "buffer",
            dv_buffer_get as fn(&mut Interp, Value, &[Value]) -> Result<Value, Value>,
        ),
        ("byteLength", dv_bytelength_get),
        ("byteOffset", dv_byteoffset_get),
    ] {
        let g = it.make_native(&format!("get {name}"), 0, getter);
        proto.borrow_mut().props.insert(
            name,
            Property::accessor_prop(Some(Value::Obj(g)), None, false, true),
        );
    }
    // DataView.prototype[@@toStringTag] = "DataView" (non-writable, non-enumerable, configurable).
    set_to_string_tag(it, &proto, "DataView");
    for &(name, f, _, set) in DV_METHODS {
        it.def_method(&proto, name, if set { 2 } else { 1 }, f);
    }
    it.def_method(&proto, "getFloat16", 1, |i, this, a| dv_get(i, &this, a, TaKind::F16));
    it.def_method(&proto, "setFloat16", 2, |i, this, a| dv_set(i, &this, a, TaKind::F16));
    it.def_method(&proto, "getBigInt64", 1, |i, this, a| {
        dv_get_big(i, &this, a, true)
    });
    it.def_method(&proto, "getBigUint64", 1, |i, this, a| {
        dv_get_big(i, &this, a, false)
    });
    it.def_method(&proto, "setBigInt64", 2, |i, this, a| {
        dv_set_big(i, &this, a)
    });
    it.def_method(&proto, "setBigUint64", 2, |i, this, a| {
        dv_set_big(i, &this, a)
    });

    let ctor = it.make_native("DataView", 1, |i, _t, a| {
        if !i.constructing {
            return Err(i.make_error("TypeError", "DataView constructor requires 'new'"));
        }
        // An ArrayBuffer object is identified by its [[ArrayBufferData]] slot (the internal
        // `__abMaxByteLength` marker), which survives detachment — a detached buffer is still an
        // ArrayBuffer, so ToNumber(byteOffset) must run before the detached check throws.
        let (bv, bp) = match arg(a, 0) {
            Value::Obj(o) if o.borrow().props.contains("__abMaxByteLength") => {
                (Value::Obj(o.clone()), Gc::as_ptr(&o) as usize)
            }
            _ => return Err(i.make_error("TypeError", "DataView requires an ArrayBuffer")),
        };
        // ToIndex(byteOffset) may run user code that detaches/resizes the buffer.
        let offset = match arg(a, 1) {
            Value::Undefined => 0,
            v => to_index(i, &v)?,
        };
        let has_len = !matches!(arg(a, 2), Value::Undefined);
        let len_arg = if has_len {
            Some(to_index(i, &arg(a, 2))?)
        } else {
            None
        };
        // Re-read the (possibly mutated) buffer state after all coercions.
        if !i.array_buffers.contains_key(&bp) {
            return Err(i.make_error("TypeError", "ArrayBuffer is detached"));
        }
        let buflen = i.array_buffers[&bp].len();
        if offset > buflen {
            return Err(i.make_error("RangeError", "DataView byteOffset is out of bounds"));
        }
        let rv = ab(i.get_member(&bv, "resizable"))?;
        let resizable = i.to_boolean(&rv);
        if let Some(l) = len_arg {
            if offset + l > buflen {
                return Err(i.make_error("RangeError", "DataView byteLength is out of bounds"));
            }
        }
        // OrdinaryCreateFromConstructor does Get(newTarget, "prototype"), which can run a custom
        // proto getter that detaches or resizes the buffer — re-validate everything afterwards.
        let obj = new_from_ctor(i, "DataView")?;
        if !i.array_buffers.contains_key(&bp) {
            return Err(i.make_error("TypeError", "ArrayBuffer is detached"));
        }
        let buflen = i.array_buffers[&bp].len();
        if offset > buflen {
            return Err(i.make_error("RangeError", "DataView byteOffset is out of bounds"));
        }
        let len = match len_arg {
            Some(l) => {
                if offset + l > buflen {
                    return Err(i.make_error("RangeError", "DataView byteLength is out of bounds"));
                }
                l
            }
            None => buflen - offset,
        };
        // A length-tracking DataView (no explicit byteLength) over a resizable buffer follows the
        // buffer's current length; its stored `len` is only the initial snapshot.
        let track = !has_len && resizable;
        let p = Gc::as_ptr(&obj) as usize;
        i.gc_pin(&obj);
        i.data_views.insert(p, (bp, offset, len, track));
        // buffer/byteOffset/byteLength are accessor getters on the prototype, not own properties;
        // only the buffer object itself is kept (hidden) for the `buffer` getter.
        set_internal(&obj, "__dv_buffer", arg(a, 0));
        Ok(Value::Obj(obj))
    });
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(proto.clone()), false, false, false),
    );
    proto
        .borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    set_builtin(&it.global, "DataView", Value::Obj(ctor));
}
