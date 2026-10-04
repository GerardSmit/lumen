//! Typed encoding primitives shared by network and native HTML embedders.
#[lumen_bind::module(name = "__lumenEncoding")]
pub mod bindings {
    use lumen::embed::{Ctx, OpError, OpResult, Value};

    #[op]
    pub fn encode(ctx: &mut Ctx, input: Value) -> OpResult<Value> {
        let input = ctx.coerce_string(&input).map_err(OpError::thrown)?;
        let bytes = crate::well_formed_utf8(&input);
        ctx.make_uint8array(bytes.as_bytes()).map_err(OpError::thrown)
    }

    #[op]
    pub fn decode(ctx: &mut Ctx, input: Value, #[default(false)] fatal: bool) -> OpResult<Value> {
        let bytes = ctx.typed_array_bytes(&input)
            .ok_or_else(|| OpError::new("TypeError", "TextDecoder.decode expects a BufferSource"))?;
        let text = if fatal {
            String::from_utf8(bytes).map_err(|_| OpError::new("TypeError", "TextDecoder: invalid utf-8 (fatal)"))?
        } else { String::from_utf8_lossy(&bytes).into_owned() };
        Ok(Value::from_string(lumen_common::smuggle::utf16_text_owned(text)))
    }

    #[op]
    pub fn btoa(ctx: &mut Ctx, input: Value) -> OpResult<Value> {
        let input = ctx.coerce_string(&input).map_err(OpError::thrown)?;
        let mut bytes = Vec::with_capacity(input.len());
        for character in input.chars() {
            let Ok(byte) = u8::try_from(u32::from(character)) else { return Ok(Value::Null) };
            bytes.push(byte);
        }
        Ok(Value::from_string(lumen_common::codec::base64_encode(&bytes, false, true)))
    }

    #[op]
    pub fn atob(ctx: &mut Ctx, input: Value) -> OpResult<Value> {
        let input = ctx.coerce_string(&input).map_err(OpError::thrown)?;
        Ok(match lumen_common::codec::base64_decode_forgiving(input.as_bytes()) {
            Some(bytes) => Value::from_string(bytes.into_iter().map(char::from).collect::<String>()),
            None => Value::Null,
        })
    }
}
