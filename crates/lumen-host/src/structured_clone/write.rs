//! `StructuredSerializeInternal`: walk a value and append it to the wire. Objects are classified
//! by their internal slots ([`CloneBrand`]), never by properties the script can replace; the
//! only author code that runs is what the specification itself runs (`[[Get]]` of own
//! properties, `stack` and the host-object protocol).

use super::wire::*;
use super::Bridge;
use crate::blob;
use crate::clone_transfer;
use lumen::embed::{CloneBrand, Ctx, OpError, OpResult, Value};
use std::collections::HashMap;

pub(super) const TRANSFER_CLONE: &str = "lumen.transferable.clone";
pub(super) const UNTRANSFERABLE: &str = "nodejs.untransferable";

fn pointer(value: &Value) -> usize {
    match value {
        Value::Obj(_) => lumen::embed::object_identity(value),
        _ => 0,
    }
}

pub(super) fn same_object(a: &Value, b: &Value) -> bool {
    matches!((a, b), (Value::Obj(_), Value::Obj(_))) && pointer(a) == pointer(b)
}

/// How an object reads in "could not be cloned" messages.
fn describe(ctx: &mut Ctx, value: &Value) -> String {
    let name = ctx
        .member_get(value, "constructor")
        .and_then(|constructor| match constructor {
            Value::Obj(_) => ctx.member_get(&constructor, "name"),
            _ => Ok(Value::Undefined),
        })
        .ok()
        .and_then(|name| match name {
            Value::Str(name) if !name.is_empty() => Some(name.to_string()),
            _ => None,
        })
        .unwrap_or_else(|| "Object".to_owned());
    format!("#<{name}> could not be cloned.")
}

/// The array-index form of an own key (`"0"` .. `"4294967294"`), if it is one.
fn array_index(key: &str) -> Option<u32> {
    let index: u32 = key.parse().ok()?;
    (index != u32::MAX && index.to_string() == key).then_some(index)
}

/// An own data property's value, or `None` for an accessor or a missing property.
fn own_data_value(ctx: &mut Ctx, object: &Value, key: &str) -> OpResult<Option<Value>> {
    let name = Value::str(key);
    let descriptor = ctx
        .reflect_get_own_property_descriptor(object, &name)
        .map_err(OpError::thrown)?;
    if !matches!(descriptor, Value::Obj(_))
        || !ctx
            .has_own_property_value(&descriptor, &Value::str("value"))
            .map_err(OpError::thrown)?
    {
        return Ok(None);
    }
    Ok(Some(
        ctx.member_get(&descriptor, "value").map_err(OpError::thrown)?,
    ))
}

pub(super) struct Writer<'a> {
    pub sink: Sink,
    memory: HashMap<usize, u32>,
    keep: Vec<Value>,
    bridge: &'a Bridge,
    transport: bool,
    port_indices: Vec<(Value, u32)>,
    locals: &'a [Value],
    local: bool,
    transfer_clone: Value,
}

impl<'a> Writer<'a> {
    pub fn new(
        ctx: &mut Ctx,
        bridge: &'a Bridge,
        transport: bool,
        locals: &'a [Value],
        local: bool,
    ) -> Self {
        Self {
            sink: Sink::default(),
            memory: HashMap::new(),
            keep: Vec::new(),
            bridge,
            transport,
            port_indices: Vec::new(),
            locals,
            local,
            transfer_clone: ctx.symbol_for(TRANSFER_CLONE),
        }
    }

    pub fn add_port(&mut self, port: Value, index: u32) {
        self.port_indices.push((port, index));
    }

    pub fn write_ports_header(&mut self) {
        if self.port_indices.is_empty() {
            return;
        }
        self.sink.u8(T_PORTS);
        self.sink.u32(self.port_indices.len() as u32);
        for (_, index) in &self.port_indices {
            self.sink.u32(*index);
        }
    }

