//! `ProgressEvent`, `XMLHttpRequestEventTarget`, `XMLHttpRequestUpload` and `XMLHttpRequest`.
//!
//! An `XMLHttpRequest` owns a [`Core`] shared with the callbacks of its in-flight request. The
//! callbacks check a generation counter after every dispatched event, because a listener may
//! `abort()`, `open()` or `send()` again. An in-flight request keeps its object alive by holding
//! it in `Core::pin`; every terminal path releases the pin.

use super::body::{charset_of, decode_text, essence_of, extract_body, ExtractedBody};
use super::flow::{header, start, RequestControl, RequestSpec, Response, ResponseKind};
use super::transport::{cancel_reader, read_chunk, Failure, ResponseBody, SyncRequest, Transport};
use super::{dom_error, has_global_object};
use crate::blob::new_blob;
use crate::events::{Event, EventTarget};
use crate::timers::{set_timeout, Timer};
use lumen::embed::{Ctx, OpError, OpResult, Value};
use lumen_common::cors::{is_forbidden_request_header, Credentials, Mode, Redirect};
use std::{cell::RefCell, rc::Rc};

const UPLOAD_POLL_MS: f64 = 50.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Empty,
    Text,
    Json,
    ArrayBuffer,
    Blob,
    Document,
}

impl Kind {
    fn parse(text: &str) -> Option<Kind> {
        Some(match text {
            "" => Kind::Empty,
            "text" => Kind::Text,
            "json" => Kind::Json,
            "arraybuffer" => Kind::ArrayBuffer,
            "blob" => Kind::Blob,
            "document" => Kind::Document,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Empty => "",
            Kind::Text => "text",
            Kind::Json => "json",
            Kind::ArrayBuffer => "arraybuffer",
            Kind::Blob => "blob",
            Kind::Document => "document",
        }
    }

    fn is_text(self) -> bool {
        matches!(self, Kind::Empty | Kind::Text)
    }
}

struct State {
    ready: u16,
    sent: bool,
    generation: u64,
    timeout: u32,
    with_credentials: bool,
    is_async: bool,
    response_type: Kind,
    method: String,
    url: String,
    request_headers: Vec<(String, String)>,
    override_mime: Option<String>,
    status: u16,
    status_text: String,
    response_url: String,
    response_headers: Vec<(String, String)>,
    body: Vec<u8>,
    text: Option<String>,
    upload_listeners: bool,
    upload_complete: bool,
    upload_loaded: f64,
    upload_total: f64,
    started: f64,
    control: Option<RequestControl>,
    timeout_timer: Option<Timer>,
    poll_timer: Option<Timer>,
}

impl State {
    fn new() -> State {
        State {
            ready: 0,
            sent: false,
            generation: 0,
            timeout: 0,
            with_credentials: false,
            is_async: true,
            response_type: Kind::Empty,
            method: String::new(),
            url: String::new(),
            request_headers: Vec::new(),
            override_mime: None,
            status: 0,
            status_text: String::new(),
            response_url: String::new(),
            response_headers: Vec::new(),
            body: Vec::new(),
            text: None,
            upload_listeners: false,
            upload_complete: true,
            upload_loaded: 0.0,
            upload_total: 0.0,
            started: 0.0,
            control: None,
            timeout_timer: None,
            poll_timer: None,
        }
    }

    fn response_header(&self, name: &str) -> Option<&str> {
        self.response_headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn charset(&self) -> Option<String> {
        self.override_mime
            .as_deref()
            .and_then(charset_of)
            .or_else(|| self.response_header("content-type").and_then(charset_of))
    }

    fn mime(&self) -> Option<String> {
        self.override_mime
            .clone()
            .or_else(|| self.response_header("content-type").map(str::to_owned))
    }
}

struct Core {
    s: RefCell<State>,
    pin: RefCell<Option<Value>>,
    response_object: RefCell<Value>,
    upload: Value,
}

impl Core {
    fn current(&self, generation: u64) -> bool {
        let state = self.s.borrow();
        state.sent && state.generation == generation
    }

    fn generation(&self) -> u64 {
        self.s.borrow().generation
    }

    fn this(&self) -> Option<Value> {
        self.pin.borrow().clone()
    }

    fn release_pin(&self) {
        if !self.s.borrow().sent {
            let pin = self.pin.borrow_mut().take();
            drop(pin);
        }
    }

    fn reset_response(&self) {
        let object = {
            let mut state = self.s.borrow_mut();
            state.status = 0;
            state.status_text.clear();
            state.response_url.clear();
            state.response_headers.clear();
            state.body = Vec::new();
            state.text = None;
            std::mem::replace(&mut *self.response_object.borrow_mut(), Value::Undefined)
        };
        drop(object);
    }

    fn clear_timers(&self, ctx: &mut Ctx) {
        let (timeout, poll) = {
            let mut state = self.s.borrow_mut();
            (state.timeout_timer.take(), state.poll_timer.take())
        };
        for timer in [timeout, poll].into_iter().flatten() {
            timer.clear(ctx);
        }
    }

