//! `itertools`. Most tools are `native_iter` classes: `__new__` returns a step closure over the
//! sources, which the VM drives directly; `count` and `repeat` keep inspectable state for their
//! `repr`.

/// Functional tools for creating and using iterators.
///
/// Infinite iterators:
/// count(start=0, step=1) --> start, start+step, start+2*step, ...
/// cycle(p) --> p0, p1, ... plast, p0, p1, ...
/// repeat(elem [,n]) --> elem, elem, elem, ... endlessly or up to n times
///
/// Iterators terminating on the shortest input sequence:
/// accumulate(p[, func]) --> p0, p0+p1, p0+p1+p2
/// batched(p, n[, strict]) --> [p0, p1, ..., p_n-1], [p_n, p_n+1, ..., p_2n-1], ...
/// chain(p, q, ...) --> p0, p1, ... plast, q0, q1, ...
/// chain.from_iterable([p, q, ...]) --> p0, p1, ... plast, q0, q1, ...
/// compress(data, selectors) --> (d[0] if s[0]), (d[1] if s[1]), ...
/// dropwhile(predicate, seq) --> seq[n], seq[n+1], starting when predicate fails
/// groupby(iterable[, keyfunc]) --> sub-iterators grouped by value of keyfunc(v)
/// filterfalse(predicate, seq) --> elements of seq where predicate(elem) is False
/// islice(seq, [start,] stop [, step]) --> elements from
///        seq[start:stop:step]
/// pairwise(s) --> (s[0],s[1]), (s[1],s[2]), (s[2], s[3]), ...
/// starmap(fun, seq) --> fun(*seq[0]), fun(*seq[1]), ...
/// tee(it, n=2) --> (it1, it2 , ... itn) splits one iterator into n
/// takewhile(predicate, seq) --> seq[0], seq[1], until predicate fails
/// zip_longest(p, q, ...) --> (p[0], q[0]), (p[1], q[1]), ...
///
/// Combinatoric generators:
/// product(p, q, ... [repeat=1]) --> cartesian product
/// permutations(p[, r])
/// combinations(p, r)
/// combinations_with_replacement(p, r)
///
#[lumen_bind::module(name = "itertools")]
pub mod itertools {
    // `__new__` of a `native_iter` class returns the step closure, not `Self`.
    #![allow(clippy::module_inception, clippy::new_ret_no_self)]
    use crate::ast::BinOp;
    use crate::bind::{KwArgs, NativeError, NativeIter, NativeResult, Py, This};
    use crate::object::*;
    use crate::vm::Interp;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::{Rc, Weak};

    fn is_true(it: &mut Interp, f: &Value, v: &Value) -> R<bool> {
        if f.is_none() {
            return it.truthy(v);
        }
        let r = it.call(f, vec![v.clone()], Vec::new())?;
        it.truthy(&r)
    }

    fn nonneg_int(it: &mut Interp, v: &Value, msg: &str) -> R<i64> {
        if !it.has_index(v) {
            return Err(it.value_error(msg));
        }
        match it.index_of(v) {
            Ok(n) if n >= 0 => Ok(n),
            _ => Err(it.value_error(msg)),
        }
    }

    // ---- count / repeat -----------------------------------------------------------------------

    /// Return a count object whose .__next__() method returns consecutive values.
    ///
    /// Equivalent to:
    ///     def count(firstval=0, step=1):
    ///         x = firstval
    ///         while 1:
    ///             yield x
    ///             x += step
    #[class(name = "count")]
    pub struct Count {
        cur: Value,
        step: Value,
    }

    #[methods]
    impl Count {
        #[constructor(hint(py(text_signature = "(start=0, step=1)")))]
        fn new(it: &mut Interp, #[kw] #[default(0)] start: Value, #[kw] #[default(1)] step: Value) -> R<Count> {
            for v in [&start, &step] {
                if !it.number_check(v) {
                    return Err(it.type_error("a number is required"));
                }
            }
            Ok(Count { cur: start, step })
        }

        #[proto(iter)]
        fn __iter__(slf: This<Py<Self>>) -> Py<Self> {
            slf.0
        }

        #[proto(next)]
        fn __next__(slf: This<Py<Self>>, it: &mut Interp) -> R<Option<Value>> {
            let slf = slf.0;
            let (cur, step) = {
                let c = slf.borrow(it)?;
                (c.cur.clone(), c.step.clone())
            };
            let next = match (&cur, &step) {
                (Value::Int(x), Value::Int(s)) => match x.checked_add(*s) {
                    Some(n) => Value::Int(n),
                    None => it.binary_op(BinOp::Add, &cur, &step)?,
                },
                _ => it.binary_op(BinOp::Add, &cur, &step)?,
            };
            slf.borrow_mut(it)?.cur = next;
            Ok(Some(cur))
        }

