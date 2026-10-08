//! The body of a `Request` or `Response`: where its bytes are, whether it was used, and the
//! `ReadableStream` that exposes it.
//!
//! A body is one of: nothing, bytes in memory, a transport body (a response read chunk by chunk
//! over the realm's [`Transport`](super::Transport)), or a stream (supplied by the page, or a
//! teed branch). The `ReadableStream` of a bytes or transport body is created only when script
//! reads `.body`, so `await response.text()` never loads the streams glue. Consuming a body reads
//! straight from the bytes or the transport; once a stream exists, consumption goes through it.

use super::transport::{cancel_reader, read_chunk, Failure};
use crate::blob::uint8_array_from_vec;
use crate::events::dom_exception;
use lumen::embed::{Ctx, Deferred, OpError, OpResult, Value, WeakValue};
use std::{any::Any, cell::RefCell, rc::Rc};

const CLONE_BODY: &str = "lumen.cloneBody";
const IS_DISTURBED: &str = "nodejs.stream.kIsDisturbed";

pub(crate) type Finished = Box<dyn FnOnce(&mut Ctx, Result<Vec<u8>, Value>)>;

/// The error a transport failure becomes in script.
pub(crate) fn failure_value(ctx: &mut Ctx, failure: &Failure) -> Value {
    match failure.name.as_str() {
        "AbortError" | "TimeoutError" => dom_exception(ctx, &failure.message, &failure.name),
        _ => OpError::type_error(failure.message.clone()).to_value(ctx),
    }
}

enum NetState {
    Open,
    Closed,
    Failed(Value),
}

/// A response body still held by the transport.
pub(crate) struct NetBody {
    pub(crate) reader: Value,
    state: RefCell<NetState>,
    controller: RefCell<Option<WeakValue>>,
    waiting: RefCell<Option<ChunkDone>>,
    keep: RefCell<Option<Rc<dyn Any>>>,
}

type ChunkDone = Box<dyn FnOnce(&mut Ctx, Result<Option<Vec<u8>>, Value>)>;

impl NetBody {
    pub(crate) fn new(reader: Value) -> Rc<NetBody> {
        Rc::new(NetBody {
            reader,
            state: RefCell::new(NetState::Open),
            controller: RefCell::new(None),
            waiting: RefCell::new(None),
            keep: RefCell::new(None),
        })
    }

    /// Keep `owner` alive until the body ends, fails or is cancelled.
    pub(crate) fn keep_alive(&self, owner: Rc<dyn Any>) {
        *self.keep.borrow_mut() = Some(owner);
    }

    pub(crate) fn trace(&self, visit: &mut dyn FnMut(&Value)) {
        visit(&self.reader);
        if let Ok(state) = self.state.try_borrow() {
            if let NetState::Failed(reason) = &*state {
                visit(reason);
            }
        }
    }

    fn release(&self) {
        let keep = self.keep.borrow_mut().take();
        drop(keep);
    }

    fn close(&self) {
        let open = matches!(*self.state.borrow(), NetState::Open);
        if open {
            *self.state.borrow_mut() = NetState::Closed;
        }
        self.release();
    }

    fn fail(&self, ctx: &mut Ctx, reason: Value) {
        let open = matches!(*self.state.borrow(), NetState::Open);
        if open {
            *self.state.borrow_mut() = NetState::Failed(reason);
            cancel_reader(ctx, &self.reader);
        }
        self.release();
    }

    /// The consumer gave up: release the transport.
    pub(crate) fn cancel(&self, ctx: &mut Ctx) {
        let open = matches!(*self.state.borrow(), NetState::Open);
        if open {
            *self.state.borrow_mut() = NetState::Closed;
            cancel_reader(ctx, &self.reader);
        }
        self.release();
        let waiting = self.waiting.borrow_mut().take();
        if let Some(done) = waiting {
            done(ctx, Ok(None));
        }
    }

    /// The request was aborted: end the body with `reason`, also in the stream that exposes it.
    pub(crate) fn abort(&self, ctx: &mut Ctx, reason: Value) {
        let open = matches!(*self.state.borrow(), NetState::Open);
        if !open {
            return;
        }
        self.fail(ctx, reason.clone());
        let waiting = self.waiting.borrow_mut().take();
        if let Some(done) = waiting {
            done(ctx, Err(reason.clone()));
        }
        let controller = self.controller.borrow().as_ref().and_then(WeakValue::upgrade);
        if let Some(controller) = controller {
            let _ = call(ctx, &controller, "error", &[reason]);
        }
    }

