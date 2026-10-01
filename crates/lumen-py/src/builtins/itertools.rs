//! `itertools`. Most tools are closures over their sources, stored in an `Iter` object whose class
//! is the tool's type; `count` and `repeat` keep inspectable state for their `repr`.

use super::native::*;
use super::slots::reg_iterator;
use crate::ast::BinOp;
use crate::object::*;
use crate::vm::*;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

type Step = Box<dyn FnMut(&mut Interp) -> R<Option<Value>>>;

fn tool_type(it: &mut Interp, name: &str) -> Obj {
    let ty = new_type(it, "itertools", name, None, Layout::Other);
    if let Kind::Type(td) = &ty.kind {
        td.flags.set(td.flags.get() & !TF_DISPATCH);
    }
    ty
}

fn make(a: &[Value], f: Step) -> Value {
    let cls = match a.first() {
        Some(Value::Obj(c)) => c.clone(),
        _ => unreachable!("__new__ is called with its class"),
    };
    Value::Obj(Object::with_cls(cls, Kind::Iter(RefCell::new(IterState::Native(f)))))
}

fn args_of(a: &[Value]) -> &[Value] {
    &a[1.min(a.len())..]
}

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

// ---- count / repeat ---------------------------------------------------------------------------

struct Count {
    cur: Value,
    step: Value,
}

struct Repeat {
    obj: Value,
    times: Option<i64>,
}

fn is_number(it: &mut Interp, v: &Value) -> bool {
    match v {
        Value::Int(_) | Value::Bool(_) | Value::Float(_) => true,
        Value::Obj(o) => matches!(o.kind, Kind::Int(_) | Kind::Float(_) | Kind::Complex(..)) || {
            let cls = it.type_of_obj(o);
            it.lookup_mro(&cls, "__add__").is_some() && it.lookup_mro(&cls, "__index__").is_some() || it.lookup_mro(&cls, "__float__").is_some()
        },
        _ => false,
    }
}

fn count_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("count", args_of(a), kw, &["start", "step"], 0)?;
    let cur = b[0].clone().unwrap_or(Value::Int(0));
    let step = b[1].clone().unwrap_or(Value::Int(1));
    for v in [&cur, &step] {
        if !is_number(it, v) {
            return Err(it.type_error("a number is required"));
        }
    }
    let Value::Obj(cls) = &a[0] else { unreachable!() };
    Ok(new_opaque(cls, Count { cur, step }))
}

fn count_next(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let Some((cur, step)) = with_opaque::<Count, _>(&a[0], |c| (c.cur.clone(), c.step.clone())) else { return Err(it.self_state_err("count")) };
    let next = match (&cur, &step) {
        (Value::Int(x), Value::Int(s)) => match x.checked_add(*s) {
            Some(n) => Value::Int(n),
            None => it.binary_op(BinOp::Add, &cur, &step)?,
        },
        _ => it.binary_op(BinOp::Add, &cur, &step)?,
    };
    with_opaque::<Count, _>(&a[0], |c| c.cur = next);
    Ok(cur)
}

fn count_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let Some((cur, step)) = with_opaque::<Count, _>(&a[0], |c| (c.cur.clone(), c.step.clone())) else { return Err(it.self_state_err("count")) };
    let ty = it.type_of(&a[0]);
    let name = it.type_display(&ty);
    let c = it.repr_of(&cur)?;
    let omit_step = matches!(step, Value::Int(1));
    Ok(Value::string(if omit_step { format!("{}({})", name, c) } else { format!("{}({}, {})", name, c, it.repr_of(&step)?) }))
}

fn repeat_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("repeat", args_of(a), kw, &["object", "times"], 1)?;
    let times = match &b[1] {
        Some(v) => Some(it.index_of(v)?.max(0)),
        None => None,
    };
    let Value::Obj(cls) = &a[0] else { unreachable!() };
    Ok(new_opaque(cls, Repeat { obj: b[0].clone().unwrap(), times }))
}

fn repeat_next(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let r = with_opaque::<Repeat, _>(&a[0], |r| match &mut r.times {
        Some(0) => None,
        Some(n) => {
            *n -= 1;
            Some(r.obj.clone())
        }
        None => Some(r.obj.clone()),
    });
    match r {
        Some(Some(v)) => Ok(v),
        Some(None) => Err(it.new_exc_str("StopIteration", "")),
        None => Err(it.self_state_err("repeat")),
    }
}

