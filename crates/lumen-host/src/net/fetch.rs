//! `Headers`, `Request`, `Response` and `fetch`.
//!
//! `fetch` builds a [`bindings::Request`] (the same construction `new Request` runs), reads its
//! body, and hands it to [`flow::start`](super::start), the request pipeline `XMLHttpRequest`
//! uses: the CORS policy of `lumen_common::cors` runs there when the realm is a browsing context
//! whose transport leaves the policy to us, and is skipped when the transport applies it. The
//! response head becomes a `Response` whose body is a [`NetBody`] over the transport reader.
//!
//! A [`FetchState`] lives while a fetch is in flight and while its response body is open. The
//! request's abort signal holds the state's abort step weakly, so a signal shared by many
//! fetches does not retain finished ones.

use super::body::{decode_text, essence_of, extract_body, ExtractedBody};
use super::fetch_body::{
    body_used, clone_body, failure_value, is_null, is_readable_stream, read_all, stream_disturbed,
    stream_locked, stream_value, transfer, unusable, Body, BodyCell, Drain, NetBody, Source,
};
use super::flow::{start, RequestControl, RequestSpec, Response as Head, ResponseKind};
use super::headers::{
    byte_string, fill, usv, ByteStr, Guard, HeaderError, HeadersData, SharedHeaders,
};
use super::transport::{ResponseBody, Transport};
use super::xhr::{document_base, without_fragment};
use crate::blob::{append_text, decode_multipart, new_blob, new_form_data, Bytes};
use crate::events::{add_owned_step, follow_signal, new_signal, signal_state};
use crate::messaging::member;
use lumen::embed::{Ctx, Deferred, OpError, OpResult, Value};
use lumen_common::cors::{Credentials, Mode, Redirect};
use std::{
    any::Any,
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
};

type Step = Rc<dyn Fn(&mut Ctx, &Value)>;

const CACHE_MODES: &[&str] = &[
    "default",
    "no-store",
    "reload",
    "no-cache",
    "force-cache",
    "only-if-cached",
];
const REFERRER_POLICIES: &[&str] = &[
    "",
    "no-referrer",
    "no-referrer-when-downgrade",
    "same-origin",
    "origin",
    "strict-origin",
    "origin-when-cross-origin",
    "strict-origin-when-cross-origin",
    "unsafe-url",
];
const NULL_BODY_STATUSES: &[u16] = &[101, 103, 204, 205, 304];
const FORBIDDEN_METHODS: &[&str] = &["CONNECT", "TRACE", "TRACK"];
const NORMALIZED_METHODS: &[&str] = &["DELETE", "GET", "HEAD", "OPTIONS", "POST", "PUT"];

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Basic,
    Cors,
    Default,
    Error,
    Opaque,
    OpaqueRedirect,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Basic => "basic",
            Kind::Cors => "cors",
            Kind::Default => "default",
            Kind::Error => "error",
            Kind::Opaque => "opaque",
            Kind::OpaqueRedirect => "opaqueredirect",
        }
    }
}

#[derive(Clone)]
pub(crate) struct Extra {
    cache: String,
    referrer: String,
    referrer_policy: String,
    integrity: String,
    keepalive: bool,
}

impl Default for Extra {
    fn default() -> Extra {
        Extra {
            cache: "default".into(),
            referrer: "about:client".into(),
            referrer_policy: String::new(),
            integrity: String::new(),
            keepalive: false,
        }
    }
}

/// A dictionary argument: `undefined` and `null` are the empty dictionary.
struct Dictionary(Option<Value>);

impl Dictionary {
    fn read(value: &Value, name: &str) -> OpResult<Dictionary> {
        match value {
            Value::Undefined | Value::Null => Ok(Dictionary(None)),
            Value::Obj(_) => Ok(Dictionary(Some(value.clone()))),
            _ => Err(OpError::type_error(format!(
                "The provided value is not of type '{name}'."
            ))),
        }
    }

    fn get(&self, ctx: &mut Ctx, key: &str) -> OpResult<Value> {
        member(ctx, &self.0, key)
    }
}

fn enum_error(value: &str, type_name: &str) -> OpError {
    OpError::type_error(format!(
        "The provided value '{value}' is not a valid enum value of type {type_name}."
    ))
}

fn parse_name(ctx: &mut Ctx, value: &Value, names: &[&str], type_name: &str) -> OpResult<String> {
    let text = usv(ctx, value)?;
    if names.contains(&text.as_str()) {
        Ok(text)
    } else {
        Err(enum_error(&text, type_name))
    }
}

fn parse_mode(ctx: &mut Ctx, value: &Value) -> OpResult<Mode> {
    let text = usv(ctx, value)?;
    match text.as_str() {
        "same-origin" => Ok(Mode::SameOrigin),
        "no-cors" => Ok(Mode::NoCors),
        "cors" => Ok(Mode::Cors),
        "navigate" => Err(OpError::type_error(
            "Request constructor: invalid request mode navigate.",
        )),
        _ => Err(enum_error(&text, "RequestMode")),
    }
}

fn parse_credentials(ctx: &mut Ctx, value: &Value) -> OpResult<Credentials> {
    let text = usv(ctx, value)?;
    match text.as_str() {
        "omit" => Ok(Credentials::Omit),
        "same-origin" => Ok(Credentials::SameOrigin),
        "include" => Ok(Credentials::Include),
        _ => Err(enum_error(&text, "RequestCredentials")),
    }
}

fn parse_redirect(ctx: &mut Ctx, value: &Value) -> OpResult<Redirect> {
    let text = usv(ctx, value)?;
    match text.as_str() {
        "follow" => Ok(Redirect::Follow),
        "error" => Ok(Redirect::Error),
        "manual" => Ok(Redirect::Manual),
        _ => Err(enum_error(&text, "RequestRedirect")),
    }
}

/// Whether requests of this realm go through the CORS policy: it is a browsing context and the
/// transport does not apply the policy itself.
fn browsing_context(ctx: &mut Ctx) -> bool {
    match Transport::current(ctx) {
        Some(transport) => transport.browser_origin(ctx).is_some(),
        None => false,
    }
}

fn header_op(error: HeaderError) -> OpError {
    error.into_op()
}

fn existing_headers(ctx: &mut Ctx, value: &Value) -> Option<SharedHeaders> {
    ctx.with_instance::<bindings::Headers, _>(value, |headers| headers.data.clone())
        .ok()
}

/// The body described by a `BodyInit` and the `Content-Type` it implies; `None` for no body.
fn init_body(ctx: &mut Ctx, value: &Value) -> OpResult<Option<(Body, Option<String>)>> {
    if matches!(value, Value::Undefined | Value::Null) {
        return Ok(None);
    }
    if is_readable_stream(ctx, value) {
        if stream_locked(ctx, value) || stream_disturbed(ctx, value) {
            return Err(OpError::type_error(
                "body stream is locked or already disturbed",
            ));
        }
        return Ok(Some((Body::stream(value.clone()), None)));
    }
    let ExtractedBody {
        bytes,
        content_type,
    } = extract_body(ctx, value)?;
    Ok(Some((Body::bytes(bytes), content_type)))
}

