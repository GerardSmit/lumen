// node:stream — a minimal but faithful streams implementation over EventEmitter. Enough for
// node:http request/response bodies and the body-parser/raw-body consumers: flowing-mode Readable
// (push/'data'/'end', pause/resume, pipe, setEncoding, async iteration), Writable (write/end,
// 'finish'/'drain'), and Duplex/Transform/PassThrough composed from them. Bodies here are already
// fully buffered by the http bridge, but stream state still follows Node's buffering contracts.

const EventEmitter = __builtins.get("events");

class Stream extends EventEmitter {}

class Readable extends Stream {
  constructor(opts = {}) {
    super();
    const objectMode = !!(opts.objectMode || opts.readableObjectMode);
    this._readableState = { flowing: null, ended: false, endEmitted: false, buffer: [], length: 0, objectMode, highWaterMark: opts.highWaterMark === undefined ? getDefaultHighWaterMark(objectMode) : Number(opts.highWaterMark), encoding: null, destroyed: false, reading: false, errored: null, autoDestroy: opts.autoDestroy !== false, emitClose: opts.emitClose !== false, closeEmitted: false };
    this.readable = true;
    if (opts.encoding) this.setEncoding(opts.encoding);
    if (typeof opts.read === "function") this._read = opts.read;
  }
  _read() {}
  get readableLength() { return this._readableState.length; }
  get readableFlowing() { return this._readableState.flowing; }
  set readableFlowing(v) { this._readableState.flowing = v; }
  get readableEnded() { return this._readableState.endEmitted; }
  get readableAborted() { return !!(this._readableState.destroyed || this._readableState.errored) && !this._readableState.endEmitted; }
  get destroyed() { return !!(this._readableState.destroyed || (this._writableState && this._writableState.destroyed)); }
  set destroyed(v) { this._readableState.destroyed = !!v; if (this._writableState) this._writableState.destroyed = !!v; }
  get closed() { return this._readableState.closeEmitted; }
  // Settable for subclasses that keep their own closed flag on the instance (node:http2's streams).
  set closed(v) { this._readableState.closeEmitted = !!v; }
  get errored() { return this._readableState.errored || (this._writableState && this._writableState.errored) || null; }
  get readableHighWaterMark() { return this._readableState.highWaterMark; }
  get readableObjectMode() { return this._readableState.objectMode; }

  push(chunk) {
    const state = this._readableState;
    if (chunk === null) {
      state.ended = true;
      maybeEmitEnd(this);
      return false;
    }
    if (state.destroyed) return false;
    if (state.encoding && chunk instanceof Uint8Array) chunk = Buffer.from(chunk).toString(state.encoding);
    if (state.flowing) this.emit("data", chunk);
    else { state.buffer.push(chunk); state.length += state.objectMode ? 1 : chunk.length; this.emit("readable"); }
    return state.length < state.highWaterMark;
  }

  // Put a chunk back at the head of the buffer — what a protocol parser does with the bytes it
  // read past its own frame (http's `upgrade` head, ws's first frame). Delivered ahead of anything
  // pushed later; a flowing stream drains it on the next tick like any buffered chunk.
  unshift(chunk) {
    const state = this._readableState;
    if (chunk === null || chunk === undefined || state.destroyed) return;
    if (state.encoding && chunk instanceof Uint8Array) chunk = Buffer.from(chunk).toString(state.encoding);
    if (!state.objectMode && chunk.length === 0) return;
    state.buffer.unshift(chunk);
    state.length += state.objectMode ? 1 : chunk.length;
    if (state.flowing) queueMicrotask(() => flow(this));
    else this.emit("readable");
  }