fn repeat_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let Some((obj, times)) = with_opaque::<Repeat, _>(&a[0], |r| (r.obj.clone(), r.times)) else { return Err(it.self_state_err("repeat")) };
    let ty = it.type_of(&a[0]);
    let name = it.type_display(&ty);
    let o = it.repr_of(&obj)?;
    Ok(Value::string(match times {
        Some(n) => format!("{}({}, {})", name, o, n),
        None => format!("{}({})", name, o),
    }))
}

fn repeat_len(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    match with_opaque::<Repeat, _>(&a[0], |r| r.times) {
        Some(Some(n)) => Ok(Value::Int(n)),
        Some(None) => Err(it.type_error("len() of unsized object")),
        None => Err(it.self_state_err("repeat")),
    }
}

fn opaque_iter_self(_it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(a[0].clone())
}

// ---- infinite and simple adaptors ---------------------------------------------------------------

fn cycle_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    it.no_kwargs("cycle", kw)?;
    it.check_args("cycle", args_of(a), 1, 1)?;
    let src = it.get_iter(&a[1])?;
    let mut saved: Vec<Value> = Vec::new();
    let mut exhausted = false;
    let mut pos = 0usize;
    Ok(make(
        a,
        Box::new(move |it| {
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
        }),
    ))
}

fn chain_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    it.no_kwargs("chain", kw)?;
    let mut pending: VecDeque<Value> = args_of(a).iter().cloned().collect();
    let mut cur: Option<Value> = None;
    Ok(make(
        a,
        Box::new(move |it| loop {
            if cur.is_none() {
                match pending.pop_front() {
                    Some(v) => cur = Some(it.get_iter(&v)?),
                    None => return Ok(None),
                }
            }
            match it.iter_next(cur.as_ref().unwrap())? {
                Some(v) => return Ok(Some(v)),
                None => cur = None,
            }
        }),
    ))
}

fn chain_from_iterable(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    it.no_kwargs("from_iterable", kw)?;
    it.check_args("from_iterable", a, 2, 2)?;
    let outer = it.get_iter(&a[1])?;
    let mut cur: Option<Value> = None;
    Ok(make(
        a,
        Box::new(move |it| loop {
            if cur.is_none() {
                match it.iter_next(&outer)? {
                    Some(v) => cur = Some(it.get_iter(&v)?),
                    None => return Ok(None),
                }
            }
            match it.iter_next(cur.as_ref().unwrap())? {
                Some(v) => return Ok(Some(v)),
                None => cur = None,
            }
        }),
    ))
}

fn accumulate_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("accumulate", args_of(a), kw, &["iterable", "func", "initial"], 1)?;
    let src = it.get_iter(b[0].as_ref().unwrap())?;
    let func = b[1].clone().unwrap_or(Value::None);
    let mut initial = b[2].clone().filter(|v| !v.is_none());
    let mut total: Option<Value> = None;
    Ok(make(
        a,
        Box::new(move |it| {
            if let Some(i) = initial.take() {
                total = Some(i.clone());
                return Ok(Some(i));
            }
            let Some(v) = it.iter_next(&src)? else { return Ok(None) };
            let next = match &total {
                None => v,
                Some(t) => {
                    if func.is_none() {
                        it.binary_op(BinOp::Add, t, &v)?
                    } else {
                        it.call(&func, vec![t.clone(), v], Vec::new())?
                    }
                }
            };
            total = Some(next.clone());
            Ok(Some(next))
        }),
    ))
}

fn compress_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("compress", args_of(a), kw, &["data", "selectors"], 2)?;
    let data = it.get_iter(b[0].as_ref().unwrap())?;
    let sel = it.get_iter(b[1].as_ref().unwrap())?;
    Ok(make(
        a,
        Box::new(move |it| loop {
            let Some(d) = it.iter_next(&data)? else { return Ok(None) };
            let Some(s) = it.iter_next(&sel)? else { return Ok(None) };
            if it.truthy(&s)? {
                return Ok(Some(d));
            }
        }),
    ))
}

fn dropwhile_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    it.no_kwargs("dropwhile", kw)?;
    it.check_args("dropwhile", args_of(a), 2, 2)?;
    let pred = a[1].clone();
    let src = it.get_iter(&a[2])?;
    let mut dropping = true;
    Ok(make(
        a,
        Box::new(move |it| loop {
            let Some(v) = it.iter_next(&src)? else { return Ok(None) };
            if dropping {
                let r = it.call(&pred, vec![v.clone()], Vec::new())?;
                if it.truthy(&r)? {
                    continue;
                }
                dropping = false;
            }
            return Ok(Some(v));
        }),
    ))
}