fn default_content_type(data: &SharedHeaders, content_type: Option<String>) -> OpResult<()> {
    let Some(content_type) = content_type else {
        return Ok(());
    };
    let missing = !data.borrow().has("content-type").map_err(header_op)?;
    if missing {
        data.borrow_mut()
            .append("content-type", &content_type)
            .map_err(header_op)?;
    }
    Ok(())
}

fn normalize_method(method: &str) -> String {
    let upper = method.to_ascii_uppercase();
    if NORMALIZED_METHODS.contains(&upper.as_str()) {
        upper
    } else {
        method.to_owned()
    }
}

fn parse_referrer(ctx: &mut Ctx, value: &Value) -> OpResult<String> {
    let text = usv(ctx, value)?;
    if text.is_empty() {
        return Ok(String::new());
    }
    let base = document_base(ctx);
    let url = lumen_common::url::parse(&text, base.as_deref())
        .map_err(|_| OpError::type_error(format!("Referrer '{text}' is not a valid URL.")))?;
    let href = url.href();
    if href == "about:client" {
        return Ok(href);
    }
    let same_origin = base
        .as_deref()
        .and_then(|base| lumen_common::url::parse_url(base, None))
        .is_some_and(|base| base.origin() == url.origin());
    Ok(if same_origin { href } else { "about:client".into() })
}

/// What `new Request(input)` takes from an existing request.
struct Inherited {
    method: String,
    url: String,
    headers: HeadersData,
    signal_source: Option<Value>,
    body: BodyCell,
    mode: Mode,
    credentials: Credentials,
    redirect: Redirect,
    extra: Extra,
}

fn inherit(ctx: &mut Ctx, input: &Value) -> Option<Inherited> {
    ctx.with_instance::<bindings::Request, _>(input, |request| Inherited {
        method: request.method.clone(),
        url: request.url.clone(),
        headers: request.headers.borrow().copy_with(Guard::None),
        signal_source: request.signal_source.clone(),
        body: request.body.clone(),
        mode: request.mode,
        credentials: request.credentials,
        redirect: request.redirect,
        extra: request.extra.clone(),
    })
    .ok()
}

/// The steps of the `Request` constructor.
fn build_request(ctx: &mut Ctx, input: &Value, init: &Value) -> OpResult<bindings::Request> {
    let init = Dictionary::read(init, "RequestInit")?;
    let init_body_value = init.get(ctx, "body")?;
    let init_cache = init.get(ctx, "cache")?;
    let init_credentials = init.get(ctx, "credentials")?;
    let init_duplex = init.get(ctx, "duplex")?;
    let init_headers = init.get(ctx, "headers")?;
    let init_integrity = init.get(ctx, "integrity")?;
    let init_keepalive = init.get(ctx, "keepalive")?;
    let init_method = init.get(ctx, "method")?;
    let init_mode = init.get(ctx, "mode")?;
    let init_redirect = init.get(ctx, "redirect")?;
    let init_referrer = init.get(ctx, "referrer")?;
    let init_referrer_policy = init.get(ctx, "referrerPolicy")?;
    let init_signal = init.get(ctx, "signal")?;
    let init_window = init.get(ctx, "window")?;

    let inherited = inherit(ctx, input);
    let (mut method, url, inherited_list, mut signal_source, input_body, mut mode, mut credentials, mut redirect, mut extra) =
        match inherited {
            Some(parent) => (
                parent.method,
                parent.url,
                Some(parent.headers),
                parent.signal_source,
                Some(parent.body),
                parent.mode,
                parent.credentials,
                parent.redirect,
                parent.extra,
            ),
            None => {
                let text = usv(ctx, input)?;
                let base = document_base(ctx);
                let url = lumen_common::url::parse(&text, base.as_deref())
                    .map_err(|_| OpError::type_error(format!("Failed to parse URL from {text}")))?;
                if !url.username.is_empty() || !url.password.is_empty() {
                    return Err(OpError::type_error(
                        "Request cannot be constructed from a URL that includes credentials",
                    ));
                }
                (
                    "GET".to_owned(),
                    url.href(),
                    None,
                    None,
                    None,
                    Mode::Cors,
                    Credentials::SameOrigin,
                    Redirect::Follow,
                    Extra::default(),
                )
            }
        };

    if !matches!(init_window, Value::Undefined | Value::Null) {
        return Err(OpError::type_error("Request constructor: window must be null"));
    }
    if !matches!(init_referrer, Value::Undefined) {
        extra.referrer = parse_referrer(ctx, &init_referrer)?;
    }
    if !matches!(init_referrer_policy, Value::Undefined) {
        extra.referrer_policy =
            parse_name(ctx, &init_referrer_policy, REFERRER_POLICIES, "ReferrerPolicy")?;
    }
    if !matches!(init_mode, Value::Undefined) {
        mode = parse_mode(ctx, &init_mode)?;
    }
    if !matches!(init_credentials, Value::Undefined) {
        credentials = parse_credentials(ctx, &init_credentials)?;
    }
    if !matches!(init_cache, Value::Undefined) {
        extra.cache = parse_name(ctx, &init_cache, CACHE_MODES, "RequestCache")?;
    }
    if !matches!(init_redirect, Value::Undefined) {
        redirect = parse_redirect(ctx, &init_redirect)?;
    }
    if !matches!(init_integrity, Value::Undefined) {
        extra.integrity = usv(ctx, &init_integrity)?;
    }
    if !matches!(init_keepalive, Value::Undefined) {
        extra.keepalive = ctx.to_boolean(&init_keepalive);
    }
    if !matches!(init_method, Value::Undefined) {
        let text = byte_string(ctx, &init_method)?;
        if !super::is_token(&text) {
            return Err(OpError::type_error(format!("'{text}' is not a valid HTTP method.")));
        }
        if FORBIDDEN_METHODS.contains(&text.to_ascii_uppercase().as_str()) {
            return Err(OpError::type_error(format!("'{text}' HTTP method is unsupported.")));
        }
        method = normalize_method(&text);
    }
    if !matches!(init_signal, Value::Undefined) {
        signal_source = match init_signal {
            Value::Null => None,
            signal if signal_state(ctx, &signal).is_some() => Some(signal),
            _ => {
                return Err(OpError::type_error(
                    "Failed to construct 'Request': member signal is not of type AbortSignal.",
                ))
            }
        };
    }
    let browser = browsing_context(ctx);
    if browser && mode == Mode::NoCors && !matches!(method.as_str(), "GET" | "HEAD" | "POST") {
        return Err(OpError::type_error(format!(
            "'{method}' is unsupported in no-cors mode."
        )));
    }

    let guard = match (browser, mode) {
        (false, _) => Guard::None,
        (true, Mode::NoCors) => Guard::RequestNoCors,
        (true, _) => Guard::Request,
    };
    let headers = Rc::new(RefCell::new(match (&init_headers, &inherited_list) {
        (Value::Undefined, Some(list)) => list.copy_with(guard),
        _ => HeadersData::new(guard),
    }));
    if !matches!(init_headers, Value::Undefined) {
        fill(ctx, &headers, &init_headers, existing_headers)?;
    }

    let supplied = init_body(ctx, &init_body_value)?;
    let has_body = supplied.is_some()
        || input_body
            .as_ref()
            .is_some_and(|body| !is_null(body));
    if has_body && matches!(method.as_str(), "GET" | "HEAD") {
        return Err(OpError::type_error(
            "Request with GET/HEAD method cannot have body.",
        ));
    }
    if !matches!(init_duplex, Value::Undefined) {
        parse_name(ctx, &init_duplex, &["half"], "RequestDuplex")?;
    }
    let body = match supplied {
        Some((body, content_type)) => {
            if matches!(body.source, Source::Stream) && matches!(init_duplex, Value::Undefined) {
                return Err(OpError::type_error(
                    "RequestInit: duplex option is required when sending a body.",
                ));
            }
            default_content_type(&headers, content_type)?;
            body.cell()
        }
        None => match input_body {
            Some(input_body) if !is_null(&input_body) => {
                if unusable(ctx, &input_body) {
                    return Err(OpError::type_error(
                        "Cannot construct a Request with a Request object that has already been used.",
                    ));
                }
                transfer(&input_body).cell()
            }
            _ => Body::null().cell(),
        },
    };

    Ok(bindings::Request {
        method,
        url,
        headers,
        headers_value: RefCell::new(None),
        signal_source,
        signal: RefCell::new(None),
        follow: RefCell::new(None),
        body,
        mode,
        credentials,
        redirect,
        extra,
    })
}

