//! The native `Performance` interface, the `performance` global and `self`.
//!
//! `Performance` extends the native `EventTarget` and reads the process clock of
//! [`crate::perf`]. [`install_globals`] publishes `Performance` as a lazy global and defines
//! `self` and `performance` with the descriptors of an assignment (writable, enumerable,
//! configurable data properties), skipping names the realm already defines.

use crate::events::EventTarget;
use lumen::embed::{Ctx, OpError, OpResult, Value};

#[lumen_bind::module(name = "performance")]
pub mod bindings {
    use super::*;

    #[class(name = "Performance", extends = EventTarget, hint(js(webidl, invalid_this)))]
    pub struct Performance {
        base: EventTarget,
    }

    #[methods]
    impl Performance {
        #[constructor]
        fn constructor() -> OpResult<Self> {
            Err(OpError::type_error("Illegal constructor").with_code("ERR_ILLEGAL_CONSTRUCTOR"))
        }

        /// Milliseconds since the time origin, on the shared 100-microsecond grid.
        fn now(&self) -> f64 {
            crate::perf::web_now_ms()
        }

        /// Unix-epoch milliseconds at the clock's zero point.
        #[getter]
        fn time_origin(&self) -> f64 {
            crate::perf::time_origin_ms()
        }

        #[method(name = "toJSON")]
        fn to_json(&self, ctx: &mut Ctx) -> OpResult<Value> {
            let object = Value::Obj(ctx.new_object());
            ctx.member_set(&object, "timeOrigin", Value::Num(crate::perf::time_origin_ms()))
                .map_err(OpError::thrown)?;
            Ok(object)
        }
    }

    impl Performance {
        pub fn singleton() -> Self {
            Self {
                base: EventTarget::from_data(crate::events::TargetData::new(None)),
            }
        }
    }
}

fn define_global(ctx: &mut Ctx, name: &str, value: Value) -> Result<(), Value> {
    let global = ctx.global_object();
    if !matches!(ctx.member_get(&global, name)?, Value::Undefined) {
        return Ok(());
    }
    let descriptor = ctx.new_object_with_proto(&Value::Null);
    for (key, field) in [
        ("value", value),
        ("writable", Value::Bool(true)),
        ("enumerable", Value::Bool(true)),
        ("configurable", Value::Bool(true)),
    ] {
        ctx.member_set(&descriptor, key, field)?;
    }
    ctx.define_property_value(&global, Value::str(name), &descriptor)
}

/// Publish `Performance`, `performance` and `self` in the active realm.
pub fn install_globals(ctx: &mut Ctx) -> Result<(), Value> {
    crate::lazy_globals::<bindings::Module>(ctx)?;
    let global = ctx.global_object();
    define_global(ctx, "self", global)?;
    let performance = ctx.new_instance(bindings::Performance::singleton());
    define_global(ctx, "performance", performance)
}
