//! `sys.monitoring` (PEP 669): tool registration, callbacks and event selection over the
//! delivery engine in `crate::trace`.

/// Monitoring tools: events, callbacks and their per-code selection.
#[lumen_bind::module(name = "sys.monitoring")]
pub mod sys_monitoring {
    use crate::object::*;
    use crate::trace::{self, ev};
    use crate::vm::{dict_set_str, Interp};

    #[constant(name = "DEBUGGER_ID")]
    const DEBUGGER_ID: i64 = 0;
    #[constant(name = "COVERAGE_ID")]
    const COVERAGE_ID: i64 = 1;
    #[constant(name = "PROFILER_ID")]
    const PROFILER_ID: i64 = 2;
    #[constant(name = "OPTIMIZER_ID")]
    const OPTIMIZER_ID: i64 = 5;

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        let mut items = vec![("NO_EVENTS", Value::Int(0))];
        for (i, name) in trace::EVENT_NAMES.iter().enumerate() {
            items.push((*name, Value::Int(1 << i)));
        }
        items.push(("BRANCH", Value::Int(trace::BRANCH_ALIAS as i64)));
        let events = it.new_namespace(items);
        dict_set_str(&d, "events", events);
        let (disable, missing) = it.monitoring_sentinels();
        dict_set_str(&d, "DISABLE", disable);
        dict_set_str(&d, "MISSING", missing);
    }

    fn valid_tool(it: &mut Interp, tool_id: i64) -> R<usize> {
        if !(0..trace::USER_TOOLS as i64).contains(&tool_id) {
            return Err(it.value_error(&format!("invalid tool {tool_id} (must be between 0 and 5)")));
        }
        Ok(tool_id as usize)
    }

    fn used_tool(it: &mut Interp, tool_id: i64) -> R<usize> {
        let tool = valid_tool(it, tool_id)?;
        if it.mon.tools[tool].name.is_none() {
            return Err(it.value_error(&format!("tool {tool_id} is not in use")));
        }
        Ok(tool)
    }

    /// Check an event set for `set_events`: a CALL implies its two ancillary events, which cannot
    /// be selected alone.
    fn checked_events(it: &mut Interp, events: i64, local: bool) -> R<u32> {
        let alias = trace::BRANCH_ALIAS as i64;
        let events = if events & alias != 0 { (events & !alias) | (ev::BRANCH_LEFT | ev::BRANCH_RIGHT) as i64 } else { events };
        let limit = if local { 1 << 11 } else { 1 << trace::EVENT_COUNT };
        if events < 0 || events >= limit {
            let what = if local { "invalid local event set" } else { "invalid event set" };
            return Err(it.value_error(&format!("{what} {events:#x}")));
        }
        let events = events as u32;
        let call_events = ev::CALL | trace::C_RETURN_EVENTS;
        if events & trace::C_RETURN_EVENTS != 0 && events & call_events != call_events {
            return Err(it.value_error("cannot set C_RETURN or C_RAISE events independently"));
        }
        Ok(trace::expand_call(events))
    }

    /// Register a tool: use_tool_id(tool_id, name)
    #[op]
    fn use_tool_id(it: &mut Interp, tool_id: i64, name: &Value) -> R<()> {
        let tool = valid_tool(it, tool_id)?;
        let Some(name) = name.as_str() else {
            return Err(it.value_error("tool name must be a str"));
        };
        if it.mon.tools[tool].name.is_some() {
            return Err(it.value_error(&format!("tool {tool_id} is already in use")));
        }
        it.mon.tools[tool].name = Some(name.into());
        Ok(())
    }

    /// Unregister all events and callbacks associated with a tool ID.
    #[op]
    fn clear_tool_id(it: &mut Interp, tool_id: i64) -> R<()> {
        let tool = valid_tool(it, tool_id)?;
        clear_tool(it, tool);
        Ok(())
    }

    fn clear_tool(it: &mut Interp, tool: usize) {
        let t = &mut it.mon.tools[tool];
        t.events = 0;
        t.callbacks = std::array::from_fn(|_| Value::None);
        for l in it.mon.locals.iter_mut() {
            l.events[tool] = 0;
        }
        it.mon.disabled.retain(|k| k.0 as usize != tool);
        it.mon.refresh();
    }

    /// The tools listening to each event interpreter-wide, as a bitmask by event name.
    #[op]
    fn _all_events(it: &mut Interp) -> Value {
        let out = it.new_dict();
        for (i, name) in trace::EVENT_NAMES.iter().enumerate() {
            let bit = 1u32 << i;
            if bit & trace::C_RETURN_EVENTS != 0 {
                continue;
            }
            let mask = (0..trace::USER_TOOLS).filter(|&t| it.mon.tools[t].events & bit != 0).fold(0i64, |m, t| m | 1 << t);
            if mask != 0 {
                dict_set_str(&out, name, Value::Int(mask));
            }
        }
        Value::Obj(out)
    }

    /// Release a tool id and everything it enabled.
    #[op]
    fn free_tool_id(it: &mut Interp, tool_id: i64) -> R<()> {
        let tool = valid_tool(it, tool_id)?;
        clear_tool(it, tool);
        it.mon.tools[tool].name = None;
        Ok(())
    }

    /// The name a tool id was registered with, or None.
    #[op]
    fn get_tool(it: &mut Interp, tool_id: i64) -> R<Value> {
        let tool = valid_tool(it, tool_id)?;
        Ok(match &it.mon.tools[tool].name {
            Some(n) => Value::str(n),
            None => Value::None,
        })
    }

    // Register `func` as the callback of a single event of a tool; returns the previous one.
    #[op]
    fn register_callback(it: &mut Interp, tool_id: i64, event: i64, func: &Value) -> R<Value> {
        let tool = valid_tool(it, tool_id)?;
        if event == trace::BRANCH_ALIAS as i64 {
            let left = register_callback(it, tool_id, ev::BRANCH_LEFT as i64, func)?;
            register_callback(it, tool_id, ev::BRANCH_RIGHT as i64, func)?;
            return Ok(left);
        }
        let idx = if event > 0 { 63 - event.leading_zeros() as i64 } else { -1 };
        if idx < 0 || idx >= trace::EVENT_COUNT as i64 {
            return Err(it.value_error(&format!("invalid event {event}")));
        }
        Ok(std::mem::replace(&mut it.mon.tools[tool].callbacks[idx as usize], func.clone()))
    }

    /// The events a tool listens to in every code object.
    #[op]
    fn get_events(it: &mut Interp, tool_id: i64) -> R<i64> {
        let tool = valid_tool(it, tool_id)?;
        Ok((it.mon.tools[tool].events & !trace::C_RETURN_EVENTS) as i64)
    }

    /// Select the events a tool listens to in every code object.
    #[op]
    fn set_events(it: &mut Interp, tool_id: i64, event_set: i64) -> R<()> {
        let tool = used_tool(it, tool_id)?;
        let events = checked_events(it, event_set, false)?;
        it.mon.tools[tool].events = events;
        it.mon.refresh();
        Ok(())
    }

    fn code_of(it: &mut Interp, code: &Value, func: &str) -> R<std::rc::Rc<crate::bytecode::Code>> {
        if let Value::Obj(o) = code {
            if let Kind::Code(c) = &o.kind {
                return Ok(c.clone());
            }
        }
        let t = it.type_name_of(code);
        Err(it.type_error(&format!("{func}() argument 2 must be code, not {t}")))
    }

    /// The events a tool listens to in one code object.
    #[op]
    fn get_local_events(it: &mut Interp, tool_id: i64, code: &Value) -> R<i64> {
        let tool = valid_tool(it, tool_id)?;
        let code = code_of(it, code, "get_local_events")?;
        Ok(match it.mon.locals.iter().find(|l| std::rc::Rc::ptr_eq(&l.code, &code)) {
            Some(l) => (l.events[tool] & !trace::C_RETURN_EVENTS) as i64,
            None => 0,
        })
    }

    /// Select the events a tool listens to in one code object.
    #[op]
    fn set_local_events(it: &mut Interp, tool_id: i64, code: &Value, event_set: i64) -> R<()> {
        let tool = used_tool(it, tool_id)?;
        let code = code_of(it, code, "set_local_events")?;
        let events = checked_events(it, event_set, true)?;
        it.mon.local_slot(&code).events[tool] = events;
        it.mon.pin(&code);
        it.mon.refresh();
        Ok(())
    }

    /// Re-enable every event whose callback returned DISABLE.
    #[op]
    fn restart_events(it: &mut Interp) {
        it.mon.disabled.clear();
    }
}
