//! `_lzma` on the shared liblzma stream in `lumen_common::compress::xz`, with CPython's filter
//! specifier, buffer and error semantics (`Modules/_lzmamodule.c`).

/// Implementation module for lzma.
#[lumen_bind::module(name = "_lzma")]
pub mod _lzma {
    #![allow(clippy::new_ret_no_self)]

    use crate::bind::{opaque_instance, Py, This};
    use crate::object::*;
    use crate::pyint::PyInt;
    use crate::vm::{dict_set_str, Interp};
    use lumen_common::compress::xz::{
        self as xz, Filter, FilterOptions, LzmaOptions, XzError, XzStatus, XzStream, CHECK_CRC64, CHECK_ID_MAX, CHECK_NONE, FILTERS_MAX,
        FILTER_ARM, FILTER_ARMTHUMB, FILTER_DELTA, FILTER_IA64, FILTER_LZMA1, FILTER_LZMA2, FILTER_POWERPC, FILTER_SPARC, FILTER_X86,
        PRESET_DEFAULT, TELL_CHECKS,
    };

    const FORMAT_AUTO: i64 = 0;
    const FORMAT_XZ: i64 = 1;
    const FORMAT_ALONE: i64 = 2;
    const FORMAT_RAW: i64 = 3;
    const CHECK_UNKNOWN: i64 = CHECK_ID_MAX as i64 + 1;
    const INITIAL_BLOCK: usize = 8 * 1024;

    #[derive(Default)]
    pub struct State {
        error: Option<Obj>,
    }

    /// `catch_lzma_error` for a failure.
    fn lzma_error(it: &mut Interp, e: XzError) -> Obj {
        match e.message() {
            Some(m) => lzma_message(it, m),
            None => it.new_exc_str("MemoryError", ""),
        }
    }

    fn lzma_message(it: &mut Interp, msg: String) -> Obj {
        let cls = match it.native_state::<State>().error.clone() {
            Some(c) => c,
            None => it.exc_type("Exception"),
        };
        it.new_exc(&cls, vec![Value::string(msg)])
    }

    /// `PyLong_AsUnsignedLongLong`.
    fn unsigned(it: &mut Interp, v: &Value) -> R<u64> {
        let Some(b) = v.as_bigint() else {
            return Err(it.type_error("an integer is required"));
        };
        if b.is_negative() {
            return Err(it.overflow_err("can't convert negative int to unsigned"));
        }
        match b.to_u64() {
            Some(n) => Ok(n),
            None => Err(it.overflow_err("int too big to convert")),
        }
    }

    /// `INT_TYPE_CONVERTER_FUNC`: an unsigned value that must fit `max`.
    fn bounded(it: &mut Interp, v: &Value, max: u64, ty: &str) -> R<u64> {
        let n = unsigned(it, v)?;
        if n > max {
            return Err(it.overflow_err(&format!("Value too large for {ty} type")));
        }
        Ok(n)
    }

    fn uint32(it: &mut Interp, v: &Value) -> R<u32> {
        bounded(it, v, u32::MAX as u64, "uint32_t").map(|n| n as u32)
    }

