//! One request through the transport, with the CORS policy applied when the realm is a browsing
//! context. The policy decisions are `lumen_common::cors::FetchPolicy`'s; this module drives its
//! state machine over the asynchronous transport: preflight, manual redirect hops, response
//! filtering.

use super::transport::{Failure, Head, Raw, ResponseBody, SendOptions, Transport};
use lumen::embed::{Ctx, Value};
use lumen_common::cors::{
    Credentials, FetchPolicy, Mode, PolicyError, Redirect, ResponseType,
};
use std::{cell::RefCell, rc::Rc};

pub type ResponseKind = ResponseType;

/// A request to start.
pub struct RequestSpec {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub mode: Mode,
    pub credentials: Credentials,
    pub redirect: Redirect,
    /// Sample the upload counters of the request that carries the body.
    pub observe_upload: bool,
    /// Force a preflight for a cross-origin request (an XHR with upload listeners).
    pub force_preflight: bool,
}

/// The response head. `body` is still unread; an opaque or filtered-out response has none.
pub struct Response {
    pub kind: ResponseKind,
    pub status: u16,
    pub status_text: String,
    pub url: String,
    pub redirected: bool,
    pub headers: Vec<(String, String)>,
    pub body: ResponseBody,
}

type Done = Box<dyn FnOnce(&mut Ctx, Result<Response, Failure>)>;

/// The upload counters of the transport handle that sends the request body.
#[derive(Clone, Copy, Debug)]
pub struct UploadProgress {
    pub loaded: f64,
    pub total: f64,
    pub complete: bool,
}

#[derive(Default)]
struct ControlState {
    current: Option<Value>,
    upload: Option<Value>,
    step: u32,
    aborted: bool,
}

/// Cancels a started request and reads its upload progress.
#[derive(Clone, Default)]
pub struct RequestControl(Rc<RefCell<ControlState>>);

impl RequestControl {
    /// Abort the in-flight transport request. The completion callback is not called afterwards.
    pub fn abort(&self, ctx: &mut Ctx) {
        let current = {
            let mut state = self.0.borrow_mut();
            state.aborted = true;
            state.upload = None;
            state.current.take()
        };
        if let Some(handle) = current {
            call_member(ctx, &handle, "abort");
        }
    }

    fn aborted(&self) -> bool {
        self.0.borrow().aborted
    }

    /// The counters of the request that carries the body, once it started.
    pub fn upload_progress(&self, ctx: &mut Ctx) -> Option<UploadProgress> {
        let handle = self.0.borrow().upload.clone()?;
        let number = |ctx: &mut Ctx, key: &str| match ctx.member_get(&handle, key) {
            Ok(Value::Num(value)) if value.is_finite() => value,
            _ => 0.0,
        };
        let loaded = number(ctx, "uploadLoaded");
        let total = number(ctx, "uploadTotal");
        let complete = matches!(ctx.member_get(&handle, "uploadComplete"), Ok(Value::Bool(true)));
        Some(UploadProgress {
            loaded,
            total,
            complete,
        })
    }
}

fn call_member(ctx: &mut Ctx, object: &Value, name: &str) {
    if let Ok(function) = ctx.member_get(object, name) {
        if function.is_callable() {
            let _ = ctx.invoke(function, object.clone(), &[]);
        }
    }
}

/// Run `task` as a microtask.
pub(crate) fn defer(ctx: &mut Ctx, task: impl FnOnce(&mut Ctx) + 'static) {
    let slot = RefCell::new(Some(task));
    let function = ctx.new_native_fn(
        "",
        0,
        Rc::new(move |ctx: &mut Ctx, _: Value, _: &[Value]| {
            let task = slot.borrow_mut().take();
            if let Some(task) = task {
                task(ctx);
            }
            Ok(Value::Undefined)
        }),
    );
    ctx.queue_microtask(function);
}

/// The first value of the header `name`.
pub(crate) fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

enum Plan {
    Browser(FetchPolicy),
    Direct {
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Option<Vec<u8>>,
    },
}