    fn abort_transport(&self, ctx: &mut Ctx) {
        let control = self.s.borrow_mut().control.take();
        if let Some(control) = control {
            control.abort(ctx);
        }
    }
}

fn dispatch(ctx: &mut Ctx, target: &Value, event: Value) {
    let _ = EventTarget::dispatch_trusted(ctx, target, &event);
}

fn fire(ctx: &mut Ctx, target: &Value, kind: &str) {
    let event = ctx.new_instance(Event::trusted(kind));
    dispatch(ctx, target, event);
}

fn fire_progress(ctx: &mut Ctx, target: &Value, kind: &str, loaded: f64, total: f64, computable: bool) {
    let event = bindings::ProgressEvent::create(ctx, kind, loaded, total, computable);
    dispatch(ctx, target, event);
}

fn has_listeners(ctx: &mut Ctx, target: &Value) -> bool {
    ctx.with_instance::<EventTarget, _>(target, |target| target.data().has_listeners())
        .unwrap_or(false)
}

fn invalid_state(ctx: &mut Ctx, message: &str) -> OpError {
    dom_error(ctx, "InvalidStateError", message)
}

fn is_token(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

fn combine_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    let mut combined: Vec<(String, String)> = Vec::new();
    for (name, value) in headers {
        let name = name.to_ascii_lowercase();
        match combined.iter_mut().find(|(key, _)| *key == name) {
            Some((_, existing)) => {
                existing.push_str(", ");
                existing.push_str(value);
            }
            None => combined.push((name, value.clone())),
        }
    }
    combined
}

fn content_length(headers: &[(String, String)]) -> (f64, bool) {
    match header(headers, "content-length") {
        Some(value)
            if !value.is_empty()
                && value.bytes().all(|byte| byte.is_ascii_digit())
                && value.parse::<u64>().is_ok_and(|length| length < (1 << 53)) =>
        {
            (value.parse::<u64>().unwrap_or(0) as f64, true)
        }
        _ => (0.0, false),
    }
}

fn without_fragment(url: &str) -> String {
    match lumen_common::url::parse_url(url, None) {
        Some(mut parsed) => {
            parsed.set_hash("");
            parsed.href()
        }
        None => url.to_owned(),
    }
}

fn document_base(ctx: &mut Ctx) -> Option<String> {
    let global = ctx.global_object();
    for (object, key) in [("document", "baseURI"), ("location", "href")] {
        let Ok(holder @ Value::Obj(_)) = ctx.member_get(&global, object) else {
            continue;
        };
        if let Ok(Value::Str(value)) = ctx.member_get(&holder, key) {
            return Some(value.to_string());
        }
    }
    None
}

fn now() -> f64 {
    crate::perf::web_now_ms()
}

fn arm_timeout(ctx: &mut Ctx, core: &Rc<Core>) {
    let previous = core.s.borrow_mut().timeout_timer.take();
    if let Some(timer) = previous {
        timer.clear(ctx);
    }
    let (timeout, started) = {
        let state = core.s.borrow();
        (state.timeout, state.started)
    };
    if timeout == 0 {
        return;
    }
    let delay = (timeout as f64 - (now() - started)).max(0.0);
    let owner = core.clone();
    let timer = set_timeout(ctx, delay, move |ctx| {
        owner.s.borrow_mut().timeout_timer = None;
        fail(ctx, &owner, "timeout");
    });
    if let Ok(timer) = timer {
        core.s.borrow_mut().timeout_timer = Some(timer);
    }
}

/// The request failed, timed out or was aborted: the `readystatechange` and terminal events of
/// the request and, if it was still uploading, of its upload.
fn fail(ctx: &mut Ctx, core: &Rc<Core>, kind: &str) {
    let Some(this) = core.this() else { return };
    if !core.s.borrow().sent {
        return;
    }
    let previous = core.generation();
    core.abort_transport(ctx);
    if !core.s.borrow().sent || core.generation() != previous {
        return;
    }
    let generation = {
        let mut state = core.s.borrow_mut();
        state.generation += 1;
        state.sent = false;
        state.generation
    };
    core.clear_timers(ctx);
    core.reset_response();
    core.s.borrow_mut().ready = 4;
    fire(ctx, &this, "readystatechange");
    if core.generation() != generation {
        return;
    }
    let upload_pending = {
        let mut state = core.s.borrow_mut();
        let pending = !state.upload_complete;
        state.upload_complete = true;
        pending && state.upload_listeners
    };
    if upload_pending {
        fire_progress(ctx, &core.upload, kind, 0.0, 0.0, false);
        if core.generation() != generation {
            return;
        }
        fire_progress(ctx, &core.upload, "loadend", 0.0, 0.0, false);
        if core.generation() != generation {
            return;
        }
    }
    fire_progress(ctx, &this, kind, 0.0, 0.0, false);
    if core.generation() != generation {
        return;
    }
    fire_progress(ctx, &this, "loadend", 0.0, 0.0, false);
    core.release_pin();
}

fn upload_update(
    ctx: &mut Ctx,
    core: &Rc<Core>,
    generation: u64,
    loaded: f64,
    total: f64,
    complete: bool,
) {
    if !core.current(generation) || core.s.borrow().upload_complete || !total.is_finite() {
        return;
    }
    let (changed, listeners) = {
        let mut state = core.s.borrow_mut();
        let changed = loaded > state.upload_loaded;
        state.upload_loaded = state.upload_loaded.max(loaded);
        state.upload_total = total;
        if complete {
            state.upload_complete = true;
        }
        (changed, state.upload_listeners)
    };
    if !listeners {
        return;
    }
    let report = |ctx: &mut Ctx, kind: &str| {
        let (loaded, total) = {
            let state = core.s.borrow();
            (state.upload_loaded, state.upload_total)
        };
        fire_progress(ctx, &core.upload, kind, loaded, total, total != 0.0);
    };
    if changed || complete {
        report(ctx, "progress");
    }
    if complete && core.current(generation) {
        report(ctx, "load");
        if core.current(generation) {
            report(ctx, "loadend");
        }
    }
}

fn poll_upload(ctx: &mut Ctx, core: &Rc<Core>, generation: u64) {
    if !core.current(generation) || core.s.borrow().upload_complete {
        return;
    }
    let control = core.s.borrow().control.clone();
    if let Some(progress) = control.and_then(|control| control.upload_progress(ctx)) {
        upload_update(ctx, core, generation, progress.loaded, progress.total, progress.complete);
    }
    if core.current(generation) && !core.s.borrow().upload_complete {
        let owner = core.clone();
        let timer = set_timeout(ctx, UPLOAD_POLL_MS, move |ctx| {
            owner.s.borrow_mut().poll_timer = None;
            poll_upload(ctx, &owner, generation);
        });
        if let Ok(timer) = timer {
            core.s.borrow_mut().poll_timer = Some(timer);
        }
    }
}

fn on_response(ctx: &mut Ctx, core: &Rc<Core>, generation: u64, result: Result<Response, Failure>) {
    let response = match result {
        Ok(response) => response,
        Err(_) => {
            if core.current(generation) {
                fail(ctx, core, "error");
            }
            return;
        }
    };
    if !core.current(generation) {
        response.body.cancel(ctx);
        return;
    }
    if matches!(response.kind, ResponseKind::Opaque | ResponseKind::OpaqueRedirect)
        || response.status == 0
    {
        response.body.cancel(ctx);
        return fail(ctx, core, "error");
    }
    let poll = core.s.borrow_mut().poll_timer.take();
    if let Some(timer) = poll {
        timer.clear(ctx);
    }
    let control = core.s.borrow().control.clone();
    if let Some(progress) = control.and_then(|control| control.upload_progress(ctx)) {
        upload_update(ctx, core, generation, progress.loaded, progress.total, progress.complete);
        if !core.current(generation) {
            response.body.cancel(ctx);
            return;
        }
    }
    let remaining = {
        let state = core.s.borrow();
        (!state.upload_complete).then_some(state.upload_total)
    };
    if let Some(total) = remaining {
        upload_update(ctx, core, generation, total, total, true);
        if !core.current(generation) {
            response.body.cancel(ctx);
            return;
        }
    }
    let Response {
        status,
        status_text,
        url,
        headers,
        body,
        ..
    } = response;
    let (total, computable) = content_length(&headers);
    {
        let mut state = core.s.borrow_mut();
        state.status = status;
        state.status_text = status_text;
        state.response_url = without_fragment(&url);
        state.response_headers = combine_headers(&headers);
        state.ready = 2;
    }
    let Some(this) = core.this() else {
        return body.cancel(ctx);
    };
    fire(ctx, &this, "readystatechange");
    if !core.current(generation) {
        return body.cancel(ctx);
    }
    match body {
        ResponseBody::None => finish(ctx, core, generation, 0.0, total, computable),
        ResponseBody::Bytes(bytes) => {
            let loaded = bytes.len() as f64;
            if bytes.is_empty() {
                return finish(ctx, core, generation, 0.0, total, computable);
            }
            if append_chunk(ctx, core, generation, &this, bytes, loaded, total, computable) {
                finish(ctx, core, generation, loaded, total, computable);
            }
        }
        ResponseBody::Reader(reader) => read_next(ctx, core, generation, reader, 0.0, total, computable),
    }
}

/// Add `chunk` to the response and report it; `false` when a listener ended the request.
fn append_chunk(
    ctx: &mut Ctx,
    core: &Rc<Core>,
    generation: u64,
    this: &Value,
    chunk: Vec<u8>,
    loaded: f64,
    total: f64,
    computable: bool,
) -> bool {
    {
        let mut state = core.s.borrow_mut();
        state.body.extend_from_slice(&chunk);
        state.text = None;
        state.ready = 3;
    }
    fire(ctx, this, "readystatechange");
    if !core.current(generation) {
        return false;
    }
    fire_progress(ctx, this, "progress", loaded, total, computable);
    core.current(generation)
}

fn read_next(
    ctx: &mut Ctx,
    core: &Rc<Core>,
    generation: u64,
    reader: Value,
    loaded: f64,
    total: f64,
    computable: bool,
) {
    let owner = core.clone();
    let handle = reader.clone();
    read_chunk(
        ctx,
        &reader,
        Box::new(move |ctx, result| {
            if !owner.current(generation) {
                return cancel_reader(ctx, &handle);
            }
            match result {
                Err(_) => {
                    cancel_reader(ctx, &handle);
                    fail(ctx, &owner, "error");
                }
                Ok(None) => finish(ctx, &owner, generation, loaded, total, computable),
                Ok(Some(chunk)) if chunk.is_empty() => {
                    read_next(ctx, &owner, generation, handle, loaded, total, computable)
                }
                Ok(Some(chunk)) => {
                    let loaded = loaded + chunk.len() as f64;
                    let Some(this) = owner.this() else {
                        return cancel_reader(ctx, &handle);
                    };
                    if append_chunk(ctx, &owner, generation, &this, chunk, loaded, total, computable) {
                        read_next(ctx, &owner, generation, handle, loaded, total, computable);
                    } else {
                        cancel_reader(ctx, &handle);
                    }
                }
            }
        }),
    );
}

fn finish(ctx: &mut Ctx, core: &Rc<Core>, generation: u64, loaded: f64, total: f64, computable: bool) {
    let Some(this) = core.this() else { return };
    if loaded == 0.0 {
        fire_progress(ctx, &this, "progress", 0.0, total, computable);
        if !core.current(generation) {
            return;
        }
    }
    core.clear_timers(ctx);
    {
        let mut state = core.s.borrow_mut();
        state.sent = false;
        state.ready = 4;
        state.control = None;
    }
    fire(ctx, &this, "readystatechange");
    if core.generation() != generation {
        return;
    }
    fire_progress(ctx, &this, "load", loaded, total, computable);
    if core.generation() != generation {
        return;
    }
    fire_progress(ctx, &this, "loadend", loaded, total, computable);
    core.release_pin();
}

fn send_async(
    ctx: &mut Ctx,
    core: &Rc<Core>,
    this: &Value,
    body: Option<Vec<u8>>,
    headers: Vec<(String, String)>,
) {
    let listeners = has_listeners(ctx, &core.upload);
    let (generation, method, url, credentials, force_preflight) = {
        let mut state = core.s.borrow_mut();
        state.sent = true;
        state.upload_listeners = listeners;
        state.upload_complete = body.is_none();
        state.upload_loaded = 0.0;
        state.upload_total = body.as_ref().map_or(0.0, |bytes| bytes.len() as f64);
        state.started = now();
        (
            state.generation,
            state.method.clone(),
            state.url.clone(),
            state.with_credentials,
            listeners,
        )
    };
    *core.pin.borrow_mut() = Some(this.clone());
    fire_progress(ctx, this, "loadstart", 0.0, 0.0, false);
    if !core.current(generation) {
        return;
    }
    arm_timeout(ctx, core);
    let uploading = body.is_some() && listeners;
    if uploading {
        let total = core.s.borrow().upload_total;
        fire_progress(ctx, &core.upload, "loadstart", 0.0, total, total != 0.0);
        if !core.current(generation) {
            return;
        }
    }
    let spec = RequestSpec {
        method,
        url,
        headers,
        body,
        mode: Mode::Cors,
        credentials: if credentials {
            Credentials::Include
        } else {
            Credentials::SameOrigin
        },
        redirect: Redirect::Follow,
        observe_upload: listeners,
        force_preflight,
    };
    let owner = core.clone();
    let control = start(ctx, spec, move |ctx, result| {
        on_response(ctx, &owner, generation, result)
    });
    if !core.current(generation) {
        return control.abort(ctx);
    }
    core.s.borrow_mut().control = Some(control);
    if uploading {
        poll_upload(ctx, core, generation);
    }
}

fn send_sync(
    ctx: &mut Ctx,
    core: &Rc<Core>,
    this: &Value,
    body: Option<Vec<u8>>,
    headers: Vec<(String, String)>,
) -> OpResult<()> {
    let transport = Transport::current(ctx).filter(|transport| transport.has_sync(ctx));
    let Some(transport) = transport else {
        return Err(dom_error(
            ctx,
            "NotSupportedError",
            "Synchronous HTTP transport is unavailable",
        ));
    };
    let force_preflight = has_listeners(ctx, &core.upload);
    let (generation, request) = {
        let mut state = core.s.borrow_mut();
        state.sent = true;
        (
            state.generation,
            SyncRequest {
                method: state.method.clone(),
                url: state.url.clone(),
                headers,
                body,
                mode: Mode::Cors,
                credentials: if state.with_credentials {
                    Credentials::Include
                } else {
                    Credentials::SameOrigin
                },
                force_preflight,
                timeout_ms: state.timeout,
                origin: None,
            },
        )
    };
    let request = SyncRequest {
        origin: transport.browser_origin(ctx),
        ..request
    };
    let response = match transport.request_sync(ctx, &request) {
        Ok(response) => response,
        Err(failure) => {
            {
                let mut state = core.s.borrow_mut();
                state.sent = false;
                state.ready = 4;
            }
            core.reset_response();
            let name = match failure.name.as_str() {
                name @ ("TimeoutError" | "AbortError" | "NotSupportedError") => name.to_owned(),
                _ => "NetworkError".to_owned(),
            };
            return Err(dom_error(ctx, &name, &failure.message));
        }
    };
    let length = response.body.len() as f64;
    let (total, computable) = content_length(&response.headers);
    {
        let mut state = core.s.borrow_mut();
        state.status = response.status;
        state.status_text = response.status_text;
        state.response_url = without_fragment(if response.url.is_empty() {
            &state.url
        } else {
            &response.url
        });
        state.response_headers = combine_headers(&response.headers);
        state.body = response.body;
        state.text = None;
        state.sent = false;
        state.upload_complete = true;
        state.ready = 4;
    }
    fire(ctx, this, "readystatechange");
    if core.generation() != generation {
        return Ok(());
    }
    fire_progress(ctx, this, "load", length, total, computable && total > 0.0);
    if core.generation() != generation {
        return Ok(());
    }
    fire_progress(ctx, this, "loadend", length, total, computable && total > 0.0);
    Ok(())
}

fn response_text(core: &Core) -> String {
    let mut state = core.s.borrow_mut();
    if state.ready == 4 {
        if let Some(text) = &state.text {
            return text.clone();
        }
    }
    let text = decode_text(&state.body, state.charset().as_deref());
    if state.ready == 4 {
        state.text = Some(text.clone());
    }
    text
}

fn document_response(ctx: &mut Ctx, core: &Rc<Core>) -> OpResult<Value> {
    let (mime, text_kind) = {
        let state = core.s.borrow();
        if state.ready != 4 || state.status == 0 {
            return Ok(Value::Null);
        }
        let mime = essence_of(&state.mime().unwrap_or_else(|| "application/xml".into()));
        (mime, state.response_type == Kind::Empty)
    };
    if text_kind && mime == "text/html" {
        return Ok(Value::Null);
    }
    let xml = mime == "text/xml"
        || mime == "application/xml"
        || (mime.ends_with("+xml") && mime.matches('/').count() == 1 && !mime.contains(char::is_whitespace));
    if mime != "text/html" && !xml {
        return Ok(Value::Null);
    }
    let cached = core.response_object.borrow().clone();
    if !matches!(cached, Value::Undefined) {
        return Ok(cached);
    }
    let global = ctx.global_object();
    let parser = ctx.member_get(&global, "DOMParser").map_err(OpError::thrown)?;
    if !parser.is_callable() {
        return Err(dom_error(ctx, "NotSupportedError", "No document parser installed"));
    }
    let instance = ctx.construct_value(parser, &[]).map_err(OpError::thrown)?;
    let parse = ctx
        .member_get(&instance, "parseFromString")
        .map_err(OpError::thrown)?;
    let text = response_text(core);
    let kind = if xml { "application/xml" } else { "text/html" };
    let document = ctx
        .invoke(parse, instance, &[Value::from_string(text), Value::str(kind)])
        .map_err(OpError::thrown)?;
    let mut result = document.clone();
    if xml {
        if let Ok(root @ Value::Obj(_)) = ctx.member_get(&document, "documentElement") {
            let local = ctx.member_get(&root, "localName").ok();
            let namespace = ctx.member_get(&root, "namespaceURI").ok();
            let is_error = matches!(&local, Some(Value::Str(name)) if name.as_str() == "parsererror")
                && matches!(&namespace, Some(Value::Str(uri))
                    if uri.as_str() == "http://www.mozilla.org/newlayout/xml/parsererror.xml");
            if is_error {
                result = Value::Null;
            }
        }
    }
    *core.response_object.borrow_mut() = result.clone();
    Ok(result)
}

fn response_value(ctx: &mut Ctx, core: &Rc<Core>) -> OpResult<Value> {
    let kind = core.s.borrow().response_type;
    if kind.is_text() {
        let (ready, _) = {
            let state = core.s.borrow();
            (state.ready, ())
        };
        return Ok(if ready < 3 {
            Value::from_string(String::new())
        } else {
            Value::from_string(response_text(core))
        });
    }
    {
        let state = core.s.borrow();
        if state.ready != 4 || state.status == 0 {
            return Ok(Value::Null);
        }
    }
    if kind == Kind::Document {
        return document_response(ctx, core);
    }
    let cached = core.response_object.borrow().clone();
    if !matches!(cached, Value::Undefined) {
        return Ok(cached);
    }
    let (bytes, mime) = {
        let mut state = core.s.borrow_mut();
        let mime = state.mime().unwrap_or_default();
        (std::mem::take(&mut state.body), mime)
    };
    let value = match kind {
        Kind::ArrayBuffer => {
            let array = ctx.make_uint8array(&bytes).map_err(OpError::thrown)?;
            ctx.member_get(&array, "buffer").map_err(OpError::thrown)?
        }
        Kind::Blob => new_blob(ctx, bytes, &mime),
        _ => {
            let text = decode_text(&bytes, None);
            ctx.json_parse(&text).unwrap_or(Value::Null)
        }
    };
    *core.response_object.borrow_mut() = value.clone();
    Ok(value)
}

fn unsigned_long(ctx: &mut Ctx, value: &Value) -> OpResult<u32> {
    let number = ctx.coerce_number(value).map_err(OpError::thrown)?;
    Ok(if number.is_finite() {
        number.trunc().rem_euclid(4294967296.0) as u32
    } else {
        0
    })
}

fn unsigned_long_long(ctx: &mut Ctx, value: &Value) -> OpResult<f64> {
    if matches!(value, Value::Undefined) {
        return Ok(0.0);
    }
    let number = ctx.coerce_number(value).map_err(OpError::thrown)?;
    Ok(if number.is_finite() {
        number.trunc().rem_euclid(18446744073709551616.0)
    } else {
        0.0
    })
}

#[lumen_bind::module(name = "networkRequests")]
pub mod bindings {
    use super::*;
    use crate::events::{node_handler_get, node_handler_set, TargetData};
    use crate::messaging::{member, Owned};
    use lumen::embed::NativeIdentityOwner;
    use lumen_bind::This;

