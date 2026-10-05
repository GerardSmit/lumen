//! Typed cancellation handle returned by the existing HTTP transport operation.
#[cfg(target_arch = "wasm32")]
use lumen::embed::OpError;
use lumen::embed::{Ctx, OpResult};

#[lumen_bind::class(name = "HttpRequestControl")]
pub(crate) struct RequestControl {
    pub(crate) id: u64,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) cancellation: lumen_os::net::TcpCancellation,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) upload_progress: std::sync::Arc<lumen_common::http_body::UploadProgress>,
}

#[lumen_bind::methods]
impl RequestControl {
    #[getter(name = "uploadLoaded")]
    fn upload_loaded(&self) -> f64 {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.upload_progress.loaded() as f64
        }
        #[cfg(target_arch = "wasm32")]
        {
            browser_upload_progress(self.id).0
        }
    }

    #[getter(name = "uploadTotal")]
    fn upload_total(&self) -> f64 {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.upload_progress.total() as f64
        }
        #[cfg(target_arch = "wasm32")]
        {
            browser_upload_progress(self.id).1
        }
    }

    #[getter(name = "uploadComplete")]
    fn upload_complete(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.upload_progress.complete()
        }
        #[cfg(target_arch = "wasm32")]
        {
            browser_upload_progress(self.id).2
        }
    }

    fn abort(&self, ctx: &mut Ctx) -> OpResult<()> {
        #[cfg(not(target_arch = "wasm32"))]
        self.cancellation.cancel();
        #[cfg(target_arch = "wasm32")]
        lumen_host::browser::call_host(
            "fetchAbort",
            &[wasm_bindgen::JsValue::from_f64(self.id as f64)],
        )
        .map_err(|message| OpError::new("Error", message))?;
        if let Some(registry) = ctx.host_mut::<lumen_host::TaskRegistry>() {
            registry.cancel(self.id);
        }
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
fn browser_upload_progress(id: u64) -> (f64, f64, bool) {
    let result = lumen_host::browser::call_host(
        "fetchUploadProgress",
        &[wasm_bindgen::JsValue::from_f64(id as f64)],
    );
    let Ok(result) = result else {
        return (0.0, 0.0, false);
    };
    let values = js_sys::Array::from(&result);
    (
        values.get(0).as_f64().unwrap_or(0.0),
        values.get(1).as_f64().unwrap_or(0.0),
        values.get(2).as_bool().unwrap_or(false),
    )
}
