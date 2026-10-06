//! Structured clone (`structuredClone`, and the wire behind `postMessage`, `MessagePort`,
//! workers and `BroadcastChannel`) as typed natives; design in `docs/native-clone.md`.
//!
//! [`bindings::Module`] publishes the `structuredClone` global (install it with
//! [`crate::lazy_globals`]). [`internals::Module`] publishes `__serializeForClone` and
//! `__deserializeClone`, the byte-level halves the worker and `worker_threads` glue call from
//! script; Rust embedders use [`serialize`] and [`deserialize`] directly.
//!
//! The serializer classifies objects by internal slots ([`lumen::embed::CloneBrand`]) and walks
//! them in the order of HTML's StructuredSerializeInternal: it takes the own enumerable string
//! keys first, then checks `HasOwnProperty` and runs `[[Get]]` for each, so a getter that
//! deletes a later property or mutates the graph behaves as in browsers. Attachments that cannot
//! be bytes (`SharedArrayBuffer` memory, `MessagePort` endpoints) go into the frame of
//! [`crate::clone_transfer`]; the wire carries only their message-local indexes.

mod read;
mod wire;
mod write;

use crate::clone_transfer;
use crate::events;
use crate::webidl::invalid_arg_type;
use lumen::embed::{Ctx, OpError, OpResult, Value};
use read::Reader;
use wire::clone_error;
use write::{same_object, Writer, UNTRANSFERABLE};

/// The object the serializer asks about ports: `isPort`, `isUntransferable`, `isUncloneable`,
/// `validate`, `export`, `detach` and `import`. The web bridge is built natively
/// ([`crate::messaging`]); `worker_threads` replaces it with its own. An absent bridge means the
/// realm has no ports.
pub(crate) struct Bridge {
    pub(crate) value: Value,
}

impl Bridge {
    /// The bridge of the realm's global, or `bridge` when it is an object.
    pub(crate) fn resolve(ctx: &mut Ctx, bridge: &Value) -> Self {
        if matches!(bridge, Value::Obj(_)) {
            return Self {
                value: bridge.clone(),
            };
        }
        let global = ctx.global_object();
        Self {
            value: ctx
                .member_get(&global, "__lumenPortClone")
                .unwrap_or(Value::Undefined),
        }
    }

    pub(crate) fn call(&self, ctx: &mut Ctx, name: &str, args: &[Value]) -> OpResult<Value> {
        if !matches!(self.value, Value::Obj(_)) {
            return Ok(Value::Undefined);
        }
        let function = ctx.member_get(&self.value, name).map_err(OpError::thrown)?;
        if !function.is_callable() {
            return Ok(Value::Undefined);
        }
        ctx.invoke(function, self.value.clone(), args)
            .map_err(OpError::thrown)
    }

    pub(crate) fn flag(&self, ctx: &mut Ctx, name: &str, value: &Value) -> OpResult<bool> {
        let answer = self.call(ctx, name, std::slice::from_ref(value))?;
        Ok(ctx.to_boolean(&answer))
    }
}

/// The items of an array, read by index.
pub(crate) fn array_items(ctx: &mut Ctx, array: &Value) -> OpResult<Vec<Value>> {
    let length = match ctx.member_get(array, "length").map_err(OpError::thrown)? {
        Value::Num(length) if length >= 0.0 => length as usize,
        _ => 0,
    };
    (0..length)
        .map(|index| {
            ctx.member_get(array, &index.to_string())
                .map_err(OpError::thrown)
        })
        .collect()
}

/// The transfer list of `postMessage(message, transfer | { transfer })`.
fn transfer_argument(ctx: &mut Ctx, transfer: &Value) -> OpResult<Vec<Value>> {
    let list = match transfer {
        Value::Undefined | Value::Null => return Ok(Vec::new()),
        Value::Obj(_) if ctx.is_array_value(transfer).map_err(OpError::thrown)? => transfer.clone(),
        Value::Obj(_) => ctx.member_get(transfer, "transfer").map_err(OpError::thrown)?,
        _ => Value::Bool(false),
    };
    match list {
        Value::Undefined | Value::Null => Ok(Vec::new()),
        Value::Obj(_) if ctx.is_array_value(&list).map_err(OpError::thrown)? => {
            array_items(ctx, &list)
        }
        _ => Err(OpError::type_error("transferList must be an array")),
    }
}

