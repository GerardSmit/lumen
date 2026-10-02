//! `binascii` over the shared codecs in `lumen_common::codec` (base64, hex, uu, quoted-printable,
//! CRC-CCITT) and zlib's CRC-32 in `lumen_common::compress`.

/// Conversion between binary data and ASCII
#[lumen_bind::module(name = "binascii")]
pub mod binascii {
    use crate::object::*;
    use crate::vm::{dict_set_str, Interp};
    use lumen_common::codec;

    #[derive(Default)]
    pub struct State {
        error: Option<Obj>,
    }

    fn error(it: &mut Interp, msg: &str) -> Obj {
        let cls = match it.native_state::<State>().error.clone() {
            Some(c) => c,
            None => it.exc_type("ValueError"),
        };
        it.new_exc(&cls, vec![Value::str(msg)])
    }

    fn buffer(it: &mut Interp, v: &Value) -> R<Vec<u8>> {
        it.buffer_bytes(v)
    }

    /// CPython's `ascii_buffer` converter: a bytes-like object or an ASCII-only str.
    fn ascii_buffer(it: &mut Interp, v: &Value) -> R<Vec<u8>> {
        if let Some(s) = v.as_str() {
            if !s.is_ascii() {
                return Err(it.value_error("string argument should contain only ASCII characters"));
            }
            return Ok(s.as_bytes().to_vec());
        }
        buffer(it, v).map_err(|_| {
            let t = it.type_name_of(v);
            it.type_error(&format!("argument should be bytes, buffer or ASCII string, not '{t}'"))
        })
    }

    /// The `crc`/`value` argument: any int, reduced to its low 32 bits (`bitwise=True`).
    fn crc_arg(it: &mut Interp, v: Option<&Value>, default: u32) -> R<u32> {
        let Some(v) = v else { return Ok(default) };
        let n = it.call_method(v, "__and__", vec![Value::Int(0xffff_ffff)])?;
        Ok(it.index_of(&n)? as u32)
    }

    /// Decode a line of uuencoded data.
    #[op]
    fn a2b_uu(it: &mut Interp, data: &Value) -> R<Vec<u8>> {
        let data = ascii_buffer(it, data)?;
        codec::uu_decode_line(&data).map_err(|e| error(it, e))
    }

    /// Uuencode line of data.
    #[op]
    fn b2a_uu(it: &mut Interp, data: &Value, #[kwonly] #[default(false)] backtick: bool) -> R<Vec<u8>> {
        let data = buffer(it, data)?;
        codec::uu_encode_line(&data, backtick).map_err(|e| error(it, e))
    }

    /// Decode a line of base64 data.
    ///
    ///   strict_mode
    ///     When set to True, bytes that are not part of the base64 standard are not allowed.
    ///     The same applies to excess data after padding (= / ==).
    #[op]
    fn a2b_base64(it: &mut Interp, data: &Value, #[kwonly] #[default(false)] strict_mode: bool) -> R<Vec<u8>> {
        let data = ascii_buffer(it, data)?;
        codec::base64_decode_binascii(&data, strict_mode).map_err(|e| error(it, &e.to_string()))
    }

    /// Base64-code line of data.
    #[op]
    fn b2a_base64(it: &mut Interp, data: &Value, #[kwonly] #[default(true)] newline: bool) -> R<Vec<u8>> {
        let data = buffer(it, data)?;
        let mut out = codec::base64_encode(&data, false, true).into_bytes();
        if newline {
            out.push(b'\n');
        }
        Ok(out)
    }

    /// Compute CRC-CCITT incrementally.
    #[op]
    fn crc_hqx(it: &mut Interp, data: &Value, crc: &Value) -> R<u32> {
        let data = buffer(it, data)?;
        let crc = crc_arg(it, Some(crc), 0)?;
        Ok(codec::crc_hqx(&data, crc))
    }

    /// Compute CRC-32 incrementally.
    #[op]
    fn crc32(it: &mut Interp, data: &Value, crc: Option<&Value>) -> R<u32> {
        let data = buffer(it, data)?;
        let crc = crc_arg(it, crc, 0)?;
        Ok(lumen_common::compress::crc32_from(crc, &data))
    }

    fn hex_impl(it: &mut Interp, data: &Value, sep: Option<&Value>, bytes_per_sep: Option<&Value>) -> R<Vec<u8>> {
        let data = buffer(it, data)?;
        let sep = crate::builtins::memview::hex_sep_arg(it, sep)?;
        let per = match bytes_per_sep {
            Some(v) => it.index_of(v)?,
            None => 1,
        };
        Ok(codec::hex_encode_sep(&data, sep, per).into_bytes())
    }

