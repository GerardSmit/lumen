// node:stream — a port of Node 20's lib/internal/streams/* (legacy, readable, writable, duplex,
// transform, passthrough, destroy, end-of-stream, pipeline, compose, operators, from,
// add-abort-signal, state, utils, duplexify) plus the webstreams adapters. The algorithms, state
// flags, event ordering and error codes follow Node's; the state objects keep plain boolean
// fields where Node packs a bitfield, so `state.ended`, `state.destroyed`, ... read and write
// the same way.

const EE = __builtins.get("events");
const { StringDecoder } = __builtins.get("string_decoder");
const {
  ERR_INVALID_ARG_TYPE, ERR_INVALID_ARG_VALUE, ERR_INVALID_RETURN_VALUE, ERR_MISSING_ARGS,
  ERR_METHOD_NOT_IMPLEMENTED, ERR_MULTIPLE_CALLBACK, ERR_OUT_OF_RANGE, ERR_STREAM_ALREADY_FINISHED,
  ERR_STREAM_CANNOT_PIPE, ERR_STREAM_DESTROYED, ERR_STREAM_NULL_VALUES, ERR_STREAM_PREMATURE_CLOSE,
  ERR_STREAM_PUSH_AFTER_EOF, ERR_STREAM_UNSHIFT_AFTER_END_EVENT, ERR_STREAM_WRITE_AFTER_END,
  ERR_UNKNOWN_ENCODING, ERR_INVALID_STATE,
} = __errors;
const {
  validateAbortSignal, validateBoolean, validateFunction, validateInteger, validateObject,
} = __validators;

const kEmptyObject = Object.freeze({ __proto__: null });
const SymbolDispose = Symbol.dispose ?? Symbol.for("nodejs.dispose");
const SymbolAsyncDispose = Symbol.asyncDispose ?? Symbol.for("nodejs.asyncDispose");
const AsyncIteratorPrototype = Object.getPrototypeOf(Object.getPrototypeOf(async function* () {}).prototype);
const nop = () => {};

class AbortError extends Error {
  constructor(message = "The operation was aborted", options = undefined) {
    if (options !== undefined && typeof options !== "object") {
      throw new ERR_INVALID_ARG_TYPE("options", "Object", options);
    }
    super(message, options);
    this.code = "ABORT_ERR";
    this.name = "AbortError";
  }
}

function once(callback) {
  let called = false;
  return function (...args) {
    if (called) return;
    called = true;
    return Reflect.apply(callback, this, args);
  };
}

function isUint8Array(v) {
  return ArrayBuffer.isView(v) && v instanceof Uint8Array;
}

function aggregateTwoErrors(innerError, outerError) {
  if (innerError && outerError && innerError !== outerError) {
    if (Array.isArray(outerError.errors)) {
      // If `outerError` is already an `AggregateError`.
      outerError.errors.push(innerError);
      return outerError;
    }
    const err = new AggregateError([outerError, innerError], outerError.message);
    err.code = outerError.code;
    return err;
  }
  return innerError || outerError;
}

// ---- utils (lib/internal/streams/utils.js) ----------------------------------------------------

const kDestroyed = Symbol("kDestroyed");
const kIsErrored = Symbol("kIsErrored");
const kIsReadable = Symbol("kIsReadable");
const kIsWritable = Symbol("kIsWritable");
const kIsDisturbed = Symbol("kIsDisturbed");
const kIsClosedPromise = Symbol.for("nodejs.webstream.isClosedPromise");
const kControllerErrorFunction = Symbol.for("nodejs.webstream.controllerErrorFunction");

function isReadableNodeStream(obj, strict = false) {
  return !!(
    obj &&
    typeof obj.pipe === "function" &&
    typeof obj.on === "function" &&
    (!strict || (typeof obj.pause === "function" && typeof obj.resume === "function")) &&
    (!obj._writableState || obj._readableState?.readable !== false) && // Duplex
    (!obj._writableState || obj._readableState) // Writable has .pipe.
  );
}

function isWritableNodeStream(obj) {
  return !!(
    obj &&
    typeof obj.write === "function" &&
    typeof obj.on === "function" &&
    (!obj._readableState || obj._writableState?.writable !== false) // Duplex
  );
}

function isDuplexNodeStream(obj) {
  return !!(
    obj &&
    typeof obj.pipe === "function" && obj._readableState &&
    typeof obj.on === "function" &&
    typeof obj.write === "function"
  );
}

function isNodeStream(obj) {
  return (
    obj &&
    (
      obj._readableState ||
      obj._writableState ||
      (typeof obj.write === "function" && typeof obj.on === "function") ||
      (typeof obj.pipe === "function" && typeof obj.on === "function")
    )
  );
}

function isReadableStream(obj) {
  return !!(
    obj &&
    !isNodeStream(obj) &&
    typeof obj.pipeThrough === "function" &&
    typeof obj.getReader === "function" &&
    typeof obj.cancel === "function"
  );
}

function isWritableStream(obj) {
  return !!(
    obj &&
    !isNodeStream(obj) &&
    typeof obj.getWriter === "function" &&
    typeof obj.abort === "function"
  );
}

function isTransformStream(obj) {
  return !!(
    obj &&
    !isNodeStream(obj) &&
    typeof obj.readable === "object" &&
    typeof obj.writable === "object"
  );
}

function isWebStream(obj) {
  return isReadableStream(obj) || isWritableStream(obj) || isTransformStream(obj);
}

function isIterable(obj, isAsync) {
  if (obj == null) return false;
  if (isAsync === true) return typeof obj[Symbol.asyncIterator] === "function";
  if (isAsync === false) return typeof obj[Symbol.iterator] === "function";
  return typeof obj[Symbol.asyncIterator] === "function" ||
    typeof obj[Symbol.iterator] === "function";
}

function isDestroyed(stream) {
  if (!isNodeStream(stream)) return null;
  const wState = stream._writableState;
  const rState = stream._readableState;
  const state = wState || rState;
  return !!(stream.destroyed || stream[kDestroyed] || state?.destroyed);
}

// Have been end():d.
function isWritableEnded(stream) {
  if (!isWritableNodeStream(stream)) return null;
  if (stream.writableEnded === true) return true;
  const wState = stream._writableState;
  if (wState?.errored) return false;
  if (typeof wState?.ended !== "boolean") return null;
  return wState.ended;
}

// Have emitted 'finish'.
function isWritableFinished(stream, strict) {
  if (!isWritableNodeStream(stream)) return null;
  if (stream.writableFinished === true) return true;
  const wState = stream._writableState;
  if (wState?.errored) return false;
  if (typeof wState?.finished !== "boolean") return null;
  return !!(
    wState.finished ||
    (strict === false && wState.ended === true && wState.length === 0)
  );
}

// Have been push(null):d.
function isReadableEnded(stream) {
  if (!isReadableNodeStream(stream)) return null;
  if (stream.readableEnded === true) return true;
  const rState = stream._readableState;
  if (!rState || rState.errored) return false;
  if (typeof rState?.ended !== "boolean") return null;
  return rState.ended;
}

// Have emitted 'end'.
function isReadableFinished(stream, strict) {
  if (!isReadableNodeStream(stream)) return null;
  const rState = stream._readableState;
  if (rState?.errored) return false;
  if (typeof rState?.endEmitted !== "boolean") return null;
  return !!(
    rState.endEmitted ||
    (strict === false && rState.ended === true && rState.length === 0)
  );
}

function isReadable(stream) {
  if (stream && stream[kIsReadable] != null) return stream[kIsReadable];
  if (typeof stream?.readable !== "boolean") return null;
  if (isDestroyed(stream)) return false;
  return isReadableNodeStream(stream) &&
    stream.readable &&
    !isReadableFinished(stream);
}

function isWritable(stream) {
  if (stream && stream[kIsWritable] != null) return stream[kIsWritable];
  if (typeof stream?.writable !== "boolean") return null;
  if (isDestroyed(stream)) return false;
  return isWritableNodeStream(stream) &&
    stream.writable &&
    !isWritableEnded(stream);
}

function isFinished(stream, opts) {
  if (!isNodeStream(stream)) return null;
  if (isDestroyed(stream)) return true;
  if (opts?.readable !== false && isReadable(stream)) return false;
  if (opts?.writable !== false && isWritable(stream)) return false;
  return true;
}

function isWritableErrored(stream) {
  if (!isNodeStream(stream)) return null;
  if (stream.writableErrored) return stream.writableErrored;
  return stream._writableState?.errored ?? null;
}

function isReadableErrored(stream) {
  if (!isNodeStream(stream)) return null;
  if (stream.readableErrored) return stream.readableErrored;
  return stream._readableState?.errored ?? null;
}

function isClosed(stream) {
  if (!isNodeStream(stream)) return null;
  if (typeof stream.closed === "boolean") return stream.closed;
  const wState = stream._writableState;
  const rState = stream._readableState;
  if (typeof wState?.closed === "boolean" || typeof rState?.closed === "boolean") {
    return wState?.closed || rState?.closed;
  }
  if (typeof stream._closed === "boolean" && isOutgoingMessage(stream)) return stream._closed;
  return null;
}

function isOutgoingMessage(stream) {
  return (
    typeof stream._closed === "boolean" &&
    typeof stream._defaultKeepAlive === "boolean" &&
    typeof stream._removedConnection === "boolean" &&
    typeof stream._removedContLen === "boolean"
  );
}

function isServerResponse(stream) {
  return typeof stream._sent100 === "boolean" && isOutgoingMessage(stream);
}

function isServerRequest(stream) {
  return (
    typeof stream._consuming === "boolean" &&
    typeof stream._dumped === "boolean" &&
    stream.req?.upgradeOrConnect === undefined
  );
}

function willEmitClose(stream) {
  if (!isNodeStream(stream)) return null;
  const wState = stream._writableState;
  const rState = stream._readableState;
  const state = wState || rState;
  return (!state && isServerResponse(stream)) || !!(
    state &&
    state.autoDestroy &&
    state.emitClose &&
    state.closed === false
  );
}

function isDisturbed(stream) {
  return !!(stream && (
    stream[kIsDisturbed] ??
    (stream.readableDidRead || stream.readableAborted)
  ));
}

function isErrored(stream) {
  return !!(stream && (
    stream[kIsErrored] ??
    stream.readableErrored ??
    stream.writableErrored ??
    stream._readableState?.errorEmitted ??
    stream._writableState?.errorEmitted ??
    stream._readableState?.errored ??
    stream._writableState?.errored
  ));
}

// ---- destroy (lib/internal/streams/destroy.js) ------------------------------------------------

const kDestroy = Symbol("kDestroy");
const kConstruct = Symbol("kConstruct");

function checkError(err, w, r) {
  if (err) {
    // Avoid V8 leak, https://github.com/nodejs/node/pull/34103#issuecomment-652002364
    err.stack; // eslint-disable-line no-unused-expressions

    if (w && !w.errored) w.errored = err;
    if (r && !r.errored) r.errored = err;
  }
}

// Backwards compat. cb() is undocumented and unused in core but unfortunately might be used by
// modules.
function destroyImpl(err, cb) {
  const r = this._readableState;
  const w = this._writableState;
  // With duplex streams we use the writable side for state.
  const s = w || r;

  if ((w?.destroyed) || (r?.destroyed)) {
    if (typeof cb === "function") cb();
    return this;
  }

  // We set destroyed to true before firing error callbacks in order to make it re-entrance safe
  // in case destroy() is called within callbacks
  checkError(err, w, r);

  if (w) w.destroyed = true;
  if (r) r.destroyed = true;

  // If still constructing then defer calling _destroy.
  if (!s.constructed) {
    this.once(kDestroy, function (er) {
      _destroy(this, aggregateTwoErrors(er, err), cb);
    });
  } else {
    _destroy(this, err, cb);
  }

  return this;
}

function _destroy(self, err, cb) {
  let called = false;

  function onDestroy(err) {
    if (called) return;
    called = true;

    const r = self._readableState;
    const w = self._writableState;

    checkError(err, w, r);

    if (w) w.closed = true;
    if (r) r.closed = true;

    if (typeof cb === "function") cb(err);

    if (err) {
      process.nextTick(emitErrorCloseNT, self, err);
    } else {
      process.nextTick(emitCloseNT, self);
    }
  }
  try {
    self._destroy(err || null, onDestroy);
  } catch (err) {
    onDestroy(err);
  }
}

function emitErrorCloseNT(self, err) {
  emitErrorNT(self, err);
  emitCloseNT(self);
}

function emitCloseNT(self) {
  const r = self._readableState;
  const w = self._writableState;

  if (w) w.closeEmitted = true;
  if (r) r.closeEmitted = true;

  if ((w?.emitClose) || (r?.emitClose)) self.emit("close");
}

function emitErrorNT(self, err) {
  const r = self._readableState;
  const w = self._writableState;

  if ((w?.errorEmitted) || (r?.errorEmitted)) return;

  if (w) w.errorEmitted = true;
  if (r) r.errorEmitted = true;

  self.emit("error", err);
}

function undestroy() {
  const r = this._readableState;
  const w = this._writableState;

  if (r) {
    r.constructed = true;
    r.closed = false;
    r.closeEmitted = false;
    r.destroyed = false;
    r.errored = null;
    r.errorEmitted = false;
    r.reading = false;
    r.ended = r.readable === false;
    r.endEmitted = r.readable === false;
  }

  if (w) {
    w.constructed = true;
    w.destroyed = false;
    w.closed = false;
    w.closeEmitted = false;
    w.errored = null;
    w.errorEmitted = false;
    w.finalCalled = false;
    w.prefinished = false;
    w.ended = w.writable === false;
    w.ending = w.writable === false;
    w.finished = w.writable === false;
  }
}

function errorOrDestroy(stream, err, sync) {
  // We have tests that rely on errors being emitted in the same tick, so changing this is
  // semver major. For now when you opt-in to autoDestroy we allow the error to be emitted
  // nextTick. In a future semver major update we should change the default to this.

  const r = stream._readableState;
  const w = stream._writableState;

  if ((w?.destroyed) || (r?.destroyed)) return this;

  if ((r?.autoDestroy) || (w?.autoDestroy)) {
    stream.destroy(err);
  } else if (err) {
    // Avoid V8 leak, https://github.com/nodejs/node/pull/34103#issuecomment-652002364
    err.stack; // eslint-disable-line no-unused-expressions

    if (w && !w.errored) w.errored = err;
    if (r && !r.errored) r.errored = err;
    if (sync) {
      process.nextTick(emitErrorNT, stream, err);
    } else {
      emitErrorNT(stream, err);
    }
  }
}

function construct(stream, cb) {
  if (typeof stream._construct !== "function") return;

  const r = stream._readableState;
  const w = stream._writableState;

  if (r) r.constructed = false;
  if (w) w.constructed = false;

  stream.once(kConstruct, cb);

  if (stream.listenerCount(kConstruct) > 1) {
    // Duplex
    return;
  }

  process.nextTick(constructNT, stream);
}

function constructNT(stream) {
  let called = false;

  function onConstruct(err) {
    if (called) {
      errorOrDestroy(stream, err ?? new ERR_MULTIPLE_CALLBACK());
      return;
    }
    called = true;

    const r = stream._readableState;
    const w = stream._writableState;
    const s = w || r;

    if (r) r.constructed = true;
    if (w) w.constructed = true;

    if (s.destroyed) {
      stream.emit(kDestroy, err);
    } else if (err) {
      errorOrDestroy(stream, err, true);
    } else {
      stream.emit(kConstruct);
    }
  }

  try {
    stream._construct((err) => {
      process.nextTick(onConstruct, err);
    });
  } catch (err) {
    process.nextTick(onConstruct, err);
  }
}

function isRequest(stream) {
  return stream?.setHeader && typeof stream.abort === "function";
}

function emitCloseLegacy(stream) {
  stream.emit("close");
}

function emitErrorCloseLegacy(stream, err) {
  stream.emit("error", err);
  process.nextTick(emitCloseLegacy, stream);
}

// Normalize destroy for legacy.
function destroyer(stream, err) {
  if (!stream || isDestroyed(stream)) return;

  if (!err && !isFinished(stream)) {
    err = new AbortError();
  }

  // TODO: Remove isRequest when Minimum Node version is 18.
  if (isServerRequest(stream)) {
    stream.socket = null;
    stream.destroy(err);
  } else if (isRequest(stream)) {
    stream.abort();
  } else if (isRequest(stream.req)) {
    stream.req.abort();
  } else if (typeof stream.destroy === "function") {
    stream.destroy(err);
  } else if (typeof stream.close === "function") {
    // TODO: Don't lose err?
    stream.close();
  } else if (err) {
    process.nextTick(emitErrorCloseLegacy, stream, err);
  } else {
    process.nextTick(emitCloseLegacy, stream);
  }

  if (!stream.destroyed) {
    stream[kDestroyed] = true;
  }
}

// ---- end-of-stream (lib/internal/streams/end-of-stream.js) ------------------------------------

function isRequestEos(stream) {
  return stream.setHeader && typeof stream.abort === "function";
}

