//! Hashing and the dict/set primitives that may run user `__hash__` / `__eq__`.

use crate::dict::{Lookup, PyDict};
use crate::num::{hash_float, hash_int};
use crate::object::*;
use crate::pyint::PyInt;
use crate::vm::*;
use std::cell::RefCell;
use std::rc::Rc;

pub fn pydict_of(o: &Obj) -> Option<&RefCell<PyDict>> {
    match &o.kind {
        Kind::Dict(d) | Kind::Set(d) | Kind::FrozenSet(d) => Some(d),
        _ => None,
    }
}

const XXP1: u64 = 11400714785074694791;
const XXP2: u64 = 14029467366897019727;
const XXP5: u64 = 2870177450012600261;

impl Interp {
    /// A special method defined by a user (heap) class, if any.
    pub fn user_special(&self, v: &Value, name: &str) -> Option<Value> {
        if let Value::Obj(o) = v {
            if let Some(cls) = &o.cls {
                if let Some((owner, m)) = self.lookup_mro_with_owner(cls, name) {
                    if self.is_heap(&owner) || self.dispatches_natively(&owner) {
                        return Some(m);
                    }
                }
            }
        }
        None
    }

    fn dispatches_natively(&self, t: &Obj) -> bool {
        matches!(&t.kind, Kind::Type(td) if td.flags.get() & TF_DISPATCH != 0)
    }

    pub fn call_user_special(&mut self, v: &Value, m: &Value, args: Vec<Value>) -> R<Value> {
        // A plain method binds to `v`: call it with `v` prepended, without a bound-method object.
        if let Value::Obj(f) = m {
            if matches!(&f.kind, Kind::Function(_)) || matches!(&f.kind, Kind::Native(nd) if nd.method) {
                let mut a = Vec::with_capacity(args.len() + 1);
                a.push(v.clone());
                a.extend(args);
                return self.call(m, a, Vec::new());
            }
        }
        let cls = self.type_of(v);
        let b = self.bind_descr(m, v, &cls)?;
        self.call(&b, args, Vec::new())
    }

    pub fn hash_value(&mut self, v: &Value) -> R<i64> {
        if let Value::Obj(_) = v {
            if let Some(m) = self.user_special(v, "__hash__") {
                if m.is_none() {
                    let t = self.tp_name_of(v);
                    return Err(self.type_error(&format!("unhashable type: '{}'", t)));
                }
                // A native `tp_hash` slot returns the hash itself, not an int to hash again.
                let slot = matches!(&m, Value::Obj(f) if matches!(&f.kind, Kind::Native(n) if n.desc.is_some_and(|d| d.role == lumen_bind::Role::Proto("hash"))));
                let r = self.call_user_special(v, &m, Vec::new())?;
                return match r {
                    Value::Int(i) if slot => Ok(if i == -1 { -2 } else { i }),
                    Value::Int(i) => Ok(hash_int(i)),
                    Value::Bool(b) => Ok(b as i64),
                    Value::Obj(ro) if matches!(ro.kind, Kind::Int(_)) => self.native_hash(&Value::Obj(ro)),
                    _ => Err(self.type_error("__hash__ method should return an integer")),
                };
            }
        }
        self.native_hash(v)
    }

