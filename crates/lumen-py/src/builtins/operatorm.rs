//! `_operator`: the intrinsic operators as functions, and `attrgetter`, `itemgetter` and
//! `methodcaller`.

/// Operator interface.
///
/// This module exports a set of functions implemented in C corresponding
/// to the intrinsic operators of Python.  For example, operator.add(x, y)
/// is equivalent to the expression x+y.  The function names are those
/// used for special methods; variants without leading and trailing
/// '__' are also provided for convenience.
#[lumen_bind::module(name = "_operator")]
pub mod _operator {
    #![allow(clippy::new_ret_no_self)]

    use crate::ast::{BinOp, CmpOp};
    use crate::bind::{opaque_instance, KwArgs, Py, This};
    use crate::bytecode::UnOp;
    use crate::object::*;
    use crate::vm::Interp;

    fn compare(it: &mut Interp, op: CmpOp, a: &Value, b: &Value) -> R<Value> {
        it.rich_compare(op, a, b)
    }

    /// Same as a < b.
    #[op]
    fn lt(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        compare(it, CmpOp::Lt, a, b)
    }

    /// Same as a <= b.
    #[op]
    fn le(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        compare(it, CmpOp::LtE, a, b)
    }

    /// Same as a == b.
    #[op]
    fn eq(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        compare(it, CmpOp::Eq, a, b)
    }

    /// Same as a != b.
    #[op]
    fn ne(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        compare(it, CmpOp::NotEq, a, b)
    }

    /// Same as a > b.
    #[op]
    fn gt(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        compare(it, CmpOp::Gt, a, b)
    }

    /// Same as a >= b.
    #[op]
    fn ge(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        compare(it, CmpOp::GtE, a, b)
    }

    /// Same as not a.
    #[op]
    fn not_(it: &mut Interp, a: &Value) -> R<bool> {
        Ok(!it.truthy(a)?)
    }

    /// Return True if a is true, False otherwise.
    #[op]
    fn truth(it: &mut Interp, a: &Value) -> R<bool> {
        it.truthy(a)
    }

    /// Same as a is b.
    #[op]
    fn is_(a: &Value, b: &Value) -> bool {
        a.is(b)
    }

    /// Same as a is not b.
    #[op]
    fn is_not(a: &Value, b: &Value) -> bool {
        !a.is(b)
    }

    /// Same as abs(a).
    #[op]
    fn abs(it: &mut Interp, a: &Value) -> R<Value> {
        let f = it.builtins_fn("abs");
        it.call(&f, vec![a.clone()], Vec::new())
    }

    /// Same as a.__index__()
    #[op]
    fn index(it: &mut Interp, a: &Value) -> R<Value> {
        crate::bind::index(it, a)
    }

    /// Same as -a.
    #[op]
    fn neg(it: &mut Interp, a: &Value) -> R<Value> {
        it.unary_op(UnOp::Neg, a)
    }

    /// Same as +a.
    #[op]
    fn pos(it: &mut Interp, a: &Value) -> R<Value> {
        it.unary_op(UnOp::Pos, a)
    }

    /// Same as ~a.
    #[op]
    fn inv(it: &mut Interp, a: &Value) -> R<Value> {
        it.unary_op(UnOp::Invert, a)
    }

    /// Same as ~a.
    #[op]
    fn invert(it: &mut Interp, a: &Value) -> R<Value> {
        it.unary_op(UnOp::Invert, a)
    }

    /// Same as a + b.
    #[op]
    fn add(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        it.binary_op(BinOp::Add, a, b)
    }

    /// Same as a += b.
    #[op]
    fn iadd(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        it.inplace_op(BinOp::Add, a.clone(), b)
    }

    /// Same as a - b.
    #[op]
    fn sub(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        it.binary_op(BinOp::Sub, a, b)
    }

    /// Same as a -= b.
    #[op]
    fn isub(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        it.inplace_op(BinOp::Sub, a.clone(), b)
    }

    /// Same as a * b.
    #[op]
    fn mul(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        it.binary_op(BinOp::Mult, a, b)
    }

    /// Same as a *= b.
    #[op]
    fn imul(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        it.inplace_op(BinOp::Mult, a.clone(), b)
    }

    /// Same as a @ b.
    #[op]
    fn matmul(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        it.binary_op(BinOp::MatMult, a, b)
    }

    /// Same as a @= b.
    #[op]
    fn imatmul(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        it.inplace_op(BinOp::MatMult, a.clone(), b)
    }

    /// Same as a / b.
    #[op]
    fn truediv(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        it.binary_op(BinOp::Div, a, b)
    }

    /// Same as a /= b.
    #[op]
    fn itruediv(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        it.inplace_op(BinOp::Div, a.clone(), b)
    }