/// The transfer list with Node's pooled buffers removed (they stay with the sender and are
/// cloned), checked for duplicates and for items that cannot be transferred.
fn settle_transfer_list(
    ctx: &mut Ctx,
    transfer: &[Value],
    transport: bool,
    bridge: &Bridge,
) -> OpResult<Vec<Value>> {
    let untransferable = ctx.symbol_for(UNTRANSFERABLE);
    let mut list: Vec<Value> = Vec::with_capacity(transfer.len());
    for item in transfer {
        let pooled = matches!(item, Value::Obj(_))
            && matches!(
                ctx.reflect_get(item, &untransferable, item)
                    .map_err(OpError::thrown)?,
                Value::Bool(true)
            )
            && !bridge.flag(ctx, "isPort", item)?;
        if !pooled {
            list.push(item.clone());
        }
    }
    for (index, item) in list.iter().enumerate() {
        let is_port = bridge.flag(ctx, "isPort", item)?;
        if list[..index].iter().any(|earlier| same_object(earlier, item)) {
            return Err(clone_error(format!(
                "Transfer list contains duplicate {}",
                if is_port { "MessagePort" } else { "ArrayBuffer" }
            )));
        }
        if bridge.flag(ctx, "isUntransferable", item)? {
            return Err(clone_error("Unsupported or duplicate transferable"));
        }
        if is_port {
            bridge.call(ctx, "validate", std::slice::from_ref(item))?;
        } else if !transport || !ctx.is_transferable_array_buffer(item) {
            return Err(clone_error("Unsupported or detached transferable"));
        }
    }
    Ok(list)
}

fn write_message(
    ctx: &mut Ctx,
    value: &Value,
    list: &[Value],
    transport: bool,
    bridge: &Bridge,
    locals: Option<&[Value]>,
    limit: usize,
) -> OpResult<Vec<u8>> {
    let mut writer = Writer::new(
        ctx,
        bridge,
        transport,
        locals.unwrap_or(&[]),
        locals.is_some(),
        limit,
    );
    for item in list {
        if !bridge.flag(ctx, "isPort", item)? {
            continue;
        }
        if !transport {
            return Err(clone_error("MessagePort requires native message transport"));
        }
        let index = match bridge.call(ctx, "export", std::slice::from_ref(item))? {
            Value::Num(index) if index >= 0.0 => index as u32,
            _ => return Err(clone_error("MessagePort could not be exported")),
        };
        writer.add_port(item.clone(), index);
    }
    writer.write_ports_header();
    writer.write(ctx, value)?;
    if writer.sink.overflow {
        return Err(writer.too_large());
    }
    // Getters may have transferred an outer buffer or port in a nested message: revalidate the
    // whole list before detaching any sender-owned resource.
    for item in list {
        if bridge.flag(ctx, "isPort", item)? {
            bridge.call(ctx, "validate", std::slice::from_ref(item))?;
        } else if !ctx.is_transferable_array_buffer(item) {
            return Err(clone_error("Transferable was detached during serialization"));
        }
    }
    for item in list {
        if bridge.flag(ctx, "isPort", item)? {
            bridge.call(ctx, "detach", std::slice::from_ref(item))?;
        } else {
            ctx.array_buffer_detach(item);
        }
    }
    Ok(writer.sink.bytes)
}

fn serialize_with(
    ctx: &mut Ctx,
    value: &Value,
    transfer: &[Value],
    transport: bool,
    bridge: &Bridge,
    locals: Option<&[Value]>,
    limit: usize,
) -> OpResult<Vec<u8>> {
    let list = settle_transfer_list(ctx, transfer, transport, bridge)?;
    if transport {
        clone_transfer::begin_frame(ctx)?;
    }
    let result = write_message(ctx, value, &list, transport, bridge, locals, limit);
    if result.is_err() && transport {
        clone_transfer::abort_frame(ctx);
    }
    result
}

fn deserialize_with(
    ctx: &mut Ctx,
    bytes: &[u8],
    bridge: &Bridge,
    locals: &[Value],
) -> OpResult<Value> {
    let result = Reader::new(bytes, bridge, locals).run(ctx);
    clone_transfer::finish_frame(ctx);
    result
}

/// StructuredSerializeWithTransfer: `value` as wire bytes, with the listed ports and
/// `ArrayBuffer`s transferred. With `transport` the attachments wait in a frame that the caller
/// must take with [`clone_transfer::take_message`] (it is dropped when serialization fails);
/// without it, memory sharing and port transfer are refused. `bridge` is the port bridge, or
/// `undefined` for the realm's `__lumenPortClone`. A transported message larger than the realm's
/// [`crate::ports::PortLimits::max_message_bytes`] fails with a `DataCloneError` while it is written.
pub fn serialize(
    ctx: &mut Ctx,
    value: &Value,
    transfer: &[Value],
    transport: bool,
    bridge: &Value,
) -> OpResult<Vec<u8>> {
    let bridge = Bridge::resolve(ctx, bridge);
    let limit = if transport {
        crate::ports::limits(ctx).max_message_bytes
    } else {
        usize::MAX
    };
    serialize_with(ctx, value, transfer, transport, &bridge, None, limit)
}