        #[proto(repr)]
        fn __repr__(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
            let slf = slf.0;
            let (cur, step) = {
                let c = slf.borrow(it)?;
                (c.cur.clone(), c.step.clone())
            };
            let ty = it.type_of(slf.value());
            let name = it.type_name(&ty);
            let c = it.repr_of(&cur)?;
            if matches!(step, Value::Int(1)) {
                return Ok(format!("{}({})", name, c));
            }
            Ok(format!("{}({}, {})", name, c, it.repr_of(&step)?))
        }
    }

    /// repeat(object [,times]) -> create an iterator which returns the object
    /// for the specified number of times.  If not specified, returns the object
    /// endlessly.
    #[class(name = "repeat")]
    pub struct Repeat {
        obj: Value,
        times: Option<i64>,
    }

    #[methods]
    impl Repeat {
        #[constructor(hint(py(text_signature = "")))]
        fn new(it: &mut Interp, #[kw] object: Value, #[kw] times: Option<&Value>) -> R<Repeat> {
            let times = match times {
                Some(v) => Some(it.index_of(v)?.max(0)),
                None => None,
            };
            Ok(Repeat { obj: object, times })
        }

        #[proto(iter)]
        fn __iter__(slf: This<Py<Self>>) -> Py<Self> {
            slf.0
        }

        #[proto(next)]
        fn __next__(&mut self) -> Option<Value> {
            match &mut self.times {
                Some(0) => None,
                Some(n) => {
                    *n -= 1;
                    Some(self.obj.clone())
                }
                None => Some(self.obj.clone()),
            }
        }

        #[proto(repr)]
        fn __repr__(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
            let slf = slf.0;
            let (obj, times) = {
                let r = slf.borrow(it)?;
                (r.obj.clone(), r.times)
            };
            let ty = it.type_of(slf.value());
            let name = it.type_name(&ty);
            let o = it.repr_of(&obj)?;
            Ok(match times {
                Some(n) => format!("{}({}, {})", name, o, n),
                None => format!("{}({})", name, o),
            })
        }

        /// Private method returning an estimate of len(list(it)).
        #[method(hint(py(text_signature = "")))]
        fn __length_hint__(&self) -> NativeResult<i64> {
            self.times.ok_or_else(|| NativeError::type_error("len() of unsized object"))
        }
    }

    // ---- infinite and simple adaptors -----------------------------------------------------------

    /// Return elements from the iterable until it is exhausted. Then repeat the sequence indefinitely.
    #[class(name = "cycle", hint(py(native_iter)))]
    pub struct Cycle;

    #[methods]
    impl Cycle {
        #[constructor(hint(py(text_signature = "(iterable, /)")))]
        fn new(it: &mut Interp, iterable: &Value) -> R<NativeIter> {
            let src = it.get_iter(iterable)?;
            let mut saved: Vec<Value> = Vec::new();
            let mut exhausted = false;
            let mut pos = 0usize;
            Ok(NativeIter::new(move |it| {
                if !exhausted {
                    match it.iter_next(&src)? {
                        Some(v) => {
                            saved.push(v.clone());
                            return Ok(Some(v));
                        }
                        None => exhausted = true,
                    }
                }
                if saved.is_empty() {
                    return Ok(None);
                }
                let v = saved[pos % saved.len()].clone();
                pos = (pos + 1) % saved.len();
                Ok(Some(v))
            }))
        }
    }

    /// Iterates each iterable `outer` yields in turn.
    fn chain_of(mut outer: impl FnMut(&mut Interp) -> R<Option<Value>> + 'static) -> NativeIter {
        let mut cur: Option<Value> = None;
        NativeIter::new(move |it| loop {
            let Some(c) = &cur else {
                match outer(it)? {
                    Some(v) => cur = Some(it.get_iter(&v)?),
                    None => return Ok(None),
                }
                continue;
            };
            match it.iter_next(c)? {
                Some(v) => return Ok(Some(v)),
                None => cur = None,
            }
        })
    }

    /// chain(*iterables) --> chain object
    ///
    /// Return a chain object whose .__next__() method returns elements from the
    /// first iterable until it is exhausted, then elements from the next
    /// iterable, until all of the iterables are exhausted.
    #[class(name = "chain", generic, hint(py(native_iter)))]
    pub struct Chain;

    #[methods]
    impl Chain {
        #[constructor(hint(py(text_signature = "")))]
        fn new(#[varargs] iterables: &[Value]) -> NativeIter {
            let mut pending: VecDeque<Value> = iterables.iter().cloned().collect();
            chain_of(move |_| Ok(pending.pop_front()))
        }

