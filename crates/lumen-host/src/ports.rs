//! Transferable MessagePort endpoints. Handles are local to a realm; only native
//! CloneMessage attachments carry endpoint ownership across worker boundaries.
//!
//! Delivery is push-driven: a realm that holds an endpoint registers a persistent loop task for
//! it (`listen`), and a sender on any thread wakes that task through the receiving realm's
//! completion channel. The JS side drains one message per wake, so each message is its own
//! macrotask (microtasks run between messages, as with Node's per-message callback scopes).
use crate::clone_transfer::{self, CloneAttachment, CloneMessage};
use lumen::embed::JsFunction;
use lumen_bind::NativeError;
use crate::{CompletionSender, Ctx, Extension, OpError, TaskId, TaskRegistry, Value};
use lumen_os::channel::{Pop, Queue};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

struct Waker {
    sender: CompletionSender,
    task: TaskId,
}

struct Endpoint {
    /// Closed on both endpoints by either side's `close()` (or its realm going away): no message
    /// is accepted any more, and the receiver reports the close once its queue is drained.
    queue: Queue<CloneMessage>,
    peer: Mutex<Weak<Endpoint>>,
    waker: Mutex<Option<Waker>>,
    wake_pending: AtomicBool,
    close_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    close_notified: AtomicBool,
    /// A BroadcastChannel endpoint: posts fan out to every other endpoint of this name.
    group: Option<String>,
}

static GROUPS: Mutex<Option<HashMap<String, Vec<Weak<Endpoint>>>>> = Mutex::new(None);

impl Endpoint {
    fn new(group: Option<String>) -> Arc<Endpoint> {
        Arc::new(Endpoint {
            group,
            queue: Queue::new(),
            peer: Mutex::new(Weak::new()),
            waker: Mutex::new(None),
            wake_pending: AtomicBool::new(false),
            close_hook: Mutex::new(None),
            close_notified: AtomicBool::new(false),
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
            Some(w) => w
                .sender
                .send(w.task, Box::new(PortTransfer(Arc::clone(self)))),
            None => self.wake_pending.store(false, Ordering::SeqCst),
        }
    }

    fn close_both(self: &Arc<Self>) {
        if let Some(name) = &self.group {
            self.queue.close();
            self.queue.clear();
            if let Some(members) = GROUPS
                .lock()
                .unwrap()
                .as_mut()
                .and_then(|g| g.get_mut(name))
            {
                members.retain(|m| {
                    m.strong_count() > 0 && !std::ptr::eq(m.as_ptr(), Arc::as_ptr(self))
                });
            }
            self.wake();
            self.notify_close();
            return;
        }
        self.queue.close();
        self.wake();
        self.notify_close();
        if let Some(peer) = self.peer() {
            peer.queue.close();
            peer.wake();
            peer.notify_close();
        }
    }

    fn notify_close(&self) {
        // A close can race with `on_close` when a port is adopted just as its peer shuts down.
        // Only mark the callback consumed after extracting an installed hook; otherwise the
        // installer observes `closed` and gets a chance to deliver it itself.
        let hook = self.close_hook.lock().unwrap().take();
        if let Some(hook) = hook {
            if !self.close_notified.swap(true, Ordering::SeqCst) {
                hook();
            }
        }
    }
}

#[derive(Clone)]
pub struct PortTransfer(Arc<Endpoint>);

impl PortTransfer {
    pub fn on_close(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.0.close_hook.lock().unwrap() = Some(hook);
        if self.0.queue.is_closed() {
            self.0.notify_close();
        }
    }