    pub(crate) fn read(self: &Rc<Self>, ctx: &mut Ctx, done: ChunkDone) {
        let settled = match &*self.state.borrow() {
            NetState::Open => None,
            NetState::Closed => Some(Ok(None)),
            NetState::Failed(reason) => Some(Err(reason.clone())),
        };
        if let Some(result) = settled {
            return done(ctx, result);
        }
        *self.waiting.borrow_mut() = Some(done);
        let body = self.clone();
        read_chunk(
            ctx,
            &self.reader,
            Box::new(move |ctx, result| {
                let waiting = body.waiting.borrow_mut().take();
                let Some(done) = waiting else {
                    return;
                };
                let failed = match &*body.state.borrow() {
                    NetState::Failed(reason) => Some(reason.clone()),
                    _ => None,
                };
                if let Some(reason) = failed {
                    return done(ctx, Err(reason));
                }
                match result {
                    Ok(None) => {
                        body.close();
                        done(ctx, Ok(None));
                    }
                    Ok(chunk) => done(ctx, Ok(chunk)),
                    Err(failure) => {
                        let reason = failure_value(ctx, &failure);
                        body.fail(ctx, reason.clone());
                        done(ctx, Err(reason));
                    }
                }
            }),
        );
    }
}

#[derive(Clone)]
pub(crate) enum Source {
    Null,
    Bytes(Rc<Vec<u8>>),
    Net(Rc<NetBody>),
    /// The bytes are in `Body::stream`.
    Stream,
}

pub(crate) struct Body {
    pub(crate) source: Source,
    pub(crate) stream: Option<Value>,
    pub(crate) used: bool,
}

pub(crate) type BodyCell = Rc<RefCell<Body>>;

impl Body {
    pub(crate) fn null() -> Body {
        Body {
            source: Source::Null,
            stream: None,
            used: false,
        }
    }

    /// Whether the body is in memory and has no bytes.
    pub(crate) fn is_empty_bytes(&self) -> bool {
        matches!(&self.source, Source::Bytes(bytes) if bytes.is_empty())
    }

    pub(crate) fn bytes(bytes: Vec<u8>) -> Body {
        Body {
            source: Source::Bytes(Rc::new(bytes)),
            stream: None,
            used: false,
        }
    }

    pub(crate) fn stream(stream: Value) -> Body {
        Body {
            source: Source::Stream,
            stream: Some(stream),
            used: false,
        }
    }

    pub(crate) fn net(net: Rc<NetBody>) -> Body {
        Body {
            source: Source::Net(net),
            stream: None,
            used: false,
        }
    }

    pub(crate) fn cell(self) -> BodyCell {
        Rc::new(RefCell::new(self))
    }

    pub(crate) fn trace(&self, visit: &mut dyn FnMut(&Value)) {
        if let Some(stream) = &self.stream {
            visit(stream);
        }
        if let Source::Net(net) = &self.source {
            net.trace(visit);
        }
    }
}

pub(crate) fn is_null(cell: &BodyCell) -> bool {
    matches!(cell.borrow().source, Source::Null)
}

fn call(ctx: &mut Ctx, object: &Value, method: &str, args: &[Value]) -> Result<Value, Value> {
    let function = ctx.member_get(object, method)?;
    ctx.invoke(function, object.clone(), args)
}

/// Whether `value` is a `ReadableStream` (of any realm glue): it carries the clone hook.
pub(crate) fn is_readable_stream(ctx: &mut Ctx, value: &Value) -> bool {
    if !matches!(value, Value::Obj(_)) {
        return false;
    }
    let key = ctx.symbol_for(CLONE_BODY);
    ctx.reflect_get(value, &key, value)
        .is_ok_and(|hook| hook.is_callable())
}

pub(crate) fn stream_locked(ctx: &mut Ctx, stream: &Value) -> bool {
    matches!(ctx.member_get(stream, "locked"), Ok(Value::Bool(true)))
}

pub(crate) fn stream_disturbed(ctx: &mut Ctx, stream: &Value) -> bool {
    let key = ctx.symbol_for(IS_DISTURBED);
    matches!(ctx.reflect_get(stream, &key, stream), Ok(Value::Bool(true)))
}