  read(size) {
    const state = this._readableState;
    if (state.objectMode) {
      if (state.buffer.length === 0) return null;
      state.length--;
      const value = state.buffer.shift();
      maybeEmitEnd(this);
      return value;
    }
    let wanted = size === undefined || size === null ? state.length : Number(size);
    if (!Number.isInteger(wanted) || wanted < 0) throw new RangeError("The value of size is out of range");
    if (wanted === 0) { maybeEmitEnd(this); return null; }
    if (state.length < wanted) {
      if (!state.ended) return null;
      wanted = state.length;
    }
    if (state.buffer.length === 0) return null;
    const chunks = [];
    let remaining = wanted;
    while (remaining > 0 && state.buffer.length) {
      const chunk = state.buffer[0];
      if (chunk.length <= remaining) { chunks.push(state.buffer.shift()); remaining -= chunk.length; }
      else { chunks.push(chunk.slice(0, remaining)); state.buffer[0] = chunk.slice(remaining); remaining = 0; }
    }
    state.length -= wanted;
    const chunk = state.encoding ? chunks.join("") : Buffer.concat(chunks.map(value => Buffer.from(value)), wanted);
    maybeEmitEnd(this);
    return chunk;
  }

  setEncoding(enc) {
    this._readableState.encoding = String(enc).toLowerCase().replace("-", "");
    return this;
  }

  on(ev, fn) {
    const res = super.on(ev, fn);
    if (ev === "data") {
      if (this._readableState.flowing !== false) this.resume();
    } else if (ev === "readable") {
      // Not fully modeled; resume so data still flows to any 'data'/pipe consumers.
    }
    return res;
  }
  addListener(ev, fn) { return this.on(ev, fn); }
  once(ev, fn) {
    const res = super.once(ev, fn);
    if (ev === "data" && this._readableState.flowing !== false) this.resume();
    return res;
  }

  resume() {
    const state = this._readableState;
    if (state.flowing) return this;
    state.flowing = true;
    queueMicrotask(() => flow(this));
    return this;
  }
  pause() {
    this._readableState.flowing = false;
    return this;
  }
  isPaused() { return this._readableState.flowing === false; }

  pipe(dest, opts = {}) {
    this.on("data", (chunk) => {
      const ok = dest.write(chunk);
      if (ok === false && typeof this.pause === "function") {
        this.pause();
        dest.once("drain", () => this.resume());
      }
    });
    if (opts.end !== false) this.on("end", () => dest.end());
    this.on("error", (err) => { if (dest.destroy) dest.destroy(err); });
    if (dest.emit) dest.emit("pipe", this);
    return dest;
  }
  unpipe() { this.pause(); return this; }

  destroy(err) {
    const state = this._readableState;
    if (state.destroyed) return this;
    state.destroyed = true;
    if (err) { state.errored = err; if (this._writableState) this._writableState.errored = err; }
    this.readable = false;
    if (this._writableState) this.writable = false;
    queueMicrotask(() => {
      if (err) this.emit("error", err);
      if (state.emitClose) { state.closeEmitted = true; this.emit("close"); }
    });
    return this;
  }

  [Symbol.asyncIterator]() {
    const self = this;
    const queue = [];
    let done = false;
    let error = null;
    let pending = null;
    const settle = () => {
      if (!pending) return;
      if (error) { const p = pending; pending = null; p.reject(error); }
      else if (queue.length) { const p = pending; pending = null; p.resolve({ value: queue.shift(), done: false }); }
      else if (done) { const p = pending; pending = null; p.resolve({ value: undefined, done: true }); }
    };
    self.on("data", (c) => { queue.push(c); settle(); });
    self.on("end", () => { done = true; settle(); });
    self.on("error", (e) => { error = e; settle(); });
    return {
      next() {
        if (queue.length) return Promise.resolve({ value: queue.shift(), done: false });
        if (error) return Promise.reject(error);
        if (done) return Promise.resolve({ value: undefined, done: true });
        return new Promise((resolve, reject) => { pending = { resolve, reject }; });
      },
      return() { self.destroy(); return Promise.resolve({ value: undefined, done: true }); },
      [Symbol.asyncIterator]() { return this; },
    };
  }
}