    /// Same as a // b.
    #[op]
    fn floordiv(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        it.binary_op(BinOp::FloorDiv, a, b)
    }

    /// Same as a //= b.
    #[op]
    fn ifloordiv(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        it.inplace_op(BinOp::FloorDiv, a.clone(), b)
    }

    /// Same as a % b.
    #[op(name = "mod")]
    fn mod_(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        it.binary_op(BinOp::Mod, a, b)
    }

    /// Same as a %= b.
    #[op]
    fn imod(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        it.inplace_op(BinOp::Mod, a.clone(), b)
    }

    /// Same as a ** b.
    #[op]
    fn pow(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        it.binary_op(BinOp::Pow, a, b)
    }

    /// Same as a **= b.
    #[op]
    fn ipow(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        it.inplace_op(BinOp::Pow, a.clone(), b)
    }

    /// Same as a << b.
    #[op]
    fn lshift(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        it.binary_op(BinOp::LShift, a, b)
    }

    /// Same as a <<= b.
    #[op]
    fn ilshift(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        it.inplace_op(BinOp::LShift, a.clone(), b)
    }

    /// Same as a >> b.
    #[op]
    fn rshift(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        it.binary_op(BinOp::RShift, a, b)
    }

    /// Same as a >>= b.
    #[op]
    fn irshift(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        it.inplace_op(BinOp::RShift, a.clone(), b)
    }

    /// Same as a & b.
    #[op]
    fn and_(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        it.binary_op(BinOp::BitAnd, a, b)
    }

    /// Same as a &= b.
    #[op]
    fn iand(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        it.inplace_op(BinOp::BitAnd, a.clone(), b)
    }

    /// Same as a | b.
    #[op]
    fn or_(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        it.binary_op(BinOp::BitOr, a, b)
    }

    /// Same as a |= b.
    #[op]
    fn ior(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        it.inplace_op(BinOp::BitOr, a.clone(), b)
    }

    /// Same as a ^ b.
    #[op]
    fn xor(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        it.binary_op(BinOp::BitXor, a, b)
    }

    /// Same as a ^= b.
    #[op]
    fn ixor(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        it.inplace_op(BinOp::BitXor, a.clone(), b)
    }

    /// `PySequence_Check`: `__getitem__` on something that is not a dict.
    fn is_sequence(it: &mut Interp, v: &Value) -> bool {
        if let Value::Obj(o) = v {
            if matches!(o.kind, Kind::Dict(_)) {
                return false;
            }
        }
        let cls = it.type_of(v);
        it.lookup_mro(&cls, "__getitem__").is_some()
    }

    fn concat_with(it: &mut Interp, a: &Value, b: &Value, inplace: bool) -> R<Value> {
        if !is_sequence(it, a) {
            let t = it.type_name_of(a);
            return Err(it.type_error(&format!("'{t}' object can't be concatenated")));
        }
        if inplace {
            it.inplace_op(BinOp::Add, a.clone(), b)
        } else {
            it.binary_op(BinOp::Add, a, b)
        }
    }

    /// Same as a + b, for a and b sequences.
    #[op]
    fn concat(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        concat_with(it, a, b, false)
    }

    /// Same as a += b, for a and b sequences.
    #[op]
    fn iconcat(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        concat_with(it, a, b, true)
    }

    /// Same as b in a (note reversed operands).
    #[op]
    fn contains(it: &mut Interp, a: &Value, b: &Value) -> R<bool> {
        it.contains(a, b)
    }

    fn matches(it: &mut Interp, item: &Value, b: &Value) -> R<bool> {
        if item.is(b) {
            return Ok(true);
        }
        let r = it.rich_compare(CmpOp::Eq, item, b)?;
        it.truthy(&r)
    }

