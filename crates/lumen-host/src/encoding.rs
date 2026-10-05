//! Typed encoding primitives shared by network and native HTML embedders.
#[lumen_bind::module(name = "__lumenEncoding")]
pub mod bindings {
    use lumen::embed::{Ctx, OpError, OpResult, Value};

    #[op]
    pub fn label(label: &str) -> Option<String> {
        lumen_common::encoding::canonical_label(label)
            .ok()
            .map(str::to_ascii_lowercase)
    }

    #[class(name = "Decoder")]
    pub struct Decoder {
        decoder: lumen_common::encoding::TextDecoder,
    }

    fn decode_error(error: lumen_common::encoding::DecodeError) -> OpError {
        use lumen_common::encoding::DecodeError;
        match error {
            DecodeError::UnsupportedLabel => {
                OpError::new("RangeError", "encoding label is unsupported")
            }
            DecodeError::Malformed => OpError::new("TypeError", "encoded data is malformed"),
            DecodeError::ResourceLimit => {
                OpError::new("RangeError", "decoded text exceeds the host resource limit")
            }
        }
    }

    #[methods]
    impl Decoder {
        #[constructor]
        fn new(label: &str, fatal: bool, ignore_bom: bool) -> OpResult<Self> {
            Ok(Self {
                decoder: lumen_common::encoding::TextDecoder::new(label, fatal, ignore_bom)
                    .map_err(decode_error)?,
            })
        }

        #[getter]
        fn encoding(&self) -> String {
            self.decoder.encoding().to_ascii_lowercase()
        }

        fn decode(&mut self, ctx: &mut Ctx, input: Value, stream: bool) -> OpResult<Value> {
            let length = match ctx.typed_array_byte_len(&input) {
                Some(length) => length,
                // Option conversion can detach a view after Web IDL conversion.
                // Such an input contributes no bytes; preserve the decoder's flush semantics.
                None if ctx.typed_array_kind(&input).is_some() => 0,
                None => return Err(OpError::type_error("decoder input must be a BufferSource")),
            };
            if length > lumen_common::encoding::MAX_DECODED_BYTES {
                return Err(decode_error(
                    lumen_common::encoding::DecodeError::ResourceLimit,
                ));
            }
            let text = if length == 0 {
                self.decoder.decode(&[], stream)
            } else {
                ctx.with_typed_array_bytes(&input, |bytes| self.decoder.decode(bytes, stream))
                    .ok_or_else(|| OpError::type_error("decoder input must be a BufferSource"))?
            }
            .map_err(decode_error)?;
            Ok(Value::from_string(lumen_common::smuggle::utf16_text_owned(
                text,
            )))
        }
    }

    #[op]
    pub fn encode(ctx: &mut Ctx, input: Value) -> OpResult<Value> {
        let input = ctx.coerce_string(&input).map_err(OpError::thrown)?;
        let bytes = crate::well_formed_utf8(&input);
        ctx.make_uint8array(bytes.as_bytes())
            .map_err(OpError::thrown)
    }

    #[op]
    pub fn decode(ctx: &mut Ctx, input: Value, #[default(false)] fatal: bool) -> OpResult<Value> {
        let bytes = ctx.typed_array_bytes(&input).ok_or_else(|| {
            OpError::new("TypeError", "TextDecoder.decode expects a BufferSource")
        })?;
        let text = if fatal {
            String::from_utf8(bytes)
                .map_err(|_| OpError::new("TypeError", "TextDecoder: invalid utf-8 (fatal)"))?
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        };
        Ok(Value::from_string(lumen_common::smuggle::utf16_text_owned(
            text,
        )))
    }

    #[op]
    pub fn btoa(ctx: &mut Ctx, input: Value) -> OpResult<Value> {
        let input = ctx.coerce_string(&input).map_err(OpError::thrown)?;
        let mut bytes = Vec::with_capacity(input.len());
        for character in input.chars() {
            let Ok(byte) = u8::try_from(u32::from(character)) else {
                return Ok(Value::Null);
            };
            bytes.push(byte);
        }
        Ok(Value::from_string(lumen_common::codec::base64_encode(
            &bytes, false, true,
        )))
    }

    #[op]
    pub fn atob(ctx: &mut Ctx, input: Value) -> OpResult<Value> {
        let input = ctx.coerce_string(&input).map_err(OpError::thrown)?;
        Ok(
            match lumen_common::codec::base64_decode_forgiving(input.as_bytes()) {
                Some(bytes) => {
                    Value::from_string(bytes.into_iter().map(char::from).collect::<String>())
                }
                None => Value::Null,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::embed::Value;

    #[test]
    fn typed_streaming_decoder_preserves_state_brands_and_buffer_ranges() {
        let mut engine = lumen::Engine::new();
        crate::namespace::<bindings::Module>(engine.ctx())
            .ok()
            .expect("encoding namespace");
        let result = engine
            .eval_value(
                r#"(() => {
            const decoder = new __lumenEncoding.Decoder('shift_jis', true, false);
            const source = Uint8Array.of(0,0xa0,0);
            if (__lumenEncoding.label(' ASCII ') !== 'windows-1252') return false;
            if (decoder.encoding !== 'shift_jis') return false;
            if (decoder.decode(Uint8Array.of(0x82),true) !== '') return false;
            if (decoder.decode(source.subarray(1,2),false) !== 'あ') return false;
            let invalid = false;
            try { decoder.decode.call({},source,false); }
            catch (e) { invalid = e instanceof TypeError; }
            if (!invalid) return false;
            let fatal = false;
            try { decoder.decode(Uint8Array.of(0x82),false); }
            catch (e) { fatal = e instanceof TypeError; }
            if (!fatal || decoder.decode(Uint8Array.of(65),false) !== 'A') return false;
            const utf8 = new __lumenEncoding.Decoder('utf-8',false,false);
            if (utf8.decode(Uint8Array.of(0xef),true) !== '') return false;
            if (utf8.decode(Uint8Array.of(0xbb,0xbf,65),false) !== 'A') return false;
            if (utf8.decode(Uint8Array.of(0xef,0xbb,0xbf,66),false) !== 'B') return false;
            return true;
        })()"#,
            )
            .expect("guard parses")
            .ok()
            .expect("guard executes");
        assert!(matches!(result, Value::Bool(true)));
    }
}
