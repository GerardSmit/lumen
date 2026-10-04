//! `Worker` on a target with no threads: the op table keeps its names, spawning throws, and the
//! hidden `__lumenWorkerOps` handle that `node:worker_threads` reads still exists.

use lumen_host::{ops, Ctx, Extension, Value};

#[lumen_bind::module(name = "__lumenSharedWorker")]
pub(crate) mod shared_worker_browser_bindings {
    use lumen::embed::{Ctx, OpError, OpResult, Value};

    #[op]
    pub fn connect(
        ctx: &mut Ctx,
        _url: String,
        _origin: String,
        _is_module: bool,
        _name: String,
        _dispatch: Value,
    ) -> OpResult<Value> {
        Err(OpError::thrown(ctx.make_error(
            "NotSupportedError",
            "SharedWorker requires a separate Lumen realm and transferable MessagePort bridge, which this browser runtime does not provide",
        )))
    }

    #[op]
    pub fn disconnect(_ctx: &mut Ctx, _id: u64) -> OpResult<Value> {
        Ok(Value::Undefined)
    }
}

fn spawn(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    let err = ctx.make_error(
        "Error",
        "Worker is not available in the browser runtime".to_string(),
    );
    let _ = ctx.set_member(&err, "code", Value::str("ERR_NOT_SUPPORTED_IN_BROWSER"));
    Err(err)
}

fn noop(_ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    Ok(Value::Undefined)
}

pub(crate) fn terminate_all(_ctx: &mut Ctx) {}

pub(crate) fn extension() -> Extension {
    Extension {
        name: "worker",
        modules: &[lumen_host::namespace::<shared_worker_browser_bindings::Module>],
        globals: &[],
        namespaces: &[(
            "__worker",
            ops![
                "spawn" (4) => spawn,
                "post" (2) => noop,
                "terminate" (1) => noop,
                "setRef" (2) => noop,
            ],
        )],
        state_init: None,
        js_init: Some(
            r#"Object.defineProperty(globalThis, "__lumenWorkerOps", { value: globalThis.__worker, configurable: true, enumerable: false, writable: false }); delete globalThis.__worker;"#,
        ),
        js_init_snapshot: None,
    }
}