    /// Return the number of items in a which are, or which equal, b.
    #[op(name = "countOf")]
    fn count_of(it: &mut Interp, a: &Value, b: &Value) -> R<i64> {
        let iter = it.get_iter(a)?;
        let mut n = 0;
        while let Some(item) = it.iter_next(&iter)? {
            if matches(it, &item, b)? {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Return the first index of b in a.
    #[op(name = "indexOf")]
    fn index_of(it: &mut Interp, a: &Value, b: &Value) -> R<i64> {
        let iter = it.get_iter(a)?;
        let mut i = 0;
        while let Some(item) = it.iter_next(&iter)? {
            if matches(it, &item, b)? {
                return Ok(i);
            }
            i += 1;
        }
        Err(it.value_error("sequence.index(x): x not in sequence"))
    }

    /// Same as a[b].
    #[op]
    fn getitem(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        it.getitem(a, b)
    }

    /// Same as a[b] = c.
    #[op]
    fn setitem(it: &mut Interp, a: &Value, b: &Value, c: &Value) -> R<()> {
        it.setitem(a, b.clone(), c.clone())
    }

    /// Same as del a[b].
    #[op]
    fn delitem(it: &mut Interp, a: &Value, b: &Value) -> R<()> {
        it.delitem(a, b)
    }

    /// Return an estimate of the number of items in obj.
    ///
    /// This is useful for presizing containers when building from an iterable.
    ///
    /// If the object supports len(), the result will be exact.
    /// Otherwise, it may over- or under-estimate by an arbitrary amount.
    /// The result will be an integer >= 0.
    #[op]
    fn length_hint(it: &mut Interp, obj: &Value, #[default(Value::Int(0))] default: Value) -> R<Value> {
        if default.as_bigint().is_none() {
            let t = it.type_name_of(&default);
            return Err(it.type_error(&format!("'{t}' object cannot be interpreted as an integer")));
        }
        let cls = it.type_of(obj);
        if it.lookup_mro(&cls, "__len__").is_some() {
            match it.len_of(obj) {
                Ok(n) => return Ok(Value::Int(n as i64)),
                Err(e) if it.exc_is(&e, "TypeError") => {}
                Err(e) => return Err(e),
            }
        }
        let Some(hint) = it.lookup_mro(&cls, "__length_hint__") else { return Ok(default) };
        let bound = it.bind_descr(&hint, obj, &cls)?;
        let r = match it.call(&bound, Vec::new(), Vec::new()) {
            Ok(r) => r,
            Err(e) if it.exc_is(&e, "TypeError") => return Ok(default),
            Err(e) => return Err(e),
        };
        if matches!(r, Value::NotImplemented) {
            return Ok(default);
        }
        let Some(n) = r.as_bigint() else {
            let t = it.type_name_of(&r);
            return Err(it.type_error(&format!("__length_hint__ must be an integer, not {t}")));
        };
        if n.is_negative() {
            return Err(it.value_error("__length_hint__() should return >= 0"));
        }
        Ok(r)
    }

    /// Same as obj(*args, **kwargs).
    #[op]
    fn call(it: &mut Interp, obj: &Value, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        it.call(obj, args.to_vec(), kw.to_vec())
    }

    /// Return 'a == b'.
    ///
    /// This function uses an approach designed to prevent
    /// timing analysis, making it appropriate for cryptography.
    ///
    /// a and b must both be of the same type: either str (ASCII only),
    /// or any bytes-like object.
    ///
    /// Note: If a and b are of different lengths, or if an error occurs,
    /// a timing attack could theoretically reveal information about the
    /// types and lengths of a and b--but not their values.
    #[op]
    fn _compare_digest(it: &mut Interp, a: &Value, b: &Value) -> R<bool> {
        super::super::hashlibm::compare_digest(it, a, b)
    }

    fn reprs(it: &mut Interp, items: &[Value]) -> R<Vec<String>> {
        items.iter().map(|v| it.repr_of(v)).collect()
    }

    /// Return a callable object that fetches the given attribute(s) from its operand.
    /// After f = attrgetter('name'), the call f(r) returns r.name.
    /// After g = attrgetter('name', 'date'), the call g(r) returns (r.name, r.date).
    /// After h = attrgetter('name.first', 'name.last'), the call h(r) returns
    /// (r.name.first, r.name.last).
    #[class(name = "attrgetter", module = "operator")]
    pub struct AttrGetter {
        attrs: Vec<Value>,
        paths: Vec<Vec<Value>>,
    }

    #[methods]
    impl AttrGetter {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
            if !kw.is_empty() {
                return Err(it.type_error("attrgetter() takes no keyword arguments"));
            }
            if args.is_empty() {
                return Err(it.type_error("attrgetter expected 1 argument, got 0"));
            }
            let mut paths = Vec::new();
            for a in args {
                let Some(s) = a.as_str() else { return Err(it.type_error("attribute name must be a string")) };
                paths.push(s.split('.').map(|p| Value::str(p)).collect());
            }
            let Value::Obj(cls) = cls.0 else { unreachable!() };
            Ok(opaque_instance(&cls, AttrGetter { attrs: args.to_vec(), paths }))
        }

        #[proto(call)]
        fn __call__(slf: This<Py<Self>>, it: &mut Interp, obj: &Value) -> R<Value> {
            let paths = slf.0.borrow(it)?.paths.clone();
            let mut out = Vec::with_capacity(paths.len());
            for path in &paths {
                let mut v = obj.clone();
                for name in path {
                    let Value::Obj(n) = name else { unreachable!() };
                    v = it.get_attr(&v, n)?;
                }
                out.push(v);
            }
            Ok(if out.len() == 1 { out.pop().unwrap() } else { Value::tuple(out) })
        }

        #[proto(repr)]
        fn __repr__(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
            let attrs = slf.0.borrow(it)?.attrs.clone();
            Ok(format!("operator.attrgetter({})", reprs(it, &attrs)?.join(", ")))
        }

        #[proto(reduce)]
        fn __reduce__(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            let attrs = slf.0.borrow(it)?.attrs.clone();
            let cls = Value::Obj(it.type_of(slf.0.value()));
            Ok(Value::tuple(vec![cls, Value::tuple(attrs)]))
        }
    }

    /// Return a callable object that fetches the given item(s) from its operand.
    /// After f = itemgetter(2), the call f(r) returns r[2].
    /// After g = itemgetter(2, 5, 3), the call g(r) returns (r[2], r[5], r[3])
    #[class(name = "itemgetter", module = "operator")]
    pub struct ItemGetter {
        items: Vec<Value>,
    }

    #[methods]
    impl ItemGetter {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
            if !kw.is_empty() {
                return Err(it.type_error("itemgetter() takes no keyword arguments"));
            }
            if args.is_empty() {
                return Err(it.type_error("itemgetter expected 1 argument, got 0"));
            }
            let Value::Obj(cls) = cls.0 else { unreachable!() };
            Ok(opaque_instance(&cls, ItemGetter { items: args.to_vec() }))
        }

        #[proto(call)]
        fn __call__(slf: This<Py<Self>>, it: &mut Interp, obj: &Value) -> R<Value> {
            let items = slf.0.borrow(it)?.items.clone();
            if let [item] = items.as_slice() {
                return it.getitem(obj, item);
            }
            let mut out = Vec::with_capacity(items.len());
            for item in &items {
                out.push(it.getitem(obj, item)?);
            }
            Ok(Value::tuple(out))
        }

        #[proto(repr)]
        fn __repr__(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
            let items = slf.0.borrow(it)?.items.clone();
            Ok(format!("operator.itemgetter({})", reprs(it, &items)?.join(", ")))
        }

        #[proto(reduce)]
        fn __reduce__(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            let items = slf.0.borrow(it)?.items.clone();
            let cls = Value::Obj(it.type_of(slf.0.value()));
            Ok(Value::tuple(vec![cls, Value::tuple(items)]))
        }
    }

    /// Return a callable object that calls the given method on its operand.
    /// After f = methodcaller('name'), the call f(r) returns r.name().
    /// After g = methodcaller('name', 'date', foo=1), the call g(r) returns
    /// r.name('date', foo=1).
    #[class(name = "methodcaller", module = "operator")]
    pub struct MethodCaller {
        name: Value,
        args: Vec<Value>,
        kwargs: Vec<(Obj, Value)>,
    }

    #[methods]
    impl MethodCaller {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
            let Some((name, rest)) = args.split_first() else {
                return Err(it.type_error("methodcaller needs at least one argument, the method name"));
            };
            let Some(s) = name.as_str() else { return Err(it.type_error("method name must be a string")) };
            let name = Value::str(s);
            let Value::Obj(cls) = cls.0 else { unreachable!() };
            Ok(opaque_instance(&cls, MethodCaller { name, args: rest.to_vec(), kwargs: kw.to_vec() }))
        }

        #[proto(call)]
        fn __call__(slf: This<Py<Self>>, it: &mut Interp, obj: &Value) -> R<Value> {
            let (name, args, kwargs) = {
                let me = slf.0.borrow(it)?;
                (me.name.clone(), me.args.clone(), me.kwargs.clone())
            };
            let Value::Obj(n) = &name else { unreachable!() };
            let method = it.get_attr(obj, n)?;
            it.call(&method, args, kwargs)
        }

        #[proto(repr)]
        fn __repr__(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
            let (name, args, kwargs) = {
                let me = slf.0.borrow(it)?;
                (me.name.clone(), me.args.clone(), me.kwargs.clone())
            };
            let mut parts = vec![it.repr_of(&name)?];
            parts.extend(reprs(it, &args)?);
            for (k, v) in &kwargs {
                let r = it.repr_of(v)?;
                parts.push(format!("{}={r}", k.as_str_kind().unwrap_or("")));
            }
            Ok(format!("operator.methodcaller({})", parts.join(", ")))
        }

        #[proto(reduce)]
        fn __reduce__(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            let (name, args, kwargs) = {
                let me = slf.0.borrow(it)?;
                (me.name.clone(), me.args.clone(), me.kwargs.clone())
            };
            let cls = Value::Obj(it.type_of(slf.0.value()));
            if kwargs.is_empty() {
                let mut all = vec![name];
                all.extend(args);
                return Ok(Value::tuple(vec![cls, Value::tuple(all)]));
            }
            let functools = Value::Obj(it.import_module("functools")?);
            let partial = it.get_attr_str(&functools, "partial")?;
            let ctor = it.call(&partial, vec![cls, name], kwargs)?;
            Ok(Value::tuple(vec![ctor, Value::tuple(args)]))
        }
    }
}
