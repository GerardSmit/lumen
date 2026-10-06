//! Typed body handle over the shared incremental transport decoder.
use lumen::embed::{Ctx, JsHost, OpResult, Promise, SendError, Value};
use std::sync::{Arc, Mutex};

/// A body read: the next chunk's bytes, or `null` at the end of the body (what the transport body tests read).
pub(crate) enum Chunk {
    Bytes(Vec<u8>),
    End,
}

impl lumen_bind::IntoRet<JsHost> for Chunk {
    const MAY_RUN: bool = false;
    fn into_ret(self, ctx: &mut Ctx) -> Result<Value, Value> {
        match self {
            Chunk::Bytes(bytes) => ctx.make_uint8array(&bytes),
            Chunk::End => Ok(Value::Null),
        }
    }
}

#[lumen_bind::class(name = "HttpResponseBody")]
pub(crate) struct ResponseBody {
    body: Arc<Mutex<Option<crate::http::OpenHttpBody>>>,
    cancellation: lumen_os::net::TcpCancellation,
}

impl ResponseBody {
    pub fn new(body: crate::http::OpenHttpBody) -> Self {
        Self {
            cancellation: body.cancellation.clone(),
            body: Arc::new(Mutex::new(Some(body))),
        }
    }
}

#[lumen_bind::methods]
impl ResponseBody {
    fn read(&self, ctx: &mut Ctx) -> Promise<Result<Chunk, SendError>> {
        let body = self.body.clone();
        ctx.spawn_thread(move || {
            let mut guard = body.lock().unwrap_or_else(|poison| poison.into_inner());
            let result = guard.as_mut().map_or(Ok(None), |body| body.read_chunk());
            if !matches!(result, Ok(Some(_))) {
                *guard = None;
            }
            result
                .map(|chunk| chunk.map_or(Chunk::End, Chunk::Bytes))
                .map_err(|error| {
                    SendError::new("TypeError", format!("HTTP body read failed: {error}"))
                })
        })
    }

    fn cancel(&self) -> OpResult<()> {
        // Interrupt a blocked read before taking its ownership lock.
        self.cancellation.cancel();
        *self
            .body
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = None;
        Ok(())
    }
}

impl Drop for ResponseBody {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}
