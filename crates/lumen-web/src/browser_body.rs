//! Typed, demand-driven response reads through the browser's actual Fetch body.
use lumen::embed::{Ctx, Deferred, OpError, OpResult, Promise};
use lumen_host::{browser::{Arg, Event, call_host}, Value};
use lumen_bind::Data;
use wasm_bindgen::JsValue;

#[lumen_bind::class(name = "HttpResponseBody")]
pub(crate) struct ResponseBody {
    pub(crate) request_id: u64,
}

#[lumen_bind::methods]
impl ResponseBody {
    fn read(&self, ctx: &mut Ctx) -> Promise<Value> {
        let deferred = Deferred::new(ctx);
        let (resolve, reject) = deferred.resolving_functions(ctx);
        let id = lumen_host::register_task(ctx, resolve, Some(reject), decode_chunk);
        match call_host("fetchRead", &[
            JsValue::from_f64(self.request_id as f64), JsValue::from_f64(id as f64),
        ]) {
            Ok(_) => Promise::pending(&deferred),
            Err(message) => {
                if let Some(registry) = ctx.host_mut::<lumen_host::TaskRegistry>() { registry.cancel(id); }
                Promise::rejected(OpError::new("TypeError", format!("fetch body: {message}")))
            }
        }
    }

    fn cancel(&self) -> OpResult<()> {
        call_host("fetchAbort", &[JsValue::from_f64(self.request_id as f64)])
            .map_err(|message| OpError::new("TypeError", message))?;
        Ok(())
    }
}

impl Drop for ResponseBody {
    fn drop(&mut self) {
        let _ = call_host("fetchAbort", &[JsValue::from_f64(self.request_id as f64)]);
    }
}

fn decode_chunk(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    let event = *payload.downcast::<Event>().expect("fetch body event");
    match (event.kind.as_str(), event.args.into_iter().next()) {
        ("chunk", Some(Arg::Data(Data::Bytes(bytes)))) => Ok(vec![ctx.make_uint8array(&bytes)?]),
        ("end", _) => Ok(vec![Value::Null]),
        ("error", Some(Arg::Data(Data::Str(message)))) => Err(ctx.make_error("TypeError", message)),
        _ => Err(ctx.make_error("TypeError", "malformed fetch body event")),
    }
}