/// `bodyUsed`.
pub(crate) fn body_used(ctx: &mut Ctx, cell: &BodyCell) -> bool {
    let (used, stream) = {
        let body = cell.borrow();
        (body.used, body.stream.clone())
    };
    used || stream.is_some_and(|stream| stream_disturbed(ctx, &stream))
}

/// Whether the body cannot be consumed: used, locked or disturbed.
pub(crate) fn unusable(ctx: &mut Ctx, cell: &BodyCell) -> bool {
    if body_used(ctx, cell) {
        return true;
    }
    let stream = cell.borrow().stream.clone();
    stream.is_some_and(|stream| stream_locked(ctx, &stream))
}

fn chunk_error() -> OpError {
    OpError::type_error("body stream chunk must be a Uint8Array")
}

fn bytes_source(ctx: &mut Ctx, bytes: Rc<Vec<u8>>) -> Value {
    let sent = std::cell::Cell::new(false);
    let pull = ctx.new_native_fn(
        "pull",
        1,
        Rc::new(move |ctx: &mut Ctx, _: Value, args: &[Value]| {
            let controller = args.first().cloned().unwrap_or(Value::Undefined);
            if sent.replace(true) {
                return Ok(Value::Undefined);
            }
            if !bytes.is_empty() {
                let chunk = uint8_array_from_vec(ctx, bytes.to_vec()).map_err(|e| e.to_value(ctx))?;
                call(ctx, &controller, "enqueue", &[chunk])?;
            }
            call(ctx, &controller, "close", &[])
        }),
    );
    let kind = Value::str("bytes");
    ctx.plain_object(&[("type", kind), ("pull", pull)])
}

fn net_source(ctx: &mut Ctx, net: Rc<NetBody>) -> Value {
    let start = {
        let net = net.clone();
        ctx.new_native_fn(
            "start",
            1,
            Rc::new(move |ctx: &mut Ctx, _: Value, args: &[Value]| {
                if let Some(controller) = args.first().and_then(|value| ctx.weak_value(value)) {
                    *net.controller.borrow_mut() = Some(controller);
                }
                Ok(Value::Undefined)
            }),
        )
    };
    let pull = {
        let net = net.clone();
        ctx.new_native_fn(
            "pull",
            1,
            Rc::new(move |ctx: &mut Ctx, _: Value, args: &[Value]| {
                let controller = args.first().cloned().unwrap_or(Value::Undefined);
                let deferred = Deferred::new(ctx);
                let promise = deferred.promise();
                pull_net(ctx, net.clone(), controller, deferred);
                Ok(promise)
            }),
        )
    };
    let cancel = ctx.new_native_fn(
        "cancel",
        1,
        Rc::new(move |ctx: &mut Ctx, _: Value, _: &[Value]| {
            net.cancel(ctx);
            Ok(Value::Undefined)
        }),
    );
    let kind = Value::str("bytes");
    ctx.plain_object(&[("type", kind), ("start", start), ("pull", pull), ("cancel", cancel)])
}

fn pull_net(ctx: &mut Ctx, net: Rc<NetBody>, controller: Value, deferred: Deferred) {
    let again = net.clone();
    again.read(
        ctx,
        Box::new(move |ctx, result| match result {
            Ok(Some(chunk)) if chunk.is_empty() => pull_net(ctx, net, controller, deferred),
            Ok(Some(chunk)) => {
                let outcome = uint8_array_from_vec(ctx, chunk)
                    .map_err(|error| error.to_value(ctx))
                    .and_then(|chunk| call(ctx, &controller, "enqueue", &[chunk]));
                match outcome {
                    Ok(_) => deferred.resolve(ctx, Value::Undefined),
                    Err(error) => deferred.reject(ctx, OpError::thrown(error)),
                }
            }
            Ok(None) => {
                let _ = call(ctx, &controller, "close", &[]);
                deferred.resolve(ctx, Value::Undefined);
            }
            Err(reason) => {
                let _ = call(ctx, &controller, "error", &[reason]);
                deferred.resolve(ctx, Value::Undefined);
            }
        }),
    );
}