    /// `PyMapping_GetItemString`, with a missing key as `None`.
    fn mapping_get(it: &mut Interp, spec: &Value, key: &str) -> R<Option<Value>> {
        match it.getitem(spec, &Value::str(key)) {
            Ok(v) => Ok(Some(v)),
            Err(e) if it.exc_is(&e, "KeyError") => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The keys and values of a filter specifier, as `PyArg_ParseTupleAndKeywords` sees its keyword
    /// dict; `None` when it cannot be iterated as one.
    fn spec_items(it: &mut Interp, spec: &Value) -> Option<Vec<(String, Value)>> {
        let keys = it.iterate_to_vec(spec).ok()?;
        let mut items = Vec::with_capacity(keys.len());
        for k in keys {
            let name = k.as_str()?.to_string();
            let v = it.getitem(spec, &k).ok()?;
            items.push((name, v));
        }
        Some(items)
    }

    fn parse_lzma(it: &mut Interp, spec: &Value, id: u64) -> R<Filter> {
        let mut preset = PRESET_DEFAULT;
        if let Some(v) = mapping_get(it, spec, "preset")? {
            preset = uint32(it, &v)?;
        }
        let Some(mut o) = LzmaOptions::preset(preset) else {
            return Err(lzma_message(it, format!("Invalid compression preset: {preset}")));
        };
        let invalid = |it: &mut Interp| it.value_error("Invalid filter specifier for LZMA filter");
        let Some(items) = spec_items(it, spec) else { return Err(invalid(it)) };
        for (name, v) in items {
            let ok = match name.as_str() {
                "id" | "preset" => Ok(()),
                "dict_size" => uint32(it, &v).map(|n| o.dict_size = n),
                "lc" => uint32(it, &v).map(|n| o.lc = n),
                "lp" => uint32(it, &v).map(|n| o.lp = n),
                "pb" => uint32(it, &v).map(|n| o.pb = n),
                "mode" => bounded(it, &v, u32::MAX as u64, "lzma_mode").map(|n| o.mode = n as u32),
                "nice_len" => uint32(it, &v).map(|n| o.nice_len = n),
                "mf" => bounded(it, &v, u32::MAX as u64, "lzma_match_finder").map(|n| o.mf = n as u32),
                "depth" => uint32(it, &v).map(|n| o.depth = n),
                _ => Err(invalid(it)),
            };
            if ok.is_err() {
                return Err(invalid(it));
            }
        }
        Ok(Filter { id, options: FilterOptions::Lzma(o) })
    }

    /// Delta and BCJ specifiers: `id` plus one optional `field`.
    fn parse_single(it: &mut Interp, spec: &Value, field: &str, default: u32, what: &str) -> R<u32> {
        let invalid = |it: &mut Interp| it.value_error(&format!("Invalid filter specifier for {what} filter"));
        let Some(items) = spec_items(it, spec) else { return Err(invalid(it)) };
        let mut value = default;
        for (name, v) in items {
            if name == "id" {
                continue;
            }
            if name != field {
                return Err(invalid(it));
            }
            match uint32(it, &v) {
                Ok(n) => value = n,
                Err(_) => return Err(invalid(it)),
            }
        }
        Ok(value)
    }

    /// `lzma_filter_converter`.
    fn filter_from_spec(it: &mut Interp, spec: &Value) -> R<Filter> {
        let not_mapping = matches!(spec, Value::None | Value::Int(_) | Value::Bool(_) | Value::Float(_));
        let id_obj = if not_mapping {
            None
        } else {
            match mapping_get(it, spec, "id") {
                Ok(v) => Some(v),
                Err(e) if it.exc_is(&e, "TypeError") => None,
                Err(e) => return Err(e),
            }
        };
        let Some(id_obj) = id_obj else {
            return Err(it.type_error("Filter specifier must be a dict or dict-like object"));
        };
        let Some(id_obj) = id_obj else {
            return Err(it.value_error("Filter specifier must have an \"id\" entry"));
        };
        let id = unsigned(it, &id_obj)?;
        match id {
            FILTER_LZMA1 | FILTER_LZMA2 => parse_lzma(it, spec, id),
            FILTER_DELTA => {
                let dist = parse_single(it, spec, "dist", 1, "delta")?;
                Ok(Filter { id, options: FilterOptions::Delta { dist } })
            }
            FILTER_X86 | FILTER_POWERPC | FILTER_IA64 | FILTER_ARM | FILTER_ARMTHUMB | FILTER_SPARC => {
                let start_offset = parse_single(it, spec, "start_offset", 0, "BCJ")?;
                Ok(Filter { id, options: FilterOptions::Bcj { start_offset: Some(start_offset) } })
            }
            _ => Err(it.value_error(&format!("Invalid filter ID: {id}"))),
        }
    }

    /// `parse_filter_chain_spec`.
    fn parse_chain(it: &mut Interp, specs: &Value) -> R<Vec<Filter>> {
        if matches!(specs, Value::Obj(o) if matches!(o.kind, Kind::Dict(_))) {
            return Err(it.type_error("object of type 'dict' has no len()"));
        }
        let n = it.len_of(specs)?;
        if n > FILTERS_MAX {
            return Err(it.value_error(&format!("Too many filters - liblzma supports a maximum of {FILTERS_MAX}")));
        }
        let mut chain = Vec::with_capacity(n);
        for i in 0..n {
            let spec = it.getitem(specs, &Value::Int(i as i64))?;
            chain.push(filter_from_spec(it, &spec)?);
        }
        Ok(chain)
    }

    /// `build_filter_spec`.
    fn build_filter_spec(it: &mut Interp, f: &Filter) -> R<Value> {
        let d = it.new_dict();
        dict_set_str(&d, "id", Value::Int(f.id as i64));
        match (f.id, &f.options) {
            (FILTER_LZMA1, FilterOptions::Lzma(o)) => {
                dict_set_str(&d, "lc", Value::Int(o.lc as i64));
                dict_set_str(&d, "lp", Value::Int(o.lp as i64));
                dict_set_str(&d, "pb", Value::Int(o.pb as i64));
                dict_set_str(&d, "dict_size", Value::Int(o.dict_size as i64));
            }
            (FILTER_LZMA2, FilterOptions::Lzma(o)) => dict_set_str(&d, "dict_size", Value::Int(o.dict_size as i64)),
            (FILTER_DELTA, FilterOptions::Delta { dist }) => dict_set_str(&d, "dist", Value::Int(*dist as i64)),
            (FILTER_X86 | FILTER_POWERPC | FILTER_IA64 | FILTER_ARM | FILTER_ARMTHUMB | FILTER_SPARC, FilterOptions::Bcj { start_offset }) => {
                if let Some(o) = start_offset {
                    dict_set_str(&d, "start_offset", Value::Int(*o as i64));
                }
            }
            (id, _) => return Err(it.value_error(&format!("Invalid filter ID: {id}"))),
        }
        Ok(Value::Obj(d))
    }

    /// Test whether the given integrity check is supported.
    ///
    /// Always returns True for CHECK_NONE and CHECK_CRC32.
    #[op]
    fn is_check_supported(check_id: i64) -> bool {
        u32::try_from(check_id).is_ok_and(xz::check_is_supported)
    }

    /// Return a bytes object encoding the options (properties) of the filter specified by *filter* (a dict).
    ///
    /// The result does not include the filter ID itself, only the options.
    #[op]
    fn _encode_filter_properties(it: &mut Interp, filter: &Value) -> R<Vec<u8>> {
        let f = filter_from_spec(it, filter)?;
        xz::encode_filter_properties(&f).map_err(|e| lzma_error(it, e))
    }

    /// Return a bytes object encoding the options (properties) of the filter specified by *filter* (a dict).
    ///
    /// The result does not include the filter ID itself, only the options.
    #[op]
    fn _decode_filter_properties(it: &mut Interp, filter_id: &Value, encoded_props: &Value) -> R<Value> {
        let id = unsigned(it, filter_id)?;
        let props = it.buffer_bytes(encoded_props)?;
        let f = xz::decode_filter_properties(id, &props).map_err(|e| lzma_error(it, e))?;
        build_filter_spec(it, &f)
    }

    /// Grows `out` when its window is exhausted; `false` once `limit` forbids more room.
    fn make_room(out: &mut Vec<u8>, produced: usize, limit: Option<usize>) -> bool {
        if produced < out.len() {
            return true;
        }
        let grow = out.len().max(INITIAL_BLOCK);
        let len = match limit {
            Some(m) if produced >= m => return false,
            Some(m) => (out.len() + grow).min(m),
            None => out.len() + grow,
        };
        out.resize(len, 0);
        true
    }

    /// `compress`: `LZMA_RUN` until the input is taken, or `LZMA_FINISH` until the stream ends.
    fn encode(z: &mut XzStream, finish: bool, input: &[u8]) -> Result<Vec<u8>, XzError> {
        let mut out: Vec<u8> = Vec::new();
        make_room(&mut out, 0, None);
        let (mut consumed, mut produced) = (0, 0);
        loop {
            let step = z.code(finish, &input[consumed..], &mut out[produced..]);
            consumed += step.consumed;
            produced += step.produced;
            let status = match step.status {
                Err(XzError::Buf) if input.is_empty() && produced < out.len() => Ok(XzStatus::Ok),
                other => other,
            };
            let status = status?;
            if (!finish && consumed == input.len()) || (finish && status == XzStatus::StreamEnd) {
                break;
            }
            if produced == out.len() {
                make_room(&mut out, produced, None);
            }
        }
        out.truncate(produced);
        Ok(out)
    }

    /// Create a compressor object for compressing data incrementally.
    ///
    ///   format
    ///     The container format to use for the output.  This can
    ///     be FORMAT_XZ (default), FORMAT_ALONE, or FORMAT_RAW.
    ///   check
    ///     The integrity check to use.  For FORMAT_XZ, the default
    ///     is CHECK_CRC64.  FORMAT_ALONE and FORMAT_RAW do not support integrity
    ///     checks; for these formats, check must be omitted, or be CHECK_NONE.
    ///   preset
    ///     If provided should be an integer in the range 0-9, optionally
    ///     OR-ed with the constant PRESET_EXTREME.
    ///   filters
    ///     If provided should be a sequence of dicts.  Each dict should
    ///     have an entry for "id" indicating the ID of the filter, plus
    ///     additional entries for options to the filter.
    ///
    /// The settings used by the compressor can be specified either as a
    /// preset compression level (with the 'preset' argument), or in detail
    /// as a custom filter chain (with the 'filters' argument).  For FORMAT_XZ
    /// and FORMAT_ALONE, the default is to use the PRESET_DEFAULT preset
    /// level.  For FORMAT_RAW, the caller must always specify a filter chain;
    /// the raw compressor does not support preset compression levels.
    ///
    /// For one-shot compression, use the compress() function instead.
    #[class(name = "LZMACompressor", module = "_lzma", hint(py(final)))]
    pub struct LZMACompressor {
        z: XzStream,
        flushed: bool,
    }

    #[methods]
    impl LZMACompressor {
        #[constructor]
        fn new(
            cls: This<Value>,
            it: &mut Interp,
            #[kw] #[default(1)] format: i64,
            #[kw] #[default(-1)] check: i64,
            #[kw] preset: Option<&Value>,
            #[kw] filters: Option<&Value>,
        ) -> R<Value> {
            let preset_obj = preset.filter(|v| !v.is_none());
            let filters = filters.filter(|v| !v.is_none());
            if format != FORMAT_XZ && check != -1 && check != CHECK_NONE as i64 {
                return Err(it.value_error("Integrity checks are only supported by FORMAT_XZ"));
            }
            if preset_obj.is_some() && filters.is_some() {
                return Err(it.value_error("Cannot specify both preset and filter chain"));
            }
            let preset = match preset_obj {
                Some(v) => uint32(it, v)?,
                None => PRESET_DEFAULT,
            };
            let z = match format {
                FORMAT_XZ => {
                    let check = if check == -1 { CHECK_CRC64 } else { check as u32 };
                    match filters {
                        None => XzStream::easy_encoder(preset, check),
                        Some(f) => {
                            let chain = parse_chain(it, f)?;
                            XzStream::stream_encoder(&chain, check)
                        }
                    }
                }
                FORMAT_ALONE => match filters {
                    None => match LzmaOptions::preset(preset) {
                        Some(o) => XzStream::alone_encoder(&o),
                        None => return Err(lzma_message(it, format!("Invalid compression preset: {preset}"))),
                    },
                    Some(f) => {
                        let chain = parse_chain(it, f)?;
                        match chain.as_slice() {
                            [Filter { id: FILTER_LZMA1, options: FilterOptions::Lzma(o) }] => XzStream::alone_encoder(o),
                            _ => {
                                return Err(it.value_error("Invalid filter chain for FORMAT_ALONE - must be a single LZMA1 filter"));
                            }
                        }
                    }
                },
                FORMAT_RAW => match filters {
                    None => return Err(it.value_error("Must specify filters for FORMAT_RAW")),
                    Some(f) => {
                        let chain = parse_chain(it, f)?;
                        XzStream::raw_encoder(&chain)
                    }
                },
                _ => return Err(it.value_error(&format!("Invalid container format: {format}"))),
            };
            let z = z.map_err(|e| lzma_error(it, e))?;
            let Value::Obj(cls) = &cls.0 else { unreachable!() };
            Ok(opaque_instance(cls, LZMACompressor { z, flushed: false }))
        }

        /// Provide data to the compressor object.
        ///
        /// Returns a chunk of compressed data if possible, or b'' otherwise.
        ///
        /// When you have finished providing data to the compressor, call the
        /// flush() method to finish the compression process.
        fn compress(slf: This<Py<Self>>, it: &mut Interp, data: &Value) -> R<Vec<u8>> {
            let data = it.buffer_bytes(data)?;
            let mut s = slf.0.borrow_mut(it)?;
            if s.flushed {
                drop(s);
                return Err(it.value_error("Compressor has been flushed"));
            }
            match encode(&mut s.z, false, &data) {
                Ok(out) => Ok(out),
                Err(e) => {
                    drop(s);
                    Err(lzma_error(it, e))
                }
            }
        }

        /// Finish the compression process.
        ///
        /// Returns the compressed data left in internal buffers.
        ///
        /// The compressor object may not be used after this method is called.
        fn flush(slf: This<Py<Self>>, it: &mut Interp) -> R<Vec<u8>> {
            let mut s = slf.0.borrow_mut(it)?;
            if s.flushed {
                drop(s);
                return Err(it.value_error("Repeated call to flush()"));
            }
            s.flushed = true;
            match encode(&mut s.z, true, &[]) {
                Ok(out) => Ok(out),
                Err(e) => {
                    drop(s);
                    Err(lzma_error(it, e))
                }
            }
        }
    }

    /// Create a decompressor object for decompressing data incrementally.
    ///
    ///   format
    ///     Specifies the container format of the input stream.  If this is
    ///     FORMAT_AUTO (the default), the decompressor will automatically detect
    ///     whether the input is FORMAT_XZ or FORMAT_ALONE.  Streams created with
    ///     FORMAT_RAW cannot be autodetected.
    ///   memlimit
    ///     Limit the amount of memory used by the decompressor.  This will cause
    ///     decompression to fail if the input cannot be decompressed within the
    ///     given limit.
    ///   filters
    ///     A custom filter chain.  This argument is required for FORMAT_RAW, and
    ///     not accepted with any other format.  When provided, this should be a
    ///     sequence of dicts, each indicating the ID and options for a single
    ///     filter.
    ///
    /// For one-shot decompression, use the decompress() function instead.
    #[class(name = "LZMADecompressor", module = "_lzma", hint(py(final)))]
    pub struct LZMADecompressor {
        z: XzStream,
        check: i64,
        eof: bool,
        unused_data: Vec<u8>,
        needs_input: bool,
        /// Input not yet consumed by liblzma.
        pending: Vec<u8>,
    }

    impl LZMADecompressor {
        /// `decompress_buf`: at most `max` bytes out; `pending` is advanced past what was consumed.
        /// Returns the output and whether the output window was left with room.
        fn drive(&mut self, max: Option<usize>) -> Result<(Vec<u8>, bool), XzError> {
            let first = max.map_or(INITIAL_BLOCK, |m| m.min(INITIAL_BLOCK));
            let mut out = vec![0u8; first];
            let (mut consumed, mut produced) = (0, 0);
            let result = loop {
                let step = self.z.code(false, &self.pending[consumed..], &mut out[produced..]);
                consumed += step.consumed;
                produced += step.produced;
                let status = match step.status {
                    Err(XzError::Buf) if consumed == self.pending.len() && produced < out.len() => Ok(XzStatus::Ok),
                    other => other,
                };
                let status = match status {
                    Ok(s) => s,
                    Err(e) => break Err(e),
                };
                if matches!(status, XzStatus::GetCheck | XzStatus::NoCheck) {
                    self.check = self.z.check() as i64;
                }
                if status == XzStatus::StreamEnd {
                    self.eof = true;
                    break Ok(());
                } else if produced == out.len() {
                    if !make_room(&mut out, produced, max) {
                        break Ok(());
                    }
                } else if consumed == self.pending.len() {
                    break Ok(());
                }
            };
            self.pending.drain(..consumed);
            result?;
            let room = produced < out.len();
            out.truncate(produced);
            Ok((out, room))
        }
    }

    #[methods]
    impl LZMADecompressor {
        #[constructor]
        fn new(
            cls: This<Value>,
            it: &mut Interp,
            #[kw] #[default(0)] format: i64,
            #[kw] memlimit: Option<&Value>,
            #[kw] filters: Option<&Value>,
        ) -> R<Value> {
            let memlimit = memlimit.filter(|v| !v.is_none());
            let filters = filters.filter(|v| !v.is_none());
            let mut limit = u64::MAX;
            if let Some(m) = memlimit {
                if format == FORMAT_RAW {
                    return Err(it.value_error("Cannot specify memory limit with FORMAT_RAW"));
                }
                limit = unsigned(it, m)?;
            }
            if format == FORMAT_RAW && filters.is_none() {
                return Err(it.value_error("Must specify filters for FORMAT_RAW"));
            } else if format != FORMAT_RAW && filters.is_some() {
                return Err(it.value_error("Cannot specify filters except with FORMAT_RAW"));
            }
            let mut check = CHECK_UNKNOWN;
            let z = match format {
                FORMAT_AUTO => XzStream::auto_decoder(limit, TELL_CHECKS),
                FORMAT_XZ => XzStream::stream_decoder(limit, TELL_CHECKS),
                FORMAT_ALONE => {
                    check = CHECK_NONE as i64;
                    XzStream::alone_decoder(limit)
                }
                FORMAT_RAW => {
                    check = CHECK_NONE as i64;
                    let chain = parse_chain(it, filters.expect("checked above"))?;
                    XzStream::raw_decoder(&chain)
                }
                _ => return Err(it.value_error(&format!("Invalid container format: {format}"))),
            };
            let z = z.map_err(|e| lzma_error(it, e))?;
            let Value::Obj(cls) = &cls.0 else { unreachable!() };
            let d = LZMADecompressor { z, check, eof: false, unused_data: Vec::new(), needs_input: true, pending: Vec::new() };
            Ok(opaque_instance(cls, d))
        }

        /// Decompress *data*, returning uncompressed data as bytes.
        ///
        /// If *max_length* is nonnegative, returns at most *max_length* bytes of
        /// decompressed data. If this limit is reached and further output can be
        /// produced, *self.needs_input* will be set to ``False``. In this case, the next
        /// call to *decompress()* may provide *data* as b'' to obtain more of the output.
        ///
        /// If all of the input data was decompressed and returned (either because this
        /// was less than *max_length* bytes, or because *max_length* was negative),
        /// *self.needs_input* will be set to True.
        ///
        /// Attempting to decompress data after the end of stream is reached raises an
        /// EOFError.  Any data found after the end of the stream is ignored and saved in
        /// the unused_data attribute.
        fn decompress(slf: This<Py<Self>>, it: &mut Interp, #[kw] data: &Value, #[kw] #[default(-1)] max_length: i64) -> R<Vec<u8>> {
            let data = it.buffer_bytes(data)?;
            let mut guard = slf.0.borrow_mut(it)?;
            let s = &mut *guard;
            if s.eof {
                drop(guard);
                return Err(it.new_exc_str("EOFError", "Already at end of stream"));
            }
            s.pending.extend_from_slice(&data);
            let max = (max_length >= 0).then_some(max_length as usize);
            match s.drive(max) {
                Err(e) => {
                    s.pending.clear();
                    drop(guard);
                    Err(lzma_error(it, e))
                }
                Ok((out, room)) => {
                    if s.eof {
                        s.needs_input = false;
                        if !s.pending.is_empty() {
                            s.unused_data = std::mem::take(&mut s.pending);
                        }
                    } else if s.pending.is_empty() {
                        s.needs_input = room;
                    } else {
                        s.needs_input = false;
                    }
                    Ok(out)
                }
            }
        }

        /// ID of the integrity check used by the input stream.
        #[getter]
        fn check(&self) -> i64 {
            self.check
        }

        /// True if the end-of-stream marker has been reached.
        #[getter]
        fn eof(&self) -> bool {
            self.eof
        }

        /// True if more input is needed before more decompressed data can be produced.
        #[getter]
        fn needs_input(&self) -> bool {
            self.needs_input
        }

        /// Data found after the end of the compressed stream.
        #[getter]
        fn unused_data(&self) -> Vec<u8> {
            self.unused_data.clone()
        }
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        let exc = it.exc_type("Exception");
        let err = crate::builtins::native::new_type(it, "_lzma", "LZMAError", Some(&exc), Layout::Exception);
        dict_set_str(&d, "LZMAError", Value::Obj(err.clone()));
        it.native_state::<State>().error = Some(err);
        let ints: [(&str, i64); 28] = [
            ("FORMAT_AUTO", FORMAT_AUTO),
            ("FORMAT_XZ", FORMAT_XZ),
            ("FORMAT_ALONE", FORMAT_ALONE),
            ("FORMAT_RAW", FORMAT_RAW),
            ("CHECK_NONE", CHECK_NONE as i64),
            ("CHECK_CRC32", xz::CHECK_CRC32 as i64),
            ("CHECK_CRC64", CHECK_CRC64 as i64),
            ("CHECK_SHA256", xz::CHECK_SHA256 as i64),
            ("CHECK_ID_MAX", CHECK_ID_MAX as i64),
            ("CHECK_UNKNOWN", CHECK_UNKNOWN),
            ("FILTER_LZMA1", FILTER_LZMA1 as i64),
            ("FILTER_LZMA2", FILTER_LZMA2 as i64),
            ("FILTER_DELTA", FILTER_DELTA as i64),
            ("FILTER_X86", FILTER_X86 as i64),
            ("FILTER_IA64", FILTER_IA64 as i64),
            ("FILTER_ARM", FILTER_ARM as i64),
            ("FILTER_ARMTHUMB", FILTER_ARMTHUMB as i64),
            ("FILTER_SPARC", FILTER_SPARC as i64),
            ("FILTER_POWERPC", FILTER_POWERPC as i64),
            ("MF_HC3", xz::MF_HC3 as i64),
            ("MF_HC4", xz::MF_HC4 as i64),
            ("MF_BT2", xz::MF_BT2 as i64),
            ("MF_BT3", xz::MF_BT3 as i64),
            ("MF_BT4", xz::MF_BT4 as i64),
            ("MODE_FAST", xz::MODE_FAST as i64),
            ("MODE_NORMAL", xz::MODE_NORMAL as i64),
            ("PRESET_DEFAULT", PRESET_DEFAULT as i64),
            ("PRESET_EXTREME", xz::PRESET_EXTREME as i64),
        ];
        for (name, v) in ints {
            dict_set_str(&d, name, Value::Int(v));
        }
    }
}
