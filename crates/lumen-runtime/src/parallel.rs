use crate::{Engine, Runtime};
use std::sync::Arc;

struct Driver(Runtime);

pub(super) fn install(engine: &mut Engine) {
    let host = Arc::new(lumen::parallel::ThreadHost::with_turn(configure, turn));
    #[cfg(feature = "compiler")]
    lumen::parallel::install(engine, host);
    #[cfg(all(not(feature = "compiler"), feature = "aot-native"))]
    lumen::parallel::install_native_with_limits(
        engine,
        host,
        lumen::parallel::Limits::default(),
        include_bytes!(concat!(env!("OUT_DIR"), "/parallel.aot")),
    )
    .expect("initialize native parallel runtime");
}

fn configure(engine: &mut Engine) {
    let mut runtime = Runtime::new();
    #[cfg(feature = "aot-native")]
    runtime
        .install_native_builtins()
        .expect("initialize native parallel builtins");
    std::mem::swap(engine, &mut runtime.engine);
    engine.ctx().op_state().put(Driver(runtime));
}

fn turn(engine: &mut Engine) -> Result<(), String> {
    let Some(Driver(mut runtime)) = engine.ctx().op_state().take::<Driver>() else {
        return Err("parallel runtime driver is unavailable".into());
    };
    std::mem::swap(engine, &mut runtime.engine);
    let status = runtime.run_until_idle();
    std::mem::swap(engine, &mut runtime.engine);
    engine.ctx().op_state().put(Driver(runtime));
    if status.halted {
        Err("parallel worker runtime halted".into())
    } else {
        Ok(())
    }
}