    /// Hexadecimal representation of binary data.
    ///
    ///   sep
    ///     An optional single character or byte to separate hex bytes.
    ///   bytes_per_sep
    ///     How many bytes between separators.  Positive values count from the
    ///     right, negative values count from the left.
    ///
    /// The return value is a bytes object.  This function is also
    /// available as "hexlify()".
    ///
    /// Example:
    /// >>> binascii.b2a_hex(b'\xb9\x01\xef')
    /// b'b901ef'
    /// >>> binascii.hexlify(b'\xb9\x01\xef', ':')
    /// b'b9:01:ef'
    /// >>> binascii.b2a_hex(b'\xb9\x01\xef', b'_', 2)
    /// b'b9_01ef'
    #[op]
    fn b2a_hex(it: &mut Interp, #[kw] data: &Value, #[kw] sep: Option<&Value>, #[kw] bytes_per_sep: Option<&Value>) -> R<Vec<u8>> {
        hex_impl(it, data, sep, bytes_per_sep)
    }

    /// Hexadecimal representation of binary data.
    ///
    ///   sep
    ///     An optional single character or byte to separate hex bytes.
    ///   bytes_per_sep
    ///     How many bytes between separators.  Positive values count from the
    ///     right, negative values count from the left.
    ///
    /// The return value is a bytes object.  This function is also
    /// available as "b2a_hex()".
    #[op]
    fn hexlify(it: &mut Interp, #[kw] data: &Value, #[kw] sep: Option<&Value>, #[kw] bytes_per_sep: Option<&Value>) -> R<Vec<u8>> {
        hex_impl(it, data, sep, bytes_per_sep)
    }

    fn unhex_impl(it: &mut Interp, hexstr: &Value) -> R<Vec<u8>> {
        let s = ascii_buffer(it, hexstr)?;
        if s.len() % 2 != 0 {
            return Err(error(it, "Odd-length string"));
        }
        let mut out = Vec::with_capacity(s.len() / 2);
        for pair in s.chunks(2) {
            match (codec::hex_digit(pair[0]), codec::hex_digit(pair[1])) {
                (Some(h), Some(l)) => out.push(h << 4 | l),
                _ => return Err(error(it, "Non-hexadecimal digit found")),
            }
        }
        Ok(out)
    }

    /// Binary data of hexadecimal representation.
    ///
    /// hexstr must contain an even number of hex digits (upper or lower case).
    /// This function is also available as "unhexlify()".
    #[op]
    fn a2b_hex(it: &mut Interp, hexstr: &Value) -> R<Vec<u8>> {
        unhex_impl(it, hexstr)
    }

    /// Binary data of hexadecimal representation.
    ///
    /// hexstr must contain an even number of hex digits (upper or lower case).
    #[op]
    fn unhexlify(it: &mut Interp, hexstr: &Value) -> R<Vec<u8>> {
        unhex_impl(it, hexstr)
    }

    /// Decode a string of qp-encoded data.
    #[op]
    fn a2b_qp(it: &mut Interp, #[kw] data: &Value, #[kw] #[default(false)] header: bool) -> R<Vec<u8>> {
        let data = ascii_buffer(it, data)?;
        Ok(codec::qp_decode(&data, header))
    }

    /// Encode a string using quoted-printable encoding.
    ///
    /// On encoding, when istext is set, newlines are not encoded, and white
    /// space at end of lines is.  When istext is not set, \r and \n (CR/LF)
    /// are both encoded.  When quotetabs is set, space and tabs are encoded.
    #[op]
    fn b2a_qp(
        it: &mut Interp,
        #[kw] data: &Value,
        #[kw] #[default(false)] quotetabs: bool,
        #[kw] #[default(true)] istext: bool,
        #[kw] #[default(false)] header: bool,
    ) -> R<Vec<u8>> {
        let data = buffer(it, data)?;
        Ok(codec::qp_encode(&data, quotetabs, istext, header))
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        let value_error = it.exc_type("ValueError");
        let err = crate::builtins::native::new_type(it, "binascii", "Error", Some(&value_error), Layout::Exception);
        let exc = it.exc_type("Exception");
        let incomplete = crate::builtins::native::new_type(it, "binascii", "Incomplete", Some(&exc), Layout::Exception);
        dict_set_str(&d, "Error", Value::Obj(err.clone()));
        dict_set_str(&d, "Incomplete", Value::Obj(incomplete));
        it.native_state::<State>().error = Some(err);
    }
}