/// The `ReadableStream` of the body: `null` for no body, the existing stream, or a new one over
/// the bytes or the transport body.
pub(crate) fn stream_value(ctx: &mut Ctx, cell: &BodyCell) -> OpResult<Value> {
    let (source, existing) = {
        let body = cell.borrow();
        (body.source.clone(), body.stream.clone())
    };
    if let Some(stream) = existing {
        return Ok(stream);
    }
    let underlying = match source {
        Source::Null => return Ok(Value::Null),
        Source::Stream => return Ok(Value::Null),
        Source::Bytes(bytes) => bytes_source(ctx, bytes),
        Source::Net(net) => net_source(ctx, net),
    };
    let global = ctx.global_object();
    let constructor = ctx.member_get(&global, "ReadableStream").map_err(OpError::thrown)?;
    if !constructor.is_callable() {
        return Err(OpError::type_error("ReadableStream is not available"));
    }
    let stream = ctx
        .construct_value(constructor, &[underlying])
        .map_err(OpError::thrown)?;
    cell.borrow_mut().stream = Some(stream.clone());
    Ok(stream)
}

/// Move the body out of `cell` into a new body; the old one counts as used.
pub(crate) fn transfer(cell: &BodyCell) -> Body {
    let mut body = cell.borrow_mut();
    let moved = Body {
        source: body.source.clone(),
        stream: body.stream.clone(),
        used: false,
    };
    body.used = true;
    moved
}

/// A second body with the content of `cell` (which must be usable): bytes are shared, a stream
/// is teed and `cell` continues with one branch.
pub(crate) fn clone_body(ctx: &mut Ctx, cell: &BodyCell) -> OpResult<Body> {
    let source = cell.borrow().source.clone();
    match source {
        Source::Null => return Ok(Body::null()),
        Source::Bytes(bytes) => {
            return Ok(Body {
                source: Source::Bytes(bytes),
                stream: None,
                used: false,
            })
        }
        Source::Net(_) | Source::Stream => {}
    }
    let stream = stream_value(ctx, cell)?;
    let key = ctx.symbol_for(CLONE_BODY);
    let tee = ctx.reflect_get(&stream, &key, &stream).map_err(OpError::thrown)?;
    let branches = ctx.invoke(tee, stream, &[]).map_err(OpError::thrown)?;
    let first = ctx.member_get(&branches, "0").map_err(OpError::thrown)?;
    let second = ctx.member_get(&branches, "1").map_err(OpError::thrown)?;
    {
        let mut body = cell.borrow_mut();
        body.source = Source::Stream;
        body.stream = Some(first);
    }
    Ok(Body::stream(second))
}

/// A body being read to its end; [`Drain::cancel`] stops it.
pub(crate) struct Drain {
    done: RefCell<Option<Finished>>,
    reader: RefCell<Option<Value>>,
    net: RefCell<Option<Rc<NetBody>>>,
    bytes: RefCell<Vec<u8>>,
    limit: usize,
}

impl Drain {
    fn append(self:&Rc<Self>,ctx:&mut Ctx,chunk:&[u8])->bool {
        if chunk.len()>self.limit.saturating_sub(self.bytes.borrow().len()) {
            let reason=OpError::type_error("Response body exceeds the resource byte budget").to_value(ctx);
            self.cancel(ctx,reason);
            false
        } else {
            self.bytes.borrow_mut().extend_from_slice(chunk);
            true
        }
    }
    fn finish(&self, ctx: &mut Ctx, result: Result<Vec<u8>, Value>) {
        let Some(done) = self.done.borrow_mut().take() else {
            return;
        };
        if let Some(reader) = self.reader.borrow_mut().take() {
            let _ = call(ctx, &reader, "releaseLock", &[]);
        }
        done(ctx, result);
    }

    /// End the read with `reason` and release the source.
    pub(crate) fn cancel(self: &Rc<Self>, ctx: &mut Ctx, reason: Value) {
        if self.done.borrow().is_none() {
            return;
        }
        let reader = self.reader.borrow().clone();
        if let Some(reader) = reader {
            if let Ok(promise) = call(ctx, &reader, "cancel", &[reason.clone()]) {
                swallow(ctx, &promise);
            }
        }
        let net = self.net.borrow_mut().take();
        if let Some(net) = net {
            net.abort(ctx, reason.clone());
        }
        self.finish(ctx, Err(reason));
    }
}

fn swallow(ctx: &mut Ctx, promise: &Value) {
    let ignore = ctx.new_native_fn(
        "",
        1,
        Rc::new(|_: &mut Ctx, _: Value, _: &[Value]| Ok(Value::Undefined)),
    );
    let _ = call(ctx, promise, "then", &[Value::Undefined, ignore]);
}

