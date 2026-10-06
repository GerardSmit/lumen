//! The realm's HTTP transport as Rust sees it.
//!
//! A host exposes its transport as `__http`-shaped objects: `request(method, url, headers, body,
//! resolve, reject, redirect, options)` returning a control with `abort()` and the upload
//! counters, `requestSync(method, url, headers, body, options)`, and optionally
//! `policyHandledByHost` and `browserOrigin`. [`Transport`] keeps those objects for one realm and
//! calls them with native callbacks, so no script function is involved.

use crate::realm_services::RealmServices;
use lumen::embed::{Ctx, OpError, Value};
use lumen_common::cors::{Credentials, Mode, Redirect};
use std::{cell::RefCell, rc::Rc};

/// A failed request: the error's `name` and `message`.
#[derive(Clone, Debug)]
pub struct Failure {
    pub name: String,
    pub message: String,
}

impl Failure {
    pub fn network(message: impl Into<String>) -> Self {
        Failure {
            name: "NetworkError".into(),
            message: message.into(),
        }
    }

    fn from_value(ctx: &mut Ctx, error: &Value) -> Self {
        let read = |ctx: &mut Ctx, key: &str| match matches!(error, Value::Obj(_))
            .then(|| ctx.member_get(error, key).ok())
            .flatten()
        {
            Some(Value::Undefined) | None => None,
            Some(value) => ctx.coerce_string(&value).ok().map(|text| text.to_string()),
        };
        match (read(ctx, "name"), read(ctx, "message")) {
            (name, Some(message)) => Failure {
                name: name.unwrap_or_else(|| "Error".into()),
                message,
            },
            (name, None) => {
                let message = ctx
                    .coerce_string(error)
                    .map(|text| text.to_string())
                    .unwrap_or_default();
                Failure {
                    name: name.unwrap_or_else(|| "Error".into()),
                    message,
                }
            }
        }
    }
}

impl From<OpError> for Failure {
    fn from(error: OpError) -> Self {
        Failure {
            name: error.class().to_owned(),
            message: error.message().to_owned(),
        }
    }
}

/// A response body that is not yet read.
pub enum ResponseBody {
    None,
    /// The whole body, when the transport delivers it with the head.
    Bytes(Vec<u8>),
    /// A transport body handle with `read()` (a promise of a `Uint8Array`, `null` at the end) and
    /// `cancel()`.
    Reader(Value),
}

impl ResponseBody {
    /// Release the transport's resources for an unread body.
    pub fn cancel(&self, ctx: &mut Ctx) {
        if let ResponseBody::Reader(reader) = self {
            cancel_reader(ctx, reader);
        }
    }
}

/// Release a transport body handle that is not read to its end.
pub fn cancel_reader(ctx: &mut Ctx, reader: &Value) {
    if let Ok(cancel) = ctx.member_get(reader, "cancel") {
        if cancel.is_callable() {
            let _ = ctx.invoke(cancel, reader.clone(), &[]);
        }
    }
}

/// One transport response head.
pub(crate) struct Raw {
    pub status: u16,
    pub status_text: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub kind: Option<String>,
    pub redirected: Option<bool>,
    pub body: ResponseBody,
}

pub(crate) type RawCallback = Box<dyn FnOnce(&mut Ctx, Result<Raw, Failure>)>;

/// The request line, headers and body of one transport request.
pub(crate) struct Head<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub headers: &'a [(String, String)],
    pub body: Option<&'a [u8]>,
}

/// The fetch options handed to the transport with a request.
pub(crate) struct SendOptions {
    pub mode: Mode,
    pub credentials: Credentials,
    pub redirect: Redirect,
    pub upload_progress: bool,
    pub force_preflight: bool,
}

pub(crate) fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Cors => "cors",
        Mode::NoCors => "no-cors",
        Mode::SameOrigin => "same-origin",
    }
}

pub(crate) fn credentials_name(credentials: Credentials) -> &'static str {
    match credentials {
        Credentials::Omit => "omit",
        Credentials::SameOrigin => "same-origin",
        Credentials::Include => "include",
    }
}

pub(crate) fn redirect_name(redirect: Redirect) -> &'static str {
    match redirect {
        Redirect::Follow => "follow",
        Redirect::Error => "error",
        Redirect::Manual => "manual",
    }
}

/// A synchronous request for [`Transport::request_sync`].
pub struct SyncRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub mode: Mode,
    pub credentials: Credentials,
    pub force_preflight: bool,
    pub timeout_ms: u32,
    /// The requesting origin when the policy is applied by the transport.
    pub origin: Option<String>,
}

