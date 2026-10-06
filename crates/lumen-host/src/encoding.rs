//! The WHATWG Encoding interfaces (`TextEncoder`, `TextDecoder`) and the base64 globals
//! (`atob`, `btoa`) as typed natives, shared by network and native HTML embedders.
//!
//! Install with `lazy_globals::<bindings::Module>`: the classes are only built when a program
//! first touches one of the names.
#[lumen_bind::module(name = "webEncoding")]
pub mod bindings {
    use crate::webidl::{coded, received_suffix};
    use lumen::embed::{Ctx, OpError, OpResult, TaKind, This, Value};
    use std::{cell::RefCell, rc::Rc};

    /// An `options` argument: absent, `null` or an object. Anything else is Node's
    /// `ERR_INVALID_ARG_TYPE`.
    fn require_options(ctx: &mut Ctx, options: &Value) -> OpResult<()> {
        match options {
            Value::Undefined | Value::Null | Value::Obj(_) => Ok(()),
            _ => Err(coded(
                OpError::type_error(format!(
                    "The \"options\" argument must be of type object.{}",
                    received_suffix(ctx, options)
                )),
                "ERR_INVALID_ARG_TYPE",
            )),
        }
    }

    fn option_flag(ctx: &mut Ctx, options: &Value, name: &str) -> OpResult<bool> {
        if !matches!(options, Value::Obj(_)) {
            return Ok(false);
        }
        let value = ctx.member_get(options, name).map_err(OpError::thrown)?;
        Ok(ctx.to_boolean(&value))
    }

    fn decode_error(error: lumen_common::encoding::DecodeError, encoding: &str) -> OpError {
        use lumen_common::encoding::DecodeError;
        match error {
            DecodeError::UnsupportedLabel => OpError::range_error("encoding label is unsupported"),
            DecodeError::Malformed => coded(
                OpError::type_error(format!(
                    "The encoded data was not valid for encoding {encoding}"
                )),
                "ERR_ENCODING_INVALID_ENCODED_DATA",
            ),
            DecodeError::ResourceLimit => {
                OpError::range_error("decoded text exceeds the host resource limit")
            }
        }
    }

    #[class(name = "TextEncoder", hint(js(webidl)))]
    pub struct TextEncoder;

    #[methods]
    impl TextEncoder {
        #[constructor]
        fn new() -> Self {
            TextEncoder
        }

        #[getter]
        fn encoding(&self) -> String {
            "utf-8".into()
        }

        /// A lone surrogate encodes as U+FFFD.
        #[method(coerce)]
        fn encode(&self, #[default("")] input: &str) -> Vec<u8> {
            lumen::well_formed_utf8(input).into_owned().into_bytes()
        }

        /// Encoding §8.1.2: as much of `source` as fits, whole UTF-8 sequences only; `read`
        /// counts UTF-16 code units.
        #[method(coerce)]
        fn encode_into(
            &self,
            ctx: &mut Ctx,
            source: &str,
            destination: Value,
        ) -> OpResult<Value> {
            if ctx.typed_array_kind(&destination) != Some(TaKind::U8) {
                return Err(OpError::type_error(
                    "TextEncoder.encodeInto: destination must be a Uint8Array",
                ));
            }
            let text = lumen::well_formed_utf8(source);
            let (read, written) = ctx
                .with_typed_array_bytes_mut(&destination, |destination| {
                    if text.len() <= destination.len() {
                        destination[..text.len()].copy_from_slice(text.as_bytes());
                        return (lumen_common::smuggle::utf16_unit_len(source), text.len());
                    }
                    let (mut read, mut written) = (0, 0);
                    for character in text.chars() {
                        let width = character.len_utf8();
                        if written + width > destination.len() {
                            break;
                        }
                        character.encode_utf8(&mut destination[written..]);
                        written += width;
                        read += character.len_utf16();
                    }
                    (read, written)
                })
                .unwrap_or((0, 0));
            Ok(ctx.plain_object(&[
                ("read", Value::Num(read as f64)),
                ("written", Value::Num(written as f64)),
            ]))
        }
    }

    #[class(name = "TextDecoder", hint(js(webidl)))]
    pub struct TextDecoder {
        decoder: lumen_common::encoding::TextDecoder,
        encoding: String,
        fatal: bool,
        ignore_bom: bool,
    }

