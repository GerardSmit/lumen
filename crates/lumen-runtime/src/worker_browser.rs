//! `Worker` on a target with no threads: the page classes exist but the backend is the default
//! unsupported one, so constructing throws `NotSupportedError`. The hidden `__lumenWorkerOps`
//! handle that `node:worker_threads` reads keeps its names.

use lumen_bind::NativeError;
use lumen_host::{Ctx, Extension, Value};

#[lumen_bind::module(name = "__lumenWorkerOps")]
mod bindings {
    use super::*;

    #[op(name = "spawn")]
    fn op_spawn(#[varargs] _args: &[Value]) -> Result<(), NativeError> {
        Err(lumen_host::browser::unsupported("Worker"))
    }

    #[op(name = "terminate")]
    fn op_terminate(#[varargs] _args: &[Value]) {}

    #[op(name = "setRef")]
    fn op_set_ref(#[varargs] _args: &[Value]) {}
}

pub(crate) fn terminate_all(_ctx: &mut Ctx) {}

pub(crate) fn extension() -> Extension {
    Extension {
        name: "worker",
        modules: &[
            lumen_host::namespace::<bindings::Module>,
            lumen_host::workers::install_page_classes,
        ],
        state_init: None,
        js_init: None,
        js_init_snapshot: None,
        lazy_globals: &[],
    }
}
