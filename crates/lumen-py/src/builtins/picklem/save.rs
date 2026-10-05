//! The pickler: object traversal and the opcode stream of `_pickle.c`'s `save`, on the shared
//! output buffer and opcode tables of `lumen_common::pickle`.

use super::shared::{attr_opt, call_bound, deep_attribute, dict_to_kw, dotted_path, is_iter, pickling_error, MethodRef, Shared};
use crate::codecs;
use crate::dict::PyDict;
use crate::object::*;
use crate::pyint::BigInt;
use crate::vm::Interp;
use lumen_common::pickle::{self as pk, op as opc, Writer};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub struct PicklerCore {
    pub proto: Cell<i64>,
    pub bin: Cell<i64>,
    pub fast: Cell<i64>,
    pub fast_nesting: Cell<i64>,
    pub fix_imports: Cell<bool>,
    pub out: RefCell<Writer>,
    /// Objects already written, by `id()`: the memo slot and the object (kept alive so the id
    /// stays unique).
    pub memo: RefCell<HashMap<usize, (usize, Value)>>,
    pub fast_memo: RefCell<HashSet<usize>>,
    pub write: RefCell<Option<Value>>,
    pub pers: RefCell<Option<MethodRef>>,
    pub dispatch_table: RefCell<Option<Value>>,
    pub reducer_override: RefCell<Option<Value>>,
    pub buffer_callback: RefCell<Option<Value>>,
    pub depth: Cell<usize>,
}

impl PicklerCore {
    pub fn reset(&self) {
        {
            let mut out = self.out.borrow_mut();
            out.clear();
            out.set_framing(false);
        }
        self.memo.borrow_mut().clear();
        self.fast_memo.borrow_mut().clear();
        *self.write.borrow_mut() = None;
        *self.pers.borrow_mut() = None;
        *self.dispatch_table.borrow_mut() = None;
        *self.reducer_override.borrow_mut() = None;
        *self.buffer_callback.borrow_mut() = None;
        self.fast.set(0);
        self.fast_nesting.set(0);
        self.depth.set(0);
    }
}

pub fn set_protocol(it: &mut Interp, core: &PicklerCore, protocol: Option<&Value>, fix_imports: bool) -> R<()> {
    let proto = match protocol {
        None => pk::DEFAULT_PROTOCOL as i64,
        Some(v) if v.is_none() => pk::DEFAULT_PROTOCOL as i64,
        Some(v) => {
            let p = it.index_of(v)?;
            if p < 0 {
                pk::HIGHEST_PROTOCOL as i64
            } else if p > pk::HIGHEST_PROTOCOL as i64 {
                return Err(it.value_error(&format!("pickle protocol must be <= {}", pk::HIGHEST_PROTOCOL)));
            } else {
                p
            }
        }
    };
    core.proto.set(proto);
    core.bin.set((proto > 0) as i64);
    core.fix_imports.set(fix_imports && proto < 3);
    Ok(())
}

pub fn set_buffer_callback(it: &mut Interp, core: &PicklerCore, cb: Option<&Value>) -> R<()> {
    let cb = cb.filter(|v| !v.is_none());
    if cb.is_some() && core.proto.get() < 5 {
        return Err(it.value_error("buffer_callback needs protocol >= 5"));
    }
    *core.buffer_callback.borrow_mut() = cb.cloned();
    Ok(())
}

fn opt(args: &[Value], i: usize) -> Option<&Value> {
    args.get(i).filter(|v| !v.is_none())
}

fn ptr_of(v: &Value) -> usize {
    match v {
        Value::Obj(o) => std::rc::Rc::as_ptr(o) as *const u8 as usize,
        _ => 0,
    }
}

enum Exact {
    Dict,
    Set,
    FrozenSet,
    List,
    Tuple,
    ByteArray,
    PickleBuffer,
    Other,
}

pub struct Pk<'a> {
    pub p: &'a PicklerCore,
    pub sh: &'a Shared,
    pub slf: Option<&'a Value>,
}

impl<'a> Pk<'a> {
    fn proto(&self) -> i64 {
        self.p.proto.get()
    }

    fn bin(&self) -> bool {
        self.p.bin.get() != 0
    }

    fn w(&self, data: &[u8]) {
        self.p.out.borrow_mut().write(data);
    }

    fn wb(&self, b: u8) {
        self.p.out.borrow_mut().write_byte(b);
    }

    fn perr(&self, it: &mut Interp, msg: &str) -> Obj {
        pickling_error(it, self.sh, msg)
    }

    pub fn flush_to_file(&self, it: &mut Interp) -> R<()> {
        let data = self.p.out.borrow_mut().take();
        let write = self.p.write.borrow().clone();
        if let Some(w) = write {
            it.call(&w, vec![Value::bytes(data)], Vec::new())?;
        }
        Ok(())
    }

    fn boundary(&self, it: &mut Interp) -> R<()> {
        let full = self.p.out.borrow().frame_full();
        if full {
            self.p.out.borrow_mut().commit_frame();
            if self.p.write.borrow().is_some() {
                self.flush_to_file(it)?;
            }
        }
        Ok(())
    }

    fn enter(&self, it: &mut Interp) -> R<()> {
        let d = self.p.depth.get() + 1;
        if it.frames.len() + d >= it.recursion_limit || lumen_common::stack::exhausted() {
            return Err(it.new_exc_str("RecursionError", "maximum recursion depth exceeded while pickling an object"));
        }
        self.p.depth.set(d);
        Ok(())
    }

    fn leave(&self) {
        self.p.depth.set(self.p.depth.get().saturating_sub(1));
    }

    fn in_memo(&self, it: &Interp, obj: &Value) -> bool {
        self.p.memo.borrow().contains_key(&it.id_of(obj))
    }