fn valid_reason_phrase(text: &str) -> bool {
    text.chars()
        .all(|c| matches!(c, '\t' | ' '..='~') || ('\u{80}'..='\u{ff}').contains(&c))
}

/// The steps of the `Response` constructor over an extracted body and the `ResponseInit`.
fn build_response(
    ctx: &mut Ctx,
    supplied: Option<(Body, Option<String>)>,
    init: &Value,
) -> OpResult<bindings::Response> {
    let init = Dictionary::read(init, "ResponseInit")?;
    let init_headers = init.get(ctx, "headers")?;
    let init_status = init.get(ctx, "status")?;
    let init_status_text = init.get(ctx, "statusText")?;
    let status = match init_status {
        Value::Undefined => 200,
        value => {
            let number = ctx.coerce_number(&value).map_err(OpError::thrown)?;
            if !number.is_finite() || number.trunc() < 0.0 || number.trunc() > 65535.0 {
                return Err(OpError::type_error(
                    "Response constructor: init[\"status\"] must be an integer between 0 and 65535.",
                ));
            }
            let status = number.trunc() as u16;
            if !(200..=599).contains(&status) {
                return Err(OpError::range_error(
                    "init[\"status\"] must be in the range of 200 to 599, inclusive.",
                ));
            }
            status
        }
    };
    let status_text = match init_status_text {
        Value::Undefined => String::new(),
        value => {
            let text = byte_string(ctx, &value)?;
            if !valid_reason_phrase(&text) {
                return Err(OpError::type_error(
                    "Response constructor: Invalid statusText",
                ));
            }
            text
        }
    };
    // Forbidden response header names only apply to responses a browsing context's script sees;
    // a server builds `Set-Cookie` itself.
    let guard = if browsing_context(ctx) { Guard::Response } else { Guard::None };
    let headers = HeadersData::shared(guard);
    fill(ctx, &headers, &init_headers, existing_headers)?;
    let supplied = match supplied {
        // Servers outside a browsing context (Bun, Lumen.serve handlers) write
        // `new Response("", { status: 204 })`; an empty body of a null body status is no body.
        Some((body, _))
            if NULL_BODY_STATUSES.contains(&status)
                && body.is_empty_bytes()
                && !browsing_context(ctx) =>
        {
            None
        }
        other => other,
    };
    let body = match supplied {
        Some((body, content_type)) => {
            if NULL_BODY_STATUSES.contains(&status) {
                return Err(OpError::type_error(format!(
                    "Response constructor: Invalid response status code {status}"
                )));
            }
            default_content_type(&headers, content_type)?;
            body.cell()
        }
        None => Body::null().cell(),
    };
    Ok(bindings::Response {
        kind: Kind::Default,
        status,
        status_text,
        url: String::new(),
        redirected: false,
        headers,
        headers_value: RefCell::new(None),
        body,
    })
}

#[derive(Clone, Copy)]
enum Consume {
    Text,
    Json,
    ArrayBuffer,
    Bytes,
    Blob,
    FormData,
}

fn boundary_of(content_type: &str) -> Option<String> {
    let lower = content_type.to_ascii_lowercase();
    let at = lower.find("boundary=")?;
    let rest = &content_type[at + "boundary=".len()..];
    let end = rest.find(';').unwrap_or(rest.len());
    let boundary = rest[..end].trim().trim_matches('"');
    (!boundary.is_empty()).then(|| boundary.to_owned())
}

fn convert(ctx: &mut Ctx, kind: Consume, bytes: Vec<u8>, content_type: String) -> OpResult<Value> {
    match kind {
        Consume::Text => Ok(Value::from_string(decode_text(&bytes, Some("utf-8")))),
        Consume::Json => {
            let text = Value::from_string(decode_text(&bytes, Some("utf-8")));
            let global = ctx.global_object();
            let json = ctx.member_get(&global, "JSON").map_err(OpError::thrown)?;
            let parse = ctx.member_get(&json, "parse").map_err(OpError::thrown)?;
            ctx.invoke(parse, json, &[text]).map_err(OpError::thrown)
        }
        Consume::ArrayBuffer => Ok(ctx.make_array_buffer_from(bytes)),
        Consume::Bytes => crate::blob::uint8_array_from_vec(ctx, bytes),
        Consume::Blob => Ok(new_blob(ctx, bytes, &content_type)),
        Consume::FormData => match essence_of(&content_type).as_str() {
            "application/x-www-form-urlencoded" => {
                let form = new_form_data(ctx);
                let text = decode_text(&bytes, Some("utf-8"));
                for (name, value) in lumen_common::url::form_urlencoded_parse(&text) {
                    append_text(ctx, &form, &name, &value)?;
                }
                Ok(form)
            }
            "multipart/form-data" => match boundary_of(&content_type) {
                Some(boundary) => decode_multipart(ctx, &Bytes::new(bytes), &boundary),
                None => Err(OpError::type_error(
                    "formData(): multipart/form-data without a boundary",
                )),
            },
            _ => Err(OpError::type_error(format!(
                "formData(): unsupported content-type '{content_type}'"
            ))),
        },
    }
}