    #[class(name = "ProgressEvent", extends = Event, hint(js(webidl, invalid_this)))]
    pub struct ProgressEvent {
        base: Event,
        length_computable: bool,
        loaded: f64,
        total: f64,
    }

    #[class(name = "XMLHttpRequestEventTarget", extends = EventTarget, hint(js(webidl, invalid_this)))]
    pub struct XMLHttpRequestEventTarget {
        base: EventTarget,
    }

    #[class(name = "XMLHttpRequestUpload", extends = XMLHttpRequestEventTarget, hint(js(webidl, invalid_this)))]
    pub struct XMLHttpRequestUpload {
        base: XMLHttpRequestEventTarget,
    }

    #[class(name = "XMLHttpRequest", extends = XMLHttpRequestEventTarget, hint(js(webidl, invalid_this)))]
    pub struct XMLHttpRequest {
        base: XMLHttpRequestEventTarget,
        core: Rc<Core>,
    }

    // ---- ProgressEvent -----------------------------------------------------------------------

    #[methods]
    impl ProgressEvent {
        #[constructor(
            coerce,
            hint(js(
                missing_message = "The \"type\" argument must be specified",
                missing_code = "ERR_MISSING_ARGS"
            ))
        )]
        fn constructor(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<Self> {
            let base = Event::new(ctx, kind, options.clone())?;
            let computable = member(ctx, &options, "lengthComputable")?;
            let length_computable = ctx.to_boolean(&computable);
            let loaded = member(ctx, &options, "loaded")?;
            let loaded = unsigned_long_long(ctx, &loaded)?;
            let total = member(ctx, &options, "total")?;
            let total = unsigned_long_long(ctx, &total)?;
            Ok(Self {
                base,
                length_computable,
                loaded,
                total,
            })
        }

        #[getter]
        fn length_computable(&self) -> bool {
            self.length_computable
        }

        #[getter]
        fn loaded(&self) -> f64 {
            self.loaded
        }

        #[getter]
        fn total(&self) -> f64 {
            self.total
        }
    }