function flow(stream) {
  const state = stream._readableState;
  while (state.flowing && state.buffer.length) {
    const chunk = state.buffer.shift();
    state.length -= state.objectMode ? 1 : chunk.length;
    stream.emit("data", chunk);
  }
  maybeEmitEnd(stream);
}

function maybeEmitEnd(stream) {
  const state = stream._readableState;
  if (state.ended && !state.endEmitted && state.buffer.length === 0 && state.flowing !== false) {
    state.endEmitted = true;
    state.flowing = false;
    stream.readable = false;
    queueMicrotask(() => { stream.emit("end"); maybeAutoDestroy(stream); });
  }
}

// Node's autoDestroy (on by default): a stream destroys itself — and so emits 'close' — once
// its readable side has ended and its writable side (if any) has finished. Consumers that wait
// on 'close' rather than 'end' (a browser's CDP pipe reader, for one) depend on it.
function maybeAutoDestroy(stream) {
  const r = stream._readableState, w = stream._writableState;
  const autoDestroy = r ? r.autoDestroy : w.autoDestroy;
  if (!autoDestroy) return;
  if (r && !(r.endEmitted || r.destroyed)) return;
  if (w && !(w.finished || w.destroyed)) return;
  if (isDestroyed(stream)) return;
  if (typeof stream.destroy === "function") stream.destroy();
}

class Writable extends Stream {
  constructor(opts = {}) {
    super();
    // `tail` serializes _write calls so writes complete in order and end() can wait for them
    // (an async _write — e.g. a subprocess stdin write — must finish before _final closes it).
    this._writableState = writableState(opts);
    this.writable = true;
    if (typeof opts.write === "function") this._write = opts.write;
    if (typeof opts.writev === "function") this._writev = opts.writev;
    if (typeof opts.final === "function") this._final = opts.final;
  }
  _write(chunk, encoding, cb) { cb(); }

  write(chunk, encoding, cb) {
    if (typeof encoding === "function") { cb = encoding; encoding = null; }
    const state = this._writableState;
    if (state.ended) {
      const err = new Error("write after end");
      if (cb) queueMicrotask(() => cb(err)); else this.emit("error", err);
      return false;
    }
    const length = state.objectMode ? 1 : (chunk && chunk.length !== undefined ? chunk.length : Buffer.byteLength(String(chunk), encoding || "utf8"));
    state.length += length;
    const entry = { chunk, encoding, cb, length };
    if (state.corked) state.corkBuffer.push(entry); else queueWrite(this, entry);
    if (state.length >= state.highWaterMark) state.needDrain = true;
    return !state.needDrain;
  }

  _queueBatch(entries) {
    const state = this._writableState;
    if (entries.length > 1 && typeof this._writev === "function") {
      queueWrite(this, { entries, length: entries.reduce((n, e) => n + e.length, 0) });
    } else for (const entry of entries) queueWrite(this, entry);
  }

  end(chunk, encoding, cb) {
    if (typeof chunk === "function") { cb = chunk; chunk = null; }
    else if (typeof encoding === "function") { cb = encoding; encoding = null; }
    const state = this._writableState;
    if (chunk != null) this.write(chunk, encoding);
    state.ended = true;
    if (state.corked) { state.corked = 0; const entries = state.corkBuffer.splice(0); this._queueBatch(entries); }
    // Wait for all queued writes to drain, then run _final (e.g. close the child's stdin).
    whenWritesDrained(this).then(() => {
      if (state.finished) return;
      const done = () => { state.finished = true; if (cb) cb(); this.emit("finish"); maybeAutoDestroy(this); };
      if (this._final) this._final((err) => { if (err) this.emit("error", err); else done(); });
      else done();
    });
    return this;
  }

