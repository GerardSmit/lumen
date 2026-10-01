//! Operator protocols: truthiness, comparison, arithmetic, subscripting, containment.

use crate::ast::{BinOp, CmpOp};
use crate::fmath;
use crate::pyint::{BigInt, PyInt};
use crate::bytecode::UnOp;
use crate::containers::pydict_of;
use crate::dict::PyDict;
use crate::num::*;
use crate::object::*;
use crate::vm::*;
use lumen_common::limits::size;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::rc::Rc;

pub fn binop_info(op: BinOp) -> (&'static str, &'static str, &'static str, &'static str) {
    match op {
        BinOp::Add => ("__add__", "__radd__", "__iadd__", "+"),
        BinOp::Sub => ("__sub__", "__rsub__", "__isub__", "-"),
        BinOp::Mult => ("__mul__", "__rmul__", "__imul__", "*"),
        BinOp::MatMult => ("__matmul__", "__rmatmul__", "__imatmul__", "@"),
        BinOp::Div => ("__truediv__", "__rtruediv__", "__itruediv__", "/"),
        BinOp::Mod => ("__mod__", "__rmod__", "__imod__", "%"),
        BinOp::Pow => ("__pow__", "__rpow__", "__ipow__", "** or pow()"),
        BinOp::LShift => ("__lshift__", "__rlshift__", "__ilshift__", "<<"),
        BinOp::RShift => ("__rshift__", "__rrshift__", "__irshift__", ">>"),
        BinOp::BitOr => ("__or__", "__ror__", "__ior__", "|"),
        BinOp::BitXor => ("__xor__", "__rxor__", "__ixor__", "^"),
        BinOp::BitAnd => ("__and__", "__rand__", "__iand__", "&"),
        BinOp::FloorDiv => ("__floordiv__", "__rfloordiv__", "__ifloordiv__", "//"),
    }
}

pub fn cmp_info(op: CmpOp) -> (&'static str, &'static str, &'static str) {
    match op {
        CmpOp::Eq => ("__eq__", "__eq__", "=="),
        CmpOp::NotEq => ("__ne__", "__ne__", "!="),
        CmpOp::Lt => ("__lt__", "__gt__", "<"),
        CmpOp::LtE => ("__le__", "__ge__", "<="),
        CmpOp::Gt => ("__gt__", "__lt__", ">"),
        CmpOp::GtE => ("__ge__", "__le__", ">="),
        _ => ("", "", ""),
    }
}

fn ord_matches(op: CmpOp, o: Ordering) -> bool {
    match op {
        CmpOp::Eq => o == Ordering::Equal,
        CmpOp::NotEq => o != Ordering::Equal,
        CmpOp::Lt => o == Ordering::Less,
        CmpOp::LtE => o != Ordering::Greater,
        CmpOp::Gt => o == Ordering::Greater,
        CmpOp::GtE => o != Ordering::Less,
        _ => false,
    }
}

pub fn norm_index(i: i64, len: usize) -> Option<usize> {
    let l = len as i64;
    let j = if i < 0 { i + l } else { i };
    if j < 0 || j >= l {
        None
    } else {
        Some(j as usize)
    }
}

pub fn slice_len(start: i64, stop: i64, step: i64) -> usize {
    if step > 0 {
        if stop > start {
            ((stop - start + step - 1) / step) as usize
        } else {
            0
        }
    } else if start > stop {
        ((start - stop - step - 1) / (-step)) as usize
    } else {
        0
    }
}

impl Interp {
    pub fn truthy(&mut self, v: &Value) -> R<bool> {
        Ok(match v {
            Value::Bool(b) => *b,
            Value::None => false,
            Value::Int(i) => *i != 0,
            Value::Float(f) => *f != 0.0,
            Value::NotImplemented | Value::Ellipsis => true,
            Value::Obj(o) => {
                if o.cls.is_some() {
                    if let Some(m) = self.user_special(v, "__bool__") {
                        let r = self.call_user_special(v, &m, Vec::new())?;
                        return match r {
                            Value::Bool(b) => Ok(b),
                            _ => Err(self.type_error(&format!("__bool__ should return bool, returned {}", self.type_name_of(&r)))),
                        };
                    }
                    if let Some(m) = self.user_special(v, "__len__") {
                        let r = self.call_user_special(v, &m, Vec::new())?;
                        let n = self.index_of(&r)?;
                        if n < 0 {
                            return Err(self.value_error("__len__() should return >= 0"));
                        }
                        return Ok(n != 0);
                    }
                }
                match &o.kind {
                    Kind::Str(s) => s.nchars > 0,
                    Kind::Int(b) => !b.is_zero(),
                    Kind::Float(f) => *f != 0.0,
                    Kind::Tuple(t) => !t.is_empty(),
                    Kind::List(l) => !l.borrow().is_empty(),
                    Kind::Dict(d) | Kind::Set(d) | Kind::FrozenSet(d) => !d.borrow().is_empty(),
                    Kind::Bytes(b) => !b.is_empty(),
                    Kind::ByteArray(b) => !b.is_empty(),
                    Kind::Range(r) => slice_len(r.start, r.stop, r.step) > 0,
                    Kind::BigRange(r) => !big_range_len(r).is_zero(),
                    Kind::Complex(a, b) => *a != 0.0 || *b != 0.0,
                    Kind::DictView(d, _) => pydict_of(d).map(|p| !p.borrow().is_empty()).unwrap_or(true),
                    _ => true,
                }
            }
        })
    }

    pub fn len_of(&mut self, v: &Value) -> R<usize> {
        if let Value::Obj(o) = v {
            if o.cls.is_some() {
                if let Some(m) = self.user_special(v, "__len__") {
                    let r = self.call_user_special(v, &m, Vec::new())?;
                    let n = self.index_of(&r)?;
                    if n < 0 {
                        return Err(self.value_error("__len__() should return >= 0"));
                    }
                    return Ok(n as usize);
                }
            }
        }
        self.native_len(v)
    }

    pub fn native_len(&mut self, v: &Value) -> R<usize> {
        if let Value::Obj(o) = v {
            match &o.kind {
                Kind::Str(s) => return Ok(s.nchars),
                Kind::Tuple(t) => return Ok(t.len()),
                Kind::List(l) => return Ok(l.borrow().len()),
                Kind::Dict(d) | Kind::Set(d) | Kind::FrozenSet(d) => return Ok(d.borrow().len()),
                Kind::Bytes(b) => return Ok(b.len()),
                Kind::ByteArray(b) => return Ok(b.len()),
                Kind::Range(r) => return Ok(slice_len(r.start, r.stop, r.step)),
                Kind::BigRange(r) => {
                    return match big_range_len(r).to_i64() {
                        Some(n) => Ok(n as usize),
                        None => Err(self.overflow_err("Python int too large to convert to C ssize_t")),
                    }
                }
                Kind::DictView(d, _) => return Ok(pydict_of(d).map(|p| p.borrow().len()).unwrap_or(0)),
                _ => {}
            }
            let cls = self.type_of_obj(o);
            if let Some(m) = self.lookup_mro(&cls, "__len__") {
                let b = self.bind_descr(&m, v, &cls)?;
                let r = self.call(&b, Vec::new(), Vec::new())?;
                let n = self.index_of(&r)?;
                if n < 0 {
                    return Err(self.value_error("__len__() should return >= 0"));
                }
                return Ok(n as usize);
            }
        }
        let t = self.type_name_of(v);
        Err(self.type_error(&format!("object of type '{}' has no len()", t)))
    }

    pub fn index_of(&mut self, v: &Value) -> R<i64> {
        match v {
            Value::Int(i) => Ok(*i),
            Value::Bool(b) => Ok(*b as i64),
            Value::Obj(o) => {
                if let Kind::Int(b) = &o.kind {
                    return match b.to_i64() {
                        Some(i) => Ok(i),
                        None => Err(self.overflow_err("Python int too large to convert to C ssize_t")),
                    };
                }
                if let Some(m) = self.user_special(v, "__index__") {
                    let r = self.call_user_special(v, &m, Vec::new())?;
                    return match r {
                        Value::Int(i) => Ok(i),
                        Value::Bool(b) => Ok(b as i64),
                        _ => Err(self.type_error("__index__ returned non-int")),
                    };
                }
                let t = self.type_name_of(v);
                Err(self.type_error(&format!("'{}' object cannot be interpreted as an integer", t)))
            }
            _ => {
                let t = self.type_name_of(v);
                Err(self.type_error(&format!("'{}' object cannot be interpreted as an integer", t)))
            }
        }
    }

    /// A sequence subscript (`PyNumber_AsSsize_t(key, PyExc_IndexError)`): an int past
    /// `Py_ssize_t` is `IndexError: cannot fit 'int' into an index-sized integer`.
    #[inline]
    pub fn seq_index(&mut self, v: &Value) -> R<i64> {
        if let Value::Int(i) = v {
            return Ok(*i);
        }
        self.index_or(v, "IndexError")
    }

    /// `PyNumber_AsSsize_t(v, exc)`: `__index__`, then `exc` ("cannot fit '<type>' into an
    /// index-sized integer") when the int does not fit.
    #[cold]
    pub fn index_or(&mut self, v: &Value, exc: &str) -> R<i64> {
        match v.as_bigint() {
            Some(b) => match b.to_i64() {
                Some(i) => Ok(i),
                None => {
                    let t = self.type_name_of(v);
                    Err(self.new_exc_str(exc, &format!("cannot fit '{}' into an index-sized integer", t)))
                }
            },
            None => match self.index_of(v) {
                Err(e) if self.exc_is(&e, "OverflowError") => {
                    let t = self.type_name_of(v);
                    Err(self.new_exc_str(exc, &format!("cannot fit '{}' into an index-sized integer", t)))
                }
                r => r,
            },
        }
    }

