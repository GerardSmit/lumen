//! Native structured-clone attachments. Wire bytes carry only message-local indexes;
//! JavaScript cannot look up a process-global shared-memory or MessagePort identity.
//! Each realm stages at most 1024 capabilities per message and 32 nested serialization frames.
//! A getter may post another message without overwriting the outer frame. Delivery replaces the incoming
//! frame; deserialization's `finish` releases it, and realm teardown drops both frames.
use lumen::embed::SharedBufferHandle;
use lumen_host::{Ctx, Extension, Value, ops};

const MAX_ATTACHMENTS: usize = 1024;
const MAX_NESTED_FRAMES: usize = 32;

pub(crate) enum CloneAttachment {
    Shared(SharedBufferHandle),
    Port(crate::ports::PortTransfer),
}

pub(crate) struct CloneMessage {
    pub bytes: Vec<u8>,
    pub attachments: Vec<CloneAttachment>,
}

#[derive(Default)]
struct CloneTransfers {
    outgoing: Vec<Vec<CloneAttachment>>,
    incoming: Vec<Option<CloneAttachment>>,
}

fn state(ctx: &mut Ctx) -> &mut CloneTransfers {
    ctx.host_mut::<CloneTransfers>()
        .expect("clone transfer state installed")
}

fn stage(ctx: &mut Ctx, attachment: CloneAttachment) -> Result<usize, Value> {
    let Some(outgoing) = state(ctx).outgoing.last_mut() else {
        return Err(ctx.make_error("DataCloneError", "No structured-clone frame is active"));
    };
    if outgoing.len() >= MAX_ATTACHMENTS {
        return Err(ctx.make_error("DataCloneError", "Too many structured-clone attachments"));
    }
    let index = outgoing.len();
    outgoing.push(attachment);
    Ok(index)
}

pub(crate) fn stage_port(ctx: &mut Ctx, port: crate::ports::PortTransfer) -> Result<usize, Value> {
    stage(ctx, CloneAttachment::Port(port))
}

pub(crate) fn take_port(ctx: &mut Ctx, index: usize) -> Result<crate::ports::PortTransfer, Value> {
    let incoming = &mut state(ctx).incoming;
    match incoming.get_mut(index) {
        Some(slot @ Some(CloneAttachment::Port(_))) => {
            let Some(CloneAttachment::Port(port)) = slot.take() else {
                unreachable!()
            };
            Ok(port)
        }
        _ => Err(ctx.make_error(
            "DataCloneError",
            "MessagePort attachment is absent or already consumed",
        )),
    }
}

pub(crate) fn take_message(ctx: &mut Ctx, bytes: Vec<u8>) -> CloneMessage {
    CloneMessage {
        bytes,
        attachments: state(ctx).outgoing.pop().unwrap_or_default(),
    }
}

pub(crate) fn install_message(ctx: &mut Ctx, message: CloneMessage) -> Vec<u8> {
    state(ctx).incoming = message.attachments.into_iter().map(Some).collect();
    message.bytes
}

fn begin(ctx: &mut Ctx, _: Value, _: &[Value]) -> Result<Value, Value> {
    if state(ctx).outgoing.len() >= MAX_NESTED_FRAMES {
        return Err(ctx.make_error("DataCloneError", "Structured-clone nesting limit exceeded"));
    }
    state(ctx).outgoing.push(Vec::new());
    Ok(Value::Undefined)
}

fn abort(ctx: &mut Ctx, _: Value, _: &[Value]) -> Result<Value, Value> {
    state(ctx).outgoing.pop();
    Ok(Value::Undefined)
}

fn finish(ctx: &mut Ctx, _: Value, _: &[Value]) -> Result<Value, Value> {
    state(ctx).incoming.clear();
    Ok(Value::Undefined)
}

fn local(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    let bytes = ctx
        .typed_array_bytes(args.first().unwrap_or(&Value::Undefined))
        .ok_or_else(|| ctx.make_error("TypeError", "local clone expects wire bytes"))?;
    let message = take_message(ctx, bytes);
    let bytes = install_message(ctx, message);
    ctx.make_uint8array(&bytes)
}

fn is_transferable_buffer(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    Ok(Value::Bool(ctx.is_transferable_array_buffer(
        args.first().unwrap_or(&Value::Undefined),
    )))
}

fn detach_buffer(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    let buffer = args.first().unwrap_or(&Value::Undefined);
    if !ctx.is_transferable_array_buffer(buffer) {
        return Err(ctx.make_error(
            "DataCloneError",
            "ArrayBuffer is detached or cannot be transferred",
        ));
    }
    ctx.array_buffer_detach(buffer);
    Ok(Value::Undefined)
}

fn clone_shared(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    match ctx.export_shared_array_buffer(args.first().unwrap_or(&Value::Undefined))? {
        Some(handle) => Ok(ctx.import_shared_array_buffer(&handle)),
        None => Ok(Value::Undefined),
    }
}

fn export_shared(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    let handle = ctx
        .export_shared_array_buffer(args.first().unwrap_or(&Value::Undefined))?
        .ok_or_else(|| ctx.make_error("DataCloneError", "Expected a genuine SharedArrayBuffer"))?;
    let index = stage(ctx, CloneAttachment::Shared(handle))?;
    Ok(Value::Num(index as f64))
}

fn import_shared(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    let index = match args.first() {
        Some(Value::Num(n))
            if n.is_finite() && *n >= 0.0 && n.fract() == 0.0 && *n < MAX_ATTACHMENTS as f64 =>
        {
            *n as usize
        }
        _ => return Err(ctx.make_error("DataCloneError", "Invalid shared-memory attachment index")),
    };
    let handle = match state(ctx).incoming.get(index) {
        Some(Some(CloneAttachment::Shared(handle))) => handle.clone(),
        _ => {
            return Err(ctx.make_error(
                "DataCloneError",
                "Shared-memory attachment is not admitted to this message",
            ));
        }
    };
    Ok(ctx.import_shared_array_buffer(&handle))
}

pub(crate) fn extension() -> Extension {
    Extension {
        name: "clone-transfer",
        globals: &[],
        namespaces: &[(
            "__cloneTransfer",
            ops![
                "begin" (0) => begin,
                "abort" (0) => abort,
                "finish" (0) => finish,
                "local" (1) => local,
                "isTransferableBuffer" (1) => is_transferable_buffer,
                "detachBuffer" (1) => detach_buffer,
                "cloneShared" (1) => clone_shared,
                "exportShared" (1) => export_shared,
                "importShared" (1) => import_shared,
            ],
        )],
        state_init: Some(|state| state.put(CloneTransfers::default())),
        js_init: None,
        js_init_snapshot: None,
    }
}
