//! `_testcapi` watcher helpers (`Modules/_testcapi/watchers.c`) on the engine's watchers
//! ([`crate::watch`]).

use crate::object::*;
use crate::vm::Interp;
use crate::watch::{self, Callback, DictEvent, Domain, Event, FuncEvent};
use std::rc::Rc;

const TEST_CODE_WATCHERS: usize = 2;
const TEST_FUNC_WATCHERS: usize = 2;

#[derive(Default)]
struct WatchState {
    dict_events: Option<Value>,
    dict_installed: i64,
    type_events: Option<Value>,
    type_installed: i64,
    code_ids: [Option<usize>; TEST_CODE_WATCHERS],
    code_created: [i64; TEST_CODE_WATCHERS],
    code_destroyed: [i64; TEST_CODE_WATCHERS],
    func_ids: [Option<usize>; TEST_FUNC_WATCHERS],
    func_watchers: [Option<Value>; TEST_FUNC_WATCHERS],
}

fn boom(it: &mut Interp) -> Obj {
    it.new_exc_str("RuntimeError", "boom!")
}

fn append_event(it: &mut Interp, events: &Option<Value>, item: Value) -> R<()> {
    match events {
        Some(Value::Obj(l)) => {
            if let Kind::List(items) = &l.kind {
                items.borrow_mut().push(item);
            }
            Ok(())
        }
        _ => Err(it.new_exc_str("RuntimeError", "no watchers active")),
    }
}

fn dict_callback(kind: i64) -> Callback {
    Rc::new(move |it: &mut Interp, ev: &Event| {
        let Event::Dict { event, key, value } = ev else { return Ok(()) };
        if kind == 1 {
            return Err(boom(it));
        }
        let events = it.native_state::<WatchState>().dict_events.clone();
        let msg = if kind == 2 {
            "second".to_string()
        } else {
            let show = |it: &mut Interp, v: &Option<Value>| match v {
                Some(v) => it.str_of(v),
                None => Ok("None".to_string()),
            };
            match event {
                DictEvent::Cleared => "clear".to_string(),
                DictEvent::Deallocated => "dealloc".to_string(),
                DictEvent::Cloned => "clone".to_string(),
                DictEvent::Added => format!("new:{}:{}", show(it, key)?, show(it, value)?),
                DictEvent::Modified => format!("mod:{}:{}", show(it, key)?, show(it, value)?),
                DictEvent::Deleted => format!("del:{}", show(it, key)?),
            }
        };
        append_event(it, &events, Value::string(msg))
    })
}

fn type_callback(kind: i64) -> Callback {
    Rc::new(move |it: &mut Interp, ev: &Event| {
        let Event::Type(ty) = ev else { return Ok(()) };
        if kind == 1 {
            return Err(boom(it));
        }
        let events = it.native_state::<WatchState>().type_events.clone();
        let item = if kind == 2 { Value::list(vec![Value::Obj(ty.clone())]) } else { Value::Obj(ty.clone()) };
        append_event(it, &events, item)
    })
}

fn code_callback(which: usize) -> Callback {
    Rc::new(move |it: &mut Interp, ev: &Event| {
        let Event::Code { create } = ev else { return Ok(()) };
        let st = it.native_state::<WatchState>();
        if *create {
            st.code_created[which] += 1;
        } else {
            st.code_destroyed[which] += 1;
        }
        Ok(())
    })
}

fn error_callback() -> Callback {
    Rc::new(|it: &mut Interp, _: &Event| Err(boom(it)))
}

fn noop_callback() -> Callback {
    Rc::new(|_: &mut Interp, _: &Event| Ok(()))
}

fn func_callback(which: usize) -> Callback {
    Rc::new(move |it: &mut Interp, ev: &Event| {
        let Event::Func { event, func, id, value } = ev else { return Ok(()) };
        let watcher = it.native_state::<WatchState>().func_watchers[which].clone().unwrap_or(Value::None);
        let subject = match (event, func) {
            (FuncEvent::Destroy, _) | (_, None) => Value::Int(*id as i64),
            (_, Some(f)) => Value::Obj(f.clone()),
        };
        it.call(&watcher, vec![Value::Int(*event as i64), subject, value.clone().unwrap_or(Value::None)], Vec::new())?;
        Ok(())
    })
}