    impl ProgressEvent {
        /// A trusted `ProgressEvent` the user agent fires.
        pub fn create(
            ctx: &mut Ctx,
            kind: &str,
            loaded: f64,
            total: f64,
            length_computable: bool,
        ) -> Value {
            ctx.new_instance(Self {
                base: Event::trusted(kind),
                length_computable,
                loaded,
                total,
            })
        }
    }

    // ---- XMLHttpRequestEventTarget -----------------------------------------------------------

    impl XMLHttpRequestEventTarget {
        fn create() -> Self {
            Self {
                base: EventTarget::from_data(TargetData::new(None)),
            }
        }
    }

    #[methods]
    impl XMLHttpRequestEventTarget {
        #[constructor]
        fn constructor() -> OpResult<Self> {
            Err(OpError::type_error("Illegal constructor").with_code("ERR_ILLEGAL_CONSTRUCTOR"))
        }

        #[getter]
        fn onloadstart(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            node_handler_get(ctx, &this.0, "loadstart")
        }

        #[setter]
        fn set_onloadstart(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
            node_handler_set(ctx, &this.0, "loadstart", value)
        }

        #[getter]
        fn onprogress(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            node_handler_get(ctx, &this.0, "progress")
        }

        #[setter]
        fn set_onprogress(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
            node_handler_set(ctx, &this.0, "progress", value)
        }

