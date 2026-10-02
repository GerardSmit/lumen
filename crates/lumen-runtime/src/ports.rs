//! Transferable MessagePort endpoints. Handles are local to a realm; only native
//! CloneMessage attachments carry endpoint ownership across worker boundaries.
//!
//! Delivery is push-driven: a realm that holds an endpoint registers a persistent loop task for
//! it (`listen`), and a sender on any thread wakes that task through the receiving realm's
//! completion channel. The JS side drains one message per wake, so each message is its own
//! macrotask (microtasks run between messages, as with Node's per-message callback scopes).
use lumen_bind::NativeError;
use lumen_host::OpError;
use crate::clone_transfer::{self, CloneAttachment, CloneMessage};
use lumen::embed::JsFunction;
use lumen_host::{CompletionSender, Ctx, Extension, TaskId, TaskRegistry, Value};
use lumen_os::channel::{Pop, Queue};
use std::collections::HashMap;
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
            self.queue.close();
            self.queue.clear();
            if let Some(members) = GROUPS.lock().unwrap().as_mut().and_then(|g| g.get_mut(name)) {
                members.retain(|m| m.strong_count() > 0 && !std::ptr::eq(m.as_ptr(), Arc::as_ptr(self)));
            }
            self.wake();
            return;
        }
        self.queue.close();
        self.wake();
        if let Some(peer) = self.peer() {
            peer.queue.close();
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

fn decode_wake(_ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    if let Ok(port) = payload.downcast::<PortTransfer>() {
        port.0.wake_pending.store(false, Ordering::SeqCst);
    }
    Ok(Vec::new())
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
        let port = lookup(ctx, id)?;
        let message = clone_transfer::take_message(ctx, bytes.to_vec());
        if let Some(name) = &port.0.group {
            if port.0.queue.is_closed() {
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
            return Ok(Value::Bool(true));
        }
        let Some(peer) = port.0.peer() else {
            return Ok(Value::Bool(false));
        };
        if port.0.queue.is_closed() || peer.queue.is_closed() {
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
        if peer.queue.push(message).is_err() {
            return Ok(Value::Bool(false));
        }
        peer.wake();
        Ok(Value::Bool(true))
    }

    /// `poll(id)` → the next message's bytes, `undefined` when none is queued, or `false` once the
    /// channel is closed and everything sent before the close has been received.
    #[op(name = "poll", coerce)]
    fn op_poll(ctx: &mut Ctx, id: f64) -> Result<Value, OpError> {
        let port = lookup(ctx, id)?;
        Ok(match port.0.queue.pop() {
            Pop::Message(message) => {
                let bytes = clone_transfer::install_message(ctx, message);
                return Ok(ctx.make_uint8array(&bytes)?);
            }
            Pop::Closed => Value::Bool(false),
            Pop::Empty => Value::Undefined,
        })
    }

    /// `broadcast(name)` → a handle on a new BroadcastChannel endpoint joined to `name`.
    #[op(name = "broadcast", coerce)]
    fn op_broadcast(ctx: &mut Ctx, name: String) -> f64 {
        let endpoint = Endpoint::new(Some(name.clone()));
        GROUPS
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .entry(name)
            .or_default()
            .push(Arc::downgrade(&endpoint));
        adopt(ctx, PortTransfer(endpoint)) as f64
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
        let id = id as u64;
        let callback = callback.into_value();
        let sender = ctx
            .op_state()
            .get::<CompletionSender>()
            .cloned()
            .ok_or_else(|| NativeError::runtime("MessagePort requires an event loop"))?;
        let Some(port) = ctx
            .host_mut::<Ports>()
            .unwrap()
            .handles
            .get(&id)
            .map(|h| (h.port.clone(), h.task))
        else {
            return Ok(());
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
        if port.0.queue.is_closed() || !port.0.queue.is_empty() {
            port.0.wake();
        }
        Ok(())
    }

    #[op(name = "setRef", coerce)]
    fn op_set_ref(ctx: &mut Ctx, id: f64, keep: Option<bool>) {
        let id = id as u64;
        let keep = keep.unwrap_or(false);
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

    /// `wake(id)` — schedule another look at this endpoint (more messages remain queued).
    #[op(name = "wake", coerce)]
    fn op_wake(ctx: &mut Ctx, id: f64) {
        if let Ok(port) = lookup(ctx, id) {
            port.0.wake();
        }
    }

    #[op(name = "export", coerce)]
    fn op_export(ctx: &mut Ctx, id: f64) -> Result<f64, OpError> {
        let port = lookup(ctx, id)?;
        Ok(clone_transfer::stage_port(ctx, port)? as f64)
    }

    #[op(name = "import", coerce)]
    fn op_import(ctx: &mut Ctx, index: f64) -> Result<f64, OpError> {
        if !(index.is_finite() && index >= 0.0 && index.fract() == 0.0 && index < 1024.0) {
            return Err(NativeError::named("DataCloneError", "Invalid MessagePort attachment index").into());
        }
        let port = clone_transfer::take_port(ctx, index as usize)?;
        Ok(adopt(ctx, port) as f64)
    }

    #[op(name = "isClosed", coerce)]
    fn op_is_closed(ctx: &mut Ctx, id: f64) -> Result<bool, OpError> {
        let port = lookup(ctx, id)?;
        Ok(port.0.queue.is_closed())
    }

    #[op(name = "detach", coerce)]
    fn op_detach(ctx: &mut Ctx, id: f64) {
        release(ctx, id as u64);
    }

    /// `close(id)` — end the channel for both sides. Messages already queued are still received;
    /// each side then sees the close.
    #[op(name = "close", coerce)]
    fn op_close(ctx: &mut Ctx, id: f64) {
        if let Ok(port) = lookup(ctx, id) {
            port.0.close_both();
        }
    }
}

pub(crate) fn extension() -> Extension {
    Extension {
        name: "message-ports",
        modules: &[lumen_host::namespace::<bindings::Module>],
        state_init: Some(|s| s.put(Ports::default())),
        js_init: None,
        js_init_snapshot: None,
    }
}