    fn memo_get(&self, it: &mut Interp, obj: &Value) -> R<()> {
        let idx = self.p.memo.borrow().get(&it.id_of(obj)).map(|e| e.0);
        let Some(idx) = idx else {
            let k = it.exc_type("KeyError");
            return Err(it.new_exc(&k, vec![obj.clone()]));
        };
        match pk::memo_get_op(self.bin(), idx) {
            Ok(op) => {
                self.w(&op);
                Ok(())
            }
            Err(m) => Err(self.perr(it, m)),
        }
    }

    fn memo_put(&self, it: &mut Interp, obj: &Value) -> R<()> {
        if self.p.fast.get() != 0 {
            return Ok(());
        }
        let idx = self.p.memo.borrow().len();
        self.p.memo.borrow_mut().insert(it.id_of(obj), (idx, obj.clone()));
        match pk::memo_put_op(self.proto() as u8, self.bin(), idx) {
            Ok(op) => {
                self.w(&op);
                Ok(())
            }
            Err(m) => Err(self.perr(it, m)),
        }
    }

    fn fast_enter(&self, it: &mut Interp, obj: &Value) -> R<()> {
        let n = self.p.fast_nesting.get() + 1;
        self.p.fast_nesting.set(n);
        if n >= pk::FAST_NESTING_LIMIT as i64 {
            let key = ptr_of(obj);
            if !self.p.fast_memo.borrow_mut().insert(key) {
                self.p.fast_nesting.set(-1);
                let t = it.tp_name_of(obj);
                return Err(it.value_error(&format!("fast mode: can't pickle cyclic objects including object type {} at {:#x}", t, key)));
            }
        }
        Ok(())
    }

    fn fast_leave(&self, obj: &Value) {
        let n = self.p.fast_nesting.get();
        self.p.fast_nesting.set(n - 1);
        if n >= pk::FAST_NESTING_LIMIT as i64 {
            self.p.fast_memo.borrow_mut().remove(&ptr_of(obj));
        }
    }

    pub fn dump(&self, it: &mut Interp, obj: &Value) -> R<()> {
        let ro = match self.slf {
            Some(s) => attr_opt(it, s, "reducer_override")?,
            None => None,
        };
        *self.p.reducer_override.borrow_mut() = ro;
        let r = self.dump_body(it, obj);
        self.p.out.borrow_mut().set_framing(false);
        *self.p.reducer_override.borrow_mut() = None;
        r
    }

    fn dump_body(&self, it: &mut Interp, obj: &Value) -> R<()> {
        if self.proto() >= 2 {
            self.w(&[opc::PROTO, self.proto() as u8]);
            if self.proto() >= 4 {
                self.p.out.borrow_mut().set_framing(true);
            }
        }
        self.save(it, obj, false)?;
        self.wb(opc::STOP);
        self.p.out.borrow_mut().commit_frame();
        Ok(())
    }

    pub fn save(&self, it: &mut Interp, obj: &Value, pers_save: bool) -> R<()> {
        self.boundary(it)?;
        if !pers_save && self.p.pers.borrow().is_some() && self.save_pers(it, obj)? {
            return Ok(());
        }
        match obj {
            Value::None => {
                self.wb(opc::NONE);
                return Ok(());
            }
            Value::Bool(b) => return self.save_bool(*b),
            Value::Int(_) => return self.save_long(it, obj),
            Value::Float(f) => return self.save_float(*f),
            Value::Obj(o) if o.cls.is_none() => match &o.kind {
                Kind::Int(_) => return self.save_long(it, obj),
                Kind::Float(f) => return self.save_float(*f),
                _ => {}
            },
            _ => {}
        }
        if self.in_memo(it, obj) {
            return self.memo_get(it, obj);
        }
        if let Value::Obj(o) = obj {
            if o.cls.is_none() {
                match &o.kind {
                    Kind::Bytes(_) => return self.save_bytes(it, obj),
                    Kind::Str(_) => return self.save_unicode(it, obj),
                    _ => {}
                }
            }
        }
        self.enter(it)?;
        let r = self.save_object(it, obj);
        self.leave();
        r
    }