/// The body-mixin read: a promise of the converted bytes.
fn consume(
    ctx: &mut Ctx,
    body: BodyCell,
    headers: SharedHeaders,
    kind: Consume,
) -> OpResult<Value> {
    let deferred = Deferred::new(ctx);
    let promise = deferred.promise();
    let content_type = headers.borrow().value_of("content-type").unwrap_or_default();
    if is_null(&body) {
        match convert(ctx, kind, Vec::new(), content_type) {
            Ok(value) => deferred.resolve(ctx, value),
            Err(error) => deferred.reject(ctx, error),
        }
        return Ok(promise);
    }
    if unusable(ctx, &body) {
        deferred.reject(ctx, OpError::type_error("body already consumed or locked"));
        return Ok(promise);
    }
    read_all(
        ctx,
        &body,
        Box::new(move |ctx, result| match result {
            Ok(bytes) => match convert(ctx, kind, bytes, content_type) {
                Ok(value) => deferred.resolve(ctx, value),
                Err(error) => deferred.reject(ctx, error),
            },
            Err(reason) => deferred.reject(ctx, OpError::thrown(reason)),
        }),
    );
    Ok(promise)
}

/// A fetch in flight, and then its response body until that ends.
struct FetchState {
    deferred: RefCell<Option<Deferred>>,
    control: RefCell<Option<RequestControl>>,
    drain: RefCell<Option<Rc<Drain>>>,
    net: RefCell<Option<Weak<NetBody>>>,
    step: RefCell<Option<Step>>,
    aborted: Cell<bool>,
}

impl FetchState {
    fn reject(&self, ctx: &mut Ctx, reason: Value) {
        let deferred = self.deferred.borrow_mut().take();
        if let Some(deferred) = deferred {
            deferred.reject(ctx, OpError::thrown(reason));
        }
    }

    fn abort(&self, ctx: &mut Ctx, reason: Value) {
        self.aborted.set(true);
        let deferred = self.deferred.borrow_mut().take();
        if let Some(deferred) = deferred {
            let control = self.control.borrow_mut().take();
            if let Some(control) = control {
                control.abort(ctx);
            }
            let drain = self.drain.borrow_mut().take();
            if let Some(drain) = drain {
                drain.cancel(ctx, reason.clone());
            }
            deferred.reject(ctx, OpError::thrown(reason));
            return;
        }
        let net = self.net.borrow().as_ref().and_then(Weak::upgrade);
        if let Some(net) = net {
            net.abort(ctx, reason);
        }
    }
}

fn run_fetch(ctx: &mut Ctx, request: bindings::Request) -> OpResult<Value> {
    let deferred = Deferred::new(ctx);
    let promise = deferred.promise();
    let signal = request.signal_source.clone();
    let signal_state = signal.as_ref().and_then(|signal| signal_state(ctx, signal));
    if let Some(aborted) = signal_state.as_ref().filter(|state| state.aborted.get()) {
        let reason = aborted.reason.borrow().clone();
        deferred.reject(ctx, OpError::thrown(reason));
        return Ok(promise);
    }
    let state = Rc::new(FetchState {
        deferred: RefCell::new(Some(deferred)),
        control: RefCell::new(None),
        drain: RefCell::new(None),
        net: RefCell::new(None),
        step: RefCell::new(None),
        aborted: Cell::new(false),
    });
    if let Some(signal_state) = &signal_state {
        let weak = Rc::downgrade(&state);
        let step: Step = Rc::new(move |ctx: &mut Ctx, reason: &Value| {
            if let Some(state) = weak.upgrade() {
                state.abort(ctx, reason.clone());
            }
        });
        add_owned_step(signal_state, &step);
        *state.step.borrow_mut() = Some(step);
    }
    let spec = RequestSpec {
        method: request.method.clone(),
        url: request.url.clone(),
        headers: request.headers.borrow().sorted_combined(),
        body: None,
        mode: request.mode,
        credentials: request.credentials,
        redirect: request.redirect,
        observe_upload: false,
        force_preflight: false,
    };
    if is_null(&request.body) {
        launch(ctx, &state, spec);
        return Ok(promise);
    }
    let waiting = state.clone();
    let drain = read_all(
        ctx,
        &request.body,
        Box::new(move |ctx, result| match result {
            Ok(bytes) => {
                let mut spec = spec;
                spec.body = Some(bytes);
                launch(ctx, &waiting, spec);
            }
            Err(reason) => waiting.reject(ctx, reason),
        }),
    );
    *state.drain.borrow_mut() = Some(drain);
    Ok(promise)
}

fn launch(ctx: &mut Ctx, state: &Rc<FetchState>, spec: RequestSpec) {
    if state.aborted.get() || state.deferred.borrow().is_none() {
        return;
    }
    let owner = state.clone();
    let control = start(ctx, spec, move |ctx, result| on_head(ctx, &owner, result));
    *state.control.borrow_mut() = Some(control);
}

fn on_head(ctx: &mut Ctx, state: &Rc<FetchState>, result: Result<Head, super::Failure>) {
    let deferred = state.deferred.borrow_mut().take();
    let Some(deferred) = deferred else {
        if let Ok(head) = result {
            head.body.cancel(ctx);
        }
        return;
    };
    state.control.borrow_mut().take();
    state.drain.borrow_mut().take();
    match result {
        Err(failure) => {
            let reason = failure_value(ctx, &failure);
            deferred.reject(ctx, OpError::thrown(reason));
        }
        Ok(head) => match response_from_head(ctx, state, head) {
            Ok(response) => deferred.resolve(ctx, response),
            Err(error) => deferred.reject(ctx, error),
        },
    }
}