    pub fn close(&self) {
        self.0.close_both();
    }
}

struct Handle {
    port: PortTransfer,
    task: Option<TaskId>,
    /// A strong reference to the JavaScript port that keeps it alive while its peer can send.
    pin: Option<Value>,
}

/// Web ports whose wrapper was collected. Their handles are released on the realm's thread.
pub struct DeadPorts {
    ids: std::cell::RefCell<Vec<u64>>,
    wake: Option<(CompletionSender, TaskId)>,
}

impl DeadPorts {
    /// Note that the wrapper behind `id` is gone and ask the realm to release its handle.
    pub fn push(&self, id: u64) {
        self.ids.borrow_mut().push(id);
        if let Some((sender, task)) = &self.wake {
            sender.send(*task, Box::new(()));
        }
    }
}

#[derive(Default)]
struct Ports {
    next: u64,
    handles: HashMap<u64, Handle>,
    dead: Option<Rc<DeadPorts>>,
    /// The ports deserialization created, innermost message last.
    received: Vec<Vec<Value>>,
}

impl Drop for Ports {
    fn drop(&mut self) {
        for handle in self.handles.values() {
            *handle.port.0.waker.lock().unwrap() = None;
            handle.port.0.close_both();
        }
    }
}

/// Whether this engine installed the ports extension.
pub fn available(ctx: &mut Ctx) -> bool {
    ctx.host_mut::<Ports>().is_some()
}

/// A fresh entangled pair, owned by no realm yet.
pub fn new_pair() -> (PortTransfer, PortTransfer) {
    let a = Endpoint::new(None);
    let b = Endpoint::new(None);
    *a.peer.lock().unwrap() = Arc::downgrade(&b);
    *b.peer.lock().unwrap() = Arc::downgrade(&a);
    (PortTransfer(a), PortTransfer(b))
}

/// Give this realm a handle on `port`; the returned id is what the JS port objects carry.
pub fn adopt(ctx: &mut Ctx, port: PortTransfer) -> u64 {
    let state = ctx.host_mut::<Ports>().unwrap();
    state.next += 1;
    let id = state.next;
    state.handles.insert(
        id,
        Handle {
            port,
            task: None,
            pin: None,
        },
    );
    id
}

fn lookup(ctx: &mut Ctx, id: f64) -> Result<PortTransfer, NativeError> {
    let id = id as u64;
    ctx.host_mut::<Ports>()
        .unwrap()
        .handles
        .get(&id)
        .map(|h| h.port.clone())
        .ok_or_else(|| NativeError::named("DataCloneError", "MessagePort is closed or detached"))
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

fn decode_wake(
    _ctx: &mut Ctx,
    payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    if let Ok(port) = payload.downcast::<PortTransfer>() {
        port.0.wake_pending.store(false, Ordering::SeqCst);
    }
    Ok(Vec::new())
}

/// What one `poll` found.
pub enum Polled {
    /// The next message's wire bytes, as a `Uint8Array`; its attachments are installed.
    Message(Value),
    Empty,
    /// The channel is closed and everything sent before the close has been received.
    Closed,
}

/// What one `post` did with its message.
pub enum Posted {
    Queued,
    /// The channel is closed: the message is dropped.
    Dropped,
    /// The message transferred the receiving endpoint itself, so both sides closed.
    Lost,
}

pub fn poll(ctx: &mut Ctx, id: u64) -> Result<Polled, OpError> {
    let port = lookup(ctx, id as f64)?;
    Ok(match port.0.queue.pop() {
        Pop::Message(message) => {
            let bytes = clone_transfer::install_message(ctx, message);
            Polled::Message(ctx.make_uint8array(&bytes)?)
        }
        Pop::Closed => Polled::Closed,
        Pop::Empty => Polled::Empty,
    })
}

pub fn post(ctx: &mut Ctx, id: u64, bytes: &[u8]) -> Result<Posted, OpError> {
    let port = lookup(ctx, id as f64)?;
    let message = clone_transfer::take_message(ctx, bytes.to_vec());
    if let Some(name) = &port.0.group {
        if port.0.queue.is_closed() {
            return Ok(Posted::Dropped);
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
            if Arc::ptr_eq(&member, &port.0) || member.queue.is_closed() {
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
            let _ = member.queue.push(CloneMessage {
                bytes: message.bytes.clone(),
                attachments,
            });
            member.wake();
        }
        return Ok(Posted::Queued);
    }
    let Some(peer) = port.0.peer() else {
        return Ok(Posted::Dropped);
    };
    if port.0.queue.is_closed() || peer.queue.is_closed() {
        return Ok(Posted::Dropped);
    }
    let carries_target = message.attachments.iter().any(|a| match a {
        CloneAttachment::Port(p) => Arc::ptr_eq(&p.0, &peer),
        _ => false,
    });
    if carries_target {
        drop(message);
        port.0.close_both();
        return Ok(Posted::Lost);
    }
    if peer.queue.push(message).is_err() {
        return Ok(Posted::Dropped);
    }
    peer.wake();
    Ok(Posted::Queued)
}

/// Run `callback` as a loop task whenever a message or the close arrives. The task starts
/// unref'd; [`set_ref`] decides whether it keeps the loop alive.
pub fn listen(ctx: &mut Ctx, id: u64, callback: Value) -> Result<(), OpError> {
    let sender = ctx
        .op_state()
        .get::<CompletionSender>()
        .cloned()
        .ok_or_else(|| NativeError::runtime("MessagePort requires an event loop"))?;
    let Some((port, old)) = ctx
        .host_mut::<Ports>()
        .unwrap()
        .handles
        .get(&id)
        .map(|h| (h.port.clone(), h.task))
    else {
        return Ok(());
    };
    let reg = ctx
        .host_mut::<TaskRegistry>()
        .expect("task registry installed");
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
    if port.0.queue.is_closed() || !port.0.queue.is_empty() {
        port.0.wake();
    }
    Ok(())
}

pub fn set_ref(ctx: &mut Ctx, id: u64, keep: bool) {
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
}

/// Schedule another look at this endpoint (more messages remain queued).
pub fn wake(ctx: &mut Ctx, id: u64) {
    if let Ok(port) = lookup(ctx, id as f64) {
        port.0.wake();
    }
}

pub fn export(ctx: &mut Ctx, id: u64) -> Result<usize, OpError> {
    let port = lookup(ctx, id as f64)?;
    clone_transfer::stage_port(ctx, port)
}

pub fn import(ctx: &mut Ctx, index: f64) -> Result<u64, OpError> {
    if !(index.is_finite() && index >= 0.0 && index.fract() == 0.0 && index < 1024.0) {
        return Err(
            NativeError::named("DataCloneError", "Invalid MessagePort attachment index").into(),
        );
    }
    let port = clone_transfer::take_port(ctx, index as usize)?;
    Ok(adopt(ctx, port))
}

pub fn is_closed(ctx: &mut Ctx, id: u64) -> Result<bool, OpError> {
    Ok(lookup(ctx, id as f64)?.0.queue.is_closed())
}

/// Drop this realm's handle on `id`.
pub fn detach(ctx: &mut Ctx, id: u64) {
    release(ctx, id);
}

/// End the channel for both sides. Messages already queued are still received; each side then
/// sees the close.
pub fn close(ctx: &mut Ctx, id: u64) {
    if let Ok(port) = lookup(ctx, id as f64) {
        port.0.close_both();
    }
}

/// A handle on a new BroadcastChannel endpoint joined to the group `key`.
pub fn broadcast(ctx: &mut Ctx, key: String) -> u64 {
    let endpoint = Endpoint::new(Some(key.clone()));
    GROUPS
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .entry(key)
        .or_default()
        .push(Arc::downgrade(&endpoint));
    adopt(ctx, PortTransfer(endpoint))
}

/// Whether a message can still arrive on `id`: its channel is open and, for a pair, the other
/// endpoint exists. A group endpoint is reachable while it is open.
pub fn can_receive(ctx: &mut Ctx, id: u64) -> bool {
    let Ok(port) = lookup(ctx, id as f64) else {
        return false;
    };
    if port.0.queue.is_closed() {
        return false;
    }
    port.0.group.is_some() || port.0.peer().is_some()
}

/// Hold `pin` (the JavaScript port) alive until it is cleared or the handle is released.
pub fn set_pin(ctx: &mut Ctx, id: u64, pin: Option<Value>) {
    if let Some(handle) = ctx.host_mut::<Ports>().unwrap().handles.get_mut(&id) {
        handle.pin = pin;
    }
}

/// The list the wrappers of web ports report themselves to when they are collected. Created with
/// the realm's reaper task on first use.
pub fn dead_ports(ctx: &mut Ctx) -> Rc<DeadPorts> {
    if let Some(dead) = ctx.host_mut::<Ports>().unwrap().dead.clone() {
        return dead;
    }
    let sender = ctx.op_state().get::<CompletionSender>().cloned();
    let wake = sender.and_then(|sender| {
        let callback = ctx.new_native_fn(
            "reapPorts",
            0,
            Rc::new(|ctx: &mut Ctx, _: Value, _: &[Value]| {
                reap(ctx);
                Ok(Value::Undefined)
            }),
        );
        let reg = ctx.host_mut::<TaskRegistry>()?;
        let task = reg.register_stream(callback, decode_reap);
        reg.set_unref(task);
        Some((sender, task))
    });
    let dead = Rc::new(DeadPorts {
        ids: Default::default(),
        wake,
    });
    ctx.host_mut::<Ports>().unwrap().dead = Some(dead.clone());
    dead
}

fn decode_reap(
    _ctx: &mut Ctx,
    _payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    Ok(Vec::new())
}

/// Release the handles of collected ports and tell their peers, which stop being kept alive
/// for a sender that no longer exists.
pub fn reap(ctx: &mut Ctx) {
    let Some(dead) = ctx.host_mut::<Ports>().and_then(|ports| ports.dead.clone()) else {
        return;
    };
    let ids = std::mem::take(&mut *dead.ids.borrow_mut());
    for id in ids {
        if let Some(port) = release(ctx, id) {
            if let Some(peer) = port.0.peer() {
                peer.wake();
            }
        }
    }
}

/// Start collecting the ports a deserialization creates; [`end_received`] returns them.
pub fn begin_received(ctx: &mut Ctx) {
    ctx.host_mut::<Ports>().unwrap().received.push(Vec::new());
}

pub fn note_received(ctx: &mut Ctx, port: &Value) {
    if let Some(frame) = ctx.host_mut::<Ports>().unwrap().received.last_mut() {
        frame.push(port.clone());
    }
}

pub fn end_received(ctx: &mut Ctx) -> Vec<Value> {
    ctx.host_mut::<Ports>()
        .unwrap()
        .received
        .pop()
        .unwrap_or_default()
}

#[lumen_bind::module(name = "__lumenPorts")]
mod bindings {
    use super::*;

    #[op(name = "pair")]
    fn op_pair(ctx: &mut Ctx) -> Value {
        let (a, b) = new_pair();
        let a = adopt(ctx, a);
        let b = adopt(ctx, b);
        let out = Value::Obj(ctx.new_object());
        let _ = ctx.set_member(&out, "a", Value::Num(a as f64));
        let _ = ctx.set_member(&out, "b", Value::Num(b as f64));
        out
    }

    /// `post(id, bytes)` → `true` when queued, `false` when the channel is closed (the message is
    /// dropped, like Node), or `"lost"` when the message transferred the receiving endpoint itself:
    /// the channel cannot survive that, so both sides close.
    #[op(name = "post", coerce)]
    fn op_post(ctx: &mut Ctx, id: f64, bytes: &[u8]) -> Result<Value, OpError> {
        Ok(match post(ctx, id as u64, bytes)? {
            Posted::Queued => Value::Bool(true),
            Posted::Dropped => Value::Bool(false),
            Posted::Lost => Value::from_string("lost".into()),
        })
    }

    /// `poll(id)` → the next message's bytes, `undefined` when none is queued, or `false` once the
    /// channel is closed and everything sent before the close has been received.
    #[op(name = "poll", coerce)]
    fn op_poll(ctx: &mut Ctx, id: f64) -> Result<Value, OpError> {
        Ok(match poll(ctx, id as u64)? {
            Polled::Message(bytes) => bytes,
            Polled::Closed => Value::Bool(false),
            Polled::Empty => Value::Undefined,
        })
    }

    /// `broadcast(name)` → a handle on a new BroadcastChannel endpoint joined to `name`.
    #[op(name = "broadcast", coerce)]
    fn op_broadcast(ctx: &mut Ctx, name: String) -> f64 {
        broadcast(ctx, name) as f64
    }

    /// `peek(id)` → 1 when a message is queued, 0 when none is (channel open), -1 when the channel
    /// is closed and drained.
    #[op(name = "peek", coerce)]
    fn op_peek(ctx: &mut Ctx, id: f64) -> Result<f64, OpError> {
        let port = lookup(ctx, id)?;
        let queued = !port.0.queue.is_empty();
        Ok(if queued {
            1.0
        } else if port.0.queue.is_closed() {
            -1.0
        } else {
            0.0
        })
    }

    /// `listen(id, callback)` — call `callback()` (as a loop task) whenever a message or the close
    /// arrives. The task starts unref'd; `setRef` decides whether it keeps the loop alive.
    #[op(name = "listen", coerce)]
    fn op_listen(ctx: &mut Ctx, id: f64, callback: JsFunction) -> Result<(), OpError> {
        listen(ctx, id as u64, callback.into_value())
    }

    #[op(name = "setRef", coerce)]
    fn op_set_ref(ctx: &mut Ctx, id: f64, keep: Option<bool>) {
        set_ref(ctx, id as u64, keep.unwrap_or(false));
    }

    /// `wake(id)` — schedule another look at this endpoint (more messages remain queued).
    #[op(name = "wake", coerce)]
    fn op_wake(ctx: &mut Ctx, id: f64) {
        wake(ctx, id as u64);
    }

    #[op(name = "export", coerce)]
    fn op_export(ctx: &mut Ctx, id: f64) -> Result<f64, OpError> {
        Ok(export(ctx, id as u64)? as f64)
    }

    #[op(name = "import", coerce)]
    fn op_import(ctx: &mut Ctx, index: f64) -> Result<f64, OpError> {
        Ok(import(ctx, index)? as f64)
    }

    #[op(name = "isClosed", coerce)]
    fn op_is_closed(ctx: &mut Ctx, id: f64) -> Result<bool, OpError> {
        is_closed(ctx, id as u64)
    }

    #[op(name = "detach", coerce)]
    fn op_detach(ctx: &mut Ctx, id: f64) {
        detach(ctx, id as u64);
    }

    /// `close(id)` — end the channel for both sides. Messages already queued are still received;
    /// each side then sees the close.
    #[op(name = "close", coerce)]
    fn op_close(ctx: &mut Ctx, id: f64) {
        close(ctx, id as u64);
    }
}

pub fn extension() -> Extension {
    Extension {
        name: "message-ports",
        modules: &[crate::namespace::<bindings::Module>],
        state_init: Some(|s| s.put(Ports::default())),
        js_init: None,
        js_init_snapshot: None,
        lazy_globals: &[],
    }
}