fn takewhile_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    it.no_kwargs("takewhile", kw)?;
    it.check_args("takewhile", args_of(a), 2, 2)?;
    let pred = a[1].clone();
    let src = it.get_iter(&a[2])?;
    let mut done = false;
    Ok(make(
        a,
        Box::new(move |it| {
            if done {
                return Ok(None);
            }
            let Some(v) = it.iter_next(&src)? else { return Ok(None) };
            let r = it.call(&pred, vec![v.clone()], Vec::new())?;
            if it.truthy(&r)? {
                Ok(Some(v))
            } else {
                done = true;
                Ok(None)
            }
        }),
    ))
}

fn filterfalse_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    it.no_kwargs("filterfalse", kw)?;
    it.check_args("filterfalse", args_of(a), 2, 2)?;
    let pred = a[1].clone();
    let src = it.get_iter(&a[2])?;
    Ok(make(
        a,
        Box::new(move |it| loop {
            let Some(v) = it.iter_next(&src)? else { return Ok(None) };
            if !is_true(it, &pred, &v)? {
                return Ok(Some(v));
            }
        }),
    ))
}

fn starmap_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    it.no_kwargs("starmap", kw)?;
    it.check_args("starmap", args_of(a), 2, 2)?;
    let f = a[1].clone();
    let src = it.get_iter(&a[2])?;
    Ok(make(
        a,
        Box::new(move |it| {
            let Some(v) = it.iter_next(&src)? else { return Ok(None) };
            let args = it.iterate_to_vec(&v)?;
            Ok(Some(it.call(&f, args, Vec::new())?))
        }),
    ))
}

fn pairwise_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    it.no_kwargs("pairwise", kw)?;
    it.check_args("pairwise", args_of(a), 1, 1)?;
    let src = it.get_iter(&a[1])?;
    let mut prev: Option<Value> = None;
    let mut started = false;
    Ok(make(
        a,
        Box::new(move |it| {
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
        }),
    ))
}

fn batched_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("batched", args_of(a), kw, &["iterable", "n"], 2)?;
    let n = it.index_of(b[1].as_ref().unwrap())?;
    if n < 1 {
        return Err(it.value_error("n must be at least one"));
    }
    let src = it.get_iter(b[0].as_ref().unwrap())?;
    Ok(make(
        a,
        Box::new(move |it| {
            let mut batch = Vec::new();
            while (batch.len() as i64) < n {
                match it.iter_next(&src)? {
                    Some(v) => batch.push(v),
                    None => break,
                }
            }
            Ok(if batch.is_empty() { None } else { Some(Value::tuple(batch)) })
        }),
    ))
}

fn islice_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    it.no_kwargs("islice", kw)?;
    let args = args_of(a);
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
    Ok(make(
        a,
        Box::new(move |it| {
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
        }),
    ))
}

fn zip_longest_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let mut fill = Value::None;
    for (k, v) in kw {
        match k.as_str_kind().unwrap_or("") {
            "fillvalue" => fill = v.clone(),
            other => return Err(it.type_error(&format!("zip_longest() got an unexpected keyword argument '{}'", other))),
        }
    }
    let mut its: Vec<Option<Value>> = Vec::new();
    for v in args_of(a) {
        its.push(Some(it.get_iter(v)?));
    }
    let mut active = its.len();
    Ok(make(
        a,
        Box::new(move |it| {
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
        }),
    ))
}

// ---- tee ------------------------------------------------------------------------------------------

struct TeeShared {
    src: Value,
    buf: VecDeque<Value>,
    base: usize,
    positions: Vec<std::rc::Weak<std::cell::Cell<usize>>>,
}

fn tee_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("tee", a, kw, &["iterable", "n"], 1)?;
    let n = match &b[1] {
        Some(v) => it.index_of(v)?,
        None => 2,
    };
    if n < 0 {
        return Err(it.value_error("n must be >= 0"));
    }
    let src = it.get_iter(b[0].as_ref().unwrap())?;
    let shared = Rc::new(RefCell::new(TeeShared { src, buf: VecDeque::new(), base: 0, positions: Vec::new() }));
    let ty = tee_type(it);
    let mut out = Vec::new();
    for _ in 0..n {
        out.push(make_tee(&ty, &shared, 0));
    }
    Ok(Value::tuple(out))
}