struct Flow {
    transport: Rc<Transport>,
    control: RequestControl,
    mode: Mode,
    credentials: Credentials,
    redirect: Redirect,
    observe_upload: bool,
    force_preflight: bool,
    upload_assigned: bool,
    starting: bool,
    internal_response: bool,
    resource_policy: Option<ResourcePolicy>,
    plan: Plan,
    done: Option<Done>,
}

/// User-agent resource policy, checked before each network hop. The Boolean
/// indicates that a redirect has already occurred.
pub type ResourcePolicy = Rc<dyn Fn(&mut Ctx, &str, bool) -> bool>;

type Shared = Rc<RefCell<Flow>>;

/// Start `spec`. `done` runs once with the response head or the failure, never from inside this
/// call, and not at all after [`RequestControl::abort`].
pub fn start(
    ctx: &mut Ctx,
    spec: RequestSpec,
    done: impl FnOnce(&mut Ctx, Result<Response, Failure>) + 'static,
) -> RequestControl {
    start_internal(ctx,spec,None,false,None,done)
}

/// Fetch a user-agent resource without exposing an opaque response to script.
/// Normal CORS, credentials and redirect rules still apply. Only this internal
/// consumer receives the response bytes after a successful no-cors request.
pub fn start_resource(ctx:&mut Ctx,document_url:&str,spec:RequestSpec,policy:ResourcePolicy,
    done:impl FnOnce(&mut Ctx,Result<Response,Failure>)+'static)->RequestControl {
    let origin=Transport::current(ctx).and_then(|transport|transport.native_origin(ctx))
        .or_else(||lumen_common::url::parse_url(document_url,None).map(|url|url.origin()));
    let Some(origin)=origin else {
        defer(ctx,move|ctx|done(ctx,Err(type_error("Invalid resource client URL"))));
        return RequestControl::default();
    };
    start_internal(ctx,spec,Some((origin,false)),true,Some(policy),done)
}

/// Send a fixed user-agent CSP report through the same asynchronous request driver.
/// Origin comes from the protected document URL, never from an author base element.
pub fn start_policy_report(ctx: &mut Ctx, document_url: &str, url: &str, body: Vec<u8>,
    done: impl FnOnce(&mut Ctx, Result<Response, Failure>) + 'static) -> RequestControl {
    let origin=lumen_common::url::parse_url(document_url,None).map(|url|url.origin());
    if origin.is_none() || body.len()>65_536 {
        defer(ctx,move|ctx|done(ctx,Err(type_error("Invalid or oversized CSP report"))));
        return RequestControl::default();
    }
    let spec=RequestSpec {method:"POST".into(),url:url.into(),headers:Vec::new(),body:Some(body),
        mode:Mode::NoCors,credentials:Credentials::SameOrigin,redirect:Redirect::Error,
        observe_upload:false,force_preflight:false};
    start_internal(ctx,spec,origin.map(|origin|(origin,true)),false,None,done)
}

/// Reporting API delivery uses normal CORS and same-origin credentials, with
/// the protected response origin captured before author globals can change.
pub fn start_reporting_report(ctx:&mut Ctx,document_url:&str,url:&str,body:Vec<u8>,
    done:impl FnOnce(&mut Ctx,Result<Response,Failure>)+'static)->RequestControl {
    let origin=lumen_common::url::parse_url(document_url,None).map(|url|url.origin());
    if origin.is_none()||body.len()>65_536 {
        defer(ctx,move|ctx|done(ctx,Err(type_error("Invalid or oversized Reporting report"))));return RequestControl::default();
    }
    let spec=RequestSpec{method:"POST".into(),url:url.into(),headers:vec![("content-type".into(),"application/reports+json".into())],body:Some(body),
        mode:Mode::Cors,credentials:Credentials::SameOrigin,redirect:Redirect::Follow,observe_upload:false,force_preflight:true};
    start_internal(ctx,spec,origin.map(|origin|(origin,false)),false,None,done)
}