function eos(stream, options, callback) {
  if (arguments.length === 2) {
    callback = options;
    options = kEmptyObject;
  } else if (options == null) {
    options = kEmptyObject;
  } else {
    validateObject(options, "options");
  }
  validateFunction(callback, "callback");
  validateAbortSignal(options.signal, "options.signal");

  callback = once(callback);

  if (isReadableStream(stream) || isWritableStream(stream)) {
    return eosWeb(stream, options, callback);
  }

  if (!isNodeStream(stream)) {
    throw new ERR_INVALID_ARG_TYPE("stream", ["ReadableStream", "WritableStream", "Stream"], stream);
  }

  const readable = options.readable ?? isReadableNodeStream(stream);
  const writable = options.writable ?? isWritableNodeStream(stream);

  const wState = stream._writableState;
  const rState = stream._readableState;

  const onlegacyfinish = () => {
    if (!stream.writable) {
      onfinish();
    }
  };

  // TODO (ronag): Improve soft detection to include core modules and
  // common ecosystem modules that do properly emit 'close' but fail
  // this generic check.
  let willEmitCloseV = (
    willEmitClose(stream) &&
    isReadableNodeStream(stream) === readable &&
    isWritableNodeStream(stream) === writable
  );

  let writableFinished = isWritableFinished(stream, false);
  const onfinish = () => {
    writableFinished = true;
    // Stream should not be destroyed here. If it is that
    // means that user space is doing something differently and
    // we cannot trust willEmitClose.
    if (stream.destroyed) {
      willEmitCloseV = false;
    }

    if (willEmitCloseV && (!stream.readable || readable)) {
      return;
    }

    if (!readable || readableFinished) {
      callback.call(stream);
    }
  };

  let readableFinished = isReadableFinished(stream, false);
  const onend = () => {
    readableFinished = true;
    // Stream should not be destroyed here. If it is that
    // means that user space is doing something differently and
    // we cannot trust willEmitClose.
    if (stream.destroyed) {
      willEmitCloseV = false;
    }

    if (willEmitCloseV && (!stream.writable || writable)) {
      return;
    }

    if (!writable || writableFinished) {
      callback.call(stream);
    }
  };

  const onerror = (err) => {
    callback.call(stream, err);
  };

  let closed = isClosed(stream);

  const onclose = () => {
    closed = true;

    const errored = isWritableErrored(stream) || isReadableErrored(stream);

    if (errored && typeof errored !== "boolean") {
      return callback.call(stream, errored);
    }

    if (readable && !readableFinished && isReadableNodeStream(stream, true)) {
      if (!isReadableFinished(stream, false)) return callback.call(stream, new ERR_STREAM_PREMATURE_CLOSE());
    }
    if (writable && !writableFinished) {
      if (!isWritableFinished(stream, false)) return callback.call(stream, new ERR_STREAM_PREMATURE_CLOSE());
    }

    callback.call(stream);
  };

  const onclosed = () => {
    closed = true;

    const errored = isWritableErrored(stream) || isReadableErrored(stream);

    if (errored && typeof errored !== "boolean") {
      return callback.call(stream, errored);
    }

    callback.call(stream);
  };

  const onrequest = () => {
    stream.req.on("finish", onfinish);
  };

  if (isRequestEos(stream)) {
    stream.on("complete", onfinish);
    if (!willEmitCloseV) {
      stream.on("abort", onclose);
    }
    if (stream.req) {
      onrequest();
    } else {
      stream.on("request", onrequest);
    }
  } else if (writable && !wState) { // legacy streams
    stream.on("end", onlegacyfinish);
    stream.on("close", onlegacyfinish);
  }

  // Not all streams will emit 'close' after 'aborted'.
  if (!willEmitCloseV && typeof stream.aborted === "boolean") {
    stream.on("aborted", onclose);
  }

  stream.on("end", onend);
  stream.on("finish", onfinish);
  if (options.error !== false) {
    stream.on("error", onerror);
  }
  stream.on("close", onclose);

  if (closed) {
    process.nextTick(onclose);
  } else if (wState?.errorEmitted || rState?.errorEmitted) {
    if (!willEmitCloseV) {
      process.nextTick(onclosed);
    }
  } else if (
    !readable &&
    (!willEmitCloseV || isReadable(stream)) &&
    (writableFinished || isWritable(stream) === false)
  ) {
    process.nextTick(onclosed);
  } else if (
    !writable &&
    (!willEmitCloseV || isWritable(stream)) &&
    (readableFinished || isReadable(stream) === false)
  ) {
    process.nextTick(onclosed);
  } else if ((rState && stream.req && stream.aborted)) {
    process.nextTick(onclosed);
  }

  const cleanup = () => {
    callback = nop;
    stream.removeListener("aborted", onclose);
    stream.removeListener("complete", onfinish);
    stream.removeListener("abort", onclose);
    stream.removeListener("request", onrequest);
    if (stream.req) stream.req.removeListener("finish", onfinish);
    stream.removeListener("end", onlegacyfinish);
    stream.removeListener("close", onlegacyfinish);
    stream.removeListener("finish", onfinish);
    stream.removeListener("end", onend);
    stream.removeListener("error", onerror);
    stream.removeListener("close", onclose);
  };

  if (options.signal && !closed) {
    const abort = () => {
      // Keep it because cleanup removes it.
      const endCallback = callback;
      cleanup();
      endCallback.call(
        stream,
        new AbortError(undefined, { cause: options.signal.reason }));
    };
    if (options.signal.aborted) {
      process.nextTick(abort);
    } else {
      const disposable = EE.addAbortListener(options.signal, abort);
      const originalCallback = callback;
      callback = once((...args) => {
        disposable[SymbolDispose]();
        Reflect.apply(originalCallback, stream, args);
      });
    }
  }

  return cleanup;
}

function eosWeb(stream, options, callback) {
  let isAborted = false;
  let abort = nop;
  if (options.signal) {
    abort = () => {
      isAborted = true;
      callback.call(stream, new AbortError(undefined, { cause: options.signal.reason }));
    };
    if (options.signal.aborted) {
      process.nextTick(abort);
    } else {
      const disposable = EE.addAbortListener(options.signal, abort);
      const originalCallback = callback;
      callback = once((...args) => {
        disposable[SymbolDispose]();
        Reflect.apply(originalCallback, stream, args);
      });
    }
  }
  const resolverFn = (...args) => {
    if (!isAborted) {
      process.nextTick(() => Reflect.apply(callback, stream, args));
    }
  };
  webStreamClosed(stream).then(resolverFn, resolverFn);
  return nop;
}

// lumen's web streams have no internal [kIsClosedPromise]; observe closure through a reader or
// writer only when the stream is not locked (otherwise a promise that never settles).
function webStreamClosed(stream) {
  if (stream[kIsClosedPromise]) return stream[kIsClosedPromise].promise;
  if (isReadableStream(stream) && !stream.locked) {
    const reader = stream.getReader();
    const p = reader.closed;
    reader.releaseLock();
    return p;
  }
  if (isWritableStream(stream) && !stream.locked) {
    const writer = stream.getWriter();
    const p = writer.closed;
    writer.releaseLock();
    return p;
  }
  return new Promise(() => {});
}

function finished(stream, opts) {
  let autoCleanup = false;
  if (opts === null) {
    opts = kEmptyObject;
  }
  if (opts?.cleanup) {
    validateBoolean(opts.cleanup, "cleanup");
    autoCleanup = opts.cleanup;
  }
  return new Promise((resolve, reject) => {
    const cleanup = eos(stream, opts, (err) => {
      if (autoCleanup) {
        cleanup();
      }
      if (err) {
        reject(err);
      } else {
        resolve();
      }
    });
  });
}

eos.finished = finished;

// ---- add-abort-signal (lib/internal/streams/add-abort-signal.js) ------------------------------

const validateAbortSignalStrict = (signal, name) => {
  if (typeof signal !== "object" || !("aborted" in signal)) {
    throw new ERR_INVALID_ARG_TYPE(name, "AbortSignal", signal);
  }
};

function addAbortSignal(signal, stream) {
  validateAbortSignalStrict(signal, "signal");
  if (!isNodeStream(stream) && !isWebStream(stream)) {
    throw new ERR_INVALID_ARG_TYPE("stream", ["ReadableStream", "WritableStream", "Stream"], stream);
  }
  return addAbortSignalNoValidate(signal, stream);
}

function addAbortSignalNoValidate(signal, stream) {
  if (typeof signal !== "object" || !("aborted" in signal)) {
    return stream;
  }
  const onAbort = isNodeStream(stream) ?
    () => {
      stream.destroy(new AbortError(undefined, { cause: signal.reason }));
    } :
    () => {
      stream[kControllerErrorFunction](new AbortError(undefined, { cause: signal.reason }));
    };
  if (signal.aborted) {
    onAbort();
  } else {
    const disposable = EE.addAbortListener(signal, onAbort);
    eos(stream, disposable[SymbolDispose]);
  }
  return stream;
}

// ---- state (lib/internal/streams/state.js) ----------------------------------------------------

let defaultHighWaterMarkBytes = 16 * 1024;
let defaultHighWaterMarkObjectMode = 16;

function highWaterMarkFrom(options, isDuplex, duplexKey) {
  return options.highWaterMark != null ? options.highWaterMark :
    isDuplex ? options[duplexKey] : null;
}

function getDefaultHighWaterMark(objectMode) {
  return objectMode ? defaultHighWaterMarkObjectMode : defaultHighWaterMarkBytes;
}

function setDefaultHighWaterMark(objectMode, value) {
  validateInteger(value, "value", 0);
  if (objectMode) {
    defaultHighWaterMarkObjectMode = value;
  } else {
    defaultHighWaterMarkBytes = value;
  }
}

function getHighWaterMark(state, options, duplexKey, isDuplex) {
  const hwm = highWaterMarkFrom(options, isDuplex, duplexKey);
  if (hwm != null) {
    if (!Number.isInteger(hwm) || hwm < 0) {
      const name = isDuplex ? `options.${duplexKey}` : "options.highWaterMark";
      throw new ERR_INVALID_ARG_VALUE(name, hwm);
    }
    return Math.floor(hwm);
  }

  // Default value
  return getDefaultHighWaterMark(state.objectMode);
}

// ---- BufferList (lib/internal/streams/buffer_list.js) -----------------------------------------

class BufferList {
  constructor() {
    this.head = null;
    this.tail = null;
    this.length = 0;
  }

  push(v) {
    const entry = { data: v, next: null };
    if (this.length > 0) this.tail.next = entry;
    else this.head = entry;
    this.tail = entry;
    ++this.length;
  }

  unshift(v) {
    const entry = { data: v, next: this.head };
    if (this.length === 0) this.tail = entry;
    this.head = entry;
    ++this.length;
  }

  shift() {
    if (this.length === 0) return;
    const ret = this.head.data;
    if (this.length === 1) this.head = this.tail = null;
    else this.head = this.head.next;
    --this.length;
    return ret;
  }

  clear() {
    this.head = this.tail = null;
    this.length = 0;
  }

  join(s) {
    if (this.length === 0) return "";
    let p = this.head;
    let ret = "" + p.data;
    while ((p = p.next) !== null) ret += s + p.data;
    return ret;
  }

  concat(n) {
    if (this.length === 0) return Buffer.alloc(0);
    const ret = Buffer.allocUnsafe(n >>> 0);
    let p = this.head;
    let i = 0;
    while (p) {
      ret.set(p.data, i);
      i += p.data.length;
      p = p.next;
    }
    return ret;
  }

  // Consumes a specified amount of bytes or characters from the buffered data.
  consume(n, hasStrings) {
    const data = this.head.data;
    if (n < data.length) {
      // `slice` is the same for buffers and strings.
      const slice = data.slice(0, n);
      this.head.data = data.slice(n);
      return slice;
    }
    if (n === data.length) {
      // First chunk is a perfect match.
      return this.shift();
    }
    // Result spans more than one buffer.
    return hasStrings ? this._getString(n) : this._getBuffer(n);
  }

  first() {
    return this.head.data;
  }

  *[Symbol.iterator]() {
    for (let p = this.head; p; p = p.next) {
      yield p.data;
    }
  }

  // Consumes a specified amount of characters from the buffered data.
  _getString(n) {
    let ret = "";
    let p = this.head;
    let c = 0;
    do {
      const str = p.data;
      if (n > str.length) {
        ret += str;
        n -= str.length;
      } else {
        if (n === str.length) {
          ret += str;
          ++c;
          if (p.next) this.head = p.next;
          else this.head = this.tail = null;
        } else {
          ret += str.slice(0, n);
          this.head = p;
          p.data = str.slice(n);
        }
        break;
      }
      ++c;
    } while ((p = p.next) !== null);
    this.length -= c;
    return ret;
  }

  // Consumes a specified amount of bytes from the buffered data.
  _getBuffer(n) {
    const ret = Buffer.allocUnsafe(n);
    const retLen = n;
    let p = this.head;
    let c = 0;
    do {
      const buf = p.data;
      if (n > buf.length) {
        ret.set(buf, retLen - n);
        n -= buf.length;
      } else {
        if (n === buf.length) {
          ret.set(buf, retLen - n);
          ++c;
          if (p.next) this.head = p.next;
          else this.head = this.tail = null;
        } else {
          ret.set(new Uint8Array(buf.buffer, buf.byteOffset, n), retLen - n);
          this.head = p;
          p.data = buf.slice(n);
        }
        break;
      }
      ++c;
    } while ((p = p.next) !== null);
    this.length -= c;
    return ret;
  }
}

// ---- legacy (lib/internal/streams/legacy.js) --------------------------------------------------

function Stream(opts) {
  EE.call(this, opts);
}
Object.setPrototypeOf(Stream.prototype, EE.prototype);
Object.setPrototypeOf(Stream, EE);

Stream.prototype.pipe = function (dest, options) {
  const source = this;

  function ondata(chunk) {
    if (dest.writable && dest.write(chunk) === false && source.pause) {
      source.pause();
    }
  }

  source.on("data", ondata);

  function ondrain() {
    if (source.readable && source.resume) {
      source.resume();
    }
  }

  dest.on("drain", ondrain);

  // If the 'end' option is not supplied, dest.end() will be called when
  // source gets the 'end' or 'close' events. Only dest.end() once.
  if (!dest._isStdio && (!options || options.end !== false)) {
    source.on("end", onend);
    source.on("close", onclose);
  }

  let didOnEnd = false;
  function onend() {
    if (didOnEnd) return;
    didOnEnd = true;

    dest.end();
  }

  function onclose() {
    if (didOnEnd) return;
    didOnEnd = true;

    if (typeof dest.destroy === "function") dest.destroy();
  }

  // Don't leave dangling pipes when there are errors.
  function onerror(er) {
    cleanup();
    if (EE.listenerCount(this, "error") === 0) {
      this.emit("error", er);
    }
  }

  prependListener(source, "error", onerror);
  prependListener(dest, "error", onerror);

  // Remove all the event listeners that were added.
  function cleanup() {
    source.removeListener("data", ondata);
    dest.removeListener("drain", ondrain);

    source.removeListener("end", onend);
    source.removeListener("close", onclose);

    source.removeListener("error", onerror);
    dest.removeListener("error", onerror);

    source.removeListener("end", cleanup);
    source.removeListener("close", cleanup);

    dest.removeListener("close", cleanup);
  }

  source.on("end", cleanup);
  source.on("close", cleanup);

  dest.on("close", cleanup);
  dest.emit("pipe", source);

  // Allow for unix-like usage: A.pipe(B).pipe(C)
  return dest;
};

function prependListener(emitter, event, fn) {
  // Sadly this is not cacheable as some libraries bundle their own
  // event emitter implementation with them.
  if (typeof emitter.prependListener === "function") return emitter.prependListener(event, fn);

  // This is a hack to make sure that our error handler is attached before any
  // userland ones.  NEVER DO THIS. This is here only because this code needs
  // to continue to work with older versions of Node.js that do not include
  // the prependListener() method. The goal is to eventually remove this hack.
  if (!emitter._events || !emitter._events[event]) {
    emitter.on(event, fn);
  } else if (Array.isArray(emitter._events[event])) {
    emitter._events[event].unshift(fn);
  } else {
    emitter._events[event] = [fn, emitter._events[event]];
  }
}

// ---- Readable (lib/internal/streams/readable.js) ----------------------------------------------

const kPaused = Symbol("kPaused");

function ReadableState(options, stream, isDuplex) {
  // Duplex streams are both readable and writable, but share the same options object.
  // However, some cases require setting options to different values for the readable and the
  // writable sides of the duplex stream. These options can be provided separately as
  // readableXXX and writableXXX.
  if (typeof isDuplex !== "boolean") isDuplex = stream instanceof Duplex;

  // Object stream flag. Used to make read(n) ignore n and to make all the buffer merging and
  // length checks go away.
  this.objectMode = !!(options && options.objectMode);

  if (isDuplex) this.objectMode = this.objectMode || !!(options && options.readableObjectMode);

  // The point at which it stops calling _read() to fill the buffer
  // Note: 0 is a valid value, means "don't call _read preemptively ever"
  this.highWaterMark = options ?
    getHighWaterMark(this, options, "readableHighWaterMark", isDuplex) :
    getDefaultHighWaterMark(false);

  // A linked list is used to store data chunks instead of an array because the linked list can
  // remove elements from the beginning faster than array.shift().
  this.buffer = new BufferList();
  this.length = 0;
  this.pipes = [];
  this.flowing = null;
  this.ended = false;
  this.endEmitted = false;
  this.reading = false;

  // Stream is still being constructed and cannot be destroyed until construction finished or
  // failed. Async construction is opt in, therefore we start as constructed.
  this.constructed = true;

  // A flag to be able to tell if the event 'readable'/'data' is emitted immediately, or on a
  // later tick. We set this to true at first, because any actions that shouldn't happen until
  // "later" should generally also not happen before the first read call.
  this.sync = true;

  // Whenever we return null, then we set a flag to say that we're awaiting a 'readable' event,
  // and immediately stop calling _read() until we do.
  this.needReadable = false;
  this.emittedReadable = false;
  this.readableListening = false;
  this.resumeScheduled = false;
  this[kPaused] = null;

  // True if the error was already emitted and should not be thrown again.
  this.errorEmitted = false;

  // Should close be emitted on destroy. Defaults to true.
  this.emitClose = !options || options.emitClose !== false;

  // Should .destroy() be called after 'end' (and potentially 'finish').
  this.autoDestroy = !options || options.autoDestroy !== false;

  // Has it been destroyed.
  this.destroyed = false;

  // Indicates whether the stream has errored. When true no further _read calls, 'data' or
  // 'readable' events should occur. This is needed since when autoDestroy is disabled we need a
  // way to tell whether the stream has failed.
  this.errored = null;

  // Indicates whether the stream has finished destroying.
  this.closed = false;

  // True if close has been emitted or would have been emitted depending on emitClose.
  this.closeEmitted = false;

  // Crypto is kind of old and crusty. Historically, its default string encoding is 'binary' so we
  // have to make this configurable. Everything else in the universe uses 'utf8', though.
  this.defaultEncoding = (options && options.defaultEncoding) || "utf8";

  // Ref the piped dest which we need a drain event on it type: null | Writable | Set<Writable>.
  this.awaitDrainWriters = null;
  this.multiAwaitDrain = false;

  // If true, a maybeReadMore has been scheduled.
  this.readingMore = false;

  this.dataEmitted = false;

  this.decoder = null;
  this.encoding = null;
  if (options && options.encoding) {
    this.decoder = new StringDecoder(options.encoding);
    this.encoding = options.encoding;
  }
}

function Readable(options) {
  // Checking for a Stream.Duplex instance is faster here instead of inside the ReadableState
  // constructor, at least with V8 6.5.
  const isDuplex = this instanceof Duplex;

  if (!isDuplex && !(this instanceof Readable)) return new Readable(options);

  this._readableState = new ReadableState(options, this, isDuplex);

  if (options) {
    if (typeof options.read === "function") this._read = options.read;

    if (typeof options.destroy === "function") this._destroy = options.destroy;

    if (typeof options.construct === "function") this._construct = options.construct;

    if (options.signal && !isDuplex) addAbortSignalNoValidate(options.signal, this);
  }

  Stream.call(this, options);

  construct(this, () => {
    if (this._readableState.needReadable) {
      maybeReadMore(this, this._readableState);
    }
  });
}
Object.setPrototypeOf(Readable.prototype, Stream.prototype);
Object.setPrototypeOf(Readable, Stream);

Readable.ReadableState = ReadableState;

Readable.prototype.destroy = destroyImpl;
Readable.prototype._undestroy = undestroy;
Readable.prototype._destroy = function (err, cb) {
  cb(err);
};

Readable.prototype[EE.captureRejectionSymbol] = function (err) {
  this.destroy(err);
};

