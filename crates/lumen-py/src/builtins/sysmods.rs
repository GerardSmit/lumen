//! Small native modules: `builtins`, `gc`, `atexit`.

use crate::object::*;
use crate::vm::*;

pub fn make_builtins(it: &mut Interp) -> Obj {
    Object::with_dict(Kind::Module, it.builtins.clone())
}

// ---- gc -----------------------------------------------------------------------------------------

/// This module provides access to the garbage collector for reference cycles.
///
/// enable() -- Enable automatic garbage collection.
/// disable() -- Disable automatic garbage collection.
/// isenabled() -- Returns true if automatic collection is enabled.
/// collect() -- Do a full collection right now.
/// get_count() -- Return the current collection counts.
/// get_stats() -- Return list of dictionaries containing per-generation stats.
/// set_debug() -- Set debugging flags.
/// get_debug() -- Get debugging flags.
/// set_threshold() -- Set the collection thresholds.
/// get_threshold() -- Return the current the collection thresholds.
/// get_objects() -- Return a list of all objects tracked by the collector.
/// is_tracked() -- Returns true if a given object is tracked.
/// is_finalized() -- Returns true if a given object has been already finalized.
/// get_referrers() -- Return the list of objects that refer to an object.
/// get_referents() -- Return the list of objects that an object refers to.
/// freeze() -- Freeze all tracked objects and ignore them for future collections.
/// unfreeze() -- Unfreeze all objects in the permanent generation.
/// get_freeze_count() -- Return the number of objects in the permanent generation.
///
#[lumen_bind::module(name = "gc")]
pub mod gc {
    use super::*;
    use crate::gc::GcState;

    #[constant(name = "DEBUG_STATS")]
    const DEBUG_STATS: i64 = 1;
    #[constant(name = "DEBUG_COLLECTABLE")]
    const DEBUG_COLLECTABLE: i64 = 2;
    #[constant(name = "DEBUG_UNCOLLECTABLE")]
    const DEBUG_UNCOLLECTABLE: i64 = 4;
    #[constant(name = "DEBUG_SAVEALL")]
    const DEBUG_SAVEALL: i64 = 32;
    #[constant(name = "DEBUG_LEAK")]
    const DEBUG_LEAK: i64 = 38;

    fn generation(it: &mut Interp, g: i64) -> R<usize> {
        if !(0..3).contains(&g) {
            return Err(it.value_error("invalid generation"));
        }
        Ok(g as usize)
    }

    /// Run the garbage collector.
    ///
    /// With no arguments, run a full collection.  The optional argument
    /// may be an integer specifying which generation to collect.  A ValueError
    /// is raised if the generation number is invalid.
    ///
    /// The number of unreachable objects is returned.
    #[op]
    fn collect(it: &mut Interp, #[kw] #[default(2)] generation: i64) -> R<i64> {
        let g = self::generation(it, generation)?;
        Ok(it.gc_collect_checked(g) as i64)
    }

    /// Enable automatic garbage collection.
    #[op]
    fn enable() {
        crate::gc::set_enabled(true);
    }

    /// Disable automatic garbage collection.
    #[op]
    fn disable() {
        crate::gc::set_enabled(false);
    }

    /// Returns true if automatic garbage collection is enabled.
    #[op]
    fn isenabled() -> bool {
        crate::gc::enabled()
    }

    /// Return a three-tuple of the current collection counts.
    #[op]
    fn get_count() -> (i64, i64, i64) {
        let c = crate::gc::counts();
        (c[0] as i64, c[1] as i64, c[2] as i64)
    }

    /// Return the current collection thresholds.
    #[op]
    fn get_threshold() -> (i64, i64, i64) {
        let t = crate::gc::thresholds();
        (t[0] as i64, t[1] as i64, t[2] as i64)
    }

    /// set_threshold(threshold0, [threshold1, threshold2]) -> None
    ///
    /// Sets the collection thresholds.  Setting threshold0 to zero disables
    /// collection.
    ///
    #[op(hint(py(arg_style = "parse", text_signature = "")))]
    fn set_threshold(threshold0: i32, threshold1: lumen_bind::Passed<i32>, threshold2: lumen_bind::Passed<i32>) {
        let t = |v: i32| v.max(0) as usize;
        crate::gc::set_thresholds([Some(t(threshold0)), threshold1.0.map(t), threshold2.0.map(t)]);
    }

    /// Set the garbage collection debugging flags.
    ///
    ///   flags
    ///     An integer that can have the following bits turned on:
    ///       DEBUG_STATS - Print statistics during collection.
    ///       DEBUG_COLLECTABLE - Print collectable objects found.
    ///       DEBUG_UNCOLLECTABLE - Print unreachable but uncollectable objects
    ///         found.
    ///       DEBUG_SAVEALL - Save objects to gc.garbage rather than freeing them.
    ///       DEBUG_LEAK - Debug leaking programs (everything but STATS).
    ///
    /// Debugging information is written to sys.stderr.
    #[op]
    fn set_debug(it: &mut Interp, flags: i32) {
        it.native_state::<GcState>().debug = flags as u32;
    }

    /// Get the garbage collection debugging flags.
    #[op]
    fn get_debug(it: &mut Interp) -> i64 {
        it.native_state::<GcState>().debug as i64
    }

    /// Freeze all current tracked objects and ignore them for future collections.
    ///
    /// This can be used before a POSIX fork() call to make the gc copy-on-write friendly.
    /// Note: collection before a POSIX fork() call may free pages for future allocation
    /// which can cause copy-on-write.
    #[op]
    fn freeze() {
        crate::gc::freeze();
    }