  cork() { this._writableState.corked++; }
  uncork() {
    const state = this._writableState;
    if (state.corked) state.corked--;
    if (!state.corked && state.corkBuffer.length) this._queueBatch(state.corkBuffer.splice(0));
  }
  setDefaultEncoding() { return this; }
  get writableLength() { return this._writableState.length; }
  get writableEnded() { return this._writableState.ended; }
  get writableFinished() { return this._writableState.finished; }
  get writableNeedDrain() { return this._writableState.needDrain; }
  get writableCorked() { return this._writableState.corked; }
  get destroyed() { return this._writableState.destroyed; }
  set destroyed(v) { this._writableState.destroyed = !!v; }
  get closed() { return this._writableState.closeEmitted; }
  set closed(v) { this._writableState.closeEmitted = !!v; }
  get errored() { return this._writableState.errored; }
  get writableHighWaterMark() { return this._writableState.highWaterMark; }
  get writableObjectMode() { return this._writableState.objectMode; }

  destroy(err) {
    const state = this._writableState;
    if (state.destroyed) return this;
    state.destroyed = true;
    if (err) { state.errored = err; if (this._readableState) this._readableState.errored = err; }
    this.writable = false;
    queueMicrotask(() => { if (err) this.emit("error", err); if (state.emitClose) { state.closeEmitted = true; this.emit("close"); } });
    return this;
  }
}

function writableState(opts) {
  const objectMode = !!(opts.objectMode || opts.writableObjectMode);
  return { ended: false, finished: false, destroyed: false, autoDestroy: opts.autoDestroy !== false, emitClose: opts.emitClose !== false, closeEmitted: false, corked: 0, corkBuffer: [], errored: null, buffered: [], writing: false, drainWaiters: [], objectMode, highWaterMark: opts.highWaterMark === undefined ? getDefaultHighWaterMark(objectMode) : Number(opts.highWaterMark), length: 0, needDrain: false };
}
// Writes go to _write one at a time, in order, and — like Node — the first one starts
// synchronously inside write(): a _write that calls back synchronously has completed (and
// writableLength has dropped) before write() returns. An entry is a chunk, a writev batch
// (`entries`), or a `run(cb)` step other code sequences behind the pending writes.
function queueWrite(stream, entry) {
  const state = stream._writableState;
  state.buffered.push(entry);
  if (!state.writing) clearWriteBuffer(stream);
}
function clearWriteBuffer(stream) {
  const state = stream._writableState;
  while (!state.writing && state.buffered.length) {
    const entry = state.buffered.shift();
    state.writing = true;
    let sync = true;
    let called = false;
    const done = (err) => {
      if (called) return;
      called = true;
      state.writing = false;
      if (entry.entries) for (const e of entry.entries) completeWrite(stream, e, err);
      else if (!entry.run) completeWrite(stream, entry, err);
      else if (err) stream.emit("error", err);
      // A synchronous callback lets the loop below pick up the next entry; an asynchronous one
      // has to restart it.
      if (!sync) clearWriteBuffer(stream);
    };
    if (entry.run) entry.run(done);
    else if (entry.entries) stream._writev(entry.entries.map(e => ({ chunk: e.chunk, encoding: e.encoding })), done);
    else stream._write(entry.chunk, entry.encoding, done);
    sync = false;
  }
  if (!state.writing && !state.buffered.length) for (const w of state.drainWaiters.splice(0)) w();
}
// Resolves once every write queued so far has completed.
function whenWritesDrained(stream) {
  const state = stream._writableState;
  if (!state.writing && !state.buffered.length) return Promise.resolve();
  return new Promise(resolve => state.drainWaiters.push(resolve));
}
// Run `fn` after the writes queued before it, before any queued after it.
function afterWrites(stream, fn) {
  queueWrite(stream, { run: (done) => { let r; try { r = fn(); } catch (e) { done(e); return; } Promise.resolve(r).then(() => done(), done); } });
}
function completeWrite(stream, entry, error) {
  const state = stream._writableState;
  state.length -= entry.length;
  // Node's errorOrDestroy: a stream that is already destroyed has reported its error.
  if (error && !isDestroyed(stream)) stream.emit("error", error);
  if (entry.cb) entry.cb(error);
  if (state.needDrain && state.length < state.highWaterMark) { state.needDrain = false; queueMicrotask(() => stream.emit("drain")); }
}