Readable.prototype[SymbolAsyncDispose] = function () {
  let error;
  if (!this.destroyed) {
    error = this.readableEnded ? null : new AbortError();
    this.destroy(error);
  }
  return new Promise((resolve, reject) => eos(this, (err) => (err && err !== error ? reject(err) : resolve(null))));
};

// Manually shove something into the read() buffer. This returns true if the highWaterMark has not
// been hit yet, similar to how Writable.write() returns true if you should write() some more.
Readable.prototype.push = function (chunk, encoding) {
  return readableAddChunk(this, chunk, encoding, false);
};

// Unshift should *always* be something directly out of read().
Readable.prototype.unshift = function (chunk, encoding) {
  return readableAddChunk(this, chunk, encoding, true);
};

function readableAddChunk(stream, chunk, encoding, addToFront) {
  const state = stream._readableState;

  let err;
  if (!state.objectMode) {
    if (typeof chunk === "string") {
      encoding = encoding || state.defaultEncoding;
      if (state.encoding !== encoding) {
        if (addToFront && state.encoding) {
          // When unshifting, if state.encoding is set, we have to save the string in the
          // BufferList with the state encoding.
          chunk = Buffer.from(chunk, encoding).toString(state.encoding);
        } else {
          chunk = Buffer.from(chunk, encoding);
          encoding = "";
        }
      }
    } else if (chunk instanceof Buffer) {
      encoding = "";
    } else if (Stream._isUint8Array(chunk)) {
      chunk = Stream._uint8ArrayToBuffer(chunk);
      encoding = "";
    } else if (chunk != null) {
      err = new ERR_INVALID_ARG_TYPE("chunk", ["string", "Buffer", "Uint8Array"], chunk);
    }
  }

  if (err) {
    errorOrDestroy(stream, err);
  } else if (chunk === null) {
    state.reading = false;
    onEofChunk(stream, state);
  } else if (state.objectMode || (chunk && chunk.length > 0)) {
    if (addToFront) {
      if (state.endEmitted) {
        errorOrDestroy(stream, new ERR_STREAM_UNSHIFT_AFTER_END_EVENT());
      } else if (state.destroyed || state.errored) {
        return false;
      } else {
        addChunk(stream, state, chunk, true);
      }
    } else if (state.ended) {
      errorOrDestroy(stream, new ERR_STREAM_PUSH_AFTER_EOF());
    } else if (state.destroyed || state.errored) {
      return false;
    } else {
      state.reading = false;
      if (state.decoder && !encoding) {
        chunk = state.decoder.write(chunk);
        if (state.objectMode || chunk.length !== 0) {
          addChunk(stream, state, chunk, false);
        } else {
          maybeReadMore(stream, state);
        }
      } else {
        addChunk(stream, state, chunk, false);
      }
    }
  } else if (!addToFront) {
    state.reading = false;
    maybeReadMore(stream, state);
  }

  // We can push more data if we are below the highWaterMark. Also, if we have no data yet, we can
  // stand some more bytes. This is to work around cases where hwm=0, such as the repl.
  return !state.ended &&
    (state.length < state.highWaterMark || state.length === 0);
}

function addChunk(stream, state, chunk, addToFront) {
  if (state.flowing && state.length === 0 && !state.sync &&
      stream.listenerCount("data") > 0) {
    // Use the guard to avoid creating `Set()` repeatedly when we have multiple pipes.
    if (state.multiAwaitDrain) {
      state.awaitDrainWriters.clear();
    } else {
      state.awaitDrainWriters = null;
    }

    state.dataEmitted = true;
    stream.emit("data", chunk);
  } else {
    // Update the buffer info.
    state.length += state.objectMode ? 1 : chunk.length;
    if (addToFront) state.buffer.unshift(chunk);
    else state.buffer.push(chunk);

    if (state.needReadable) emitReadable(stream);
  }
  maybeReadMore(stream, state);
}

Readable.prototype.isPaused = function () {
  const state = this._readableState;
  return state[kPaused] === true || state.flowing === false;
};

// Backwards compatibility.
Readable.prototype.setEncoding = function (enc) {
  const decoder = new StringDecoder(enc);
  this._readableState.decoder = decoder;
  // If setEncoding(null), decoder.encoding equals utf8.
  this._readableState.encoding = this._readableState.decoder.encoding;

  const buffer = this._readableState.buffer;
  // Iterate over current buffer to convert already stored Buffers:
  let content = "";
  for (const data of buffer) {
    content += decoder.write(data);
  }
  buffer.clear();
  if (content !== "") buffer.push(content);
  this._readableState.length = content.length;
  return this;
};

// Don't raise the hwm > 1GB.
const MAX_HWM = 0x40000000;
function computeNewHighWaterMark(n) {
  if (n > MAX_HWM) {
    throw new ERR_OUT_OF_RANGE("size", "<= 1GiB", n);
  } else {
    // Get the next highest power of 2 to prevent increasing hwm excessively in tiny amounts.
    n--;
    n |= n >>> 1;
    n |= n >>> 2;
    n |= n >>> 4;
    n |= n >>> 8;
    n |= n >>> 16;
    n++;
  }
  return n;
}

// This function is designed to be inlinable, so please take care when making changes to the
// function body.
function howMuchToRead(n, state) {
  if (n <= 0 || (state.length === 0 && state.ended)) return 0;
  if (state.objectMode) return 1;
  if (Number.isNaN(n)) {
    // Only flow one buffer at a time.
    if (state.flowing && state.length) return state.buffer.first().length;
    return state.length;
  }
  if (n <= state.length) return n;
  return state.ended ? state.length : 0;
}

// You can override either this method, or the async _read(n) below.
Readable.prototype.read = function (n) {
  // Same as parseInt(undefined, 10), however V8 7.3 performance regressed in this scenario, so we
  // are doing it manually.
  if (n === undefined) {
    n = NaN;
  } else if (!Number.isInteger(n)) {
    n = Number.parseInt(n, 10);
  }
  const state = this._readableState;
  const nOrig = n;

  // If we're asking for more than the current hwm, then raise the hwm.
  if (n > state.highWaterMark) state.highWaterMark = computeNewHighWaterMark(n);

  if (n !== 0) state.emittedReadable = false;

  // If we're doing read(0) to trigger a readable event, but we already have a bunch of data in
  // the buffer, then just trigger the 'readable' event and move on.
  if (n === 0 &&
      state.needReadable &&
      ((state.highWaterMark !== 0 ?
        state.length >= state.highWaterMark :
        state.length > 0) ||
       state.ended)) {
    if (state.length === 0 && state.ended) endReadable(this);
    else emitReadable(this);
    return null;
  }

  n = howMuchToRead(n, state);

  // If we've ended, and we're now clear, then finish it up.
  if (n === 0 && state.ended) {
    if (state.length === 0) endReadable(this);
    return null;
  }

  // All the actual chunk generation logic needs to be *below* the call to _read. The reason is
  // that in certain synthetic stream cases, such as passthrough streams, _read may be a
  // completely synchronous operation which may change the state of the read buffer, providing
  // enough data when before there was *not* enough.
  //
  // So, the steps are:
  // 1. Figure out what the state of things will be after we do a read from the buffer.
  //
  // 2. If that resulting state will trigger a _read, then call _read. Note that this may be
  // asynchronous, or synchronous. Yes, it is deeply ugly to write APIs this way, but that still
  // doesn't mean that the Readable class should behave improperly, as streams are designed to
  // be sync/async agnostic. Take note if the _read call is sync or async (ie, if the read call
  // has returned yet), so that we know whether or not it's safe to emit 'readable' etc.
  //
  // 3. Actually pull the requested chunks out of the buffer and return.

  // if we need a readable event, then we need to do some reading.
  let doRead = state.needReadable;

  // If we currently have less than the highWaterMark, then also read some.
  if (state.length === 0 || state.length - n < state.highWaterMark) {
    doRead = true;
  }

  // However, if we've ended, then there's no point, if we're already reading, then it's
  // unnecessary, if we're constructing we have to wait, and if we're destroyed or errored, then
  // it's not allowed,
  if (state.ended || state.reading || state.destroyed || state.errored || !state.constructed) {
    doRead = false;
  } else if (doRead) {
    state.reading = true;
    state.sync = true;
    // If the length is currently zero, then we *need* a readable event.
    if (state.length === 0) state.needReadable = true;

    // Call internal read method
    try {
      this._read(state.highWaterMark);
    } catch (error) {
      errorOrDestroy(this, error);
    }
    state.sync = false;
    // If _read pushed data synchronously, then `reading` will be false, and we need to
    // re-evaluate how much data we can return to the user.
    if (!state.reading) n = howMuchToRead(nOrig, state);
  }

  let ret;
  if (n > 0) ret = fromList(n, state);
  else ret = null;

  if (ret === null) {
    state.needReadable = state.length <= state.highWaterMark;
    n = 0;
  } else {
    state.length -= n;
    if (state.multiAwaitDrain) {
      state.awaitDrainWriters.clear();
    } else {
      state.awaitDrainWriters = null;
    }
  }

  if (state.length === 0) {
    // If we have nothing in the buffer, then we want to know as soon as we *do* get something
    // into the buffer.
    if (!state.ended) state.needReadable = true;

    // If we tried to read() past the EOF, then emit end on the next tick.
    if (nOrig !== n && state.ended) endReadable(this);
  }

  if (ret !== null && !state.errorEmitted && !state.closeEmitted) {
    state.dataEmitted = true;
    this.emit("data", ret);
  }

  return ret;
};

function onEofChunk(stream, state) {
  if (state.ended) return;
  if (state.decoder) {
    const chunk = state.decoder.end();
    if (chunk && chunk.length) {
      state.buffer.push(chunk);
      state.length += state.objectMode ? 1 : chunk.length;
    }
  }
  state.ended = true;

  if (state.sync) {
    // If we are sync, wait until next tick to emit the data. Otherwise we risk emitting data in
    // the flow() the readable code triggers during a read() call.
    emitReadable(stream);
  } else {
    // Emit 'readable' now to make sure it gets picked up.
    state.needReadable = false;
    state.emittedReadable = true;
    // We have to emit readable now that we are EOF. Modules in the ecosystem (e.g. dicer) rely on
    // this event being sync.
    emitReadable_(stream);
  }
}

// Don't emit readable right away in sync mode, because this can trigger another read() call =>
// stack overflow. This way, it might trigger a nextTick recursion warning, but that's not so bad.
function emitReadable(stream) {
  const state = stream._readableState;
  state.needReadable = false;
  if (!state.emittedReadable) {
    state.emittedReadable = true;
    process.nextTick(emitReadable_, stream);
  }
}

function emitReadable_(stream) {
  const state = stream._readableState;
  if (!state.destroyed && !state.errored && (state.length || state.ended)) {
    stream.emit("readable");
    state.emittedReadable = false;
  }

  // The stream needs another readable event if:
  // 1. It is not flowing, as the flow mechanism will take care of it.
  // 2. It is not ended.
  // 3. It is below the highWaterMark, so we can schedule another readable later.
  state.needReadable =
    !state.flowing &&
    !state.ended &&
    state.length <= state.highWaterMark;
  flow(stream);
}

// At this point, the user has presumably seen the 'readable' event, and called read() to consume
// some data. That may have triggered in turn another _read(n) call, in which case reading = true
// if it's in progress. However, if we're not ended, or reading, and the length < hwm, then go
// ahead and try to read some more preemptively.
function maybeReadMore(stream, state) {
  if (!state.readingMore && state.constructed) {
    state.readingMore = true;
    process.nextTick(maybeReadMore_, stream, state);
  }
}

function maybeReadMore_(stream, state) {
  // Attempt to read more data if we should.
  //
  // The conditions for reading more data are (one of):
  // - Not enough data buffered (state.length < state.highWaterMark). The loop is responsible for
  //   filling the buffer with enough data if such data is available. If highWaterMark is 0 and we
  //   are not in the flowing mode we should _not_ attempt to buffer any extra data. We'll get
  //   more data when the stream consumer calls read() instead.
  // - No data in the buffer, and the stream is in flowing mode. In this mode the loop below is
  //   responsible for ensuring read() is called. Failing to call read here would abort the flow
  //   and there's no other mechanism for continuing the flow if the stream consumer has just
  //   subscribed to the 'data' event.
  //
  // In addition to the above conditions to keep reading data, the following conditions prevent
  // the data from being read:
  // - The stream has ended (state.ended).
  // - There is already a pending 'read' operation (state.reading). This is a case where the
  //   stream has called the implementation defined _read() method, but they are processing the
  //   call asynchronously and have _not_ called push() with new data. In this case we skip
  //   performing more read()s. The execution ends in this method again after the _read() ends up
  //   calling push() with more data.
  while (!state.reading && !state.ended &&
         (state.length < state.highWaterMark ||
          (state.flowing && state.length === 0))) {
    const len = state.length;
    stream.read(0);
    if (len === state.length) {
      // Didn't get any data, stop spinning.
      break;
    }
  }
  state.readingMore = false;
}

// Abstract method. to be overridden in specific implementation classes. call cb(er, data) where
// data is <= n in length. for virtual (non-string, non-buffer) streams, "length" is somewhat
// arbitrary, and perhaps not very meaningful.
Readable.prototype._read = function (n) {
  throw new ERR_METHOD_NOT_IMPLEMENTED("_read()");
};

Readable.prototype.pipe = function (dest, pipeOpts) {
  const src = this;
  const state = this._readableState;

  if (state.pipes.length === 1) {
    if (!state.multiAwaitDrain) {
      state.multiAwaitDrain = true;
      state.awaitDrainWriters = new Set(state.awaitDrainWriters ? [state.awaitDrainWriters] : []);
    }
  }

  state.pipes.push(dest);

  const doEnd = (!pipeOpts || pipeOpts.end !== false) &&
              dest !== process.stdout &&
              dest !== process.stderr;

  const endFn = doEnd ? onend : unpipe;
  if (state.endEmitted) process.nextTick(endFn);
  else src.once("end", endFn);

  dest.on("unpipe", onunpipe);
  function onunpipe(readable, unpipeInfo) {
    if (readable === src) {
      if (unpipeInfo && unpipeInfo.hasUnpiped === false) {
        unpipeInfo.hasUnpiped = true;
        cleanup();
      }
    }
  }

  function onend() {
    dest.end();
  }

  let ondrain;

  let cleanedUp = false;
  function cleanup() {
    // Cleanup event handlers once the pipe is broken.
    dest.removeListener("close", onclose);
    dest.removeListener("finish", onfinish);
    if (ondrain) {
      dest.removeListener("drain", ondrain);
    }
    dest.removeListener("error", onerror);
    dest.removeListener("unpipe", onunpipe);
    src.removeListener("end", onend);
    src.removeListener("end", unpipe);
    src.removeListener("data", ondata);

    cleanedUp = true;

    // If the reader is waiting for a drain event from this specific writer, then it would cause it
    // to never start flowing again. So, if this is awaiting a drain, then we just call it now. If
    // we don't know, then assume that we are waiting for one.
    if (ondrain && state.awaitDrainWriters &&
        (!dest._writableState || dest._writableState.needDrain)) ondrain();
  }

  function pause() {
    // If the user unpiped during `dest.write()`, it is possible to get stuck in a permanently
    // paused state if that write also returned false. => Check whether `dest` is still a piping
    // destination.
    if (!cleanedUp) {
      if (state.pipes.length === 1 && state.pipes[0] === dest) {
        state.awaitDrainWriters = dest;
        state.multiAwaitDrain = false;
      } else if (state.pipes.length > 1 && state.pipes.includes(dest)) {
        state.awaitDrainWriters.add(dest);
      }
      src.pause();
    }
    if (!ondrain) {
      // When the dest drains, it reduces the awaitDrain counter on the source. This would be more
      // elegant with a .once() handler in flow(), but adding and removing repeatedly is too slow.
      ondrain = pipeOnDrain(src, dest);
      dest.on("drain", ondrain);
    }
  }

  src.on("data", ondata);
  function ondata(chunk) {
    const ret = dest.write(chunk);
    if (ret === false) {
      pause();
    }
  }

  // If the dest has an error, then stop piping into it. However, don't suppress the throwing
  // behavior for this.
  function onerror(er) {
    unpipe();
    dest.removeListener("error", onerror);
    if (dest.listenerCount("error") === 0) {
      const s = dest._writableState || dest._readableState;
      if (s && !s.errorEmitted) {
        // User incorrectly emitted 'error' directly on the stream.
        errorOrDestroy(dest, er);
      } else {
        dest.emit("error", er);
      }
    }
  }

  // Make sure our error handler is attached before userland ones.
  prependListener(dest, "error", onerror);

  // Both close and finish should trigger unpipe, but only once.
  function onclose() {
    dest.removeListener("finish", onfinish);
    unpipe();
  }
  dest.once("close", onclose);
  function onfinish() {
    dest.removeListener("close", onclose);
    unpipe();
  }
  dest.once("finish", onfinish);

  function unpipe() {
    src.unpipe(dest);
  }

  // Tell the dest that it's being piped to.
  dest.emit("pipe", src);

  // Start the flow if it hasn't been started already.

  if (dest.writableNeedDrain === true) {
    pause();
  } else if (!state.flowing) {
    src.resume();
  }

  return dest;
};

function pipeOnDrain(src, dest) {
  return function pipeOnDrainFunctionResult() {
    const state = src._readableState;

    // `ondrain` will call directly,
    // `this` maybe not a reference to dest,
    // so we use the real dest here.
    if (state.awaitDrainWriters === dest) {
      state.awaitDrainWriters = null;
    } else if (state.multiAwaitDrain) {
      state.awaitDrainWriters.delete(dest);
    }

    if ((!state.awaitDrainWriters || state.awaitDrainWriters.size === 0) &&
      src.listenerCount("data")) {
      src.resume();
    }
  };
}

Readable.prototype.unpipe = function (dest) {
  const state = this._readableState;
  const unpipeInfo = { hasUnpiped: false };

  // If we're not piping anywhere, then do nothing.
  if (state.pipes.length === 0) return this;

  if (!dest) {
    // remove all.
    const dests = state.pipes;
    state.pipes = [];
    this.pause();

    for (let i = 0; i < dests.length; i++) dests[i].emit("unpipe", this, { hasUnpiped: false });
    return this;
  }

  // Try to find the right one.
  const index = state.pipes.indexOf(dest);
  if (index === -1) return this;

  state.pipes.splice(index, 1);

  if (state.pipes.length === 0) this.pause();

  dest.emit("unpipe", this, unpipeInfo);

  return this;
};

// Set up data events if they are asked for. Ensure readable listeners eventually get something.
Readable.prototype.on = function (ev, fn) {
  const res = Stream.prototype.on.call(this, ev, fn);
  const state = this._readableState;

  if (ev === "data") {
    // Update readableListening so that resume() may be a no-op a few lines down. This is needed
    // to support once('readable').
    state.readableListening = this.listenerCount("readable") > 0;

    // Try start flowing on next tick if stream isn't explicitly paused.
    if (state.flowing !== false) this.resume();
  } else if (ev === "readable") {
    if (!state.endEmitted && !state.readableListening) {
      state.readableListening = state.needReadable = true;
      state.flowing = false;
      state.emittedReadable = false;
      if (state.length) {
        emitReadable(this);
      } else if (!state.reading) {
        process.nextTick(nReadingNextTick, this);
      }
    }
  }

  return res;
};
Readable.prototype.addListener = Readable.prototype.on;

