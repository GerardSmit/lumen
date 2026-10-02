//! Cross-interpreter data (CPython's `_PyCrossInterpreterData`): the values one interpreter may
//! hand to another, copied into a form that is `Send` so it can cross the thread each interpreter
//! runs on. Used by `_xxsubinterpreters` (the `shared` namespace of `run_string`),
//! `_xxinterpchannels` (channel items) and `_testcapi`.

use super::interpchanm::_xxinterpchannels as chans;
use crate::bind::{is_instance, Interp};
use crate::object::*;
use crate::builtins::native::with_opaque;

/// The exact types CPython 3.12 can share: `None`, `bytes`, `str`, `int` (that fits an `i64`) and
/// channel IDs.
#[derive(Clone, Debug)]
pub enum Shared {
    None,
    Bytes(Vec<u8>),
    Str(String),
    Int(i64),
    Channel { id: i64, end: i32, resolve: bool },
}

pub fn share(it: &mut Interp, v: &Value) -> R<Shared> {
    match v {
        Value::None => return Ok(Shared::None),
        Value::Int(i) => return Ok(Shared::Int(*i)),
        Value::Obj(o) => {
            if is_instance::<chans::ChannelID>(it, v) {
                if let Some((id, end, resolve)) = with_opaque::<chans::ChannelID, _>(v, |c| (c.id, c.end, c.resolve)) {
                    return Ok(Shared::Channel { id, end, resolve });
                }
            }
            if o.cls.is_none() {
                match &o.kind {
                    Kind::Bytes(b) => return Ok(Shared::Bytes(b.clone())),
                    Kind::Str(s) => return Ok(Shared::Str(s.s.to_string())),
                    Kind::Int(_) => return Err(it.overflow_err("Python int too large to convert to C long")),
                    _ => {}
                }
            }
        }
        _ => {}
    }
    let text = it.str_of(v)?;
    Err(it.value_error(&format!("{text} does not support cross-interpreter data")))
}

pub fn unshare(it: &mut Interp, data: &Shared) -> R<Value> {
    match data {
        Shared::None => Ok(Value::None),
        Shared::Bytes(b) => Ok(Value::bytes(b.clone())),
        Shared::Str(s) => Ok(Value::str(s)),
        Shared::Int(i) => Ok(Value::Int(*i)),
        Shared::Channel { id, end, resolve } => chans::channel_id_from_shared(it, *id, *end, *resolve),
    }
}