        /// Alternative chain() constructor taking a single iterable argument that evaluates lazily.
        #[classmethod]
        fn from_iterable(cls: This<Value>, it: &mut Interp, iterable: &Value) -> R<Value> {
            let Value::Obj(cls) = cls.0 else { return Err(it.type_error("from_iterable() needs a class")) };
            let outer = it.get_iter(iterable)?;
            Ok(chain_of(move |it| it.iter_next(&outer)).into_object(&cls))
        }
    }

    /// Return series of accumulated sums (or other binary function results).
    #[class(name = "accumulate", hint(py(native_iter)))]
    pub struct Accumulate;

    #[methods]
    impl Accumulate {
        #[constructor(hint(py(text_signature = "(iterable, func=None, *, initial=None)")))]
        fn new(it: &mut Interp, #[kw] iterable: &Value, #[kw] #[default(Value::None)] func: Value, #[kwonly] initial: Option<Value>) -> R<NativeIter> {
            let src = it.get_iter(iterable)?;
            let mut initial = initial;
            let mut total: Option<Value> = None;
            Ok(NativeIter::new(move |it| {
                if let Some(i) = initial.take() {
                    total = Some(i.clone());
                    return Ok(Some(i));
                }
                let Some(v) = it.iter_next(&src)? else { return Ok(None) };
                let next = match &total {
                    None => v,
                    Some(t) if func.is_none() => it.binary_op(BinOp::Add, t, &v)?,
                    Some(t) => it.call(&func, vec![t.clone(), v], Vec::new())?,
                };
                total = Some(next.clone());
                Ok(Some(next))
            }))
        }
    }

    /// Return data elements corresponding to true selector elements.
    ///
    /// Forms a shorter iterator from selected data elements using the selectors to
    /// choose the data elements.
    #[class(name = "compress", hint(py(native_iter)))]
    pub struct Compress;

    #[methods]
    impl Compress {
        #[constructor(hint(py(text_signature = "(data, selectors)")))]
        fn new(it: &mut Interp, #[kw] data: &Value, #[kw] selectors: &Value) -> R<NativeIter> {
            let data = it.get_iter(data)?;
            let sel = it.get_iter(selectors)?;
            Ok(NativeIter::new(move |it| loop {
                let Some(d) = it.iter_next(&data)? else { return Ok(None) };
                let Some(s) = it.iter_next(&sel)? else { return Ok(None) };
                if it.truthy(&s)? {
                    return Ok(Some(d));
                }
            }))
        }
    }

    /// Drop items from the iterable while predicate(item) is true.
    ///
    /// Afterwards, return every element until the iterable is exhausted.
    #[class(name = "dropwhile", hint(py(native_iter)))]
    pub struct DropWhile;

    #[methods]
    impl DropWhile {
        #[constructor(hint(py(text_signature = "(predicate, iterable, /)")))]
        fn new(it: &mut Interp, predicate: Value, iterable: &Value) -> R<NativeIter> {
            let src = it.get_iter(iterable)?;
            let mut dropping = true;
            Ok(NativeIter::new(move |it| loop {
                let Some(v) = it.iter_next(&src)? else { return Ok(None) };
                if dropping {
                    let r = it.call(&predicate, vec![v.clone()], Vec::new())?;
                    if it.truthy(&r)? {
                        continue;
                    }
                    dropping = false;
                }
                return Ok(Some(v));
            }))
        }
    }

    /// Return successive entries from an iterable as long as the predicate evaluates to true for each entry.
    #[class(name = "takewhile", hint(py(native_iter)))]
    pub struct TakeWhile;

    #[methods]
    impl TakeWhile {
        #[constructor(hint(py(text_signature = "(predicate, iterable, /)")))]
        fn new(it: &mut Interp, predicate: Value, iterable: &Value) -> R<NativeIter> {
            let src = it.get_iter(iterable)?;
            let mut done = false;
            Ok(NativeIter::new(move |it| {
                if done {
                    return Ok(None);
                }
                let Some(v) = it.iter_next(&src)? else { return Ok(None) };
                let r = it.call(&predicate, vec![v.clone()], Vec::new())?;
                if it.truthy(&r)? {
                    Ok(Some(v))
                } else {
                    done = true;
                    Ok(None)
                }
            }))
        }
    }

    /// Return those items of iterable for which function(item) is false.
    ///
    /// If function is None, return the items that are false.
    #[class(name = "filterfalse", hint(py(native_iter)))]
    pub struct FilterFalse;