fn tee_type(it: &mut Interp) -> Obj {
    let m = match dict_get_str(&it.modules, "itertools") {
        Some(Value::Obj(m)) => m,
        _ => unreachable!("itertools is loaded"),
    };
    let d = it.module_dict(&m);
    match dict_get_str(&d, "_tee") {
        Some(Value::Obj(t)) => t,
        _ => unreachable!(),
    }
}

fn make_tee(ty: &Obj, shared: &Rc<RefCell<TeeShared>>, start: usize) -> Value {
    let pos = Rc::new(std::cell::Cell::new(start));
    shared.borrow_mut().positions.push(Rc::downgrade(&pos));
    let shared = shared.clone();
    let f: Step = Box::new(move |it| {
        let p = pos.get();
        let (have, src) = {
            let s = shared.borrow();
            (p < s.base + s.buf.len(), s.src.clone())
        };
        let v = if have {
            let s = shared.borrow();
            s.buf[p - s.base].clone()
        } else {
            let Some(v) = it.iter_next(&src)? else { return Ok(None) };
            shared.borrow_mut().buf.push_back(v.clone());
            v
        };
        pos.set(p + 1);
        let mut s = shared.borrow_mut();
        s.positions.retain(|w| w.strong_count() > 0);
        let min = s.positions.iter().filter_map(|w| w.upgrade()).map(|c| c.get()).min().unwrap_or(p + 1);
        while s.base < min && !s.buf.is_empty() {
            s.buf.pop_front();
            s.base += 1;
        }
        Ok(Some(v))
    });
    Value::Obj(Object::with_cls(ty.clone(), Kind::Iter(RefCell::new(IterState::Native(f)))))
}

// ---- groupby --------------------------------------------------------------------------------------

struct GroupBy {
    src: Value,
    keyfunc: Value,
    tgtkey: Option<Value>,
    currkey: Option<Value>,
    currvalue: Option<Value>,
    grouper_id: u64,
}

fn groupby_step(it: &mut Interp, g: &Rc<RefCell<GroupBy>>) -> R<bool> {
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

fn groupby_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("groupby", args_of(a), kw, &["iterable", "key"], 1)?;
    let src = it.get_iter(b[0].as_ref().unwrap())?;
    let keyfunc = b[1].clone().unwrap_or(Value::None);
    let g = Rc::new(RefCell::new(GroupBy { src, keyfunc, tgtkey: None, currkey: None, currvalue: None, grouper_id: 0 }));
    let grouper_ty = groupby_grouper_type(it);
    let mut next_id = 0u64;
    Ok(make(
        a,
        Box::new(move |it| {
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
            let f: Step = Box::new(move |it| {
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
            let grouper = Value::Obj(Object::with_cls(grouper_ty.clone(), Kind::Iter(RefCell::new(IterState::Native(f)))));
            Ok(Some(Value::tuple(vec![key, grouper])))
        }),
    ))
}

fn groupby_grouper_type(it: &mut Interp) -> Obj {
    let m = match dict_get_str(&it.modules, "itertools") {
        Some(Value::Obj(m)) => m,
        _ => unreachable!("itertools is loaded"),
    };
    let d = it.module_dict(&m);
    match dict_get_str(&d, "_grouper") {
        Some(Value::Obj(t)) => t,
        _ => unreachable!(),
    }
}

// ---- combinatorics --------------------------------------------------------------------------------

fn product_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let mut repeat = 1i64;
    for (k, v) in kw {
        match k.as_str_kind().unwrap_or("") {
            "repeat" => repeat = it.index_of(v)?,
            other => return Err(it.type_error(&format!("product() got an unexpected keyword argument '{}'", other))),
        }
    }
    if repeat < 0 {
        return Err(it.value_error("repeat argument cannot be negative"));
    }
    let mut base: Vec<Vec<Value>> = Vec::new();
    for v in args_of(a) {
        base.push(it.iterate_to_vec(v)?);
    }
    let mut pools: Vec<Vec<Value>> = Vec::new();
    for _ in 0..repeat {
        pools.extend(base.iter().cloned());
    }
    let mut indices = vec![0usize; pools.len()];
    let mut started = false;
    let mut done = pools.iter().any(|p| p.is_empty());
    Ok(make(
        a,
        Box::new(move |_it| {
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
        }),
    ))
}

fn permutations_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("permutations", args_of(a), kw, &["iterable", "r"], 1)?;
    let pool = it.iterate_to_vec(b[0].as_ref().unwrap())?;
    let n = pool.len();
    let r = match &b[1] {
        Some(v) if !v.is_none() => {
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
    Ok(make(
        a,
        Box::new(move |_it| {
            if done {
                return Ok(None);
            }
            if !started {
                started = true;
                return Ok(Some(Value::tuple(indices[..r].iter().map(|&i| pool[i].clone()).collect())));
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
                    return Ok(Some(Value::tuple(indices[..r].iter().map(|&i| pool[i].clone()).collect())));
                }
            }
        }),
    ))
}

fn combinations_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("combinations", args_of(a), kw, &["iterable", "r"], 2)?;
    let pool = it.iterate_to_vec(b[0].as_ref().unwrap())?;
    let r = it.index_of(b[1].as_ref().unwrap())?;
    if r < 0 {
        return Err(it.value_error("r must be non-negative"));
    }
    let (n, r) = (pool.len(), r as usize);
    let mut indices: Vec<usize> = (0..r).collect();
    let mut started = false;
    let mut done = r > n;
    Ok(make(
        a,
        Box::new(move |_it| {
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
            Ok(Some(Value::tuple(indices.iter().map(|&i| pool[i].clone()).collect())))
        }),
    ))
}

