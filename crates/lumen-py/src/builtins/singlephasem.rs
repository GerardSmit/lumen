//! `_testsinglephase`: an extension module with single-phase initialisation, no module state and
//! no support for repeated initialisation. Its state lives in the process (here: the
//! interpreter's thread), and a second import builds the module from the copy taken at the
//! first one instead of initialising again; `_testinternalcapi.clear_extension` drops that copy.

use super::subinterpm;
use crate::object::*;
use crate::vm::{dict_set_str, Interp};
use std::cell::Cell;
use std::sync::OnceLock;
use std::time::Instant;

const NOT_INITIALIZED: i64 = -1;
const NAME: &str = "_testsinglephase";

struct Cached {
    error: Obj,
    initialized: f64,
}

thread_local! {
    static COUNT: Cell<i64> = const { Cell::new(NOT_INITIALIZED) };
    static INITIALIZED: Cell<f64> = const { Cell::new(0.0) };
}

/// The copy of the module taken at the first import; it holds an object, so it lives in the
/// interpreter, not in a thread-local.
#[derive(Default)]
struct Cache(Option<Cached>);

/// A time that no earlier call returned: the state is stamped with it.
fn unique_time() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    let start = *START.get_or_init(Instant::now);
    let nanos = |t: Instant| t.duration_since(start).as_nanos() + 1;
    let prev = nanos(Instant::now());
    loop {
        let t = nanos(Instant::now());
        if t != prev {
            return t as f64 / 1e9;
        }
    }
}

fn clear_global_state() {
    INITIALIZED.with(|c| c.set(0.0));
    COUNT.with(|c| c.set(NOT_INITIALIZED));
}

/// `clear_extension(name, filename)` of `_testinternalcapi`: forget the cached copy of the
/// module, so that the next import initialises it again.
pub fn clear_extension(it: &mut Interp, name: &Value, filename: &Value) -> R<()> {
    if name.as_str().is_none() {
        let t = it.type_name_of(name);
        return Err(it.type_error(&format!("clear_extension() argument 1 must be str, not {t}")));
    }
    if filename.as_str().is_none() && !matches!(filename, Value::Obj(o) if matches!(o.kind, Kind::Bytes(_))) {
        let t = it.type_name_of(filename);
        return Err(it.type_error(&format!("clear_extension() argument 2 must be str, not {t}")));
    }
    if name.as_str() == Some(NAME) {
        it.native_state::<Cache>().0 = None;
    }
    Ok(())
}

#[lumen_bind::module(name = "_testsinglephase")]
pub mod _testsinglephase {
    use super::*;

    #[op(hint(py(text_signature = "", doc = "look_up_self()\n\nReturn the module associated with this module's def.m_base.m_index.")))]
    fn look_up_self(it: &mut Interp) -> R<Value> {
        let m = it.import_module(NAME)?;
        Ok(Value::Obj(m))
    }

    #[op(hint(py(arg_style = "parse", arg_name = "sum", text_signature = "", doc = "sum(i,j)\n\nReturn the sum of i and j.")))]
    fn sum(i: i64, j: i64) -> i64 {
        i.wrapping_add(j)
    }

    #[op(hint(py(text_signature = "", doc = "state_initialized()\n\nReturn the seconds-since-epoch when the module state was initialized.")))]
    fn state_initialized() -> f64 {
        INITIALIZED.with(Cell::get)
    }

    #[op(hint(py(text_signature = "", doc = "initialized_count()\n\nReturn how many times the module has been initialized.")))]
    fn initialized_count() -> i64 {
        COUNT.with(Cell::get)
    }

    #[op(hint(py(text_signature = "", doc = "_clear_globals()\n\nFree all global state and set it to uninitialized.")))]
    fn _clear_globals() {
        clear_global_state();
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) -> R<()> {
        let Value::Obj(m) = m else { return Ok(()) };
        if subinterpm::extensions_check_enabled() {
            return Err(it.new_exc_str("ImportError", "module _testsinglephase does not support loading in subinterpreters"));
        }
        let d = it.module_dict(m);
        dict_set_str(&d, "__doc__", Value::str("Test module _testsinglephase"));
        let cached = it.native_state::<Cache>().0.as_ref().map(|c| (c.error.clone(), c.initialized));
        let (error, initialized) = match cached {
            Some(copy) => copy,
            None => {
                if COUNT.with(Cell::get) == NOT_INITIALIZED {
                    COUNT.with(|c| c.set(0));
                }
                let initialized = unique_time();
                INITIALIZED.with(|c| c.set(initialized));
                let base = it.exc_type("Exception");
                let error = crate::builtins::native::new_type(it, NAME, "error", Some(&base), Layout::Exception);
                COUNT.with(|c| c.set(c.get() + 1));
                it.native_state::<Cache>().0 = Some(Cached { error: error.clone(), initialized });
                (error, initialized)
            }
        };
        dict_set_str(&d, "error", Value::Obj(error));
        dict_set_str(&d, "int_const", Value::Int(1969));
        dict_set_str(&d, "str_const", Value::str("something different"));
        dict_set_str(&d, "_module_initialized", Value::Float(initialized));
        Ok(())
    }
}