    fn save_object(&self, it: &mut Interp, obj: &Value) -> R<()> {
        let ty = it.type_of(obj);
        let exact = {
            let t = &it.types;
            if std::rc::Rc::ptr_eq(&ty, &t.dict) {
                Exact::Dict
            } else if std::rc::Rc::ptr_eq(&ty, &t.set) {
                Exact::Set
            } else if std::rc::Rc::ptr_eq(&ty, &t.frozenset) {
                Exact::FrozenSet
            } else if std::rc::Rc::ptr_eq(&ty, &t.list) {
                Exact::List
            } else if std::rc::Rc::ptr_eq(&ty, &t.tuple) {
                Exact::Tuple
            } else if std::rc::Rc::ptr_eq(&ty, &t.bytearray) {
                Exact::ByteArray
            } else if super::is_pickle_buffer(it, obj) {
                Exact::PickleBuffer
            } else {
                Exact::Other
            }
        };
        match exact {
            Exact::Dict => return self.save_dict(it, obj),
            Exact::Set => return self.save_set(it, obj),
            Exact::FrozenSet => return self.save_frozenset(it, obj),
            Exact::List => return self.save_list(it, obj),
            Exact::Tuple => return self.save_tuple(it, obj),
            Exact::ByteArray => return self.save_bytearray(it, obj),
            Exact::PickleBuffer => return self.save_picklebuffer(it, obj),
            Exact::Other => {}
        }

        let mut reduce_value: Option<Value> = None;
        let ro = self.p.reducer_override.borrow().clone();
        if let Some(ro) = ro {
            let r = it.call(&ro, vec![obj.clone()], Vec::new())?;
            if !matches!(r, Value::NotImplemented) {
                reduce_value = Some(r);
            }
        }

        let rv = match reduce_value {
            Some(v) => v,
            None => {
                if std::rc::Rc::ptr_eq(&ty, &it.types.type_) {
                    return self.save_type(it, obj);
                }
                if std::rc::Rc::ptr_eq(&ty, &it.types.function) {
                    return self.save_global(it, obj, None);
                }
                let key = Value::Obj(ty.clone());
                let dt = self.p.dispatch_table.borrow().clone();
                let reduce_func = match dt {
                    None => it.dict_get(&self.sh.dispatch_table, &key)?,
                    Some(dt) => match it.getitem(&dt, &key) {
                        Ok(f) => Some(f),
                        Err(e) if it.exc_is(&e, "KeyError") => None,
                        Err(e) => return Err(e),
                    },
                };
                if let Some(f) = reduce_func {
                    it.call(&f, vec![obj.clone()], Vec::new())?
                } else if it.is_subtype(&ty, &it.types.type_) {
                    return self.save_global(it, obj, None);
                } else if let Some(f) = attr_opt(it, obj, "__reduce_ex__")? {
                    it.call(&f, vec![Value::Int(self.proto())], Vec::new())?
                } else if let Some(f) = attr_opt(it, obj, "__reduce__")? {
                    it.call(&f, Vec::new(), Vec::new())?
                } else {
                    let t = it.tp_name_of(obj);
                    let r = it.repr_of(obj)?;
                    return Err(self.perr(it, &format!("can't pickle '{}' object: {}", t, r)));
                }
            }
        };

        if rv.as_str().is_some() {
            return self.save_global(it, obj, Some(&rv));
        }
        let Some(items) = rv.tuple_items() else {
            return Err(self.perr(it, "__reduce__ must return a string or tuple"));
        };
        let items = items.to_vec();
        self.save_reduce(it, &items, Some(obj))
    }

    fn save_pers(&self, it: &mut Interp, obj: &Value) -> R<bool> {
        let Some(f) = self.p.pers.borrow().clone() else { return Ok(false) };
        let pid = call_bound(it, &f, self.slf, obj.clone())?;
        if pid.is_none() {
            return Ok(false);
        }
        if self.bin() {
            self.save(it, &pid, true)?;
            self.wb(opc::BINPERSID);
        } else {
            let s = it.str_of(&pid)?;
            if !s.is_ascii() {
                return Err(self.perr(it, "persistent IDs in protocol 0 must be ASCII strings"));
            }
            self.wb(opc::PERSID);
            self.w(s.as_bytes());
            self.wb(b'\n');
        }
        Ok(true)
    }

    fn save_bool(&self, b: bool) -> R<()> {
        if self.proto() >= 2 {
            self.wb(if b { opc::NEWTRUE } else { opc::NEWFALSE });
        } else {
            self.w(if b { b"I01\n" } else { b"I00\n" });
        }
        Ok(())
    }

    fn save_long(&self, it: &mut Interp, obj: &Value) -> R<()> {
        let n: BigInt = match obj.as_bigint() {
            Some(n) => n,
            None => return Err(it.type_error("int expected")),
        };
        if let Some(v) = n.to_i64() {
            if (-0x8000_0000..=0x7fff_ffff).contains(&v) {
                self.w(&pk::encode_small_int(self.bin(), v));
                return Ok(());
            }
        }
        if self.proto() >= 2 {
            let bytes = pk::encode_long(&n);
            if bytes.len() < 256 {
                self.w(&[opc::LONG1, bytes.len() as u8]);
            } else {
                self.wb(opc::LONG4);
                self.w(&(bytes.len() as u32).to_le_bytes());
            }
            self.w(&bytes);
        } else {
            let r = it.repr_of(obj)?;
            self.wb(opc::LONG);
            self.w(r.as_bytes());
            self.w(b"L\n");
        }
        Ok(())
    }

    fn save_float(&self, f: f64) -> R<()> {
        if self.bin() {
            let mut d = [0u8; 9];
            d[0] = opc::BINFLOAT;
            d[1..].copy_from_slice(&f.to_be_bytes());
            self.w(&d);
        } else {
            self.wb(opc::FLOAT);
            self.w(crate::num::float_repr(f).as_bytes());
            self.wb(b'\n');
        }
        Ok(())
    }

    /// `_Pickler_write_bytes`: a header and a payload, the payload of a large object streamed
    /// straight to the file past the output buffer.
    fn write_bytes(&self, it: &mut Interp, header: &[u8], data: &[u8], payload: Option<&Value>) -> R<()> {
        let bypass = data.len() >= pk::FRAME_SIZE_TARGET;
        let framing = self.p.out.borrow().framing();
        if bypass {
            let mut out = self.p.out.borrow_mut();
            out.commit_frame();
            out.set_framing(false);
        }
        self.w(header);
        let write = self.p.write.borrow().clone();
        match write {
            Some(w) if bypass => {
                self.flush_to_file(it)?;
                let pl = match payload {
                    Some(p) => p.clone(),
                    None => Value::bytes(data.to_vec()),
                };
                it.call(&w, vec![pl], Vec::new())?;
                self.p.out.borrow_mut().clear();
            }
            _ => self.w(data),
        }
        self.p.out.borrow_mut().set_framing(framing);
        Ok(())
    }