    fn register(&mut self, value: &Value) {
        let index = self.memory.len() as u32;
        self.memory.insert(pointer(value), index);
        self.keep.push(value.clone());
    }

    pub fn write(&mut self, ctx: &mut Ctx, value: &Value) -> OpResult<()> {
        match value {
            Value::Undefined | Value::Empty => self.sink.u8(T_UNDEFINED),
            Value::Null => self.sink.u8(T_NULL),
            Value::Bool(flag) => self.sink.u8(if *flag { T_TRUE } else { T_FALSE }),
            Value::Num(number) => {
                self.sink.u8(T_NUMBER);
                self.sink.f64(*number);
            }
            Value::Str(text) => {
                self.sink.u8(T_STRING);
                self.sink.str(text.as_str());
            }
            Value::BigInt(number) => {
                self.sink.u8(T_BIGINT);
                self.sink.str(&number.to_string_radix(10));
            }
            Value::Sym(symbol) => {
                return Err(clone_error(format!(
                    "Symbol({}) could not be cloned.",
                    symbol.description.as_deref().unwrap_or("")
                )));
            }
            Value::Obj(_) => return self.write_object(ctx, value),
        }
        Ok(())
    }

    fn write_object(&mut self, ctx: &mut Ctx, value: &Value) -> OpResult<()> {
        ctx.check_native_stack_for_host().map_err(OpError::thrown)?;
        if let Some(&index) = self.memory.get(&pointer(value)) {
            self.sink.u8(T_REF);
            self.sink.u32(index);
            return Ok(());
        }
        if let Some(index) = self.locals.iter().position(|local| same_object(local, value)) {
            self.register(value);
            self.sink.u8(T_LOCAL);
            self.sink.u32(index as u32);
            return Ok(());
        }
        let brand = ctx.clone_brand(value);
        if let CloneBrand::Uncloneable(kind) = brand {
            return Err(clone_error(if kind == "Function" {
                format!("{} could not be cloned.", ctx.function_source_text(value))
            } else {
                format!("#<{kind}> could not be cloned.")
            }));
        }
        if self.bridge.flag(ctx, "isUncloneable", value)? {
            return Err(clone_error("Object marked uncloneable"));
        }
        match brand {
            CloneBrand::Date(time) => {
                self.register(value);
                self.sink.u8(T_DATE);
                self.sink.f64(time);
            }
            CloneBrand::RegExp { source, flags } => {
                self.register(value);
                self.sink.u8(T_REGEXP);
                self.sink.str(&source);
                self.sink.str(&flags);
            }
            CloneBrand::Boolean(flag) => {
                self.register(value);
                self.sink.u8(T_BOOLOBJ);
                self.sink.u8(flag as u8);
            }
            CloneBrand::Number(number) => {
                self.register(value);
                self.sink.u8(T_NUMOBJ);
                self.sink.f64(number);
            }
            CloneBrand::String(Value::Str(text)) => {
                self.register(value);
                self.sink.u8(T_STROBJ);
                self.sink.str(text.as_str());
            }
            CloneBrand::BigInt(Value::BigInt(number)) => {
                self.register(value);
                self.sink.u8(T_BIGINTOBJ);
                self.sink.str(&number.to_string_radix(10));
            }
            CloneBrand::SharedArrayBuffer => {
                if !self.transport {
                    return Err(clone_error("SharedArrayBuffer sharing requires runtime"));
                }
                self.register(value);
                let index = clone_transfer::stage_shared(ctx, value)?;
                self.sink.u8(T_SHARED);
                self.sink.u32(index as u32);
            }
            CloneBrand::ArrayBuffer { detached } => {
                if detached {
                    return Err(clone_error("An ArrayBuffer is detached and could not be cloned."));
                }
                self.register(value);
                self.sink.u8(T_ARRAYBUFFER);
                self.write_buffer_bytes(ctx, value);
            }
            CloneBrand::DataView {
                buffer,
                byte_offset,
                byte_length,
            } => {
                let Some(byte_length) = byte_length else {
                    return Err(clone_error("DataView is out of bounds or its buffer is detached."));
                };
                self.register(value);
                self.sink.u8(T_DATAVIEW);
                self.sink.u32(byte_offset as u32);
                self.sink.u32(byte_length as u32);
                self.write(ctx, &buffer)?;
            }
            CloneBrand::TypedArray {
                kind,
                buffer,
                byte_offset,
                length,
            } => {
                let Some(length) = length else {
                    return Err(clone_error("Typed array is out of bounds or its buffer is detached."));
                };
                self.register(value);
                self.sink.u8(T_TYPEDARRAY);
                self.sink.u8(kind.clone_code());
                self.sink.u32(byte_offset as u32);
                self.sink.u32(length as u32);
                self.write(ctx, &buffer)?;
            }
            CloneBrand::Map => {
                self.register(value);
                let entries = ctx.collection_entries(value).unwrap_or_default();
                self.sink.u8(T_MAP);
                self.sink.u32(entries.len() as u32);
                for (key, entry) in &entries {
                    self.write(ctx, key)?;
                    self.write(ctx, entry)?;
                }
            }
            CloneBrand::Set => {
                self.register(value);
                let entries = ctx.collection_entries(value).unwrap_or_default();
                self.sink.u8(T_SET);
                self.sink.u32(entries.len() as u32);
                for (key, _) in &entries {
                    self.write(ctx, key)?;
                }
            }
            CloneBrand::Error => self.write_error(ctx, value)?,
            CloneBrand::Array => self.write_array(ctx, value)?,
            CloneBrand::Ordinary => self.write_ordinary(ctx, value)?,
            CloneBrand::String(_) | CloneBrand::BigInt(_) | CloneBrand::Uncloneable(_) => {
                return Err(clone_error(describe(ctx, value)));
            }
        }
        Ok(())
    }