/// Read the whole body, marking it used. `done` may run before this returns when the bytes are
/// in memory.
pub(crate) fn read_all(ctx: &mut Ctx, cell: &BodyCell, done: Finished) -> Rc<Drain> {
    read_all_bounded(ctx,cell,usize::MAX,done)
}

pub(crate) fn read_all_bounded(ctx:&mut Ctx,cell:&BodyCell,limit:usize,done:Finished)->Rc<Drain> {
    let (source, stream) = {
        let mut body = cell.borrow_mut();
        body.used = true;
        (body.source.clone(), body.stream.clone())
    };
    let drain = Rc::new(Drain {
        done: RefCell::new(Some(done)),
        reader: RefCell::new(None),
        net: RefCell::new(None),
        bytes: RefCell::new(Vec::new()),
        limit,
    });
    if let Some(stream) = stream {
        match call(ctx, &stream, "getReader", &[]) {
            Ok(reader) => {
                *drain.reader.borrow_mut() = Some(reader);
                pump_stream(ctx, &drain);
            }
            Err(error) => drain.finish(ctx, Err(error)),
        }
        return drain;
    }
    match source {
        Source::Null | Source::Stream => drain.finish(ctx, Ok(Vec::new())),
        Source::Bytes(bytes) => {
            if drain.append(ctx,&bytes) {
                let bytes=std::mem::take(&mut *drain.bytes.borrow_mut());
                drain.finish(ctx,Ok(bytes));
            }
        },
        Source::Net(net) => {
            *drain.net.borrow_mut() = Some(net);
            pump_net(ctx, &drain);
        }
    }
    drain
}

fn pump_net(ctx: &mut Ctx, drain: &Rc<Drain>) {
    let Some(net) = drain.net.borrow().clone() else {
        return;
    };
    let drain = drain.clone();
    net.read(
        ctx,
        Box::new(move |ctx, result| match result {
            Ok(Some(chunk)) => {
                if drain.append(ctx,&chunk) {pump_net(ctx, &drain);}
            }
            Ok(None) => {
                let bytes = std::mem::take(&mut *drain.bytes.borrow_mut());
                drain.finish(ctx, Ok(bytes));
            }
            Err(reason) => drain.finish(ctx, Err(reason)),
        }),
    );
}

fn pump_stream(ctx: &mut Ctx, drain: &Rc<Drain>) {
    let Some(reader) = drain.reader.borrow().clone() else {
        return;
    };
    let promise = match call(ctx, &reader, "read", &[]) {
        Ok(promise) => promise,
        Err(error) => return drain.finish(ctx, Err(error)),
    };
    let on_chunk = {
        let drain = drain.clone();
        ctx.new_native_fn(
            "",
            1,
            Rc::new(move |ctx: &mut Ctx, _: Value, args: &[Value]| {
                if drain.done.borrow().is_none() {
                    return Ok(Value::Undefined);
                }
                let result = args.first().cloned().unwrap_or(Value::Undefined);
                let finished = ctx.member_get(&result, "done")?;
                if ctx.to_boolean(&finished) {
                    let bytes = std::mem::take(&mut *drain.bytes.borrow_mut());
                    drain.finish(ctx, Ok(bytes));
                    return Ok(Value::Undefined);
                }
                let chunk = ctx.member_get(&result, "value")?;
                match ctx.typed_array_bytes(&chunk) {
                    Some(bytes) => {
                        if drain.append(ctx,&bytes) {pump_stream(ctx, &drain);}
                    }
                    None => {
                        let reason = chunk_error().to_value(ctx);
                        drain.cancel(ctx, reason);
                    }
                }
                Ok(Value::Undefined)
            }),
        )
    };
    let on_error = {
        let drain = drain.clone();
        ctx.new_native_fn(
            "",
            1,
            Rc::new(move |ctx: &mut Ctx, _: Value, args: &[Value]| {
                let reason = args.first().cloned().unwrap_or(Value::Undefined);
                drain.finish(ctx, Err(reason));
                Ok(Value::Undefined)
            }),
        )
    };
    if let Err(error) = call(ctx, &promise, "then", &[on_chunk, on_error]) {
        drain.finish(ctx, Err(error));
    }
}