fn response_from_head(ctx: &mut Ctx, state: &Rc<FetchState>, head: Head) -> OpResult<Value> {
    let kind = match head.kind {
        ResponseKind::Basic => Kind::Basic,
        ResponseKind::Cors => Kind::Cors,
        ResponseKind::Opaque => Kind::Opaque,
        ResponseKind::OpaqueRedirect => Kind::OpaqueRedirect,
    };
    let opaque = matches!(kind, Kind::Opaque | Kind::OpaqueRedirect);
    let headers = if opaque {
        HeadersData::new(Guard::Immutable)
    } else {
        HeadersData::from_pairs(&head.headers, Guard::Immutable)
    };
    let body = match head.body {
        _ if opaque || NULL_BODY_STATUSES.contains(&head.status) => {
            head.body.cancel(ctx);
            Body::null()
        }
        ResponseBody::None => Body::null(),
        ResponseBody::Bytes(bytes) => Body::bytes(bytes),
        ResponseBody::Reader(reader) => {
            let net = NetBody::new(reader);
            let owner: Rc<dyn Any> = state.clone();
            net.keep_alive(owner);
            *state.net.borrow_mut() = Some(Rc::downgrade(&net));
            Body::net(net)
        }
    };
    let response = bindings::Response {
        kind,
        status: head.status,
        status_text: head.status_text,
        url: if opaque { String::new() } else { without_fragment(&head.url) },
        redirected: head.redirected,
        headers: Rc::new(RefCell::new(headers)),
        headers_value: RefCell::new(None),
        body: body.cell(),
    };
    new_response(ctx, response)
}

fn new_request(ctx: &mut Ctx, request: bindings::Request) -> OpResult<Value> {
    let value = ctx.new_instance(request);
    ctx.set_native_identity_owner::<bindings::Request>(&value)?;
    Ok(value)
}

fn new_response(ctx: &mut Ctx, response: bindings::Response) -> OpResult<Value> {
    let value = ctx.new_instance(response);
    ctx.set_native_identity_owner::<bindings::Response>(&value)?;
    Ok(value)
}

/// The script-visible `Headers` of a request or response, created on first use.
fn headers_object(
    ctx: &mut Ctx,
    existing: Option<Value>,
    data: SharedHeaders,
    store: impl FnOnce(&mut Ctx, Value) -> OpResult<()>,
) -> OpResult<Value> {
    if let Some(existing) = existing {
        return Ok(existing);
    }
    let object = ctx.new_instance(bindings::Headers { data });
    store(ctx, object.clone())?;
    Ok(object)
}

/// A `Request` as an HTTP server receives it: the headers are the wire's, whatever the realm's
/// guard rules, and the body is the bytes read from the socket (`GET` and `HEAD` have none).
pub fn server_request(
    ctx: &mut Ctx,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Vec<u8>,
) -> OpResult<Value> {
    if !super::is_token(method) {
        return Err(OpError::type_error(format!("'{method}' is not a valid HTTP method.")));
    }
    if FORBIDDEN_METHODS.contains(&method.to_ascii_uppercase().as_str()) {
        return Err(OpError::type_error(format!("'{method}' HTTP method is unsupported.")));
    }
    let method = normalize_method(method);
    let parsed = lumen_common::url::parse(url, None)
        .map_err(|_| OpError::type_error(format!("Failed to parse URL from {url}")))?;
    if !parsed.username.is_empty() || !parsed.password.is_empty() {
        return Err(OpError::type_error(
            "Request cannot be constructed from a URL that includes credentials",
        ));
    }
    let body = if body.is_empty() || matches!(method.as_str(), "GET" | "HEAD") {
        Body::null()
    } else {
        Body::bytes(body)
    };
    new_request(
        ctx,
        bindings::Request {
            method,
            url: parsed.href(),
            headers: Rc::new(RefCell::new(HeadersData::from_pairs(headers, Guard::None))),
            headers_value: RefCell::new(None),
            signal_source: None,
            signal: RefCell::new(None),
            follow: RefCell::new(None),
            body: body.cell(),
            mode: Mode::Cors,
            credentials: Credentials::SameOrigin,
            redirect: Redirect::Follow,
            extra: Extra::default(),
        },
    )
}

/// The combined value of header `name` of a `Request`, `None` for another value or no header.
pub fn request_header(ctx: &mut Ctx, request: &Value, name: &str) -> Option<String> {
    ctx.with_instance::<bindings::Request, _>(request, |request| {
        request.headers.borrow().value_of(&name.to_ascii_lowercase())
    })
    .ok()
    .flatten()
}

/// The entries of a `Headers` object as iteration yields them, `None` for another value.
pub fn headers_entries(ctx: &mut Ctx, headers: &Value) -> Option<Vec<(String, String)>> {
    ctx.with_instance::<bindings::Headers, _>(headers, |headers| {
        headers.data.borrow().sorted_combined()
    })
    .ok()
}

/// The status line and headers of a `Response`, as a server writes them.
pub struct ServedResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
}

/// The head of `response`, `None` when it is not a `Response`.
pub fn served_response(ctx: &mut Ctx, response: &Value) -> Option<ServedResponse> {
    ctx.with_instance::<bindings::Response, _>(response, |response| ServedResponse {
        status: response.status,
        status_text: response.status_text.clone(),
        headers: response.headers.borrow().sorted_combined(),
    })
    .ok()
}

/// Read the body of a `Response` to its end for a server to write. A body that is missing, was
/// already used or is locked, or whose stream fails, reads as empty. `done` may run before this
/// returns when the bytes are in memory.
pub fn read_served_body(
    ctx: &mut Ctx,
    response: &Value,
    done: Box<dyn FnOnce(&mut Ctx, Vec<u8>)>,
) {
    let body = ctx
        .with_instance::<bindings::Response, _>(response, |response| response.body.clone())
        .ok();
    let Some(body) = body.filter(|body| !is_null(body) && !unusable(ctx, body)) else {
        return done(ctx, Vec::new());
    };
    read_all(
        ctx,
        &body,
        Box::new(move |ctx, result| done(ctx, result.unwrap_or_default())),
    );
}

fn rejected(ctx: &mut Ctx, error: OpError) -> Value {
    let deferred = Deferred::new(ctx);
    let promise = deferred.promise();
    deferred.reject(ctx, error);
    promise
}

#[lumen_bind::module(name = "webFetch")]
pub mod bindings {
    use super::*;
    use crate::webidl::invalid_this;
    use lumen::embed::NativeIdentityOwner;
    use crate::messaging::Owned;
    use lumen_bind::This;

    #[class(name = "Headers", hint(js(webidl, invalid_this)))]
    pub struct Headers {
        pub(crate) data: SharedHeaders,
    }

    #[class(name = "Headers Iterator", skip(js), hint(js(webidl, iterator)))]
    pub struct HeadersIterator {
        data: SharedHeaders,
        kind: IterKind,
        index: usize,
        cache: Option<(u64, Vec<(String, String)>)>,
    }

    #[derive(Clone, Copy)]
    enum IterKind {
        Keys,
        Values,
        Entries,
    }

    #[class(name = "Request", hint(js(webidl, invalid_this)))]
    pub struct Request {
        pub(super) method: String,
        pub(super) url: String,
        pub(super) headers: SharedHeaders,
        pub(super) headers_value: RefCell<Option<Value>>,
        pub(super) signal_source: Option<Value>,
        pub(super) signal: RefCell<Option<Value>>,
        pub(super) follow: RefCell<Option<Step>>,
        pub(super) body: BodyCell,
        pub(super) mode: Mode,
        pub(super) credentials: Credentials,
        pub(super) redirect: Redirect,
        pub(super) extra: Extra,
    }