    pub fn native_hash(&mut self, v: &Value) -> R<i64> {
        Ok(match v {
            Value::Int(i) => hash_int(*i),
            Value::Bool(b) => *b as i64,
            Value::Float(f) => hash_float(*f),
            Value::None => 0x5f3a_b1c2,
            Value::NotImplemented => 0x4e49,
            Value::Ellipsis => 0x454c,
            Value::Obj(o) => {
                match &o.kind {
                    Kind::Str(s) => s.hash(),
                    Kind::Int(b) => b.py_hash(),
                    Kind::Float(f) => hash_float(*f),
                    Kind::Tuple(items) => {
                        let items = items.clone();
                        let mut acc: u64 = XXP5;
                        for it in &items {
                            let lane = self.hash_value(it)? as u64;
                            acc = acc.wrapping_add(lane.wrapping_mul(XXP2));
                            acc = acc.rotate_left(31);
                            acc = acc.wrapping_mul(XXP1);
                        }
                        acc = acc.wrapping_add((items.len() as u64) ^ (XXP5 ^ 3527539));
                        if acc as i64 == -1 {
                            1546275796
                        } else {
                            acc as i64
                        }
                    }
                    Kind::Bytes(b) => hash_bytes(b),
                    Kind::FrozenSet(d) => {
                        let hashes: Vec<i64> = d.borrow().iter().map(|e| e.hash).collect();
                        let mut h: u64 = 0;
                        for x in hashes {
                            let x = x as u64;
                            h ^= ((x ^ 89869747) ^ (x << 16)).wrapping_mul(3644798167);
                        }
                        h ^= ((d.borrow().len() as u64) + 1).wrapping_mul(1927868237);
                        h ^= (h >> 11) ^ (h >> 25);
                        h = h.wrapping_mul(69069).wrapping_add(907133923);
                        if h as i64 == -1 {
                            590923713
                        } else {
                            h as i64
                        }
                    }
                    Kind::List(_) | Kind::Dict(_) | Kind::Set(_) | Kind::ByteArray(_) => {
                        let t = self.type_name_of(v);
                        return Err(self.type_error(&format!("unhashable type: '{}'", t)));
                    }
                    Kind::Complex(re, im) => match hash_float(*re).wrapping_add(hash_float(*im).wrapping_mul(1000003)) {
                        -1 => -2,
                        h => h,
                    },
                    Kind::Slice(..) => {
                        let t = self.type_name_of(v);
                        return Err(self.type_error(&format!("unhashable type: '{}'", t)));
                    }
                    Kind::BigRange(r) => r[0].py_hash() ^ r[1].py_hash().rotate_left(7) ^ r[2].py_hash().rotate_left(13),
                    Kind::Range(r) => hash_int(r.start) ^ hash_int(r.stop).rotate_left(7) ^ hash_int(r.step).rotate_left(13),
                    _ => ((Rc::as_ptr(o) as usize) >> 4) as i64,
                }
            }
        })
    }

    pub fn values_eq(&mut self, a: &Value, b: &Value) -> R<bool> {
        if let Some(r) = crate::dict::fast_eq(a, b) {
            return Ok(r);
        }
        if a.is(b) {
            return Ok(true);
        }
        let r = self.compare_op(crate::ast::CmpOp::Eq, a, b)?;
        self.truthy(&r)
    }

    pub fn dict_find(&mut self, d: &Obj, hash: i64, key: &Value) -> R<Option<usize>> {
        let pd = match pydict_of(d) {
            Some(p) => p,
            None => return Ok(None),
        };
        let r = pd.borrow().lookup(hash, key);
        match r {
            Lookup::Found(i) => Ok(Some(i)),
            Lookup::Missing => Ok(None),
            Lookup::Slow(cands) => {
                for (i, k) in cands {
                    if k.is(key) || self.values_eq(&k, key)? {
                        let still = pd.borrow().get(i).map(|e| e.hash == hash).unwrap_or(false);
                        if still {
                            return Ok(Some(i));
                        }
                    }
                }
                Ok(None)
            }
        }
    }

    pub fn hash_key(&mut self, v: &Value, what: &str) -> R<i64> {
        match self.hash_value(v) {
            Ok(h) => Ok(h),
            Err(e) => {
                let msg = match &e.kind {
                    Kind::Exception(d) if self.exc_is(&e, "TypeError") => d.borrow().args.tuple_items().and_then(|t| t.first()).and_then(|m| m.as_str()).map(str::to_string),
                    _ => None,
                };
                match msg {
                    Some(m) if m.starts_with("unhashable type:") => {
                        let t = self.type_name_of(v);
                        Err(self.type_error(&format!("cannot use '{}' as a {} ({})", t, what, m)))
                    }
                    _ => Err(e),
                }
            }
        }
    }

    pub fn dict_get(&mut self, d: &Obj, key: &Value) -> R<Option<Value>> {
        let what = if matches!(d.kind, Kind::Dict(_)) { "dict key" } else { "set element" };
        let h = self.hash_key(key, what)?;
        match self.dict_find(d, h, key)? {
            Some(i) => Ok(pydict_of(d).and_then(|p| p.borrow().get(i).map(|e| e.val.clone()))),
            None => Ok(None),
        }
    }

    pub fn dict_set(&mut self, d: &Obj, key: Value, val: Value) -> R<()> {
        let h = self.hash_key(&key, "dict key")?;
        match self.dict_find(d, h, &key)? {
            Some(i) => {
                if let Some(p) = pydict_of(d) {
                    p.borrow_mut().set_val(i, val);
                }
            }
            None => {
                if let Some(p) = pydict_of(d) {
                    p.borrow_mut().insert_new(h, key, val);
                }
            }
        }
        Ok(())
    }

