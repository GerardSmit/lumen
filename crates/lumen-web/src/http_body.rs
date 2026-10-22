//! Typed body handle over the shared incremental transport decoder.
use crate::body_pump::{BodyPump, Take};
use lumen::embed::{Ctx, Deferred, JsHost, OpResult, Promise, SendError, Value};
use std::sync::Mutex;

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
    source: Mutex<Option<crate::http::OpenHttpBody>>,
    pump: BodyPump,
    cancellation: lumen_os::net::TcpCancellation,
}

impl ResponseBody {
    pub fn new(body: crate::http::OpenHttpBody) -> Self {
        Self {
            cancellation: body.cancellation.clone(),
            source: Mutex::new(Some(body)),
            pump: BodyPump::default(),
        }
    }

    fn ensure_started(&self) {
        let body = self
            .source
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        let Some(mut body) = body else { return };
        self.pump.start(
            move || body.read_chunk(),
            |job| {
                lumen_host::spawn_thread("lumen-http-body", lumen_os::sched::Purpose::Blocking, job)
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            },
        );
    }
}

#[lumen_bind::methods]
impl ResponseBody {
    fn read(&self, ctx: &mut Ctx) -> Promise<Result<Chunk, SendError>> {
        self.ensure_started();
        let deferred = Deferred::new(ctx);
        let promise = Promise::pending(&deferred);
        let completer = ctx.completer(deferred);
        self.pump.request(move |taken| {
            completer.complete(match taken {
                Take::Data(bytes) => Ok(Chunk::Bytes(bytes)),
                Take::End => Ok(Chunk::End),
                Take::Failed(message) => Err(SendError::new("TypeError", message)),
            })
        });
        promise
    }

    fn cancel(&self) -> OpResult<()> {
        // Interrupt a blocked read before dropping the reader.
        self.cancellation.cancel();
        self.pump.cancel();
        *self
            .source
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = None;
        Ok(())
    }
}

impl Drop for ResponseBody {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.pump.cancel();
    }
}
