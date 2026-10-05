//! Typed cancellation handle returned by the existing HTTP transport operation.
use lumen::embed::{Ctx, OpResult};
#[cfg(target_arch = "wasm32")]
use lumen::embed::OpError;

#[lumen_bind::class(name = "HttpRequestControl")]
pub(crate) struct RequestControl {
    pub(crate) id: u64,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) cancellation: lumen_os::net::TcpCancellation,
}

#[lumen_bind::methods]
impl RequestControl {
    fn abort(&self, ctx: &mut Ctx) -> OpResult<()> {
        #[cfg(not(target_arch = "wasm32"))]
        self.cancellation.cancel();
        #[cfg(target_arch = "wasm32")]
        lumen_host::browser::call_host("fetchAbort", &[wasm_bindgen::JsValue::from_f64(self.id as f64)])
            .map_err(|message| OpError::new("Error", message))?;
        if let Some(registry) = ctx.host_mut::<lumen_host::TaskRegistry>() {
            registry.cancel(self.id);
        }
        Ok(())
    }
}