    pub fn dict_remove(&mut self, d: &Obj, key: &Value) -> R<Option<Value>> {
        let what = if matches!(d.kind, Kind::Dict(_)) { "dict key" } else { "set element" };
        let h = self.hash_key(key, what)?;
        match self.dict_find(d, h, key)? {
            Some(i) => Ok(pydict_of(d).and_then(|p| p.borrow_mut().remove(i)).map(|e| e.val)),
            None => Ok(None),
        }
    }

    pub fn set_contains(&mut self, s: &Obj, key: &Value) -> R<bool> {
        let h = match self.hash_key(key, "set element") {
            Ok(h) => h,
            Err(e) => {
                if let Some(items) = set_items_for_hash_fallback(key) {
                    let fs = self.new_frozenset_from(items)?;
                    self.hash_value(&fs)?
                } else {
                    return Err(e);
                }
            }
        };
        Ok(self.dict_find(s, h, key)?.is_some())
    }

    pub fn new_set(&mut self, items: Vec<Value>) -> R<Value> {
        let s = Object::new(Kind::Set(RefCell::new(PyDict::new_set())));
        for i in items {
            self.set_add_obj(&s, i)?;
        }
        Ok(Value::Obj(s))
    }

    pub fn new_frozenset_from(&mut self, items: Vec<Value>) -> R<Value> {
        let s = Object::new(Kind::FrozenSet(RefCell::new(PyDict::new_set())));
        for i in items {
            self.set_add_obj(&s, i)?;
        }
        Ok(Value::Obj(s))
    }

    pub fn set_add_obj(&mut self, s: &Obj, key: Value) -> R<()> {
        let key = self.freeze_set_key(key);
        let h = self.hash_key(&key, "set element")?;
        if self.dict_find(s, h, &key)?.is_none() {
            if let Some(p) = pydict_of(s) {
                p.borrow_mut().insert_new(h, key, Value::None);
            }
        }
        Ok(())
    }

    fn freeze_set_key(&mut self, key: Value) -> Value {
        key
    }

    pub fn set_add(&mut self, s: &Value, key: Value) -> R<()> {
        match s {
            Value::Obj(o) => self.set_add_obj(o, key),
            _ => Ok(()),
        }
    }

    pub fn dict_update_from(&mut self, d: &Obj, src: &Value) -> R<()> {
        if let Value::Obj(so) = src {
            if let Kind::Dict(sd) = &so.kind {
                if so.cls.is_none() || self.user_special(src, "keys").is_none() {
                    let entries: Vec<(Value, Value)> = sd.borrow().iter().map(|e| (e.key.clone(), e.val.clone())).collect();
                    for (k, v) in entries {
                        self.dict_set(d, k, v)?;
                    }
                    return Ok(());
                }
            }
        }
        let keys_attr = self.get_attr_str(src, "keys");
        match keys_attr {
            Ok(kf) => {
                let ks = self.call(&kf, Vec::new(), Vec::new())?;
                let keys = self.iterate_to_vec(&ks)?;
                for k in keys {
                    let v = self.getitem(src, &k)?;
                    self.dict_set(d, k, v)?;
                }
            }
            Err(e) => {
                if !self.exc_is(&e, "AttributeError") {
                    return Err(e);
                }
                let items = self.iterate_to_vec(src)?;
                for (n, it) in items.into_iter().enumerate() {
                    let pair = self.iterate_to_vec(&it).map_err(|_| {
                        self.new_exc_str("TypeError", &format!("cannot convert dictionary update sequence element #{} to a sequence", n))
                    })?;
                    if pair.len() != 2 {
                        return Err(self.value_error(&format!(
                            "dictionary update sequence element #{} has length {}; 2 is required",
                            n,
                            pair.len()
                        )));
                    }
                    self.dict_set(d, pair[0].clone(), pair[1].clone())?;
                }
            }
        }
        Ok(())
    }
}

fn set_items_for_hash_fallback(key: &Value) -> Option<Vec<Value>> {
    match key {
        Value::Obj(o) => match &o.kind {
            Kind::Set(d) => Some(d.borrow().keys()),
            _ => None,
        },
        _ => None,
    }
}