/// StructuredDeserialize of bytes made by [`serialize`] in any realm of this process, after the
/// attachments were installed with [`clone_transfer::install_message`].
pub fn deserialize(ctx: &mut Ctx, bytes: &[u8], bridge: &Value) -> OpResult<Value> {
    let bridge = Bridge::resolve(ctx, bridge);
    deserialize_with(ctx, bytes, &bridge, &[])
}

/// `structuredClone(value, { transfer })` for an already validated transfer list. A transferable
/// `AbortSignal` in the list is replaced by a linked copy instead of going through the wire.
pub fn structured_clone(ctx: &mut Ctx, value: &Value, transfer: Vec<Value>) -> OpResult<Value> {
    let bridge = Bridge::resolve(ctx, &Value::Undefined);
    let mut list = Vec::with_capacity(transfer.len());
    let mut originals = Vec::new();
    let mut copies = Vec::new();
    for item in transfer {
        if events::is_transferable_signal(ctx, &item) {
            if !originals.iter().any(|earlier| same_object(earlier, &item)) {
                copies.push(events::clone_transferable(ctx, &item)?);
                originals.push(item);
            }
        } else {
            list.push(item);
        }
    }
    let bytes = serialize_with(ctx, value, &list, true, &bridge, Some(&originals), usize::MAX)?;
    let message = clone_transfer::take_message(ctx, bytes);
    let bytes = clone_transfer::install_message(ctx, message);
    deserialize_with(ctx, &bytes, &bridge, &copies)
}

fn transfer_option(ctx: &mut Ctx, options: Option<&Value>) -> OpResult<Vec<Value>> {
    let options = match options {
        None | Some(Value::Undefined | Value::Null) => return Ok(Vec::new()),
        Some(options @ Value::Obj(_)) => options,
        Some(other) => return Err(invalid_arg_type(ctx, "options", "of type object", other)),
    };
    let list = ctx.member_get(options, "transfer").map_err(OpError::thrown)?;
    if matches!(list, Value::Undefined | Value::Null) {
        return Ok(Vec::new());
    }
    let iterator = ctx.well_known_symbol("iterator").expect("Symbol.iterator");
    let iterable = matches!(list, Value::Obj(_))
        && ctx
            .reflect_get(&list, &iterator, &list)
            .map_err(OpError::thrown)?
            .is_callable();
    if !iterable {
        return Err(invalid_arg_type(
            ctx,
            "options.transfer",
            "of type object",
            &list,
        ));
    }
    ctx.iterable_to_list(&list, usize::MAX)
}

/// `structuredClone`, the global.
#[lumen_bind::module(name = "structuredClone")]
pub mod bindings {
    use super::*;

    /// `structuredClone(value, options)`.
    #[op(
        name = "structuredClone",
        hint(js(
            webidl,
            missing_code = "ERR_MISSING_ARGS",
            missing_message = "The \"value\" argument must be specified"
        ))
    )]
    pub fn structured_clone(
        ctx: &mut Ctx,
        value: Value,
        options: Option<Value>,
    ) -> OpResult<Value> {
        let transfer = transfer_option(ctx, options.as_ref())?;
        super::structured_clone(ctx, &value, transfer)
    }
}

/// The byte-level serializer the worker glue calls from script, as hidden globals.
#[lumen_bind::module(name = "structuredCloneInternals")]
pub mod internals {
    use super::*;

    /// `__serializeForClone(value, transfer, transport, bridge)`: the wire bytes as a
    /// `Uint8Array`.
    #[op(name = "__serializeForClone")]
    pub fn serialize_for_clone(
        ctx: &mut Ctx,
        value: Value,
        transfer: Option<Value>,
        transport: Option<Value>,
        bridge: Option<Value>,
    ) -> OpResult<Value> {
        let transfer = match transfer {
            Some(transfer) => transfer_argument(ctx, &transfer)?,
            None => Vec::new(),
        };
        let transport = transport.is_some_and(|flag| ctx.to_boolean(&flag));
        let bridge = bridge.unwrap_or(Value::Undefined);
        let bytes = super::serialize(ctx, &value, &transfer, transport, &bridge)?;
        Ok(ctx.make_uint8array(&bytes)?)
    }

    /// `__deserializeClone(bytes, bridge)`.
    #[op(name = "__deserializeClone")]
    pub fn deserialize_clone(
        ctx: &mut Ctx,
        bytes: Value,
        bridge: Option<Value>,
    ) -> OpResult<Value> {
        let bytes = ctx
            .buffer_source_bytes(&bytes)
            .ok_or_else(|| invalid_arg_type(ctx, "bytes", "an instance of Uint8Array", &bytes))?;
        let bridge = bridge.unwrap_or(Value::Undefined);
        super::deserialize(ctx, &bytes, &bridge)
    }
}