// Duplex: readable + writable. Compose by inheriting Readable and copying Writable's methods.
class Duplex extends Readable {
  constructor(opts = {}) {
    super(opts);
    // Mirror Writable's state (including `tail`, which write()/end() chain on to serialize writes).
    this._writableState = writableState(opts);
    this.writable = true;
    if (typeof opts.write === "function") this._write = opts.write;
    if (typeof opts.writev === "function") this._writev = opts.writev;
    if (typeof opts.final === "function") this._final = opts.final;
  }
}
for (const m of ["_write", "write", "_queueBatch", "end", "cork", "uncork", "setDefaultEncoding"]) {
  Duplex.prototype[m] = Writable.prototype[m];
}
for (const name of ["writableLength", "writableHighWaterMark", "writableObjectMode", "writableEnded", "writableFinished", "writableNeedDrain", "writableCorked"]) {
  Object.defineProperty(Duplex.prototype, name, Object.getOwnPropertyDescriptor(Writable.prototype, name));
}

class Transform extends Duplex {
  constructor(opts = {}) {
    super(opts);
    if (typeof opts.transform === "function") this._transform = opts.transform;
    if (typeof opts.flush === "function") this._flush = opts.flush;
  }
  _transform(chunk, encoding, cb) { cb(null, chunk); }
  _write(chunk, encoding, cb) {
    this._transform(chunk, encoding, (err, data) => {
      if (err) return cb(err);
      if (data != null) this.push(data);
      cb();
    });
  }
  _final(cb) {
    if (this._flush) this._flush((err, data) => { if (data != null) this.push(data); this.push(null); cb(err); });
    else { this.push(null); cb(); }
  }
}

class PassThrough extends Transform {
  _transform(chunk, encoding, cb) { cb(null, chunk); }
}

// Readable.from(iterable) — async or sync.
Readable.from = function (iterable, opts) {
  const r = new Readable(opts);
  (async () => {
    try {
      for await (const chunk of iterable) r.push(chunk);
      r.push(null);
    } catch (err) { r.destroy(err); }
  })();
  return r;
};

function prematureClose() {
  const e = new Error("Premature close");
  e.code = "ERR_STREAM_PREMATURE_CLOSE";
  return e;
}

// eos/finished: invoke cb exactly once when the stream is fully done. For a duplex/transform we
// wait for BOTH the readable side ('end') and the writable side ('finish'); a bare 'close' (from
// destroy() without an error) before completion is a premature close, matching Node.
function finished(stream, opts, cb) {
  if (typeof opts === "function") { cb = opts; opts = {}; }
  opts = opts || {};
  const rState = stream._readableState;
  const wState = stream._writableState;
  const wantReadable = opts.readable !== false && !!rState;
  const wantWritable = opts.writable !== false && !!wState;
  let readableDone = !wantReadable || rState.endEmitted;
  let writableDone = !wantWritable || wState.finished;
  let called = false;

  const cleanup = () => {
    if (!stream.removeListener) return;
    stream.removeListener("end", onend);
    stream.removeListener("finish", onfinish);
    stream.removeListener("close", onclose);
    stream.removeListener("error", onerror);
  };
  const settle = (err) => {
    if (called) return;
    called = true;
    cleanup();
    queueMicrotask(() => cb(err || null));
  };
  const check = () => { if (readableDone && writableDone) settle(); };
  const onend = () => { readableDone = true; check(); };
  const onfinish = () => { writableDone = true; check(); };
  const onerror = (err) => settle(err || prematureClose());
  const onclose = () => { if (readableDone && writableDone) settle(); else settle(prematureClose()); };

  stream.on("end", onend);
  stream.on("finish", onfinish);
  stream.on("close", onclose);
  stream.on("error", onerror);
  // Already errored/destroyed before we attached — fire on the next tick.
  if ((rState && rState.errored) || (wState && wState.errored)) {
    settle((rState && rState.errored) || (wState && wState.errored));
  } else {
    check();
  }
  return cleanup;
}

