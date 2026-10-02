//! `Worker` on a target with no threads: the op table keeps its names, spawning throws, and the
//! hidden `__lumenWorkerOps` handle that `node:worker_threads` reads still exists.

use lumen_host::{Ctx, Extension, Value};

#[lumen_bind::module(name = "__worker")]
mod bindings {
    use super::*;

    #[op(name = "spawn")]
    fn op_spawn(ctx: &mut Ctx, #[varargs] _args: &[Value]) -> Result<(), Value> {
        let err = ctx.make_error(
            "Error",
            "Worker is not available in the browser runtime".to_string(),
        );
        let _ = ctx.set_member(&err, "code", Value::str("ERR_NOT_SUPPORTED_IN_BROWSER"));
        Err(err)
    }

    #[op(name = "post")]
    fn op_post(#[varargs] _args: &[Value]) {}

    #[op(name = "terminate")]
    fn op_terminate(#[varargs] _args: &[Value]) {}

    #[op(name = "setRef")]
    fn op_set_ref(#[varargs] _args: &[Value]) {}
}

pub(crate) fn terminate_all(_ctx: &mut Ctx) {}

pub(crate) fn extension() -> Extension {
    Extension {
        name: "worker",
        modules: &[lumen_host::namespace::<bindings::Module>],
        state_init: None,
        js_init: Some(
            r#"Object.defineProperty(globalThis, "__lumenWorkerOps", { value: globalThis.__worker, configurable: true, enumerable: false, writable: false }); delete globalThis.__worker;"#,
        ),
        js_init_snapshot: None,
    }
}