Readable.prototype.removeListener = function (ev, fn) {
  const res = Stream.prototype.removeListener.call(this, ev, fn);

  if (ev === "readable") {
    // We need to check if there is someone still listening to readable and reset the state.
    // However this needs to happen after readable has been emitted but before I/O (nextTick) to
    // support once('readable', fn) cycles. This means that calling resume within the same tick
    // will have no effect.
    process.nextTick(updateReadableListening, this);
  }

  return res;
};
Readable.prototype.off = Readable.prototype.removeListener;

Readable.prototype.removeAllListeners = function (ev) {
  const res = Stream.prototype.removeAllListeners.apply(this, arguments);

  if (ev === "readable" || ev === undefined) {
    // We need to check if there is someone still listening to readable and reset the state.
    // However this needs to happen after readable has been emitted but before I/O (nextTick) to
    // support once('readable', fn) cycles. This means that calling resume within the same tick
    // will have no effect.
    process.nextTick(updateReadableListening, this);
  }

  return res;
};

function updateReadableListening(self) {
  const state = self._readableState;
  state.readableListening = self.listenerCount("readable") > 0;

  if (state.resumeScheduled && state[kPaused] === false) {
    // Flowing needs to be set to true now, otherwise the upcoming resume will not flow.
    state.flowing = true;

    // Crude way to check if we should resume.
  } else if (self.listenerCount("data") > 0) {
    self.resume();
  } else if (!state.readableListening) {
    state.flowing = null;
  }
}

function nReadingNextTick(self) {
  self.read(0);
}

// pause() and resume() are remnants of the legacy readable stream API. If the user uses them,
// then switch into old mode.
Readable.prototype.resume = function () {
  const state = this._readableState;
  if (!state.flowing) {
    // We flow only if there is no one listening for readable, but we still have to call resume().
    state.flowing = !state.readableListening;
    resume(this, state);
  }
  state[kPaused] = false;
  return this;
};

function resume(stream, state) {
  if (!state.resumeScheduled) {
    state.resumeScheduled = true;
    process.nextTick(resume_, stream, state);
  }
}

function resume_(stream, state) {
  if (!state.reading) {
    stream.read(0);
  }

  state.resumeScheduled = false;
  stream.emit("resume");
  flow(stream);
  if (state.flowing && !state.reading) stream.read(0);
}

Readable.prototype.pause = function () {
  if (this._readableState.flowing !== false) {
    this._readableState.flowing = false;
    this.emit("pause");
  }
  this._readableState[kPaused] = true;
  return this;
};

function flow(stream) {
  const state = stream._readableState;
  while (state.flowing && stream.read() !== null);
}

// Wrap an old-style stream as the async data source. This is *not* part of the readable stream
// interface. It is an ugly unfortunate mess of history.
Readable.prototype.wrap = function (stream) {
  let paused = false;

  // TODO (ronag): Should this.destroy(err) emit 'error' on the wrapped stream? Would require a
  // wrapped stream to be treated as a proper stream.

  stream.on("data", (chunk) => {
    if (!this.push(chunk) && stream.pause) {
      paused = true;
      stream.pause();
    }
  });

  stream.on("end", () => {
    this.push(null);
  });

  stream.on("error", (err) => {
    errorOrDestroy(this, err);
  });

  stream.on("close", () => {
    this.destroy();
  });

  stream.on("destroy", () => {
    this.destroy();
  });

  this._read = () => {
    if (paused && stream.resume) {
      paused = false;
      stream.resume();
    }
  };

  // Proxy all the other methods. Important when wrapping filters and duplexes.
  const streamKeys = Object.keys(stream);
  for (let j = 1; j < streamKeys.length; j++) {
    const i = streamKeys[j];
    if (this[i] === undefined && typeof stream[i] === "function") {
      this[i] = stream[i].bind(stream);
    }
  }

  return this;
};

Readable.prototype[Symbol.asyncIterator] = function () {
  return streamToAsyncIterator(this);
};

Readable.prototype.iterator = function (options) {
  if (options !== undefined) {
    validateObject(options, "options");
  }
  return streamToAsyncIterator(this, options);
};

function streamToAsyncIterator(stream, options) {
  if (typeof stream.read !== "function") {
    stream = Readable.wrap(stream, { objectMode: true });
  }

  const iter = createAsyncIterator(stream, options);
  iter.stream = stream;
  return iter;
}

async function* createAsyncIterator(stream, options) {
  let callback = nop;

  function next(resolve) {
    if (this === stream) {
      callback();
      callback = nop;
    } else {
      callback = resolve;
    }
  }

  stream.on("readable", next);

  let error;
  const cleanup = eos(stream, { writable: false }, (err) => {
    error = err ? aggregateTwoErrors(error, err) : null;
    callback();
    callback = nop;
  });

  try {
    while (true) {
      const chunk = stream.destroyed ? null : stream.read();
      if (chunk !== null) {
        yield chunk;
      } else if (error) {
        throw error;
      } else if (error === null) {
        return;
      } else {
        await new Promise(next);
      }
    }
  } catch (err) {
    error = aggregateTwoErrors(error, err);
    throw error;
  } finally {
    if (
      (error || options?.destroyOnReturn !== false) &&
      (error === undefined || stream._readableState.autoDestroy)
    ) {
      destroyer(stream, null);
    } else {
      stream.off("readable", next);
      cleanup();
    }
  }
}

// Making it explicit these properties are not enumerable because otherwise some prototype
// manipulation in userland will fail.
Object.defineProperties(Readable.prototype, {
  readable: {
    __proto__: null,
    get() {
      const r = this._readableState;
      // r.readable === false means that this is part of a Duplex stream where the readable side
      // was disabled upon construction. Compat. The user might manually disable readable side
      // through deprecated setter.
      return !!r && r.readable !== false && !r.destroyed && !r.errorEmitted &&
        !r.endEmitted;
    },
    set(val) {
      // Backwards compat.
      if (this._readableState) {
        this._readableState.readable = !!val;
      }
    },
  },

  readableDidRead: {
    __proto__: null,
    enumerable: false,
    get: function () {
      return this._readableState.dataEmitted;
    },
  },

  readableAborted: {
    __proto__: null,
    enumerable: false,
    get: function () {
      return !!(
        this._readableState.readable !== false &&
        (this._readableState.destroyed || this._readableState.errored) &&
        !this._readableState.endEmitted
      );
    },
  },

  readableHighWaterMark: {
    __proto__: null,
    enumerable: false,
    get: function () {
      return this._readableState.highWaterMark;
    },
  },

  readableBuffer: {
    __proto__: null,
    enumerable: false,
    get: function () {
      return this._readableState && this._readableState.buffer;
    },
  },

  readableFlowing: {
    __proto__: null,
    enumerable: false,
    get: function () {
      return this._readableState.flowing;
    },
    set: function (state) {
      if (this._readableState) {
        this._readableState.flowing = state;
      }
    },
  },

  readableLength: {
    __proto__: null,
    enumerable: false,
    get() {
      return this._readableState.length;
    },
  },

  readableObjectMode: {
    __proto__: null,
    enumerable: false,
    get() {
      return this._readableState ? this._readableState.objectMode : false;
    },
  },

  readableEncoding: {
    __proto__: null,
    enumerable: false,
    get() {
      return this._readableState ? this._readableState.encoding : null;
    },
  },

  errored: {
    __proto__: null,
    enumerable: false,
    get() {
      return this._readableState ? this._readableState.errored : null;
    },
  },

  closed: {
    __proto__: null,
    get() {
      return this._readableState ? this._readableState.closed : false;
    },
  },

  destroyed: {
    __proto__: null,
    enumerable: false,
    get() {
      return this._readableState ? this._readableState.destroyed : false;
    },
    set(value) {
      // We ignore the value if the stream has not been initialized yet.
      if (!this._readableState) {
        return;
      }

      // Backward compatibility, the user is explicitly managing destroyed.
      this._readableState.destroyed = value;
    },
  },

  readableEnded: {
    __proto__: null,
    enumerable: false,
    get() {
      return this._readableState ? this._readableState.endEmitted : false;
    },
  },

});

Object.defineProperties(ReadableState.prototype, {
  // Legacy getter for `pipesCount`.
  pipesCount: {
    __proto__: null,
    get() {
      return this.pipes.length;
    },
  },

  // Legacy property for `paused`.
  paused: {
    __proto__: null,
    get() {
      return this[kPaused] !== false;
    },
    set(value) {
      this[kPaused] = !!value;
    },
  },
});

// Exposed for testing purposes only.
Readable._fromList = fromList;

// Pluck off n bytes from an array of buffers. Length is the combined lengths of all the buffers
// in the list. This function is designed to be inlinable, so please take care when making
// changes to the function body.
function fromList(n, state) {
  // nothing buffered.
  if (state.length === 0) return null;

  let ret;
  if (state.objectMode) ret = state.buffer.shift();
  else if (!n || n >= state.length) {
    // Read it all, truncate the list.
    if (state.decoder) ret = state.buffer.join("");
    else if (state.buffer.length === 1) ret = state.buffer.first();
    else ret = state.buffer.concat(state.length);
    state.buffer.clear();
  } else {
    // read part of list.
    ret = state.buffer.consume(n, state.decoder);
  }

  return ret;
}

function endReadable(stream) {
  const state = stream._readableState;

  if (!state.endEmitted) {
    state.ended = true;
    process.nextTick(endReadableNT, state, stream);
  }
}

function endReadableNT(state, stream) {
  // Check that we didn't get one last unshift.
  if (!state.errored && !state.closeEmitted &&
      !state.endEmitted && state.length === 0) {
    state.endEmitted = true;
    stream.emit("end");

    if (stream.writable && stream.allowHalfOpen === false) {
      process.nextTick(endWritableNT, stream);
    } else if (state.autoDestroy) {
      // In case of duplex streams we need a way to detect if the writable side is ready for
      // autoDestroy as well.
      const wState = stream._writableState;
      const autoDestroy = !wState || (
        wState.autoDestroy &&
        // We don't expect the writable to ever 'finish' if writable is explicitly set to false.
        (wState.finished || wState.writable === false)
      );

      if (autoDestroy) {
        stream.destroy();
      }
    }
  }
}

function endWritableNT(stream) {
  const writable = stream.writable && !stream.writableEnded &&
    !stream.destroyed;
  if (writable) {
    stream.end();
  }
}

Readable.from = function (iterable, opts) {
  return from(Readable, iterable, opts);
};

Readable.fromWeb = function (readableStream, options) {
  return newStreamReadableFromReadableStream(readableStream, options);
};

Readable.toWeb = function (streamReadable, options) {
  return newReadableStreamFromStreamReadable(streamReadable, options);
};

Readable.wrap = function (src, options) {
  return new Readable({
    objectMode: src.readableObjectMode ?? src.objectMode ?? true,
    ...options,
    destroy(err, callback) {
      destroyer(src, err);
      callback(err);
    },
  }).wrap(src);
};

// ---- Writable (lib/internal/streams/writable.js) ----------------------------------------------

const kOnFinished = Symbol("kOnFinished");

function WritableState(options, stream, isDuplex) {
  // Duplex streams are both readable and writable, but share the same options object. However,
  // some cases require setting options to different values for the readable and the writable
  // sides of the duplex stream, e.g. options.readableObjectMode vs. options.writableObjectMode.
  if (typeof isDuplex !== "boolean") isDuplex = stream instanceof Duplex;

  // Object stream flag to indicate whether or not this stream contains buffers or objects.
  this.objectMode = !!(options && options.objectMode);

  if (isDuplex) this.objectMode = this.objectMode || !!(options && options.writableObjectMode);

  // The point at which write() starts returning false Note: 0 is a valid value, means that we
  // always return false if the entire buffer is not flushed immediately on write().
  this.highWaterMark = options ?
    getHighWaterMark(this, options, "writableHighWaterMark", isDuplex) :
    getDefaultHighWaterMark(false);

  // if _final has been called.
  this.finalCalled = false;

  // drain event flag.
  this.needDrain = false;
  // At the start of calling end()
  this.ending = false;
  // When end() has been called, and returned.
  this.ended = false;
  // When 'finish' is emitted.
  this.finished = false;

  // Has it been destroyed
  this.destroyed = false;

  // Should we decode strings into buffers before passing to _write? this is here so that some
  // node-core streams can optimize string handling at a lower level.
  const noDecode = !!(options && options.decodeStrings === false);
  this.decodeStrings = !noDecode;

  // Crypto is kind of old and crusty. Historically, its default string encoding is 'binary' so we
  // have to make this configurable. Everything else in the universe uses 'utf8', though.
  this.defaultEncoding = (options && options.defaultEncoding) || "utf8";

  // Not an actual buffer we keep track of, but a measurement of how much we're waiting to get
  // pushed to some underlying socket or file.
  this.length = 0;

  // A flag to see when we're in the middle of a write.
  this.writing = false;

  // When true all writes will be buffered until .uncork() call.
  this.corked = 0;

  // A flag to be able to tell if the onwrite cb is called immediately, or on a later tick. We set
  // this to true at first, because any actions that shouldn't happen until "later" should
  // generally also not happen before the first write call.
  this.sync = true;

  // A flag to know if we're processing previously buffered items, which may call the _write()
  // callback in the same tick, so that we don't end up in an overlapped onwrite situation.
  this.bufferProcessing = false;

  // The callback that's passed to _write(chunk, cb).
  this.onwrite = onwrite.bind(undefined, stream);

  // The callback that the user supplies to write(chunk, encoding, cb).
  this.writecb = null;

  // The amount that is being written when _write is called.
  this.writelen = 0;

  // Storage for data passed to the afterWrite() callback in case of synchronous _write()
  // completion.
  this.afterWriteTickInfo = null;

  resetBuffer(this);

  // Number of pending user-supplied write callbacks this must be 0 before 'finish' can be
  // emitted.
  this.pendingcb = 0;

  // Stream is still being constructed and cannot be destroyed until construction finished or
  // failed. Async construction is opt in, therefore we start as constructed.
  this.constructed = true;

  // Emit prefinish if the only thing we're waiting for is _write cbs This is relevant for
  // synchronous Transform streams.
  this.prefinished = false;

  // True if the error was already emitted and should not be thrown again.
  this.errorEmitted = false;

  // Should close be emitted on destroy. Defaults to true.
  this.emitClose = !options || options.emitClose !== false;

  // Should .destroy() be called after 'finish' (and potentially 'end').
  this.autoDestroy = !options || options.autoDestroy !== false;

  // Indicates whether the stream has errored. When true all write() calls should return false.
  // This is needed since when autoDestroy is disabled we need a way to tell whether the stream
  // has failed.
  this.errored = null;

  // Indicates whether the stream has finished destroying.
  this.closed = false;

  // True if close has been emitted or would have been emitted depending on emitClose.
  this.closeEmitted = false;

  this[kOnFinished] = [];
}

function resetBuffer(state) {
  state.buffered = [];
  state.bufferedIndex = 0;
  state.allBuffers = true;
  state.allNoop = true;
}

WritableState.prototype.getBuffer = function getBuffer() {
  return this.buffered.slice(this.bufferedIndex);
};

Object.defineProperty(WritableState.prototype, "bufferedRequestCount", {
  __proto__: null,
  get() {
    return this.buffered.length - this.bufferedIndex;
  },
});

function Writable(options) {
  // Writable ctor is applied to Duplexes, too. `realHasInstance` is necessary because using plain
  // `instanceof` would not work for Duplexes, since they inherit from Readable and only
  // polyfill Writable's methods.

  // Trying to use the custom `instanceof` for Writable here will also break the Node.js
  // LazyTransform implementation, which has a non-trivial getter for `_writableState` that
  // would lead to infinite recursion.

  // Checking for a Stream.Duplex instance is faster here instead of inside the WritableState
  // constructor, at least with V8 6.5.
  const isDuplex = (this instanceof Duplex);

  if (!isDuplex && !realHasInstance.call(Writable, this)) return new Writable(options);

  this._writableState = new WritableState(options, this, isDuplex);

  if (options) {
    if (typeof options.write === "function") this._write = options.write;

    if (typeof options.writev === "function") this._writev = options.writev;

    if (typeof options.destroy === "function") this._destroy = options.destroy;

    if (typeof options.final === "function") this._final = options.final;

    if (typeof options.construct === "function") this._construct = options.construct;

    if (options.signal) addAbortSignalNoValidate(options.signal, this);
  }

  Stream.call(this, options);

  construct(this, () => {
    const state = this._writableState;

    if (!state.writing) {
      clearBuffer(this, state);
    }

    finishMaybe(this, state);
  });
}
Object.setPrototypeOf(Writable.prototype, Stream.prototype);
Object.setPrototypeOf(Writable, Stream);

Writable.WritableState = WritableState;

const realHasInstance = Function.prototype[Symbol.hasInstance];
Object.defineProperty(Writable, Symbol.hasInstance, {
  __proto__: null,
  value: function (object) {
    if (realHasInstance.call(this, object)) return true;
    if (this !== Writable) return false;

    return object && object._writableState instanceof WritableState;
  },
});

// Otherwise people can pipe Writable streams, which is just wrong.
Writable.prototype.pipe = function () {
  errorOrDestroy(this, new ERR_STREAM_CANNOT_PIPE());
};

function _write(stream, chunk, encoding, cb) {
  const state = stream._writableState;

  if (typeof encoding === "function") {
    cb = encoding;
    encoding = state.defaultEncoding;
  } else {
    if (!encoding) encoding = state.defaultEncoding;
    else if (encoding !== "buffer" && !Buffer.isEncoding(encoding)) throw new ERR_UNKNOWN_ENCODING(encoding);
    if (typeof cb !== "function") cb = nop;
  }

  if (chunk === null) {
    throw new ERR_STREAM_NULL_VALUES();
  } else if (!state.objectMode) {
    if (typeof chunk === "string") {
      if (state.decodeStrings !== false) {
        chunk = Buffer.from(chunk, encoding);
        encoding = "buffer";
      }
    } else if (chunk instanceof Buffer) {
      encoding = "buffer";
    } else if (Stream._isUint8Array(chunk)) {
      chunk = Stream._uint8ArrayToBuffer(chunk);
      encoding = "buffer";
    } else {
      throw new ERR_INVALID_ARG_TYPE("chunk", ["string", "Buffer", "Uint8Array"], chunk);
    }
  }

  let err;
  if (state.ending) {
    err = new ERR_STREAM_WRITE_AFTER_END();
  } else if (state.destroyed) {
    err = new ERR_STREAM_DESTROYED("write");
  }

  if (err) {
    process.nextTick(cb, err);
    errorOrDestroy(stream, err, true);
    return err;
  }
  state.pendingcb++;
  return writeOrBuffer(stream, state, chunk, encoding, cb);
}