fn allocate_all(it: &mut Interp, domain: Domain) -> R<()> {
    let mut ids = Vec::new();
    let mut failure = None;
    for _ in 0..=watch::MAX_WATCHERS {
        match watch::add(it, domain, noop_callback()) {
            Ok(id) => ids.push(id),
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }
    for id in ids {
        watch::clear(it, domain, id as i64)?;
    }
    match failure {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

#[lumen_bind::module(name = "_testcapi")]
pub mod watchersm {
    use super::*;

    #[constant(name = "PYFUNC_EVENT_CREATE")]
    const PYFUNC_EVENT_CREATE: i64 = 0;
    #[constant(name = "PYFUNC_EVENT_DESTROY")]
    const PYFUNC_EVENT_DESTROY: i64 = 1;
    #[constant(name = "PYFUNC_EVENT_MODIFY_CODE")]
    const PYFUNC_EVENT_MODIFY_CODE: i64 = 2;
    #[constant(name = "PYFUNC_EVENT_MODIFY_DEFAULTS")]
    const PYFUNC_EVENT_MODIFY_DEFAULTS: i64 = 3;
    #[constant(name = "PYFUNC_EVENT_MODIFY_KWDEFAULTS")]
    const PYFUNC_EVENT_MODIFY_KWDEFAULTS: i64 = 4;

    #[op]
    fn add_dict_watcher(it: &mut Interp, kind: i64) -> R<i64> {
        let id = watch::add(it, Domain::Dict, dict_callback(kind))?;
        let st = it.native_state::<WatchState>();
        if st.dict_installed == 0 {
            st.dict_events = Some(Value::list(Vec::new()));
        }
        st.dict_installed += 1;
        Ok(id as i64)
    }

    #[op]
    fn clear_dict_watcher(it: &mut Interp, watcher_id: i64) -> R<()> {
        watch::clear(it, Domain::Dict, watcher_id)?;
        let st = it.native_state::<WatchState>();
        st.dict_installed -= 1;
        if st.dict_installed == 0 {
            st.dict_events = None;
        }
        Ok(())
    }

    #[op]
    fn watch_dict(it: &mut Interp, watcher_id: i64, dict: &Value) -> R<()> {
        watch::watch_dict(it, watcher_id, dict)
    }

    #[op]
    fn unwatch_dict(it: &mut Interp, watcher_id: i64, dict: &Value) -> R<()> {
        watch::unwatch_dict(it, watcher_id, dict)
    }

    #[op]
    fn get_dict_watcher_events(it: &mut Interp) -> R<Value> {
        it.flush_watchers();
        match it.native_state::<WatchState>().dict_events.clone() {
            Some(v) => Ok(v),
            None => Err(it.new_exc_str("RuntimeError", "no watchers active")),
        }
    }

    #[op]
    fn add_type_watcher(it: &mut Interp, kind: i64) -> R<i64> {
        let id = watch::add(it, Domain::Type, type_callback(kind))?;
        let st = it.native_state::<WatchState>();
        if st.type_installed == 0 {
            st.type_events = Some(Value::list(Vec::new()));
        }
        st.type_installed += 1;
        Ok(id as i64)
    }

    #[op]
    fn clear_type_watcher(it: &mut Interp, watcher_id: i64) -> R<()> {
        watch::clear(it, Domain::Type, watcher_id)?;
        let st = it.native_state::<WatchState>();
        st.type_installed -= 1;
        if st.type_installed == 0 {
            st.type_events = None;
        }
        Ok(())
    }

    #[op]
    fn get_type_modified_events(it: &mut Interp) -> R<Value> {
        match it.native_state::<WatchState>().type_events.clone() {
            Some(v) => Ok(v),
            None => Err(it.new_exc_str("RuntimeError", "no watchers active")),
        }
    }

    #[op]
    fn watch_type(it: &mut Interp, watcher_id: i64, ty: &Value) -> R<()> {
        watch::watch_type(it, watcher_id, ty)
    }

    #[op]
    fn unwatch_type(it: &mut Interp, watcher_id: i64, ty: &Value) -> R<()> {
        watch::unwatch_type(it, watcher_id, ty)
    }

    #[op]
    fn add_code_watcher(it: &mut Interp, which_watcher: i64) -> R<i64> {
        let which = match which_watcher {
            0 | 1 => which_watcher as usize,
            2 => {
                return Ok(watch::add(it, Domain::Code, error_callback())? as i64);
            }
            n => return Err(it.value_error(&format!("invalid watcher {n}"))),
        };
        let id = watch::add(it, Domain::Code, code_callback(which))?;
        let st = it.native_state::<WatchState>();
        st.code_ids[which] = Some(id);
        st.code_created[which] = 0;
        st.code_destroyed[which] = 0;
        Ok(id as i64)
    }

    #[op]
    fn clear_code_watcher(it: &mut Interp, watcher_id: i64) -> R<()> {
        watch::clear(it, Domain::Code, watcher_id)?;
        let st = it.native_state::<WatchState>();
        for i in 0..TEST_CODE_WATCHERS {
            if st.code_ids[i] == Some(watcher_id as usize) {
                st.code_ids[i] = None;
                st.code_created[i] = 0;
                st.code_destroyed[i] = 0;
            }
        }
        Ok(())
    }

    #[op]
    fn get_code_watcher_num_created_events(it: &mut Interp, watcher_id: i64) -> i64 {
        it.flush_watchers();
        it.native_state::<WatchState>().code_created.get(watcher_id as usize).copied().unwrap_or(0)
    }

    #[op]
    fn get_code_watcher_num_destroyed_events(it: &mut Interp, watcher_id: i64) -> i64 {
        it.flush_watchers();
        it.native_state::<WatchState>().code_destroyed.get(watcher_id as usize).copied().unwrap_or(0)
    }

    #[op]
    fn allocate_too_many_code_watchers(it: &mut Interp) -> R<()> {
        allocate_all(it, Domain::Code)
    }

    #[op]
    fn add_func_watcher(it: &mut Interp, func: &Value) -> R<i64> {
        if !matches!(func, Value::Obj(o) if matches!(o.kind, Kind::Function(_))) {
            return Err(it.type_error("'func' must be a function"));
        }
        let Some(idx) = it.native_state::<WatchState>().func_ids.iter().position(Option::is_none) else {
            return Err(it.new_exc_str("RuntimeError", "no free test watchers"));
        };
        let id = watch::add(it, Domain::Func, func_callback(idx))?;
        let st = it.native_state::<WatchState>();
        st.func_ids[idx] = Some(id);
        st.func_watchers[idx] = Some(func.clone());
        Ok(id as i64)
    }

    #[op]
    fn clear_func_watcher(it: &mut Interp, watcher_id: i64) -> R<()> {
        if watcher_id < i64::from(i32::MIN) || watcher_id > i64::from(i32::MAX) {
            return Err(it.value_error("invalid watcher ID"));
        }
        watch::clear(it, Domain::Func, watcher_id)?;
        let st = it.native_state::<WatchState>();
        if let Some(idx) = st.func_ids.iter().position(|i| *i == Some(watcher_id as usize)) {
            st.func_watchers[idx] = None;
            st.func_ids[idx] = None;
        }
        Ok(())
    }

    #[op]
    fn allocate_too_many_func_watchers(it: &mut Interp) -> R<()> {
        allocate_all(it, Domain::Func)
    }

    #[op]
    fn set_func_defaults_via_capi(it: &mut Interp, func: &Value, defaults: &Value) -> R<()> {
        if !matches!(func, Value::Obj(o) if matches!(o.kind, Kind::Function(_))) {
            return Err(it.new_exc_str("SystemError", "bad argument to internal function"));
        }
        if !defaults.is_none() && defaults.tuple_items().is_none() {
            return Err(it.new_exc_str("SystemError", "non-tuple default args"));
        }
        let name = Value::str("__defaults__");
        let Value::Obj(n) = name else { return Ok(()) };
        it.set_attr(func, &n, defaults.clone())
    }

    #[op]
    fn set_func_kwdefaults_via_capi(it: &mut Interp, func: &Value, defaults: &Value) -> R<()> {
        if !matches!(func, Value::Obj(o) if matches!(o.kind, Kind::Function(_))) {
            return Err(it.new_exc_str("SystemError", "bad argument to internal function"));
        }
        let is_dict = matches!(defaults, Value::Obj(o) if matches!(o.kind, Kind::Dict(_)));
        if !defaults.is_none() && !is_dict {
            return Err(it.new_exc_str("SystemError", "non-dict keyword only default args"));
        }
        let name = Value::str("__kwdefaults__");
        let Value::Obj(n) = name else { return Ok(()) };
        it.set_attr(func, &n, defaults.clone())
    }
}