    #[methods]
    impl FilterFalse {
        #[constructor(hint(py(text_signature = "(function, iterable, /)")))]
        fn new(it: &mut Interp, function: Value, iterable: &Value) -> R<NativeIter> {
            let src = it.get_iter(iterable)?;
            Ok(NativeIter::new(move |it| loop {
                let Some(v) = it.iter_next(&src)? else { return Ok(None) };
                if !is_true(it, &function, &v)? {
                    return Ok(Some(v));
                }
            }))
        }
    }

    /// Return an iterator whose values are returned from the function evaluated with an argument tuple taken from the given sequence.
    #[class(name = "starmap", hint(py(native_iter)))]
    pub struct StarMap;

    #[methods]
    impl StarMap {
        #[constructor(hint(py(text_signature = "(function, iterable, /)")))]
        fn new(it: &mut Interp, function: Value, iterable: &Value) -> R<NativeIter> {
            let src = it.get_iter(iterable)?;
            Ok(NativeIter::new(move |it| {
                let Some(v) = it.iter_next(&src)? else { return Ok(None) };
                let args = it.iterate_to_vec(&v)?;
                Ok(Some(it.call(&function, args, Vec::new())?))
            }))
        }
    }

    /// Return an iterator of overlapping pairs taken from the input iterator.
    ///
    ///     s -> (s0,s1), (s1,s2), (s2, s3), ...
    #[class(name = "pairwise", hint(py(native_iter)))]
    pub struct Pairwise;

    #[methods]
    impl Pairwise {
        #[constructor(hint(py(text_signature = "(iterable, /)")))]
        fn new(it: &mut Interp, iterable: &Value) -> R<NativeIter> {
            let src = it.get_iter(iterable)?;
            let mut prev: Option<Value> = None;
            let mut started = false;
            Ok(NativeIter::new(move |it| {
                if !started {
                    started = true;
                    prev = it.iter_next(&src)?;
                }
                let Some(p) = prev.clone() else { return Ok(None) };
                match it.iter_next(&src)? {
                    Some(n) => {
                        prev = Some(n.clone());
                        Ok(Some(Value::tuple(vec![p, n])))
                    }
                    None => {
                        prev = None;
                        Ok(None)
                    }
                }
            }))
        }
    }

    /// Batch data into tuples of length n. The last batch may be shorter than n.
    ///
    /// Loops over the input iterable and accumulates data into tuples
    /// up to size n.  The input is consumed lazily, just enough to
    /// fill a batch.  The result is yielded as soon as a batch is full
    /// or when the input iterable is exhausted.
    ///
    ///     >>> for batch in batched('ABCDEFG', 3):
    ///     ...     print(batch)
    ///     ...
    ///     ('A', 'B', 'C')
    ///     ('D', 'E', 'F')
    ///     ('G',)
    ///
    /// If "strict" is True, raises a ValueError if the final batch is shorter
    /// than n.
    #[class(name = "batched", hint(py(native_iter)))]
    pub struct Batched;

    #[methods]
    impl Batched {
        #[constructor(hint(py(text_signature = "(iterable, n, *, strict=False)")))]
        fn new(it: &mut Interp, #[kw] iterable: &Value, #[kw] n: isize, #[kwonly] #[default(false)] strict: bool) -> R<NativeIter> {
            if n < 1 {
                return Err(it.value_error("n must be at least one"));
            }
            let src = it.get_iter(iterable)?;
            let n = n as usize;
            Ok(NativeIter::new(move |it| {
                let mut batch = Vec::new();
                while batch.len() < n {
                    match it.iter_next(&src)? {
                        Some(v) => batch.push(v),
                        None => break,
                    }
                }
                if batch.is_empty() {
                    return Ok(None);
                }
                if strict && batch.len() < n {
                    return Err(it.value_error("batched(): incomplete batch"));
                }
                Ok(Some(Value::tuple(batch)))
            }))
        }
    }

    /// islice(iterable, stop) --> islice object
    /// islice(iterable, start, stop[, step]) --> islice object
    ///
    /// Return an iterator whose next() method returns selected values from an
    /// iterable.  If start is specified, will skip all preceding elements;
    /// otherwise, start defaults to zero.  Step defaults to one.  If
    /// specified as another value, step determines how many values are
    /// skipped between successive calls.  Works like a slice() on a list
    /// but returns an iterator.
    #[class(name = "islice", hint(py(native_iter)))]
    pub struct ISlice;