/// The response of a synchronous request.
pub struct SyncResponse {
    pub status: u16,
    pub status_text: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// The HTTP transport objects of one realm.
pub struct Transport {
    request: Value,
    sync: Value,
    policy: Value,
}

impl Transport {
    /// Register the transport of the current realm. `request` has `request()`; `sync` has
    /// `requestSync()`; `policy` (the same object or another) may have `policyHandledByHost` and
    /// `browserOrigin`. `undefined` stands for an absent object.
    pub fn install(ctx: &mut Ctx, request: Value, sync: Value, policy: Value) {
        RealmServices::replace_current(
            ctx,
            Transport {
                request,
                sync,
                policy,
            },
        );
    }

    /// The current realm's transport.
    pub fn current(ctx: &mut Ctx) -> Option<Rc<Transport>> {
        RealmServices::<Transport>::current(ctx)
    }

    fn callable_member(&self, ctx: &mut Ctx, object: &Value, name: &str) -> Option<Value> {
        if !matches!(object, Value::Obj(_)) {
            return None;
        }
        ctx.member_get(object, name).ok().filter(Value::is_callable)
    }

    /// Whether the transport applies CORS and credentials policy to every request itself.
    pub(crate) fn policy_handled_by_host(&self, ctx: &mut Ctx) -> bool {
        for object in [&self.policy, &self.request] {
            if !matches!(object, Value::Obj(_)) {
                continue;
            }
            match ctx.member_get(object, "policyHandledByHost") {
                Ok(Value::Bool(true)) => return true,
                Ok(function) if function.is_callable() => {
                    if matches!(
                        ctx.invoke(function, object.clone(), &[]),
                        Ok(Value::Bool(true))
                    ) {
                        return true;
                    }
                }
                _ => {}
            }
        }
        false
    }

    /// The origin the host reports for its browsing context, if it reports one.
    pub(crate) fn native_origin(&self, ctx: &mut Ctx) -> Option<String> {
        let function = self.callable_member(ctx, &self.request, "browserOrigin")?;
        match ctx.invoke(function, self.request.clone(), &[]) {
            Ok(Value::Str(origin)) => Some(origin.to_string()),
            _ => None,
        }
    }

    /// The origin requests are made from when the realm is a browsing context whose transport
    /// leaves CORS and credentials policy to us; `None` when the transport applies the policy
    /// itself or the realm is not a browsing context.
    pub(crate) fn browser_origin(&self, ctx: &mut Ctx) -> Option<String> {
        if self.policy_handled_by_host(ctx) {
            return None;
        }
        if let Some(origin) = self.native_origin(ctx) {
            return Some(origin);
        }
        let global = ctx.global_object();
        let mut in_browser = false;
        for (object, key) in [("document", "URL"), ("location", "href")] {
            let Ok(holder @ Value::Obj(_)) = ctx.member_get(&global, object) else {
                continue;
            };
            in_browser = true;
            if let Some(href) = text_member(ctx, &holder, key) {
                if let Some(url) = lumen_common::url::parse_url(&href, None) {
                    return Some(url.origin());
                }
            }
        }
        in_browser.then(|| "null".to_owned())
    }

    /// Whether a synchronous request is possible.
    pub fn has_sync(&self, ctx: &mut Ctx) -> bool {
        self.callable_member(ctx, &self.sync, "requestSync").is_some()
            || self.callable_member(ctx, &self.request, "requestSync").is_some()
    }