        #[getter]
        fn onabort(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            node_handler_get(ctx, &this.0, "abort")
        }

        #[setter]
        fn set_onabort(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
            node_handler_set(ctx, &this.0, "abort", value)
        }

        #[getter]
        fn onerror(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            node_handler_get(ctx, &this.0, "error")
        }

        #[setter]
        fn set_onerror(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
            node_handler_set(ctx, &this.0, "error", value)
        }

        #[getter]
        fn onload(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            node_handler_get(ctx, &this.0, "load")
        }

        #[setter]
        fn set_onload(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
            node_handler_set(ctx, &this.0, "load", value)
        }

        #[getter]
        fn ontimeout(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            node_handler_get(ctx, &this.0, "timeout")
        }

        #[setter]
        fn set_ontimeout(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
            node_handler_set(ctx, &this.0, "timeout", value)
        }

        #[getter]
        fn onloadend(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            node_handler_get(ctx, &this.0, "loadend")
        }

        #[setter]
        fn set_onloadend(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
            node_handler_set(ctx, &this.0, "loadend", value)
        }
    }

    // ---- XMLHttpRequestUpload ----------------------------------------------------------------

    #[methods]
    impl XMLHttpRequestUpload {
        #[constructor]
        fn constructor() -> OpResult<Self> {
            Err(OpError::type_error("Illegal constructor").with_code("ERR_ILLEGAL_CONSTRUCTOR"))
        }
    }