fn start_internal(ctx:&mut Ctx,spec:RequestSpec,report_origin:Option<(String,bool)>,internal_response:bool,resource_policy:Option<ResourcePolicy>,
    done:impl FnOnce(&mut Ctx,Result<Response,Failure>)+'static)->RequestControl {
    let control = RequestControl::default();
    let Some(transport) = Transport::current(ctx) else {
        let failure = Failure::network("HTTP transport is unavailable");
        defer(ctx, move |ctx| done(ctx, Err(failure)));
        return control;
    };
    let plan = if let Some((origin,legacy))=report_origin {
        let policy=if legacy {FetchPolicy::new_policy_report(&origin,&spec.url,spec.body.clone().unwrap_or_default()).map_err(policy_failure)}else{browser_policy(&origin,&spec)};
        match policy {Ok(policy)=>Plan::Browser(policy),Err(error)=>{defer(ctx,move|ctx|done(ctx,Err(error)));return control;}}
    } else { match transport.browser_origin(ctx) {
        Some(origin) => match browser_policy(&origin, &spec) {
            Ok(policy) => Plan::Browser(policy),
            Err(failure) => {
                defer(ctx, move |ctx| done(ctx, Err(failure)));
                return control;
            }
        },
        None => Plan::Direct {
            method: spec.method.clone(),
            url: spec.url.clone(),
            headers: spec.headers.clone(),
            body: spec.body.clone(),
        },
    }};
    let flow: Shared = Rc::new(RefCell::new(Flow {
        transport,
        control: control.clone(),
        mode: spec.mode,
        credentials: spec.credentials,
        redirect: spec.redirect,
        observe_upload: spec.observe_upload,
        force_preflight: spec.force_preflight,
        upload_assigned: false,
        starting: true,
        internal_response,
        resource_policy,
        plan,
        done: Some(Box::new(done)),
    }));
    begin(ctx, &flow);
    flow.borrow_mut().starting = false;
    control
}

fn browser_policy(origin: &str, spec: &RequestSpec) -> Result<FetchPolicy, Failure> {
    let url_origin = lumen_common::url::parse_url(&spec.url, None)
        .ok_or_else(|| type_error("Invalid URL"))?
        .origin();
    let mut policy = FetchPolicy::new(
        origin,
        &spec.method,
        &spec.url,
        &url_origin,
        spec.headers.clone(),
        spec.body.clone(),
        spec.mode,
        spec.credentials,
        spec.redirect,
    )
    .map_err(policy_failure)?;
    policy.set_force_preflight(spec.force_preflight);
    Ok(policy)
}

fn type_error(message: &str) -> Failure {
    Failure {
        name: "TypeError".into(),
        message: message.into(),
    }
}

fn policy_failure(error: PolicyError) -> Failure {
    type_error(match error {
        PolicyError::InvalidMode => "Invalid request mode",
        PolicyError::InvalidCredentials => "Invalid credentials mode",
        PolicyError::InvalidRedirect => "Invalid redirect mode",
        PolicyError::SameOrigin => "Fetch blocked by same-origin mode",
        PolicyError::NoCorsMethod => "no-cors requests require GET, HEAD, or POST",
        PolicyError::NoCorsRedirect => "no-cors requests require redirect mode follow",
        PolicyError::Preflight => "CORS preflight failed",
        PolicyError::Cors => "CORS check failed",
        PolicyError::Redirect => "redirect is disallowed",
        PolicyError::TooManyRedirects => "too many redirects",
        PolicyError::InvalidMethod => "Invalid method",
        PolicyError::InvalidHeader => "Invalid header",
        PolicyError::InvalidUrl => "Invalid URL",
    })
}

fn begin(ctx: &mut Ctx, flow: &Shared) {
    let check = {
        let state=flow.borrow();
        state.resource_policy.clone().map(|check| {
            let (url,redirected)=match &state.plan {
                Plan::Browser(policy)=>(policy.actual_request_head().url,policy.is_redirected()),
                Plan::Direct{url,..}=>(url.clone(),false),
            };
            (check,url,redirected)
        })
    };
    if let Some((check,url,redirected))=check {
        if !check(ctx,&url,redirected) {
            finish(ctx,flow,Err(Failure::network("Resource blocked by document policy")));
            return;
        }
    }
    let preflight = match &flow.borrow().plan {
        Plan::Browser(policy) => Some(policy.preflight_request()),
        Plan::Direct { .. } => None,
    };
    match preflight {
        Some(Some(head)) => send(ctx, flow, head.method, head.url, head.headers, None, true, false, on_preflight),
        Some(None) => send_actual(ctx, flow),
        None => send_direct(ctx, flow),
    }
}

