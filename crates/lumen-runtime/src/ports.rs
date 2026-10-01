//! Transferable MessagePort endpoints. Handles are local to a realm; only native
//! CloneMessage attachments carry endpoint ownership across worker boundaries.
//!
//! Delivery is push-driven: a realm that holds an endpoint registers a persistent loop task for
//! it (`listen`), and a sender on any thread wakes that task through the receiving realm's
//! completion channel. The JS side drains one message per wake, so each message is its own
//! macrotask (microtasks run between messages, as with Node's per-message callback scopes).
use crate::clone_transfer::{self, CloneAttachment, CloneMessage};
use lumen_host::{ops, CompletionSender, Ctx, Extension, TaskId, TaskRegistry, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

struct Waker {
    sender: CompletionSender,
    task: TaskId,
}

struct Endpoint {
    queue: Mutex<VecDeque<CloneMessage>>,
    peer: Mutex<Weak<Endpoint>>,
    /// Set on both endpoints by either side's `close()` (or its realm going away): no message is
    /// accepted any more, and the receiver reports the close once its queue is drained.
    closed: AtomicBool,
    waker: Mutex<Option<Waker>>,
    wake_pending: AtomicBool,
    /// A BroadcastChannel endpoint: posts fan out to every other endpoint of this name.
    group: Option<String>,
}

static GROUPS: Mutex<Option<HashMap<String, Vec<Weak<Endpoint>>>>> = Mutex::new(None);

impl Endpoint {
    fn new(group: Option<String>) -> Arc<Endpoint> {
        Arc::new(Endpoint {
            group,
            queue: Mutex::new(VecDeque::new()),
            peer: Mutex::new(Weak::new()),
            closed: AtomicBool::new(false),
            waker: Mutex::new(None),
            wake_pending: AtomicBool::new(false),
        })
    }

    fn peer(&self) -> Option<Arc<Endpoint>> {
        self.peer.lock().unwrap().upgrade()
    }

    /// Ask the owning realm to look at this endpoint (coalesced until the wake is consumed).
    fn wake(self: &Arc<Self>) {
        if self.wake_pending.swap(true, Ordering::SeqCst) {
            return;
        }
        let waker = self.waker.lock().unwrap();
        match waker.as_ref() {
            Some(w) => w.sender.send(w.task, Box::new(PortTransfer(Arc::clone(self)))),
            None => self.wake_pending.store(false, Ordering::SeqCst),
        }
    }

    fn close_both(self: &Arc<Self>) {
        if let Some(name) = &self.group {
            self.closed.store(true, Ordering::SeqCst);
            self.queue.lock().unwrap().clear();
            if let Some(members) = GROUPS.lock().unwrap().as_mut().and_then(|g| g.get_mut(name)) {
                members.retain(|m| m.strong_count() > 0 && !std::ptr::eq(m.as_ptr(), Arc::as_ptr(self)));
            }
            self.wake();
            return;
        }
        self.closed.store(true, Ordering::SeqCst);
        self.wake();
        if let Some(peer) = self.peer() {
            peer.closed.store(true, Ordering::SeqCst);
            peer.wake();
        }
    }
}

#[derive(Clone)]
pub(crate) struct PortTransfer(Arc<Endpoint>);

struct Handle {
    port: PortTransfer,
    task: Option<TaskId>,
}

#[derive(Default)]
struct Ports {
    next: u64,
    handles: HashMap<u64, Handle>,
}

impl Drop for Ports {
    fn drop(&mut self) {
        for handle in self.handles.values() {
            *handle.port.0.waker.lock().unwrap() = None;
            handle.port.0.close_both();
        }
    }
}

/// A fresh entangled pair, owned by no realm yet.
pub(crate) fn new_pair() -> (PortTransfer, PortTransfer) {
    let a = Endpoint::new(None);
    let b = Endpoint::new(None);
    *a.peer.lock().unwrap() = Arc::downgrade(&b);
    *b.peer.lock().unwrap() = Arc::downgrade(&a);
    (PortTransfer(a), PortTransfer(b))
}

/// Give this realm a handle on `port`; the returned id is what the JS port objects carry.
pub(crate) fn adopt(ctx: &mut Ctx, port: PortTransfer) -> u64 {
    let state = ctx.host_mut::<Ports>().unwrap();
    state.next += 1;
    let id = state.next;
    state.handles.insert(id, Handle { port, task: None });
    id
}

fn handle_id(args: &[Value]) -> u64 {
    args.first().and_then(Value::as_num_opt).unwrap_or(-1.0) as u64
}

fn lookup(ctx: &mut Ctx, args: &[Value]) -> Result<PortTransfer, Value> {
    let id = handle_id(args);
    ctx.host_mut::<Ports>()
        .unwrap()
        .handles
        .get(&id)
        .map(|h| h.port.clone())
        .ok_or_else(|| ctx.make_error("DataCloneError", "MessagePort is closed or detached"))
}

/// Drop this realm's handle: its wake task stops and the endpoint no longer wakes this realm.
fn release(ctx: &mut Ctx, id: u64) -> Option<PortTransfer> {
    let handle = ctx.host_mut::<Ports>().unwrap().handles.remove(&id)?;
    *handle.port.0.waker.lock().unwrap() = None;
    handle.port.0.wake_pending.store(false, Ordering::SeqCst);
    if let (Some(task), Some(reg)) = (handle.task, ctx.host_mut::<TaskRegistry>()) {
        reg.cancel(task);
    }
    Some(handle.port)
}

fn pair(ctx: &mut Ctx, _: Value, _: &[Value]) -> Result<Value, Value> {
    let (a, b) = new_pair();
    let a = adopt(ctx, a);
    let b = adopt(ctx, b);
    let out = Value::Obj(ctx.new_object());
    let _ = ctx.set_member(&out, "a", Value::Num(a as f64));
    let _ = ctx.set_member(&out, "b", Value::Num(b as f64));
    Ok(out)
}

/// `post(id, bytes)` → `true` when queued, `false` when the channel is closed (the message is
/// dropped, like Node), or `"lost"` when the message transferred the receiving endpoint itself:
/// the channel cannot survive that, so both sides close.
fn post(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    let port = lookup(ctx, args)?;
    let bytes = ctx
        .typed_array_bytes(args.get(1).unwrap_or(&Value::Undefined))
        .ok_or_else(|| ctx.make_error("TypeError", "port post expects bytes"))?;
    let message = clone_transfer::take_message(ctx, bytes);
    if let Some(name) = &port.0.group {
        if port.0.closed.load(Ordering::SeqCst) {
            return Ok(Value::Bool(false));
        }
        let members: Vec<Arc<Endpoint>> = GROUPS
            .lock()
            .unwrap()
            .as_mut()
            .and_then(|g| g.get_mut(name))
            .map(|members| {
                members.retain(|m| m.strong_count() > 0);
                members.iter().filter_map(Weak::upgrade).collect()
            })
            .unwrap_or_default();
        for member in members {
            if Arc::ptr_eq(&member, &port.0) || member.closed.load(Ordering::SeqCst) {
                continue;
            }
            let attachments = message
                .attachments
                .iter()
                .filter_map(|a| match a {
                    CloneAttachment::Shared(h) => Some(CloneAttachment::Shared(h.clone())),
                    CloneAttachment::Port(_) => None,
                })
                .collect();
            member.queue.lock().unwrap().push_back(CloneMessage {
                bytes: message.bytes.clone(),
                attachments,
            });
            member.wake();
        }
        return Ok(Value::Bool(true));
    }
    let Some(peer) = port.0.peer() else {
        return Ok(Value::Bool(false));
    };
    if port.0.closed.load(Ordering::SeqCst) || peer.closed.load(Ordering::SeqCst) {
        return Ok(Value::Bool(false));
    }
    let carries_target = message.attachments.iter().any(|a| match a {
        CloneAttachment::Port(p) => Arc::ptr_eq(&p.0, &peer),
        _ => false,
    });
    if carries_target {
        drop(message);
        port.0.close_both();
        return Ok(Value::from_string("lost".into()));
    }
    peer.queue.lock().unwrap().push_back(message);
    peer.wake();
    Ok(Value::Bool(true))
}

/// `poll(id)` → the next message's bytes, `undefined` when none is queued, or `false` once the
/// channel is closed and everything sent before the close has been received.
fn poll(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    let port = lookup(ctx, args)?;
    let message = port.0.queue.lock().unwrap().pop_front();
    if let Some(message) = message {
        let bytes = clone_transfer::install_message(ctx, message);
        return ctx.make_uint8array(&bytes);
    }
    Ok(if port.0.closed.load(Ordering::SeqCst) {
        Value::Bool(false)
    } else {
        Value::Undefined
    })
}

/// `broadcast(name)` → a handle on a new BroadcastChannel endpoint joined to `name`.
fn broadcast(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    let name = ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string();
    let endpoint = Endpoint::new(Some(name.clone()));
    GROUPS
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .entry(name)
        .or_default()
        .push(Arc::downgrade(&endpoint));
    Ok(Value::Num(adopt(ctx, PortTransfer(endpoint)) as f64))
}

/// `peek(id)` → 1 when a message is queued, 0 when none is (channel open), -1 when the channel
/// is closed and drained.
fn peek(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    let port = lookup(ctx, args)?;
    let queued = !port.0.queue.lock().unwrap().is_empty();
    Ok(Value::Num(if queued {
        1.0
    } else if port.0.closed.load(Ordering::SeqCst) {
        -1.0
    } else {
        0.0
    }))
}

fn decode_wake(_ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    if let Ok(port) = payload.downcast::<PortTransfer>() {
        port.0.wake_pending.store(false, Ordering::SeqCst);
    }
    Ok(Vec::new())
}

/// `listen(id, callback)` — call `callback()` (as a loop task) whenever a message or the close
/// arrives. The task starts unref'd; `setRef` decides whether it keeps the loop alive.
fn listen(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    let id = handle_id(args);
    let callback = match args.get(1) {
        Some(v) if v.is_callable() => v.clone(),
        _ => return Err(ctx.make_error("TypeError", "listen expects a callback")),
    };
    let sender = ctx
        .op_state()
        .get::<CompletionSender>()
        .cloned()
        .ok_or_else(|| ctx.make_error("Error", "MessagePort requires an event loop"))?;
    let Some(port) = ctx
        .host_mut::<Ports>()
        .unwrap()
        .handles
        .get(&id)
        .map(|h| (h.port.clone(), h.task))
    else {
        return Ok(Value::Undefined);
    };
    let (port, old) = port;
    let reg = ctx.host_mut::<TaskRegistry>().expect("task registry installed");
    if let Some(old) = old {
        reg.cancel(old);
    }
    let task = reg.register_stream(callback, decode_wake);
    reg.set_unref(task);
    if let Some(h) = ctx.host_mut::<Ports>().unwrap().handles.get_mut(&id) {
        h.task = Some(task);
    }
    *port.0.waker.lock().unwrap() = Some(Waker { sender, task });
    port.0.wake_pending.store(false, Ordering::SeqCst);
    if port.0.closed.load(Ordering::SeqCst) || !port.0.queue.lock().unwrap().is_empty() {
        port.0.wake();
    }
    Ok(Value::Undefined)
}

fn set_ref(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    let id = handle_id(args);
    let keep = matches!(args.get(1), Some(Value::Bool(true)));
    let task = ctx
        .host_mut::<Ports>()
        .unwrap()
        .handles
        .get(&id)
        .and_then(|h| h.task);
    if let (Some(task), Some(reg)) = (task, ctx.host_mut::<TaskRegistry>()) {
        if keep {
            reg.set_ref(task);
        } else {
            reg.set_unref(task);
        }
    }
    Ok(Value::Undefined)
}

/// `wake(id)` — schedule another look at this endpoint (more messages remain queued).
fn wake(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    if let Ok(port) = lookup(ctx, args) {
        port.0.wake();
    }
    Ok(Value::Undefined)
}

fn export(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    let port = lookup(ctx, args)?;
    let index = clone_transfer::stage_port(ctx, port)?;
    Ok(Value::Num(index as f64))
}

fn import(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    let index = match args.first() {
        Some(Value::Num(n)) if n.is_finite() && *n >= 0.0 && n.fract() == 0.0 && *n < 1024.0 => {
            *n as usize
        }
        _ => return Err(ctx.make_error("DataCloneError", "Invalid MessagePort attachment index")),
    };
    let port = clone_transfer::take_port(ctx, index)?;
    Ok(Value::Num(adopt(ctx, port) as f64))
}

fn is_closed(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    let port = lookup(ctx, args)?;
    Ok(Value::Bool(port.0.closed.load(Ordering::SeqCst)))
}

fn detach(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    release(ctx, handle_id(args));
    Ok(Value::Undefined)
}

/// `close(id)` — end the channel for both sides. Messages already queued are still received;
/// each side then sees the close.
fn close(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    if let Ok(port) = lookup(ctx, args) {
        port.0.close_both();
    }
    Ok(Value::Undefined)
}

pub(crate) fn extension() -> Extension {
    Extension {
        name: "message-ports",
        globals: &[],
        namespaces: &[(
            "__lumenPorts",
            ops![
                "pair" (0) => pair,
                "post" (2) => post,
                "poll" (1) => poll,
                "peek" (1) => peek,
                "broadcast" (1) => broadcast,
                "listen" (2) => listen,
                "setRef" (2) => set_ref,
                "wake" (1) => wake,
                "isClosed" (1) => is_closed,
                "export" (1) => export,
                "import" (1) => import,
                "detach" (1) => detach,
                "close" (1) => close,
            ],
        )],
        state_init: Some(|s| s.put(Ports::default())),
        js_init: None,
        js_init_snapshot: None,
    }
}