Writable.prototype.write = function (chunk, encoding, cb) {
  return _write(this, chunk, encoding, cb) === true;
};

Writable.prototype.cork = function () {
  this._writableState.corked++;
};

Writable.prototype.uncork = function () {
  const state = this._writableState;

  if (state.corked) {
    state.corked--;

    if (!state.writing) clearBuffer(this, state);
  }
};

Writable.prototype.setDefaultEncoding = function setDefaultEncoding(encoding) {
  // node::ParseEncoding() requires lower case.
  if (typeof encoding === "string") encoding = encoding.toLowerCase();
  if (!Buffer.isEncoding(encoding)) throw new ERR_UNKNOWN_ENCODING(encoding);
  this._writableState.defaultEncoding = encoding;
  return this;
};

// If we're already writing something, then just put this in the queue, and wait our turn.
// Otherwise, call _write If we return false, then we need a drain event, so set that flag.
function writeOrBuffer(stream, state, chunk, encoding, callback) {
  const len = state.objectMode ? 1 : chunk.length;

  state.length += len;

  // stream._write resets state.length
  const ret = state.length < state.highWaterMark;
  // We must ensure that previous needDrain will not be reset to false.
  if (!ret) state.needDrain = true;

  if (state.writing || state.corked || state.errored || !state.constructed) {
    state.buffered.push({ chunk, encoding, callback });
    if (state.allBuffers && encoding !== "buffer") {
      state.allBuffers = false;
    }
    if (state.allNoop && callback !== nop) {
      state.allNoop = false;
    }
  } else {
    state.writelen = len;
    state.writecb = callback;
    state.writing = true;
    state.sync = true;
    stream._write(chunk, encoding, state.onwrite);
    state.sync = false;
  }

  // Return false if errored or destroyed in order to break any synchronous while(stream.write(data))
  // loops.
  return ret && !state.errored && !state.destroyed;
}

function doWrite(stream, state, writev, len, chunk, encoding, cb) {
  state.writelen = len;
  state.writecb = cb;
  state.writing = true;
  state.sync = true;
  if (state.destroyed) state.onwrite(new ERR_STREAM_DESTROYED("write"));
  else if (writev) stream._writev(chunk, state.onwrite);
  else stream._write(chunk, encoding, state.onwrite);
  state.sync = false;
}

function onwriteError(stream, state, er, cb) {
  --state.pendingcb;

  cb(er);
  // Ensure callbacks are invoked even when autoDestroy is not enabled. Passing `er` here doesn't
  // make sense since it's related to one specific write, not to the buffered writes.
  errorBuffer(state);
  // This can emit error, but error must always follow cb.
  errorOrDestroy(stream, er);
}

function onwrite(stream, er) {
  const state = stream._writableState;
  const sync = state.sync;
  const cb = state.writecb;

  if (typeof cb !== "function") {
    errorOrDestroy(stream, new ERR_MULTIPLE_CALLBACK());
    return;
  }

  state.writing = false;
  state.writecb = null;
  state.length -= state.writelen;
  state.writelen = 0;

  if (er) {
    // Avoid V8 leak, https://github.com/nodejs/node/pull/34103#issuecomment-652002364
    er.stack; // eslint-disable-line no-unused-expressions

    if (!state.errored) {
      state.errored = er;
    }

    // In case of duplex streams we need to notify the readable side of the error.
    if (stream._readableState && !stream._readableState.errored) {
      stream._readableState.errored = er;
    }

    if (sync) {
      process.nextTick(onwriteError, stream, state, er, cb);
    } else {
      onwriteError(stream, state, er, cb);
    }
  } else {
    if (state.buffered.length > state.bufferedIndex) {
      clearBuffer(stream, state);
    }

    if (sync) {
      // It is a common case that the callback passed to .write() is always the same. In that
      // case, we do not schedule a new nextTick(), but rather just increase a counter, to
      // improve performance and avoid memory allocations.
      if (state.afterWriteTickInfo !== null &&
          state.afterWriteTickInfo.cb === cb) {
        state.afterWriteTickInfo.count++;
      } else {
        state.afterWriteTickInfo = { count: 1, cb, stream, state };
        process.nextTick(afterWriteTick, state.afterWriteTickInfo);
      }
    } else {
      afterWrite(stream, state, 1, cb);
    }
  }
}

function afterWriteTick({ stream, state, count, cb }) {
  state.afterWriteTickInfo = null;
  return afterWrite(stream, state, count, cb);
}

function afterWrite(stream, state, count, cb) {
  const needDrain = !state.ending && !stream.destroyed && state.length === 0 &&
    state.needDrain;
  if (needDrain) {
    state.needDrain = false;
    stream.emit("drain");
  }

  while (count-- > 0) {
    state.pendingcb--;
    cb(null);
  }

  if (state.destroyed) {
    errorBuffer(state);
  }

  finishMaybe(stream, state);
}

// If there's something in the buffer waiting, then invoke callbacks.
function errorBuffer(state) {
  if (state.writing) {
    return;
  }

  for (let n = state.bufferedIndex; n < state.buffered.length; ++n) {
    const { chunk, callback } = state.buffered[n];
    const len = state.objectMode ? 1 : chunk.length;
    state.length -= len;
    callback(state.errored ?? new ERR_STREAM_DESTROYED("write"));
  }

  const onfinishCallbacks = state[kOnFinished].splice(0);
  for (let i = 0; i < onfinishCallbacks.length; i++) {
    onfinishCallbacks[i](state.errored ?? new ERR_STREAM_DESTROYED("end"));
  }

  resetBuffer(state);
}

// If there's something in the buffer waiting, then process it.
function clearBuffer(stream, state) {
  if (state.corked ||
      state.bufferProcessing ||
      state.destroyed ||
      !state.constructed) {
    return;
  }

  const { buffered, bufferedIndex, objectMode } = state;
  const bufferedLength = buffered.length - bufferedIndex;

  if (!bufferedLength) {
    return;
  }

  let i = bufferedIndex;

  state.bufferProcessing = true;
  if (bufferedLength > 1 && stream._writev) {
    state.pendingcb -= bufferedLength - 1;

    const callback = state.allNoop ? nop : (err) => {
      for (let n = i; n < buffered.length; ++n) {
        buffered[n].callback(err);
      }
    };
    // Make a copy of `buffered` if it's going to be used by `callback` above, since
    // `doWrite` will mutate the array.
    const chunks = state.allNoop && i === 0 ?
      buffered : buffered.slice(i);
    chunks.allBuffers = state.allBuffers;

    doWrite(stream, state, true, state.length, chunks, "", callback);

    resetBuffer(state);
  } else {
    do {
      const { chunk, encoding, callback } = buffered[i];
      buffered[i++] = null;
      const len = objectMode ? 1 : chunk.length;
      doWrite(stream, state, false, len, chunk, encoding, callback);
    } while (i < buffered.length && !state.writing);

    if (i === buffered.length) {
      resetBuffer(state);
    } else if (i > 256) {
      buffered.splice(0, i);
      state.bufferedIndex = 0;
    } else {
      state.bufferedIndex = i;
    }
  }
  state.bufferProcessing = false;
}

Writable.prototype._write = function (chunk, encoding, cb) {
  if (this._writev) {
    this._writev([{ chunk, encoding }], cb);
  } else {
    throw new ERR_METHOD_NOT_IMPLEMENTED("_write()");
  }
};

Writable.prototype._writev = null;

Writable.prototype.end = function (chunk, encoding, cb) {
  const state = this._writableState;

  if (typeof chunk === "function") {
    cb = chunk;
    chunk = null;
    encoding = null;
  } else if (typeof encoding === "function") {
    cb = encoding;
    encoding = null;
  }

  let err;

  if (chunk !== null && chunk !== undefined) {
    const ret = _write(this, chunk, encoding);
    if (ret instanceof Error) {
      err = ret;
    }
  }

  // .end() fully uncorks.
  if (state.corked) {
    state.corked = 1;
    this.uncork();
  }

  if (err) {
    // Do nothing...
  } else if (!state.errored && !state.ending) {
    // This is forgiving in terms of unnecessary calls to end() and can hide logic errors.
    // However, usually such errors are harmless and causing a hard error can be
    // disproportionately destructive. It is not always trivial for the user to determine
    // whether end() needs to be called.
    //
    // This is forgiving in terms of unnecessary calls to end() and can hide logic errors.

    state.ending = true;
    finishMaybe(this, state, true);
    state.ended = true;
  } else if (state.finished) {
    err = new ERR_STREAM_ALREADY_FINISHED("end");
  } else if (state.destroyed) {
    err = new ERR_STREAM_DESTROYED("end");
  }

  if (typeof cb === "function") {
    if (err || state.finished) {
      process.nextTick(cb, err);
    } else {
      state[kOnFinished].push(cb);
    }
  }

  return this;
};

function needFinish(state) {
  return (state.ending &&
          !state.destroyed &&
          state.constructed &&
          state.length === 0 &&
          !state.errored &&
          state.buffered.length === 0 &&
          !state.finished &&
          !state.writing &&
          !state.errorEmitted &&
          !state.closeEmitted);
}

function callFinal(stream, state) {
  let called = false;

  function onFinish(err) {
    if (called) {
      errorOrDestroy(stream, err ?? new ERR_MULTIPLE_CALLBACK());
      return;
    }
    called = true;

    state.pendingcb--;
    if (err) {
      const onfinishCallbacks = state[kOnFinished].splice(0);
      for (let i = 0; i < onfinishCallbacks.length; i++) {
        onfinishCallbacks[i](err);
      }
      errorOrDestroy(stream, err, state.sync);
    } else if (needFinish(state)) {
      state.prefinished = true;
      stream.emit("prefinish");
      // Backwards compat. Don't check state.sync here. Some streams assume 'finish' will be
      // emitted asynchronously relative to _final callback.
      state.pendingcb++;
      process.nextTick(finish, stream, state);
    }
  }

  state.sync = true;
  state.pendingcb++;

  try {
    stream._final(onFinish);
  } catch (err) {
    onFinish(err);
  }

  state.sync = false;
}

function prefinish(stream, state) {
  if (!state.prefinished && !state.finalCalled) {
    if (typeof stream._final === "function" && !state.destroyed) {
      state.finalCalled = true;
      callFinal(stream, state);
    } else {
      state.prefinished = true;
      stream.emit("prefinish");
    }
  }
}

function finishMaybe(stream, state, sync) {
  if (needFinish(state)) {
    prefinish(stream, state);
    if (state.pendingcb === 0) {
      if (sync) {
        state.pendingcb++;
        process.nextTick((stream, state) => {
          if (needFinish(state)) {
            finish(stream, state);
          } else {
            state.pendingcb--;
          }
        }, stream, state);
      } else if (needFinish(state)) {
        state.pendingcb++;
        finish(stream, state);
      }
    }
  }
}

function finish(stream, state) {
  state.pendingcb--;
  state.finished = true;

  const onfinishCallbacks = state[kOnFinished].splice(0);
  for (let i = 0; i < onfinishCallbacks.length; i++) {
    onfinishCallbacks[i](null);
  }

  stream.emit("finish");

  if (state.autoDestroy) {
    // In case of duplex streams we need a way to detect if the readable side is ready for
    // autoDestroy as well.
    const rState = stream._readableState;
    const autoDestroy = !rState || (
      rState.autoDestroy &&
      // We don't expect the readable to ever 'end' if readable is explicitly set to false.
      (rState.endEmitted || rState.readable === false)
    );
    if (autoDestroy) {
      stream.destroy();
    }
  }
}

Object.defineProperties(Writable.prototype, {

  closed: {
    __proto__: null,
    get() {
      return this._writableState ? this._writableState.closed : false;
    },
  },

  destroyed: {
    __proto__: null,
    get() {
      return this._writableState ? this._writableState.destroyed : false;
    },
    set(value) {
      // Backward compatibility, the user is explicitly managing destroyed.
      if (this._writableState) {
        this._writableState.destroyed = value;
      }
    },
  },

  writable: {
    __proto__: null,
    get() {
      const w = this._writableState;
      // w.writable === false means that this is part of a Duplex stream where the writable side
      // was disabled upon construction. Compat. The user might manually disable writable side
      // through deprecated setter.
      return !!w && w.writable !== false && !w.destroyed && !w.errored &&
        !w.ending && !w.ended;
    },
    set(val) {
      // Backwards compatible.
      if (this._writableState) {
        this._writableState.writable = !!val;
      }
    },
  },

  writableFinished: {
    __proto__: null,
    get() {
      return this._writableState ? this._writableState.finished : false;
    },
  },

  writableObjectMode: {
    __proto__: null,
    get() {
      return this._writableState ? this._writableState.objectMode : false;
    },
  },

  writableBuffer: {
    __proto__: null,
    get() {
      return this._writableState && this._writableState.getBuffer();
    },
  },

  writableEnded: {
    __proto__: null,
    get() {
      return this._writableState ? this._writableState.ending : false;
    },
  },

  writableNeedDrain: {
    __proto__: null,
    get() {
      const wState = this._writableState;
      if (!wState) return false;
      return !wState.destroyed && !wState.ending && wState.needDrain;
    },
  },

  writableHighWaterMark: {
    __proto__: null,
    get() {
      return this._writableState && this._writableState.highWaterMark;
    },
  },

  writableCorked: {
    __proto__: null,
    get() {
      return this._writableState ? this._writableState.corked : 0;
    },
  },

  writableLength: {
    __proto__: null,
    get() {
      return this._writableState && this._writableState.length;
    },
  },

  errored: {
    __proto__: null,
    enumerable: false,
    get() {
      return this._writableState ? this._writableState.errored : null;
    },
  },

  writableAborted: {
    __proto__: null,
    enumerable: false,
    get: function () {
      return !!(
        this._writableState.writable !== false &&
        (this._writableState.destroyed || this._writableState.errored) &&
        !this._writableState.finished
      );
    },
  },
});

const destroy = destroyImpl;
Writable.prototype.destroy = function (err, cb) {
  const state = this._writableState;

  // Invoke pending callbacks.
  if (!state.destroyed &&
    (state.bufferedIndex < state.buffered.length ||
      state[kOnFinished].length)) {
    process.nextTick(errorBuffer, state);
  }

  destroy.call(this, err, cb);
  return this;
};

Writable.prototype._undestroy = undestroy;
Writable.prototype._destroy = function (err, cb) {
  cb(err);
};

Writable.prototype[EE.captureRejectionSymbol] = function (err) {
  this.destroy(err);
};

Writable.fromWeb = function (writableStream, options) {
  return newStreamWritableFromWritableStream(writableStream, options);
};

Writable.toWeb = function (streamWritable) {
  return newWritableStreamFromStreamWritable(streamWritable);
};

// ---- Duplex (lib/internal/streams/duplex.js) --------------------------------------------------

function Duplex(options) {
  if (!(this instanceof Duplex)) return new Duplex(options);

  Readable.call(this, options);
  Writable.call(this, options);

  if (options) {
    this.allowHalfOpen = options.allowHalfOpen !== false;

    if (options.readable === false) {
      this._readableState.readable = false;
      this._readableState.ended = true;
      this._readableState.endEmitted = true;
    }

    if (options.writable === false) {
      this._writableState.writable = false;
      this._writableState.ending = true;
      this._writableState.ended = true;
      this._writableState.finished = true;
    }
  } else {
    this.allowHalfOpen = true;
  }
}
Object.setPrototypeOf(Duplex.prototype, Readable.prototype);
Object.setPrototypeOf(Duplex, Readable);

{
  const keys = Object.keys(Writable.prototype);
  // Allow the keys array to be GC'ed.
  for (let i = 0; i < keys.length; i++) {
    const method = keys[i];
    if (!Duplex.prototype[method]) Duplex.prototype[method] = Writable.prototype[method];
  }
}

Object.defineProperties(Duplex.prototype, {
  writable: { __proto__: null, ...Object.getOwnPropertyDescriptor(Writable.prototype, "writable") },
  writableHighWaterMark: { __proto__: null, ...Object.getOwnPropertyDescriptor(Writable.prototype, "writableHighWaterMark") },
  writableObjectMode: { __proto__: null, ...Object.getOwnPropertyDescriptor(Writable.prototype, "writableObjectMode") },
  writableBuffer: { __proto__: null, ...Object.getOwnPropertyDescriptor(Writable.prototype, "writableBuffer") },
  writableLength: { __proto__: null, ...Object.getOwnPropertyDescriptor(Writable.prototype, "writableLength") },
  writableFinished: { __proto__: null, ...Object.getOwnPropertyDescriptor(Writable.prototype, "writableFinished") },
  writableCorked: { __proto__: null, ...Object.getOwnPropertyDescriptor(Writable.prototype, "writableCorked") },
  writableEnded: { __proto__: null, ...Object.getOwnPropertyDescriptor(Writable.prototype, "writableEnded") },
  writableNeedDrain: { __proto__: null, ...Object.getOwnPropertyDescriptor(Writable.prototype, "writableNeedDrain") },

  destroyed: {
    __proto__: null,
    get() {
      if (this._readableState === undefined ||
          this._writableState === undefined) {
        return false;
      }
      return this._readableState.destroyed && this._writableState.destroyed;
    },
    set(value) {
      // Backward compatibility, the user is explicitly managing destroyed.
      if (this._readableState && this._writableState) {
        this._readableState.destroyed = value;
        this._writableState.destroyed = value;
      }
    },
  },
});

let webStreamsAdapters;

// Lazy to avoid circular references
function lazyWebStreams() {
  if (webStreamsAdapters === undefined) webStreamsAdapters = webAdapters;
  return webStreamsAdapters;
}

Duplex.fromWeb = function (pair, options) {
  return lazyWebStreams().newStreamDuplexFromReadableWritablePair(pair, options);
};

Duplex.toWeb = function (duplex) {
  return lazyWebStreams().newReadableWritablePairFromDuplex(duplex);
};

Duplex.from = function (body) {
  return duplexify(body, "body");
};

// ---- Transform (lib/internal/streams/transform.js) --------------------------------------------

const kCallback = Symbol("kCallback");

