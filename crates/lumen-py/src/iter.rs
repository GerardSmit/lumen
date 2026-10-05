//! The iterator protocol and the builtin iterator state machines.

use crate::containers::pydict_of;
use crate::object::*;
use crate::vm::*;
use std::cell::RefCell;

impl Interp {
    pub fn mk_iter(&self, st: IterState) -> Value {
        Value::Obj(Object::new(Kind::Iter(RefCell::new(st))))
    }

    pub fn native_iter(&self, f: Box<dyn FnMut(&mut Interp) -> R<Option<Value>>>) -> Value {
        self.mk_iter(IterState::Native(f))
    }

    pub fn get_iter(&mut self, v: &Value) -> R<Value> {
        if let Value::Obj(o) = v {
            if o.cls.is_some() {
                if let Some(m) = self.user_special(v, "__iter__") {
                    if m.is_none() {
                        let t = self.type_name_of(v);
                        return Err(self.type_error(&format!("'{}' object is not iterable", t)));
                    }
                    let it = self.call_user_special(v, &m, Vec::new())?;
                    return self.check_iterator(it);
                }
                if let Kind::Instance | Kind::Exception(_) = &o.kind {
                    return self.get_iter_fallback(v);
                }
            }
        }
        self.native_get_iter(v)
    }

    pub fn native_get_iter(&mut self, v: &Value) -> R<Value> {
        if let Value::Obj(o) = v {
            match &o.kind {
                Kind::List(_) => {
                    return Ok(self.mk_iter(IterState::List {
                        list: o.clone(),
                        idx: 0,
                    }));
                }
                Kind::Tuple(_) => {
                    return Ok(self.mk_iter(IterState::Tuple {
                        tup: o.clone(),
                        idx: 0,
                    }));
                }
                Kind::Str(_) => {
                    return Ok(self.mk_iter(IterState::Str {
                        s: o.clone(),
                        pos: 0,
                    }));
                }
                Kind::Bytes(_) | Kind::ByteArray(_) => {
                    return Ok(self.mk_iter(IterState::Bytes {
                        b: o.clone(),
                        idx: 0,
                    }));
                }
                Kind::Range(r) => {
                    return Ok(self.mk_iter(IterState::Range {
                        cur: r.start,
                        stop: r.stop,
                        step: r.step,
                    }));
                }
                Kind::BigRange(_) => {
                    return Ok(self.mk_iter(IterState::Seq {
                        obj: v.clone(),
                        idx: 0,
                    }));
                }
                Kind::Dict(d) => {
                    let len = d.borrow().len();
                    return Ok(self.mk_iter(IterState::Dict {
                        dict: o.clone(),
                        pos: 0,
                        len,
                        kind: ViewKind::Keys,
                    }));
                }
                Kind::Set(d) | Kind::FrozenSet(d) => {
                    let len = d.borrow().len();
                    return Ok(self.mk_iter(IterState::Set {
                        set: o.clone(),
                        pos: 0,
                        len,
                    }));
                }
                Kind::DictView(d, vk) => {
                    let len = pydict_of(d).map(|p| p.borrow().len()).unwrap_or(0);
                    return Ok(self.mk_iter(IterState::Dict {
                        dict: d.clone(),
                        pos: 0,
                        len,
                        kind: *vk,
                    }));
                }
                Kind::Iter(_) => return Ok(v.clone()),
                Kind::Generator(g) => {
                    if g.kind == GenKind::Generator {
                        return Ok(v.clone());
                    }
                    let t = self.type_name_of(v);
                    return Err(self.type_error(&format!("'{}' object is not iterable", t)));
                }
                _ => {}
            }
        }
        self.get_iter_fallback(v)
    }

    fn get_iter_fallback(&mut self, v: &Value) -> R<Value> {
        let cls = self.type_of(v);
        if let Some(m) = self.lookup_mro(&cls, "__iter__") {
            if m.is_none() {
                let t = self.type_name_of(v);
                return Err(self.type_error(&format!("'{}' object is not iterable", t)));
            }
            let b = self.bind_descr(&m, v, &cls)?;
            let it = self.call(&b, Vec::new(), Vec::new())?;
            return self.check_iterator(it);
        }
        if self.lookup_mro(&cls, "__getitem__").is_some() && !v.is_type() {
            return Ok(self.mk_iter(IterState::Seq {
                obj: v.clone(),
                idx: 0,
            }));
        }
        let t = self.type_name_of(v);
        Err(self.type_error(&format!("'{}' object is not iterable", t)))
    }

    fn check_iterator(&mut self, it: Value) -> R<Value> {
        let cls = self.type_of(&it);
        if self.lookup_mro(&cls, "__next__").is_none() {
            let t = self.type_name_of(&it);
            return Err(self.type_error(&format!("iter() returned non-iterator of type '{}'", t)));
        }
        Ok(it)
    }