    /// `request(...)`: start one request and call `callback` once with its head or its failure.
    /// Returns the control object. The callback is dropped without a call when the transport
    /// never settles.
    pub(crate) fn send(
        &self,
        ctx: &mut Ctx,
        head: &Head<'_>,
        manual_redirect: bool,
        options: &SendOptions,
        callback: RawCallback,
    ) -> Result<Value, Failure> {
        let request = self
            .callable_member(ctx, &self.request, "request")
            .ok_or_else(|| Failure::network("HTTP transport is unavailable"))?;
        let slot: Rc<RefCell<Option<RawCallback>>> = Rc::new(RefCell::new(Some(callback)));
        let resolve = {
            let slot = slot.clone();
            ctx.new_native_fn(
                "",
                1,
                Rc::new(move |ctx: &mut Ctx, _: Value, args: &[Value]| {
                    let callback = slot.borrow_mut().take();
                    if let Some(callback) = callback {
                        let raw = args
                            .first()
                            .map(|value| parse_raw(ctx, value))
                            .unwrap_or_else(|| Err(Failure::network("empty response")));
                        callback(ctx, raw);
                    }
                    Ok(Value::Undefined)
                }),
            )
        };
        let reject = ctx.new_native_fn(
            "",
            1,
            Rc::new(move |ctx: &mut Ctx, _: Value, args: &[Value]| {
                let callback = slot.borrow_mut().take();
                if let Some(callback) = callback {
                    let error = args.first().cloned().unwrap_or(Value::Undefined);
                    let failure = Failure::from_value(ctx, &error);
                    callback(ctx, Err(failure));
                }
                Ok(Value::Undefined)
            }),
        );
        let headers = header_array(ctx, head.headers);
        let body = match head.body {
            Some(bytes) => ctx.make_uint8array(bytes).map_err(|e| failure_of(ctx, &e))?,
            None => Value::Undefined,
        };
        let option_object = Value::Obj(ctx.new_object());
        for (key, value) in [
            ("mode", Value::str(mode_name(options.mode))),
            ("credentials", Value::str(credentials_name(options.credentials))),
            ("redirect", Value::str(redirect_name(options.redirect))),
            ("uploadProgress", Value::Bool(options.upload_progress)),
            ("forcePreflight", Value::Bool(options.force_preflight)),
        ] {
            let _ = ctx.member_set(&option_object, key, value);
        }
        let arguments = [
            Value::str(head.method),
            Value::str(head.url),
            headers,
            body,
            resolve,
            reject,
            Value::str(if manual_redirect { "manual" } else { "follow" }),
            option_object,
        ];
        ctx.invoke(request, self.request.clone(), &arguments)
            .map_err(|error| failure_of(ctx, &error))
    }

