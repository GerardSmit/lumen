//! Network requests for native web classes: one request pipeline over the realm's HTTP
//! transport, and the classes built on it (`fetch` and its classes, `XMLHttpRequest`,
//! `ProgressEvent`). Design:
//! `docs/native-network.md`.
//!
//! - [`Transport`] adapts the host's `__http`-shaped operations (the desktop runtime's
//!   `lumen-web` ops, the Bitnest kernel's `__bitnestHttp`) to Rust. A realm registers it once
//!   with [`Transport::install`].
//! - [`start`] runs one request through the CORS policy of `lumen_common::cors` (preflight,
//!   manual redirects, response filtering) when the realm is a browsing context whose transport
//!   does not apply the policy itself, and straight through the transport otherwise.
//! - [`extract_body`] turns a `BodyInit` into bytes and a default content type; [`ResponseBody`]
//!   and [`read_chunk`] stream a response; [`bindings`] publishes the classes.
//!
//! - [`fetch_bindings`] publishes `Headers`, `Request`, `Response` and `fetch` over the same
//!   pipeline; its body handling is in `fetch_body`, its header list in `headers`.

mod body;
mod fetch;
mod fetch_body;
mod flow;
mod headers;
mod transport;
mod xhr;

use crate::events::DomException;
use lumen::embed::{Ctx, OpError, Value};

pub use body::{extract_body, ExtractedBody};
pub use flow::{start, start_policy_report, start_reporting_report, start_resource, ResourcePolicy, RequestControl, RequestSpec, Response, ResponseKind};
pub use lumen_common::cors::{Credentials, Mode, Redirect};
pub use transport::{
    cancel_reader, read_chunk, Failure, ResponseBody, SyncRequest, SyncResponse, Transport,
};
pub use fetch::{
    bindings as fetch_bindings, headers_entries, read_served_body, request_from_parts, request_header, server_request,
    served_response, ServedResponse,
};
pub use xhr::bindings;

struct ApiBaseUrlProvider(fn(&mut Ctx) -> Option<String>);

/// Register the embedder's native settings-object URL authority.
pub fn set_api_base_url_provider(ctx:&mut Ctx,provider:fn(&mut Ctx)->Option<String>) {
    ctx.op_state().put(ApiBaseUrlProvider(provider));
}

fn native_api_base_url(ctx:&mut Ctx)->Option<String> {
    let provider=ctx.op_state().get::<ApiBaseUrlProvider>().map(|provider|provider.0)?;
    provider(ctx)
}

/// Shared API URL basis for fetch, XHR and streaming web classes.
pub fn api_base_url(ctx:&mut Ctx)->Option<String> {xhr::document_base(ctx)}

/// Cancellation for a user-agent resource body still being consumed.
pub struct ResourceBodyControl(std::rc::Rc<fetch_body::Drain>);
impl ResourceBodyControl {
    pub fn cancel(&self,ctx:&mut Ctx) {
        let reason=OpError::new("AbortError","Resource body cancelled").to_value(ctx);
        self.0.cancel(ctx,reason);
    }
}

/// Consume a transport resource with the same reader and cancellation machinery
/// as fetch, enforcing the user-agent resource's own byte budget while reading.
pub fn consume_resource_body(ctx:&mut Ctx,body:ResponseBody,limit:usize,
    done:impl FnOnce(&mut Ctx,Result<Vec<u8>,Value>)+'static)->Option<ResourceBodyControl> {
    let body=match body {
        ResponseBody::None=>fetch_body::Body::null(),
        ResponseBody::Bytes(bytes)=>{
            if bytes.len()>limit {let reason=OpError::type_error("Response body exceeds the resource byte budget").to_value(ctx);done(ctx,Err(reason));}
            else {done(ctx,Ok(bytes));}
            return None;
        },
        ResponseBody::Reader(reader)=>fetch_body::Body::net(fetch_body::NetBody::new(reader)),
    }.cell();
    Some(ResourceBodyControl(fetch_body::read_all_bounded(ctx,&body,limit,Box::new(done))))
}

/// A `DOMException` of the given name as an error to throw.
pub fn dom_error(ctx: &mut Ctx, name: &str, message: impl AsRef<str>) -> OpError {
    OpError::thrown(ctx.new_instance(DomException::with_name(message.as_ref(), name)))
}

/// Whether `text` is an HTTP token (a method or a header name).
pub(crate) fn is_token(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

pub(crate) fn has_global_object(ctx: &mut Ctx, name: &str) -> bool {
    let global = ctx.global_object();
    matches!(ctx.member_get(&global, name), Ok(Value::Obj(_)))
}