    fn save_bytes_data(&self, it: &mut Interp, obj: &Value, data: &[u8]) -> R<()> {
        let size = data.len();
        let mut h: Vec<u8> = Vec::with_capacity(9);
        if size <= 0xff {
            h.push(opc::SHORT_BINBYTES);
            h.push(size as u8);
        } else if size <= 0xffff_ffff {
            h.push(opc::BINBYTES);
            h.extend_from_slice(&(size as u32).to_le_bytes());
        } else if self.proto() >= 4 {
            h.push(opc::BINBYTES8);
            h.extend_from_slice(&(size as u64).to_le_bytes());
        } else {
            return Err(it.new_exc_str("OverflowError", "serializing a bytes object larger than 4 GiB requires pickle protocol 4 or higher"));
        }
        self.write_bytes(it, &h, data, Some(obj))?;
        self.memo_put(it, obj)
    }

    fn save_bytes(&self, it: &mut Interp, obj: &Value) -> R<()> {
        let Value::Obj(o) = obj else { return Ok(()) };
        let Kind::Bytes(data) = &o.kind else { return Ok(()) };
        if self.proto() < 3 {
            let bytes_ty = Value::Obj(it.types.bytes.clone());
            let rv = if data.is_empty() {
                vec![bytes_ty, Value::tuple(Vec::new())]
            } else {
                let text = Value::string(codecs::latin1_decode(data));
                vec![self.sh.codecs_encode.clone(), Value::tuple(vec![text, Value::str("latin1")])]
            };
            return self.save_reduce(it, &rv, Some(obj));
        }
        self.save_bytes_data(it, obj, data)
    }

    fn save_bytearray_data(&self, it: &mut Interp, obj: &Value, data: &[u8]) -> R<()> {
        let mut h = Vec::with_capacity(9);
        h.push(opc::BYTEARRAY8);
        h.extend_from_slice(&(data.len() as u64).to_le_bytes());
        self.write_bytes(it, &h, data, Some(obj))?;
        self.memo_put(it, obj)
    }

    fn save_bytearray(&self, it: &mut Interp, obj: &Value) -> R<()> {
        let Value::Obj(o) = obj else { return Ok(()) };
        let Kind::ByteArray(store) = &o.kind else { return Ok(()) };
        let data = store.to_vec();
        if self.proto() < 5 {
            let ba = Value::Obj(it.types.bytearray.clone());
            let rv = if data.is_empty() {
                vec![ba, Value::tuple(Vec::new())]
            } else {
                vec![ba, Value::tuple(vec![Value::bytes(data)])]
            };
            return self.save_reduce(it, &rv, Some(obj));
        }
        self.save_bytearray_data(it, obj, &data)
    }

    fn save_picklebuffer(&self, it: &mut Interp, obj: &Value) -> R<()> {
        if self.proto() < 5 {
            return Err(self.perr(it, "PickleBuffer can only be pickled with protocol >= 5"));
        }
        let view = super::pickle_buffer_data(it, obj)?;
        if !view.contiguous {
            return Err(self.perr(it, "PickleBuffer can not be pickled when pointing to a non-contiguous buffer"));
        }
        let mut in_band = true;
        let cb = self.p.buffer_callback.borrow().clone();
        if let Some(cb) = cb {
            let r = it.call(&cb, vec![obj.clone()], Vec::new())?;
            in_band = it.truthy(&r)?;
        }
        if in_band {
            if view.readonly {
                self.save_bytes_data(it, obj, &view.data)
            } else {
                self.save_bytearray_data(it, obj, &view.data)
            }
        } else {
            self.wb(opc::NEXT_BUFFER);
            if view.readonly {
                self.wb(opc::READONLY_BUFFER);
            }
            Ok(())
        }
    }

    fn save_unicode(&self, it: &mut Interp, obj: &Value) -> R<()> {
        let Some(s) = obj.as_str() else { return Ok(()) };
        if self.bin() {
            let encoded;
            let data: &[u8] = if obj.as_pystr().is_some_and(|p| p.ascii) {
                s.as_bytes()
            } else {
                encoded = it.encode_str(s, "utf-8", "surrogatepass")?;
                &encoded
            };
            let size = data.len();
            let mut h: Vec<u8> = Vec::with_capacity(9);
            if size <= 0xff && self.proto() >= 4 {
                h.push(opc::SHORT_BINUNICODE);
                h.push(size as u8);
            } else if size <= 0xffff_ffff {
                h.push(opc::BINUNICODE);
                h.extend_from_slice(&(size as u32).to_le_bytes());
            } else if self.proto() >= 4 {
                h.push(opc::BINUNICODE8);
                h.extend_from_slice(&(size as u64).to_le_bytes());
            } else {
                return Err(it.new_exc_str("OverflowError", "serializing a string larger than 4 GiB requires pickle protocol 4 or higher"));
            }
            self.write_bytes(it, &h, data, None)?;
        } else {
            let escaped = pk::raw_unicode_escape(lumen_common::smuggle::code_points(s));
            self.wb(opc::UNICODE);
            self.w(&escaped);
            self.wb(b'\n');
        }
        self.memo_put(it, obj)
    }

    fn store_tuple_elements(&self, it: &mut Interp, items: &[Value]) -> R<()> {
        for x in items {
            self.save(it, x, false)?;
        }
        Ok(())
    }

    fn save_tuple(&self, it: &mut Interp, obj: &Value) -> R<()> {
        let items: Vec<Value> = obj.tuple_items().map(|t| t.to_vec()).unwrap_or_default();
        let len = items.len();
        if len == 0 {
            if self.proto() != 0 {
                self.wb(opc::EMPTY_TUPLE);
            } else {
                self.w(&[opc::MARK, opc::TUPLE]);
            }
            return Ok(());
        }
        if len <= 3 && self.proto() >= 2 {
            self.store_tuple_elements(it, &items)?;
            if self.in_memo(it, obj) {
                for _ in 0..len {
                    self.wb(opc::POP);
                }
                return self.memo_get(it, obj);
            }
            self.wb([opc::EMPTY_TUPLE, opc::TUPLE1, opc::TUPLE2, opc::TUPLE3][len]);
            return self.memo_put(it, obj);
        }
        self.wb(opc::MARK);
        self.store_tuple_elements(it, &items)?;
        if self.in_memo(it, obj) {
            if self.bin() {
                self.wb(opc::POP_MARK);
            } else {
                for _ in 0..=len {
                    self.wb(opc::POP);
                }
            }
            return self.memo_get(it, obj);
        }
        self.wb(opc::TUPLE);
        self.memo_put(it, obj)
    }