    /// `requestSync(...)`: one blocking request, with the policy applied by the transport.
    pub fn request_sync(
        &self,
        ctx: &mut Ctx,
        request: &SyncRequest,
    ) -> Result<SyncResponse, Failure> {
        let (function, owner) = match self.callable_member(ctx, &self.request, "requestSync") {
            Some(function) => (function, self.request.clone()),
            None => (
                self.callable_member(ctx, &self.sync, "requestSync").ok_or_else(|| Failure {
                    name: "NotSupportedError".into(),
                    message: "Synchronous HTTP transport is unavailable".into(),
                })?,
                self.sync.clone(),
            ),
        };
        let headers = header_array(ctx, &request.headers);
        let body = match &request.body {
            Some(bytes) => ctx.make_uint8array(bytes).map_err(|e| failure_of(ctx, &e))?,
            None => Value::Undefined,
        };
        let options = Value::Obj(ctx.new_object());
        for (key, value) in [
            ("mode", Value::str(mode_name(request.mode))),
            ("credentials", Value::str(credentials_name(request.credentials))),
            ("redirect", Value::str("follow")),
            ("timeout", Value::Num(request.timeout_ms as f64)),
            ("forcePreflight", Value::Bool(request.force_preflight)),
            (
                "origin",
                request
                    .origin
                    .as_deref()
                    .map_or(Value::Undefined, Value::str),
            ),
        ] {
            let _ = ctx.member_set(&options, key, value);
        }
        let result = ctx
            .invoke(
                function,
                owner,
                &[
                    Value::str(&request.method),
                    Value::str(&request.url),
                    headers,
                    body,
                    options,
                ],
            )
            .map_err(|error| failure_of(ctx, &error))?;
        let raw = parse_raw(ctx, &result)?;
        if raw.status == 0 {
            return Err(Failure::network("Synchronous HTTP request failed"));
        }
        let body = match raw.body {
            ResponseBody::Bytes(bytes) => bytes,
            _ => Vec::new(),
        };
        Ok(SyncResponse {
            status: raw.status,
            status_text: raw.status_text,
            url: raw.url,
            headers: raw.headers,
            body,
        })
    }
}

fn failure_of(ctx: &mut Ctx, error: &Value) -> Failure {
    Failure::from_value(ctx, error)
}

fn header_array(ctx: &mut Ctx, headers: &[(String, String)]) -> Value {
    let pairs = headers
        .iter()
        .map(|(name, value)| {
            ctx.make_array(vec![Value::str(name), Value::str(value)])
        })
        .collect();
    ctx.make_array(pairs)
}

fn text_member(ctx: &mut Ctx, object: &Value, key: &str) -> Option<String> {
    match ctx.member_get(object, key).ok()? {
        Value::Undefined | Value::Null => None,
        value => ctx.coerce_string(&value).ok().map(|text| text.to_string()),
    }
}

fn parse_raw(ctx: &mut Ctx, object: &Value) -> Result<Raw, Failure> {
    if !matches!(object, Value::Obj(_)) {
        return Err(Failure::network("malformed response from the transport"));
    }
    let status = match ctx.member_get(object, "status") {
        Ok(Value::Num(status)) if status.is_finite() && (0.0..=999.0).contains(&status) => {
            status as u16
        }
        _ => return Err(Failure::network("malformed response status")),
    };
    let headers_value = ctx
        .member_get(object, "headers")
        .map_err(|error| failure_of(ctx, &error))?;
    let mut headers = Vec::new();
    if matches!(headers_value, Value::Obj(_)) {
        let pairs = ctx
            .iterable_to_list(&headers_value, usize::MAX)
            .map_err(Failure::from)?;
        for pair in pairs {
            let (Some(name), Some(value)) = (text_member(ctx, &pair, "0"), text_member(ctx, &pair, "1"))
            else {
                continue;
            };
            headers.push((name, value));
        }
    }
    let reader = ctx.member_get(object, "bodyReader").unwrap_or(Value::Undefined);
    let body = if matches!(reader, Value::Obj(_)) {
        ResponseBody::Reader(reader)
    } else {
        match ctx.member_get(object, "body") {
            Ok(value @ Value::Obj(_)) => match ctx.typed_array_bytes(&value) {
                Some(bytes) => ResponseBody::Bytes(bytes),
                None => ResponseBody::None,
            },
            _ => ResponseBody::None,
        }
    };
    let redirected = match ctx.member_get(object, "redirected") {
        Ok(Value::Bool(flag)) => Some(flag),
        _ => None,
    };
    Ok(Raw {
        status,
        status_text: text_member(ctx, object, "statusText").unwrap_or_default(),
        url: text_member(ctx, object, "url").unwrap_or_default(),
        headers,
        kind: text_member(ctx, object, "type"),
        redirected,
        body,
    })
}

/// Read the next chunk of a transport body handle: `Ok(None)` at the end of the body.
pub fn read_chunk(
    ctx: &mut Ctx,
    reader: &Value,
    done: Box<dyn FnOnce(&mut Ctx, Result<Option<Vec<u8>>, Failure>)>,
) {
    let slot: Rc<RefCell<Option<Box<dyn FnOnce(&mut Ctx, Result<Option<Vec<u8>>, Failure>)>>>> =
        Rc::new(RefCell::new(Some(done)));
    let promise = match ctx
        .member_get(reader, "read")
        .and_then(|read| ctx.invoke(read, reader.clone(), &[]))
    {
        Ok(promise) => promise,
        Err(error) => {
            let failure = failure_of(ctx, &error);
            if let Some(done) = slot.borrow_mut().take() {
                done(ctx, Err(failure));
            }
            return;
        }
    };
    let on_chunk = {
        let slot = slot.clone();
        ctx.new_native_fn(
            "",
            1,
            Rc::new(move |ctx: &mut Ctx, _: Value, args: &[Value]| {
                let Some(done) = slot.borrow_mut().take() else {
                    return Ok(Value::Undefined);
                };
                let chunk = args.first().cloned().unwrap_or(Value::Null);
                let result = match chunk {
                    Value::Null | Value::Undefined => Ok(None),
                    chunk => ctx
                        .typed_array_bytes(&chunk)
                        .map(Some)
                        .ok_or_else(|| Failure {
                            name: "TypeError".into(),
                            message: "response body chunk is not bytes".into(),
                        }),
                };
                done(ctx, result);
                Ok(Value::Undefined)
            }),
        )
    };
    let on_error = ctx.new_native_fn(
        "",
        1,
        Rc::new(move |ctx: &mut Ctx, _: Value, args: &[Value]| {
            let Some(done) = slot.borrow_mut().take() else {
                return Ok(Value::Undefined);
            };
            let error = args.first().cloned().unwrap_or(Value::Undefined);
            let failure = failure_of(ctx, &error);
            done(ctx, Err(failure));
            Ok(Value::Undefined)
        }),
    );
    let attached = ctx
        .member_get(&promise, "then")
        .and_then(|then| ctx.invoke(then, promise.clone(), &[on_chunk, on_error]));
    let _ = attached;
}
