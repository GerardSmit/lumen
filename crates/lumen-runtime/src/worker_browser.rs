//! `Worker` on a target with no threads: the op table keeps its names, spawning throws, and the
//! hidden `__lumenWorkerOps` handle that `node:worker_threads` reads still exists.

use lumen_host::{ops, Ctx, Extension, Value};

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
        modules: &[],
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
