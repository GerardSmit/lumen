// node:zlib — Node 20.11's lib/zlib.js (MIT, Copyright Node.js contributors), ported over a JS
// implementation of `internalBinding('zlib')`: the Zlib, BrotliEncoder and BrotliDecoder handles
// run native codecs (`__zlib.handle*` ops: lumen-node's zlib.rs over lumen-host's codec wrappers
// around zlib-rs, brotli and zstd) with zlib's write(flush, in, inOff, inLen, out, outOff, outLen)
// contract. Beyond Node 20.11 it keeps `crc32` and the Zstd classes (Node 22's API, over one-shot
// zstd).

{
  const { Buffer } = __builtins.get("buffer");
  const { Transform, finished } = __builtins.get("stream");
  const util = __builtins.get("util");
  const { isArrayBufferView, isAnyArrayBuffer, isUint8Array } = util.types;
  const { ERR_INVALID_ARG_TYPE, ERR_OUT_OF_RANGE } = __errors;
  const { validateFunction, validateNumber } = __validators;
  const kMaxLength = __builtins.get("buffer").kMaxLength;

  const ERR_BUFFER_TOO_LARGE = (max) =>
    __nodeError(RangeError, "ERR_BUFFER_TOO_LARGE", `Cannot create a Buffer larger than ${max} bytes`);
  const ERR_BROTLI_INVALID_PARAM = (param) =>
    __nodeError(RangeError, "ERR_BROTLI_INVALID_PARAM", `${param} is not a valid Brotli parameter`);
  const ERR_ZLIB_INITIALIZATION_FAILED = () =>
    __nodeError(Error, "ERR_ZLIB_INITIALIZATION_FAILED", "Initialization failed");
  function genericNodeError(message, errorProperties) {
    const err = new Error(message);
    Object.assign(err, errorProperties);
    return err;
  }
  function assert(value, message) {
    if (!value) throw __nodeError(Error, "ERR_INTERNAL_ASSERTION", message || "Assertion failed");
  }

  // ---- internalBinding('constants').zlib ------------------------------------------------------

  const constants = {
    Z_NO_FLUSH: 0, Z_PARTIAL_FLUSH: 1, Z_SYNC_FLUSH: 2, Z_FULL_FLUSH: 3, Z_FINISH: 4, Z_BLOCK: 5,
    Z_OK: 0, Z_STREAM_END: 1, Z_NEED_DICT: 2, Z_ERRNO: -1, Z_STREAM_ERROR: -2, Z_DATA_ERROR: -3,
    Z_MEM_ERROR: -4, Z_BUF_ERROR: -5, Z_VERSION_ERROR: -6, Z_NO_COMPRESSION: 0, Z_BEST_SPEED: 1,
    Z_BEST_COMPRESSION: 9, Z_DEFAULT_COMPRESSION: -1, Z_FILTERED: 1, Z_HUFFMAN_ONLY: 2, Z_RLE: 3,
    Z_FIXED: 4, Z_DEFAULT_STRATEGY: 0, ZLIB_VERNUM: 4865, DEFLATE: 1, INFLATE: 2, GZIP: 3,
    GUNZIP: 4, DEFLATERAW: 5, INFLATERAW: 6, UNZIP: 7, BROTLI_DECODE: 8, BROTLI_ENCODE: 9,
    ZSTD_COMPRESS: 10, ZSTD_DECOMPRESS: 11, Z_MIN_WINDOWBITS: 8, Z_MAX_WINDOWBITS: 15,
    Z_DEFAULT_WINDOWBITS: 15, Z_MIN_CHUNK: 64, Z_MAX_CHUNK: Infinity, Z_DEFAULT_CHUNK: 16384,
    Z_MIN_MEMLEVEL: 1, Z_MAX_MEMLEVEL: 9, Z_DEFAULT_MEMLEVEL: 8, Z_MIN_LEVEL: -1, Z_MAX_LEVEL: 9,
    Z_DEFAULT_LEVEL: -1, BROTLI_OPERATION_PROCESS: 0, BROTLI_OPERATION_FLUSH: 1,
    BROTLI_OPERATION_FINISH: 2, BROTLI_OPERATION_EMIT_METADATA: 3, BROTLI_PARAM_MODE: 0,
    BROTLI_MODE_GENERIC: 0, BROTLI_MODE_TEXT: 1, BROTLI_MODE_FONT: 2, BROTLI_DEFAULT_MODE: 0,
    BROTLI_PARAM_QUALITY: 1, BROTLI_MIN_QUALITY: 0, BROTLI_MAX_QUALITY: 11,
    BROTLI_DEFAULT_QUALITY: 11, BROTLI_PARAM_LGWIN: 2, BROTLI_MIN_WINDOW_BITS: 10,
    BROTLI_MAX_WINDOW_BITS: 24, BROTLI_LARGE_MAX_WINDOW_BITS: 30, BROTLI_DEFAULT_WINDOW: 22,
    BROTLI_PARAM_LGBLOCK: 3, BROTLI_MIN_INPUT_BLOCK_BITS: 16, BROTLI_MAX_INPUT_BLOCK_BITS: 24,
    BROTLI_PARAM_DISABLE_LITERAL_CONTEXT_MODELING: 4, BROTLI_PARAM_SIZE_HINT: 5,
    BROTLI_PARAM_LARGE_WINDOW: 6, BROTLI_PARAM_NPOSTFIX: 7, BROTLI_PARAM_NDIRECT: 8,
    BROTLI_DECODER_RESULT_ERROR: 0, BROTLI_DECODER_RESULT_SUCCESS: 1,
    BROTLI_DECODER_RESULT_NEEDS_MORE_INPUT: 2, BROTLI_DECODER_RESULT_NEEDS_MORE_OUTPUT: 3,
    BROTLI_DECODER_PARAM_DISABLE_RING_BUFFER_REALLOCATION: 0, BROTLI_DECODER_PARAM_LARGE_WINDOW: 1,
    BROTLI_DECODER_NO_ERROR: 0, BROTLI_DECODER_SUCCESS: 1, BROTLI_DECODER_NEEDS_MORE_INPUT: 2,
    BROTLI_DECODER_NEEDS_MORE_OUTPUT: 3, BROTLI_DECODER_ERROR_FORMAT_EXUBERANT_NIBBLE: -1,
    BROTLI_DECODER_ERROR_FORMAT_RESERVED: -2, BROTLI_DECODER_ERROR_FORMAT_EXUBERANT_META_NIBBLE: -3,
    BROTLI_DECODER_ERROR_FORMAT_SIMPLE_HUFFMAN_ALPHABET: -4,
    BROTLI_DECODER_ERROR_FORMAT_SIMPLE_HUFFMAN_SAME: -5, BROTLI_DECODER_ERROR_FORMAT_CL_SPACE: -6,
    BROTLI_DECODER_ERROR_FORMAT_HUFFMAN_SPACE: -7, BROTLI_DECODER_ERROR_FORMAT_CONTEXT_MAP_REPEAT: -8,
    BROTLI_DECODER_ERROR_FORMAT_BLOCK_LENGTH_1: -9, BROTLI_DECODER_ERROR_FORMAT_BLOCK_LENGTH_2: -10,
    BROTLI_DECODER_ERROR_FORMAT_TRANSFORM: -11, BROTLI_DECODER_ERROR_FORMAT_DICTIONARY: -12,
    BROTLI_DECODER_ERROR_FORMAT_WINDOW_BITS: -13, BROTLI_DECODER_ERROR_FORMAT_PADDING_1: -14,
    BROTLI_DECODER_ERROR_FORMAT_PADDING_2: -15, BROTLI_DECODER_ERROR_FORMAT_DISTANCE: -16,
    BROTLI_DECODER_ERROR_DICTIONARY_NOT_SET: -19, BROTLI_DECODER_ERROR_INVALID_ARGUMENTS: -20,
    BROTLI_DECODER_ERROR_ALLOC_CONTEXT_MODES: -21, BROTLI_DECODER_ERROR_ALLOC_TREE_GROUPS: -22,
    BROTLI_DECODER_ERROR_ALLOC_CONTEXT_MAP: -25, BROTLI_DECODER_ERROR_ALLOC_RING_BUFFER_1: -26,
    BROTLI_DECODER_ERROR_ALLOC_RING_BUFFER_2: -27, BROTLI_DECODER_ERROR_ALLOC_BLOCK_TYPE_TREES: -30,
    BROTLI_DECODER_ERROR_UNREACHABLE: -31, ZSTD_e_continue: 0, ZSTD_e_flush: 1, ZSTD_e_end: 2,
    ZSTD_fast: 1, ZSTD_dfast: 2, ZSTD_greedy: 3, ZSTD_lazy: 4, ZSTD_lazy2: 5, ZSTD_btlazy2: 6,
    ZSTD_btopt: 7, ZSTD_btultra: 8, ZSTD_btultra2: 9, ZSTD_c_compressionLevel: 100,
    ZSTD_c_windowLog: 101, ZSTD_c_hashLog: 102, ZSTD_c_chainLog: 103, ZSTD_c_searchLog: 104,
    ZSTD_c_minMatch: 105, ZSTD_c_targetLength: 106, ZSTD_c_strategy: 107,
    ZSTD_c_enableLongDistanceMatching: 160, ZSTD_c_ldmHashLog: 161, ZSTD_c_ldmMinMatch: 162,
    ZSTD_c_ldmBucketSizeLog: 163, ZSTD_c_ldmHashRateLog: 164, ZSTD_c_contentSizeFlag: 200,
    ZSTD_c_checksumFlag: 201, ZSTD_c_dictIDFlag: 202, ZSTD_c_nbWorkers: 400, ZSTD_c_jobSize: 401,
    ZSTD_c_overlapLog: 402, ZSTD_d_windowLogMax: 100, ZSTD_CLEVEL_DEFAULT: 3,
    ZSTD_error_no_error: 0, ZSTD_error_GENERIC: 1, ZSTD_error_prefix_unknown: 10,
    ZSTD_error_version_unsupported: 12, ZSTD_error_frameParameter_unsupported: 14,
    ZSTD_error_frameParameter_windowTooLarge: 16, ZSTD_error_corruption_detected: 20,
    ZSTD_error_checksum_wrong: 22, ZSTD_error_literals_headerWrong: 24,
    ZSTD_error_dictionary_corrupted: 30, ZSTD_error_dictionary_wrong: 32,
    ZSTD_error_dictionaryCreation_failed: 34, ZSTD_error_parameter_unsupported: 40,
    ZSTD_error_parameter_combination_unsupported: 41, ZSTD_error_parameter_outOfBound: 42,
    ZSTD_error_tableLog_tooLarge: 44, ZSTD_error_maxSymbolValue_tooLarge: 46,
    ZSTD_error_maxSymbolValue_tooSmall: 48, ZSTD_error_stabilityCondition_notRespected: 50,
    ZSTD_error_stage_wrong: 60, ZSTD_error_init_missing: 62, ZSTD_error_memory_allocation: 64,
    ZSTD_error_workSpace_tooSmall: 66, ZSTD_error_dstSize_tooSmall: 70,
    ZSTD_error_srcSize_wrong: 72, ZSTD_error_dstBuffer_null: 74,
    ZSTD_error_noForwardProgress_destFull: 80, ZSTD_error_noForwardProgress_inputEmpty: 82,
  };
  Object.freeze(constants);

  const {
    Z_NO_FLUSH, Z_BLOCK, Z_PARTIAL_FLUSH, Z_SYNC_FLUSH, Z_FULL_FLUSH, Z_FINISH,
    Z_MIN_CHUNK, Z_MIN_WINDOWBITS, Z_MAX_WINDOWBITS, Z_MIN_LEVEL, Z_MAX_LEVEL,
    Z_MIN_MEMLEVEL, Z_MAX_MEMLEVEL, Z_DEFAULT_CHUNK, Z_DEFAULT_COMPRESSION,
    Z_DEFAULT_STRATEGY, Z_DEFAULT_WINDOWBITS, Z_DEFAULT_MEMLEVEL, Z_FIXED,
    DEFLATE, DEFLATERAW, INFLATE, INFLATERAW, GZIP, GUNZIP, UNZIP,
    BROTLI_DECODE, BROTLI_ENCODE, ZSTD_COMPRESS, ZSTD_DECOMPRESS,
    BROTLI_OPERATION_PROCESS, BROTLI_OPERATION_FLUSH,
    BROTLI_OPERATION_FINISH, BROTLI_OPERATION_EMIT_METADATA,
    ZSTD_e_continue, ZSTD_e_flush, ZSTD_e_end,
  } = constants;

  // ---- internalBinding('zlib') ----------------------------------------------------------------

  const owner_symbol = Symbol("owner_symbol");
  const kNoBytes = new Uint8Array(0);
  // A handle dropped without close() releases its native codec when collected.
  const handleFinalizer = typeof FinalizationRegistry === "function"
    ? new FinalizationRegistry((id) => __zlib.handleClose(id))
    : null;

  const asBytes = (view) =>
    (view instanceof Uint8Array ? view : new Uint8Array(view.buffer, view.byteOffset, view.byteLength));

  // The handle is a thin shell over a native codec (`__zlib.handle*`). Like Node's, write() runs
  // the codec over one window pair and reports [availOut, availIn] in the write state; the async
  // form completes on a later turn.
  class CompressionHandle {
    constructor(mode) {
      this._mode = mode;
      this._id = 0;
      this._writeState = null;
      this._processCallback = null;
      this._pendingClose = false;
      this.writeInProgress = false;
    }

    _open(...args) {
      this._id = __zlib.handleOpen(this._mode, ...args);
      handleFinalizer?.register(this, this._id, this);
    }

    _write(flush, input, inOff, inLen, out, outOff, outLen) {
      if (!this._id) return ["zlib binding closed", constants.Z_STREAM_ERROR, "Z_STREAM_ERROR"];
      const result = __zlib.handleWrite(this._id, flush, asBytes(input), inOff, inLen, out, outOff, outLen);
      if (typeof result[0] === "string") return result;
      this._writeState[0] = result[0];
      this._writeState[1] = result[1];
      return null;
    }

    _fail(error) {
      this.onerror(error[0], error[1], error[2]);
    }

    writeSync(flush, input, inOff, inLen, out, outOff, outLen) {
      const error = this._write(flush, input, inOff, inLen, out, outOff, outLen);
      if (error) this._fail(error);
    }

    write(flush, input, inOff, inLen, out, outOff, outLen) {
      this.writeInProgress = true;
      const error = this._write(flush, input, inOff, inLen, out, outOff, outLen);
      setImmediate(() => {
        this.writeInProgress = false;
        if (this._pendingClose) {
          this.close();
          return;
        }
        if (error) this._fail(error);
        else this._processCallback.call(this);
      });
    }

    params() {}

    reset() {
      if (this._id) __zlib.handleReset(this._id);
    }

    close() {
      if (this.writeInProgress) {
        this._pendingClose = true;
        return;
      }
      this._pendingClose = false;
      if (this._id) {
        __zlib.handleClose(this._id);
        handleFinalizer?.unregister(this);
        this._id = 0;
      }
    }

    getAsyncId() { return -1; }
  }

  class ZlibHandle extends CompressionHandle {
    init(windowBits, level, memLevel, strategy, writeState, processCallback, dictionary) {
      this._writeState = writeState;
      this._processCallback = processCallback;
      this._open(windowBits, level, memLevel, strategy, dictionary === undefined ? undefined : asBytes(dictionary));
    }

    params(level, strategy) {
      if (!this._id) return;
      const error = __zlib.handleParams(this._id, level, strategy);
      if (error) this._fail(error);
    }
  }

  class BrotliHandle extends CompressionHandle {
    init(params, writeState, processCallback) {
      this._writeState = writeState;
      this._processCallback = processCallback;
      const unset = 0xffffffff;
      const pairs = [];
      for (let i = 0; i < params.length; i++) {
        if (params[i] !== unset) pairs.push(i, params[i]);
      }
      if (this._mode === BROTLI_ENCODE) {
        // BrotliEncoderSetParameter accepts any value, except a non-boolean literal context
        // modeling flag.
        const disableContext = params[constants.BROTLI_PARAM_DISABLE_LITERAL_CONTEXT_MODELING];
        if (disableContext !== unset && disableContext > 1) return false;
        this._open(new Uint32Array(pairs));
      } else {
        // The decoder knows only its two parameters.
        for (let i = 2; i < params.length; i++) if (params[i] !== unset) return false;
        this._open();
      }
      return true;
    }
  }

  // Zstd streams buffer their input and (de)compress it whole: a compressor emits one frame per
  // flush and at the end; a decompressor decodes at the end.
  class ZstdHandle extends CompressionHandle {
    init(writeState, processCallback) {
      this._writeState = writeState;
      this._processCallback = processCallback;
      this._chunks = [];
      this._id = 1;
      this._wroteFrame = false;
      this._out = kNoBytes;
      this._outPos = 0;
      this._ended = false;
    }

    _write(flush, input, inOff, inLen, out, outOff, outLen) {
      let availOut = outLen;
      let availIn = inLen;
      const continuing = this._outPos < this._out.length;
      availOut -= this._drain(out, outOff, availOut);
      if (availOut > 0 && !(continuing && inLen === 0)) {
        const chunk = inLen ? asBytes(input).subarray(inOff, inOff + inLen) : kNoBytes;
        const result = this._codec(flush, chunk);
        if (typeof result[0] === "string") return result;
        availIn = inLen - result[1];
        this._ended = result[2];
        this._out = result[0];
        this._outPos = 0;
        availOut -= this._drain(out, outOff + (outLen - availOut), availOut);
      }
      if (availOut > 0 && !this._ended && flush === ZSTD_e_end && this._mode === ZSTD_DECOMPRESS) {
        return ["unexpected end of file", constants.Z_BUF_ERROR, "Z_BUF_ERROR"];
      }
      this._writeState[0] = availOut;
      this._writeState[1] = availIn;
      return null;
    }

    _drain(out, outOff, room) {
      const n = Math.min(room, this._out.length - this._outPos);
      if (n > 0) {
        out.set(this._out.subarray(this._outPos, this._outPos + n), outOff);
        this._outPos += n;
      }
      return n;
    }

    _codec(flush, chunk) {
      if (this._ended) return [kNoBytes, 0, true];
      if (chunk.length) this._chunks.push(Buffer.from(chunk));
      const compress = this._mode === ZSTD_COMPRESS;
      if (flush === ZSTD_e_continue || (flush === ZSTD_e_flush && !compress)) {
        return [kNoBytes, chunk.length, false];
      }
      const input = Buffer.concat(this._chunks);
      this._chunks = [];
      const end = flush === ZSTD_e_end;
      try {
        if (compress) {
          if (!input.length && (this._wroteFrame || !end)) return [kNoBytes, chunk.length, end];
          this._wroteFrame = true;
          return [__zlib.zstdCompress(input), chunk.length, end];
        }
        if (!input.length) return [kNoBytes, chunk.length, false];
        return [__zlib.zstdDecompress(input), chunk.length, true];
      } catch (e) {
        return [String(e && e.message), constants.ZSTD_error_corruption_detected, "ZSTD_error_corruption_detected"];
      }
    }

    reset() {
      this._chunks = [];
      this._wroteFrame = false;
      this._out = kNoBytes;
      this._outPos = 0;
      this._ended = false;
    }

    close() {
      if (this.writeInProgress) {
        this._pendingClose = true;
        return;
      }
      this._pendingClose = false;
      this._id = 0;
      this._chunks = [];
      this._out = kNoBytes;
    }
  }

  const binding = {
    Zlib: ZlibHandle,
    BrotliEncoder: BrotliHandle,
    BrotliDecoder: BrotliHandle,
    ZstdCompress: ZstdHandle,
    ZstdDecompress: ZstdHandle,
  };

  // ---- lib/zlib.js ----------------------------------------------------------------------------

  const kFlushFlag = Symbol("kFlushFlag");
  const kError = Symbol("kError");

  // Translation table for return codes.
  const codes = {
    Z_OK: constants.Z_OK,
    Z_STREAM_END: constants.Z_STREAM_END,
    Z_NEED_DICT: constants.Z_NEED_DICT,
    Z_ERRNO: constants.Z_ERRNO,
    Z_STREAM_ERROR: constants.Z_STREAM_ERROR,
    Z_DATA_ERROR: constants.Z_DATA_ERROR,
    Z_MEM_ERROR: constants.Z_MEM_ERROR,
    Z_BUF_ERROR: constants.Z_BUF_ERROR,
    Z_VERSION_ERROR: constants.Z_VERSION_ERROR,
  };

  for (const ckey of Object.keys(codes)) {
    codes[codes[ckey]] = ckey;
  }

  function zlibBuffer(engine, buffer, callback) {
    validateFunction(callback, "callback");
    // Streams do not support non-Uint8Array ArrayBufferViews yet. Convert it to a
    // Buffer without copying.
    if (isArrayBufferView(buffer) && !isUint8Array(buffer)) {
      buffer = Buffer.from(buffer.buffer, buffer.byteOffset, buffer.byteLength);
    } else if (isAnyArrayBuffer(buffer)) {
      buffer = Buffer.from(buffer);
    }
    engine.buffers = null;
    engine.nread = 0;
    engine.cb = callback;
    engine.on("data", zlibBufferOnData);
    engine.on("error", zlibBufferOnError);
    engine.on("end", zlibBufferOnEnd);
    engine.end(buffer);
  }

  function zlibBufferOnData(chunk) {
    if (!this.buffers)
      this.buffers = [chunk];
    else
      this.buffers.push(chunk);
    this.nread += chunk.length;
    if (this.nread > this._maxOutputLength) {
      this.close();
      this.removeAllListeners("end");
      this.cb(ERR_BUFFER_TOO_LARGE(this._maxOutputLength));
    }
  }

  function zlibBufferOnError(err) {
    this.removeAllListeners("end");
    this.cb(err);
  }

  function zlibBufferOnEnd() {
    let buf;
    if (this.nread === 0) {
      buf = Buffer.alloc(0);
    } else {
      const bufs = this.buffers;
      buf = (bufs.length === 1 ? bufs[0] : Buffer.concat(bufs, this.nread));
    }
    this.close();
    if (this._info)
      this.cb(null, { buffer: buf, engine: this });
    else
      this.cb(null, buf);
  }

  function zlibBufferSync(engine, buffer) {
    if (typeof buffer === "string") {
      buffer = Buffer.from(buffer);
    } else if (!isArrayBufferView(buffer)) {
      if (isAnyArrayBuffer(buffer)) {
        buffer = Buffer.from(buffer);
      } else {
        throw new ERR_INVALID_ARG_TYPE(
          "buffer",
          ["string", "Buffer", "TypedArray", "DataView", "ArrayBuffer"],
          buffer,
        );
      }
    }
    buffer = processChunkSync(engine, buffer, engine._finishFlushFlag);
    if (engine._info)
      return { buffer, engine };
    return buffer;
  }

  function zlibOnError(message, errno, code) {
    const self = this[owner_symbol];
    // There is no way to cleanly recover.
    // Continuing only obscures problems.

    const error = genericNodeError(message, { errno, code });
    error.errno = errno;
    error.code = code;
    self.destroy(error);
    self[kError] = error;
  }

  // 1. Returns false for undefined and NaN
  // 2. Returns true for finite numbers
  // 3. Throws ERR_INVALID_ARG_TYPE for non-numbers
  // 4. Throws ERR_OUT_OF_RANGE for infinite numbers
  const checkFiniteNumber = (number, name) => {
    // Common case
    if (number === undefined) {
      return false;
    }

    if (Number.isFinite(number)) {
      return true; // Is a valid number
    }

    if (Number.isNaN(number)) {
      return false;
    }

    validateNumber(number, name);

    // Infinite numbers
    throw new ERR_OUT_OF_RANGE(name, "a finite number", number);
  };

  // 1. Returns def for number when it's undefined or NaN
  // 2. Returns number for finite numbers >= lower and <= upper
  // 3. Throws ERR_INVALID_ARG_TYPE for non-numbers
  // 4. Throws ERR_OUT_OF_RANGE for infinite numbers or numbers > upper or < lower
  const checkRangesOrGetDefault = (number, name, lower, upper, def) => {
    if (!checkFiniteNumber(number, name)) {
      return def;
    }
    if (number < lower || number > upper) {
      throw new ERR_OUT_OF_RANGE(name,
                                 `>= ${lower} and <= ${upper}`, number);
    }
    return number;
  };

  const FLUSH_BOUND = [
    [Z_NO_FLUSH, Z_BLOCK],
    [BROTLI_OPERATION_PROCESS, BROTLI_OPERATION_EMIT_METADATA],
    [ZSTD_e_continue, ZSTD_e_end],
  ];
  const FLUSH_BOUND_IDX_NORMAL = 0;
  const FLUSH_BOUND_IDX_BROTLI = 1;
  const FLUSH_BOUND_IDX_ZSTD = 2;

  // The base class for all Zlib-style streams.
  function ZlibBase(opts, mode, handle, { flush, finishFlush, fullFlush }) {
    let chunkSize = Z_DEFAULT_CHUNK;
    let maxOutputLength = kMaxLength;
    // The ZlibBase class is not exported to user land, the mode should only be
    // passed in by us.
    assert(typeof mode === "number");
    assert(mode >= DEFLATE && mode <= ZSTD_DECOMPRESS);

    let flushBoundIdx;
    if (mode === BROTLI_ENCODE || mode === BROTLI_DECODE) {
      flushBoundIdx = FLUSH_BOUND_IDX_BROTLI;
    } else if (mode === ZSTD_COMPRESS || mode === ZSTD_DECOMPRESS) {
      flushBoundIdx = FLUSH_BOUND_IDX_ZSTD;
    } else {
      flushBoundIdx = FLUSH_BOUND_IDX_NORMAL;
    }

    if (opts) {
      chunkSize = opts.chunkSize;
      if (!checkFiniteNumber(chunkSize, "options.chunkSize")) {
        chunkSize = Z_DEFAULT_CHUNK;
      } else if (chunkSize < Z_MIN_CHUNK) {
        throw new ERR_OUT_OF_RANGE("options.chunkSize",
                                   `>= ${Z_MIN_CHUNK}`, chunkSize);
      }

      flush = checkRangesOrGetDefault(
        opts.flush, "options.flush",
        FLUSH_BOUND[flushBoundIdx][0], FLUSH_BOUND[flushBoundIdx][1], flush);

      finishFlush = checkRangesOrGetDefault(
        opts.finishFlush, "options.finishFlush",
        FLUSH_BOUND[flushBoundIdx][0], FLUSH_BOUND[flushBoundIdx][1],
        finishFlush);

      maxOutputLength = checkRangesOrGetDefault(
        opts.maxOutputLength, "options.maxOutputLength",
        1, kMaxLength, kMaxLength);

      if (opts.encoding || opts.objectMode || opts.writableObjectMode) {
        opts = { ...opts };
        opts.encoding = null;
        opts.objectMode = false;
        opts.writableObjectMode = false;
      }
    }

    Reflect.apply(Transform, this, [{ autoDestroy: true, ...opts }]);
    this[kError] = null;
    this.bytesWritten = 0;
    this._handle = handle;
    handle[owner_symbol] = this;
    // Used by processCallback() and zlibOnError()
    handle.onerror = zlibOnError;
    this._outBuffer = Buffer.allocUnsafe(chunkSize);
    this._outOffset = 0;

    this._chunkSize = chunkSize;
    this._defaultFlushFlag = flush;
    this._finishFlushFlag = finishFlush;
    this._defaultFullFlushFlag = fullFlush;
    this._info = opts && opts.info;
    this._maxOutputLength = maxOutputLength;
  }
  Object.setPrototypeOf(ZlibBase.prototype, Transform.prototype);
  Object.setPrototypeOf(ZlibBase, Transform);

  Object.defineProperty(ZlibBase.prototype, "_closed", {
    __proto__: null,
    configurable: true,
    enumerable: true,
    get() {
      return !this._handle;
    },
  });

  // `bytesRead` made sense as a name when looking from the zlib engine's
  // perspective, but it is inconsistent with all other streams exposed by Node.js
  // that have this concept, where it stands for the number of bytes read
  // *from* the stream (that is, net.Socket/tls.Socket & file system streams).
  Object.defineProperty(ZlibBase.prototype, "bytesRead", {
    __proto__: null,
    configurable: true,
    enumerable: true,
    get: util.deprecate(function() {
      return this.bytesWritten;
    }, "zlib.bytesRead is deprecated and will change its meaning in the " +
       "future. Use zlib.bytesWritten instead.", "DEP0108"),
    set: util.deprecate(function(value) {
      this.bytesWritten = value;
    }, "Setting zlib.bytesRead is deprecated. " +
       "This feature will be removed in the future.", "DEP0108"),
  });

  ZlibBase.prototype.reset = function() {
    if (!this._handle)
      assert(false, "zlib binding closed");
    return this._handle.reset();
  };

  // This is the _flush function called by the transform class,
  // internally, when the last chunk has been written.
  ZlibBase.prototype._flush = function(callback) {
    this._transform(Buffer.alloc(0), "", callback);
  };

  // Force Transform compat behavior.
  ZlibBase.prototype._final = function(callback) {
    callback();
  };

  // If a flush is scheduled while another flush is still pending, a way to figure
  // out which one is the "stronger" flush is needed.
  // This is currently only used to figure out which flush flag to use for the
  // last chunk.
  // Roughly, the following holds:
  // Z_NO_FLUSH (< Z_TREES) < Z_BLOCK < Z_PARTIAL_FLUSH <
  //     Z_SYNC_FLUSH < Z_FULL_FLUSH < Z_FINISH
  const flushiness = [];
  let i = 0;
  const kFlushFlagList = [Z_NO_FLUSH, Z_BLOCK, Z_PARTIAL_FLUSH,
                          Z_SYNC_FLUSH, Z_FULL_FLUSH, Z_FINISH];
  for (const flushFlag of kFlushFlagList) {
    flushiness[flushFlag] = i++;
  }

  function maxFlush(a, b) {
    return flushiness[a] > flushiness[b] ? a : b;
  }

  // Set up a list of 'special' buffers that can be written using .write()
  // from the .flush() code as a way of introducing flushing operations into the
  // write sequence.
  const kFlushBuffers = [];
  {
    const dummyArrayBuffer = new ArrayBuffer();
    for (const flushFlag of kFlushFlagList) {
      kFlushBuffers[flushFlag] = Buffer.from(dummyArrayBuffer);
      kFlushBuffers[flushFlag][kFlushFlag] = flushFlag;
    }
  }

  ZlibBase.prototype.flush = function(kind, callback) {
    if (typeof kind === "function" || (kind === undefined && !callback)) {
      callback = kind;
      kind = this._defaultFullFlushFlag;
    }

    if (this.writableFinished) {
      if (callback)
        process.nextTick(callback);
    } else if (this.writableEnded) {
      if (callback)
        this.once("end", callback);
    } else {
      this.write(kFlushBuffers[kind], "", callback);
    }
  };

  ZlibBase.prototype.close = function(callback) {
    if (callback) finished(this, callback);
    this.destroy();
  };

  ZlibBase.prototype._destroy = function(err, callback) {
    _close(this);
    callback(err);
  };

  ZlibBase.prototype._transform = function(chunk, encoding, cb) {
    let flushFlag = this._defaultFlushFlag;
    // We use a 'fake' zero-length chunk to carry information about flushes from
    // the public API to the actual stream implementation.
    if (typeof chunk[kFlushFlag] === "number") {
      flushFlag = chunk[kFlushFlag];
    }

    // For the last chunk, also apply `_finishFlushFlag`.
    if (this.writableEnded && this.writableLength === chunk.byteLength) {
      flushFlag = maxFlush(flushFlag, this._finishFlushFlag);
    }
    processChunk(this, chunk, flushFlag, cb);
  };

  ZlibBase.prototype._processChunk = function(chunk, flushFlag, cb) {
    // _processChunk() is left for backwards compatibility
    if (typeof cb === "function")
      processChunk(this, chunk, flushFlag, cb);
    else
      return processChunkSync(this, chunk, flushFlag);
  };

  function processChunkSync(self, chunk, flushFlag) {
    let availInBefore = chunk.byteLength;
    let availOutBefore = self._chunkSize - self._outOffset;
    let inOff = 0;
    let availOutAfter;
    let availInAfter;

    let buffers = null;
    let nread = 0;
    let inputRead = 0;
    const state = self._writeState;
    const handle = self._handle;
    let buffer = self._outBuffer;
    let offset = self._outOffset;
    const chunkSize = self._chunkSize;

    let error;
    self.on("error", function onError(er) {
      error = er;
    });

    while (true) {
      handle.writeSync(flushFlag,
                       chunk, // in
                       inOff, // in_off
                       availInBefore, // in_len
                       buffer, // out
                       offset, // out_off
                       availOutBefore); // out_len
      if (error)
        throw error;
      else if (self[kError])
        throw self[kError];

      availOutAfter = state[0];
      availInAfter = state[1];

      const inDelta = (availInBefore - availInAfter);
      inputRead += inDelta;

      const have = availOutBefore - availOutAfter;
      if (have > 0) {
        const out = buffer.slice(offset, offset + have);
        offset += have;
        if (!buffers)
          buffers = [out];
        else
          buffers.push(out);
        nread += out.byteLength;

        if (nread > self._maxOutputLength) {
          _close(self);
          throw ERR_BUFFER_TOO_LARGE(self._maxOutputLength);
        }

      } else {
        assert(have === 0, "have should not go down");
      }

      // Exhausted the output buffer, or used all the input create a new one.
      if (availOutAfter === 0 || offset >= chunkSize) {
        availOutBefore = chunkSize;
        offset = 0;
        buffer = Buffer.allocUnsafe(chunkSize);
      }

      if (availOutAfter === 0) {
        // Not actually done. Need to reprocess.
        // Also, update the availInBefore to the availInAfter value,
        // so that if we have to hit it a third (fourth, etc.) time,
        // it'll have the correct byte counts.
        inOff += inDelta;
        availInBefore = availInAfter;
      } else {
        break;
      }
    }

    self.bytesWritten = inputRead;
    _close(self);

    if (nread === 0)
      return Buffer.alloc(0);

    return (buffers.length === 1 ? buffers[0] : Buffer.concat(buffers, nread));
  }

  function processChunk(self, chunk, flushFlag, cb) {
    const handle = self._handle;
    if (!handle) return process.nextTick(cb);

    handle.buffer = chunk;
    handle.cb = cb;
    handle.availOutBefore = self._chunkSize - self._outOffset;
    handle.availInBefore = chunk.byteLength;
    handle.inOff = 0;
    handle.flushFlag = flushFlag;

    handle.write(flushFlag,
                 chunk, // in
                 0, // in_off
                 handle.availInBefore, // in_len
                 self._outBuffer, // out
                 self._outOffset, // out_off
                 handle.availOutBefore); // out_len
  }

  function processCallback() {
    // This callback's context (`this`) is the `_handle` (ZCtx) object. It is
    // important to null out the values once they are no longer needed since
    // `_handle` can stay in memory long after the buffer is needed.
    const handle = this;
    const self = this[owner_symbol];
    const state = self._writeState;

    if (self.destroyed) {
      this.buffer = null;
      this.cb();
      return;
    }

    const availOutAfter = state[0];
    const availInAfter = state[1];

    const inDelta = handle.availInBefore - availInAfter;
    self.bytesWritten += inDelta;

    const have = handle.availOutBefore - availOutAfter;
    if (have > 0) {
      const out = self._outBuffer.slice(self._outOffset, self._outOffset + have);
      self._outOffset += have;
      self.push(out);
    } else {
      assert(have === 0, "have should not go down");
    }

    if (self.destroyed) {
      this.cb();
      return;
    }

    // Exhausted the output buffer, or used all the input create a new one.
    if (availOutAfter === 0 || self._outOffset >= self._chunkSize) {
      handle.availOutBefore = self._chunkSize;
      self._outOffset = 0;
      self._outBuffer = Buffer.allocUnsafe(self._chunkSize);
    }

    if (availOutAfter === 0) {
      // Not actually done. Need to reprocess.
      // Also, update the availInBefore to the availInAfter value,
      // so that if we have to hit it a third (fourth, etc.) time,
      // it'll have the correct byte counts.
      handle.inOff += inDelta;
      handle.availInBefore = availInAfter;

      this.write(handle.flushFlag,
                 this.buffer, // in
                 handle.inOff, // in_off
                 handle.availInBefore, // in_len
                 self._outBuffer, // out
                 self._outOffset, // out_off
                 self._chunkSize); // out_len
      return;
    }

    if (availInAfter > 0) {
      // If we have more input that should be written, but we also have output
      // space available, that means that the compression library was not
      // interested in receiving more data, and in particular that the input
      // stream has ended early.
      // This applies to streams where we don't check data past the end of
      // what was consumed; that is, everything except Gunzip/Unzip.
      self.push(null);
    }

    // Finished with the chunk.
    this.buffer = null;
    this.cb();
  }

  function _close(engine) {
    // Caller may invoke .close after a zlib error (which will null _handle).
    if (!engine._handle)
      return;

    engine._handle.close();
    engine._handle = null;
  }

  const zlibDefaultOpts = {
    flush: Z_NO_FLUSH,
    finishFlush: Z_FINISH,
    fullFlush: Z_FULL_FLUSH,
  };
  // Base class for all streams actually backed by zlib and using zlib-specific
  // parameters.
  function Zlib(opts, mode) {
    let windowBits = Z_DEFAULT_WINDOWBITS;
    let level = Z_DEFAULT_COMPRESSION;
    let memLevel = Z_DEFAULT_MEMLEVEL;
    let strategy = Z_DEFAULT_STRATEGY;
    let dictionary;

    if (opts) {
      // windowBits is special. On the compression side, 0 is an invalid value.
      // But on the decompression side, a value of 0 for windowBits tells zlib
      // to use the window size in the zlib header of the compressed stream.
      if ((opts.windowBits == null || opts.windowBits === 0) &&
          (mode === INFLATE ||
           mode === GUNZIP ||
           mode === UNZIP)) {
        windowBits = 0;
      } else {
        // `{ windowBits: 8 }` is valid for deflate but not gzip.
        const min = Z_MIN_WINDOWBITS + (mode === GZIP ? 1 : 0);
        windowBits = checkRangesOrGetDefault(
          opts.windowBits, "options.windowBits",
          min, Z_MAX_WINDOWBITS, Z_DEFAULT_WINDOWBITS);
      }

      level = checkRangesOrGetDefault(
        opts.level, "options.level",
        Z_MIN_LEVEL, Z_MAX_LEVEL, Z_DEFAULT_COMPRESSION);

      memLevel = checkRangesOrGetDefault(
        opts.memLevel, "options.memLevel",
        Z_MIN_MEMLEVEL, Z_MAX_MEMLEVEL, Z_DEFAULT_MEMLEVEL);

      strategy = checkRangesOrGetDefault(
        opts.strategy, "options.strategy",
        Z_DEFAULT_STRATEGY, Z_FIXED, Z_DEFAULT_STRATEGY);

      dictionary = opts.dictionary;
      if (dictionary !== undefined && !isArrayBufferView(dictionary)) {
        if (isAnyArrayBuffer(dictionary)) {
          dictionary = Buffer.from(dictionary);
        } else {
          throw new ERR_INVALID_ARG_TYPE(
            "options.dictionary",
            ["Buffer", "TypedArray", "DataView", "ArrayBuffer"],
            dictionary,
          );
        }
      }
    }

    const handle = new binding.Zlib(mode);
    // Ideally, we could let ZlibBase() set up _writeState. I haven't been able
    // to come up with a good solution that doesn't break our internal API,
    // and with it all supported npm versions at the time of writing.
    this._writeState = new Uint32Array(2);
    handle.init(windowBits,
                level,
                memLevel,
                strategy,
                this._writeState,
                processCallback,
                dictionary);

    Reflect.apply(ZlibBase, this, [opts, mode, handle, zlibDefaultOpts]);

    this._level = level;
    this._strategy = strategy;
  }
  Object.setPrototypeOf(Zlib.prototype, ZlibBase.prototype);
  Object.setPrototypeOf(Zlib, ZlibBase);

  // This callback is used by `.params()` to wait until a full flush happened
  // before adjusting the parameters. In particular, the call to the native
  // `params()` function should not happen while a write is currently in progress
  // on the threadpool.
  function paramsAfterFlushCallback(level, strategy, callback) {
    assert(this._handle, "zlib binding closed");
    this._handle.params(level, strategy);
    if (!this.destroyed) {
      this._level = level;
      this._strategy = strategy;
      if (callback) callback();
    }
  }

  Zlib.prototype.params = function params(level, strategy, callback) {
    checkRangesOrGetDefault(level, "level", Z_MIN_LEVEL, Z_MAX_LEVEL);
    checkRangesOrGetDefault(strategy, "strategy", Z_DEFAULT_STRATEGY, Z_FIXED);

    if (this._level !== level || this._strategy !== strategy) {
      this.flush(Z_SYNC_FLUSH,
                 paramsAfterFlushCallback.bind(this, level, strategy, callback));
    } else {
      process.nextTick(callback);
    }
  };

  // generic zlib
  // minimal 2-byte header
  function Deflate(opts) {
    if (!(this instanceof Deflate))
      return new Deflate(opts);
    Reflect.apply(Zlib, this, [opts, DEFLATE]);
  }
  Object.setPrototypeOf(Deflate.prototype, Zlib.prototype);
  Object.setPrototypeOf(Deflate, Zlib);

  function Inflate(opts) {
    if (!(this instanceof Inflate))
      return new Inflate(opts);
    Reflect.apply(Zlib, this, [opts, INFLATE]);
  }
  Object.setPrototypeOf(Inflate.prototype, Zlib.prototype);
  Object.setPrototypeOf(Inflate, Zlib);

  function Gzip(opts) {
    if (!(this instanceof Gzip))
      return new Gzip(opts);
    Reflect.apply(Zlib, this, [opts, GZIP]);
  }
  Object.setPrototypeOf(Gzip.prototype, Zlib.prototype);
  Object.setPrototypeOf(Gzip, Zlib);

  function Gunzip(opts) {
    if (!(this instanceof Gunzip))
      return new Gunzip(opts);
    Reflect.apply(Zlib, this, [opts, GUNZIP]);
  }
  Object.setPrototypeOf(Gunzip.prototype, Zlib.prototype);
  Object.setPrototypeOf(Gunzip, Zlib);

  function DeflateRaw(opts) {
    if (opts && opts.windowBits === 8) opts.windowBits = 9;
    if (!(this instanceof DeflateRaw))
      return new DeflateRaw(opts);
    Reflect.apply(Zlib, this, [opts, DEFLATERAW]);
  }
  Object.setPrototypeOf(DeflateRaw.prototype, Zlib.prototype);
  Object.setPrototypeOf(DeflateRaw, Zlib);

  function InflateRaw(opts) {
    if (!(this instanceof InflateRaw))
      return new InflateRaw(opts);
    Reflect.apply(Zlib, this, [opts, INFLATERAW]);
  }
  Object.setPrototypeOf(InflateRaw.prototype, Zlib.prototype);
  Object.setPrototypeOf(InflateRaw, Zlib);

  function Unzip(opts) {
    if (!(this instanceof Unzip))
      return new Unzip(opts);
    Reflect.apply(Zlib, this, [opts, UNZIP]);
  }
  Object.setPrototypeOf(Unzip.prototype, Zlib.prototype);
  Object.setPrototypeOf(Unzip, Zlib);

  function createConvenienceMethod(ctor, sync) {
    if (sync) {
      return function syncBufferWrapper(buffer, opts) {
        return zlibBufferSync(new ctor(opts), buffer);
      };
    }
    return function asyncBufferWrapper(buffer, opts, callback) {
      if (typeof opts === "function") {
        callback = opts;
        opts = {};
      }
      return zlibBuffer(new ctor(opts), buffer, callback);
    };
  }

  const kMaxBrotliParam = Math.max(...Object.keys(constants).map(
    (key) => (key.startsWith("BROTLI_PARAM_") ? constants[key] : 0),
  ));

  const brotliInitParamsArray = new Uint32Array(kMaxBrotliParam + 1);

  const brotliDefaultOpts = {
    flush: BROTLI_OPERATION_PROCESS,
    finishFlush: BROTLI_OPERATION_FINISH,
    fullFlush: BROTLI_OPERATION_FLUSH,
  };
  function Brotli(opts, mode) {
    assert(mode === BROTLI_DECODE || mode === BROTLI_ENCODE);

    brotliInitParamsArray.fill(-1);
    if (opts?.params) {
      Object.keys(opts.params).forEach((origKey) => {
        const key = +origKey;
        if (Number.isNaN(key) || key < 0 || key > kMaxBrotliParam ||
            (brotliInitParamsArray[key] | 0) !== -1) {
          throw ERR_BROTLI_INVALID_PARAM(origKey);
        }

        const value = opts.params[origKey];
        if (typeof value !== "number" && typeof value !== "boolean") {
          throw new ERR_INVALID_ARG_TYPE("options.params[key]",
                                         "number", opts.params[origKey]);
        }
        brotliInitParamsArray[key] = value;
      });
    }

    const handle = mode === BROTLI_DECODE ?
      new binding.BrotliDecoder(mode) : new binding.BrotliEncoder(mode);

    this._writeState = new Uint32Array(2);
    if (!handle.init(brotliInitParamsArray,
                     this._writeState,
                     processCallback)) {
      throw ERR_ZLIB_INITIALIZATION_FAILED();
    }

    Reflect.apply(ZlibBase, this, [opts, mode, handle, brotliDefaultOpts]);
  }
  Object.setPrototypeOf(Brotli.prototype, Zlib.prototype);
  Object.setPrototypeOf(Brotli, Zlib);

  function BrotliCompress(opts) {
    if (!(this instanceof BrotliCompress))
      return new BrotliCompress(opts);
    Reflect.apply(Brotli, this, [opts, BROTLI_ENCODE]);
  }
  Object.setPrototypeOf(BrotliCompress.prototype, Brotli.prototype);
  Object.setPrototypeOf(BrotliCompress, Brotli);

  function BrotliDecompress(opts) {
    if (!(this instanceof BrotliDecompress))
      return new BrotliDecompress(opts);
    Reflect.apply(Brotli, this, [opts, BROTLI_DECODE]);
  }
  Object.setPrototypeOf(BrotliDecompress.prototype, Brotli.prototype);
  Object.setPrototypeOf(BrotliDecompress, Brotli);

  // Zstd streams as Node 22 shapes them (ZSTD_e_* flush values; `params` keyed by ZSTD_c_*/d_*).
  const zstdDefaultOpts = {
    flush: ZSTD_e_continue,
    finishFlush: ZSTD_e_end,
    fullFlush: ZSTD_e_flush,
  };
  function Zstd(opts, mode) {
    if (opts?.params !== undefined && (opts.params === null || typeof opts.params !== "object")) {
      throw new ERR_INVALID_ARG_TYPE("options.params", "Object", opts.params);
    }
    const handle = mode === ZSTD_COMPRESS ?
      new binding.ZstdCompress(mode) : new binding.ZstdDecompress(mode);
    this._writeState = new Uint32Array(2);
    handle.init(this._writeState, processCallback);
    Reflect.apply(ZlibBase, this, [opts, mode, handle, zstdDefaultOpts]);
  }
  Object.setPrototypeOf(Zstd.prototype, ZlibBase.prototype);
  Object.setPrototypeOf(Zstd, ZlibBase);

  function ZstdCompress(opts) {
    if (!(this instanceof ZstdCompress))
      return new ZstdCompress(opts);
    Reflect.apply(Zstd, this, [opts, ZSTD_COMPRESS]);
  }
  Object.setPrototypeOf(ZstdCompress.prototype, Zstd.prototype);
  Object.setPrototypeOf(ZstdCompress, Zstd);

  function ZstdDecompress(opts) {
    if (!(this instanceof ZstdDecompress))
      return new ZstdDecompress(opts);
    Reflect.apply(Zstd, this, [opts, ZSTD_DECOMPRESS]);
  }
  Object.setPrototypeOf(ZstdDecompress.prototype, Zstd.prototype);
  Object.setPrototypeOf(ZstdDecompress, Zstd);

  function createProperty(ctor) {
    return {
      __proto__: null,
      configurable: true,
      enumerable: true,
      value: function(options) {
        return new ctor(options);
      },
    };
  }

  // Legacy alias on the C++ wrapper object. This is not public API, so we may
  // want to runtime-deprecate it at some point. There's no hurry, though.
  Object.defineProperty(binding.Zlib.prototype, "jsref", {
    __proto__: null,
    get() { return this[owner_symbol]; },
    set(v) { return this[owner_symbol] = v; },
  });

  // zlib.crc32(data[, value]): CRC-32 of a string (UTF-8) or bytes, continued from `value`.
  function crc32(data, value = 0) {
    if (typeof data !== "string" && !isArrayBufferView(data)) {
      throw new ERR_INVALID_ARG_TYPE("data", ["Buffer", "TypedArray", "DataView", "string"], data);
    }
    __validators.validateUint32(value, "value");
    const bytes = typeof data === "string" ? Buffer.from(data)
      : new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    return __zlib.crc32(bytes, value);
  }

  const zlib = {
    crc32,

    Deflate,
    Inflate,
    Gzip,
    Gunzip,
    DeflateRaw,
    InflateRaw,
    Unzip,
    BrotliCompress,
    BrotliDecompress,
    ZstdCompress,
    ZstdDecompress,

    // Convenience methods.
    // compress/decompress a string or buffer in one step.
    deflate: createConvenienceMethod(Deflate, false),
    deflateSync: createConvenienceMethod(Deflate, true),
    gzip: createConvenienceMethod(Gzip, false),
    gzipSync: createConvenienceMethod(Gzip, true),
    deflateRaw: createConvenienceMethod(DeflateRaw, false),
    deflateRawSync: createConvenienceMethod(DeflateRaw, true),
    unzip: createConvenienceMethod(Unzip, false),
    unzipSync: createConvenienceMethod(Unzip, true),
    inflate: createConvenienceMethod(Inflate, false),
    inflateSync: createConvenienceMethod(Inflate, true),
    gunzip: createConvenienceMethod(Gunzip, false),
    gunzipSync: createConvenienceMethod(Gunzip, true),
    inflateRaw: createConvenienceMethod(InflateRaw, false),
    inflateRawSync: createConvenienceMethod(InflateRaw, true),
    brotliCompress: createConvenienceMethod(BrotliCompress, false),
    brotliCompressSync: createConvenienceMethod(BrotliCompress, true),
    brotliDecompress: createConvenienceMethod(BrotliDecompress, false),
    brotliDecompressSync: createConvenienceMethod(BrotliDecompress, true),
    zstdCompress: createConvenienceMethod(ZstdCompress, false),
    zstdCompressSync: createConvenienceMethod(ZstdCompress, true),
    zstdDecompress: createConvenienceMethod(ZstdDecompress, false),
    zstdDecompressSync: createConvenienceMethod(ZstdDecompress, true),
  };

  Object.defineProperties(zlib, {
    createDeflate: createProperty(Deflate),
    createInflate: createProperty(Inflate),
    createDeflateRaw: createProperty(DeflateRaw),
    createInflateRaw: createProperty(InflateRaw),
    createGzip: createProperty(Gzip),
    createGunzip: createProperty(Gunzip),
    createUnzip: createProperty(Unzip),
    createBrotliCompress: createProperty(BrotliCompress),
    createBrotliDecompress: createProperty(BrotliDecompress),
    createZstdCompress: createProperty(ZstdCompress),
    createZstdDecompress: createProperty(ZstdDecompress),
    constants: {
      __proto__: null,
      configurable: false,
      enumerable: true,
      value: constants,
    },
    codes: {
      __proto__: null,
      enumerable: true,
      writable: false,
      value: Object.freeze(codes),
    },
  });

  // These should be considered deprecated
  // expose all the zlib constants
  for (const bkey of Object.keys(constants)) {
    if (bkey.startsWith("BROTLI") || bkey.startsWith("ZSTD")) continue;
    Object.defineProperty(zlib, bkey, {
      __proto__: null,
      enumerable: false, value: constants[bkey], writable: false,
    });
  }

  __builtins.set("zlib", zlib);
}