    fn write_buffer_bytes(&mut self, ctx: &mut Ctx, buffer: &Value) {
        let length = ctx.with_buffer_source_bytes(buffer, <[u8]>::len).unwrap_or(0);
        self.sink.u32(length as u32);
        let sink = &mut self.sink;
        ctx.with_buffer_source_bytes(buffer, |bytes| sink.raw(bytes));
    }

    fn write_ordinary(&mut self, ctx: &mut Ctx, value: &Value) -> OpResult<()> {
        if self.bridge.flag(ctx, "isPort", value)? {
            let Some((_, index)) = self
                .port_indices
                .iter()
                .find(|(port, _)| same_object(port, value))
            else {
                let message =
                    "Object that needs transfer was found in message but not listed in transferList";
                return Err(if self.local {
                    clone_error(message)
                } else {
                    OpError::type_error(message)
                        .with_code("ERR_MISSING_TRANSFERABLE_IN_TRANSFER_LIST")
                });
            };
            let index = *index;
            self.register(value);
            self.sink.u8(T_PORT);
            self.sink.u32(index);
            return Ok(());
        }
        if let Some(snapshot) = blob::snapshot_blob(ctx, value) {
            let snapshot = snapshot?;
            self.register(value);
            self.sink.u8(T_BLOB);
            self.sink.u8(snapshot.file as u8);
            self.sink.str(&snapshot.content_type);
            self.sink.str(&snapshot.name);
            self.sink.f64(snapshot.last_modified);
            self.sink.u32(snapshot.bytes.len() as u32);
            self.sink.raw(&snapshot.bytes);
            return Ok(());
        }
        let method = ctx
            .reflect_get(value, &self.transfer_clone, value)
            .map_err(OpError::thrown)?;
        if method.is_callable() {
            self.register(value);
            let cloned = ctx
                .invoke(method, value.clone(), &[])
                .map_err(OpError::thrown)?;
            let data = ctx.member_get(&cloned, "data").map_err(OpError::thrown)?;
            let info = ctx
                .member_get(&cloned, "deserializeInfo")
                .map_err(OpError::thrown)?;
            let info = ctx.coerce_string(&info).map_err(OpError::thrown)?;
            self.sink.u8(T_HOST);
            self.sink.str(&info);
            return self.write(ctx, &data);
        }
        if ctx.is_native_instance(value) {
            return Err(clone_error(describe(ctx, value)));
        }
        self.register(value);
        self.sink.u8(T_OBJECT);
        let count_at = self.sink.bytes.len();
        self.sink.u32(0);
        let mut count = 0;
        for key in ctx.enumerable_own_string_keys(value).map_err(OpError::thrown)? {
            let Some(entry) = self.own_entry(ctx, value, &key)? else {
                continue;
            };
            let Value::Str(name) = &key else { continue };
            self.sink.str(name.as_str());
            self.write(ctx, &entry)?;
            count += 1;
        }
        self.sink.patch_u32(count_at, count);
        Ok(())
    }