    fn decoder_of(ctx: &Ctx, receiver: &Value) -> OpResult<Rc<RefCell<TextDecoder>>> {
        ctx.instance_data::<TextDecoder>(receiver).ok_or_else(|| {
            coded(
                OpError::type_error("Value of \"this\" must be of type TextDecoder"),
                "ERR_INVALID_THIS",
            )
        })
    }

    #[methods]
    impl TextDecoder {
        #[constructor]
        fn new(
            ctx: &mut Ctx,
            #[default(Value::Undefined)] label: Value,
            #[default(Value::Undefined)] options: Value,
        ) -> OpResult<Self> {
            let label = match label {
                Value::Undefined => "utf-8".into(),
                label => ctx.coerce_string(&label).map_err(OpError::thrown)?,
            };
            let unsupported = || {
                coded(
                    OpError::range_error(format!("The \"{label}\" encoding is not supported")),
                    "ERR_ENCODING_NOT_SUPPORTED",
                )
            };
            let encoding = lumen_common::encoding::canonical_label(&label)
                .map_err(|_| unsupported())?
                .to_ascii_lowercase();
            require_options(ctx, &options)?;
            let fatal = option_flag(ctx, &options, "fatal")?;
            let ignore_bom = option_flag(ctx, &options, "ignoreBOM")?;
            let decoder = lumen_common::encoding::TextDecoder::new(&encoding, fatal, ignore_bom)
                .map_err(|_| unsupported())?;
            Ok(Self {
                decoder,
                encoding,
                fatal,
                ignore_bom,
            })
        }

        #[getter]
        fn encoding(this: This<Value>, ctx: &mut Ctx) -> OpResult<String> {
            Ok(decoder_of(ctx, &this)?.borrow().encoding.clone())
        }

        #[getter]
        fn fatal(this: This<Value>, ctx: &mut Ctx) -> OpResult<bool> {
            Ok(decoder_of(ctx, &this)?.borrow().fatal)
        }

        #[getter(rename(js = "ignoreBOM"))]
        fn ignore_bom(this: This<Value>, ctx: &mut Ctx) -> OpResult<bool> {
            Ok(decoder_of(ctx, &this)?.borrow().ignore_bom)
        }

        /// `input` is read after `options`, so a `stream` getter that detaches the buffer
        /// leaves nothing to decode.
        fn decode(
            this: This<Value>,
            ctx: &mut Ctx,
            #[default(Value::Undefined)] input: Value,
            #[default(Value::Undefined)] options: Value,
        ) -> OpResult<Value> {
            let state = decoder_of(ctx, &this)?;
            let has_input = !matches!(input, Value::Undefined);
            if has_input && ctx.with_buffer_source_bytes(&input, |_| ()).is_none() {
                return Err(OpError::type_error(
                    "TextDecoder input must be a BufferSource",
                ));
            }
            require_options(ctx, &options)?;
            let stream = option_flag(ctx, &options, "stream")?;
            let mut state = state.try_borrow_mut().map_err(|_| {
                OpError::type_error("TextDecoder.decode: decoder is already in use")
            })?;
            let encoding = state.encoding.clone();
            let decoded = if has_input {
                ctx.with_buffer_source_bytes(&input, |bytes| {
                    if bytes.len() > lumen_common::encoding::MAX_DECODED_BYTES {
                        return Err(lumen_common::encoding::DecodeError::ResourceLimit);
                    }
                    state.decoder.decode(bytes, stream)
                })
                .unwrap_or_else(|| state.decoder.decode(&[], stream))
            } else {
                state.decoder.decode(&[], stream)
            }
            .map_err(|error| decode_error(error, &encoding))?;
            Ok(Value::from_string(lumen_common::smuggle::utf16_text_owned(
                decoded,
            )))
        }