fn cwr_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("combinations_with_replacement", args_of(a), kw, &["iterable", "r"], 2)?;
    let pool = it.iterate_to_vec(b[0].as_ref().unwrap())?;
    let r = it.index_of(b[1].as_ref().unwrap())?;
    if r < 0 {
        return Err(it.value_error("r must be non-negative"));
    }
    let (n, r) = (pool.len(), r as usize);
    let mut indices = vec![0usize; r];
    let mut started = false;
    let mut done = n == 0 && r > 0;
    Ok(make(
        a,
        Box::new(move |_it| {
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
            Ok(Some(Value::tuple(indices.iter().map(|&i| pool[i].clone()).collect())))
        }),
    ))
}

pub fn make_module(it: &mut Interp) -> Obj {
    let m = it.new_module("itertools");
    let d = it.module_dict(&m);
    it.register_module("itertools", &m);

    let simple: &[(&str, NativeFn)] = &[
        ("cycle", cycle_new),
        ("chain", chain_new),
        ("accumulate", accumulate_new),
        ("compress", compress_new),
        ("dropwhile", dropwhile_new),
        ("takewhile", takewhile_new),
        ("filterfalse", filterfalse_new),
        ("starmap", starmap_new),
        ("pairwise", pairwise_new),
        ("batched", batched_new),
        ("islice", islice_new),
        ("zip_longest", zip_longest_new),
        ("groupby", groupby_new),
        ("product", product_new),
        ("permutations", permutations_new),
        ("combinations", combinations_new),
        ("combinations_with_replacement", cwr_new),
    ];
    for (name, f) in simple {
        let ty = tool_type(it, name);
        it.reg_new(&ty, *f);
        reg_iterator(it, &ty);
        if *name == "chain" {
            it.reg_class(&ty, "from_iterable", chain_from_iterable);
        }
        set_type(&d, name, &ty);
    }
    for name in ["_tee", "_grouper"] {
        let ty = tool_type(it, name);
        reg_iterator(it, &ty);
        set_type(&d, name, &ty);
    }
    let tee_fn = it.new_native("tee", tee_new, false);
    dict_set_str(&d, "tee", tee_fn);

    let count = new_type(it, "itertools", "count", None, Layout::Other);
    it.reg_new(&count, count_new);
    it.reg(&count, "__iter__", opaque_iter_self);
    it.reg(&count, "__next__", count_next);
    it.reg(&count, "__repr__", count_repr);
    set_type(&d, "count", &count);

    let repeat = new_type(it, "itertools", "repeat", None, Layout::Other);
    it.reg_new(&repeat, repeat_new);
    it.reg(&repeat, "__iter__", opaque_iter_self);
    it.reg(&repeat, "__next__", repeat_next);
    it.reg(&repeat, "__repr__", repeat_repr);
    it.reg(&repeat, "__length_hint__", repeat_len);
    set_type(&d, "repeat", &repeat);
    m
}