function Transform(options) {
  if (!(this instanceof Transform)) return new Transform(options);

  // TODO (ronag): This should preferably always be applied but would be semver-major. Or even
  // better; make Transform a Readable with the Writable interface.
  const readableHighWaterMark = options ? getHighWaterMark(this, options, "readableHighWaterMark", true) : null;
  if (readableHighWaterMark === 0) {
    // A Duplex will buffer both on the writable and readable side while a Transform just wants to
    // buffer hwm number of elements. To avoid buffering twice we disable buffering on the writable
    // side.
    options = {
      ...options,
      highWaterMark: null,
      readableHighWaterMark,
      // TODO (ronag): 0 is not optimal since we have a "bug" where we check needDrain before
      // calling _write and not after. Refs: https://github.com/nodejs/node/pull/32887 Refs:
      // https://github.com/nodejs/node/pull/35941
      writableHighWaterMark: options.writableHighWaterMark || 0,
    };
  }

  Duplex.call(this, options);

  // We have implemented the _read method, and done the other things that are required for a
  // Readable. `readable.push()` doesn't need a user supplied `_read` method.
  this._readableState.sync = false;

  this[kCallback] = null;

  if (options) {
    if (typeof options.transform === "function") this._transform = options.transform;

    if (typeof options.flush === "function") this._flush = options.flush;
  }

  // When the writable side finishes, then flush out anything remaining. Backwards compat. Some
  // Transform streams incorrectly implement _final instead of or in addition to _flush. By using
  // 'prefinish' instead of implementing _final we continue supporting this unfortunate use case.
  this.on("prefinish", prefinishTransform);
}
Object.setPrototypeOf(Transform.prototype, Duplex.prototype);
Object.setPrototypeOf(Transform, Duplex);

function finalTransform(cb) {
  if (typeof this._flush === "function" && !this.destroyed) {
    this._flush((er, data) => {
      if (er) {
        if (cb) {
          cb(er);
        } else {
          this.destroy(er);
        }
        return;
      }

      if (data != null) {
        this.push(data);
      }
      this.push(null);
      if (cb) {
        cb();
      }
    });
  } else {
    this.push(null);
    if (cb) {
      cb();
    }
  }
}

function prefinishTransform() {
  if (this._final !== finalTransform) {
    finalTransform.call(this);
  }
}

Transform.prototype._final = finalTransform;

Transform.prototype._transform = function (chunk, encoding, callback) {
  throw new ERR_METHOD_NOT_IMPLEMENTED("_transform()");
};

Transform.prototype._write = function (chunk, encoding, callback) {
  const rState = this._readableState;
  const wState = this._writableState;
  const length = rState.length;

  this._transform(chunk, encoding, (err, val) => {
    if (err) {
      callback(err);
      return;
    }

    if (val != null) {
      this.push(val);
    }

    if (
      wState.ended || // Backwards compat.
      length === rState.length || // Backwards compat.
      rState.length < rState.highWaterMark
    ) {
      callback();
    } else {
      this[kCallback] = callback;
    }
  });
};

Transform.prototype._read = function () {
  if (this[kCallback]) {
    const callback = this[kCallback];
    this[kCallback] = null;
    callback();
  }
};

// ---- PassThrough (lib/internal/streams/passthrough.js) ----------------------------------------

function PassThrough(options) {
  if (!(this instanceof PassThrough)) return new PassThrough(options);

  Transform.call(this, options);
}
Object.setPrototypeOf(PassThrough.prototype, Transform.prototype);
Object.setPrototypeOf(PassThrough, Transform);

PassThrough.prototype._transform = function (chunk, encoding, cb) {
  cb(null, chunk);
};

// ---- from (lib/internal/streams/from.js) ------------------------------------------------------

function from(Readable, iterable, opts) {
  let iterator;
  if (typeof iterable === "string" || iterable instanceof Buffer) {
    return new Readable({
      objectMode: true,
      ...opts,
      read() {
        this.push(iterable);
        this.push(null);
      },
    });
  }

  let isAsync;
  if (iterable && iterable[Symbol.asyncIterator]) {
    isAsync = true;
    iterator = iterable[Symbol.asyncIterator]();
  } else if (iterable && iterable[Symbol.iterator]) {
    isAsync = false;
    iterator = iterable[Symbol.iterator]();
  } else {
    throw new ERR_INVALID_ARG_TYPE("iterable", ["Iterable"], iterable);
  }

  const readable = new Readable({
    objectMode: true,
    highWaterMark: 1,
    // TODO(ronag): What options should be allowed?
    ...opts,
  });

  // Flag to protect against _read being called while async iterator is already being processed.
  let reading = false;

  readable._read = function () {
    if (!reading) {
      reading = true;
      next();
    }
  };

  readable._destroy = function (error, cb) {
    close(error).then(
      () => process.nextTick(cb, error), // nextTick is here in case cb throws
      (e) => process.nextTick(cb, e || error),
    );
  };

  async function close(error) {
    const hadError = (error !== undefined) && (error !== null);
    const hasThrow = typeof iterator.throw === "function";
    if (hadError && hasThrow) {
      const { value, done } = await iterator.throw(error);
      await value;
      if (done) {
        return;
      }
    }
    if (typeof iterator.return === "function") {
      const { value } = await iterator.return();
      await value;
    }
  }

  async function next() {
    for (;;) {
      try {
        const { value, done } = isAsync ?
          await iterator.next() :
          iterator.next();

        if (done) {
          readable.push(null);
        } else {
          const res = (value &&
            typeof value.then === "function") ?
            await value :
            value;
          if (res === null) {
            reading = false;
            throw new ERR_STREAM_NULL_VALUES();
          } else if (readable.push(res)) {
            continue;
          } else {
            reading = false;
          }
        }
      } catch (err) {
        readable.destroy(err);
      }
      break;
    }
  }
  return readable;
}

// ---- duplexify (lib/internal/streams/duplexify.js) --------------------------------------------

function isBlob(b) {
  return typeof Blob !== "undefined" && b instanceof Blob;
}

// This is needed for pre node 17.
class Duplexify extends Duplex {
  constructor(options) {
    super(options);

    // https://github.com/nodejs/node/pull/34385

    if (options?.readable === false) {
      this._readableState.readable = false;
      this._readableState.ended = true;
      this._readableState.endEmitted = true;
    }

    if (options?.writable === false) {
      this._writableState.writable = false;
      this._writableState.ending = true;
      this._writableState.ended = true;
      this._writableState.finished = true;
    }
  }
}

function duplexify(body, name) {
  if (isDuplexNodeStream(body)) {
    return body;
  }

  if (isReadableNodeStream(body)) {
    return _duplexify({ readable: body });
  }

  if (isWritableNodeStream(body)) {
    return _duplexify({ writable: body });
  }

  if (isNodeStream(body)) {
    return _duplexify({ writable: false, readable: false });
  }

  if (isReadableStream(body)) {
    return _duplexify({ readable: Readable.fromWeb(body) });
  }

  if (isWritableStream(body)) {
    return _duplexify({ writable: Writable.fromWeb(body) });
  }

  if (typeof body === "function") {
    const { value, write, final, destroy } = fromAsyncGen(body);

    if (isIterable(value)) {
      return from(Duplexify, value, {
        // TODO (ronag): highWaterMark?
        objectMode: true,
        write,
        final,
        destroy,
      });
    }

    const then = value?.then;
    if (typeof then === "function") {
      let d;

      const promise = Reflect.apply(then, value, [
        (val) => {
          if (val != null) {
            throw new ERR_INVALID_RETURN_VALUE("nully", "body", val);
          }
        },
        (err) => {
          destroyer(d, err);
        },
      ]);

      return d = new Duplexify({
        // TODO (ronag): highWaterMark?
        objectMode: true,
        readable: false,
        write,
        final(cb) {
          final(async () => {
            try {
              await promise;
              process.nextTick(cb, null);
            } catch (err) {
              process.nextTick(cb, err);
            }
          });
        },
        destroy,
      });
    }

    throw new ERR_INVALID_RETURN_VALUE("Iterable, AsyncIterable or AsyncFunction", name, value);
  }

  if (isBlob(body)) {
    return duplexify(body.arrayBuffer());
  }

  if (isIterable(body)) {
    return from(Duplexify, body, {
      // TODO (ronag): highWaterMark?
      objectMode: true,
      writable: false,
    });
  }

  if (
    isReadableStream(body?.readable) &&
    isWritableStream(body?.writable)
  ) {
    return Duplexify.fromWeb(body);
  }

  if (
    typeof body?.writable === "object" ||
    typeof body?.readable === "object"
  ) {
    const readable = body?.readable ?
      isReadableNodeStream(body?.readable) ? body?.readable :
        duplexify(body.readable) :
      undefined;

    const writable = body?.writable ?
      isWritableNodeStream(body?.writable) ? body?.writable :
        duplexify(body.writable) :
      undefined;

    return _duplexify({ readable, writable });
  }

  const then = body?.then;
  if (typeof then === "function") {
    let d;

    Reflect.apply(then, body, [
      (val) => {
        if (val != null) {
          d.push(val);
        }
        d.push(null);
      },
      (err) => {
        destroyer(d, err);
      },
    ]);

    return d = new Duplexify({
      objectMode: true,
      writable: false,
      read() {},
    });
  }

  throw new ERR_INVALID_ARG_TYPE(
    name,
    ["Blob", "ReadableStream", "WritableStream", "Stream", "Iterable", "AsyncIterable",
      "Function", "{ readable, writable } pair", "Promise"],
    body);
}

function fromAsyncGen(fn) {
  let { promise, resolve } = createDeferredPromise();
  const ac = new AbortController();
  const signal = ac.signal;
  const value = fn(async function* () {
    while (true) {
      const _promise = promise;
      promise = null;
      const { chunk, done, cb } = await _promise;
      process.nextTick(cb);
      if (done) return;
      if (signal.aborted) throw new AbortError(undefined, { cause: signal.reason });
      ({ promise, resolve } = createDeferredPromise());
      yield chunk;
    }
  }(), { signal });

  return {
    value,
    write(chunk, encoding, cb) {
      const _resolve = resolve;
      resolve = null;
      _resolve({ chunk, done: false, cb });
    },
    final(cb) {
      const _resolve = resolve;
      resolve = null;
      _resolve({ done: true, cb });
    },
    destroy(err, cb) {
      ac.abort();
      cb(err);
    },
  };
}