function isNodeStream(x) {
  return !!(x && typeof x === "object" && (x._readableState || x._writableState) && typeof x.on === "function");
}

// pipeline(src, ...transforms, dst[, cb]) with real error propagation and cleanup. Non-stream
// sources/transforms (async iterables, arrays, async generator functions) are adapted through
// Readable.from, so `pipeline(asyncIterable, transform, writable, cb)` works like Node's.
function pipeline(...args) {
  const cb = typeof args[args.length - 1] === "function" ? args.pop() : () => {};
  const parts = args.flat();
  if (parts.length < 2) throw new Error("pipeline requires at least 2 streams");

  const nodes = [];
  let done = false;
  const finish = (err) => {
    if (done) return;
    done = true;
    if (err) for (const n of nodes) if (isNodeStream(n) && !isDestroyed(n)) n.destroy(err);
    queueMicrotask(() => cb(err || null));
  };

  let current = parts[0];
  if (!isNodeStream(current)) current = Readable.from(current);
  nodes.push(current);

  for (let i = 1; i < parts.length; i++) {
    const part = parts[i];
    const isLast = i === parts.length - 1;
    if (typeof part === "function") {
      // async generator transform / async destination consuming the previous stream.
      let result;
      try { result = part(current); } catch (e) { finish(e); return current; }
      if (isLast && !(result && typeof result[Symbol.asyncIterator] === "function")) {
        Promise.resolve(result).then(() => finish(null), (e) => finish(e));
        current = null;
        break;
      }
      current = Readable.from(result);
      nodes.push(current);
    } else {
      current.pipe(part);
      nodes.push(part);
      current = part;
    }
  }

  for (const n of nodes) if (isNodeStream(n)) n.on("error", (e) => finish(e));
  if (current) finished(current, (err) => finish(err));
  return current || nodes[nodes.length - 1];
}

const streamPromises = {
  finished(stream, opts) {
    return new Promise((resolve, reject) => finished(stream, opts || {}, (err) => (err ? reject(err) : resolve())));
  },
  pipeline(...args) {
    return new Promise((resolve, reject) => pipeline(...args, (err) => (err ? reject(err) : resolve())));
  },
};

// --- state helpers, matching Node's public predicates ---
function destroy(stream, err) { if (stream && typeof stream.destroy === "function") stream.destroy(err); return stream; }
function isDestroyed(stream) {
  const r = stream && stream._readableState, w = stream && stream._writableState;
  return !!((r && r.destroyed) || (w && w.destroyed));
}
function isErrored(stream) {
  const r = stream && stream._readableState, w = stream && stream._writableState;
  return !!((r && r.errored) || (w && w.errored) || (stream && stream.errored));
}
function isReadable(stream) {
  const r = stream && stream._readableState;
  if (!r) return !!(stream && stream.readable === true);
  return !r.ended && !r.endEmitted && !r.destroyed && stream.readable !== false;
}
function isWritable(stream) {
  const w = stream && stream._writableState;
  if (!w) return !!(stream && stream.writable === true);
  return !w.ended && !w.finished && !w.destroyed && stream.writable !== false;
}
function isDisturbed(stream) {
  const r = stream && stream._readableState;
  return !!(r && (r.flowing !== null || r.endEmitted || r.ended));
}

let defaultHighWaterMark = 65536;
let defaultHighWaterMarkObjectMode = 16;
function getDefaultHighWaterMark(objectMode) { return objectMode ? defaultHighWaterMarkObjectMode : defaultHighWaterMark; }
function setDefaultHighWaterMark(objectMode, value) {
  if (objectMode) defaultHighWaterMarkObjectMode = value; else defaultHighWaterMark = value;
}