    // ---- XMLHttpRequest ----------------------------------------------------------------------

    impl NativeIdentityOwner for XMLHttpRequest {
        const TRACES_NATIVE_VALUES: bool = true;

        fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}

        fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
            self.base.base.data().trace_callbacks(visit);
            visit(&self.core.upload);
            visit(&self.core.response_object.borrow());
        }
    }

    fn sync_in_window(ctx: &mut Ctx, core: &Core) -> bool {
        !core.s.borrow().is_async && has_global_object(ctx, "document")
    }

    #[methods]
    impl XMLHttpRequest {
        #[constant(name = "UNSENT")]
        const UNSENT: u16 = 0;
        #[constant(name = "OPENED")]
        const OPENED: u16 = 1;
        #[constant(name = "HEADERS_RECEIVED")]
        const HEADERS_RECEIVED: u16 = 2;
        #[constant(name = "LOADING")]
        const LOADING: u16 = 3;
        #[constant(name = "DONE")]
        const DONE: u16 = 4;

        #[constructor]
        fn constructor(ctx: &mut Ctx) -> OpResult<Owned<Self>> {
            let upload = ctx.new_instance(XMLHttpRequestUpload {
                base: XMLHttpRequestEventTarget::create(),
            });
            Ok(Owned(Self {
                base: XMLHttpRequestEventTarget::create(),
                core: Rc::new(Core {
                    s: RefCell::new(State::new()),
                    pin: RefCell::new(None),
                    response_object: RefCell::new(Value::Undefined),
                    upload,
                }),
            }))
        }

        #[getter]
        fn onreadystatechange(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            node_handler_get(ctx, &this.0, "readystatechange")
        }

        #[setter]
        fn set_onreadystatechange(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
            node_handler_set(ctx, &this.0, "readystatechange", value)
        }

        #[getter]
        fn ready_state(&self) -> u16 {
            self.core.s.borrow().ready
        }

        #[getter]
        fn status(&self) -> u16 {
            self.core.s.borrow().status
        }

        #[getter]
        fn status_text(&self) -> String {
            self.core.s.borrow().status_text.clone()
        }

        #[getter(name = "responseURL")]
        fn response_url(&self) -> String {
            self.core.s.borrow().response_url.clone()
        }

        #[getter]
        fn upload(&self) -> Value {
            self.core.upload.clone()
        }

        #[getter]
        fn with_credentials(&self) -> bool {
            self.core.s.borrow().with_credentials
        }

        #[setter]
        fn set_with_credentials(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> {
            let (ready, sent) = {
                let state = self.core.s.borrow();
                (state.ready, state.sent)
            };
            if ready > 1 || sent {
                return Err(invalid_state(ctx, "Cannot change credentials after send"));
            }
            let flag = ctx.to_boolean(&value);
            self.core.s.borrow_mut().with_credentials = flag;
            Ok(())
        }

        #[getter]
        fn timeout(&self) -> u32 {
            self.core.s.borrow().timeout
        }

        #[setter]
        fn set_timeout(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> {
            if sync_in_window(ctx, &self.core) {
                return Err(dom_error(
                    ctx,
                    "InvalidAccessError",
                    "Synchronous Window requests cannot have a timeout",
                ));
            }
            let timeout = unsigned_long(ctx, &value)?;
            let sent = {
                let mut state = self.core.s.borrow_mut();
                state.timeout = timeout;
                state.sent
            };
            if sent {
                arm_timeout(ctx, &self.core);
            }
            Ok(())
        }

        #[getter]
        fn response_type(&self) -> String {
            self.core.s.borrow().response_type.name().to_owned()
        }

        #[setter]
        fn set_response_type(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> {
            if sync_in_window(ctx, &self.core) {
                return Err(dom_error(
                    ctx,
                    "InvalidAccessError",
                    "Synchronous Window requests cannot set responseType",
                ));
            }
            if matches!(self.core.s.borrow().ready, 3 | 4) {
                return Err(invalid_state(ctx, "Response is already loading"));
            }
            let text = ctx.coerce_string(&value).map_err(OpError::thrown)?;
            if let Some(kind) = Kind::parse(&text) {
                self.core.s.borrow_mut().response_type = kind;
            }
            Ok(())
        }

        #[getter]
        fn response_text(&self, ctx: &mut Ctx) -> OpResult<String> {
            let (kind, ready) = {
                let state = self.core.s.borrow();
                (state.response_type, state.ready)
            };
            if !kind.is_text() {
                return Err(invalid_state(ctx, "Response type is not text"));
            }
            Ok(if ready < 3 {
                String::new()
            } else {
                response_text(&self.core)
            })
        }

        #[getter(name = "responseXML")]
        fn response_xml(&self, ctx: &mut Ctx) -> OpResult<Value> {
            let kind = self.core.s.borrow().response_type;
            if !matches!(kind, Kind::Empty | Kind::Document) {
                return Err(invalid_state(ctx, "Response type is not a document"));
            }
            document_response(ctx, &self.core)
        }

        #[getter]
        fn response(&self, ctx: &mut Ctx) -> OpResult<Value> {
            response_value(ctx, &self.core)
        }

        #[method(coerce)]
        fn open(
            &self,
            ctx: &mut Ctx,
            this: This<Value>,
            method: &str,
            url: &str,
            #[default(Value::Undefined)] is_async: Value,
            #[default(Value::Undefined)] username: Value,
            #[default(Value::Undefined)] password: Value,
        ) -> OpResult<()> {
            if !is_token(method) {
                return Err(dom_error(ctx, "SyntaxError", "Invalid method"));
            }
            if ["CONNECT", "TRACE", "TRACK"]
                .iter()
                .any(|blocked| method.eq_ignore_ascii_case(blocked))
            {
                return Err(dom_error(ctx, "SecurityError", "Forbidden method"));
            }
            let method = if ["DELETE", "GET", "HEAD", "OPTIONS", "POST", "PUT"]
                .iter()
                .any(|known| method.eq_ignore_ascii_case(known))
            {
                method.to_ascii_uppercase()
            } else {
                method.to_owned()
            };
            let is_async = match is_async {
                Value::Undefined => true,
                flag => ctx.to_boolean(&flag),
            };
            let incompatible = {
                let state = self.core.s.borrow();
                state.timeout != 0 || state.response_type != Kind::Empty
            };
            if !is_async && incompatible && has_global_object(ctx, "document") {
                return Err(dom_error(
                    ctx,
                    "InvalidAccessError",
                    "Synchronous Window request has incompatible timeout or responseType",
                ));
            }
            let credential = |ctx: &mut Ctx, value: Value| match value {
                Value::Undefined | Value::Null => Ok(None),
                value => ctx
                    .coerce_string(&value)
                    .map(|text| Some(text.to_string()))
                    .map_err(OpError::thrown),
            };
            let username = credential(ctx, username)?;
            let password = credential(ctx, password)?;
            let base = document_base(ctx);
            let mut target = lumen_common::url::parse(url, base.as_deref())
                .map_err(|_| dom_error(ctx, "SyntaxError", "Invalid URL"))?;
            if let Some(username) = &username {
                target.set_username(username);
            }
            if let Some(password) = &password {
                target.set_password(password);
            }
            {
                let mut state = self.core.s.borrow_mut();
                state.generation += 1;
                state.sent = false;
            }
            self.core.abort_transport(ctx);
            self.core.clear_timers(ctx);
            let notify = {
                let mut state = self.core.s.borrow_mut();
                state.is_async = is_async;
                state.method = method;
                state.url = target.href();
                state.request_headers.clear();
                state.override_mime = None;
                state.ready != 1
            };
            self.core.reset_response();
            self.core.release_pin();
            if notify {
                self.core.s.borrow_mut().ready = 1;
                fire(ctx, &this.0, "readystatechange");
            }
            Ok(())
        }

        #[method(name = "setRequestHeader", coerce)]
        fn set_request_header(&self, ctx: &mut Ctx, name: &str, value: &str) -> OpResult<()> {
            {
                let state = self.core.s.borrow();
                if state.ready != 1 || state.sent {
                    drop(state);
                    return Err(invalid_state(ctx, "Request is not open"));
                }
            }
            let value = value.trim_matches(|c| matches!(c, '\t' | '\n' | '\r' | ' '));
            if !is_token(name) {
                return Err(dom_error(ctx, "SyntaxError", "Invalid header name"));
            }
            if value.contains(['\r', '\n', '\0']) {
                return Err(dom_error(ctx, "SyntaxError", "Invalid header value"));
            }
            if is_forbidden_request_header(name) {
                return Ok(());
            }
            self.core
                .s
                .borrow_mut()
                .request_headers
                .push((name.to_ascii_lowercase(), value.to_owned()));
            Ok(())
        }

        #[method(name = "getResponseHeader", coerce)]
        fn get_response_header(&self, name: &str) -> Value {
            let state = self.core.s.borrow();
            let name = name.to_ascii_lowercase();
            if state.ready < 2 || name == "set-cookie" || name == "set-cookie2" {
                return Value::Null;
            }
            state
                .response_header(&name)
                .map_or(Value::Null, |value| Value::from_string(value.to_owned()))
        }

        #[method(name = "getAllResponseHeaders")]
        fn get_all_response_headers(&self) -> String {
            let state = self.core.s.borrow();
            if state.ready < 2 {
                return String::new();
            }
            let mut headers: Vec<_> = state
                .response_headers
                .iter()
                .filter(|(name, _)| name != "set-cookie" && name != "set-cookie2")
                .collect();
            headers.sort();
            headers
                .into_iter()
                .map(|(name, value)| format!("{name}: {value}\r\n"))
                .collect()
        }

        #[method(name = "overrideMimeType", coerce)]
        fn override_mime_type(&self, ctx: &mut Ctx, mime: &str) -> OpResult<()> {
            if matches!(self.core.s.borrow().ready, 3 | 4) {
                return Err(invalid_state(ctx, "Response is already loading"));
            }
            self.core.s.borrow_mut().override_mime = Some(mime.to_owned());
            Ok(())
        }

        fn send(
            &self,
            ctx: &mut Ctx,
            this: This<Value>,
            #[default(Value::Null)] body: Value,
        ) -> OpResult<()> {
            let (is_async, method) = {
                let state = self.core.s.borrow();
                if state.ready != 1 || state.sent {
                    drop(state);
                    return Err(invalid_state(
                        ctx,
                        "Request is not open or has already been sent",
                    ));
                }
                (state.is_async, state.method.clone())
            };
            let extracted: Option<ExtractedBody> =
                if matches!(body, Value::Null | Value::Undefined)
                    || method == "GET"
                    || method == "HEAD"
                {
                    None
                } else {
                    Some(extract_body(ctx, &body)?)
                };
            let mut headers = self.core.s.borrow().request_headers.clone();
            if let Some(content_type) = extracted.as_ref().and_then(|body| body.content_type.clone())
            {
                if header(&headers, "content-type").is_none() {
                    headers.push(("content-type".into(), content_type));
                }
            }
            let headers = combine_headers(&headers);
            let bytes = extracted.map(|body| body.bytes);
            if is_async {
                send_async(ctx, &self.core, &this.0, bytes, headers);
                Ok(())
            } else {
                send_sync(ctx, &self.core, &this.0, bytes, headers)
            }
        }

        fn abort(&self, ctx: &mut Ctx) {
            if self.core.s.borrow().sent {
                let generation = self.core.generation();
                fail(ctx, &self.core, "abort");
                if self.core.generation() != generation + 1 {
                    return;
                }
            }
            if self.core.s.borrow().ready == 4 {
                self.core.s.borrow_mut().ready = 0;
                self.core.reset_response();
            }
        }
    }
}