    #[class(name = "Response", hint(js(webidl, invalid_this)))]
    pub struct Response {
        pub(super) kind: Kind,
        pub(super) status: u16,
        pub(super) status_text: String,
        pub(super) url: String,
        pub(super) redirected: bool,
        pub(super) headers: SharedHeaders,
        pub(super) headers_value: RefCell<Option<Value>>,
        pub(super) body: BodyCell,
    }

    // ---- Headers -----------------------------------------------------------------------------

    #[methods]
    impl Headers {
        #[constructor]
        fn constructor(ctx: &mut Ctx, #[default(Value::Undefined)] init: Value) -> OpResult<Headers> {
            let data = HeadersData::shared(Guard::None);
            fill(ctx, &data, &init, existing_headers)?;
            Ok(Headers { data })
        }

        #[method(hint(js(missing_message = "The \"name\" and \"value\" arguments must be specified", missing_code = "ERR_MISSING_ARGS")))]
        fn append(&self, name: ByteStr, value: ByteStr) -> OpResult<()> {
            self.data
                .borrow_mut()
                .append(&name.0, &value.0)
                .map_err(header_op)
        }

        #[method(hint(js(missing_message = "The \"name\" argument must be specified", missing_code = "ERR_MISSING_ARGS")))]
        fn delete(&self, name: ByteStr) -> OpResult<()> {
            self.data.borrow_mut().delete(&name.0).map_err(header_op)
        }

        #[method(hint(js(missing_message = "The \"name\" argument must be specified", missing_code = "ERR_MISSING_ARGS")))]
        fn get(&self, name: ByteStr) -> OpResult<Value> {
            let value = self.data.borrow().get(&name.0).map_err(header_op)?;
            Ok(value.map_or(Value::Null, Value::from_string))
        }

        fn get_set_cookie(&self, ctx: &mut Ctx) -> Value {
            let cookies = self.data.borrow().set_cookies();
            let values = cookies.into_iter().map(Value::from_string).collect();
            ctx.make_array(values)
        }

        #[method(hint(js(missing_message = "The \"name\" argument must be specified", missing_code = "ERR_MISSING_ARGS")))]
        fn has(&self, name: ByteStr) -> OpResult<bool> {
            self.data.borrow().has(&name.0).map_err(header_op)
        }

        #[method(hint(js(missing_message = "The \"name\" and \"value\" arguments must be specified", missing_code = "ERR_MISSING_ARGS")))]
        fn set(&self, name: ByteStr, value: ByteStr) -> OpResult<()> {
            self.data
                .borrow_mut()
                .set(&name.0, &value.0)
                .map_err(header_op)
        }

        #[method(hint(js(also_iterator)))]
        fn entries(&self) -> HeadersIterator {
            self.iterator(IterKind::Entries)
        }

        fn keys(&self) -> HeadersIterator {
            self.iterator(IterKind::Keys)
        }

        fn values(&self) -> HeadersIterator {
            self.iterator(IterKind::Values)
        }