function _isArrayBufferView(x) { return ArrayBuffer.isView(x); }
function _isUint8Array(x) { return x instanceof Uint8Array; }
function _uint8ArrayToBuffer(chunk) { return Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength); }

// addAbortSignal(signal, stream): destroy `stream` with an ABORT_ERR when `signal` aborts.
function addAbortSignal(signal, stream) {
  if (!signal || typeof signal.addEventListener !== "function") return stream;
  const abort = () => {
    let err = signal.reason;
    if (!err) { err = new Error("The operation was aborted"); err.code = "ABORT_ERR"; err.name = "AbortError"; }
    if (typeof stream.destroy === "function") stream.destroy(err);
  };
  if (signal.aborted) queueMicrotask(abort);
  else signal.addEventListener("abort", abort, { once: true });
  return stream;
}

// duplexPair(): two crossed Duplexes — writes to one surface as readable data on the other.
function duplexPair() {
  let a, b;
  a = new Duplex({ read() {}, write(chunk, enc, cb) { b.push(chunk); cb(); }, final(cb) { b.push(null); cb(); } });
  b = new Duplex({ read() {}, write(chunk, enc, cb) { a.push(chunk); cb(); }, final(cb) { a.push(null); cb(); } });
  return [a, b];
}

// compose(...streams): fuse a chain into a single Duplex (write → head, read ← tail).
function compose(...streams) {
  streams = streams.flat().filter((s) => s != null);
  if (streams.length === 0) return new PassThrough();
  const head = streams[0];
  const tail = streams[streams.length - 1];
  for (let i = 0; i < streams.length - 1; i++) streams[i].pipe(streams[i + 1]);
  const d = new Duplex({
    read() {},
    write(chunk, enc, cb) { head.write(chunk, enc); cb(); },
    final(cb) { head.end(); cb(); },
  });
  if (tail.on) {
    tail.on("data", (c) => d.push(c));
    tail.on("end", () => d.push(null));
  }
  for (const s of streams) if (s.on) s.on("error", (e) => d.destroy(e));
  return d;
}

// Callable and `Readable.call(this, opts)`-inheritable, as in Node (see __legacyConstructor).
Stream = __legacyConstructor(Stream, (self, [opts]) => EventEmitter.init.call(self, opts));
Readable = __legacyConstructor(Readable, __initByCopy(Readable));
Writable = __legacyConstructor(Writable, __initByCopy(Writable));
Duplex = __legacyConstructor(Duplex, __initByCopy(Duplex));
Transform = __legacyConstructor(Transform, __initByCopy(Transform));
PassThrough = __legacyConstructor(PassThrough, __initByCopy(PassThrough));

Stream.Readable = Readable;
Stream.Writable = Writable;
// Internal: lets node:zlib sequence a flush behind the writes already queued on a stream.
Object.defineProperty(Stream, "_afterWrites", { value: afterWrites, enumerable: false });
Stream.Duplex = Duplex;
Stream.Transform = Transform;
Stream.PassThrough = PassThrough;
Stream.Stream = Stream;
Stream.finished = finished;
Stream.pipeline = pipeline;
Stream.promises = streamPromises;
Stream.destroy = destroy;
Stream.addAbortSignal = addAbortSignal;
Stream.compose = compose;
Stream.duplexPair = duplexPair;
Stream.isDestroyed = isDestroyed;
Stream.isErrored = isErrored;
Stream.isReadable = isReadable;
Stream.isWritable = isWritable;
Stream.isDisturbed = isDisturbed;
Stream.getDefaultHighWaterMark = getDefaultHighWaterMark;
Stream.setDefaultHighWaterMark = setDefaultHighWaterMark;
Stream._isArrayBufferView = _isArrayBufferView;
Stream._isUint8Array = _isUint8Array;
Stream._uint8ArrayToBuffer = _uint8ArrayToBuffer;

__builtins.set("stream", Stream);

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