    fn batch_list(&self, it: &mut Interp, iter: &Value) -> R<()> {
        if self.proto() == 0 {
            while let Some(x) = it.iter_next(iter)? {
                self.save(it, &x, false)?;
                self.wb(opc::APPEND);
            }
            return Ok(());
        }
        loop {
            let Some(first) = it.iter_next(iter)? else { break };
            let Some(second) = it.iter_next(iter)? else {
                self.save(it, &first, false)?;
                self.wb(opc::APPEND);
                break;
            };
            self.wb(opc::MARK);
            self.save(it, &first, false)?;
            let mut n = 1;
            let mut cur = Some(second);
            while let Some(x) = cur {
                self.save(it, &x, false)?;
                n += 1;
                if n == pk::BATCHSIZE {
                    break;
                }
                cur = it.iter_next(iter)?;
            }
            self.wb(opc::APPENDS);
            if n != pk::BATCHSIZE {
                break;
            }
        }
        Ok(())
    }

    fn batch_list_exact(&self, it: &mut Interp, list: &RefCell<Vec<Value>>) -> R<()> {
        let len = list.borrow().len();
        if len == 1 {
            let item = list.borrow()[0].clone();
            self.save(it, &item, false)?;
            self.wb(opc::APPEND);
            return Ok(());
        }
        let mut total = 0usize;
        loop {
            let mut this_batch = 0usize;
            self.wb(opc::MARK);
            loop {
                let item = list.borrow().get(total).cloned();
                let Some(item) = item else { break };
                self.save(it, &item, false)?;
                total += 1;
                this_batch += 1;
                if this_batch == pk::BATCHSIZE {
                    break;
                }
            }
            self.wb(opc::APPENDS);
            if total >= list.borrow().len() {
                break;
            }
        }
        Ok(())
    }

    fn save_list(&self, it: &mut Interp, obj: &Value) -> R<()> {
        let fast = self.p.fast.get() != 0;
        if fast {
            self.fast_enter(it, obj)?;
        }
        let r = self.save_list_body(it, obj);
        if fast {
            self.fast_leave(obj);
        }
        r
    }

    fn save_list_body(&self, it: &mut Interp, obj: &Value) -> R<()> {
        let Value::Obj(o) = obj else { return Ok(()) };
        let Kind::List(list) = &o.kind else { return Ok(()) };
        if self.bin() {
            self.wb(opc::EMPTY_LIST);
        } else {
            self.w(&[opc::MARK, opc::LIST]);
        }
        let len = list.borrow().len();
        self.memo_put(it, obj)?;
        if len != 0 {
            if self.proto() > 0 {
                self.enter(it)?;
                let r = self.batch_list_exact(it, list);
                self.leave();
                r?;
            } else {
                let iter = it.get_iter(obj)?;
                self.enter(it)?;
                let r = self.batch_list(it, &iter);
                self.leave();
                r?;
            }
        }
        Ok(())
    }

    fn next_pair(&self, it: &mut Interp, iter: &Value) -> R<Option<(Value, Value)>> {
        let Some(x) = it.iter_next(iter)? else { return Ok(None) };
        match x.tuple_items() {
            Some(t) if t.len() == 2 => Ok(Some((t[0].clone(), t[1].clone()))),
            _ => Err(it.type_error("dict items iterator must return 2-tuples")),
        }
    }

    fn batch_dict(&self, it: &mut Interp, iter: &Value) -> R<()> {
        if self.proto() == 0 {
            while let Some((k, v)) = self.next_pair(it, iter)? {
                self.save(it, &k, false)?;
                self.save(it, &v, false)?;
                self.wb(opc::SETITEM);
            }
            return Ok(());
        }
        loop {
            let Some((fk, fv)) = self.next_pair(it, iter)? else { break };
            let Some(second) = self.next_pair(it, iter)? else {
                self.save(it, &fk, false)?;
                self.save(it, &fv, false)?;
                self.wb(opc::SETITEM);
                break;
            };
            self.wb(opc::MARK);
            self.save(it, &fk, false)?;
            self.save(it, &fv, false)?;
            let mut n = 1;
            let mut cur = Some(second);
            while let Some((k, v)) = cur {
                self.save(it, &k, false)?;
                self.save(it, &v, false)?;
                n += 1;
                if n == pk::BATCHSIZE {
                    break;
                }
                cur = self.next_pair(it, iter)?;
            }
            self.wb(opc::SETITEMS);
            if n != pk::BATCHSIZE {
                break;
            }
        }
        Ok(())
    }