    /// An optional `start`/`stop` argument (`_PyEval_SliceIndexNotNone`): ints out of range clamp.
    pub fn slice_index(&mut self, v: &Value) -> R<i64> {
        if !self.has_index(v) {
            return Err(self.type_error("slice indices must be integers or have an __index__ method"));
        }
        if let Some(b) = v.as_bigint() {
            if b.to_i64().is_none() {
                return Ok(if b.is_negative() { i64::MIN } else { i64::MAX });
            }
        }
        self.index_of(v)
    }

    pub fn has_index(&self, v: &Value) -> bool {
        match v {
            Value::Int(_) | Value::Bool(_) => true,
            Value::Obj(o) => matches!(o.kind, Kind::Int(_)) || self.user_special(v, "__index__").is_some(),
            _ => false,
        }
    }

    // ---- comparison ------------------------------------------------------------------------

    pub fn compare_op(&mut self, op: CmpOp, a: &Value, b: &Value) -> R<Value> {
        match op {
            CmpOp::Is => return Ok(Value::Bool(a.is(b))),
            CmpOp::IsNot => return Ok(Value::Bool(!a.is(b))),
            CmpOp::In => return Ok(Value::Bool(self.contains(b, a)?)),
            CmpOp::NotIn => return Ok(Value::Bool(!self.contains(b, a)?)),
            _ => {}
        }
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => return Ok(Value::Bool(ord_matches(op, x.cmp(y)))),
            (Value::Float(x), Value::Float(y)) => {
                return Ok(Value::Bool(match x.partial_cmp(y) {
                    Some(o) => ord_matches(op, o),
                    None => op == CmpOp::NotEq,
                }))
            }
            _ => {}
        }
        self.rich_compare(op, a, b)
    }

    fn call_cmp_user(&mut self, recv: &Value, other: &Value, name: &str) -> R<Option<Value>> {
        if let Some(m) = self.user_special(recv, name) {
            let r = self.call_user_special(recv, &m, vec![other.clone()])?;
            if !matches!(r, Value::NotImplemented) {
                return Ok(Some(r));
            }
        }
        Ok(None)
    }

    pub fn rich_compare(&mut self, op: CmpOp, a: &Value, b: &Value) -> R<Value> {
        let a_user = matches!(a, Value::Obj(o) if o.cls.is_some());
        let b_user = matches!(b, Value::Obj(o) if o.cls.is_some());
        let (fwd, rev, sym) = cmp_info(op);
        if !a_user && !b_user {
            if let Some(r) = self.native_compare(op, a, b)? {
                return Ok(Value::Bool(r));
            }
        } else {
            let ta = self.type_of(a);
            let tb = self.type_of(b);
            let b_first = !Rc::ptr_eq(&ta, &tb) && self.is_subtype(&tb, &ta) && self.user_special(b, rev).is_some();
            if b_first {
                if let Some(r) = self.call_cmp_user(b, a, rev)? {
                    return Ok(r);
                }
            }
            if a_user {
                if let Some(r) = self.call_cmp_user(a, b, fwd)? {
                    return Ok(r);
                }
                if op == CmpOp::NotEq && self.user_special(a, "__ne__").is_none() {
                    if let Some(r) = self.call_cmp_user(a, b, "__eq__")? {
                        let t = self.truthy(&r)?;
                        return Ok(Value::Bool(!t));
                    }
                }
            }
            if !b_first && b_user {
                if let Some(r) = self.call_cmp_user(b, a, rev)? {
                    return Ok(r);
                }
                if op == CmpOp::NotEq && self.user_special(b, "__ne__").is_none() {
                    if let Some(r) = self.call_cmp_user(b, a, "__eq__")? {
                        let t = self.truthy(&r)?;
                        return Ok(Value::Bool(!t));
                    }
                }
            }
            if let Some(r) = self.native_compare(op, a, b)? {
                return Ok(Value::Bool(r));
            }
        }
        match op {
            CmpOp::Eq => Ok(Value::Bool(a.is(b))),
            CmpOp::NotEq => Ok(Value::Bool(!a.is(b))),
            _ => {
                let (ta, tb) = (self.type_name_of(a), self.type_name_of(b));
                Err(self.type_error(&format!("'{}' not supported between instances of '{}' and '{}'", sym, ta, tb)))
            }
        }
    }

    /// Comparison implemented by the builtin types; `None` means NotImplemented.
    pub fn native_compare(&mut self, op: CmpOp, a: &Value, b: &Value) -> R<Option<bool>> {
        if op == CmpOp::Eq || op == CmpOp::NotEq {
            let cx = |v: &Value| match v {
                Value::Obj(o) => match &o.kind {
                    Kind::Complex(r, i) => Some((*r, *i)),
                    _ => None,
                },
                _ => None,
            };
            let real = |s: &mut Self, v: &Value| -> Option<f64> {
                if cx(v).is_some() {
                    return None;
                }
                to_num(v)?;
                s.float_arg(v).ok()
            };
            let pair = match (cx(a), cx(b)) {
                (Some(p), None) => real(self, b).map(|f| (p, (f, 0.0))),
                (None, Some(q)) => real(self, a).map(|f| ((f, 0.0), q)),
                _ => None,
            };
            if let Some((p, q)) = pair {
                return Ok(Some((p == q) == (op == CmpOp::Eq)));
            }
        }
        if let (Some(x), Some(y)) = (to_num(a), to_num(b)) {
            return Ok(Some(match num_cmp(&x, &y) {
                Some(o) => ord_matches(op, o),
                None => op == CmpOp::NotEq,
            }));
        }
        let (oa, ob) = match (a, b) {
            (Value::Obj(x), Value::Obj(y)) => (x, y),
            _ => return Ok(None),
        };
        match (&oa.kind, &ob.kind) {
            (Kind::Str(x), Kind::Str(y)) => Ok(Some(ord_matches(op, lumen_common::smuggle::cmp_code_points(&x.s, &y.s)))),
            (Kind::Bytes(x), Kind::Bytes(y)) => Ok(Some(ord_matches(op, x.cmp(y)))),
            (Kind::ByteArray(x), Kind::ByteArray(y)) => Ok(Some(ord_matches(op, x.bytes().cmp(&y.bytes())))),
            (Kind::Bytes(x), Kind::ByteArray(y)) => Ok(Some(ord_matches(op, x.as_slice().cmp(&*y.bytes())))),
            (Kind::ByteArray(x), Kind::Bytes(y)) => Ok(Some(ord_matches(op, (*x.bytes()).cmp(y.as_slice())))),
            (Kind::Tuple(x), Kind::Tuple(y)) => {
                let (x, y) = (x.clone(), y.clone());
                self.seq_compare(op, &x, &y)
            }
            (Kind::List(x), Kind::List(y)) => {
                let (x, y) = (x.borrow().clone(), y.borrow().clone());
                self.seq_compare(op, &x, &y)
            }
            (Kind::Set(_) | Kind::FrozenSet(_), Kind::Set(_) | Kind::FrozenSet(_)) => self.set_compare(op, oa, ob),
            (Kind::DictView(d, vk), Kind::Set(_) | Kind::FrozenSet(_)) if *vk != ViewKind::Values => {
                let s = self.view_to_set(d, *vk)?;
                match s {
                    Value::Obj(so) => self.set_compare(op, &so, ob),
                    _ => Ok(None),
                }
            }
            (Kind::Set(_) | Kind::FrozenSet(_), Kind::DictView(d, vk)) if *vk != ViewKind::Values => {
                let s = self.view_to_set(d, *vk)?;
                match s {
                    Value::Obj(so) => self.set_compare(op, oa, &so),
                    _ => Ok(None),
                }
            }
            (Kind::DictView(d1, vk1), Kind::DictView(d2, vk2)) if *vk1 != ViewKind::Values && *vk2 != ViewKind::Values => {
                let s1 = self.view_to_set(d1, *vk1)?;
                let s2 = self.view_to_set(d2, *vk2)?;
                match (s1, s2) {
                    (Value::Obj(p), Value::Obj(q)) => self.set_compare(op, &p, &q),
                    _ => Ok(None),
                }
            }
            (Kind::Dict(x), Kind::Dict(y)) => {
                if op != CmpOp::Eq && op != CmpOp::NotEq {
                    return Ok(None);
                }
                if Rc::ptr_eq(oa, ob) {
                    return Ok(Some(op == CmpOp::Eq));
                }
                if x.borrow().len() != y.borrow().len() {
                    return Ok(Some(op == CmpOp::NotEq));
                }
                let entries: Vec<(Value, Value)> = x.borrow().iter().map(|e| (e.key.clone(), e.val.clone())).collect();
                for (k, v) in entries {
                    match self.dict_get(ob, &k)? {
                        Some(v2) => {
                            if !self.values_eq(&v, &v2)? {
                                return Ok(Some(op == CmpOp::NotEq));
                            }
                        }
                        None => return Ok(Some(op == CmpOp::NotEq)),
                    }
                }
                Ok(Some(op == CmpOp::Eq))
            }
            (Kind::BigRange(x), Kind::BigRange(y)) => {
                if op != CmpOp::Eq && op != CmpOp::NotEq {
                    return Ok(None);
                }
                let eq = x.iter().zip(y.iter()).all(|(p, q)| p.cmp(q) == std::cmp::Ordering::Equal);
                Ok(Some(eq == (op == CmpOp::Eq)))
            }
            (Kind::Range(x), Kind::Range(y)) => {
                if op != CmpOp::Eq && op != CmpOp::NotEq {
                    return Ok(None);
                }
                let (lx, ly) = (slice_len(x.start, x.stop, x.step), slice_len(y.start, y.stop, y.step));
                let eq = lx == ly && (lx == 0 || (x.start == y.start && (lx == 1 || x.step == y.step)));
                Ok(Some(eq == (op == CmpOp::Eq)))
            }
            (Kind::Slice(a1, b1, c1), Kind::Slice(a2, b2, c2)) => {
                let (x, y) = ([a1.clone(), b1.clone(), c1.clone()], [a2.clone(), b2.clone(), c2.clone()]);
                self.seq_compare(op, &x, &y)
            }
            (Kind::Complex(a1, b1), Kind::Complex(a2, b2)) => {
                if op != CmpOp::Eq && op != CmpOp::NotEq {
                    return Ok(None);
                }
                Ok(Some((a1 == a2 && b1 == b2) == (op == CmpOp::Eq)))
            }
            _ => Ok(None),
        }
    }

    fn seq_compare(&mut self, op: CmpOp, x: &[Value], y: &[Value]) -> R<Option<bool>> {
        if op == CmpOp::Eq || op == CmpOp::NotEq {
            if x.len() != y.len() {
                return Ok(Some(op == CmpOp::NotEq));
            }
            for (p, q) in x.iter().zip(y.iter()) {
                if !self.values_eq(p, q)? {
                    return Ok(Some(op == CmpOp::NotEq));
                }
            }
            return Ok(Some(op == CmpOp::Eq));
        }
        for (p, q) in x.iter().zip(y.iter()) {
            if !self.values_eq(p, q)? {
                let r = self.compare_op(op, p, q)?;
                return Ok(Some(self.truthy(&r)?));
            }
        }
        Ok(Some(ord_matches(op, x.len().cmp(&y.len()))))
    }

    fn set_compare(&mut self, op: CmpOp, a: &Obj, b: &Obj) -> R<Option<bool>> {
        let (la, lb) = (pydict_of(a).unwrap().borrow().len(), pydict_of(b).unwrap().borrow().len());
        let subset = |it: &mut Interp, x: &Obj, y: &Obj| -> R<bool> {
            let keys = pydict_of(x).unwrap().borrow().keys();
            for k in keys {
                if !it.set_contains(y, &k)? {
                    return Ok(false);
                }
            }
            Ok(true)
        };
        Ok(Some(match op {
            CmpOp::Eq => la == lb && subset(self, a, b)?,
            CmpOp::NotEq => !(la == lb && subset(self, a, b)?),
            CmpOp::LtE => la <= lb && subset(self, a, b)?,
            CmpOp::Lt => la < lb && subset(self, a, b)?,
            CmpOp::GtE => lb <= la && subset(self, b, a)?,
            CmpOp::Gt => lb < la && subset(self, b, a)?,
            _ => return Ok(None),
        }))
    }

    pub fn view_to_set(&mut self, d: &Obj, vk: ViewKind) -> R<Value> {
        let items: Vec<Value> = match pydict_of(d) {
            Some(p) => match vk {
                ViewKind::Keys => p.borrow().keys(),
                ViewKind::Values => p.borrow().values(),
                ViewKind::Items => p.borrow().iter().map(|e| Value::tuple(vec![e.key.clone(), e.val.clone()])).collect(),
            },
            None => Vec::new(),
        };
        self.new_set(items)
    }

    // ---- arithmetic ------------------------------------------------------------------------

    pub fn binary_op(&mut self, op: BinOp, a: &Value, b: &Value) -> R<Value> {
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => {
                if let Some(v) = Interp::int_binop_fast(op, *x, *y) {
                    return Ok(v);
                }
            }
            (Value::Float(x), Value::Float(y)) => match op {
                BinOp::Add => return Ok(Value::Float(x + y)),
                BinOp::Sub => return Ok(Value::Float(x - y)),
                BinOp::Mult => return Ok(Value::Float(x * y)),
                _ => {}
            },
            _ => {}
        }
        let a_user = matches!(a, Value::Obj(o) if o.cls.is_some());
        let b_user = matches!(b, Value::Obj(o) if o.cls.is_some());
        if !a_user && !b_user {
            if let Some(v) = self.native_binop(op, a, b)? {
                return Ok(v);
            }
            return Err(self.binop_error(op, a, b, false));
        }
        let (fwd, rev, _, _) = binop_info(op);
        let ta = self.type_of(a);
        let tb = self.type_of(b);
        let differ = !Rc::ptr_eq(&ta, &tb);
        let b_first = differ && self.is_subtype(&tb, &ta) && self.user_special(b, rev).is_some();
        if b_first {
            if let Some(r) = self.call_cmp_user(b, a, rev)? {
                return Ok(r);
            }
        }
        if a_user {
            if let Some(r) = self.call_cmp_user(a, b, fwd)? {
                return Ok(r);
            }
        }
        if let Some(v) = self.native_binop(op, a, b)? {
            return Ok(v);
        }
        if !b_first && differ && b_user {
            if let Some(r) = self.call_cmp_user(b, a, rev)? {
                return Ok(r);
            }
        }
        Err(self.binop_error(op, a, b, false))
    }

    pub fn binop_error(&mut self, op: BinOp, a: &Value, b: &Value, inplace: bool) -> Obj {
        let (_, _, _, sym) = binop_info(op);
        let (ta, tb) = (self.type_name_of(a), self.type_name_of(b));
        if op == BinOp::Add && !inplace || op == BinOp::Add {
            let seq_left = matches!(ta.as_str(), "str" | "list" | "tuple" | "bytes" | "bytearray");
            if seq_left && matches!(a, Value::Obj(_)) {
                return self.type_error(&format!("can only concatenate {} (not \"{}\") to {}", ta, tb, ta));
            }
        }
        if op == BinOp::Mult {
            let is_seq = |t: &str| matches!(t, "str" | "list" | "tuple" | "bytes" | "bytearray");
            if is_seq(&ta) && !self.has_index(b) {
                return self.type_error(&format!("can't multiply sequence by non-int of type '{}'", tb));
            }
            if is_seq(&tb) && !self.has_index(a) {
                return self.type_error(&format!("can't multiply sequence by non-int of type '{}'", ta));
            }
        }
        let sym = if inplace { format!("{}=", sym.trim_end_matches(" or pow()").trim_end_matches(" **")) } else { sym.to_string() };
        let sym = if sym == "** or pow()=" || sym == "**=" { "**=".to_string() } else { sym };
        self.type_error(&format!("unsupported operand type(s) for {}: '{}' and '{}'", sym, ta, tb))
    }

    pub fn inplace_op(&mut self, op: BinOp, a: Value, b: &Value) -> R<Value> {
        let (_, _, iname, _) = binop_info(op);
        if matches!(&a, Value::Obj(o) if o.cls.is_some()) {
            if let Some(r) = self.call_cmp_user(&a, b, iname)? {
                return Ok(r);
            }
        }
        if let Some(v) = self.native_inplace(op, &a, b)? {
            return Ok(v);
        }
        self.binary_op(op, &a, b).map_err(|e| {
            if self.exc_is(&e, "TypeError") {
                let msg = self.exc_message(&e);
                if msg.starts_with("unsupported operand type(s) for ") {
                    let (ta, tb) = (self.type_name_of(&a), self.type_name_of(b));
                    let (_, _, _, sym) = binop_info(op);
                    let sym = if sym == "** or pow()" { "**" } else { sym };
                    return self.type_error(&format!("unsupported operand type(s) for {}=: '{}' and '{}'", sym, ta, tb));
                }
            }
            e
        })
    }

    fn native_inplace(&mut self, op: BinOp, a: &Value, b: &Value) -> R<Option<Value>> {
        let o = match a {
            Value::Obj(o) => o,
            _ => return Ok(None),
        };
        match (&o.kind, op) {
            (Kind::List(l), BinOp::Add) => {
                let items = self.iterate_to_vec(b)?;
                l.borrow_mut().extend(items);
                Ok(Some(a.clone()))
            }
            (Kind::List(l), BinOp::Mult) => {
                if !self.has_index(b) {
                    return Ok(None);
                }
                let n = self.repeat_count(b)?;
                let cur = l.borrow().clone();
                let out = self.repeat_values(&cur, n)?;
                *l.borrow_mut() = out;
                Ok(Some(a.clone()))
            }
            (Kind::ByteArray(l), BinOp::Mult) => {
                if !self.has_index(b) {
                    return Ok(None);
                }
                let n = self.repeat_count(b)?;
                let cur = l.to_vec();
                let out = self.repeat_bytes(&cur, n, crate::limits::MAX_BYTES_LEN)?;
                self.ba_edit(l, |v| *v = out)?;
                Ok(Some(a.clone()))
            }
            (Kind::ByteArray(l), BinOp::Add) => {
                let items = self.bytes_of(b)?;
                self.ba_edit(l, |v| v.extend(items))?;
                Ok(Some(a.clone()))
            }
            (Kind::Set(_), BinOp::BitOr | BinOp::BitAnd | BinOp::Sub | BinOp::BitXor) => {
                if !matches!(b, Value::Obj(bo) if matches!(bo.kind, Kind::Set(_) | Kind::FrozenSet(_))) {
                    return Ok(None);
                }
                let r = self.set_binop(op, a, b)?;
                if let (Some(Value::Obj(res)), Kind::Set(dst)) = (r, &o.kind) {
                    if let Some(p) = pydict_of(&res) {
                        *dst.borrow_mut() = p.borrow().clone();
                    }
                }
                Ok(Some(a.clone()))
            }
            (Kind::Dict(_), BinOp::BitOr) => {
                self.dict_update_from(o, b)?;
                Ok(Some(a.clone()))
            }
            _ => Ok(None),
        }
    }

    pub fn exc_message(&mut self, e: &Obj) -> String {
        self.str_of(&Value::Obj(e.clone())).unwrap_or_default()
    }

    pub fn native_binop(&mut self, op: BinOp, a: &Value, b: &Value) -> R<Option<Value>> {
        if op == BinOp::BitOr {
            if let Some(u) = self.union_binop(a, b)? {
                return Ok(Some(u));
            }
        }
        if let Some(v) = self.num_binop(op, a, b)? {
            return Ok(if matches!(v, Value::NotImplemented) { None } else { Some(v) });
        }
        let fmt_lhs = op == BinOp::Mod && matches!(a, Value::Obj(x) if matches!(x.kind, Kind::Str(_) | Kind::Bytes(_) | Kind::ByteArray(_)));
        if fmt_lhs {
        } else if let (Value::Obj(x), Value::Obj(y)) = (a, b) {
            if let (Kind::Complex(..), _) | (_, Kind::Complex(..)) = (&x.kind, &y.kind) {
                return self.complex_binop(op, a, b);
            }
        } else if matches!(a, Value::Obj(x) if matches!(x.kind, Kind::Complex(..))) || matches!(b, Value::Obj(y) if matches!(y.kind, Kind::Complex(..))) {
            return self.complex_binop(op, a, b);
        }
        let oa = match a {
            Value::Obj(o) => o,
            _ => {
                if let Value::Obj(ob) = b {
                    if op == BinOp::Mult && self.has_index(a) {
                        return self.repeat_seq(ob, a);
                    }
                }
                return Ok(None);
            }
        };
        match (&oa.kind, op) {
            (Kind::Str(x), BinOp::Add) => {
                if let Some(y) = b.as_str() {
                    self.check_str_len(x.s.len() + y.len())?;
                    let mut s = String::with_capacity(x.s.len() + y.len());
                    s.push_str(&x.s);
                    s.push_str(y);
                    return Ok(Some(Value::string(s)));
                }
                Ok(None)
            }
            (Kind::Str(_), BinOp::Mod) => {
                let s = crate::builtins::format::percent_format(self, a, b)?;
                Ok(Some(Value::string(s)))
            }
            (Kind::Bytes(_) | Kind::ByteArray(_), BinOp::Mod) => {
                let raw = match &oa.kind {
                    Kind::Bytes(d) => d.clone(),
                    Kind::ByteArray(d) => d.to_vec(),
                    _ => Vec::new(),
                };
                let latin = |v: &Value| -> Value {
                    match v {
                        Value::Obj(o) => match &o.kind {
                            Kind::Bytes(d) => Value::string(d.iter().map(|&c| c as char).collect()),
                            Kind::ByteArray(d) => Value::string(d.bytes().iter().map(|&c| c as char).collect()),
                            _ => v.clone(),
                        },
                        _ => v.clone(),
                    }
                };
                let args = match b.tuple_items() {
                    Some(t) => {
                        let items: Vec<Value> = t.iter().map(latin).collect();
                        Value::tuple(items)
                    }
                    None => latin(b),
                };
                let fmt = Value::string(raw.iter().map(|&c| c as char).collect());
                let s = crate::builtins::format::percent_format(self, &fmt, &args)?;
                let mut out = Vec::with_capacity(s.len());
                for c in s.chars() {
                    match u8::try_from(c as u32) {
                        Ok(x) => out.push(x),
                        Err(_) => return Err(self.value_error("memoryview: invalid character in bytes format")),
                    }
                }
                if matches!(oa.kind, Kind::ByteArray(_)) {
                    return Ok(Some(Value::Obj(Object::new(Kind::ByteArray(ba_store(out))))));
                }
                Ok(Some(Value::bytes(out)))
            }
            (Kind::Str(_) | Kind::List(_) | Kind::Tuple(_) | Kind::Bytes(_) | Kind::ByteArray(_), BinOp::Mult) => {
                if self.has_index(b) {
                    self.repeat_seq(oa, b)
                } else {
                    Ok(None)
                }
            }
            (Kind::List(x), BinOp::Add) => match b {
                Value::Obj(ob) if matches!(ob.kind, Kind::List(_)) => {
                    if let Kind::List(y) = &ob.kind {
                        self.check_seq_len(x.borrow().len() + y.borrow().len())?;
                    }
                    let mut v = x.borrow().clone();
                    if let Kind::List(y) = &ob.kind {
                        v.extend(y.borrow().iter().cloned());
                    }
                    Ok(Some(Value::list(v)))
                }
                _ => Ok(None),
            },
            (Kind::Tuple(x), BinOp::Add) => match b.tuple_items() {
                Some(y) => {
                    self.check_seq_len(x.len() + y.len())?;
                    let mut v = x.clone();
                    v.extend(y.iter().cloned());
                    Ok(Some(Value::tuple(v)))
                }
                None => Ok(None),
            },
            (Kind::Bytes(x), BinOp::Add) => match b {
                Value::Obj(ob) => match &ob.kind {
                    Kind::Bytes(y) => {
                        self.check_bytes_len(x.len() + y.len())?;
                        let mut v = x.clone();
                        v.extend_from_slice(y);
                        Ok(Some(Value::bytes(v)))
                    }
                    Kind::ByteArray(y) => {
                        let mut v = x.clone();
                        v.extend_from_slice(&y.bytes());
                        Ok(Some(Value::bytes(v)))
                    }
                    _ => Ok(None),
                },
                _ => Ok(None),
            },
            (Kind::ByteArray(x), BinOp::Add) => match b {
                Value::Obj(ob) => {
                    let mut v = x.to_vec();
                    match &ob.kind {
                        Kind::Bytes(y) => v.extend_from_slice(y),
                        Kind::ByteArray(y) => v.extend_from_slice(&y.bytes()),
                        _ => return Ok(None),
                    }
                    Ok(Some(Value::Obj(Object::new(Kind::ByteArray(ba_store(v))))))
                }
                _ => Ok(None),
            },
            (Kind::Set(_) | Kind::FrozenSet(_), BinOp::BitOr | BinOp::BitAnd | BinOp::Sub | BinOp::BitXor) => {
                if matches!(b, Value::Obj(bo) if matches!(bo.kind, Kind::Set(_) | Kind::FrozenSet(_))) {
                    self.set_binop(op, a, b)
                } else if let Value::Obj(bo) = b {
                    if let Kind::DictView(d, vk) = &bo.kind {
                        if *vk != ViewKind::Values {
                            let s = self.view_to_set(d, *vk)?;
                            return self.set_binop(op, a, &s);
                        }
                    }
                    Ok(None)
                } else {
                    Ok(None)
                }
            }
            (Kind::DictView(d, vk), BinOp::BitOr | BinOp::BitAnd | BinOp::Sub | BinOp::BitXor) if *vk != ViewKind::Values => {
                let s = self.view_to_set(d, *vk)?;
                let other = match b {
                    Value::Obj(bo) => match &bo.kind {
                        Kind::Set(_) | Kind::FrozenSet(_) => b.clone(),
                        Kind::DictView(d2, vk2) if *vk2 != ViewKind::Values => self.view_to_set(d2, *vk2)?,
                        _ => {
                            let items = self.iterate_to_vec(b)?;
                            self.new_set(items)?
                        }
                    },
                    _ => return Ok(None),
                };
                self.set_binop(op, &s, &other)
            }
            (Kind::Dict(x), BinOp::BitOr) => match b {
                Value::Obj(ob) if matches!(ob.kind, Kind::Dict(_)) => {
                    let n = Object::new(Kind::Dict(RefCell::new(x.borrow().clone())));
                    self.dict_update_from(&n, b)?;
                    Ok(Some(Value::Obj(n)))
                }
                _ => Ok(None),
            },
            _ => {
                if op == BinOp::Mult {
                    if let Value::Obj(ob) = b {
                        if matches!(ob.kind, Kind::Str(_) | Kind::List(_) | Kind::Tuple(_) | Kind::Bytes(_) | Kind::ByteArray(_)) && self.has_index(a) {
                            return self.repeat_seq(ob, a);
                        }
                    }
                }
                Ok(None)
            }
        }
    }

    fn repeat_count(&mut self, n: &Value) -> R<usize> {
        match self.index_of(n) {
            Ok(n) => Ok(n.max(0) as usize),
            Err(e) if self.exc_is(&e, "OverflowError") => Err(self.overflow_err("cannot fit 'int' into an index-sized integer")),
            Err(e) => Err(e),
        }
    }

    /// `src` repeated `n` times, refusing results past the sequence cap before allocating.
    pub fn repeat_values(&mut self, src: &[Value], n: usize) -> R<Vec<Value>> {
        let total = size::repeat(src.len(), n, crate::limits::MAX_SEQ_LEN).map_err(|_| self.memory_error())?;
        if total == 0 {
            return Ok(Vec::new());
        }
        let mut out = self.vec_with_capacity(total, crate::limits::MAX_SEQ_LEN)?;
        let mut next_poll = 0;
        for _ in 0..n {
            out.extend_from_slice(src);
            if out.len() >= next_poll {
                self.poll()?;
                next_poll = out.len() + (1 << 16);
            }
        }
        Ok(out)
    }

    /// `src` repeated `n` times as bytes, built by doubling.
    pub fn repeat_bytes(&mut self, src: &[u8], n: usize, max: usize) -> R<Vec<u8>> {
        let total = size::repeat(src.len(), n, max).map_err(|_| self.memory_error())?;
        if total == 0 {
            return Ok(Vec::new());
        }
        let mut out = self.vec_with_capacity(total, max)?;
        out.extend_from_slice(src);
        while out.len() < total {
            let take = out.len().min(total - out.len());
            out.extend_from_within(..take);
            self.poll()?;
        }
        Ok(out)
    }

    fn repeat_seq(&mut self, seq: &Obj, n: &Value) -> R<Option<Value>> {
        let n = self.repeat_count(n)?;
        match &seq.kind {
            Kind::Str(s) => {
                let bytes = self.repeat_bytes(s.s.as_bytes(), n, crate::limits::MAX_STR_LEN)?;
                Ok(Some(Value::string(String::from_utf8(bytes).expect("whole copies of valid UTF-8"))))
            }
            Kind::List(l) => {
                let src = l.borrow().clone();
                Ok(Some(Value::list(self.repeat_values(&src, n)?)))
            }
            Kind::Tuple(t) => Ok(Some(Value::tuple(self.repeat_values(t, n)?))),
            Kind::Bytes(b) => Ok(Some(Value::bytes(self.repeat_bytes(b, n, crate::limits::MAX_BYTES_LEN)?))),
            Kind::ByteArray(b) => {
                let src = b.to_vec();
                let out = self.repeat_bytes(&src, n, crate::limits::MAX_BYTES_LEN)?;
                Ok(Some(Value::Obj(Object::new(Kind::ByteArray(ba_store(out))))))
            }
            _ => Ok(None),
        }
    }

    pub fn set_binop(&mut self, op: BinOp, a: &Value, b: &Value) -> R<Option<Value>> {
        let (oa, ob) = match (a, b) {
            (Value::Obj(x), Value::Obj(y)) => (x, y),
            _ => return Ok(None),
        };
        let frozen = matches!(oa.kind, Kind::FrozenSet(_));
        let res = Object::new(if frozen { Kind::FrozenSet(RefCell::new(PyDict::new_set())) } else { Kind::Set(RefCell::new(PyDict::new_set())) });
        let ka = pydict_of(oa).unwrap().borrow().keys();
        let kb = pydict_of(ob).unwrap().borrow().keys();
        match op {
            BinOp::BitOr => {
                for k in ka.into_iter().chain(kb) {
                    self.set_add_obj(&res, k)?;
                }
            }
            BinOp::BitAnd => {
                for k in ka {
                    if self.set_contains(ob, &k)? {
                        self.set_add_obj(&res, k)?;
                    }
                }
            }
            BinOp::Sub => {
                for k in ka {
                    if !self.set_contains(ob, &k)? {
                        self.set_add_obj(&res, k)?;
                    }
                }
            }
            BinOp::BitXor => {
                for k in &ka {
                    if !self.set_contains(ob, k)? {
                        self.set_add_obj(&res, k.clone())?;
                    }
                }
                for k in kb {
                    if !self.set_contains(oa, &k)? {
                        self.set_add_obj(&res, k)?;
                    }
                }
            }
            _ => return Ok(None),
        }
        Ok(Some(Value::Obj(res)))
    }

    fn complex_binop(&mut self, op: BinOp, a: &Value, b: &Value) -> R<Option<Value>> {
        let get = |v: &Value| -> Option<(f64, f64)> {
            match v {
                Value::Obj(o) => match &o.kind {
                    Kind::Complex(r, i) => Some((*r, *i)),
                    Kind::Int(b) => b.to_float().map(|f| (f, 0.0)),
                    Kind::Float(f) => Some((*f, 0.0)),
                    _ => None,
                },
                Value::Int(i) => Some((*i as f64, 0.0)),
                Value::Bool(b) => Some((*b as i64 as f64, 0.0)),
                Value::Float(f) => Some((*f, 0.0)),
                _ => None,
            }
        };
        let ((a1, b1), (a2, b2)) = match (get(a), get(b)) {
            (Some(x), Some(y)) => (x, y),
            _ => return Ok(None),
        };
        let mk = |r: f64, i: f64| Some(Value::Obj(Object::new(Kind::Complex(r, i))));
        Ok(match op {
            BinOp::Add => mk(a1 + a2, b1 + b2),
            BinOp::Sub => mk(a1 - a2, b1 - b2),
            BinOp::Mult => mk(a1 * a2 - b1 * b2, a1 * b2 + b1 * a2),
            BinOp::Div => {
                let d = a2 * a2 + b2 * b2;
                if d == 0.0 {
                    return Err(self.zero_div("division by zero"));
                }
                mk((a1 * a2 + b1 * b2) / d, (b1 * a2 - a1 * b2) / d)
            }
            BinOp::Pow => {
                if a2 == 0.0 && b2 == 0.0 {
                    return Ok(mk(1.0, 0.0));
                }
                if b2 == 0.0 && a2 == fmath::trunc(a2) && a2.abs() <= 100.0 {
                    let mul = |x: (f64, f64), y: (f64, f64)| (x.0 * y.0 - x.1 * y.1, x.0 * y.1 + x.1 * y.0);
                    let mut n = a2.abs() as u32;
                    let (mut result, mut base) = ((1.0, 0.0), (a1, b1));
                    while n > 0 {
                        if n & 1 == 1 {
                            result = mul(result, base);
                        }
                        base = mul(base, base);
                        n >>= 1;
                    }
                    if a2 < 0.0 {
                        let d = result.0 * result.0 + result.1 * result.1;
                        if d == 0.0 {
                            return Err(self.zero_div("zero to a negative power"));
                        }
                        result = (result.0 / d, -result.1 / d);
                    }
                    return Ok(mk(result.0, result.1));
                }
                let r = fmath::sqrt(a1 * a1 + b1 * b1);
                let theta = fmath::atan2(b1, a1);
                if r == 0.0 {
                    return Ok(mk(0.0, 0.0));
                }
                let lnr = fmath::ln(r);
                let nr = fmath::exp(a2 * lnr - b2 * theta);
                let nt = b2 * lnr + a2 * theta;
                mk(nr * fmath::cos(nt), nr * fmath::sin(nt))
            }
            _ => None,
        })
    }

    pub fn unary_op(&mut self, op: UnOp, a: &Value) -> R<Value> {
        if op == UnOp::Not {
            return Ok(Value::Bool(!self.truthy(a)?));
        }
        if let Value::Obj(o) = a {
            if o.cls.is_some() {
                let name = match op {
                    UnOp::Neg => "__neg__",
                    UnOp::Pos => "__pos__",
                    _ => "__invert__",
                };
                if let Some(m) = self.user_special(a, name) {
                    return self.call_user_special(a, &m, Vec::new());
                }
            }
        }
        match (op, a) {
            (UnOp::Neg, Value::Int(i)) => {
                return Ok(match i.checked_neg() {
                    Some(n) => Value::Int(n),
                    None => Value::big(BigInt::from_i64(*i).neg()),
                })
            }
            (UnOp::Neg, Value::Bool(b)) => return Ok(Value::Int(-(*b as i64))),
            (UnOp::Neg, Value::Float(f)) => return Ok(Value::Float(-f)),
            (UnOp::Pos, Value::Int(_) | Value::Float(_)) => return Ok(a.clone()),
            (UnOp::Pos, Value::Bool(b)) => return Ok(Value::Int(*b as i64)),
            (UnOp::Invert, Value::Int(i)) => return Ok(Value::Int(!*i)),
            (UnOp::Invert, Value::Bool(b)) => return Ok(Value::Int(!(*b as i64))),
            _ => {}
        }
        if let Value::Obj(o) = a {
            match (&o.kind, op) {
                (Kind::Int(b), UnOp::Neg) => return Ok(Value::big(b.neg())),
                (Kind::Int(b), UnOp::Pos) => return Ok(Value::big(b.clone())),
                (Kind::Int(b), UnOp::Invert) => return Ok(Value::big(b.not())),
                (Kind::Float(f), UnOp::Neg) => return Ok(Value::Float(-f)),
                (Kind::Float(f), UnOp::Pos) => return Ok(Value::Float(*f)),
                (Kind::Complex(r, i), UnOp::Neg) => return Ok(Value::Obj(Object::new(Kind::Complex(-r, -i)))),
                (Kind::Complex(r, i), UnOp::Pos) => return Ok(Value::Obj(Object::new(Kind::Complex(*r, *i)))),
                _ => {}
            }
        }
        let sym = match op {
            UnOp::Neg => "-",
            UnOp::Pos => "+",
            _ => "~",
        };
        let t = self.type_name_of(a);
        Err(self.type_error(&format!("bad operand type for unary {}: '{}'", sym, t)))
    }

    // ---- subscripting ----------------------------------------------------------------------

    pub fn slice_bounds(&mut self, s: &Value, len: usize) -> R<(i64, i64, i64)> {
        let (start, stop, step) = match s {
            Value::Obj(o) => match &o.kind {
                Kind::Slice(a, b, c) => (a.clone(), b.clone(), c.clone()),
                _ => return Err(self.type_error("slice expected")),
            },
            _ => return Err(self.type_error("slice expected")),
        };
        let len = len as i64;
        let step = if step.is_none() { 1 } else { self.index_of(&step)? };
        if step == 0 {
            return Err(self.value_error("slice step cannot be zero"));
        }
        let adjust = |it: &mut Interp, v: &Value, dflt: i64| -> R<i64> {
            if v.is_none() {
                return Ok(dflt);
            }
            let mut i = match it.index_of(v) {
                Ok(i) => i,
                Err(e) => {
                    if it.exc_is(&e, "OverflowError") {
                        if let Some(b) = v.as_bigint() {
                            return Ok(if b.is_negative() { if step < 0 { -1 } else { 0 } } else if step < 0 { len - 1 } else { len });
                        }
                    }
                    return Err(e);
                }
            };
            if i < 0 {
                i += len;
                if i < 0 {
                    i = if step < 0 { -1 } else { 0 };
                }
            } else if i >= len {
                i = if step < 0 { len - 1 } else { len };
            }
            Ok(i)
        };
        let (dstart, dstop) = if step > 0 { (0, len) } else { (len - 1, -1) };
        let start = adjust(self, &start, dstart)?;
        let stop = adjust(self, &stop, dstop)?;
        Ok((start, stop, step))
    }

    pub fn slice_indices(&mut self, s: &Value, len: usize) -> R<Vec<usize>> {
        let (start, stop, step) = self.slice_bounds(s, len)?;
        let n = slice_len(start, stop, step);
        let mut out = Vec::with_capacity(n);
        let mut i = start;
        for _ in 0..n {
            out.push(i as usize);
            i += step;
        }
        Ok(out)
    }

    fn seq_index_error(&mut self, what: &str) -> Obj {
        self.new_exc_str("IndexError", &format!("{} index out of range", what))
    }

    fn bad_index_type(&mut self, what: &str, key: &Value) -> Obj {
        let t = self.type_name_of(key);
        if what == "string" {
            return self.type_error(&format!("string indices must be integers, not '{}'", t));
        }
        self.type_error(&format!("{} indices must be integers or slices, not {}", what, t))
    }

    pub fn getitem(&mut self, obj: &Value, key: &Value) -> R<Value> {
        let o = match obj {
            Value::Obj(o) => o,
            _ => {
                let t = self.type_name_of(obj);
                return Err(self.type_error(&format!("'{}' object is not subscriptable", t)));
            }
        };
        if o.cls.is_some() {
            if let Some(m) = self.user_special(obj, "__getitem__") {
                return self.call_user_special(obj, &m, vec![key.clone()]);
            }
        }
        self.native_getitem(obj, key)
    }

    pub fn native_getitem(&mut self, obj: &Value, key: &Value) -> R<Value> {
        let o = match obj {
            Value::Obj(o) => o,
            _ => {
                let t = self.type_name_of(obj);
                return Err(self.type_error(&format!("'{}' object is not subscriptable", t)));
            }
        };
        match &o.kind {
            Kind::List(l) => {
                if let Value::Int(i) = key {
                    let l = l.borrow();
                    return match norm_index(*i, l.len()) {
                        Some(j) => Ok(l[j].clone()),
                        None => Err(self.seq_index_error("list")),
                    };
                }
                if self.is_slice(key) {
                    let len = l.borrow().len();
                    let idx = self.slice_indices(key, len)?;
                    let l = l.borrow();
                    return Ok(Value::list(idx.into_iter().filter_map(|i| l.get(i).cloned()).collect()));
                }
                if self.has_index(key) {
                    let i = self.seq_index(key)?;
                    let l = l.borrow();
                    return match norm_index(i, l.len()) {
                        Some(j) => Ok(l[j].clone()),
                        None => Err(self.seq_index_error("list")),
                    };
                }
                Err(self.bad_index_type("list", key))
            }
            Kind::Tuple(t) => {
                if self.is_slice(key) {
                    let idx = self.slice_indices(key, t.len())?;
                    return Ok(Value::tuple(idx.into_iter().map(|i| t[i].clone()).collect()));
                }
                if self.has_index(key) {
                    let i = self.seq_index(key)?;
                    return match norm_index(i, t.len()) {
                        Some(j) => Ok(t[j].clone()),
                        None => Err(self.seq_index_error("tuple")),
                    };
                }
                Err(self.bad_index_type("tuple", key))
            }
            Kind::Str(s) => {
                if self.is_slice(key) {
                    let idx_bounds = self.slice_bounds(key, s.nchars)?;
                    let (start, stop, step) = idx_bounds;
                    if step == 1 {
                        return Ok(Value::str(s.slice(start as usize, stop.max(start) as usize)));
                    }
                    let n = slice_len(start, stop, step);
                    let mut out = String::with_capacity(n);
                    if s.ascii {
                        let b = s.s.as_bytes();
                        let mut i = start;
                        for _ in 0..n {
                            out.push(b[i as usize] as char);
                            i += step;
                        }
                    } else {
                        let chars: Vec<u32> = lumen_common::smuggle::code_points(&s.s).collect();
                        let mut i = start;
                        for _ in 0..n {
                            lumen_common::smuggle::push_code_point(&mut out, chars[i as usize]);
                            i += step;
                        }
                    }
                    return Ok(Value::string(out));
                }
                if self.has_index(key) {
                    let i = self.seq_index(key)?;
                    return match norm_index(i, s.nchars) {
                        Some(j) => Ok(Value::str(s.slice(j, j + 1))),
                        None => Err(self.seq_index_error("string")),
                    };
                }
                Err(self.bad_index_type("string", key))
            }
            Kind::Dict(d) => {
                let _ = d;
                match self.dict_get(o, key)? {
                    Some(v) => Ok(v),
                    None => {
                        if o.cls.is_some() {
                            if let Some(m) = self.user_special(obj, "__missing__") {
                                return self.call_user_special(obj, &m, vec![key.clone()]);
                            }
                        }
                        Err(self.new_exc_val("KeyError", key.clone()))
                    }
                }
            }
            Kind::Bytes(b) => {
                if self.is_slice(key) {
                    let idx = self.slice_indices(key, b.len())?;
                    return Ok(Value::bytes(idx.into_iter().map(|i| b[i]).collect()));
                }
                let i = self.seq_index(key)?;
                match norm_index(i, b.len()) {
                    Some(j) => Ok(Value::Int(b[j] as i64)),
                    None => Err(self.new_exc_str("IndexError", "index out of range")),
                }
            }
            Kind::ByteArray(b) => {
                if self.is_slice(key) {
                    let len = b.len();
                    let idx = self.slice_indices(key, len)?;
                    let b = b.bytes();
                    return Ok(Value::Obj(Object::new(Kind::ByteArray(ba_store(idx.into_iter().map(|i| b[i]).collect())))));
                }
                let i = self.seq_index(key)?;
                let b = b.bytes();
                match norm_index(i, b.len()) {
                    Some(j) => Ok(Value::Int(b[j] as i64)),
                    None => Err(self.new_exc_str("IndexError", "bytearray index out of range")),
                }
            }
            Kind::BigRange(r) => {
                if self.is_slice(key) {
                    return Err(self.type_error("slicing a range with huge bounds is not supported"));
                }
                let Some(i) = key.as_bigint() else {
                    let t = self.type_name_of(key);
                    return Err(self.type_error(&format!("range indices must be integers or slices, not {}", t)));
                };
                let len = big_range_len(r);
                let j = if i.is_negative() { i.add(&len) } else { i };
                if j.is_negative() || j.cmp(&len) != std::cmp::Ordering::Less {
                    return Err(self.seq_index_error("range object"));
                }
                Ok(Value::big(r[0].add(&j.mul(&r[2]))))
            }
            Kind::Range(r) => {
                let len = slice_len(r.start, r.stop, r.step);
                if self.is_slice(key) {
                    let (start, stop, step) = self.slice_bounds(key, len)?;
                    return Ok(Value::Obj(Object::new(Kind::Range(RangeData {
                        start: r.start + start * r.step,
                        stop: r.start + stop * r.step,
                        step: r.step * step,
                    }))));
                }
                let i = match self.index_of(key) {
                    Err(e) if self.exc_is(&e, "OverflowError") => return Err(self.seq_index_error("range object")),
                    r => r?,
                };
                match norm_index(i, len) {
                    Some(j) => Ok(Value::Int(r.start + j as i64 * r.step)),
                    None => Err(self.seq_index_error("range object")),
                }
            }
            Kind::Type(_) => {
                if let Some(m) = self.lookup_mro(&self.type_of_obj(o), "__getitem__") {
                    let meta = self.type_of_obj(o);
                    let b = self.bind_descr(&m, obj, &meta)?;
                    return self.call(&b, vec![key.clone()], Vec::new());
                }
                if let Some(m) = self.lookup_mro(o, "__class_getitem__") {
                    let b = match &m {
                        Value::Obj(f) if matches!(f.kind, Kind::Function(_)) => Value::Obj(Object::new(Kind::Method(m.clone(), obj.clone()))),
                        _ => self.bind_descr_cls(&m, o)?,
                    };
                    return self.call(&b, vec![key.clone()], Vec::new());
                }
                if self.is_builtin_generic(o) {
                    return Ok(self.make_alias(obj.clone(), key));
                }
                let n = self.type_name(o);
                Err(self.type_error(&format!("type '{}' is not subscriptable", n)))
            }
            _ => {
                let cls = self.type_of_obj(o);
                match self.lookup_mro(&cls, "__getitem__") {
                    Some(m) => {
                        let b = self.bind_descr(&m, obj, &cls)?;
                        self.call(&b, vec![key.clone()], Vec::new())
                    }
                    None => {
                        let t = self.type_name_of(obj);
                        Err(self.type_error(&format!("'{}' object is not subscriptable", t)))
                    }
                }
            }
        }
    }

    fn is_builtin_generic(&self, t: &Obj) -> bool {
        !self.is_heap(t)
            && matches!(
                self.type_name(t).as_str(),
                "list" | "dict" | "set" | "frozenset" | "tuple" | "type" | "str" | "int" | "float" | "bytes"
            )
    }

    pub fn is_slice(&self, v: &Value) -> bool {
        matches!(v, Value::Obj(o) if matches!(o.kind, Kind::Slice(..)))
    }

    pub fn setitem(&mut self, obj: &Value, key: Value, val: Value) -> R<()> {
        let o = match obj {
            Value::Obj(o) => o,
            _ => {
                let t = self.type_name_of(obj);
                return Err(self.type_error(&format!("'{}' object does not support item assignment", t)));
            }
        };
        if o.cls.is_some() {
            if let Some(m) = self.user_special(obj, "__setitem__") {
                self.call_user_special(obj, &m, vec![key, val])?;
                return Ok(());
            }
        }
        self.native_setitem(obj, key, val)
    }

    pub fn native_setitem(&mut self, obj: &Value, key: Value, val: Value) -> R<()> {
        let o = match obj {
            Value::Obj(o) => o,
            _ => {
                let t = self.type_name_of(obj);
                return Err(self.type_error(&format!("'{}' object does not support item assignment", t)));
            }
        };
        match &o.kind {
            Kind::List(l) => {
                if self.is_slice(&key) {
                    let len = l.borrow().len();
                    let (start, stop, step) = self.slice_bounds(&key, len)?;
                    let items = match self.iterate_to_vec(&val) {
                        Ok(i) => i,
                        Err(e) if self.exc_is(&e, "TypeError") && self.get_iter(&val).is_err() => {
                            return Err(self.type_error("must assign iterable to extended slice"));
                        }
                        Err(e) => return Err(e),
                    };
                    return self.list_slice_assign(l, start, stop, step, items);
                }
                let i = if let Value::Int(i) = &key {
                    *i
                } else if self.has_index(&key) {
                    self.seq_index(&key)?
                } else {
                    return Err(self.bad_index_type("list", &key));
                };
                let mut l = l.borrow_mut();
                match norm_index(i, l.len()) {
                    Some(j) => {
                        l[j] = val;
                        Ok(())
                    }
                    None => {
                        drop(l);
                        Err(self.new_exc_str("IndexError", "list assignment index out of range"))
                    }
                }
            }
            Kind::Dict(_) => self.dict_set(o, key, val),
            Kind::ByteArray(b) => {
                if self.is_slice(&key) {
                    let len = b.len();
                    let (start, stop, step) = self.slice_bounds(&key, len)?;
                    let items: Vec<u8> = self.bytes_of(&val)?;
                    if step == 1 {
                        let (start, stop) = (start as usize, stop.max(start) as usize);
                        if items.len() == stop - start {
                            self.ba_write(b)?[start..stop].copy_from_slice(&items);
                        } else {
                            self.ba_edit(b, |v| drop(v.splice(start..stop, items)))?;
                        }
                        return Ok(());
                    }
                    let n = slice_len(start, stop, step);
                    if items.len() != n {
                        let msg = format!("attempt to assign bytes of size {} to extended slice of size {}", items.len(), n);
                        return Err(self.value_error(&msg));
                    }
                    let mut b = self.ba_write(b)?;
                    let mut idx = start;
                    for v in items {
                        b[idx as usize] = v;
                        idx += step;
                    }
                    return Ok(());
                }
                let i = self.seq_index(&key)?;
                let v = self.index_of(&val)?;
                if !(0..256).contains(&v) {
                    return Err(self.value_error("byte must be in range(0, 256)"));
                }
                let mut b = self.ba_write(b)?;
                match norm_index(i, b.len()) {
                    Some(j) => {
                        b[j] = v as u8;
                        Ok(())
                    }
                    None => {
                        drop(b);
                        Err(self.new_exc_str("IndexError", "bytearray index out of range"))
                    }
                }
            }
            _ => {
                let cls = self.type_of_obj(o);
                match self.lookup_mro(&cls, "__setitem__") {
                    Some(m) => {
                        let b = self.bind_descr(&m, obj, &cls)?;
                        self.call(&b, vec![key, val], Vec::new())?;
                        Ok(())
                    }
                    None => {
                        let t = self.type_name_of(obj);
                        Err(self.type_error(&format!("'{}' object does not support item assignment", t)))
                    }
                }
            }
        }
    }

    fn list_slice_assign(&mut self, l: &RefCell<Vec<Value>>, start: i64, stop: i64, step: i64, items: Vec<Value>) -> R<()> {
        if step == 1 {
            let stop = stop.max(start) as usize;
            l.borrow_mut().splice(start as usize..stop, items);
            return Ok(());
        }
        let n = slice_len(start, stop, step);
        if n != items.len() {
            return Err(self.value_error(&format!(
                "attempt to assign sequence of size {} to extended slice of size {}",
                items.len(),
                n
            )));
        }
        let mut lm = l.borrow_mut();
        let mut i = start;
        for it in items {
            lm[i as usize] = it;
            i += step;
        }
        Ok(())
    }

    pub fn delitem(&mut self, obj: &Value, key: &Value) -> R<()> {
        let o = match obj {
            Value::Obj(o) => o,
            _ => {
                let t = self.type_name_of(obj);
                return Err(self.type_error(&format!("'{}' object doesn't support item deletion", t)));
            }
        };
        if o.cls.is_some() {
            if let Some(m) = self.user_special(obj, "__delitem__") {
                self.call_user_special(obj, &m, vec![key.clone()])?;
                return Ok(());
            }
        }
        self.native_delitem(obj, key)
    }

    pub fn native_delitem(&mut self, obj: &Value, key: &Value) -> R<()> {
        let o = match obj {
            Value::Obj(o) => o,
            _ => {
                let t = self.type_name_of(obj);
                return Err(self.type_error(&format!("'{}' object doesn't support item deletion", t)));
            }
        };
        match &o.kind {
            Kind::List(l) => {
                if self.is_slice(key) {
                    let len = l.borrow().len();
                    let (start, stop, step) = self.slice_bounds(key, len)?;
                    if step == 1 {
                        let stop = stop.max(start) as usize;
                        l.borrow_mut().drain(start as usize..stop);
                        return Ok(());
                    }
                    let mut idx = self.slice_indices(key, len)?;
                    idx.sort_unstable();
                    let mut lm = l.borrow_mut();
                    for i in idx.into_iter().rev() {
                        lm.remove(i);
                    }
                    return Ok(());
                }
                let i = self.seq_index(key)?;
                let mut lm = l.borrow_mut();
                match norm_index(i, lm.len()) {
                    Some(j) => {
                        lm.remove(j);
                        Ok(())
                    }
                    None => {
                        drop(lm);
                        Err(self.new_exc_str("IndexError", "list assignment index out of range"))
                    }
                }
            }
            Kind::Dict(_) => match self.dict_remove(o, key)? {
                Some(_) => Ok(()),
                None => Err(self.new_exc_val("KeyError", key.clone())),
            },
            Kind::ByteArray(b) => {
                if self.is_slice(key) {
                    let len = b.len();
                    let mut idx = self.slice_indices(key, len)?;
                    if idx.is_empty() {
                        return Ok(());
                    }
                    idx.sort_unstable();
                    self.ba_edit(b, |bm| {
                        for i in idx.into_iter().rev() {
                            bm.remove(i);
                        }
                    })?;
                    return Ok(());
                }
                let i = self.seq_index(key)?;
                match norm_index(i, b.len()) {
                    Some(j) => {
                        self.ba_edit(b, |bm| bm.remove(j))?;
                        Ok(())
                    }
                    None => {
                        Err(self.new_exc_str("IndexError", "bytearray index out of range"))
                    }
                }
            }
            _ => {
                let cls = self.type_of_obj(o);
                match self.lookup_mro(&cls, "__delitem__") {
                    Some(m) => {
                        let b = self.bind_descr(&m, obj, &cls)?;
                        self.call(&b, vec![key.clone()], Vec::new())?;
                        Ok(())
                    }
                    None => {
                        let t = self.type_name_of(obj);
                        Err(self.type_error(&format!("'{}' object doesn't support item deletion", t)))
                    }
                }
            }
        }
    }

    pub fn contains(&mut self, container: &Value, item: &Value) -> R<bool> {
        let o = match container {
            Value::Obj(o) => o,
            _ => {
                let t = self.type_name_of(container);
                return Err(self.type_error(&format!("argument of type '{}' is not iterable", t)));
            }
        };
        if o.cls.is_some() {
            if let Some(m) = self.user_special(container, "__contains__") {
                let r = self.call_user_special(container, &m, vec![item.clone()])?;
                return self.truthy(&r);
            }
        }
        self.native_contains(container, item)
    }

    pub fn native_contains(&mut self, container: &Value, item: &Value) -> R<bool> {
        let o = match container {
            Value::Obj(o) => o,
            _ => {
                let t = self.type_name_of(container);
                return Err(self.type_error(&format!("argument of type '{}' is not iterable", t)));
            }
        };
        match &o.kind {
            Kind::Str(s) => match item.as_str() {
                Some(sub) => Ok(s.s.contains(sub)),
                None => {
                    let t = self.type_name_of(item);
                    Err(self.type_error(&format!("'in <string>' requires string as left operand, not {}", t)))
                }
            },
            Kind::List(l) => {
                let mut i = 0;
                loop {
                    let x = match l.borrow().get(i) {
                        Some(x) => x.clone(),
                        None => return Ok(false),
                    };
                    if self.values_eq(&x, item)? {
                        return Ok(true);
                    }
                    i += 1;
                }
            }
            Kind::Tuple(t) => {
                for x in t.clone() {
                    if self.values_eq(&x, item)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Kind::Dict(_) => Ok(self.dict_get(o, item)?.is_some()),
            Kind::Set(_) | Kind::FrozenSet(_) => self.set_contains(o, item),
            Kind::BigRange(r) => {
                let Some(i) = item.as_bigint() else { return Ok(false) };
                let d = i.sub(&r[0]);
                let (q, m) = d.floor_divmod(&r[2]);
                Ok(m.is_zero() && !q.is_negative() && q.cmp(&big_range_len(r)) == std::cmp::Ordering::Less)
            }
            Kind::Range(r) => {
                if let Value::Int(i) = item {
                    let len = slice_len(r.start, r.stop, r.step) as i64;
                    let k = if r.step > 0 { (*i - r.start).checked_div(r.step) } else { (r.start - *i).checked_div(-r.step) };
                    let rem_ok = (*i - r.start) % r.step == 0;
                    return Ok(rem_ok && matches!(k, Some(k) if k >= 0 && k < len));
                }
                let items = self.iterate_to_vec(container)?;
                for x in items {
                    if self.values_eq(&x, item)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Kind::Bytes(b) => self.bytes_contains(b, item),
            Kind::ByteArray(b) => {
                let b = b.to_vec();
                self.bytes_contains(&b, item)
            }
            Kind::DictView(d, vk) => match vk {
                ViewKind::Keys => Ok(self.dict_get(d, item)?.is_some()),
                ViewKind::Values => {
                    let vals = pydict_of(d).map(|p| p.borrow().values()).unwrap_or_default();
                    for v in vals {
                        if self.values_eq(&v, item)? {
                            return Ok(true);
                        }
                    }
                    Ok(false)
                }
                ViewKind::Items => match item.tuple_items() {
                    Some([k, v]) => match self.dict_get(d, k)? {
                        Some(cur) => self.values_eq(&cur, v),
                        None => Ok(false),
                    },
                    _ => Ok(false),
                },
            },
            _ => {
                let cls = self.type_of_obj(o);
                if let Some(m) = self.lookup_mro(&cls, "__contains__") {
                    let b = self.bind_descr(&m, container, &cls)?;
                    let r = self.call(&b, vec![item.clone()], Vec::new())?;
                    return self.truthy(&r);
                }
                let it = self.get_iter(container).map_err(|e| {
                    if self.exc_is(&e, "TypeError") {
                        let t = self.type_name_of(container);
                        self.type_error(&format!("argument of type '{}' is not iterable", t))
                    } else {
                        e
                    }
                })?;
                while let Some(x) = self.iter_next(&it)? {
                    if self.values_eq(&x, item)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
        }
    }

    fn bytes_contains(&mut self, b: &[u8], item: &Value) -> R<bool> {
        match item {
            Value::Int(i) => {
                if !(0..256).contains(i) {
                    return Err(self.value_error("byte must be in range(0, 256)"));
                }
                Ok(b.contains(&(*i as u8)))
            }
            Value::Obj(o) => match &o.kind {
                Kind::Bytes(s) => Ok(s.is_empty() || b.windows(s.len()).any(|w| w == s.as_slice())),
                Kind::ByteArray(s) => {
                    let s = s.bytes();
                    Ok(s.is_empty() || b.windows(s.len()).any(|w| w == &s[..]))
                }
                _ => Err(self.type_error("a bytes-like object is required, not 'str'")),
            },
            _ => Err(self.type_error("a bytes-like object is required")),
        }
    }

    pub fn bytes_of(&mut self, v: &Value) -> R<Vec<u8>> {
        match v {
            Value::Obj(o) => match &o.kind {
                Kind::Bytes(b) => Ok(b.clone()),
                Kind::ByteArray(b) => Ok(b.to_vec()),
                Kind::Opaque(_) if crate::builtins::memview::is_memoryview(self, v) => {
                    Ok(crate::builtins::memview::contiguous_bytes(self, v)?.unwrap_or_default())
                }
                Kind::List(_) | Kind::Tuple(_) | Kind::Range(_) => {
                    let items = self.iterate_to_vec(v)?;
                    let mut out = Vec::new();
                    for i in items {
                        let n = self.index_of(&i)?;
                        if !(0..256).contains(&n) {
                            return Err(self.value_error("bytes must be in range(0, 256)"));
                        }
                        out.push(n as u8);
                    }
                    Ok(out)
                }
                _ => {
                    let t = self.type_name_of(v);
                    Err(self.type_error(&format!("a bytes-like object is required, not '{}'", t)))
                }
            },
            _ => {
                let t = self.type_name_of(v);
                Err(self.type_error(&format!("a bytes-like object is required, not '{}'", t)))
            }
        }
    }

    pub fn unpack_items(&mut self, v: &Value, n: usize, after: Option<usize>) -> R<Vec<Value>> {
        let mut sized = false;
        let limit = if after.is_none() { Some(n + 1) } else { None };
        let items: Vec<Value> = match v {
            Value::Obj(o) if o.cls.is_none() => match &o.kind {
                Kind::Tuple(t) => {
                    sized = true;
                    t.clone()
                }
                Kind::List(l) => {
                    sized = true;
                    l.borrow().clone()
                }
                Kind::Dict(_) | Kind::Set(_) | Kind::FrozenSet(_) => {
                    sized = true;
                    self.iterate_unpack(v, None)?
                }
                _ => self.iterate_unpack(v, limit)?,
            },
            _ => self.iterate_unpack(v, limit)?,
        };
        match after {
            None => {
                if items.len() < n {
                    return Err(self.value_error(&format!("not enough values to unpack (expected {}, got {})", n, items.len())));
                }
                if items.len() > n {
                    let msg = if sized {
                        format!("too many values to unpack (expected {}, got {})", n, items.len())
                    } else {
                        format!("too many values to unpack (expected {})", n)
                    };
                    return Err(self.value_error(&msg));
                }
                Ok(items)
            }
            Some(after) => {
                let min = n + after;
                if items.len() < min {
                    return Err(self.value_error(&format!(
                        "not enough values to unpack (expected at least {}, got {})",
                        min,
                        items.len()
                    )));
                }
                let mid_end = items.len() - after;
                let mut out: Vec<Value> = items[..n].to_vec();
                out.push(Value::list(items[n..mid_end].to_vec()));
                out.extend(items[mid_end..].iter().cloned());
                Ok(out)
            }
        }
    }

    fn iterate_unpack(&mut self, v: &Value, limit: Option<usize>) -> R<Vec<Value>> {
        match self.get_iter(v) {
            Ok(it) => {
                let mut out = Vec::new();
                while limit.is_none_or(|l| out.len() < l) {
                    match self.iter_next(&it)? {
                        Some(x) => out.push(x),
                        None => break,
                    }
                }
                Ok(out)
            }
            Err(e) => {
                if self.exc_is(&e, "TypeError") {
                    let t = self.type_name_of(v);
                    Err(self.type_error(&format!("cannot unpack non-iterable {} object", t)))
                } else {
                    Err(e)
                }
            }
        }
    }

    // ---- structural pattern matching -------------------------------------------------------

    pub fn match_is_sequence(&self, v: &Value) -> bool {
        match v {
            Value::Obj(o) => match &o.kind {
                Kind::List(_) | Kind::Tuple(_) | Kind::Range(_) => true,
                Kind::Str(_) | Kind::Bytes(_) | Kind::ByteArray(_) | Kind::Dict(_) => false,
                _ => {
                    let cls = self.type_of_obj(o);
                    self.lookup_mro(&cls, "__getitem__").is_some() && self.lookup_mro(&cls, "keys").is_none() && self.lookup_mro(&cls, "__len__").is_some()
                }
            },
            _ => false,
        }
    }

    pub fn match_is_mapping(&self, v: &Value) -> bool {
        match v {
            Value::Obj(o) => match &o.kind {
                Kind::Dict(_) => true,
                Kind::Instance => {
                    let cls = self.type_of_obj(o);
                    self.lookup_mro(&cls, "keys").is_some() && self.lookup_mro(&cls, "__getitem__").is_some()
                }
                _ => false,
            },
            _ => false,
        }
    }

    pub fn match_keys(&mut self, subj: &Value, keys: &[Value]) -> R<Value> {
        let mut out = Vec::new();
        for k in keys {
            let present = match subj {
                Value::Obj(o) if matches!(o.kind, Kind::Dict(_)) => self.dict_get(o, k)?,
                _ => {
                    let has = self.contains(subj, k)?;
                    if has {
                        Some(self.getitem(subj, k)?)
                    } else {
                        None
                    }
                }
            };
            match present {
                Some(v) => out.push(v),
                None => return Ok(Value::None),
            }
        }
        Ok(Value::tuple(out))
    }

    pub fn match_rest(&mut self, subj: &Value, keys: &[Value]) -> R<Value> {
        let d = Object::new(Kind::Dict(RefCell::new(PyDict::new())));
        let ks = self.call_method(subj, "keys", Vec::new())?;
        for k in self.iterate_to_vec(&ks)? {
            let mut skip = false;
            for used in keys {
                if self.values_eq(&k, used)? {
                    skip = true;
                    break;
                }
            }
            if !skip {
                let v = self.getitem(subj, &k)?;
                self.dict_set(&d, k, v)?;
            }
        }
        Ok(Value::Obj(d))
    }

    pub fn match_class(&mut self, subj: &Value, cls: &Value, npos: usize, nkw: usize, kwnames: &Value) -> R<Value> {
        let c = match cls {
            Value::Obj(c) if matches!(c.kind, Kind::Type(_)) => c.clone(),
            _ => return Err(self.type_error("called match pattern must be a class")),
        };
        if !self.isinstance_value(subj, cls)? {
            return Ok(Value::None);
        }
        let mut out = Vec::new();
        if npos > 0 {
            let builtin_self = matches!(
                self.type_name(&c).as_str(),
                "bool" | "int" | "float" | "str" | "bytes" | "bytearray" | "dict" | "list" | "tuple" | "set" | "frozenset"
            ) && !self.is_heap(&c);
            if builtin_self {
                if npos > 1 {
                    return Err(self.type_error("too many positional sub-patterns"));
                }
                out.push(subj.clone());
            } else {
                let ma = match self.lookup_mro(&c, "__match_args__") {
                    Some(m) => m,
                    None => {
                        let n = self.type_name(&c);
                        return Err(self.type_error(&format!("{}() accepts 0 positional sub-patterns ({} given)", n, npos)));
                    }
                };
                let names: Vec<Value> = ma.tuple_items().unwrap_or(&[]).to_vec();
                if names.len() < npos {
                    let n = self.type_name(&c);
                    return Err(self.type_error(&format!("{}() accepts {} positional sub-patterns ({} given)", n, names.len(), npos)));
                }
                for nm in names.iter().take(npos) {
                    let name = nm.as_str().unwrap_or("").to_string();
                    match self.get_attr_str(subj, &name) {
                        Ok(v) => out.push(v),
                        Err(e) => {
                            if self.exc_is(&e, "AttributeError") {
                                return Ok(Value::None);
                            }
                            return Err(e);
                        }
                    }
                }
            }
        }
        let _ = nkw;
        for nm in kwnames.tuple_items().unwrap_or(&[]).to_vec() {
            let name = nm.as_str().unwrap_or("").to_string();
            match self.get_attr_str(subj, &name) {
                Ok(v) => out.push(v),
                Err(e) => {
                    if self.exc_is(&e, "AttributeError") {
                        return Ok(Value::None);
                    }
                    return Err(e);
                }
            }
        }
        Ok(Value::tuple(out))
    }
}

pub fn big_range_len(r: &[crate::pyint::BigInt; 3]) -> crate::pyint::BigInt {
    use crate::pyint::BigInt;
    use std::cmp::Ordering;
    let one = BigInt::from_i64(1);
    let (lo, hi, step) = if r[2].is_negative() {
        (&r[1], &r[0], r[2].neg())
    } else {
        (&r[0], &r[1], r[2].clone())
    };
    if lo.cmp(hi) != Ordering::Less {
        return BigInt::zero();
    }
    hi.sub(lo).sub(&one).floor_div(&step).add(&one)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pyint::BigInt;

    fn r(a: i128, b: i128, c: i128) -> [BigInt; 3] {
        [BigInt::from_i128(a), BigInt::from_i128(b), BigInt::from_i128(c)]
    }

    #[test]
    fn big_range_len_counts_both_directions() {
        let n = |x: [BigInt; 3]| big_range_len(&x).to_string_radix(10);
        assert_eq!(n(r(0, 1 << 70, 1 << 60)), "1024");
        assert_eq!(n(r(10, 0, -3)), "4");
        assert_eq!(n(r(0, 10, -1)), "0");
        assert_eq!(n(r(5, 5, 1)), "0");
        assert_eq!(n(r(-(1 << 70), 1 << 70, 1 << 69)), "4");
    }
}
