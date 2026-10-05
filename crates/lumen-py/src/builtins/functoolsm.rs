//! `_functools`: `reduce`, `cmp_to_key`, `partial` and the `lru_cache` wrapper.

/// Tools that operate on functions.
#[lumen_bind::module(name = "_functools")]
pub mod _functools {
    #![allow(clippy::new_ret_no_self)]

    use crate::ast::CmpOp;
    use crate::bind::{opaque_instance, type_object, KwArgs, Py, This};
    use crate::object::*;
    use crate::vm::Interp;

    /// Apply a function of two arguments cumulatively to the items of a sequence
    /// or iterable, from left to right, so as to reduce the iterable to a single
    /// value.  For example, reduce(lambda x, y: x+y, [1, 2, 3, 4, 5]) calculates
    /// ((((1+2)+3)+4)+5).  If initial is present, it is placed before the items
    /// of the iterable in the calculation, and serves as a default when the
    /// iterable is empty.
    #[op(hint(py(
        text_signature = "($module, function, iterable, initial=<unrepresentable>, /)"
    )))]
    fn reduce(
        it: &mut Interp,
        function: &Value,
        iterable: &Value,
        #[varargs] initial: &[Value],
    ) -> R<Value> {
        if initial.len() > 1 {
            return Err(it.type_error(&format!(
                "reduce expected at most 3 arguments, got {}",
                initial.len() + 2
            )));
        }
        let iter = match it.get_iter(iterable) {
            Ok(i) => i,
            Err(e) if it.exc_is(&e, "TypeError") => {
                return Err(it.type_error("reduce() arg 2 must support iteration"));
            }
            Err(e) => return Err(e),
        };
        let mut acc = initial.first().cloned();
        while let Some(item) = it.iter_next(&iter)? {
            acc = Some(match acc {
                None => item,
                Some(a) => it.call(function, vec![a, item], Vec::new())?,
            });
        }
        acc.ok_or_else(|| it.type_error("reduce() of empty iterable with no initial value"))
    }

    /// Convert a cmp= function into a key= function.
    ///
    ///   mycmp
    ///     Function that compares two objects.
    #[op]
    fn cmp_to_key(it: &mut Interp, #[kw] mycmp: &Value) -> R<Value> {
        Ok(Py::new(
            it,
            KeyWrapper {
                cmp: mycmp.clone(),
                obj: None,
            },
        )
        .value()
        .clone())
    }

    #[class(name = "KeyWrapper", module = "functools", hint(py(final, unhashable)))]
    pub struct KeyWrapper {
        cmp: Value,
        obj: Option<Value>,
    }

    fn key_compare(it: &mut Interp, slf: &Py<KeyWrapper>, other: &Value, op: CmpOp) -> R<Value> {
        let Some(other) = Py::<KeyWrapper>::from_value(it, other) else {
            return Err(it.type_error("other argument must be K instance"));
        };
        let (cmp, a) = {
            let me = slf.borrow(it)?;
            (me.cmp.clone(), me.obj.clone())
        };
        let b = other.borrow(it)?.obj.clone();
        let (Some(a), Some(b)) = (a, b) else {
            return Err(it.new_exc_str("AttributeError", "object"));
        };
        let r = it.call(&cmp, vec![a, b], Vec::new())?;
        it.rich_compare(op, &r, &Value::Int(0))
    }

    #[methods]
    impl KeyWrapper {
        #[proto(call)]
        fn __call__(slf: This<Py<Self>>, it: &mut Interp, #[kw] obj: &Value) -> R<Value> {
            let cmp = slf.0.borrow(it)?.cmp.clone();
            Ok(Py::new(
                it,
                KeyWrapper {
                    cmp,
                    obj: Some(obj.clone()),
                },
            )
            .value()
            .clone())
        }

        /// Value wrapped by a key function.
        #[getter]
        fn obj(&self, it: &mut Interp) -> R<Value> {
            self.obj
                .clone()
                .ok_or_else(|| it.new_exc_str("AttributeError", "obj"))
        }

        #[setter(name = "obj")]
        fn set_obj(&mut self, value: &Value) {
            self.obj = Some(value.clone());
        }

        #[proto(lt)]
        fn __lt__(slf: This<Py<Self>>, it: &mut Interp, other: &Value) -> R<Value> {
            key_compare(it, &slf.0, other, CmpOp::Lt)
        }

        #[proto(le)]
        fn __le__(slf: This<Py<Self>>, it: &mut Interp, other: &Value) -> R<Value> {
            key_compare(it, &slf.0, other, CmpOp::LtE)
        }

        #[proto(gt)]
        fn __gt__(slf: This<Py<Self>>, it: &mut Interp, other: &Value) -> R<Value> {
            key_compare(it, &slf.0, other, CmpOp::Gt)
        }

        #[proto(ge)]
        fn __ge__(slf: This<Py<Self>>, it: &mut Interp, other: &Value) -> R<Value> {
            key_compare(it, &slf.0, other, CmpOp::GtE)
        }

        #[proto(eq)]
        fn __eq__(slf: This<Py<Self>>, it: &mut Interp, other: &Value) -> R<Value> {
            key_compare(it, &slf.0, other, CmpOp::Eq)
        }

        #[proto(ne)]
        fn __ne__(slf: This<Py<Self>>, it: &mut Interp, other: &Value) -> R<Value> {
            key_compare(it, &slf.0, other, CmpOp::NotEq)
        }
    }

    /// partial(func, *args, **keywords) - new function with partial application
    /// of the given arguments and keywords.
    #[class(name = "partial", module = "functools")]
    pub struct Partial {
        func: Value,
        args: Value,
        keywords: Value,
        phcount: usize,
    }

    /// The type of the Placeholder singleton.
    ///
    /// Used as a placeholder for partial arguments.
    #[class(name = "_PlaceholderType", module = "functools", hint(py(final)))]
    pub struct PlaceholderType {}

    #[methods]
    impl PlaceholderType {
        #[constructor]
        fn new(it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
            if !args.is_empty() || !kw.to_vec().is_empty() {
                return Err(it.type_error("PlaceholderType takes no arguments"));
            }
            let m = Value::Obj(it.import_module("_functools")?);
            it.get_attr_str(&m, "Placeholder")
        }

        #[proto(repr)]
        fn __repr__(&self) -> &'static str {
            "Placeholder"
        }

        #[proto(reduce, hint(py(text_signature = "")))]
        fn __reduce__(&self) -> &'static str {
            "Placeholder"
        }
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) -> R<()> {
        if let Value::Obj(m) = m {
            let ph = Py::new(it, PlaceholderType {}).value().clone();
            crate::vm::dict_set_str(&it.module_dict(m), "Placeholder", ph);
        }
        Ok(())
    }

    fn is_placeholder(it: &mut Interp, v: &Value) -> bool {
        Py::<PlaceholderType>::from_value(it, v).is_some()
    }

    fn count_placeholders(it: &mut Interp, args: &[Value]) -> usize {
        match args.split_last() {
            Some((_, init)) => init.iter().filter(|a| is_placeholder(it, a)).count(),
            None => 0,
        }
    }

    fn trailing_placeholder(it: &mut Interp, args: &[Value]) -> R<()> {
        if args.last().is_some_and(|a| is_placeholder(it, a)) {
            return Err(it.type_error("trailing Placeholders are not allowed"));
        }
        Ok(())
    }

    fn dict_len(d: &Obj) -> usize {
        crate::containers::pydict_of(d).map_or(0, |d| d.borrow().len())
    }

    fn new_dict(it: &mut Interp) -> Value {
        Value::Obj(it.new_dict())
    }

    fn partial_parts(it: &mut Interp, p: &Py<Partial>) -> R<(Value, Value, Value)> {
        let me = p.borrow(it)?;
        Ok((me.func.clone(), me.args.clone(), me.keywords.clone()))
    }

    #[methods]
    impl Partial {
        #[constructor]
        fn new(
            cls: This<Value>,
            it: &mut Interp,
            #[varargs] args: &[Value],
            #[varkw] kw: KwArgs,
        ) -> R<Value> {
            let Some((func, rest)) = args.split_first() else {
                return Err(it.type_error("type 'partial' takes at least one argument"));
            };
            let Value::Obj(cls) = cls.0 else {
                unreachable!()
            };
            let mut func = func.clone();
            if !it.is_callable(&func) {
                return Err(it.type_error("the first argument must be callable"));
            }
            trailing_placeholder(it, rest)?;
            let kw = kw.to_vec();
            if kw.iter().any(|(_, v)| is_placeholder(it, v)) {
                return Err(it.type_error("Placeholder cannot be passed as a keyword argument"));
            }
            let keywords = new_dict(it);
            let mut pto_args: Vec<Value> = Vec::new();
            let mut pto_phcount = 0;
            // A plain partial without instance attributes is flattened into the new one.
            if let Some(inner) = Py::<Partial>::from_value(it, &func) {
                let plain = matches!(&func, Value::Obj(o) if o.dict.borrow().as_ref().is_none_or(|d| dict_len(d) == 0));
                let partial_type = type_object::<Partial>(it);
                if plain && std::rc::Rc::ptr_eq(&it.type_of(&func), &partial_type) {
                    let (f, a, k) = partial_parts(it, &inner)?;
                    pto_phcount = inner.borrow(it)?.phcount;
                    pto_args = a.tuple_items().unwrap_or(&[]).to_vec();
                    it.dict_update_from(keywords.as_obj().unwrap(), &k)?;
                    func = f;
                }
            }
            let phcount = count_placeholders(it, rest);
            let (all_args, phcount) = if pto_phcount > 0 && !rest.is_empty() {
                let new_nargs = rest.len();
                let npargs = pto_args.len();
                let tot = npargs + new_nargs.saturating_sub(pto_phcount);
                let mut remaining = pto_phcount;
                let mut out = Vec::with_capacity(tot);
                let mut j = 0;
                for i in 0..tot {
                    if i < npargs {
                        let mut item = pto_args[i].clone();
                        if j < new_nargs && is_placeholder(it, &item) {
                            item = rest[j].clone();
                            j += 1;
                            remaining -= 1;
                        }
                        out.push(item);
                    } else {
                        out.push(rest[j].clone());
                        j += 1;
                    }
                }
                (out, remaining + phcount)
            } else {
                pto_args.extend(rest.iter().cloned());
                (pto_args, pto_phcount + phcount)
            };
            let kd = keywords.as_obj().unwrap().clone();
            for (k, v) in kw {
                it.dict_set(&kd, Value::Obj(k), v)?;
            }
            Ok(opaque_instance(
                &cls,
                Partial {
                    func,
                    args: Value::tuple(all_args),
                    keywords,
                    phcount,
                },
            ))
        }

        fn __get__(slf: This<Py<Self>>, obj: &Value, _cls: Option<&Value>) -> R<Value> {
            let me = slf.0.value().clone();
            if obj.is_none() {
                return Ok(me);
            }
            Ok(Value::Obj(Object::new(Kind::Method(me, obj.clone()))))
        }

        #[proto(call)]
        fn __call__(
            slf: This<Py<Self>>,
            it: &mut Interp,
            #[varargs] args: &[Value],
            #[varkw] kw: KwArgs,
        ) -> R<Value> {
            let (func, pargs, keywords) = partial_parts(it, &slf.0)?;
            let phcount = slf.0.borrow(it)?.phcount;
            if args.len() < phcount {
                return Err(it.type_error(&format!(
                    "missing positional arguments in 'partial' call; expected at least {phcount}, got {}",
                    args.len()
                )));
            }
            let mut all: Vec<Value> = pargs.tuple_items().unwrap_or(&[]).to_vec();
            let mut next = 0;
            if phcount > 0 {
                for slot in all.iter_mut() {
                    if next < phcount && is_placeholder(it, slot) {
                        *slot = args[next].clone();
                        next += 1;
                    }
                }
            }
            all.extend(args[next..].iter().cloned());
            let mut kwargs = it.dict_to_kwargs(&keywords)?;
            for (k, v) in kw.to_vec() {
                let name = k.as_str_kind().unwrap_or("");
                match kwargs
                    .iter_mut()
                    .find(|(e, _)| e.as_str_kind() == Some(name))
                {
                    Some(slot) => slot.1 = v,
                    None => kwargs.push((k, v)),
                }
            }
            it.call(&func, all, kwargs)
        }

        /// function object to use in future partial calls
        #[getter]
        fn func(&self) -> Value {
            self.func.clone()
        }

        /// tuple of arguments to future partial calls
        #[getter]
        fn args(&self) -> Value {
            self.args.clone()
        }

        /// dictionary of keyword arguments to future partial calls
        #[getter]
        fn keywords(&self) -> Value {
            self.keywords.clone()
        }

        #[proto(repr)]
        fn __repr__(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
            let me = slf.0.value().clone();
            let Some(me_obj) = me.as_obj().cloned() else {
                unreachable!()
            };
            if it.repr_enter(&me_obj) {
                return Ok("...".into());
            }
            let r = (|| -> R<String> {
                let (func, args, keywords) = partial_parts(it, &slf.0)?;
                let mut parts = vec![it.repr_of(&func)?];
                for a in args.tuple_items().unwrap_or(&[]).to_vec() {
                    parts.push(it.repr_of(&a)?);
                }
                let items: Vec<(Value, Value)> =
                    match crate::containers::pydict_of(keywords.as_obj().unwrap()) {
                        Some(d) => d
                            .borrow()
                            .iter()
                            .map(|e| (e.key.clone(), e.val.clone()))
                            .collect(),
                        None => Vec::new(),
                    };
                for (k, v) in items {
                    let ks = it.str_of(&k)?;
                    let vs = it.repr_of(&v)?;
                    parts.push(format!("{ks}={vs}"));
                }
                Ok(format!("{}({})", it.tp_name_of(&me), parts.join(", ")))
            })();
            it.repr_leave();
            r
        }

        #[proto(reduce, hint(py(text_signature = "")))]
        fn __reduce__(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            let (func, args, keywords) = partial_parts(it, &slf.0)?;
            let me = slf.0.value().clone();
            let cls = Value::Obj(it.type_of(&me));
            let dict = match me.as_obj().and_then(|o| o.dict.borrow().clone()) {
                Some(d) if dict_len(&d) > 0 => Value::Obj(d),
                _ => Value::None,
            };
            Ok(Value::tuple(vec![
                cls,
                Value::tuple(vec![func.clone()]),
                Value::tuple(vec![func, args, keywords, dict]),
            ]))
        }

        #[method(hint(py(text_signature = "")))]
        fn __setstate__(slf: This<Py<Self>>, it: &mut Interp, state: &Value) -> R<()> {
            let items = match state.tuple_items() {
                Some(t) if t.len() == 4 => t.to_vec(),
                _ => return Err(it.type_error("invalid partial state")),
            };
            let (func, args, kw, dict) = (&items[0], &items[1], &items[2], &items[3]);
            let args_ok = matches!(args, Value::Obj(o) if matches!(o.kind, Kind::Tuple(_)));
            let kw_ok = kw.is_none() || dict_of(kw).is_some();
            let dict_ok = dict.is_none() || dict_of(dict).is_some();
            if !it.is_callable(func) || !args_ok || !kw_ok || !dict_ok {
                return Err(it.type_error("invalid partial state"));
            }
            let arg_items = args.tuple_items().unwrap_or(&[]).to_vec();
            trailing_placeholder(it, &arg_items)?;
            let phcount = count_placeholders(it, &arg_items);
            let args = match args {
                Value::Obj(o) if o.cls.is_some() => {
                    Value::tuple(args.tuple_items().unwrap_or(&[]).to_vec())
                }
                _ => args.clone(),
            };
            let keywords = match kw {
                Value::Obj(o) if o.cls.is_none() && matches!(o.kind, Kind::Dict(_)) => kw.clone(),
                _ => {
                    let d = new_dict(it);
                    if !kw.is_none() {
                        it.dict_update_from(d.as_obj().unwrap(), kw)?;
                    }
                    d
                }
            };
            {
                let mut me = slf.0.borrow_mut(it)?;
                me.func = func.clone();
                me.args = args;
                me.keywords = keywords;
                me.phcount = phcount;
            }
            if let Some(o) = slf.0.value().as_obj() {
                *o.dict.borrow_mut() = match dict {
                    Value::Obj(d) => Some(d.clone()),
                    _ => None,
                };
            }
            Ok(())
        }

        /// See PEP 585
        #[classmethod(hint(py(text_signature = "")))]
        fn __class_getitem__(cls: This<Value>, it: &mut Interp, item: &Value) -> R<Value> {
            let types = Value::Obj(it.import_module("types")?);
            let alias = it.get_attr_str(&types, "GenericAlias")?;
            it.call(&alias, vec![cls.0, item.clone()], Vec::new())
        }
    }

    /// Create a cached callable that wraps another function.
    ///
    /// user_function:      the function being cached
    ///
    /// maxsize:  0         for no caching
    ///           None      for unlimited cache size
    ///           n         for a bounded cache
    ///
    /// typed:    False     cache f(3) and f(3.0) as identical calls
    ///           True      cache f(3) and f(3.0) as distinct calls
    ///
    /// cache_info_type:    namedtuple class with the fields:
    ///                         hits misses currsize maxsize
    #[class(name = "_lru_cache_wrapper", module = "functools")]
    pub struct LruCache {
        func: Value,
        maxsize: Option<usize>,
        typed: bool,
        cache_info_type: Value,
        cache: Obj,
        hits: u64,
        misses: u64,
        kwd_mark: Value,
    }

    fn make_key(
        it: &mut Interp,
        args: &[Value],
        kw: &[(Obj, Value)],
        typed: bool,
        kwd_mark: &Value,
    ) -> Value {
        if kw.is_empty() && !typed {
            if let [only] = args {
                let fast = match only {
                    Value::Int(_) => true,
                    Value::Obj(o) => {
                        o.cls.is_none() && matches!(o.kind, Kind::Str(_) | Kind::Int(_))
                    }
                    _ => false,
                };
                if fast {
                    return only.clone();
                }
            }
        }
        let mut key: Vec<Value> = args.to_vec();
        if !kw.is_empty() {
            key.push(kwd_mark.clone());
            for (k, v) in kw {
                key.push(Value::Obj(k.clone()));
                key.push(v.clone());
            }
        }
        if typed {
            for a in args {
                key.push(Value::Obj(it.type_of(a)));
            }
            for (_, v) in kw {
                key.push(Value::Obj(it.type_of(v)));
            }
        }
        Value::tuple(key)
    }

    #[methods]
    impl LruCache {
        #[constructor]
        fn new(
            cls: This<Value>,
            it: &mut Interp,
            #[kw] user_function: &Value,
            #[kw] maxsize: &Value,
            #[kw] typed: &Value,
            #[kw] cache_info_type: &Value,
        ) -> R<Value> {
            if !it.is_callable(user_function) {
                return Err(it.type_error("the first argument must be callable"));
            }
            let maxsize = if maxsize.is_none() {
                None
            } else if maxsize.as_bigint().is_some() {
                Some(it.index_of(maxsize)?.max(0) as usize)
            } else {
                return Err(it.type_error("maxsize should be integer or None"));
            };
            let typed = it.truthy(typed)?;
            let Value::Obj(cls) = cls.0 else {
                unreachable!()
            };
            let kwd_mark = Value::Obj(Object::new(Kind::Instance));
            let cache = it.new_dict();
            let state = LruCache {
                func: user_function.clone(),
                maxsize,
                typed,
                cache_info_type: cache_info_type.clone(),
                cache,
                hits: 0,
                misses: 0,
                kwd_mark,
            };
            Ok(opaque_instance(&cls, state))
        }

        #[proto(call)]
        fn __call__(
            slf: This<Py<Self>>,
            it: &mut Interp,
            #[varargs] args: &[Value],
            #[varkw] kw: KwArgs,
        ) -> R<Value> {
            let kw = kw.to_vec();
            let (func, maxsize, typed, cache, kwd_mark) = {
                let me = slf.0.borrow(it)?;
                (
                    me.func.clone(),
                    me.maxsize,
                    me.typed,
                    me.cache.clone(),
                    me.kwd_mark.clone(),
                )
            };
            if maxsize == Some(0) {
                slf.0.borrow_mut(it)?.misses += 1;
                return it.call(&func, args.to_vec(), kw);
            }
            let key = make_key(it, args, &kw, typed, &kwd_mark);
            // The key is hashed once; the cache entries are moved and inserted with that hash.
            let hash = it.hash_value(&key)?;
            let Some(store) = crate::containers::pydict_of(&cache) else {
                unreachable!()
            };
            if let Some(idx) = it.dict_find(&cache, hash, &key)? {
                let entry = if maxsize.is_some() {
                    store.borrow_mut().remove(idx)
                } else {
                    None
                };
                let value = match entry {
                    Some(e) => {
                        let v = e.val.clone();
                        store.borrow_mut().insert_new(e.hash, e.key, e.val);
                        Some(v)
                    }
                    None => store.borrow().get(idx).map(|e| e.val.clone()),
                };
                if let Some(v) = value {
                    slf.0.borrow_mut(it)?.hits += 1;
                    return Ok(v);
                }
            }
            slf.0.borrow_mut(it)?.misses += 1;
            let result = it.call(&func, args.to_vec(), kw)?;
            if it.dict_find(&cache, hash, &key)?.is_some() {
                // A recursive call already cached this key.
                return Ok(result);
            }
            if let Some(max) = maxsize {
                if store.borrow().len() >= max {
                    let oldest = store.borrow().next_live(0);
                    if let Some(i) = oldest {
                        store.borrow_mut().remove(i);
                    }
                }
            }
            store.borrow_mut().insert_new(hash, key, result.clone());
            Ok(result)
        }

        /// Report cache statistics
        fn cache_info(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            let (ty, hits, misses, maxsize, cache) = {
                let me = slf.0.borrow(it)?;
                (
                    me.cache_info_type.clone(),
                    me.hits,
                    me.misses,
                    me.maxsize,
                    me.cache.clone(),
                )
            };
            let size = crate::containers::pydict_of(&cache).map_or(0, |d| d.borrow().len());
            let maxsize = match maxsize {
                Some(m) => Value::Int(m as i64),
                None => Value::None,
            };
            it.call(
                &ty,
                vec![
                    Value::Int(hits as i64),
                    Value::Int(misses as i64),
                    maxsize,
                    Value::Int(size as i64),
                ],
                Vec::new(),
            )
        }

        /// Clear the cache and cache statistics
        fn cache_clear(slf: This<Py<Self>>, it: &mut Interp) -> R<()> {
            let cache = {
                let mut me = slf.0.borrow_mut(it)?;
                me.hits = 0;
                me.misses = 0;
                me.cache.clone()
            };
            if let Some(d) = crate::containers::pydict_of(&cache) {
                d.borrow_mut().clear();
            }
            Ok(())
        }

        fn __get__(slf: This<Py<Self>>, obj: &Value, _cls: Option<&Value>) -> R<Value> {
            let me = slf.0.value().clone();
            if obj.is_none() {
                return Ok(me);
            }
            Ok(Value::Obj(Object::new(Kind::Method(me, obj.clone()))))
        }

        #[proto(reduce, hint(py(text_signature = "")))]
        fn __reduce__(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            it.get_attr_str(slf.0.value(), "__qualname__")
        }

        #[proto(copy, hint(py(text_signature = "")))]
        fn __copy__(slf: This<Py<Self>>) -> Value {
            slf.0.value().clone()
        }

        #[proto(deepcopy, hint(py(text_signature = "")))]
        fn __deepcopy__(slf: This<Py<Self>>, _memo: &Value) -> Value {
            slf.0.value().clone()
        }
    }
}
