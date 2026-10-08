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
    Native { kind: &'static str, payload: Option<Box<dyn std::any::Any + Send>> },
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

/// A realm-local native codec. Only the exported Rust capability crosses realms;
/// matching and validation never consult author-controlled JS properties.
#[derive(Clone, Copy)]
pub struct NativeTransferCodec {
    pub kind: &'static str,
    pub matches: fn(&mut Ctx, &Value) -> bool,
    pub validate: fn(&mut Ctx, &Value) -> Result<(), OpError>,
    pub export: fn(&mut Ctx, &Value) -> Result<Box<dyn std::any::Any + Send>, OpError>,
    pub detach: fn(&mut Ctx, &Value, &(dyn std::any::Any + Send)),
    pub import: fn(&mut Ctx, Box<dyn std::any::Any + Send>) -> Result<Value, OpError>,
}
#[derive(Default)]
struct NativeTransferCodecs(Vec<NativeTransferCodec>);
pub fn register_native_codec(ctx: &mut Ctx, codec: NativeTransferCodec) {
    if !ctx.op_state().has::<NativeTransferCodecs>() { ctx.op_state().put(NativeTransferCodecs::default()); }
    let codecs = ctx.host_mut::<NativeTransferCodecs>().expect("native transfer registry");
    if let Some(old) = codecs.0.iter_mut().find(|old| old.kind == codec.kind) { *old = codec; }
    else { codecs.0.push(codec); }
}
pub(crate) fn native_codec(ctx: &mut Ctx, value: &Value) -> Option<NativeTransferCodec> {
    let codecs = ctx.host_mut::<NativeTransferCodecs>().map(|codecs| codecs.0.clone()).unwrap_or_default();
    codecs.into_iter().find(|codec| (codec.matches)(ctx, value))
}
pub(crate) fn reserve_native(ctx: &mut Ctx, kind: &'static str) -> Result<usize, OpError> {
    stage(ctx, CloneAttachment::Native { kind, payload: None })
}
pub(crate) fn fill_native(ctx: &mut Ctx, index: usize, payload: Box<dyn std::any::Any + Send>) {
    if let Some(CloneAttachment::Native { payload: slot, .. }) = state(ctx).outgoing.last_mut().and_then(|frame| frame.get_mut(index)) {
        *slot = Some(payload);
    }
}
pub(crate) fn import_native(ctx: &mut Ctx, index: usize) -> Result<Value, OpError> {
    let attachment = state(ctx).incoming.get_mut(index).and_then(Option::take)
        .ok_or_else(|| OpError::new("DataCloneError", "Native attachment absent or consumed"))?;
    let CloneAttachment::Native { kind, payload: Some(payload) } = attachment else {
        return Err(OpError::new("DataCloneError", "Invalid native attachment"));
    };
    let codec = ctx.host_mut::<NativeTransferCodecs>().and_then(|codecs| codecs.0.iter().find(|codec| codec.kind == kind).copied())
        .ok_or_else(|| OpError::new("DataCloneError", "Native transfer interface unavailable"))?;
    (codec.import)(ctx, payload)
}

/// Trusted byte-only native values also work in persistent storage. Codecs
/// validate their private payload and may not invoke author clone hooks.
#[derive(Clone, Copy)]
pub struct NativeValueCodec {
    pub kind: &'static str,
    pub max_bytes: usize,
    pub matches: fn(&mut Ctx, &Value) -> bool,
    pub serialize: fn(&mut Ctx, &Value) -> Result<Vec<u8>, OpError>,
    pub deserialize: fn(&mut Ctx, &[u8]) -> Result<Value, OpError>,
}
#[derive(Default)]
struct NativeValueCodecs(Vec<NativeValueCodec>);
pub fn register_native_value_codec(ctx: &mut Ctx, codec: NativeValueCodec) {
    assert!(codec.kind.len() <= 256 && codec.max_bytes <= 64 * 1024 * 1024, "native codec budget");
    if !ctx.op_state().has::<NativeValueCodecs>() { ctx.op_state().put(NativeValueCodecs::default()); }
    let codecs = ctx.host_mut::<NativeValueCodecs>().expect("native value registry");
    if let Some(old) = codecs.0.iter_mut().find(|old| old.kind == codec.kind) { *old = codec; }
    else { codecs.0.push(codec); }
}
pub(crate) fn native_value_codec(ctx: &mut Ctx, value: &Value) -> Option<NativeValueCodec> {
    let codecs = ctx.host_mut::<NativeValueCodecs>().map(|codecs| codecs.0.clone()).unwrap_or_default();
    codecs.into_iter().find(|codec| (codec.matches)(ctx, value))
}
pub(crate) fn value_codec_for_kind(ctx: &mut Ctx, kind: &str) -> Option<NativeValueCodec> {
    ctx.host_mut::<NativeValueCodecs>().and_then(|codecs| codecs.0.iter().find(|codec| codec.kind == kind).copied())
}

/// Native values with private sub-values use the normal clone graph, preserving
/// aliases and back-references. Creation precedes reading children; population
/// is a native operation and never invokes author setters.
#[derive(Clone, Copy)]
pub struct NativeGraphValueCodec {
    pub kind: &'static str,
    pub max_bytes: usize,
    pub max_children: usize,
    pub matches: fn(&mut Ctx, &Value) -> bool,
    pub serialize: fn(&mut Ctx, &Value) -> Result<(Vec<u8>, Vec<Value>), OpError>,
    pub create: fn(&mut Ctx, &[u8]) -> Result<Value, OpError>,
    pub populate: fn(&mut Ctx, &Value, Vec<Value>) -> Result<(), OpError>,
}
#[derive(Default)]
struct NativeGraphValueCodecs(Vec<NativeGraphValueCodec>);
pub fn register_native_graph_value_codec(ctx: &mut Ctx, codec: NativeGraphValueCodec) {
    assert!(codec.kind.len() <= 256 && codec.max_bytes <= 64 * 1024 * 1024 && codec.max_children <= 1024, "native graph codec budget");
    if !ctx.op_state().has::<NativeGraphValueCodecs>() { ctx.op_state().put(NativeGraphValueCodecs::default()); }
    let codecs = ctx.host_mut::<NativeGraphValueCodecs>().expect("native graph value registry");
    if let Some(old) = codecs.0.iter_mut().find(|old| old.kind == codec.kind) { *old = codec; }
    else { codecs.0.push(codec); }
}
pub(crate) fn native_graph_value_codec(ctx: &mut Ctx, value: &Value) -> Option<NativeGraphValueCodec> {
    let codecs = ctx.host_mut::<NativeGraphValueCodecs>().map(|codecs| codecs.0.clone()).unwrap_or_default();
    codecs.into_iter().find(|codec| (codec.matches)(ctx, value))
}
pub(crate) fn graph_value_codec_for_kind(ctx: &mut Ctx, kind: &str) -> Option<NativeGraphValueCodec> {
    ctx.host_mut::<NativeGraphValueCodecs>().and_then(|codecs| codecs.0.iter().find(|codec| codec.kind == kind).copied())
}