        #[method(hint(js(
            missing_message = "The \"callback\" argument must be of type function. Received undefined",
            missing_code = "ERR_INVALID_ARG_TYPE"
        )))]
        fn for_each(
            this: This<Value>,
            ctx: &mut Ctx,
            callback: Value,
            #[default(Value::Undefined)] this_arg: Value,
        ) -> OpResult<()> {
            let data = ctx
                .with_instance::<Headers, _>(&this, |headers| headers.data.clone())
                .map_err(|_| invalid_this("Headers"))?;
            if !callback.is_callable() {
                return Err(crate::webidl::invalid_arg_type(
                    ctx,
                    "callback",
                    "of type function",
                    &callback,
                ));
            }
            let mut index = 0;
            loop {
                let entry = data.borrow().sorted_combined().into_iter().nth(index);
                let Some((name, value)) = entry else {
                    return Ok(());
                };
                ctx.invoke(
                    callback.clone(),
                    this_arg.clone(),
                    &[Value::from_string(value), Value::from_string(name), this.0.clone()],
                )
                .map_err(OpError::thrown)?;
                index += 1;
            }
        }
    }

    impl Headers {
        fn iterator(&self, kind: IterKind) -> HeadersIterator {
            HeadersIterator {
                data: self.data.clone(),
                kind,
                index: 0,
                cache: None,
            }
        }
    }

    #[methods]
    impl HeadersIterator {
        #[proto(next)]
        fn next(this: This<Value>, ctx: &mut Ctx) -> OpResult<Option<Value>> {
            let state = ctx
                .instance_data::<HeadersIterator>(&this)
                .ok_or_else(|| invalid_this("HeadersIterator"))?;
            let mut state = state.borrow_mut();
            let version = state.data.borrow().version();
            if !matches!(&state.cache, Some((known, _)) if *known == version) {
                let list = state.data.borrow().sorted_combined();
                state.cache = Some((version, list));
            }
            let entry = state
                .cache
                .as_ref()
                .and_then(|(_, list)| list.get(state.index))
                .cloned();
            let Some((name, value)) = entry else {
                return Ok(None);
            };
            state.index += 1;
            Ok(Some(match state.kind {
                IterKind::Keys => Value::from_string(name),
                IterKind::Values => Value::from_string(value),
                IterKind::Entries => {
                    ctx.make_array(vec![Value::from_string(name), Value::from_string(value)])
                }
            }))
        }
    }

    // ---- Request -----------------------------------------------------------------------------

    impl NativeIdentityOwner for Request {
        const TRACES_NATIVE_VALUES: bool = true;

        fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}

        fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
            for slot in [&self.headers_value, &self.signal] {
                if let Ok(slot) = slot.try_borrow() {
                    if let Some(value) = &*slot {
                        visit(value);
                    }
                }
            }
            if let Some(source) = &self.signal_source {
                visit(source);
            }
            if let Ok(body) = self.body.try_borrow() {
                body.trace(visit);
            }
        }
    }

    fn request_parts(ctx: &mut Ctx, this: &Value) -> OpResult<(BodyCell, SharedHeaders)> {
        ctx.with_instance::<Request, _>(this, |request| {
            (request.body.clone(), request.headers.clone())
        })
        .map_err(|_| invalid_this("Request"))
    }

    #[methods]
    impl Request {
        #[constructor]
        fn constructor(
            ctx: &mut Ctx,
            input: Value,
            #[default(Value::Undefined)] init: Value,
        ) -> OpResult<Owned<Request>> {
            build_request(ctx, &input, &init).map(Owned)
        }

        #[getter]
        fn method(&self) -> String {
            self.method.clone()
        }

        #[getter]
        fn url(&self) -> String {
            self.url.clone()
        }

        #[getter]
        fn headers(this: This<Value>, ctx: &mut Ctx) -> OpResult<Value> {
            let (existing, data) = ctx
                .with_instance::<Request, _>(&this, |request| {
                    (request.headers_value.borrow().clone(), request.headers.clone())
                })
                .map_err(|_| invalid_this("Request"))?;
            let owner = this.0.clone();
            headers_object(ctx, existing, data, move |ctx, object| {
                ctx.with_instance::<Request, _>(&owner, |request| {
                    *request.headers_value.borrow_mut() = Some(object);
                })
            })
        }

        #[getter]
        fn destination(&self) -> String {
            String::new()
        }

        #[getter]
        fn referrer(&self) -> String {
            self.extra.referrer.clone()
        }

        #[getter]
        fn referrer_policy(&self) -> String {
            self.extra.referrer_policy.clone()
        }

        #[getter]
        fn mode(&self) -> &'static str {
            super::super::transport::mode_name(self.mode)
        }

        #[getter]
        fn credentials(&self) -> &'static str {
            super::super::transport::credentials_name(self.credentials)
        }

        #[getter]
        fn cache(&self) -> String {
            self.extra.cache.clone()
        }

        #[getter]
        fn redirect(&self) -> &'static str {
            super::super::transport::redirect_name(self.redirect)
        }

        #[getter]
        fn integrity(&self) -> String {
            self.extra.integrity.clone()
        }

        #[getter]
        fn keepalive(&self) -> bool {
            self.extra.keepalive
        }

        #[getter]
        fn is_reload_navigation(&self) -> bool {
            false
        }

        #[getter]
        fn is_history_navigation(&self) -> bool {
            false
        }

        #[getter]
        fn signal(this: This<Value>, ctx: &mut Ctx) -> OpResult<Value> {
            let (existing, source) = ctx
                .with_instance::<Request, _>(&this, |request| {
                    (request.signal.borrow().clone(), request.signal_source.clone())
                })
                .map_err(|_| invalid_this("Request"))?;
            if let Some(existing) = existing {
                return Ok(existing);
            }
            let signal = new_signal(ctx)?;
            let step = match &source {
                Some(source) => follow_signal(ctx, source, &signal)?,
                None => None,
            };
            ctx.with_instance::<Request, _>(&this, |request| {
                *request.signal.borrow_mut() = Some(signal.clone());
                *request.follow.borrow_mut() = step;
            })?;
            Ok(signal)
        }

        #[getter]
        fn duplex(&self) -> &'static str {
            "half"
        }

        #[getter]
        fn body(this: This<Value>, ctx: &mut Ctx) -> OpResult<Value> {
            let (body, _) = request_parts(ctx, &this)?;
            stream_value(ctx, &body)
        }

        #[getter]
        fn body_used(this: This<Value>, ctx: &mut Ctx) -> OpResult<bool> {
            let (body, _) = request_parts(ctx, &this)?;
            Ok(body_used(ctx, &body))
        }

        #[method(name = "clone")]
        fn clone_request(this: This<Value>, ctx: &mut Ctx) -> OpResult<Value> {
            let parent = ctx
                .with_instance::<Request, _>(&this, |request| Request {
                    method: request.method.clone(),
                    url: request.url.clone(),
                    headers: Rc::new(RefCell::new(
                        request.headers.borrow().copy_with(request.headers.borrow().guard),
                    )),
                    headers_value: RefCell::new(None),
                    signal_source: request.signal_source.clone(),
                    signal: RefCell::new(None),
                    follow: RefCell::new(None),
                    body: request.body.clone(),
                    mode: request.mode,
                    credentials: request.credentials,
                    redirect: request.redirect,
                    extra: request.extra.clone(),
                })
                .map_err(|_| invalid_this("Request"))?;
            if unusable(ctx, &parent.body) {
                return Err(OpError::type_error(
                    "Failed to execute 'clone' on 'Request': Request body is already used",
                ));
            }
            let body = clone_body(ctx, &parent.body)?.cell();
            new_request(ctx, Request { body, ..parent })
        }

        fn array_buffer(this: This<Value>, ctx: &mut Ctx) -> OpResult<Value> {
            let (body, headers) = request_parts(ctx, &this)?;
            consume(ctx, body, headers, Consume::ArrayBuffer)
        }

        fn blob(this: This<Value>, ctx: &mut Ctx) -> OpResult<Value> {
            let (body, headers) = request_parts(ctx, &this)?;
            consume(ctx, body, headers, Consume::Blob)
        }

        fn bytes(this: This<Value>, ctx: &mut Ctx) -> OpResult<Value> {
            let (body, headers) = request_parts(ctx, &this)?;
            consume(ctx, body, headers, Consume::Bytes)
        }

        fn form_data(this: This<Value>, ctx: &mut Ctx) -> OpResult<Value> {
            let (body, headers) = request_parts(ctx, &this)?;
            consume(ctx, body, headers, Consume::FormData)
        }

        fn json(this: This<Value>, ctx: &mut Ctx) -> OpResult<Value> {
            let (body, headers) = request_parts(ctx, &this)?;
            consume(ctx, body, headers, Consume::Json)
        }

        fn text(this: This<Value>, ctx: &mut Ctx) -> OpResult<Value> {
            let (body, headers) = request_parts(ctx, &this)?;
            consume(ctx, body, headers, Consume::Text)
        }
    }

    // ---- Response ----------------------------------------------------------------------------

    impl NativeIdentityOwner for Response {
        const TRACES_NATIVE_VALUES: bool = true;

        fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}

        fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
            if let Ok(slot) = self.headers_value.try_borrow() {
                if let Some(value) = &*slot {
                    visit(value);
                }
            }
            if let Ok(body) = self.body.try_borrow() {
                body.trace(visit);
            }
        }
    }

    fn response_parts(ctx: &mut Ctx, this: &Value) -> OpResult<(BodyCell, SharedHeaders)> {
        ctx.with_instance::<Response, _>(this, |response| {
            (response.body.clone(), response.headers.clone())
        })
        .map_err(|_| invalid_this("Response"))
    }

    #[methods]
    impl Response {
        #[constructor]
        fn constructor(
            ctx: &mut Ctx,
            #[default(Value::Null)] body: Value,
            #[default(Value::Undefined)] init: Value,
        ) -> OpResult<Owned<Response>> {
            let supplied = init_body(ctx, &body)?;
            build_response(ctx, supplied, &init).map(Owned)
        }

        fn error(ctx: &mut Ctx) -> OpResult<Value> {
            new_response(
                ctx,
                Response {
                    kind: Kind::Error,
                    status: 0,
                    status_text: String::new(),
                    url: String::new(),
                    redirected: false,
                    headers: HeadersData::shared(Guard::Immutable),
                    headers_value: RefCell::new(None),
                    body: Body::null().cell(),
                },
            )
        }

        fn redirect(
            ctx: &mut Ctx,
            url: Value,
            #[default(Value::Undefined)] status: Value,
        ) -> OpResult<Value> {
            let text = usv(ctx, &url)?;
            let base = document_base(ctx);
            let parsed = lumen_common::url::parse(&text, base.as_deref())
                .map_err(|_| OpError::type_error(format!("Failed to parse URL from {text}")))?;
            let status = match status {
                Value::Undefined => 302,
                value => {
                    let number = ctx.coerce_number(&value).map_err(OpError::thrown)?;
                    if !number.is_finite() || !(0.0..=65535.0).contains(&number.trunc()) {
                        return Err(OpError::type_error(
                            "Response.redirect: status must be an integer between 0 and 65535.",
                        ));
                    }
                    number.trunc() as u16
                }
            };
            if !matches!(status, 301 | 302 | 303 | 307 | 308) {
                return Err(OpError::range_error(format!(
                    "Invalid status code {status}"
                )));
            }
            let mut headers = HeadersData::new(Guard::Response);
            headers
                .append("location", &parsed.href())
                .map_err(header_op)?;
            headers.guard = Guard::Immutable;
            new_response(
                ctx,
                Response {
                    kind: Kind::Default,
                    status,
                    status_text: String::new(),
                    url: String::new(),
                    redirected: false,
                    headers: Rc::new(RefCell::new(headers)),
                    headers_value: RefCell::new(None),
                    body: Body::null().cell(),
                },
            )
        }

        #[method(name = "json")]
        fn json_static(
            ctx: &mut Ctx,
            data: Value,
            #[default(Value::Undefined)] init: Value,
        ) -> OpResult<Value> {
            let global = ctx.global_object();
            let json = ctx.member_get(&global, "JSON").map_err(OpError::thrown)?;
            let stringify = ctx.member_get(&json, "stringify").map_err(OpError::thrown)?;
            let text = ctx
                .invoke(stringify, json, &[data])
                .map_err(OpError::thrown)?;
            let Value::Str(text) = text else {
                return Err(OpError::type_error("Value is not JSON serializable"));
            };
            let body = Body::bytes(text.to_string().into_bytes());
            let response = build_response(
                ctx,
                Some((body, Some("application/json".into()))),
                &init,
            )?;
            new_response(ctx, response)
        }

        #[getter(name = "type")]
        fn kind(&self) -> &'static str {
            self.kind.name()
        }

        #[getter]
        fn url(&self) -> String {
            self.url.clone()
        }

        #[getter]
        fn redirected(&self) -> bool {
            self.redirected
        }

        #[getter]
        fn status(&self) -> u16 {
            self.status
        }

        #[getter]
        fn ok(&self) -> bool {
            (200..=299).contains(&self.status)
        }

        #[getter]
        fn status_text(&self) -> String {
            self.status_text.clone()
        }

        #[getter]
        fn headers(this: This<Value>, ctx: &mut Ctx) -> OpResult<Value> {
            let (existing, data) = ctx
                .with_instance::<Response, _>(&this, |response| {
                    (response.headers_value.borrow().clone(), response.headers.clone())
                })
                .map_err(|_| invalid_this("Response"))?;
            let owner = this.0.clone();
            headers_object(ctx, existing, data, move |ctx, object| {
                ctx.with_instance::<Response, _>(&owner, |response| {
                    *response.headers_value.borrow_mut() = Some(object);
                })
            })
        }

        #[getter]
        fn body(this: This<Value>, ctx: &mut Ctx) -> OpResult<Value> {
            let (body, _) = response_parts(ctx, &this)?;
            stream_value(ctx, &body)
        }

        #[getter]
        fn body_used(this: This<Value>, ctx: &mut Ctx) -> OpResult<bool> {
            let (body, _) = response_parts(ctx, &this)?;
            Ok(body_used(ctx, &body))
        }

        #[method(name = "clone")]
        fn clone_response(this: This<Value>, ctx: &mut Ctx) -> OpResult<Value> {
            let parent = ctx
                .with_instance::<Response, _>(&this, |response| Response {
                    kind: response.kind,
                    status: response.status,
                    status_text: response.status_text.clone(),
                    url: response.url.clone(),
                    redirected: response.redirected,
                    headers: Rc::new(RefCell::new(
                        response
                            .headers
                            .borrow()
                            .copy_with(response.headers.borrow().guard),
                    )),
                    headers_value: RefCell::new(None),
                    body: response.body.clone(),
                })
                .map_err(|_| invalid_this("Response"))?;
            if unusable(ctx, &parent.body) {
                return Err(OpError::type_error(
                    "Failed to execute 'clone' on 'Response': Response body is already used",
                ));
            }
            let body = clone_body(ctx, &parent.body)?.cell();
            new_response(ctx, Response { body, ..parent })
        }

        fn array_buffer(this: This<Value>, ctx: &mut Ctx) -> OpResult<Value> {
            let (body, headers) = response_parts(ctx, &this)?;
            consume(ctx, body, headers, Consume::ArrayBuffer)
        }

        fn blob(this: This<Value>, ctx: &mut Ctx) -> OpResult<Value> {
            let (body, headers) = response_parts(ctx, &this)?;
            consume(ctx, body, headers, Consume::Blob)
        }

        fn bytes(this: This<Value>, ctx: &mut Ctx) -> OpResult<Value> {
            let (body, headers) = response_parts(ctx, &this)?;
            consume(ctx, body, headers, Consume::Bytes)
        }

        fn form_data(this: This<Value>, ctx: &mut Ctx) -> OpResult<Value> {
            let (body, headers) = response_parts(ctx, &this)?;
            consume(ctx, body, headers, Consume::FormData)
        }

        fn json(this: This<Value>, ctx: &mut Ctx) -> OpResult<Value> {
            let (body, headers) = response_parts(ctx, &this)?;
            consume(ctx, body, headers, Consume::Json)
        }

        fn text(this: This<Value>, ctx: &mut Ctx) -> OpResult<Value> {
            let (body, headers) = response_parts(ctx, &this)?;
            consume(ctx, body, headers, Consume::Text)
        }
    }

    // ---- fetch -------------------------------------------------------------------------------

    /// `fetch(input, init)`: failures of the request construction and of the exchange reject.
    #[op(hint(js(
        webidl,
        missing_message = "The \"input\" argument must be specified",
        missing_code = "ERR_MISSING_ARGS"
    )))]
    pub fn fetch(
        ctx: &mut Ctx,
        input: Value,
        #[default(Value::Undefined)] init: Value,
    ) -> OpResult<Value> {
        match build_request(ctx, &input, &init) {
            Ok(request) => run_fetch(ctx, request),
            Err(error) => Ok(rejected(ctx, error)),
        }
    }
}