function createDeferredPromise() {
  let resolve;
  let reject;
  const promise = new Promise((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

function _duplexify(pair) {
  const r = pair.readable && typeof pair.readable.read !== "function" ?
    Readable.wrap(pair.readable) : pair.readable;
  const w = pair.writable;

  let readable = !!isReadable(r);
  let writable = !!isWritable(w);

  let ondrain;
  let onfinish;
  let onreadable;
  let onclose;
  let d;

  function onfinished(err) {
    const cb = onclose;
    onclose = null;

    if (cb) {
      cb(err);
    } else if (err) {
      d.destroy(err);
    }
  }

  // TODO(ronag): Avoid double buffering. Implement Writable/Readable/Duplex methods directly
  // instead of using a Duplex.

  d = new Duplexify({
    // TODO (ronag): highWaterMark?
    readableObjectMode: !!r?.readableObjectMode,
    writableObjectMode: !!w?.writableObjectMode,
    readable,
    writable,
  });

  if (writable) {
    eos(w, (err) => {
      writable = false;
      if (err) {
        destroyer(r, err);
      }
      onfinished(err);
    });

    d._write = function (chunk, encoding, callback) {
      if (w.write(chunk, encoding)) {
        callback();
      } else {
        ondrain = callback;
      }
    };

    d._final = function (callback) {
      w.end();
      onfinish = callback;
    };

    w.on("drain", function () {
      if (ondrain) {
        const cb = ondrain;
        ondrain = null;
        cb();
      }
    });

    w.on("finish", function () {
      if (onfinish) {
        const cb = onfinish;
        onfinish = null;
        cb();
      }
    });
  }

  if (readable) {
    eos(r, (err) => {
      readable = false;
      if (err) {
        destroyer(r, err);
      }
      onfinished(err);
    });

    r.on("readable", function () {
      if (onreadable) {
        const cb = onreadable;
        onreadable = null;
        cb();
      }
    });

    r.on("end", function () {
      d.push(null);
    });

    d._read = function () {
      while (true) {
        const buf = r.read();

        if (buf === null) {
          onreadable = d._read;
          return;
        }

        if (!d.push(buf)) {
          return;
        }
      }
    };
  }

  d._destroy = function (err, callback) {
    if (!err && onclose !== null) {
      err = new AbortError();
    }

    onreadable = null;
    ondrain = null;
    onfinish = null;

    if (onclose === null) {
      callback(err);
    } else {
      onclose = callback;
      destroyer(w, err);
      destroyer(r, err);
    }
  };

  return d;
}

// ---- pipeline (lib/internal/streams/pipeline.js) ----------------------------------------------

function destroyerPipeline(stream, reading, writing) {
  let finished = false;
  stream.on("close", () => {
    finished = true;
  });

  const cleanup = eos(stream, { readable: reading, writable: writing }, (err) => {
    finished = !err;
  });

  return {
    destroy: (err) => {
      if (finished) return;
      finished = true;
      destroyer(stream, err || new ERR_STREAM_DESTROYED("pipe"));
    },
    cleanup,
  };
}

function popCallback(streams) {
  // Streams should never be an empty array. It should always contain at least a single stream.
  // Therefore optimize for the average case instead of checking for length === 0 as well.
  validateFunction(streams[streams.length - 1], "streams[stream.length - 1]");
  return streams.pop();
}

function makeAsyncIterable(val) {
  if (isIterable(val)) {
    return val;
  } else if (isReadableNodeStream(val)) {
    // Legacy streams are not Iterable.
    return fromReadable(val);
  }
  throw new ERR_INVALID_ARG_TYPE("val", ["Readable", "Iterable", "AsyncIterable"], val);
}

async function* fromReadable(val) {
  yield* Readable.prototype[Symbol.asyncIterator].call(val);
}

async function pumpToNode(iterable, writable, finish, { end }) {
  let error;
  let onresolve = null;

  const resume = (err) => {
    if (err) {
      error = err;
    }

    if (onresolve) {
      const callback = onresolve;
      onresolve = null;
      callback();
    }
  };

  const wait = () => new Promise((resolve, reject) => {
    if (error) {
      reject(error);
    } else {
      onresolve = () => {
        if (error) {
          reject(error);
        } else {
          resolve();
        }
      };
    }
  });

  writable.on("drain", resume);
  const cleanup = eos(writable, { readable: false }, resume);

  try {
    if (writable.writableNeedDrain) {
      await wait();
    }

    for await (const chunk of iterable) {
      if (!writable.write(chunk)) {
        await wait();
      }
    }

    if (end) {
      writable.end();
      await wait();
    }

    finish();
  } catch (err) {
    finish(error !== err ? aggregateTwoErrors(error, err) : err);
  } finally {
    cleanup();
    writable.off("drain", resume);
  }
}

async function pumpToWeb(readable, writable, finish, { end }) {
  if (isTransformStream(writable)) {
    writable = writable.writable;
  }
  // https://streams.spec.whatwg.org/#example-manual-write-with-backpressure
  const writer = writable.getWriter();
  try {
    for await (const chunk of readable) {
      await writer.ready;
      writer.write(chunk).catch(() => {});
    }

    await writer.ready;

    if (end) {
      await writer.close();
    }

    finish();
  } catch (err) {
    try {
      await writer.abort(err);
      finish(err);
    } catch (err) {
      finish(err);
    }
  }
}

function pipeline(...streams) {
  return pipelineImpl(streams, once(popCallback(streams)));
}

function pipelineImpl(streams, callback, opts) {
  if (streams.length === 1 && Array.isArray(streams[0])) {
    streams = streams[0];
  }

  if (streams.length < 2) {
    throw new ERR_MISSING_ARGS("streams");
  }

  const ac = new AbortController();
  const signal = ac.signal;
  const outerSignal = opts?.signal;

  // Need to cleanup event listeners if last stream is readable
  // https://github.com/nodejs/node/issues/35452
  const lastStreamCleanup = [];

  validateAbortSignal(outerSignal, "options.signal");

  function abort() {
    finishImpl(new AbortError());
  }

  let disposable;
  if (outerSignal) {
    disposable = EE.addAbortListener(outerSignal, abort);
  }

  let error;
  let value;
  const destroys = [];

  let finishCount = 0;

  function finish(err) {
    finishImpl(err, --finishCount === 0);
  }

  function finishImpl(err, final) {
    if (err && (!error || error.code === "ERR_STREAM_PREMATURE_CLOSE")) {
      error = err;
    }

    if (!error && !final) {
      return;
    }

    while (destroys.length) {
      destroys.shift()(error);
    }

    disposable?.[SymbolDispose]();
    ac.abort();

    if (final) {
      if (!error) {
        lastStreamCleanup.forEach((fn) => fn());
      }
      process.nextTick(callback, error, value);
    }
  }

  let ret;
  for (let i = 0; i < streams.length; i++) {
    const stream = streams[i];
    const reading = i < streams.length - 1;
    const writing = i > 0;
    const end = reading || opts?.end !== false;
    const isLastStream = i === streams.length - 1;

    if (isNodeStream(stream)) {
      if (end) {
        const { destroy, cleanup } = destroyerPipeline(stream, reading, writing);
        destroys.push(destroy);

        if (isReadable(stream) && isLastStream) {
          lastStreamCleanup.push(cleanup);
        }
      }

      // Catch stream errors that occur after pipe/pump has completed.
      function onError(err) {
        if (
          err &&
          err.name !== "AbortError" &&
          err.code !== "ERR_STREAM_PREMATURE_CLOSE"
        ) {
          finish(err);
        }
      }
      stream.on("error", onError);
      if (isReadable(stream) && isLastStream) {
        lastStreamCleanup.push(() => {
          stream.removeListener("error", onError);
        });
      }
    }

    if (i === 0) {
      if (typeof stream === "function") {
        ret = stream({ signal });
        if (!isIterable(ret)) {
          throw new ERR_INVALID_RETURN_VALUE("Iterable, AsyncIterable or Stream", "source", ret);
        }
      } else if (isIterable(stream) || isReadableNodeStream(stream) || isTransformStream(stream)) {
        ret = stream;
      } else {
        ret = Duplex.from(stream);
      }
    } else if (typeof stream === "function") {
      if (isTransformStream(ret)) {
        ret = makeAsyncIterable(ret?.readable);
      } else {
        ret = makeAsyncIterable(ret);
      }
      ret = stream(ret, { signal });

      if (reading) {
        if (!isIterable(ret, true)) {
          throw new ERR_INVALID_RETURN_VALUE("AsyncIterable", `transform[${i - 1}]`, ret);
        }
      } else {
        // If the last argument to pipeline is not a stream we must create a proxy stream so that
        // pipeline(...) always returns a stream which can be further composed through `.pipe(stream)`.
        const pt = new PassThrough({
          objectMode: true,
        });

        // Handle Promises/A+ spec, `then` could be a getter that throws on second use.
        const then = ret?.then;
        if (typeof then === "function") {
          finishCount++;
          then.call(ret,
            (val) => {
              value = val;
              if (val != null) {
                pt.write(val);
              }
              if (end) {
                pt.end();
              }
              process.nextTick(finish);
            }, (err) => {
              pt.destroy(err);
              process.nextTick(finish, err);
            },
          );
        } else if (isIterable(ret, true)) {
          finishCount++;
          pumpToNode(ret, pt, finish, { end });
        } else if (isReadableStream(ret) || isTransformStream(ret)) {
          const toRead = ret.readable || ret;
          finishCount++;
          pumpToNode(toRead, pt, finish, { end });
        } else {
          throw new ERR_INVALID_RETURN_VALUE("AsyncIterable or Promise", "destination", ret);
        }

        ret = pt;

        const { destroy, cleanup } = destroyerPipeline(ret, false, true);
        destroys.push(destroy);
        if (isLastStream) {
          lastStreamCleanup.push(cleanup);
        }
      }
    } else if (isNodeStream(stream)) {
      if (isReadableNodeStream(ret)) {
        finishCount += 2;
        const cleanup = pipe(ret, stream, finish, { end });
        if (isReadable(stream) && isLastStream) {
          lastStreamCleanup.push(cleanup);
        }
      } else if (isTransformStream(ret) || isReadableStream(ret)) {
        const toRead = ret.readable || ret;
        finishCount++;
        pumpToNode(toRead, stream, finish, { end });
      } else if (isIterable(ret)) {
        finishCount++;
        pumpToNode(ret, stream, finish, { end });
      } else {
        throw new ERR_INVALID_ARG_TYPE("val",
          ["Readable", "Iterable", "AsyncIterable", "ReadableStream", "TransformStream"], ret);
      }
      ret = stream;
    } else if (isWebStream(stream)) {
      if (isReadableNodeStream(ret)) {
        finishCount++;
        pumpToWeb(makeAsyncIterable(ret), stream, finish, { end });
      } else if (isReadableStream(ret) || isIterable(ret)) {
        finishCount++;
        pumpToWeb(ret, stream, finish, { end });
      } else if (isTransformStream(ret)) {
        finishCount++;
        pumpToWeb(ret.readable, stream, finish, { end });
      } else {
        throw new ERR_INVALID_ARG_TYPE("val",
          ["Readable", "Iterable", "AsyncIterable", "ReadableStream", "TransformStream"], ret);
      }
      ret = stream;
    } else {
      ret = Duplex.from(stream);
    }
  }

  if (signal?.aborted || outerSignal?.aborted) {
    process.nextTick(abort);
  }

  return ret;
}

function pipe(src, dst, finish, { end }) {
  let ended = false;
  dst.on("close", () => {
    if (!ended) {
      // Finish if the destination closes before the source has completed.
      finish(new ERR_STREAM_PREMATURE_CLOSE());
    }
  });

  src.pipe(dst, { end: false }); // If end is true we already will have a listener to end dst.

  if (end) {
    // Compat. Before node v10.12.0 stdio used to throw an error so pipe() did/does not end()
    // stdio. Now they allow it but "secretly" don't close the underlying fd.

    function endFn() {
      ended = true;
      dst.end();
    }

    if (isReadableFinished(src)) { // End the destination if the source has already ended.
      process.nextTick(endFn);
    } else {
      src.once("end", endFn);
    }

    eos(src, { readable: true, writable: false }, (err) => {
      const rState = src._readableState;
      if (
        err &&
        err.code === "ERR_STREAM_PREMATURE_CLOSE" &&
        (rState && rState.ended && !rState.errored && !rState.errorEmitted)
      ) {
        // Some readable streams will emit 'close' before 'end'. However, since this is on the
        // readable side 'end' should still be emitted if the stream has been ended and no error
        // emitted. This should be allowed in favor of backwards compatibility. Since the stream is
        // piped to a destination this should not result in any observable difference. We don't
        // need to check if this is a writable premature close since eos will only fail with
        // premature close on the reading side for duplex streams.
        src
          .once("end", finish)
          .once("error", finish);
      } else {
        finish(err);
      }
    });
  } else {
    finish();
  }
  return eos(dst, { readable: false, writable: true }, finish);
}

// ---- compose (lib/internal/streams/compose.js) ------------------------------------------------

function compose(...streams) {
  if (streams.length === 0) {
    throw new ERR_MISSING_ARGS("streams");
  }

  if (streams.length === 1) {
    return Duplex.from(streams[0]);
  }

  const orgStreams = [...streams];

  if (typeof streams[0] === "function") {
    streams[0] = Duplex.from(streams[0]);
  }

  if (typeof streams[streams.length - 1] === "function") {
    const idx = streams.length - 1;
    streams[idx] = Duplex.from(streams[idx]);
  }

  for (let n = 0; n < streams.length; ++n) {
    if (!isNodeStream(streams[n]) && !isWebStream(streams[n])) {
      // TODO(ronag): Add checks for non streams.
      continue;
    }
    if (
      n < streams.length - 1 &&
      !(
        isReadable(streams[n]) ||
        isReadableStream(streams[n]) ||
        isTransformStream(streams[n])
      )
    ) {
      throw new ERR_INVALID_ARG_VALUE(
        `streams[${n}]`,
        orgStreams[n],
        "must be readable",
      );
    }
    if (n > 0 && !(isWritable(streams[n]) || isWritableStream(streams[n]) || isTransformStream(streams[n]))) {
      throw new ERR_INVALID_ARG_VALUE(
        `streams[${n}]`,
        orgStreams[n],
        "must be writable",
      );
    }
  }

  let ondrain;
  let onfinish;
  let onreadable;
  let onclose;
  let d;

  function onfinished(err) {
    const cb = onclose;
    onclose = null;

    if (cb) {
      cb(err);
    } else if (err) {
      d.destroy(err);
    } else if (!readable && !writable) {
      d.destroy();
    }
  }

  const head = streams[0];
  const tail = pipeline(streams, onfinished);

  const writable = !!(isWritable(head) || isWritableStream(head) || isTransformStream(head));
  const readable = !!(isReadable(tail) || isReadableStream(tail) || isTransformStream(tail));

  // TODO(ronag): Avoid double buffering. Implement Writable/Readable/Duplex methods directly
  // instead of using a Duplex.
  d = new Duplex({
    // TODO (ronag): highWaterMark?
    writableObjectMode: !!head?.writableObjectMode,
    readableObjectMode: !!tail?.readableObjectMode,
    writable,
    readable,
  });

  if (writable) {
    if (isNodeStream(head)) {
      d._write = function (chunk, encoding, callback) {
        if (head.write(chunk, encoding)) {
          callback();
        } else {
          ondrain = callback;
        }
      };

      d._final = function (callback) {
        head.end();
        onfinish = callback;
      };

      head.on("drain", function () {
        if (ondrain) {
          const cb = ondrain;
          ondrain = null;
          cb();
        }
      });
    } else if (isWebStream(head)) {
      const writable = isTransformStream(head) ? head.writable : head;
      const writer = writable.getWriter();

      d._write = async function (chunk, encoding, callback) {
        try {
          await writer.ready;
          writer.write(chunk).catch(() => {});
          callback();
        } catch (err) {
          callback(err);
        }
      };

      d._final = async function (callback) {
        try {
          await writer.ready;
          writer.close().catch(() => {});
          onfinish = callback;
        } catch (err) {
          callback(err);
        }
      };
    }

    const toRead = isTransformStream(tail) ? tail.readable : tail;

    eos(toRead, () => {
      if (onfinish) {
        const cb = onfinish;
        onfinish = null;
        cb();
      }
    });
  }

  if (readable) {
    if (isNodeStream(tail)) {
      tail.on("readable", function () {
        if (onreadable) {
          const cb = onreadable;
          onreadable = null;
          cb();
        }
      });

      tail.on("end", function () {
        d.push(null);
      });

      d._read = function () {
        while (true) {
          const buf = tail.read();
          if (buf === null) {
            onreadable = d._read;
            return;
          }

          if (!d.push(buf)) {
            return;
          }
        }
      };
    } else if (isWebStream(tail)) {
      const readable = isTransformStream(tail) ? tail.readable : tail;
      const reader = readable.getReader();
      d._read = async function () {
        while (true) {
          try {
            const { value, done } = await reader.read();

            if (!d.push(value)) {
              return;
            }

            if (done) {
              d.push(null);
              return;
            }
          } catch {
            return;
          }
        }
      };
    }
  }

  d._destroy = function (err, callback) {
    if (!err && onclose !== null) {
      err = new AbortError();
    }

    onreadable = null;
    ondrain = null;
    onfinish = null;

    if (onclose === null) {
      callback(err);
    } else {
      onclose = callback;
      if (isNodeStream(tail)) {
        destroyer(tail, err);
      }
    }
  };

  return d;
}

// ---- operators (lib/internal/streams/operators.js) --------------------------------------------

const kWeakHandler = Symbol("kWeak");
const kResistStopPropagation = Symbol("kResistStopPropagation");
const kEmpty = Symbol("kEmpty");
const kEof = Symbol("kEof");

function composeOp(stream, options) {
  if (options != null) {
    validateObject(options, "options");
  }
  if (options?.signal != null) {
    validateAbortSignal(options.signal, "options.signal");
  }

  if (isNodeStream(stream) && !isWritable(stream)) {
    throw new ERR_INVALID_ARG_VALUE("stream", stream, "must be writable");
  }

  const composedStream = compose(this, stream);

  if (options?.signal) {
    // Not validating as we already validated before
    addAbortSignalNoValidate(
      options.signal,
      composedStream,
    );
  }

  return composedStream;
}

function map(fn, options) {
  if (typeof fn !== "function") {
    throw new ERR_INVALID_ARG_TYPE("fn", ["Function", "AsyncFunction"], fn);
  }
  if (options != null) {
    validateObject(options, "options");
  }
  if (options?.signal != null) {
    validateAbortSignal(options.signal, "options.signal");
  }

  let concurrency = 1;
  if (options?.concurrency != null) {
    concurrency = Math.floor(options.concurrency);
  }

  let highWaterMark = concurrency - 1;
  if (options?.highWaterMark != null) {
    highWaterMark = Math.floor(options.highWaterMark);
  }

  validateInteger(concurrency, "options.concurrency", 1);
  validateInteger(highWaterMark, "options.highWaterMark", 0);

  highWaterMark += concurrency;

  return async function* map() {
    const ac = new AbortController();
    const stream = this;
    const queue = [];
    const signalOpt = { signal: ac.signal };

    let next;
    let resume;
    let done = false;
    let cnt = 0;

    function onCatch() {
      done = true;
      afterItemProcessed();
    }

    function afterItemProcessed() {
      cnt -= 1;
      maybeResume();
    }

    function maybeResume() {
      if (
        resume &&
        !done &&
        cnt < concurrency &&
        queue.length < highWaterMark
      ) {
        resume();
        resume = null;
      }
    }

    async function pump() {
      try {
        for await (let val of stream) {
          if (done) {
            return;
          }

          if (ac.signal.aborted) {
            throw new AbortError();
          }

          try {
            val = fn(val, signalOpt);

            if (val === kEmpty) {
              continue;
            }

            val = Promise.resolve(val);
          } catch (err) {
            val = Promise.reject(err);
          }

          cnt += 1;

          val.then(afterItemProcessed, onCatch);

          queue.push(val);
          if (next) {
            next();
            next = null;
          }

          if (!done && (queue.length >= highWaterMark || cnt >= concurrency)) {
            await new Promise((resolve) => {
              resume = resolve;
            });
          }
        }
        queue.push(kEof);
      } catch (err) {
        const val = Promise.reject(err);
        val.then(afterItemProcessed, onCatch);
        queue.push(val);
      } finally {
        done = true;
        if (next) {
          next();
          next = null;
        }
      }
    }

    const outerSignal = options?.signal;
    let abortListener;
    if (outerSignal) {
      abortListener = () => ac.abort();
      if (outerSignal.aborted) ac.abort();
      else outerSignal.addEventListener("abort", abortListener);
    }

    pump();

    try {
      while (true) {
        while (queue.length > 0) {
          const val = await queue[0];

          if (val === kEof) {
            return;
          }

          if (ac.signal.aborted) {
            throw new AbortError();
          }

          if (val !== kEmpty) {
            yield val;
          }

          queue.shift();
          maybeResume();
        }

        await new Promise((resolve) => {
          next = resolve;
        });
      }
    } finally {
      done = true;
      if (resume) {
        resume();
        resume = null;
      }
      if (outerSignal && abortListener) outerSignal.removeEventListener("abort", abortListener);
    }
  }.call(this);
}

function asIndexedPairs(options = undefined) {
  if (options != null) {
    validateObject(options, "options");
  }
  if (options?.signal != null) {
    validateAbortSignal(options.signal, "options.signal");
  }

  return async function* asIndexedPairs() {
    let index = 0;
    for await (const val of this) {
      if (options?.signal?.aborted) {
        throw new AbortError(undefined, { cause: options.signal.reason });
      }
      yield [index++, val];
    }
  }.call(this);
}

async function some(fn, options = undefined) {
  for await (const unused of filter.call(this, fn, options)) { // eslint-disable-line no-unused-vars
    return true;
  }
  return false;
}

async function every(fn, options = undefined) {
  if (typeof fn !== "function") {
    throw new ERR_INVALID_ARG_TYPE("fn", ["Function", "AsyncFunction"], fn);
  }
  // https://en.wikipedia.org/wiki/De_Morgan%27s_laws
  return !(await some.call(this, async (...args) => {
    return !(await fn(...args));
  }, options));
}

async function find(fn, options) {
  for await (const result of filter.call(this, fn, options)) {
    return result;
  }
  return undefined;
}

async function forEach(fn, options) {
  if (typeof fn !== "function") {
    throw new ERR_INVALID_ARG_TYPE("fn", ["Function", "AsyncFunction"], fn);
  }
  async function forEachFn(value, options) {
    await fn(value, options);
    return kEmpty;
  }
  // eslint-disable-next-line no-unused-vars
  for await (const unused of map.call(this, forEachFn, options));
}

function filter(fn, options) {
  if (typeof fn !== "function") {
    throw new ERR_INVALID_ARG_TYPE("fn", ["Function", "AsyncFunction"], fn);
  }
  async function filterFn(value, options) {
    if (await fn(value, options)) {
      return value;
    }
    return kEmpty;
  }
  return map.call(this, filterFn, options);
}

// Specific to provide better error to reduce since the argument is only missing if the stream
// has no items in it - but the code is still appropriate
class ReduceAwareErrMissingArgs extends ERR_MISSING_ARGS {
  constructor() {
    super("reduce");
    this.message = "Reduce of an empty stream requires an initial value";
  }
}

async function reduce(reducer, initialValue, options) {
  if (typeof reducer !== "function") {
    throw new ERR_INVALID_ARG_TYPE("reducer", ["Function", "AsyncFunction"], reducer);
  }
  if (options != null) {
    validateObject(options, "options");
  }
  if (options?.signal != null) {
    validateAbortSignal(options.signal, "options.signal");
  }

  let hasInitialValue = arguments.length > 1;
  if (options?.signal?.aborted) {
    const err = new AbortError(undefined, { cause: options.signal.reason });
    this.once("error", () => {}); // The error is already propagated
    await finished(this.destroy(err));
    throw err;
  }
  const ac = new AbortController();
  const signal = ac.signal;
  let gotAnyItemFromStream = false;
  let abortListener;
  if (options?.signal) {
    abortListener = () => ac.abort();
    options.signal.addEventListener("abort", abortListener, { once: true });
  }
  try {
    for await (const value of this) {
      gotAnyItemFromStream = true;
      if (options?.signal?.aborted) {
        throw new AbortError();
      }
      if (!hasInitialValue) {
        initialValue = value;
        hasInitialValue = true;
      } else {
        initialValue = await reducer(initialValue, value, { signal });
      }
    }
    if (!gotAnyItemFromStream && !hasInitialValue) {
      throw new ReduceAwareErrMissingArgs();
    }
  } finally {
    ac.abort();
    if (abortListener) options.signal.removeEventListener("abort", abortListener);
  }
  return initialValue;
}

async function toArray(options) {
  if (options != null) {
    validateObject(options, "options");
  }
  if (options?.signal != null) {
    validateAbortSignal(options.signal, "options.signal");
  }

  const result = [];
  for await (const val of this) {
    if (options?.signal?.aborted) {
      throw new AbortError(undefined, { cause: options.signal.reason });
    }
    result.push(val);
  }
  return result;
}

function flatMap(fn, options) {
  const values = map.call(this, fn, options);
  return async function* flatMap() {
    for await (const val of values) {
      yield* val;
    }
  }.call(this);
}

function toIntegerOrInfinity(num) {
  // We coerce here to align with the spec
  // https://github.com/tc39/proposal-iterator-helpers/issues/169
  num = Number(num);
  if (Number.isNaN(num)) {
    return 0;
  }
  if (num < 0) {
    throw new ERR_OUT_OF_RANGE("number", ">= 0", num);
  }
  return num;
}

function drop(number, options = undefined) {
  if (options != null) {
    validateObject(options, "options");
  }
  if (options?.signal != null) {
    validateAbortSignal(options.signal, "options.signal");
  }

  number = toIntegerOrInfinity(number);
  return async function* drop() {
    if (options?.signal?.aborted) {
      throw new AbortError();
    }
    for await (const val of this) {
      if (options?.signal?.aborted) {
        throw new AbortError();
      }
      if (number-- <= 0) {
        yield val;
      }
    }
  }.call(this);
}

function take(number, options = undefined) {
  if (options != null) {
    validateObject(options, "options");
  }
  if (options?.signal != null) {
    validateAbortSignal(options.signal, "options.signal");
  }

  number = toIntegerOrInfinity(number);
  return async function* take() {
    if (options?.signal?.aborted) {
      throw new AbortError();
    }
    for await (const val of this) {
      if (options?.signal?.aborted) {
        throw new AbortError();
      }
      if (number-- > 0) {
        yield val;
      }

      // Don't get another item from iterator in case we reached the end
      if (number <= 0) {
        return;
      }
    }
  }.call(this);
}

const streamReturningOperators = {
  asIndexedPairs,
  drop,
  filter,
  flatMap,
  map,
  take,
  compose: composeOp,
};

const promiseReturningOperators = {
  every,
  forEach,
  reduce,
  toArray,
  some,
  find,
};

for (const key of Object.keys(streamReturningOperators)) {
  const op = streamReturningOperators[key];
  function fn(...args) {
    if (new.target) {
      throw new __errors.ERR_ILLEGAL_CONSTRUCTOR();
    }
    return Readable.from(Reflect.apply(op, this, args));
  }
  Object.defineProperty(fn, "name", { __proto__: null, value: op.name });
  Object.defineProperty(fn, "length", { __proto__: null, value: op.length });
  Object.defineProperty(Readable.prototype, key, {
    __proto__: null,
    value: fn,
    enumerable: false,
    configurable: true,
    writable: true,
  });
}
for (const key of Object.keys(promiseReturningOperators)) {
  const op = promiseReturningOperators[key];
  function fn(...args) {
    if (new.target) {
      throw new __errors.ERR_ILLEGAL_CONSTRUCTOR();
    }
    return Reflect.apply(op, this, args);
  }
  Object.defineProperty(fn, "name", { __proto__: null, value: op.name });
  Object.defineProperty(fn, "length", { __proto__: null, value: op.length });
  Object.defineProperty(Readable.prototype, key, {
    __proto__: null,
    value: fn,
    enumerable: false,
    configurable: true,
    writable: true,
  });
}

// ---- webstreams adapters (lib/internal/webstreams/adapters.js) --------------------------------

function destroyStream(stream, err) {
  if (stream && typeof stream.destroy === "function") stream.destroy(err);
}

function newStreamReadableFromReadableStream(readableStream, options = kEmptyObject) {
  if (!isReadableStream(readableStream)) {
    throw new ERR_INVALID_ARG_TYPE("readableStream", "ReadableStream", readableStream);
  }

  validateObject(options, "options");
  const {
    highWaterMark,
    encoding,
    objectMode = false,
    signal,
  } = options;

  if (encoding !== undefined && !Buffer.isEncoding(encoding)) {
    throw new ERR_INVALID_ARG_VALUE(encoding, "options.encoding");
  }
  validateBoolean(objectMode, "options.objectMode");

  const reader = readableStream.getReader();
  let closed = false;

  const readable = new Readable({
    objectMode,
    highWaterMark,
    encoding,
    signal,

    read() {
      reader.read().then((chunk) => {
        if (chunk.done) {
          // Value should always be undefined here.
          readable.push(null);
        } else {
          readable.push(chunk.value);
        }
      }, (error) => destroyStream(readable, error));
    },

    destroy(error, callback) {
      function done() {
        try {
          callback(error);
        } catch (error) {
          // In a next tick because this is happening within a promise context, and if there are
          // any errors thrown we don't want those to cause an unhandled rejection. Let's just
          // escape the promise and handle it separately.
          process.nextTick(() => { throw error; });
        }
      }

      if (!closed) {
        reader.cancel(error).then(done, done);
        return;
      }
      done();
    },
  });

  reader.closed.then(() => {
    closed = true;
  }, (error) => {
    closed = true;
    destroyStream(readable, error);
  });

  return readable;
}

function newReadableStreamFromStreamReadable(streamReadable, options = kEmptyObject) {
  // Not using the internal/streams/utils isReadableNodeStream utility here because it will return
  // false if the argument is a Duplex whose readable side has ended.
  if (typeof streamReadable?._readableState !== "object") {
    throw new ERR_INVALID_ARG_TYPE("streamReadable", "stream.Readable", streamReadable);
  }

  if (isDestroyed(streamReadable) || !isReadable(streamReadable)) {
    const readable = new ReadableStream();
    readable.cancel();
    return readable;
  }

  const objectMode = streamReadable.readableObjectMode;
  const highWaterMark = streamReadable.readableHighWaterMark;

  const evaluateStrategyOrFallback = (strategy) => {
    // If there is a strategy available, use it
    if (strategy) return strategy;

    if (objectMode) {
      // When running in objectMode explicitly but no strategy, we just fall back to
      // CountQueuingStrategy
      return new CountQueuingStrategy({ highWaterMark });
    }

    // When not running in objectMode explicitly, we just fall back to a minimal strategy that just
    // specifies the highWaterMark and no size algorithm. Using a ByteLengthQueuingStrategy here is
    // unnecessary.
    return { highWaterMark };
  };

  const strategy = evaluateStrategyOrFallback(options?.strategy);

  let controller;

  function onData(chunk) {
    // Copy the Buffer to detach it from the pool.
    if (Buffer.isBuffer(chunk) && !objectMode) chunk = new Uint8Array(chunk);
    controller.enqueue(chunk);
    if (controller.desiredSize <= 0) streamReadable.pause();
  }

  function onFinished(error) {
    cleanup();
    // This is a protection against non-standard, legacy streams that happen to emit an error
    // event again after finished is called.
    streamReadable.on("error", () => {});
    if (error) return controller.error(error);
    controller.close();
  }

  streamReadable.on("data", onData);
  const cleanup = eos(streamReadable, onFinished);

  return new ReadableStream({
    start(c) { controller = c; },

    pull() { streamReadable.resume(); },

    cancel(reason) {
      destroyStream(streamReadable, reason);
    },
  }, strategy);
}

function newStreamWritableFromWritableStream(writableStream, options = kEmptyObject) {
  if (!isWritableStream(writableStream)) {
    throw new ERR_INVALID_ARG_TYPE("writableStream", "WritableStream", writableStream);
  }

  validateObject(options, "options");
  const {
    highWaterMark,
    decodeStrings = true,
    objectMode = false,
    signal,
  } = options;

  validateBoolean(objectMode, "options.objectMode");
  validateBoolean(decodeStrings, "options.decodeStrings");

  const writer = writableStream.getWriter();
  let closed = false;

  const writable = new Writable({
    highWaterMark,
    objectMode,
    decodeStrings,
    signal,

    writev(chunks, callback) {
      function done(error) {
        error = Array.isArray(error) ? error.filter((e) => e) : [error];
        try {
          callback(error.length === 0 ? undefined : error[0]);
        } catch (error) {
          // In a next tick because this is happening within a promise context, and if there are
          // any errors thrown we don't want those to cause an unhandled rejection. Let's just
          // escape the promise and handle it separately.
          process.nextTick(() => destroyStream(writable, error));
        }
      }

      writer.ready.then(
        () => Promise.all(chunks.map((data) => writer.write(data.chunk))).then(done, done),
        done);
    },

    write(chunk, encoding, callback) {
      if (typeof chunk === "string" && decodeStrings && !objectMode) {
        chunk = Buffer.from(chunk, encoding);
        chunk = new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength);
      }

      function done(error) {
        try {
          callback(error);
        } catch (error) {
          destroyStream(writable, error);
        }
      }

      writer.ready.then(
        () => writer.write(chunk).then(done, done),
        done);
    },

    destroy(error, callback) {
      function done() {
        try {
          callback(error);
        } catch (error) {
          // In a next tick because this is happening within a promise context, and if there are
          // any errors thrown we don't want those to cause an unhandled rejection. Let's just
          // escape the promise and handle it separately.
          process.nextTick(() => { throw error; });
        }
      }

      if (!closed) {
        if (error != null) {
          writer.abort(error).then(done, done);
        } else {
          writer.close().then(done, done);
        }
        return;
      }

      done();
    },

    final(callback) {
      function done(error) {
        try {
          callback(error);
        } catch (error) {
          // In a next tick because this is happening within a promise context, and if there are
          // any errors thrown we don't want those to cause an unhandled rejection. Let's just
          // escape the promise and handle it separately.
          process.nextTick(() => destroyStream(writable, error));
        }
      }

      if (!closed) {
        writer.close().then(done, done);
      }
    },
  });

  writer.closed.then(() => {
    // If the WritableStream closes before the stream.Writable has been ended, we signal an
    // error on the stream.Writable.
    closed = true;
    if (!isWritableEnded(writable)) destroyStream(writable, new ERR_STREAM_PREMATURE_CLOSE());
  }, (error) => {
    // If the WritableStream errors before the stream.Writable has been destroyed, signal an error
    // on the stream.Writable.
    closed = true;
    destroyStream(writable, error);
  });

  return writable;
}

function newWritableStreamFromStreamWritable(streamWritable) {
  // Not using the internal/streams/utils isWritableNodeStream utility here because it will return
  // false if the argument is a Duplex whose writable side has ended.
  if (typeof streamWritable?._writableState !== "object") {
    throw new ERR_INVALID_ARG_TYPE("streamWritable", "stream.Writable", streamWritable);
  }

  if (isDestroyed(streamWritable) || !isWritable(streamWritable)) {
    const writable = new WritableStream();
    writable.close();
    return writable;
  }

  const highWaterMark = streamWritable.writableHighWaterMark;
  const strategy = streamWritable.writableObjectMode ?
    new CountQueuingStrategy({ highWaterMark }) :
    { highWaterMark };

  let controller;
  let backpressurePromise;
  let closed;

  function onDrain() {
    if (backpressurePromise !== undefined) backpressurePromise.resolve();
  }

  const cleanup = eos(streamWritable, (error) => {
    if (error?.code === "ERR_STREAM_PREMATURE_CLOSE") {
      const err = new AbortError(undefined, { cause: error });
      error = err;
    }

    cleanup();
    // This is a protection against non-standard, legacy streams that happen to emit an error
    // event again after finished is called.
    streamWritable.on("error", () => {});
    if (error != null) {
      if (backpressurePromise !== undefined) backpressurePromise.reject(error);
      // If closed is not undefined, the error is happening after the WritableStream close has
      // already started. We need to reject it here.
      if (closed !== undefined) {
        closed.reject(error);
        closed = undefined;
      }
      controller.error(error);
      controller = undefined;
      return;
    }

    if (closed !== undefined) {
      closed.resolve();
      closed = undefined;
      return;
    }
    controller.error(new AbortError());
    controller = undefined;
  });

  streamWritable.on("drain", onDrain);

  return new WritableStream({
    start(c) { controller = c; },

    write(chunk) {
      if (streamWritable.writableNeedDrain || !streamWritable.write(chunk)) {
        backpressurePromise = createDeferredPromise();
        return backpressurePromise.promise.finally(() => {
          backpressurePromise = undefined;
        });
      }
    },

    abort(reason) {
      destroyStream(streamWritable, reason);
    },

    close() {
      if (closed === undefined && !isWritableEnded(streamWritable)) {
        closed = createDeferredPromise();
        streamWritable.end();
        return closed.promise;
      }

      controller = undefined;
      return Promise.resolve();
    },
  }, strategy);
}

function newReadableWritablePairFromDuplex(duplex) {
  // Not using the internal/streams/utils isWritableNodeStream and isReadableNodeStream utilities
  // here because they will return false if the argument is a Duplex whose readable or writable
  // side has ended.
  if (typeof duplex?._writableState !== "object" ||
      typeof duplex?._readableState !== "object") {
    throw new ERR_INVALID_ARG_TYPE("duplex", "stream.Duplex", duplex);
  }

  if (isDestroyed(duplex)) {
    const writable = new WritableStream();
    const readable = new ReadableStream();
    writable.close();
    readable.cancel();
    return { readable, writable };
  }

  const writable =
    isWritable(duplex) ?
      newWritableStreamFromStreamWritable(duplex) :
      new WritableStream();

  if (!isWritable(duplex)) writable.close();

  const readable =
    isReadable(duplex) ?
      newReadableStreamFromStreamReadable(duplex) :
      new ReadableStream();

  if (!isReadable(duplex)) readable.cancel();

  return { writable, readable };
}

function newStreamDuplexFromReadableWritablePair(pair = kEmptyObject, options = kEmptyObject) {
  validateObject(pair, "pair");
  const {
    readable: readableStream,
    writable: writableStream,
  } = pair;

  if (!isReadableStream(readableStream)) {
    throw new ERR_INVALID_ARG_TYPE("pair.readable", "ReadableStream", readableStream);
  }
  if (!isWritableStream(writableStream)) {
    throw new ERR_INVALID_ARG_TYPE("pair.writable", "WritableStream", writableStream);
  }

  validateObject(options, "options");
  const {
    allowHalfOpen = false,
    objectMode = false,
    encoding,
    decodeStrings = true,
    highWaterMark,
    signal,
  } = options;

  validateBoolean(objectMode, "options.objectMode");
  if (encoding !== undefined && !Buffer.isEncoding(encoding)) {
    throw new ERR_INVALID_ARG_VALUE(encoding, "options.encoding");
  }

  const writer = writableStream.getWriter();
  const reader = readableStream.getReader();
  let writableClosed = false;
  let readableClosed = false;

  const duplex = new Duplex({
    allowHalfOpen,
    highWaterMark,
    objectMode,
    encoding,
    decodeStrings,
    signal,

    writev(chunks, callback) {
      function done(error) {
        error = Array.isArray(error) ? error.filter((e) => e) : [error];
        try {
          callback(error.length === 0 ? undefined : error[0]);
        } catch (error) {
          // In a next tick because this is happening within a promise context, and if there are
          // any errors thrown we don't want those to cause an unhandled rejection. Let's just
          // escape the promise and handle it separately.
          process.nextTick(() => destroyStream(duplex, error));
        }
      }

      writer.ready.then(
        () => Promise.all(chunks.map((data) => writer.write(data.chunk))).then(done, done),
        done);
    },

    write(chunk, encoding, callback) {
      if (typeof chunk === "string" && decodeStrings && !objectMode) {
        chunk = Buffer.from(chunk, encoding);
        chunk = new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength);
      }

      function done(error) {
        try {
          callback(error);
        } catch (error) {
          destroyStream(duplex, error);
        }
      }

      writer.ready.then(
        () => writer.write(chunk).then(done, done),
        done);
    },

    final(callback) {
      function done(error) {
        try {
          callback(error);
        } catch (error) {
          // In a next tick because this is happening within a promise context, and if there are
          // any errors thrown we don't want those to cause an unhandled rejection. Let's just
          // escape the promise and handle it separately.
          process.nextTick(() => destroyStream(duplex, error));
        }
      }

      if (!writableClosed) {
        writer.close().then(done, done);
      }
    },

    read() {
      reader.read().then((chunk) => {
        if (chunk.done) {
          duplex.push(null);
        } else {
          duplex.push(chunk.value);
        }
      }, (error) => destroyStream(duplex, error));
    },

    destroy(error, callback) {
      function done() {
        try {
          callback(error);
        } catch (error) {
          // In a next tick because this is happening within a promise context, and if there are
          // any errors thrown we don't want those to cause an unhandled rejection. Let's just
          // escape the promise and handle it separately.
          process.nextTick(() => { throw error; });
        }
      }

      async function closeWriter() {
        if (!writableClosed) await writer.abort(error);
      }

      async function closeReader() {
        if (!readableClosed) await reader.cancel(error);
      }

      if (!writableClosed || !readableClosed) {
        Promise.all([
          closeWriter(),
          closeReader(),
        ]).then(done, done);
        return;
      }

      done();
    },
  });

  writer.closed.then(() => {
    writableClosed = true;
    if (!isWritableEnded(duplex)) destroyStream(duplex, new ERR_STREAM_PREMATURE_CLOSE());
  }, (error) => {
    writableClosed = true;
    readableClosed = true;
    destroyStream(duplex, error);
  });

  reader.closed.then(() => {
    readableClosed = true;
  }, (error) => {
    writableClosed = true;
    readableClosed = true;
    destroyStream(duplex, error);
  });

  return duplex;
}

const webAdapters = {
  newStreamReadableFromReadableStream,
  newReadableStreamFromStreamReadable,
  newStreamWritableFromWritableStream,
  newWritableStreamFromStreamWritable,
  newReadableWritablePairFromDuplex,
  newStreamDuplexFromReadableWritablePair,
};

// ---- stream/promises (lib/stream/promises.js) -------------------------------------------------

const promises = {
  finished,
  pipeline(...streams) {
    return new Promise((resolve, reject) => {
      let signal;
      let end;
      const lastArg = streams[streams.length - 1];
      if (lastArg && typeof lastArg === "object" &&
          !isNodeStream(lastArg) &&
          !isIterable(lastArg) &&
          !isWebStream(lastArg)) {
        const options = streams.pop();
        signal = options.signal;
        end = options.end;
      }

      pipelineImpl(streams, (err, value) => {
        if (err) {
          reject(err);
        } else {
          resolve(value);
        }
      }, { signal, end });
    });
  },
};

// ---- node:stream (lib/stream.js) --------------------------------------------------------------

// duplexPair(): two crossed Duplexes — writes to one surface as readable data on the other.
function duplexPair(options) {
  let a;
  let b;
  a = new Duplex({
    ...options,
    read() {},
    write(chunk, enc, cb) { b.push(chunk); cb(); },
    final(cb) { b.push(null); cb(); },
  });
  b = new Duplex({
    ...options,
    read() {},
    write(chunk, enc, cb) { a.push(chunk); cb(); },
    final(cb) { a.push(null); cb(); },
  });
  return [a, b];
}

const customPromisify = Symbol.for("nodejs.util.promisify.custom");

Stream.isDestroyed = isDestroyed;
Stream.isDisturbed = isDisturbed;
Stream.isErrored = isErrored;
Stream.isReadable = isReadable;
Stream.isWritable = isWritable;
Stream.Readable = Readable;
Stream.Writable = Writable;
Stream.Duplex = Duplex;
Stream.Transform = Transform;
Stream.PassThrough = PassThrough;
Stream.duplexPair = duplexPair;
Stream.pipeline = pipeline;
Stream.addAbortSignal = addAbortSignal;
Stream.finished = eos;
Stream.destroy = destroyer;
Stream.compose = compose;
Stream.setDefaultHighWaterMark = setDefaultHighWaterMark;
Stream.getDefaultHighWaterMark = getDefaultHighWaterMark;

Object.defineProperty(Stream, "promises", {
  __proto__: null,
  configurable: true,
  enumerable: true,
  get() {
    return promises;
  },
});

Object.defineProperty(pipeline, customPromisify, {
  __proto__: null,
  enumerable: true,
  get() {
    return promises.pipeline;
  },
});

Object.defineProperty(eos, customPromisify, {
  __proto__: null,
  enumerable: true,
  get() {
    return promises.finished;
  },
});

// Backwards-compat with node 0.4.x
Stream.Stream = Stream;

Stream._isArrayBufferView = (v) => ArrayBuffer.isView(v);
Stream._isUint8Array = isUint8Array;
Stream._uint8ArrayToBuffer = function _uint8ArrayToBuffer(chunk) {
  return Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength);
};