fn send_actual(ctx: &mut Ctx, flow: &Shared) {
    let (head, body, cookies_allowed) = match &flow.borrow().plan {
        Plan::Browser(policy) => (policy.actual_request_head(), policy.actual_body().map(<[u8]>::to_vec), policy.credentials_allowed()),
        Plan::Direct { .. } => return,
    };
    send(ctx, flow, head.method, head.url, head.headers, body, true, cookies_allowed, on_actual);
}

fn send_direct(ctx: &mut Ctx, flow: &Shared) {
    let (method, url, headers, body, manual) = {
        let state = flow.borrow();
        let Plan::Direct {
            method,
            url,
            headers,
            body,
        } = &state.plan
        else {
            return;
        };
        (
            method.clone(),
            url.clone(),
            headers.clone(),
            body.clone(),
            state.redirect != Redirect::Follow,
        )
    };
    let cookies_allowed = flow.borrow().credentials != Credentials::Omit;
    send(ctx, flow, method, url, headers, body, manual, cookies_allowed, on_direct);
}

#[allow(clippy::too_many_arguments)]
fn send(
    ctx: &mut Ctx,
    flow: &Shared,
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    manual_redirect: bool,
    cookies_allowed: bool,
    next: fn(&mut Ctx, &Shared, Raw),
) {
    let (transport, control, options, observe) = {
        let mut state = flow.borrow_mut();
        let observe = body.is_some() && state.observe_upload && !state.upload_assigned;
        if observe {
            state.upload_assigned = true;
        }
        let options = SendOptions {
            cookies_allowed,
            mode: state.mode,
            credentials: state.credentials,
            redirect: state.redirect,
            upload_progress: observe || state.force_preflight,
            force_preflight: state.force_preflight,
        };
        (state.transport.clone(), state.control.clone(), options, observe)
    };
    let step = {
        let mut state = control.0.borrow_mut();
        state.step += 1;
        state.step
    };
    let callback = {
        let flow = flow.clone();
        Box::new(move |ctx: &mut Ctx, result: Result<Raw, Failure>| {
            if flow.borrow().control.aborted() {
                if let Ok(raw) = &result {
                    raw.body.cancel(ctx);
                }
                flow.borrow_mut().done = None;
                return;
            }
            match result {
                Ok(raw) => next(ctx, &flow, raw),
                Err(failure) => finish(ctx, &flow, Err(failure)),
            }
        })
    };
    let head = Head {
        method: &method,
        url: &url,
        headers: &headers,
        body: body.as_deref(),
    };
    match transport.send(ctx, &head, manual_redirect, &options, callback) {
        Ok(handle) => {
            let mut state = control.0.borrow_mut();
            if state.step == step && !state.aborted {
                if observe {
                    state.upload = Some(handle.clone());
                }
                state.current = Some(handle);
            }
        }
        Err(failure) => finish(ctx, flow, Err(failure)),
    }
}

fn finish(ctx: &mut Ctx, flow: &Shared, result: Result<Response, Failure>) {
    let (done, starting) = {
        let mut state = flow.borrow_mut();
        (state.done.take(), state.starting)
    };
    let Some(done) = done else {
        if let Ok(response) = result {
            response.body.cancel(ctx);
        }
        return;
    };
    if starting {
        defer(ctx, move |ctx| done(ctx, result));
    } else {
        done(ctx, result);
    }
}

fn opaque(kind: ResponseKind) -> Response {
    Response {
        kind,
        status: 0,
        status_text: String::new(),
        url: String::new(),
        redirected: false,
        headers: Vec::new(),
        body: ResponseBody::None,
    }
}

fn opaque_kind(raw: &Raw) -> ResponseType {
    if raw.kind.as_deref() == Some("opaqueredirect") {
        ResponseType::OpaqueRedirect
    } else {
        ResponseType::Opaque
    }
}