    #[methods]
    impl ISlice {
        // `islice(iterable, stop)` / `islice(iterable, start, stop[, step])`.
        #[constructor(hint(py(text_signature = "")))]
        fn new(it: &mut Interp, #[varargs] args: &[Value]) -> R<NativeIter> {
            if args.len() < 2 {
                return Err(it.type_error(&format!("islice expected at least 2 arguments, got {}", args.len())));
            }
            if args.len() > 4 {
                return Err(it.type_error(&format!("islice expected at most 4 arguments, got {}", args.len())));
            }
            let stop_msg = "Stop argument for islice() must be None or an integer: 0 <= x <= sys.maxsize.";
            let idx_msg = "Indices for islice() must be None or an integer: 0 <= x <= sys.maxsize.";
            let (start, stop, step) = if args.len() == 2 {
                let stop = if args[1].is_none() { None } else { Some(nonneg_int(it, &args[1], stop_msg)?) };
                (0, stop, 1)
            } else {
                let start = if args[1].is_none() { 0 } else { nonneg_int(it, &args[1], idx_msg)? };
                let stop = if args[2].is_none() { None } else { Some(nonneg_int(it, &args[2], idx_msg)?) };
                let step = match args.get(3) {
                    Some(v) if !v.is_none() => match it.has_index(v).then(|| it.index_of(v)) {
                        Some(Ok(s)) if s > 0 => s,
                        _ => return Err(it.value_error("Step for islice() must be a positive integer or None.")),
                    },
                    _ => 1,
                };
                (start, stop, step)
            };
            let src = it.get_iter(&args[0])?;
            let mut cnt: i64 = 0;
            let mut next = start;
            let mut done = false;
            Ok(NativeIter::new(move |it| {
                if done {
                    return Ok(None);
                }
                while cnt < next {
                    if it.iter_next(&src)?.is_none() {
                        done = true;
                        return Ok(None);
                    }
                    cnt += 1;
                }
                if stop.is_some_and(|s| cnt >= s) {
                    done = true;
                    return Ok(None);
                }
                let Some(v) = it.iter_next(&src)? else {
                    done = true;
                    return Ok(None);
                };
                cnt += 1;
                let old = next;
                next = next.saturating_add(step);
                if next < old || stop.is_some_and(|s| next > s) {
                    next = stop.unwrap_or(i64::MAX);
                }
                Ok(Some(v))
            }))
        }
    }

    /// Return a zip_longest object whose .__next__() method returns a tuple where
    /// the i-th element comes from the i-th iterable argument.  The .__next__()
    /// method continues until the longest iterable in the argument sequence
    /// is exhausted and then it raises StopIteration.  When the shorter iterables
    /// are exhausted, the fillvalue is substituted in their place.  The fillvalue
    /// defaults to None or can be specified by a keyword argument.
    ///
    #[class(name = "zip_longest", hint(py(native_iter)))]
    pub struct ZipLongest;

    #[methods]
    impl ZipLongest {
        #[constructor(hint(py(text_signature = "")))]
        fn new(it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<NativeIter> {
            let mut fill = Value::None;
            if !kwargs.is_empty() {
                match kwargs.get("fillvalue") {
                    Some(v) if kwargs.len() == 1 => fill = v.clone(),
                    _ => return Err(it.type_error("zip_longest() got an unexpected keyword argument")),
                }
            }
            let mut its: Vec<Option<Value>> = Vec::with_capacity(args.len());
            for v in args {
                its.push(Some(it.get_iter(v)?));
            }
            let mut active = its.len();
            Ok(NativeIter::new(move |it| {
                if its.is_empty() || active == 0 {
                    return Ok(None);
                }
                let mut out = Vec::with_capacity(its.len());
                for slot in its.iter_mut() {
                    match slot {
                        None => out.push(fill.clone()),
                        Some(src) => match it.iter_next(src)? {
                            Some(v) => out.push(v),
                            None => {
                                *slot = None;
                                active -= 1;
                                if active == 0 {
                                    return Ok(None);
                                }
                                out.push(fill.clone());
                            }
                        },
                    }
                }
                Ok(Some(Value::tuple(out)))
            }))
        }
    }

    // ---- tee ----------------------------------------------------------------------------------

    struct TeeShared {
        src: Value,
        buf: VecDeque<Value>,
        base: usize,
        positions: Vec<Weak<Cell<usize>>>,
        /// Set while the source is stepped (CPython's `teedataobject.running`).
        running: bool,
    }

    // One of `tee`'s iterators: a position in the buffer the siblings share.
    #[class(name = "_tee")]
    pub struct Tee {
        shared: Rc<RefCell<TeeShared>>,
        pos: Rc<Cell<usize>>,
    }

    impl Tee {
        fn at(shared: &Rc<RefCell<TeeShared>>, p: usize) -> Tee {
            let pos = Rc::new(Cell::new(p));
            shared.borrow_mut().positions.push(Rc::downgrade(&pos));
            Tee { shared: shared.clone(), pos }
        }