    /// The `strict=True` check of `zip` / `map` when iterator `n` of `its` is exhausted: a
    /// `ValueError` unless every iterator ends together.
    fn strict_exhausted(&mut self, name: &str, its: &[Value], n: usize) -> R<()> {
        let range = |m: usize| {
            if m > 1 {
                format!("s 1-{m}")
            } else {
                " 1".to_string()
            }
        };
        if n > 0 {
            let msg = format!(
                "{name}() argument {} is shorter than argument{}",
                n + 1,
                range(n)
            );
            return Err(self.value_error(&msg));
        }
        for (m, other) in its.iter().enumerate().skip(1) {
            if self.iter_next(other)?.is_some() {
                let msg = format!(
                    "{name}() argument {} is longer than argument{}",
                    m + 1,
                    range(m)
                );
                return Err(self.value_error(&msg));
            }
        }
        Ok(())
    }

    /// Next item, or `None` when exhausted (StopIteration is swallowed).
    pub fn iter_next(&mut self, it: &Value) -> R<Option<Value>> {
        self.poll()?;
        self.iter_step(it)
    }

    /// [`Interp::iter_next`] without the interrupt poll, for the `for` loop whose back-edge polls.
    #[inline]
    pub(crate) fn iter_step(&mut self, it: &Value) -> R<Option<Value>> {
        let o = match it {
            Value::Obj(o) => o,
            _ => return Err(self.not_iterator(it)),
        };
        match &o.kind {
            Kind::Iter(st) => {
                if o.cls.is_some() {
                    if let Some(m) = self.user_special(it, "__next__") {
                        return self.user_next(it, &m);
                    }
                }
                self.step_iter(st)
            }
            Kind::Generator(g) => {
                if g.kind != GenKind::Generator && o.cls.is_none() {
                    return Err(self.not_iterator(it));
                }
                match self.gen_send(o, Value::None)? {
                    GenResult::Yield(v) => Ok(Some(v)),
                    GenResult::Return(v) => {
                        self.ret_val = v;
                        Ok(None)
                    }
                }
            }
            _ => {
                if o.cls.is_some() {
                    if let Some(m) = self.user_special(it, "__next__") {
                        return self.user_next(it, &m);
                    }
                }
                let cls = self.type_of_obj(o);
                match self.lookup_mro(&cls, "__next__") {
                    Some(m) => {
                        let b = self.bind_descr(&m, it, &cls)?;
                        {
                            let r = self.call(&b, Vec::new(), Vec::new());
                            self.stop_to_none(r)
                        }
                    }
                    None => Err(self.not_iterator(it)),
                }
            }
        }
    }

    pub fn native_iter_next(&mut self, it: &Value) -> R<Option<Value>> {
        if let Value::Obj(o) = it {
            match &o.kind {
                Kind::Iter(st) => return self.step_iter(st),
                Kind::Generator(_) => match self.gen_send(o, Value::None)? {
                    GenResult::Yield(v) => return Ok(Some(v)),
                    GenResult::Return(_) => return Ok(None),
                },
                _ => {}
            }
        }
        Err(self.not_iterator(it))
    }

    fn user_next(&mut self, it: &Value, m: &Value) -> R<Option<Value>> {
        let r = self.call_user_special(it, m, Vec::new());
        self.stop_to_none(r)
    }

    fn stop_to_none(&mut self, r: R<Value>) -> R<Option<Value>> {
        match r {
            Ok(v) => Ok(Some(v)),
            Err(e) => {
                if self.exc_is(&e, "StopIteration") {
                    Ok(None)
                } else {
                    Err(e)
                }
            }
        }
    }

    fn not_iterator(&mut self, it: &Value) -> Obj {
        let t = self.type_name_of(it);
        self.type_error(&format!("'{}' object is not an iterator", t))
    }