    fn batch_dict_exact(&self, it: &mut Interp, d: &RefCell<PyDict>) -> R<()> {
        let size = d.borrow().len();
        if size == 1 {
            let (k, v) = {
                let b = d.borrow();
                let i = b.next_live(0).expect("a dict of one entry has a live slot");
                let e = b.get(i).expect("live slot");
                (e.key.clone(), e.val.clone())
            };
            self.save(it, &k, false)?;
            self.save(it, &v, false)?;
            self.wb(opc::SETITEM);
            return Ok(());
        }
        let mut pos = 0usize;
        loop {
            let mut i = 0usize;
            self.wb(opc::MARK);
            loop {
                let next = {
                    let b = d.borrow();
                    b.next_live(pos).map(|idx| {
                        let e = b.get(idx).expect("live slot");
                        (idx, e.key.clone(), e.val.clone())
                    })
                };
                let Some((idx, k, v)) = next else { break };
                pos = idx + 1;
                self.save(it, &k, false)?;
                self.save(it, &v, false)?;
                i += 1;
                if i == pk::BATCHSIZE {
                    break;
                }
            }
            self.wb(opc::SETITEMS);
            if d.borrow().len() != size {
                return Err(it.runtime_error("dictionary changed size during iteration"));
            }
            if i != pk::BATCHSIZE {
                break;
            }
        }
        Ok(())
    }

    fn save_dict(&self, it: &mut Interp, obj: &Value) -> R<()> {
        let fast = self.p.fast.get() != 0;
        if fast {
            self.fast_enter(it, obj)?;
        }
        let r = self.save_dict_body(it, obj);
        if fast {
            self.fast_leave(obj);
        }
        r
    }

    fn save_dict_body(&self, it: &mut Interp, obj: &Value) -> R<()> {
        let Value::Obj(o) = obj else { return Ok(()) };
        let Kind::Dict(d) = &o.kind else { return Ok(()) };
        if self.bin() {
            self.wb(opc::EMPTY_DICT);
        } else {
            self.w(&[opc::MARK, opc::DICT]);
        }
        self.memo_put(it, obj)?;
        if d.borrow().len() != 0 {
            if self.proto() > 0 {
                self.enter(it)?;
                let r = self.batch_dict_exact(it, d);
                self.leave();
                r?;
            } else {
                let items = it.call_method(obj, "items", Vec::new())?;
                let iter = it.get_iter(&items)?;
                self.enter(it)?;
                let r = self.batch_dict(it, &iter);
                self.leave();
                r?;
            }
        }
        Ok(())
    }

    fn save_set(&self, it: &mut Interp, obj: &Value) -> R<()> {
        if self.proto() < 4 {
            let items = Value::list(it.iterate_to_vec(obj)?);
            let rv = vec![Value::Obj(it.types.set.clone()), Value::tuple(vec![items])];
            return self.save_reduce(it, &rv, Some(obj));
        }
        let Value::Obj(o) = obj else { return Ok(()) };
        let Kind::Set(d) = &o.kind else { return Ok(()) };
        self.wb(opc::EMPTY_SET);
        self.memo_put(it, obj)?;
        let size = d.borrow().len();
        if size == 0 {
            return Ok(());
        }
        let mut pos = 0usize;
        loop {
            let mut i = 0usize;
            self.wb(opc::MARK);
            loop {
                let next = {
                    let b = d.borrow();
                    b.next_live(pos).map(|idx| (idx, b.get(idx).expect("live slot").key.clone()))
                };
                let Some((idx, item)) = next else { break };
                pos = idx + 1;
                self.save(it, &item, false)?;
                i += 1;
                if i == pk::BATCHSIZE {
                    break;
                }
            }
            self.wb(opc::ADDITEMS);
            if d.borrow().len() != size {
                return Err(it.runtime_error("set changed size during iteration"));
            }
            if i != pk::BATCHSIZE {
                break;
            }
        }
        Ok(())
    }

    fn save_frozenset(&self, it: &mut Interp, obj: &Value) -> R<()> {
        if self.proto() < 4 {
            let items = Value::list(it.iterate_to_vec(obj)?);
            let rv = vec![Value::Obj(it.types.frozenset.clone()), Value::tuple(vec![items])];
            return self.save_reduce(it, &rv, Some(obj));
        }
        self.wb(opc::MARK);
        let items = it.iterate_to_vec(obj)?;
        for x in &items {
            self.save(it, x, false)?;
        }
        if self.in_memo(it, obj) {
            self.wb(opc::POP_MARK);
            return self.memo_get(it, obj);
        }
        self.wb(opc::FROZENSET);
        self.memo_put(it, obj)
    }

    fn fix_imports(&self, it: &mut Interp, module: &mut Value, name: &mut Value) -> R<()> {
        let key = Value::tuple(vec![module.clone(), name.clone()]);
        if let Some(item) = it.dict_get(&self.sh.name_3to2, &key)? {
            let pair = match item.tuple_items() {
                Some(t) if t.len() == 2 => t.to_vec(),
                _ => {
                    let t = it.tp_name_of(&item);
                    return Err(it.runtime_error(&format!("_compat_pickle.REVERSE_NAME_MAPPING values should be 2-tuples, not {}", t)));
                }
            };
            if pair[0].as_str().is_none() || pair[1].as_str().is_none() {
                let (a, b) = (it.tp_name_of(&pair[0]), it.tp_name_of(&pair[1]));
                return Err(it.runtime_error(&format!("_compat_pickle.REVERSE_NAME_MAPPING values should be pairs of str, not ({}, {})", a, b)));
            }
            *module = pair[0].clone();
            *name = pair[1].clone();
            return Ok(());
        }
        if let Some(item) = it.dict_get(&self.sh.import_3to2, module)? {
            if item.as_str().is_none() {
                let t = it.tp_name_of(&item);
                return Err(it.runtime_error(&format!("_compat_pickle.REVERSE_IMPORT_MAPPING values should be strings, not {}", t)));
            }
            *module = item;
        }
        Ok(())
    }

