//! Small native modules: `builtins`, `gc`, `atexit`.

use super::native::*;
use crate::object::*;
use crate::vm::*;

pub fn make_builtins(it: &mut Interp) -> Obj {
    Object::with_dict(Kind::Module, it.builtins.clone())
}

// ---- gc -----------------------------------------------------------------------------------------

fn gc_collect(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    it.run_weak_callbacks();
    Ok(Value::Int(0))
}

fn gc_enable(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    it.gc_enabled = true;
    Ok(Value::None)
}

fn gc_disable(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    it.gc_enabled = false;
    Ok(Value::None)
}

fn gc_isenabled(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Bool(it.gc_enabled))
}

fn gc_get_count(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::tuple(vec![Value::Int(0), Value::Int(0), Value::Int(0)]))
}

fn gc_get_threshold(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::tuple(vec![Value::Int(700), Value::Int(10), Value::Int(10)]))
}

fn gc_none(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::None)
}

fn gc_empty_list(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::list(Vec::new()))
}

fn gc_zero(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Int(0))
}

fn gc_false(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Bool(false))
}

fn gc_get_stats(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    let mut out = Vec::new();
    for _ in 0..3 {
        let d = it.new_dict();
        for k in ["collections", "collected", "uncollectable"] {
            dict_set_str(&d, k, Value::Int(0));
        }
        out.push(Value::Obj(d));
    }
    Ok(Value::list(out))
}

pub fn make_gc(it: &mut Interp) -> Obj {
    let m = it.new_module("gc");
    let d = it.module_dict(&m);
    let fns: &[(&'static str, NativeFn)] = &[
        ("collect", gc_collect),
        ("enable", gc_enable),
        ("disable", gc_disable),
        ("isenabled", gc_isenabled),
        ("get_count", gc_get_count),
        ("get_threshold", gc_get_threshold),
        ("set_threshold", gc_none),
        ("set_debug", gc_none),
        ("get_debug", gc_zero),
        ("freeze", gc_none),
        ("unfreeze", gc_none),
        ("get_freeze_count", gc_zero),
        ("get_objects", gc_empty_list),
        ("get_referrers", gc_empty_list),
        ("get_referents", gc_empty_list),
        ("get_stats", gc_get_stats),
        ("is_tracked", gc_false),
        ("is_finalized", gc_false),
    ];
    for (n, f) in fns {
        set_fn(it, &d, n, *f);
    }
    dict_set_str(&d, "garbage", Value::list(Vec::new()));
    dict_set_str(&d, "callbacks", Value::list(Vec::new()));
    for (n, v) in [("DEBUG_STATS", 1), ("DEBUG_COLLECTABLE", 2), ("DEBUG_UNCOLLECTABLE", 4), ("DEBUG_SAVEALL", 32), ("DEBUG_LEAK", 38)] {
        dict_set_str(&d, n, Value::Int(v));
    }
    m
}

// ---- atexit -------------------------------------------------------------------------------------

fn atexit_register(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    if a.is_empty() {
        return Err(it.type_error("register() takes at least 1 argument (0 given)"));
    }
    if !it.is_callable(&a[0]) {
        return Err(it.type_error("the first argument must be callable"));
    }
    it.atexit.push((a[0].clone(), a[1..].to_vec(), kw.to_vec()));
    Ok(a[0].clone())
}

fn atexit_unregister(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("unregister", a, 1, 1)?;
    let mut keep = Vec::new();
    for entry in std::mem::take(&mut it.atexit) {
        if !it.values_eq(&entry.0, &a[0])? {
            keep.push(entry);
        }
    }
    it.atexit = keep;
    Ok(Value::None)
}

fn atexit_clear(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    it.atexit.clear();
    Ok(Value::None)
}

fn atexit_ncallbacks(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Int(it.atexit.len() as i64))
}

fn atexit_run(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    it.run_atexit();
    Ok(Value::None)
}

impl Interp {
    /// Runs the registered exit functions, last registered first.
    pub fn run_atexit(&mut self) {
        while let Some((f, args, kw)) = self.atexit.pop() {
            if let Err(e) = self.call(&f, args, kw) {
                if self.exc_is(&e, "SystemExit") {
                    continue;
                }
                self.flush_out();
                let repr = self.repr_of(&f).unwrap_or_default();
                self.write_stderr(&format!("Exception ignored in atexit callback {}:\n", repr));
                let text = self.format_exception(&e);
                self.write_stderr(&text);
            }
        }
    }
}

pub fn make_atexit(it: &mut Interp) -> Obj {
    let m = it.new_module("atexit");
    let d = it.module_dict(&m);
    set_fn(it, &d, "register", atexit_register);
    set_fn(it, &d, "unregister", atexit_unregister);
    set_fn(it, &d, "_clear", atexit_clear);
    set_fn(it, &d, "_ncallbacks", atexit_ncallbacks);
    set_fn(it, &d, "_run_exitfuncs", atexit_run);
    m
}