    fn step_iter(&mut self, st: &RefCell<IterState>) -> R<Option<Value>> {
        let mut s = st.borrow_mut();
        match &mut *s {
            IterState::List { list, idx } => {
                if let Kind::List(l) = &list.kind {
                    if let Some(v) = l.borrow().get(*idx) {
                        *idx += 1;
                        return Ok(Some(v.clone()));
                    }
                }
                *s = IterState::Empty;
                Ok(None)
            }
            IterState::Tuple { tup, idx } => {
                if let Kind::Tuple(t) = &tup.kind {
                    if let Some(v) = t.get(*idx) {
                        *idx += 1;
                        return Ok(Some(v.clone()));
                    }
                }
                *s = IterState::Empty;
                Ok(None)
            }
            IterState::Str { s: so, pos } => {
                if let Kind::Str(ps) = &so.kind {
                    if *pos < ps.s.len() {
                        let (_, n) = lumen_common::smuggle::decode_at(&ps.s, *pos);
                        let c = &ps.s[*pos..*pos + n];
                        *pos += n;
                        return Ok(Some(Value::str(c)));
                    }
                }
                *s = IterState::Empty;
                Ok(None)
            }
            IterState::Bytes { b, idx } => {
                let v = match &b.kind {
                    Kind::Bytes(x) => x.get(*idx).copied(),
                    Kind::ByteArray(x) => x.bytes().get(*idx).copied(),
                    _ => None,
                };
                match v {
                    Some(x) => {
                        *idx += 1;
                        Ok(Some(Value::Int(x as i64)))
                    }
                    None => {
                        *s = IterState::Empty;
                        Ok(None)
                    }
                }
            }
            IterState::Range { cur, stop, step } => {
                let more = if *step > 0 {
                    *cur < *stop
                } else {
                    *cur > *stop
                };
                if more {
                    let v = *cur;
                    match cur.checked_add(*step) {
                        Some(n) => *cur = n,
                        None => *cur = *stop,
                    }
                    Ok(Some(Value::Int(v)))
                } else {
                    Ok(None)
                }
            }
            IterState::Dict {
                dict,
                pos,
                len,
                kind,
            } => {
                let pd = match pydict_of(dict) {
                    Some(p) => p,
                    None => return Ok(None),
                };
                let pdb = pd.borrow();
                if pdb.len() != *len {
                    drop(pdb);
                    *len = usize::MAX;
                    drop(s);
                    return Err(self
                        .new_exc_str("RuntimeError", "dictionary changed size during iteration"));
                }
                match pdb.next_live(*pos) {
                    Some(i) => {
                        *pos = i + 1;
                        let e = pdb.get(i).unwrap();
                        Ok(Some(match kind {
                            ViewKind::Keys => e.key.clone(),
                            ViewKind::Values => e.val.clone(),
                            ViewKind::Items => Value::tuple(vec![e.key.clone(), e.val.clone()]),
                        }))
                    }
                    None => {
                        drop(pdb);
                        *s = IterState::Empty;
                        Ok(None)
                    }
                }
            }
            IterState::Set { set, pos, len } => {
                let pd = match pydict_of(set) {
                    Some(p) => p,
                    None => return Ok(None),
                };
                let pdb = pd.borrow();
                if pdb.len() != *len {
                    drop(pdb);
                    drop(s);
                    return Err(
                        self.new_exc_str("RuntimeError", "Set changed size during iteration")
                    );
                }
                match pdb.next_live(*pos) {
                    Some(i) => {
                        *pos = i + 1;
                        Ok(Some(pdb.get(i).unwrap().key.clone()))
                    }
                    None => {
                        drop(pdb);
                        *s = IterState::Empty;
                        Ok(None)
                    }
                }
            }
            IterState::Seq { obj, idx } => {
                if *idx == i64::MIN {
                    return Ok(None);
                }
                let (obj, i) = (obj.clone(), *idx);
                *idx += 1;
                drop(s);
                match self.getitem(&obj, &Value::Int(i)) {
                    Ok(v) => Ok(Some(v)),
                    Err(e) => {
                        if self.exc_is(&e, "IndexError") || self.exc_is(&e, "StopIteration") {
                            if let IterState::Seq { idx, .. } = &mut *st.borrow_mut() {
                                *idx = i64::MIN;
                            }
                            Ok(None)
                        } else {
                            Err(e)
                        }
                    }
                }
            }
            IterState::CallIter { f, sentinel, done } => {
                if *done {
                    return Ok(None);
                }
                let (f, sentinel) = (f.clone(), sentinel.clone());
                drop(s);
                let v = match self.call(&f, Vec::new(), Vec::new()) {
                    Ok(v) => v,
                    Err(e) => {
                        if self.exc_is(&e, "StopIteration") {
                            self.mark_calliter_done(st);
                            return Ok(None);
                        }
                        return Err(e);
                    }
                };
                if self.values_eq(&v, &sentinel)? {
                    self.mark_calliter_done(st);
                    return Ok(None);
                }
                Ok(Some(v))
            }
            IterState::Reversed { seq, idx } => {
                if *idx < 0 {
                    return Ok(None);
                }
                let (seq, i) = (seq.clone(), *idx);
                *idx -= 1;
                drop(s);
                match self.getitem(&seq, &Value::Int(i)) {
                    Ok(v) => Ok(Some(v)),
                    Err(e) => {
                        if self.exc_is(&e, "IndexError") {
                            *st.borrow_mut() = IterState::Empty;
                            Ok(None)
                        } else {
                            Err(e)
                        }
                    }
                }
            }
            IterState::Enumerate { it, idx } => {
                let (it, i) = (it.clone(), *idx);
                drop(s);
                match self.iter_next(&it)? {
                    Some(v) => {
                        if let IterState::Enumerate { idx, .. } = &mut *st.borrow_mut() {
                            *idx += 1;
                        }
                        Ok(Some(Value::tuple(vec![Value::Int(i), v])))
                    }
                    None => Ok(None),
                }
            }
            IterState::Zip { its, strict } => {
                if its.is_empty() {
                    return Ok(None);
                }
                let (its, strict) = (its.clone(), *strict);
                drop(s);
                let mut out = Vec::with_capacity(its.len());
                for (n, it) in its.iter().enumerate() {
                    match self.iter_next(it)? {
                        Some(v) => out.push(v),
                        None => {
                            if strict {
                                self.strict_exhausted("zip", &its, n)?;
                            }
                            return Ok(None);
                        }
                    }
                }
                Ok(Some(Value::tuple(out)))
            }
            IterState::Map { f, its, strict } => {
                let (f, its, strict) = (f.clone(), its.clone(), *strict);
                drop(s);
                let mut args = Vec::with_capacity(its.len());
                for (n, it) in its.iter().enumerate() {
                    match self.iter_next(it)? {
                        Some(v) => args.push(v),
                        None => {
                            if strict {
                                self.strict_exhausted("map", &its, n)?;
                            }
                            return Ok(None);
                        }
                    }
                }
                Ok(Some(self.call(&f, args, Vec::new())?))
            }
            IterState::Filter { f, it } => {
                let (f, it) = (f.clone(), it.clone());
                drop(s);
                loop {
                    let v = match self.iter_next(&it)? {
                        Some(v) => v,
                        None => return Ok(None),
                    };
                    let keep = if f.is_none() {
                        self.truthy(&v)?
                    } else {
                        let r = self.call(&f, vec![v.clone()], Vec::new())?;
                        self.truthy(&r)?
                    };
                    if keep {
                        return Ok(Some(v));
                    }
                }
            }
            IterState::Native(_) => {
                let mut taken = std::mem::replace(&mut *s, IterState::Running);
                drop(s);
                let r = match &mut taken {
                    IterState::Native(f) => f(self),
                    _ => Ok(None),
                };
                let exhausted = matches!(r, Ok(None) | Err(_));
                *st.borrow_mut() = if exhausted { IterState::Empty } else { taken };
                r
            }
            // Re-entered from code its own step runs: CPython's itertools would step the same
            // source again, which is the generator already running.
            IterState::Running => {
                drop(s);
                Err(self.value_error("generator already executing"))
            }
            IterState::Empty => Ok(None),
        }
    }