        fn of_iter(src: Value) -> Tee {
            let shared = Rc::new(RefCell::new(TeeShared { src, buf: VecDeque::new(), base: 0, positions: Vec::new(), running: false }));
            Tee::at(&shared, 0)
        }
    }

    #[methods]
    impl Tee {
        #[constructor]
        fn new(it: &mut Interp, iterable: &Value) -> R<Tee> {
            let src = it.get_iter(iterable)?;
            Ok(Tee::of_iter(src))
        }

        #[proto(iter)]
        fn __iter__(slf: This<Py<Self>>) -> Py<Self> {
            slf.0
        }

        #[proto(next)]
        fn __next__(&self, it: &mut Interp) -> R<Option<Value>> {
            let p = self.pos.get();
            let cached = {
                let s = self.shared.borrow();
                if s.running {
                    drop(s);
                    return Err(it.new_exc_str("RuntimeError", "cannot re-enter the tee iterator"));
                }
                (p < s.base + s.buf.len()).then(|| s.buf[p - s.base].clone())
            };
            let v = match cached {
                Some(v) => v,
                None => {
                    let src = {
                        let mut s = self.shared.borrow_mut();
                        s.running = true;
                        s.src.clone()
                    };
                    let r = it.iter_next(&src);
                    self.shared.borrow_mut().running = false;
                    let Some(v) = r? else { return Ok(None) };
                    self.shared.borrow_mut().buf.push_back(v.clone());
                    v
                }
            };
            self.pos.set(p + 1);
            let mut s = self.shared.borrow_mut();
            s.positions.retain(|w| w.strong_count() > 0);
            let min = s.positions.iter().filter_map(|w| w.upgrade()).map(|c| c.get()).min().unwrap_or(p + 1);
            while s.base < min && !s.buf.is_empty() {
                s.buf.pop_front();
                s.base += 1;
            }
            Ok(Some(v))
        }

        /// Returns an independent iterator.
        #[proto(copy, hint(py(text_signature = "")))]
        fn __copy__(&self) -> Tee {
            Tee::at(&self.shared, self.pos.get())
        }
    }

    /// Returns a tuple of n independent iterators.
    #[op]
    fn tee(it: &mut Interp, iterable: &Value, #[default(2)] n: isize) -> R<Value> {
        if n < 0 {
            return Err(it.value_error("n must be >= 0"));
        }
        if n == 0 {
            return Ok(Value::tuple(Vec::new()));
        }
        let src = it.get_iter(iterable)?;
        // An iterator with `__copy__` (another tee) is copied rather than wrapped.
        let copy = match it.get_attr_str(&src, "__copy__") {
            Ok(f) => Some(f),
            Err(e) if it.exc_is(&e, "AttributeError") => None,
            Err(e) => return Err(e),
        };
        let (first, copy) = match copy {
            Some(f) => (src, f),
            None => {
                let t = crate::bind::Py::new(it, Tee::of_iter(src)).into_value();
                let f = it.get_attr_str(&t, "__copy__")?;
                (t, f)
            }
        };
        let mut out = vec![first];
        for _ in 1..n {
            out.push(it.call(&copy, Vec::new(), Vec::new())?);
        }
        Ok(Value::tuple(out))
    }

    // ---- groupby ------------------------------------------------------------------------------

    struct GroupState {
        src: Value,
        keyfunc: Value,
        tgtkey: Option<Value>,
        currkey: Option<Value>,
        currvalue: Option<Value>,
        grouper_id: u64,
    }

    fn groupby_step(it: &mut Interp, g: &Rc<RefCell<GroupState>>) -> R<bool> {
        let (src, keyfunc) = {
            let b = g.borrow();
            (b.src.clone(), b.keyfunc.clone())
        };
        let Some(v) = it.iter_next(&src)? else { return Ok(false) };
        let k = if keyfunc.is_none() { v.clone() } else { it.call(&keyfunc, vec![v.clone()], Vec::new())? };
        let mut b = g.borrow_mut();
        b.currvalue = Some(v);
        b.currkey = Some(k);
        Ok(true)
    }

    #[class(name = "_grouper", hint(py(native_iter)))]
    pub struct Grouper;

    #[methods]
    impl Grouper {}

    /// make an iterator that returns consecutive keys and groups from the iterable
    ///
    ///   iterable
    ///     Elements to divide into groups according to the key function.
    ///   key
    ///     A function for computing the group category for each element.
    ///     If the key function is not specified or is None, the element itself
    ///     is used for grouping.
    #[class(name = "groupby", hint(py(native_iter)))]
    pub struct GroupBy;