        /// `util.inspect` output, keyed by the `nodejs.util.inspect.custom` registry symbol.
        #[method(hint(js(symbol_for = "nodejs.util.inspect.custom")))]
        fn inspect_custom(
            this: This<Value>,
            ctx: &mut Ctx,
            #[default(Value::Undefined)] depth: Value,
            #[default(Value::Undefined)] options: Value,
            #[default(Value::Undefined)] inspect: Value,
        ) -> OpResult<Value> {
            let (encoding, fatal, ignore_bom) = {
                let state = decoder_of(ctx, &this)?;
                let state = state.borrow();
                (state.encoding.clone(), state.fatal, state.ignore_bom)
            };
            if matches!(depth, Value::Num(depth) if depth < 0.0) {
                return Ok(this.0.clone());
            }
            let shown = ctx.plain_object(&[
                ("encoding", Value::from_string(encoding)),
                ("fatal", Value::Bool(fatal)),
                ("ignoreBOM", Value::Bool(ignore_bom)),
            ]);
            let constructor = ctx.member_get(&this, "constructor").map_err(OpError::thrown)?;
            let name = ctx.member_get(&constructor, "name").map_err(OpError::thrown)?;
            let name = ctx.coerce_string(&name).map_err(OpError::thrown)?;
            let text = if inspect.is_callable() {
                ctx.invoke(inspect, Value::Undefined, &[shown, options])
                    .map_err(OpError::thrown)?
            } else {
                let global = ctx.global_object();
                let json = ctx.member_get(&global, "JSON").map_err(OpError::thrown)?;
                let stringify = ctx.member_get(&json, "stringify").map_err(OpError::thrown)?;
                ctx.invoke(stringify, json, &[shown]).map_err(OpError::thrown)?
            };
            let text = ctx.coerce_string(&text).map_err(OpError::thrown)?;
            Ok(Value::from_string(format!("{name} {text}")))
        }
    }

    /// `btoa(data)`: Latin-1 text to base64.
    #[op(
        coerce,
        hint(js(
            webidl,
            missing_code = "ERR_MISSING_ARGS",
            missing_message = "The \"input\" argument must be specified"
        ))
    )]
    pub fn btoa(data: &str) -> OpResult<String> {
        let mut bytes = Vec::with_capacity(data.len());
        for character in data.chars() {
            let Ok(byte) = u8::try_from(u32::from(character)) else {
                return Err(OpError::new(
                    "InvalidCharacterError",
                    "btoa: character beyond latin1 range",
                ));
            };
            bytes.push(byte);
        }
        Ok(lumen_common::codec::base64_encode(&bytes, false, true))
    }

    /// `atob(data)`: forgiving-base64 to Latin-1 text.
    #[op(
        coerce,
        hint(js(
            webidl,
            missing_code = "ERR_MISSING_ARGS",
            missing_message = "The \"input\" argument must be specified"
        ))
    )]
    pub fn atob(data: &str) -> OpResult<String> {
        lumen_common::codec::base64_decode_forgiving(data.as_bytes())
            .map(|bytes| bytes.into_iter().map(char::from).collect())
            .ok_or_else(|| OpError::new("InvalidCharacterError", "atob: invalid base64"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::embed::Value;

    fn eval(source: &str) -> Value {
        let mut engine = lumen::Engine::new();
        crate::globals::<bindings::Module>(engine.ctx())
            .ok()
            .expect("encoding globals");
        engine
            .eval_value(source)
            .expect("script parses")
            .ok()
            .expect("script runs")
    }

    #[test]
    fn streaming_decoder_preserves_state_brands_and_buffer_ranges() {
        let result = eval(
            r#"(() => {
            const decoder = new TextDecoder('shift_jis', { fatal: true });
            const source = Uint8Array.of(0,0xa0,0);
            if (decoder.encoding !== 'shift_jis' || !decoder.fatal || decoder.ignoreBOM) return false;
            if (decoder.decode(Uint8Array.of(0x82), { stream: true }) !== '') return false;
            if (decoder.decode(source.subarray(1,2)) !== 'あ') return false;
            let invalid = false;
            try { decoder.decode.call({}, source); }
            catch (e) { invalid = e instanceof TypeError && e.code === 'ERR_INVALID_THIS'; }
            if (!invalid) return false;
            let fatal = false;
            try { decoder.decode(Uint8Array.of(0x82)); }
            catch (e) { fatal = e instanceof TypeError && e.code === 'ERR_ENCODING_INVALID_ENCODED_DATA'; }
            if (!fatal || decoder.decode(Uint8Array.of(65)) !== 'A') return false;
            const utf8 = new TextDecoder();
            if (utf8.decode(Uint8Array.of(0xef), { stream: true }) !== '') return false;
            if (utf8.decode(Uint8Array.of(0xbb,0xbf,65)) !== 'A') return false;
            if (utf8.decode(Uint8Array.of(0xef,0xbb,0xbf,66)) !== 'B') return false;
            return true;
        })()"#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }
}