    /// Unfreeze all objects in the permanent generation.
    ///
    /// Put all objects in the permanent generation back into oldest generation.
    #[op]
    fn unfreeze() {
        crate::gc::unfreeze();
    }

    /// Return the number of objects in the permanent generation.
    #[op]
    fn get_freeze_count() -> i64 {
        crate::gc::generation_sizes().1 as i64
    }

    /// Return a list of objects tracked by the collector (excluding the list returned).
    ///
    ///   generation
    ///     Generation to extract the objects from.
    ///
    /// If generation is not None, return only the objects tracked by the collector
    /// that are in that generation.
    #[op]
    fn get_objects(it: &mut Interp, #[kw] generation: Option<i64>) -> R<Vec<Value>> {
        let g = match generation {
            Some(g) if g >= 3 => return Err(it.value_error("generation parameter must be less than the number of available generations (3)")),
            Some(g) if g < 0 => return Err(it.value_error("generation parameter cannot be negative")),
            Some(g) => Some(g as usize),
            None => None,
        };
        Ok(crate::gc::objects(g).into_iter().map(Value::Obj).collect())
    }

    /// get_referrers(*objs) -> list
    /// Return the list of objects that directly refer to any of objs.
    #[op(hint(py(text_signature = "")))]
    fn get_referrers(#[varargs] objs: &[Value]) -> Vec<Value> {
        crate::gc::referrers(objs).into_iter().map(Value::Obj).collect()
    }

    /// get_referents(*objs) -> list
    /// Return the list of objects that are directly referred to by objs.
    #[op(hint(py(text_signature = "")))]
    fn get_referents(#[varargs] objs: &[Value]) -> Vec<Value> {
        let mut out = Vec::new();
        for v in objs {
            if let Value::Obj(o) = v {
                if o.gc.idx.get() != 0 {
                    out.extend(crate::gc_traverse::referents(o).into_iter().map(Value::Obj));
                }
            }
        }
        out
    }

    /// Return a list of dictionaries containing per-generation statistics.
    #[op]
    fn get_stats(it: &mut Interp) -> Vec<Value> {
        crate::gc::stats()
            .iter()
            .map(|s| {
                let d = it.new_dict();
                dict_set_str(&d, "collections", Value::Int(s.collections as i64));
                dict_set_str(&d, "collected", Value::Int(s.collected as i64));
                dict_set_str(&d, "uncollectable", Value::Int(s.uncollectable as i64));
                Value::Obj(d)
            })
            .collect()
    }

    /// Returns true if the object is tracked by the garbage collector.
    ///
    /// Simple atomic objects will return false.
    #[op]
    fn is_tracked(obj: &Value) -> bool {
        matches!(obj, Value::Obj(o) if crate::gc::is_tracked(o))
    }

    /// Returns true if the object has been already finalized by the GC.
    #[op]
    fn is_finalized(obj: &Value) -> bool {
        matches!(obj, Value::Obj(o) if crate::gc::is_finalized(o))
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        let garbage = Value::list(Vec::new());
        let callbacks = Value::list(Vec::new());
        dict_set_str(&d, "garbage", garbage.clone());
        dict_set_str(&d, "callbacks", callbacks.clone());
        let st = it.native_state::<GcState>();
        st.garbage = Some(garbage);
        st.callbacks = Some(callbacks);
    }
}

// ---- atexit -------------------------------------------------------------------------------------

/// allow programmer to define multiple exit functions to be executed
/// upon normal program termination.
///
/// Two public functions, register and unregister, are defined.
///
#[lumen_bind::module(name = "atexit")]
pub mod atexit {
    use super::*;
    use crate::bind::KwArgs;

    /// register(func, *args, **kwargs) -> func
    ///
    /// Register a function to be executed upon normal program termination
    ///
    ///     func - function to be called at exit
    ///     args - optional arguments to pass to func
    ///     kwargs - optional keyword arguments to pass to func
    ///
    ///     func is returned to facilitate usage as a decorator.
    #[op(hint(py(arg_style = "parse", text_signature = "")))]
    fn register(it: &mut Interp, func: &Value, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
        if !it.is_callable(func) {
            return Err(it.type_error("the first argument must be callable"));
        }
        it.atexit.push((func.clone(), args.to_vec(), kwargs.to_vec()));
        Ok(func.clone())
    }

    /// unregister(func) -> None
    ///
    /// Unregister an exit function which was previously registered using
    /// atexit.register
    ///
    ///     func - function to be unregistered
    #[op(hint(py(text_signature = "")))]
    fn unregister(it: &mut Interp, func: &Value) -> R<()> {
        let mut keep = Vec::new();
        for entry in std::mem::take(&mut it.atexit) {
            if !it.values_eq(&entry.0, func)? {
                keep.push(entry);
            }
        }
        it.atexit = keep;
        Ok(())
    }

    /// _clear() -> None
    ///
    /// Clear the list of previously registered exit functions.
    #[op(hint(py(text_signature = "")))]
    fn _clear(it: &mut Interp) {
        it.atexit.clear();
    }

    /// _ncallbacks() -> int
    ///
    /// Return the number of registered exit functions.
    #[op(hint(py(text_signature = "")))]
    fn _ncallbacks(it: &mut Interp) -> usize {
        it.atexit.len()
    }

    /// _run_exitfuncs() -> None
    ///
    /// Run all registered exit functions.
    ///
    /// If a callback raises an exception, it is logged with sys.unraisablehook.
    #[op(hint(py(text_signature = "")))]
    fn _run_exitfuncs(it: &mut Interp) {
        it.run_atexit();
    }
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