    fn encode_ident(&self, it: &mut Interp, v: &Value, what: &str) -> R<Vec<u8>> {
        let s = v.as_str().unwrap_or("");
        let enc = if self.proto() == 3 { "utf-8" } else { "ascii" };
        match it.encode_str(s, enc, "strict") {
            Ok(b) => Ok(b),
            Err(e) if it.exc_is(&e, "UnicodeEncodeError") => {
                Err(self.perr(it, &format!("can't pickle {} identifier '{}' using pickle protocol {}", what, s, self.proto())))
            }
            Err(e) => Err(e),
        }
    }

    fn whichmodule(&self, it: &mut Interp, obj: &Value, dotted: &[String]) -> R<Value> {
        if let Some(m) = attr_opt(it, obj, "__module__")? {
            if !m.is_none() {
                return Ok(m);
            }
        }
        let entries: Vec<(Value, Value)> = match crate::containers::pydict_of(&it.modules) {
            Some(d) => d.borrow().iter().map(|e| (e.key.clone(), e.val.clone())).collect(),
            None => Vec::new(),
        };
        for (name, module) in entries {
            if module.is_none() || name.as_str() == Some("__main__") {
                continue;
            }
            if let Some((cand, _)) = deep_attribute(it, &module, dotted)? {
                if cand.is(obj) {
                    return Ok(name);
                }
            }
        }
        Ok(Value::str("__main__"))
    }

    fn save_global(&self, it: &mut Interp, obj: &Value, name: Option<&Value>) -> R<()> {
        let mut global_name = match name {
            Some(n) => n.clone(),
            None => match attr_opt(it, obj, "__qualname__")? {
                Some(q) => q,
                None => it.get_attr_str(obj, "__name__")?,
            },
        };
        let dotted = dotted_path(it, &global_name, None)?;
        let module_name = self.whichmodule(it, obj, &dotted)?;
        let imported = match module_name.as_str() {
            Some(s) => it.import_module(s).ok(),
            None => None,
        };
        let Some(module) = imported.map(Value::Obj) else {
            let o = it.repr_of(obj)?;
            let m = it.repr_of(&module_name)?;
            return Err(self.perr(it, &format!("Can't pickle {}: import of module {} failed", o, m)));
        };
        let Some((cls, parent)) = deep_attribute(it, &module, &dotted)? else {
            let o = it.repr_of(obj)?;
            let g = it.str_of(&global_name)?;
            let m = it.str_of(&module_name)?;
            return Err(self.perr(it, &format!("Can't pickle {}: attribute lookup {} on {} failed", o, g, m)));
        };
        if !cls.is(obj) {
            let o = it.repr_of(obj)?;
            let g = it.str_of(&global_name)?;
            let m = it.str_of(&module_name)?;
            return Err(self.perr(it, &format!("Can't pickle {}: it's not the same object as {}.{}", o, m, g)));
        }

        if self.proto() >= 2 {
            let key = Value::tuple(vec![module_name.clone(), global_name.clone()]);
            if let Some(code_obj) = it.dict_get(&self.sh.ext_registry, &key)? {
                let code = it.index_of(&code_obj)?;
                if code <= 0 || code > 0x7fff_ffff {
                    return Err(it.runtime_error(&format!("extension code {} is out of range", code)));
                }
                if code <= 0xff {
                    self.w(&[opc::EXT1, code as u8]);
                } else if code <= 0xffff {
                    self.wb(opc::EXT2);
                    self.w(&(code as u16).to_le_bytes());
                } else {
                    self.wb(opc::EXT4);
                    self.w(&(code as u32).to_le_bytes());
                }
                return Ok(());
            }
        }

        let mut dotted = Some(dotted);
        if parent.is(&module) {
            if let Some(last) = dotted.as_ref().and_then(|d| d.last()) {
                global_name = Value::str(last);
            }
            dotted = None;
        }
        if self.proto() >= 4 {
            self.save(it, &module_name, false)?;
            self.save(it, &global_name, false)?;
            self.wb(opc::STACK_GLOBAL);
        } else {
            let mut gname = global_name.clone();
            if let Some(d) = &dotted {
                if d.len() > 1 {
                    gname = Value::str(&d[0]);
                }
                for _ in 1..d.len() {
                    let getattr = self.sh.getattr.clone();
                    self.save(it, &getattr, false)?;
                    if self.proto() < 2 {
                        self.wb(opc::MARK);
                    }
                }
            }
            self.wb(opc::GLOBAL);
            let mut mname = module_name.clone();
            if self.proto() < 3 && self.p.fix_imports.get() {
                self.fix_imports(it, &mut mname, &mut gname)?;
            }
            let enc = self.encode_ident(it, &mname, "module")?;
            self.w(&enc);
            self.wb(b'\n');
            let enc = self.encode_ident(it, &gname, "global")?;
            self.w(&enc);
            self.wb(b'\n');
            if let Some(d) = &dotted {
                for part in &d[1.min(d.len())..] {
                    self.save(it, &Value::str(part), false)?;
                    self.wb(if self.proto() < 2 { opc::TUPLE } else { opc::TUPLE2 });
                    self.wb(opc::REDUCE);
                }
            }
        }
        self.memo_put(it, obj)
    }

    fn save_type(&self, it: &mut Interp, obj: &Value) -> R<()> {
        let single = match obj {
            Value::Obj(o) if std::rc::Rc::ptr_eq(o, &it.types.none_type) => Some(Value::None),
            Value::Obj(o) if std::rc::Rc::ptr_eq(o, &it.types.ellipsis_type) => Some(Value::Ellipsis),
            Value::Obj(o) if std::rc::Rc::ptr_eq(o, &it.types.notimpl_type) => Some(Value::NotImplemented),
            _ => None,
        };
        match single {
            Some(s) => {
                let rv = vec![Value::Obj(it.types.type_.clone()), Value::tuple(vec![s])];
                self.save_reduce(it, &rv, Some(obj))
            }
            None => self.save_global(it, obj, None),
        }
    }