    #[methods]
    impl GroupBy {
        #[constructor(hint(py(text_signature = "(iterable, key=None)")))]
        fn new(it: &mut Interp, #[kw] iterable: &Value, #[kw] #[default(Value::None)] key: Value) -> R<NativeIter> {
            let src = it.get_iter(iterable)?;
            let g = Rc::new(RefCell::new(GroupState { src, keyfunc: key, tgtkey: None, currkey: None, currvalue: None, grouper_id: 0 }));
            let mut next_id = 0u64;
            Ok(NativeIter::new(move |it| {
                next_id += 1;
                g.borrow_mut().grouper_id = next_id;
                loop {
                    let (currkey, tgtkey) = {
                        let b = g.borrow();
                        (b.currkey.clone(), b.tgtkey.clone())
                    };
                    match (&currkey, &tgtkey) {
                        (None, _) => {}
                        (Some(_), None) => break,
                        (Some(c), Some(t)) => {
                            if !it.values_eq(t, c)? {
                                break;
                            }
                        }
                    }
                    if !groupby_step(it, &g)? {
                        return Ok(None);
                    }
                }
                let key = g.borrow().currkey.clone().unwrap();
                g.borrow_mut().tgtkey = Some(key.clone());
                let id = next_id;
                let g2 = g.clone();
                let grouper = NativeIter::new(move |it| {
                    if g2.borrow().grouper_id != id {
                        return Ok(None);
                    }
                    if g2.borrow().currvalue.is_none() && !groupby_step(it, &g2)? {
                        return Ok(None);
                    }
                    let (t, c) = {
                        let b = g2.borrow();
                        (b.tgtkey.clone().unwrap(), b.currkey.clone().unwrap())
                    };
                    if !it.values_eq(&t, &c)? {
                        return Ok(None);
                    }
                    Ok(g2.borrow_mut().currvalue.take())
                });
                Ok(Some(Value::tuple(vec![key, grouper.instance_of::<Grouper>(it)])))
            }))
        }
    }

    // ---- combinatorics ------------------------------------------------------------------------

    fn pick(pool: &[Value], indices: &[usize]) -> Value {
        Value::tuple(indices.iter().map(|&i| pool[i].clone()).collect())
    }

    /// product(*iterables, repeat=1) --> product object
    ///
    /// Cartesian product of input iterables.  Equivalent to nested for-loops.
    ///
    /// For example, product(A, B) returns the same as:  ((x,y) for x in A for y in B).
    /// The leftmost iterators are in the outermost for-loop, so the output tuples
    /// cycle in a manner similar to an odometer (with the rightmost element changing
    /// on every iteration).
    ///
    /// To compute the product of an iterable with itself, specify the number
    /// of repetitions with the optional repeat keyword argument. For example,
    /// product(A, repeat=4) means the same as product(A, A, A, A).
    ///
    /// product('ab', range(3)) --> ('a',0) ('a',1) ('a',2) ('b',0) ('b',1) ('b',2)
    /// product((0,1), (0,1), (0,1)) --> (0,0,0) (0,0,1) (0,1,0) (0,1,1) (1,0,0) ...
    #[class(name = "product", hint(py(native_iter)))]
    pub struct Product;

    #[methods]
    impl Product {
        #[constructor(hint(py(text_signature = "")))]
        fn new(it: &mut Interp, #[varargs] iterables: &[Value], #[kwonly] #[default(1)] repeat: isize) -> R<NativeIter> {
            if repeat < 0 {
                return Err(it.value_error("repeat argument cannot be negative"));
            }
            let mut base: Vec<Vec<Value>> = Vec::with_capacity(iterables.len());
            for v in iterables {
                base.push(it.iterate_to_vec(v)?);
            }
            let mut pools: Vec<Vec<Value>> = Vec::new();
            for _ in 0..repeat {
                pools.extend(base.iter().cloned());
            }
            let mut indices = vec![0usize; pools.len()];
            let mut started = false;
            let mut done = pools.iter().any(|p| p.is_empty());
            Ok(NativeIter::new(move |_it| {
                if done {
                    return Ok(None);
                }
                if started {
                    let mut i = pools.len();
                    loop {
                        if i == 0 {
                            done = true;
                            return Ok(None);
                        }
                        i -= 1;
                        indices[i] += 1;
                        if indices[i] < pools[i].len() {
                            break;
                        }
                        indices[i] = 0;
                    }
                }
                started = true;
                Ok(Some(Value::tuple(indices.iter().zip(pools.iter()).map(|(&i, p)| p[i].clone()).collect())))
            }))
        }
    }

    /// Return successive r-length permutations of elements in the iterable.
    ///
    /// permutations(range(3), 2) --> (0,1), (0,2), (1,0), (1,2), (2,0), (2,1)
    #[class(name = "permutations", hint(py(native_iter)))]
    pub struct Permutations;