fn is_redirect_status(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn on_preflight(ctx: &mut Ctx, flow: &Shared, raw: Raw) {
    raw.body.cancel(ctx);
    let valid = match &flow.borrow().plan {
        Plan::Browser(policy) => policy.validate_preflight(raw.status, &raw.headers),
        Plan::Direct { .. } => Ok(()),
    };
    match valid {
        Ok(()) => send_actual(ctx, flow),
        Err(error) => finish(ctx, flow, Err(policy_failure(error))),
    }
}

fn on_actual(ctx: &mut Ctx, flow: &Shared, raw: Raw) {
    if raw.status == 0 || matches!(raw.kind.as_deref(), Some("opaque" | "opaqueredirect")) {
        raw.body.cancel(ctx);
        return finish(ctx, flow, Ok(opaque(opaque_kind(&raw))));
    }
    let location = header(&raw.headers, "location").map(str::to_owned);
    let next = {
        let state = flow.borrow();
        match (&state.plan, &location) {
            (Plan::Browser(policy), Some(location)) => {
                lumen_common::url::parse(location, Some(policy.current_url()))
                    .ok()
                    .map(|url| (url.href(), url.origin()))
            }
            _ => None,
        }
    };
    let (stepped, redirect_mode, filtered, current_url, redirected) = {
        let mut state = flow.borrow_mut();
        let redirect_mode = state.redirect;
        let Plan::Browser(policy) = &mut state.plan else {
            return;
        };
        let stepped = policy.response_head(
            raw.status,
            &raw.headers,
            location.as_deref(),
            next.as_ref().map(|(url, origin)| (url.as_str(), origin.as_str())),
        );
        (
            stepped,
            redirect_mode,
            policy.filter_response(&raw.headers),
            policy.current_url().to_owned(),
            policy.is_redirected(),
        )
    };
    match stepped {
        Err(error) => {
            raw.body.cancel(ctx);
            finish(ctx, flow, Err(policy_failure(error)));
        }
        Ok(true) => {
            raw.body.cancel(ctx);
            begin(ctx, flow);
        }
        Ok(false) => {
            if redirect_mode == Redirect::Manual
                && is_redirect_status(raw.status)
                && location.is_some()
            {
                raw.body.cancel(ctx);
                return finish(ctx, flow, Ok(opaque(ResponseType::OpaqueRedirect)));
            }
            let internal_response=flow.borrow().internal_response;
            if filtered.kind == ResponseType::Opaque && !internal_response {
                raw.body.cancel(ctx);
                return finish(ctx, flow, Ok(opaque(ResponseType::Opaque)));
            }
            let url = if raw.url.is_empty() { current_url } else { raw.url };
            finish(
                ctx,
                flow,
                Ok(Response {
                    kind: filtered.kind,
                    status: raw.status,
                    status_text: raw.status_text,
                    url,
                    redirected: raw.redirected.unwrap_or(redirected),
                    headers: if internal_response {raw.headers} else {filtered.headers},
                    body: raw.body,
                }),
            );
        }
    }
}

fn on_direct(ctx: &mut Ctx, flow: &Shared, raw: Raw) {
    if raw.status == 0 || matches!(raw.kind.as_deref(), Some("opaque" | "opaqueredirect")) {
        raw.body.cancel(ctx);
        return finish(ctx, flow, Ok(opaque(opaque_kind(&raw))));
    }
    let redirect = flow.borrow().redirect;
    if redirect != Redirect::Follow
        && is_redirect_status(raw.status)
        && header(&raw.headers, "location").is_some()
    {
        raw.body.cancel(ctx);
        return match redirect {
            Redirect::Manual => finish(ctx, flow, Ok(opaque(ResponseType::OpaqueRedirect))),
            _ => finish(ctx, flow, Err(type_error("redirect is disallowed"))),
        };
    }
    let url = match &flow.borrow().plan {
        Plan::Direct { url, .. } if raw.url.is_empty() => url.clone(),
        _ => raw.url.clone(),
    };
    let kind = match raw.kind.as_deref() {
        Some("cors") => ResponseType::Cors,
        _ => ResponseType::Basic,
    };
    finish(
        ctx,
        flow,
        Ok(Response {
            kind,
            status: raw.status,
            status_text: raw.status_text,
            url,
            redirected: raw.redirected.unwrap_or(false),
            headers: raw.headers,
            body: raw.body,
        }),
    );
}
