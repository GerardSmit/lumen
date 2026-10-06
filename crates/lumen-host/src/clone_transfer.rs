//! Native structured-clone attachments (`__cloneTransfer`), used by [`crate::structured_clone`]
//! directly and, for tests, through the namespace. Wire bytes carry only message-local indexes;
//! JavaScript cannot look up a process-global shared-memory or MessagePort identity.
//! Each realm stages at most 1024 capabilities per message and 32 nested serialization frames.
//! A getter may post another message without overwriting the outer frame. Delivery replaces the incoming
//! frame; deserialization's `finish` releases it, and realm teardown drops both frames.
use lumen::embed::SharedBufferHandle;
use lumen_bind::NativeError;
use crate::{Ctx, Extension, OpError, Value};

const MAX_ATTACHMENTS: usize = 1024;
const MAX_NESTED_FRAMES: usize = 32;

pub enum CloneAttachment {
    Shared(SharedBufferHandle),
    Port(crate::ports::PortTransfer),
}

pub struct CloneMessage {
    pub bytes: Vec<u8>,
    pub attachments: Vec<CloneAttachment>,
}

#[derive(Default)]
struct CloneTransfers {
    outgoing: Vec<Vec<CloneAttachment>>,
    incoming: Vec<Option<CloneAttachment>>,
}

/// The realm's frames; a realm that did not install the extension (the kernel's) gets empty ones
/// on first use, so a local `structuredClone` can still share memory.
fn state(ctx: &mut Ctx) -> &mut CloneTransfers {
    if !ctx.op_state().has::<CloneTransfers>() {
        ctx.op_state().put(CloneTransfers::default());
    }
    ctx.host_mut::<CloneTransfers>()
        .expect("clone transfer state installed")
}

/// Open the frame that collects the attachments of the message being serialized.
pub(crate) fn begin_frame(ctx: &mut Ctx) -> Result<(), OpError> {
    if state(ctx).outgoing.len() >= MAX_NESTED_FRAMES {
        return Err(NativeError::named(
            "DataCloneError",
            "Structured-clone nesting limit exceeded",
        )
        .into());
    }
    state(ctx).outgoing.push(Vec::new());
    Ok(())
}

/// Drop the frame of a serialization that failed.
pub(crate) fn abort_frame(ctx: &mut Ctx) {
    state(ctx).outgoing.pop();
}

/// Release the attachments of the message that was just deserialized.
pub(crate) fn finish_frame(ctx: &mut Ctx) {
    state(ctx).incoming.clear();
}

/// Stage a `SharedArrayBuffer` in the open frame; the wire carries the returned index.
pub(crate) fn stage_shared(ctx: &mut Ctx, buffer: &Value) -> Result<usize, OpError> {
    let handle = ctx.export_shared_array_buffer(buffer)?.ok_or_else(|| {
        NativeError::named("DataCloneError", "Expected a genuine SharedArrayBuffer")
    })?;
    stage(ctx, CloneAttachment::Shared(handle))
}

/// The `SharedArrayBuffer` of the incoming attachment `index`.
pub(crate) fn import_shared(ctx: &mut Ctx, index: f64) -> Result<Value, OpError> {
    if !(index.is_finite()
        && index >= 0.0
        && index.fract() == 0.0
        && index < MAX_ATTACHMENTS as f64)
    {
        return Err(
            NativeError::named("DataCloneError", "Invalid shared-memory attachment index").into(),
        );
    }
    let handle = match state(ctx).incoming.get(index as usize) {
        Some(Some(CloneAttachment::Shared(handle))) => handle.clone(),
        _ => {
            return Err(NativeError::named(
                "DataCloneError",
                "Shared-memory attachment is not admitted to this message",
            )
            .into());
        }
    };
    Ok(ctx.import_shared_array_buffer(&handle))
}

fn stage(ctx: &mut Ctx, attachment: CloneAttachment) -> Result<usize, OpError> {
    let Some(outgoing) = state(ctx).outgoing.last_mut() else {
        return Err(
            NativeError::named("DataCloneError", "No structured-clone frame is active").into(),
        );
    };
    if outgoing.len() >= MAX_ATTACHMENTS {
        return Err(
            NativeError::named("DataCloneError", "Too many structured-clone attachments").into(),
        );
    }
    let index = outgoing.len();
    outgoing.push(attachment);
    Ok(index)
}

pub fn stage_port(
    ctx: &mut Ctx,
    port: crate::ports::PortTransfer,
) -> Result<usize, OpError> {
    stage(ctx, CloneAttachment::Port(port))
}

pub fn take_port(
    ctx: &mut Ctx,
    index: usize,
) -> Result<crate::ports::PortTransfer, OpError> {
    let incoming = &mut state(ctx).incoming;
    match incoming.get_mut(index) {
        Some(slot @ Some(CloneAttachment::Port(_))) => {
            let Some(CloneAttachment::Port(port)) = slot.take() else {
                unreachable!()
            };
            Ok(port)
        }
        _ => Err(NativeError::named(
            "DataCloneError",
            "MessagePort attachment is absent or already consumed",
        )
        .into()),
    }
}

pub fn take_message(ctx: &mut Ctx, bytes: Vec<u8>) -> CloneMessage {
    CloneMessage {
        bytes,
        attachments: state(ctx).outgoing.pop().unwrap_or_default(),
    }
}

pub fn install_message(ctx: &mut Ctx, message: CloneMessage) -> Vec<u8> {
    state(ctx).incoming = message.attachments.into_iter().map(Some).collect();
    message.bytes
}

/// What the capability tests drive from script: the attachment checks of `postMessage`.
#[lumen_bind::module(name = "__cloneTransfer")]
mod bindings {
    use super::*;

    #[op(name = "isTransferableBuffer")]
    fn op_is_transferable_buffer(ctx: &mut Ctx, buffer: &Value) -> bool {
        ctx.is_transferable_array_buffer(buffer)
    }

    #[op(name = "detachBuffer")]
    fn op_detach_buffer(ctx: &mut Ctx, buffer: &Value) -> Result<(), OpError> {
        if !ctx.is_transferable_array_buffer(buffer) {
            return Err(NativeError::named(
                "DataCloneError",
                "ArrayBuffer is detached or cannot be transferred",
            )
            .into());
        }
        ctx.array_buffer_detach(buffer);
        Ok(())
    }

    #[op(name = "importShared", coerce)]
    fn op_import_shared(ctx: &mut Ctx, index: f64) -> Result<Value, OpError> {
        import_shared(ctx, index)
    }
}

pub fn extension() -> Extension {
    Extension {
        name: "clone-transfer",
        modules: &[crate::namespace::<bindings::Module>],
        state_init: Some(|state| state.put(CloneTransfers::default())),
        js_init: None,
        js_init_snapshot: None,
        lazy_globals: &[],
    }
}