    /// `HasOwnProperty(value, key)` then `Get(value, key)`: the property may have gone away
    /// while earlier getters ran.
    fn own_entry(&mut self, ctx: &mut Ctx, value: &Value, key: &Value) -> OpResult<Option<Value>> {
        if !ctx.has_own_property_value(value, key).map_err(OpError::thrown)? {
            return Ok(None);
        }
        ctx.reflect_get(value, key, value)
            .map(Some)
            .map_err(OpError::thrown)
    }

    fn write_array(&mut self, ctx: &mut Ctx, value: &Value) -> OpResult<()> {
        self.register(value);
        let length = match ctx.member_get(value, "length").map_err(OpError::thrown)? {
            Value::Num(length) => length as u32,
            _ => 0,
        };
        self.sink.u8(T_ARRAY);
        self.sink.u32(length);
        let count_at = self.sink.bytes.len();
        self.sink.u32(0);
        let mut count = 0;
        for key in ctx.enumerable_own_string_keys(value).map_err(OpError::thrown)? {
            let Some(entry) = self.own_entry(ctx, value, &key)? else {
                continue;
            };
            let Value::Str(name) = &key else { continue };
            match array_index(name.as_str()) {
                Some(index) => {
                    self.sink.u8(KEY_INDEX);
                    self.sink.u32(index);
                }
                None => {
                    self.sink.u8(KEY_NAME);
                    self.sink.str(name.as_str());
                }
            }
            self.write(ctx, &entry)?;
            count += 1;
        }
        self.sink.patch_u32(count_at, count);
        Ok(())
    }

    fn write_error(&mut self, ctx: &mut Ctx, value: &Value) -> OpResult<()> {
        self.register(value);
        let name = ctx.member_get(value, "name").map_err(OpError::thrown)?;
        let name_index = match &name {
            Value::Str(name) => ERROR_NAMES
                .iter()
                .position(|known| *known == name.as_str())
                .unwrap_or(0),
            _ => 0,
        };
        self.sink.u8(T_ERROR);
        self.sink.u8(name_index as u8);
        let mut message = own_data_value(ctx, value, "message")?;
        if message.is_none() && ctx.is_native_instance(value) {
            // A native error class (DOMException) keeps its message in a prototype accessor.
            message = Some(ctx.member_get(value, "message").map_err(OpError::thrown)?);
        }
        match message {
            Some(message) => {
                let message = ctx.coerce_string(&message).map_err(OpError::thrown)?;
                self.sink.u8(1);
                self.sink.str(&message);
            }
            None => self.sink.u8(0),
        }
        match ctx.member_get(value, "stack").map_err(OpError::thrown)? {
            Value::Str(stack) => {
                self.sink.u8(1);
                self.sink.str(stack.as_str());
            }
            _ => self.sink.u8(0),
        }
        match own_data_value(ctx, value, "cause")? {
            Some(cause) => {
                self.sink.u8(1);
                self.write(ctx, &cause)?;
            }
            None => self.sink.u8(0),
        }
        Ok(())
    }
}
