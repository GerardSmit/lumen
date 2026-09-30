//! Transferable MessagePort endpoints. Handles are local to a realm; only native
//! CloneMessage attachments carry endpoint ownership across worker boundaries.
use crate::clone_transfer::{self, CloneMessage};
use lumen_host::{ops, Ctx, Extension, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, Weak};

struct Endpoint {
    queue: Mutex<VecDeque<CloneMessage>>,
    peer: Mutex<Weak<Endpoint>>,
    closed: std::sync::atomic::AtomicBool,
}
#[derive(Clone)]
pub(crate) struct PortTransfer(Arc<Endpoint>);
#[derive(Default)]
struct Ports {
    next: u64,
    handles: HashMap<u64, PortTransfer>,
}
impl Drop for Ports {
    fn drop(&mut self) {
        for port in self.handles.values() {
            port.0
                .closed
                .store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(peer) = port.0.peer.lock().unwrap().upgrade() {
                peer.closed.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }
}

fn store(ctx: &mut Ctx, port: PortTransfer) -> u64 {
    let state = ctx.host_mut::<Ports>().unwrap();
    state.next += 1;
    let id = state.next;
    state.handles.insert(id, port);
    id
}
fn lookup(ctx: &mut Ctx, args: &[Value]) -> Result<PortTransfer, Value> {
    let id = args.first().and_then(Value::as_num_opt).unwrap_or(-1.0) as u64;
    ctx.host_mut::<Ports>()
        .unwrap()
        .handles
        .get(&id)
        .cloned()
        .ok_or_else(|| ctx.make_error("DataCloneError", "MessagePort is closed or detached"))
}
fn pair(ctx: &mut Ctx, _: Value, _: &[Value]) -> Result<Value, Value> {
    let new = || {
        Arc::new(Endpoint {
            queue: Mutex::new(VecDeque::new()),
            peer: Mutex::new(Weak::new()),
            closed: std::sync::atomic::AtomicBool::new(false),
        })
    };
    let a = new();
    let b = new();
    *a.peer.lock().unwrap() = Arc::downgrade(&b);
    *b.peer.lock().unwrap() = Arc::downgrade(&a);
    let a = store(ctx, PortTransfer(a));
    let b = store(ctx, PortTransfer(b));
    let out = Value::Obj(ctx.new_object());
    let _ = ctx.set_member(&out, "a", Value::Num(a as f64));
    let _ = ctx.set_member(&out, "b", Value::Num(b as f64));
    Ok(out)
}
fn post(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    let port = lookup(ctx, args)?;
    let bytes = ctx
        .typed_array_bytes(args.get(1).unwrap_or(&Value::Undefined))
        .ok_or_else(|| ctx.make_error("TypeError", "port post expects bytes"))?;
    let message = clone_transfer::take_message(ctx, bytes);
    if let Some(peer) = port.0.peer.lock().unwrap().upgrade() {
        if !peer.closed.load(std::sync::atomic::Ordering::SeqCst) {
            peer.queue.lock().unwrap().push_back(message);
        }
    }
    Ok(Value::Undefined)
}
fn poll(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    let port = lookup(ctx, args)?;
    let message = port.0.queue.lock().unwrap().pop_front();
    if let Some(message) = message {
        let bytes = clone_transfer::install_message(ctx, message);
        return ctx.make_uint8array(&bytes);
    }
    Ok(if port.0.closed.load(std::sync::atomic::Ordering::SeqCst) {
        Value::Bool(false)
    } else {
        Value::Undefined
    })
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
    Ok(Value::Num(store(ctx, port) as f64))
}
fn is_closed(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    let port = lookup(ctx, args)?;
    Ok(Value::Bool(
        port.0.closed.load(std::sync::atomic::Ordering::SeqCst),
    ))
}

fn detach(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    let id = args.first().and_then(Value::as_num_opt).unwrap_or(-1.0) as u64;
    ctx.host_mut::<Ports>().unwrap().handles.remove(&id);
    Ok(Value::Undefined)
}
fn close(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    let port = lookup(ctx, args)?;
    port.0
        .closed
        .store(true, std::sync::atomic::Ordering::SeqCst);
    port.0.queue.lock().unwrap().clear();
    let peer = port.0.peer.lock().unwrap().upgrade();
    if let Some(peer) = peer {
        peer.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        let id = ctx
            .host_mut::<Ports>()
            .unwrap()
            .handles
            .iter()
            .find_map(|(id, p)| Arc::ptr_eq(&p.0, &peer).then_some(*id));
        return Ok(id
            .map(|id| Value::Num(id as f64))
            .unwrap_or(Value::Undefined));
    }
    Ok(Value::Undefined)
}

pub(crate) fn extension() -> Extension {
    Extension {
        name: "message-ports",
        globals: &[],
        namespaces: &[(
            "__lumenPorts",
            ops!["pair" (0)=>pair,"post" (2)=>post,"poll" (1)=>poll, "isClosed" (1)=>is_closed,"export" (1)=>export,"import" (1)=>import,"detach" (1)=>detach,"close" (1)=>close],
        )],
        state_init: Some(|s| s.put(Ports::default())),
        js_init: None,
        js_init_snapshot: None,
    }
}