// Internal: lets node:zlib run a step after the writes already queued on a stream (a zero-length
// write travels through the same queue).
Object.defineProperty(Stream, "_afterWrites", {
  value(stream, fn) {
    stream.write(Buffer.alloc(0), (err) => {
      if (err) return;
      try {
        fn();
      } catch (e) {
        stream.destroy(e);
      }
    });
  },
  enumerable: false,
});

__builtins.set("stream", Stream);
__builtins.set("stream/promises", promises);
// Node's internal/streams/{destroy,utils} helpers, for the fs port (see fs.js).
__internals.set("streams", { errorOrDestroy, isIterable, kResistStopPropagation });

// node:stream/web — the WHATWG streams. lumen-web already ships these as globals (spec-correct
// pull/backpressure, BYOB, and CompressionStream/DecompressionStream over the shared DEFLATE
// codec), so re-export the exact same constructors by identity.
const webStreams = {};
for (const name of [
  "ReadableStream", "ReadableStreamDefaultReader", "ReadableStreamBYOBReader",
  "ReadableStreamDefaultController", "ReadableByteStreamController", "ReadableStreamBYOBRequest",
  "WritableStream", "WritableStreamDefaultWriter", "WritableStreamDefaultController",
  "TransformStream", "TransformStreamDefaultController",
  "ByteLengthQueuingStrategy", "CountQueuingStrategy",
  "TextEncoderStream", "TextDecoderStream", "CompressionStream", "DecompressionStream",
]) {
  if (typeof globalThis[name] !== "undefined") webStreams[name] = globalThis[name];
}
__builtins.set("stream/web", webStreams);