    fn mark_calliter_done(&mut self, st: &RefCell<IterState>) {
        if let IterState::CallIter { done, .. } = &mut *st.borrow_mut() {
            *done = true;
        }
    }

    pub fn iterate_to_vec(&mut self, v: &Value) -> R<Vec<Value>> {
        if let Value::Obj(o) = v {
            if o.cls.is_none() || self.user_special(v, "__iter__").is_none() {
                match &o.kind {
                    Kind::List(l) => return Ok(l.borrow().clone()),
                    Kind::Tuple(t) => return Ok(t.clone()),
                    Kind::Dict(d) => return Ok(d.borrow().keys()),
                    Kind::Set(d) | Kind::FrozenSet(d) => return Ok(d.borrow().keys()),
                    Kind::Str(s) => {
                        let mut out = Vec::with_capacity(s.nchars);
                        let mut i = 0;
                        while i < s.s.len() {
                            let (_, n) = lumen_common::smuggle::decode_at(&s.s, i);
                            out.push(Value::str(&s.s[i..i + n]));
                            i += n;
                        }
                        return Ok(out);
                    }
                    _ => {}
                }
            }
        }
        let hint = match v {
            Value::Obj(o) => match &o.kind {
                Kind::Range(r) => crate::ops::slice_len(r.start, r.stop, r.step),
                Kind::BigRange(r) => match crate::ops::big_range_len(r).to_i64() {
                    Some(n) => n as usize,
                    None => {
                        return Err(
                            self.overflow_err("Python int too large to convert to C ssize_t")
                        );
                    }
                },
                _ => 0,
            },
            _ => 0,
        };
        let mut out = self.vec_with_capacity(hint, crate::limits::MAX_SEQ_LEN)?;
        let it = self.get_iter(v)?;
        while let Some(x) = self.iter_next(&it)? {
            if out.len() >= crate::limits::MAX_SEQ_LEN {
                return Err(self.memory_error());
            }
            out.push(x);
        }
        Ok(out)
    }
}

impl Value {
    pub fn as_bytes_empty(&self) -> bool {
        matches!(self, Value::Obj(o) if matches!(&o.kind, Kind::Bytes(b) if b.is_empty()))
    }
}