    pub fn save_reduce(&self, it: &mut Interp, args: &[Value], obj: Option<&Value>) -> R<()> {
        let size = args.len();
        if !(2..=6).contains(&size) {
            return Err(self.perr(it, "tuple returned by __reduce__ must contain 2 through 6 elements"));
        }
        let mut callable = args[0].clone();
        let argtup = &args[1];
        let state = opt(args, 2);
        let listitems = opt(args, 3);
        let dictitems = opt(args, 4);
        let state_setter = opt(args, 5);

        if !it.is_callable(&callable) {
            return Err(self.perr(it, "first item of the tuple returned by __reduce__ must be callable"));
        }
        let Some(argitems) = argtup.tuple_items() else {
            return Err(self.perr(it, "second item of the tuple returned by __reduce__ must be a tuple"));
        };
        let argitems = argitems.to_vec();
        if let Some(l) = listitems {
            if !is_iter(it, l) {
                let t = it.tp_name_of(l);
                return Err(self.perr(it, &format!("fourth element of the tuple returned by __reduce__ must be an iterator, not {}", t)));
            }
        }
        if let Some(d) = dictitems {
            if !is_iter(it, d) {
                let t = it.tp_name_of(d);
                return Err(self.perr(it, &format!("fifth element of the tuple returned by __reduce__ must be an iterator, not {}", t)));
            }
        }
        if let Some(s) = state_setter {
            if !it.is_callable(s) {
                let t = it.tp_name_of(s);
                return Err(self.perr(it, &format!("sixth element of the tuple returned by __reduce__ must be a function, not {}", t)));
            }
        }

        let (mut use_newobj_ex, mut use_newobj) = (false, false);
        if self.proto() >= 2 {
            if let Some(n) = attr_opt(it, &callable, "__name__")? {
                if let Some(s) = n.as_str() {
                    use_newobj_ex = s == "__newobj_ex__";
                    if !use_newobj_ex {
                        use_newobj = s == "__newobj__";
                    }
                }
            }
        }

        if use_newobj_ex {
            if argitems.len() != 3 {
                return Err(self.perr(it, &format!("length of the NEWOBJ_EX argument tuple must be exactly 3, not {}", argitems.len())));
            }
            let cls = &argitems[0];
            if !cls.is_type() {
                let t = it.tp_name_of(cls);
                return Err(self.perr(it, &format!("first item from NEWOBJ_EX argument tuple must be a class, not {}", t)));
            }
            let cargs = &argitems[1];
            let Some(cargs_items) = cargs.tuple_items() else {
                let t = it.tp_name_of(cargs);
                return Err(self.perr(it, &format!("second item from NEWOBJ_EX argument tuple must be a tuple, not {}", t)));
            };
            let cargs_items = cargs_items.to_vec();
            let kwargs = &argitems[2];
            if dict_of(kwargs).is_none() {
                let t = it.tp_name_of(kwargs);
                return Err(self.perr(it, &format!("third item from NEWOBJ_EX argument tuple must be a dict, not {}", t)));
            }
            if self.proto() >= 4 {
                self.save(it, cls, false)?;
                self.save(it, cargs, false)?;
                self.save(it, kwargs, false)?;
                self.wb(opc::NEWOBJ_EX);
            } else {
                let cls_new = it.get_attr_str(cls, "__new__")?;
                let mut newargs = Vec::with_capacity(cargs_items.len() + 2);
                newargs.push(cls_new);
                newargs.push(cls.clone());
                newargs.extend(cargs_items);
                let kw = dict_to_kw(it, kwargs)?;
                let partial = self.sh.partial.clone();
                callable = it.call(&partial, newargs, kw)?;
                let empty = Value::tuple(Vec::new());
                self.save(it, &callable, false)?;
                self.save(it, &empty, false)?;
                self.wb(opc::REDUCE);
            }
        } else if use_newobj {
            if argitems.is_empty() {
                return Err(self.perr(it, "__newobj__ arglist is empty"));
            }
            let cls = &argitems[0];
            if !cls.is_type() {
                return Err(self.perr(it, "args[0] from __newobj__ args is not a type"));
            }
            if let Some(o) = obj {
                let obj_class = match attr_opt(it, o, "__class__")? {
                    Some(c) => c,
                    None => Value::Obj(it.type_of(o)),
                };
                if !obj_class.is(cls) {
                    return Err(self.perr(it, "args[0] from __newobj__ args has the wrong class"));
                }
            }
            self.save(it, cls, false)?;
            let newargs = Value::tuple(argitems[1..].to_vec());
            self.save(it, &newargs, false)?;
            self.wb(opc::NEWOBJ);
        } else {
            self.save(it, &callable, false)?;
            self.save(it, argtup, false)?;
            self.wb(opc::REDUCE);
        }

        if let Some(o) = obj {
            if self.in_memo(it, o) {
                self.wb(opc::POP);
                return self.memo_get(it, o);
            }
            self.memo_put(it, o)?;
        }

        if let Some(l) = listitems {
            self.batch_list(it, l)?;
        }
        if let Some(d) = dictitems {
            self.batch_dict(it, d)?;
        }
        if let Some(state) = state {
            match state_setter {
                None => {
                    self.save(it, state, false)?;
                    self.wb(opc::BUILD);
                }
                Some(setter) => {
                    let target = obj.cloned().unwrap_or(Value::None);
                    self.save(it, setter, false)?;
                    self.save(it, &target, false)?;
                    self.save(it, state, false)?;
                    self.wb(opc::TUPLE2);
                    self.wb(opc::REDUCE);
                    self.wb(opc::POP);
                }
            }
        }
        Ok(())
    }
}