    #[methods]
    impl Permutations {
        #[constructor(hint(py(text_signature = "(iterable, r=None)")))]
        fn new(it: &mut Interp, #[kw] iterable: &Value, #[kw] r: Option<&Value>) -> R<NativeIter> {
            let pool = it.iterate_to_vec(iterable)?;
            let n = pool.len();
            let r = match r {
                Some(v) if !matches!(v, Value::None) => {
                    if !matches!(v, Value::Int(_) | Value::Bool(_)) && v.as_bigint().is_none() {
                        return Err(it.type_error("Expected int as r"));
                    }
                    let r = it.index_of(v)?;
                    if r < 0 {
                        return Err(it.value_error("r must be non-negative"));
                    }
                    r as usize
                }
                _ => n,
            };
            let mut indices: Vec<usize> = (0..n).collect();
            let mut cycles: Vec<usize> = (0..r).map(|i| n - i.min(n)).collect();
            let mut started = false;
            let mut done = r > n;
            Ok(NativeIter::new(move |_it| {
                if done {
                    return Ok(None);
                }
                if !started {
                    started = true;
                    return Ok(Some(pick(&pool, &indices[..r])));
                }
                let mut i = r;
                loop {
                    if i == 0 {
                        done = true;
                        return Ok(None);
                    }
                    i -= 1;
                    cycles[i] -= 1;
                    if cycles[i] == 0 {
                        let moved = indices.remove(i);
                        indices.push(moved);
                        cycles[i] = n - i;
                    } else {
                        let j = n - cycles[i];
                        indices.swap(i, j);
                        return Ok(Some(pick(&pool, &indices[..r])));
                    }
                }
            }))
        }
    }

    fn combinations_r(it: &mut Interp, r: &Value) -> R<usize> {
        let r = it.index_of(r)?;
        if r < 0 {
            return Err(it.value_error("r must be non-negative"));
        }
        Ok(r as usize)
    }

    /// Return successive r-length combinations of elements in the iterable.
    ///
    /// combinations(range(4), 3) --> (0,1,2), (0,1,3), (0,2,3), (1,2,3)
    #[class(name = "combinations", hint(py(native_iter)))]
    pub struct Combinations;

    #[methods]
    impl Combinations {
        #[constructor(hint(py(text_signature = "(iterable, r)")))]
        fn new(it: &mut Interp, #[kw] iterable: &Value, #[kw] r: &Value) -> R<NativeIter> {
            let pool = it.iterate_to_vec(iterable)?;
            let r = combinations_r(it, r)?;
            let n = pool.len();
            let mut indices: Vec<usize> = (0..r).collect();
            let mut started = false;
            let mut done = r > n;
            Ok(NativeIter::new(move |_it| {
                if done {
                    return Ok(None);
                }
                if started {
                    let mut i = r;
                    loop {
                        if i == 0 {
                            done = true;
                            return Ok(None);
                        }
                        i -= 1;
                        if indices[i] != i + n - r {
                            break;
                        }
                    }
                    indices[i] += 1;
                    for j in i + 1..r {
                        indices[j] = indices[j - 1] + 1;
                    }
                }
                started = true;
                Ok(Some(pick(&pool, &indices)))
            }))
        }
    }

    /// Return successive r-length combinations of elements in the iterable allowing individual elements to have successive repeats.
    ///
    /// combinations_with_replacement('ABC', 2) --> ('A','A'), ('A','B'), ('A','C'), ('B','B'), ('B','C'), ('C','C')
    #[class(name = "combinations_with_replacement", hint(py(native_iter)))]
    pub struct CombinationsWithReplacement;

    #[methods]
    impl CombinationsWithReplacement {
        #[constructor(hint(py(text_signature = "(iterable, r)")))]
        fn new(it: &mut Interp, #[kw] iterable: &Value, #[kw] r: &Value) -> R<NativeIter> {
            let pool = it.iterate_to_vec(iterable)?;
            let r = combinations_r(it, r)?;
            let n = pool.len();
            let mut indices = vec![0usize; r];
            let mut started = false;
            let mut done = n == 0 && r > 0;
            Ok(NativeIter::new(move |_it| {
                if done {
                    return Ok(None);
                }
                if started {
                    let mut i = r;
                    loop {
                        if i == 0 {
                            done = true;
                            return Ok(None);
                        }
                        i -= 1;
                        if indices[i] != n - 1 {
                            break;
                        }
                    }
                    let v = indices[i] + 1;
                    for slot in indices.iter_mut().skip(i) {
                        *slot = v;
                    }
                }
                started = true;
                Ok(Some(pick(&pool, &indices)))
            }))
        }
    }
}
