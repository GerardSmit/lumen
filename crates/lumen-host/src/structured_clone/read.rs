//! `StructuredDeserialize`: rebuild a value from the wire in the receiving realm, from that
//! realm's own intrinsics.

use super::wire::*;
use super::Bridge;
use crate::blob::{self, BlobSnapshot, Bytes};
use crate::clone_transfer;
use lumen::embed::{Ctx, OpError, OpResult, TaKind, Value};
use lumen_common::bigint::BigInt;
use std::collections::HashMap;

const TRANSFER_DESERIALIZE: &str = "lumen.transferable.deserialize";

pub(super) struct Reader<'a> {
    source: Source<'a>,
    memory: Vec<Value>,
    transferred: HashMap<u32, Value>,
    bridge: &'a Bridge,
    locals: &'a [Value],
}

fn define_hidden(ctx: &mut Ctx, target: &Value, key: &str, value: Value) -> OpResult<()> {
    let descriptor = ctx.new_object_with_proto(&Value::Null);
    for (name, entry) in [
        ("value", value),
        ("writable", Value::Bool(true)),
        ("enumerable", Value::Bool(false)),
        ("configurable", Value::Bool(true)),
    ] {
        ctx.member_set(&descriptor, name, entry)
            .map_err(OpError::thrown)?;
    }
    ctx.define_property_value(target, Value::str(key), &descriptor)
        .map_err(OpError::thrown)
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8], bridge: &'a Bridge, locals: &'a [Value]) -> Self {
        Self {
            source: Source::new(bytes),
            memory: Vec::new(),
            transferred: HashMap::new(),
            bridge,
            locals,
        }
    }

    pub fn run(&mut self, ctx: &mut Ctx) -> OpResult<Value> {
        if self.source.is_empty() {
            return Err(malformed());
        }
        if self.source.peek() == Some(T_PORTS) {
            self.source.u8()?;
            for _ in 0..self.source.u32()? {
                let index = self.source.u32()?;
                let port = self.import_port(ctx, index)?;
                self.transferred.insert(index, port);
            }
        }
        self.read(ctx)
    }

    fn import_port(&mut self, ctx: &mut Ctx, index: u32) -> OpResult<Value> {
        if !matches!(self.bridge.value, Value::Obj(_)) {
            return Err(clone_error("MessagePort requires native message transport"));
        }
        self.bridge.call(ctx, "import", &[Value::Num(index as f64)])
    }

    fn push(&mut self, value: Value) -> Value {
        self.memory.push(value.clone());
        value
    }

    fn read(&mut self, ctx: &mut Ctx) -> OpResult<Value> {
        ctx.check_native_stack_for_host().map_err(OpError::thrown)?;
        let tag = self.source.u8()?;
        Ok(match tag {
            T_UNDEFINED => Value::Undefined,
            T_NULL => Value::Null,
            T_FALSE => Value::Bool(false),
            T_TRUE => Value::Bool(true),
            T_NUMBER => Value::Num(self.source.f64()?),
            T_STRING => Value::str(self.source.str()?),
            T_BIGINT => Value::BigInt(self.bigint()?),
            T_REF => {
                let index = self.source.u32()? as usize;
                self.memory.get(index).cloned().ok_or_else(malformed)?
            }
            T_DATE => {
                let time = self.source.f64()?;
                let date = ctx.new_date_value(time);
                self.push(date)
            }
            T_REGEXP => {
                let source = self.source.str()?;
                let flags = self.source.str()?;
                let regexp = ctx.new_regexp_value(source, flags).map_err(OpError::thrown)?;
                self.push(regexp)
            }
            T_BOOLOBJ => {
                let flag = self.source.u8()? != 0;
                let boxed = ctx.new_boxed_primitive(Value::Bool(flag));
                self.push(boxed)
            }
            T_NUMOBJ => {
                let number = self.source.f64()?;
                let boxed = ctx.new_boxed_primitive(Value::Num(number));
                self.push(boxed)
            }
            T_STROBJ => {
                let text = self.source.str()?;
                let boxed = ctx.new_boxed_primitive(Value::str(text));
                self.push(boxed)
            }
            T_BIGINTOBJ => {
                let number = self.bigint()?;
                let boxed = ctx.new_boxed_primitive(Value::BigInt(number));
                self.push(boxed)
            }
            T_ARRAYBUFFER => {
                let length = self.source.u32()? as usize;
                let bytes = self.source.take(length)?.to_vec();
                let buffer = ctx.make_array_buffer_from(bytes);
                self.push(buffer)
            }
            T_SHARED => {
                let index = self.source.u32()?;
                let buffer = clone_transfer::import_shared(ctx, index as f64)?;
                self.push(buffer)
            }
            T_PORT => {
                let index = self.source.u32()?;
                let port = match self.transferred.get(&index) {
                    Some(port) => port.clone(),
                    None => self.import_port(ctx, index)?,
                };
                self.push(port)
            }
            T_LOCAL => {
                let index = self.source.u32()? as usize;
                let local = self.locals.get(index).cloned().ok_or_else(malformed)?;
                self.push(local)
            }
            T_BLOB => {
                let file = self.source.u8()? != 0;
                let content_type = self.source.str()?.to_owned();
                let name = self.source.str()?.to_owned();
                let last_modified = self.source.f64()?;
                let length = self.source.u32()? as usize;
                let bytes = Bytes::new(self.source.take(length)?.to_vec());
                let restored = blob::restore_blob(
                    ctx,
                    BlobSnapshot {
                        file,
                        content_type,
                        name,
                        last_modified,
                        bytes,
                    },
                );
                self.push(restored)
            }
            T_DATAVIEW => {
                let byte_offset = self.source.u32()? as usize;
                let byte_length = self.source.u32()? as usize;
                let slot = self.memory.len();
                self.memory.push(Value::Undefined);
                let buffer = self.read(ctx)?;
                let view = ctx
                    .new_data_view_value(&buffer, byte_offset, byte_length)
                    .map_err(OpError::thrown)?;
                self.memory[slot] = view.clone();
                view
            }
            T_TYPEDARRAY => {
                let kind = TaKind::from_clone_code(self.source.u8()?).ok_or_else(malformed)?;
                let byte_offset = self.source.u32()? as usize;
                let length = self.source.u32()? as usize;
                let slot = self.memory.len();
                self.memory.push(Value::Undefined);
                let buffer = self.read(ctx)?;
                let view = ctx
                    .new_typed_array_view(kind, &buffer, byte_offset, length)
                    .map_err(OpError::thrown)?;
                self.memory[slot] = view.clone();
                view
            }
            T_MAP | T_SET => {
                let set = tag == T_SET;
                let collection = ctx.new_collection_value(set);
                self.push(collection.clone());
                for _ in 0..self.source.u32()? {
                    let key = self.read(ctx)?;
                    let entry = if set { Value::Undefined } else { self.read(ctx)? };
                    ctx.collection_insert(&collection, key, entry);
                }
                collection
            }
            T_ARRAY => self.read_array(ctx)?,
            T_OBJECT => {
                let object = Value::Obj(ctx.new_object());
                self.push(object.clone());
                for _ in 0..self.source.u32()? {
                    let key = self.source.str()?;
                    let entry = self.read(ctx)?;
                    ctx.create_data_property(&object, key, entry)
                        .map_err(OpError::thrown)?;
                }
                object
            }
            T_ERROR => self.read_error(ctx)?,
            T_HOST => self.read_host(ctx)?,
            _ => return Err(malformed()),
        })
    }

    fn bigint(&mut self) -> OpResult<BigInt> {
        BigInt::parse_dec(self.source.str()?).ok_or_else(malformed)
    }

    fn read_array(&mut self, ctx: &mut Ctx) -> OpResult<Value> {
        let length = self.source.u32()?;
        let array = ctx.make_array(Vec::new());
        self.push(array.clone());
        for _ in 0..self.source.u32()? {
            let key = match self.source.u8()? {
                KEY_INDEX => self.source.u32()?.to_string(),
                KEY_NAME => self.source.str()?.to_owned(),
                _ => return Err(malformed()),
            };
            let entry = self.read(ctx)?;
            ctx.create_data_property(&array, &key, entry)
                .map_err(OpError::thrown)?;
        }
        ctx.member_set(&array, "length", Value::Num(length as f64))
            .map_err(OpError::thrown)?;
        Ok(array)
    }

    fn read_error(&mut self, ctx: &mut Ctx) -> OpResult<Value> {
        let kind = ERROR_NAMES
            .get(self.source.u8()? as usize)
            .copied()
            .unwrap_or("Error");
        let message = match self.source.u8()? {
            0 => String::new(),
            _ => self.source.str()?.to_owned(),
        };
        let stack = match self.source.u8()? {
            0 => None,
            _ => Some(self.source.str()?.to_owned()),
        };
        let error = ctx.make_error(kind, message);
        self.push(error.clone());
        if let Some(stack) = stack {
            define_hidden(ctx, &error, "stack", Value::from_string(stack))?;
        }
        if self.source.u8()? != 0 {
            let cause = self.read(ctx)?;
            define_hidden(ctx, &error, "cause", cause)?;
        }
        Ok(error)
    }

    fn read_host(&mut self, ctx: &mut Ctx) -> OpResult<Value> {
        let info = self.source.str()?.to_owned();
        let slot = self.memory.len();
        self.memory.push(Value::Undefined);
        let global = ctx.global_object();
        let resolve = ctx
            .member_get(&global, "__lumenCloneResolve")
            .map_err(OpError::thrown)?;
        let constructor = if resolve.is_callable() {
            ctx.invoke(resolve, Value::Undefined, &[Value::str(info.as_str())])
                .map_err(OpError::thrown)?
        } else {
            Value::Undefined
        };
        if !constructor.is_callable() {
            return Err(clone_error(format!("Cannot deserialize {info}")));
        }
        let data = self.read(ctx)?;
        let prototype = ctx
            .member_get(&constructor, "prototype")
            .map_err(OpError::thrown)?;
        let method = match prototype {
            Value::Obj(_) => {
                let key = ctx.symbol_for(TRANSFER_DESERIALIZE);
                ctx.reflect_get(&prototype, &key, &prototype)
                    .map_err(OpError::thrown)?
            }
            _ => Value::Undefined,
        };
        let value = if method.is_callable() {
            let object = ctx
                .construct_value(constructor, &[])
                .map_err(OpError::thrown)?;
            ctx.invoke(method, object.clone(), &[data])
                .map_err(OpError::thrown)?;
            object
        } else {
            ctx.invoke(constructor, Value::Undefined, &[data])
                .map_err(OpError::thrown)?
        };
        self.memory[slot] = value.clone();
        Ok(value)
    }
}

