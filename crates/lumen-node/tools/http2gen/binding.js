// ---- internalBinding('http2') ---------------------------------------------------------------------
// The surface of Node's node_http2.cc over nghttp2, in JS: Http2Session and Http2Stream handles
// that speak HTTP/2 (RFC 7540) with nghttp2's behaviour — framing, HPACK, flow control, SETTINGS
// and PING bookkeeping, GOAWAY, priority, push. A session consumes its socket's 'data' events and
// writes frames with `socket.write`. Calls into JS happen synchronously from frame processing,
// as they do from nghttp2's callbacks.

const FRAME_DATA = 0, FRAME_HEADERS = 1, FRAME_PRIORITY = 2, FRAME_RST_STREAM = 3, FRAME_SETTINGS = 4,
  FRAME_PUSH_PROMISE = 5, FRAME_PING = 6, FRAME_GOAWAY = 7, FRAME_WINDOW_UPDATE = 8,
  FRAME_CONTINUATION = 9, FRAME_ALTSVC = 10, FRAME_ORIGIN = 12;
const FLAG_END_STREAM = 1, FLAG_END_HEADERS = 4, FLAG_ACK = 1, FLAG_PADDED = 8, FLAG_PRIORITY = 32;
const ERR_NO_ERROR = 0, ERR_PROTOCOL = 1, ERR_INTERNAL = 2, ERR_FLOW_CONTROL = 3, ERR_SETTINGS_TIMEOUT = 4,
  ERR_STREAM_CLOSED = 5, ERR_FRAME_SIZE = 6, ERR_REFUSED_STREAM = 7, ERR_CANCEL = 8, ERR_COMPRESSION = 9,
  ERR_ENHANCE_YOUR_CALM = 11;
const SESSION_SERVER = 0, SESSION_CLIENT = 1;
const HCAT_REQUEST = 0, HCAT_RESPONSE = 1, HCAT_PUSH_RESPONSE = 2, HCAT_HEADERS = 3;
const STREAM_OPTION_EMPTY_PAYLOAD = 1, STREAM_OPTION_GET_TRAILERS = 2;
const MAX_WINDOW = 0x7fffffff;
const CLIENT_PREFACE = Buffer.from("PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");

// nghttp2 library error codes
const NG_ERR_INVALID_ARGUMENT = -501, NG_ERR_PROTO = -505, NG_ERR_STREAM_ID_NOT_AVAILABLE = -509,
  NG_ERR_STREAM_CLOSED = -510, NG_ERR_START_STREAM_NOT_ALLOWED = -516, NG_ERR_FRAME_SIZE_ERROR = -522,
  NG_ERR_PUSH_DISABLED = -528, NG_ERR_NOMEM = -901, NG_ERR_BAD_CLIENT_MAGIC = -903;
const nghttp2Messages = {
  0: "Success", "-501": "Invalid argument", "-502": "Out of buffer space",
  "-503": "Unsupported SPDY version", "-504": "Operation would block", "-505": "Protocol error",
  "-506": "Invalid frame octets", "-507": "EOF", "-508": "Data transfer deferred",
  "-509": "No more Stream ID available", "-510": "Stream was already closed or invalid",
  "-511": "Stream is closing", "-512": "The transmission is not allowed for this stream",
  "-513": "Stream ID is invalid", "-514": "Invalid stream state",
  "-515": "Another DATA frame has already been deferred", "-516": "request HEADERS is not allowed",
  "-517": "GOAWAY has already been sent", "-518": "Invalid header block", "-519": "Invalid state",
  "-521": "The user callback function failed due to the temporal error",
  "-522": "The length of the frame is invalid", "-523": "Header compression/decompression error",
  "-524": "Flow control error", "-525": "Insufficient buffer size given to function",
  "-526": "Callback was paused by the application", "-527": "Too many inflight SETTINGS",
  "-528": "Server push is disabled by peer",
  "-529": "DATA or HEADERS frame has already been submitted for the stream",
  "-530": "The current session is closing", "-531": "Invalid HTTP header field was received",
  "-532": "Violation in HTTP messaging rule", "-533": "Stream was refused", "-534": "Internal error",
  "-535": "Cancel",
  "-536": "When a local endpoint expects to receive SETTINGS frame, it receives an other type of frame",
  "-537": "The errors from peer exceeds the limit", "-900": "Fatal error", "-901": "Out of memory",
  "-902": "The user callback function failed", "-903": "Received bad client magic byte string",
  "-904": "Flooding was detected in this HTTP/2 session, and it must be closed",
};
function nghttp2ErrorString(code) {
  return nghttp2Messages[code] ?? "Unknown error code";
}
const nameForErrorCode = [
  "NGHTTP2_NO_ERROR", "NGHTTP2_PROTOCOL_ERROR", "NGHTTP2_INTERNAL_ERROR", "NGHTTP2_FLOW_CONTROL_ERROR",
  "NGHTTP2_SETTINGS_TIMEOUT", "NGHTTP2_STREAM_CLOSED", "NGHTTP2_FRAME_SIZE_ERROR", "NGHTTP2_REFUSED_STREAM",
  "NGHTTP2_CANCEL", "NGHTTP2_COMPRESSION_ERROR", "NGHTTP2_CONNECT_ERROR", "NGHTTP2_ENHANCE_YOUR_CALM",
  "NGHTTP2_INADEQUATE_SECURITY", "NGHTTP2_HTTP_1_1_REQUIRED",
];

// SETTINGS ids in the order Node serializes them.
const SETTING_IDS = [
  { id: 1, key: "headerTableSize", idx: 0 },
  { id: 2, key: "enablePush", idx: 1 },
  { id: 3, key: "maxConcurrentStreams", idx: 4 },
  { id: 4, key: "initialWindowSize", idx: 2 },
  { id: 5, key: "maxFrameSize", idx: 3 },
  { id: 6, key: "maxHeaderListSize", idx: 5 },
  { id: 8, key: "enableConnectProtocol", idx: 6 },
];
const settingsBuffer = new Uint32Array(8);
const optionsBuffer = new Uint32Array(11);
const sessionStateBuffer = new Float64Array(9);
const streamStateBuffer = new Float64Array(6);
const DEFAULT_SETTINGS = {
  headerTableSize: 4096, enablePush: 1, maxConcurrentStreams: 4294967295, initialWindowSize: 65535,
  maxFrameSize: 16384, maxHeaderListSize: 65535, enableConnectProtocol: 0,
};

const MAX_HEADERS_LENGTH = 65536;
const kBitfield = 0, kSessionPriorityListenerCount = 1, kSessionFrameErrorListenerCount = 2,
  kSessionMaxInvalidFrames = 4, kSessionMaxRejectedStreams = 8, kSessionUint8FieldCount = 12;
const kSessionHasRemoteSettingsListeners = 0, kSessionRemoteSettingsIsUpToDate = 1,
  kSessionHasPingListeners = 2, kSessionHasAltsvcListeners = 3;

let callbacks = null;

// Node runs the nextTick queue whenever the outermost native-to-JS callback returns, so work a
// callback schedules happens before the session handles its next frame.
let callbackDepth = 0;
function makeCallback(fn, self, args) {
  callbackDepth++;
  let result;
  try {
    result = Reflect.apply(fn, self, args);
  } finally {
    callbackDepth--;
  }
  if (callbackDepth === 0) process._tickCallback();
  return result;
}

function frameBytes(type, flags, streamId, payload) {
  const length = payload.length;
  const frame = Buffer.allocUnsafe(9 + length);
  frame[0] = length >>> 16;
  frame[1] = length >>> 8;
  frame[2] = length;
  frame[3] = type;
  frame[4] = flags;
  frame.writeUInt32BE(streamId >>> 0, 5);
  if (length !== 0) frame.set(payload, 9);
  return frame;
}

// "name\0value\0F" repeated (F: one char, the NV flags), as util.js mapToHeaders builds it.
function parseHeaderList(list) {
  const text = list[0];
  const out = [];
  let p = 0;
  while (p < text.length) {
    const nameEnd = text.indexOf("\0", p);
    const valueEnd = text.indexOf("\0", nameEnd + 1);
    out.push([text.slice(p, nameEnd), text.slice(nameEnd + 1, valueEnd), text.charCodeAt(valueEnd + 1) === 1]);
    p = valueEnd + 2;
  }
  return out;
}

const isConnectionSpecific = new Set(["connection", "keep-alive", "proxy-connection", "transfer-encoding", "upgrade"]);

class Http2Ping {
  constructor(callback, payload) {
    this.callback = callback;
    this.payload = payload;
    this.start = performance.now();
  }
  done(ack, payload) {
    const duration = ack ? performance.now() - this.start : 0;
    this.callback(ack, duration, payload ?? Buffer.alloc(8));
  }
}

class Http2Stream {
  constructor(session, id) {
    this.session = session;
    this._id = id;
    this.onread = null;
    this.reading = false;
    this._asyncId = newAsyncId();
    this.state = 0;
    this.localEnded = false;
    this.remoteEnded = false;
    this.headersSent = false;
    this.headersReceived = 0;
    this.closed = false;
    this.destroyed = false;
    this.rstCode = 0;
    this.sendWindow = session._remote.initialWindowSize;
    this.localWindow = session._local.initialWindowSize;
    this.consumed = 0;
    this.pausedConsumed = 0;
    this.sendQueue = [];
    this.sendQueueBytes = 0;
    this.shutdownRequested = false;
    this.getTrailers = false;
    this.trailersRequested = false;
    this.frameFailed = false;
    this.pendingHeaders = null;
    this.contentLength = -1;
    this.receivedBytes = 0;
    this.noBody = false;
    this.weight = 16;
    this.parent = 0;
    this.isPush = false;
    this.finalResponse = false;
    this.counted = null;
  }

  id() { return this._id; }
  getAsyncId() { return this._asyncId; }
  getProviderType() { return 0; }
  isStreamBase = false;
  get writeQueueSize() { return this.sendQueueBytes; }

  _writable() {
    return !this.closed && !this.destroyed && !this.localEnded;
  }

  respond(list, options) {
    if (this.closed || this.destroyed) return NG_ERR_STREAM_CLOSED;
    // A stream already shut down (HEAD, 204, 304) has no payload to follow the headers.
    const empty = (options & STREAM_OPTION_EMPTY_PAYLOAD) !== 0 ||
      (this.shutdownRequested && this.sendQueueBytes === 0);
    const flags = empty ? FLAG_END_STREAM : 0;
    if ((options & STREAM_OPTION_GET_TRAILERS) && !empty) this.getTrailers = true;
    return this.session._sendHeaders(this, parseHeaderList(list), flags);
  }

  info(list) {
    if (this.closed || this.destroyed) return NG_ERR_STREAM_CLOSED;
    return this.session._sendHeaders(this, parseHeaderList(list), 0);
  }

  trailers(list) {
    if (this.closed || this.destroyed) return NG_ERR_STREAM_CLOSED;
    this.trailersRequested = false;
    const headers = parseHeaderList(list);
    if (headers.length === 0) {
      this.session._queue(FRAME_DATA, FLAG_END_STREAM, this._id, Buffer.alloc(0));
      this._localEnd();
      return 0;
    }
    const ret = this.session._sendHeaders(this, headers, FLAG_END_STREAM);
    if (ret === 0 && !this.frameFailed) this._localEnd();
    return ret;
  }

  pushPromise(list, options) {
    const session = this.session;
    if (this.closed || this.destroyed) return NG_ERR_STREAM_CLOSED;
    if (session._remote.enablePush === 0) return NG_ERR_PUSH_DISABLED;
    if (session.nextPushId > MAX_WINDOW) return NG_ERR_STREAM_ID_NOT_AVAILABLE;
    const id = session.nextPushId;
    session.nextPushId += 2;
    const pushed = new Http2Stream(session, id);
    pushed.isPush = true;
    pushed.remoteEnded = true;
    pushed.state = 3;
    pushed.localEnded = (options & STREAM_OPTION_EMPTY_PAYLOAD) !== 0;
    session.streams.set(id, pushed);
    session._sendPushPromise(this, pushed, parseHeaderList(list));
    return pushed;
  }

  rstStream(code) {
    if (this.closed || this.destroyed) return 0;
    // Data already written goes out ahead of the reset, as Node purges pending data first.
    this.session._pumpData();
    if (this.frameFailed && !this.headersSent) {
      this._close(code, true);
      return 0;
    }
    this.session._queue(FRAME_RST_STREAM, 0, this._id, Buffer.from([code >>> 24, code >>> 16, code >>> 8, code]));
    this._close(code, true);
    return 0;
  }

  priority(parent, weight, exclusive, silent) {
    weight = Math.min(256, Math.max(1, weight));
    this.parent = parent;
    this.weight = weight;
    if (!silent && !this.closed) {
      const payload = Buffer.alloc(5);
      payload.writeUInt32BE(((exclusive ? 0x80000000 : 0) | parent) >>> 0, 0);
      payload[4] = weight - 1;
      this.session._queue(FRAME_PRIORITY, 0, this._id, payload);
    }
    return 0;
  }

  refreshState() {
    streamStateBuffer[0] = this._stateCode();
    streamStateBuffer[1] = this.weight;
    streamStateBuffer[2] = 0;
    streamStateBuffer[3] = this.localEnded ? 1 : 0;
    streamStateBuffer[4] = this.remoteEnded ? 1 : 0;
    streamStateBuffer[5] = this.localWindow;
  }

  _stateCode() {
    if (this.closed) return 7;
    if (this.isPush && !this.headersSent) return this.session.type === SESSION_SERVER ? 3 : 4;
    if (this.localEnded && this.remoteEnded) return 7;
    if (this.localEnded) return 5;
    if (this.remoteEnded) return 6;
    return 2;
  }

  readStart() {
    if (this.destroyed) return -9;
    this.reading = true;
    if (this.pausedConsumed > 0) {
      const n = this.pausedConsumed;
      this.pausedConsumed = 0;
      this.session._consumeStream(this, n);
    }
    return 0;
  }

  readStop() {
    this.reading = false;
    return 0;
  }

  shutdown(_req) {
    if (this.destroyed) return -32;
    this.shutdownRequested = true;
    this.session._schedule();
    return 1;
  }

  _queueWrite(req, bytes) {
    streamBaseState[kBytesWritten] = bytes.length;
    streamBaseState[kLastWriteWasAsync] = 1;
    if (this.destroyed || this.closed || this.localEnded) {
      // Writes racing the stream's close are dropped, as a destroyed stream drops them.
      const status = this.localEnded && !this.closed && !this.destroyed ? UV_EOF : 0;
      setImmediate(() => { if (typeof req.oncomplete === "function") req.oncomplete(status); });
      return 0;
    }
    if (bytes.length === 0) {
      setImmediate(() => { if (typeof req.oncomplete === "function") req.oncomplete(0); });
      return 0;
    }
    this.sendQueue.push({ req, bytes, offset: 0 });
    this.sendQueueBytes += bytes.length;
    this.session._schedule();
    return 0;
  }
  writeBuffer(req, data) {
    const bytes = data instanceof Uint8Array ? data : new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    return this._queueWrite(req, bytes);
  }
  writeUtf8String(req, s) { return this._queueWrite(req, Buffer.from(s, "utf8")); }
  writeLatin1String(req, s) { return this._queueWrite(req, Buffer.from(s, "latin1")); }
  writeAsciiString(req, s) { return this._queueWrite(req, Buffer.from(s, "latin1")); }
  writeUcs2String(req, s) { return this._queueWrite(req, Buffer.from(s, "utf16le")); }
  writev(req, chunks, allBuffers) {
    const parts = [];
    if (allBuffers) {
      for (let i = 0; i < chunks.length; i++) parts.push(chunks[i]);
    } else {
      for (let i = 0; i < chunks.length; i += 2) {
        const chunk = chunks[i];
        parts.push(typeof chunk === "string" ? Buffer.from(chunk, chunks[i + 1]) : chunk);
      }
    }
    return this._queueWrite(req, Buffer.concat(parts));
  }

  _localEnd() {
    this.localEnded = true;
    if (this.remoteEnded) this._close(ERR_NO_ERROR);
  }

  _remoteEnd() {
    this.remoteEnded = true;
    if (this.localEnded) this._close(ERR_NO_ERROR);
  }

  // Cancels what is still queued for sending and tells JS the stream is over; `defer` runs the
  // JS callback after the current call stack, as nghttp2 reports a stream it reset when the
  // RST_STREAM frame goes out.
  _close(code, defer = false) {
    if (this.closed) return;
    this.closed = true;
    this.rstCode = code;
    const session = this.session;
    session.streams.delete(this._id);
    if (this.counted === "out") {
      session.activeOutgoing--;
      session._releaseOutgoing();
    } else if (this.counted === "in") {
      session.activeIncoming--;
    }
    this.counted = null;
    const queue = this.sendQueue;
    this.sendQueue = [];
    this.sendQueueBytes = 0;
    if (this.pausedConsumed > 0) {
      session._consumeConnection(this.pausedConsumed);
      this.pausedConsumed = 0;
    }
    const notify = () => {
      if (!this.destroyed && callbacks !== null) makeCallback(callbacks.onStreamClose, this, [code]);
    };
    if (defer) setImmediate(notify);
    else notify();
    // Cancelled writes complete once the stream's owner has seen it close; an owner still draining
    // its readable side must not see an error for them.
    if (queue.length > 0) {
      setImmediate(() => {
        for (const entry of queue) {
          if (typeof entry.req.oncomplete === "function") entry.req.oncomplete(0);
        }
      });
    }
  }

  destroy() {
    this.destroyed = true;
    this.session.streams.delete(this._id);
  }
}

class Http2Session {
  constructor(type) {
    this.type = type;
    this.fields = new Uint8Array(kSessionUint8FieldCount);
    this._asyncId = newAsyncId();
    this.ondone = null;
    this.chunksSentSinceLastWrite = 0;
    this.chunksSent = 0;

    const flags = optionsBuffer[10];
    const opt = (bit, fallback) => ((flags & (1 << bit)) !== 0 ? optionsBuffer[bit] : fallback);
    this.maxDeflateDynamicTableSize = opt(0, 4096);
    this.maxReservedRemoteStreams = opt(1, 200);
    this.maxSendHeaderBlockLength = (flags & (1 << 2)) !== 0 ? optionsBuffer[2] : 0;
    this.peerMaxConcurrentStreams = opt(3, 100);
    this.paddingStrategy = opt(4, 0);
    this.maxHeaderListPairs = Math.max(type === SESSION_SERVER ? 4 : 1, opt(5, 128));
    this.maxOutstandingPings = opt(6, 10);
    this.maxOutstandingSettings = opt(7, 10);
    this.maxSettings = opt(9, 32);

    this._local = { ...DEFAULT_SETTINGS };
    this._remote = { ...DEFAULT_SETTINGS, maxConcurrentStreams: this.peerMaxConcurrentStreams };
    this.remoteSettingsKnown = false;
    this.pendingLocalMaxConcurrent = DEFAULT_SETTINGS.maxConcurrentStreams;
    this.pendingSettings = [];
    this.pings = [];
    this.encoder = new __lumenHttp2Codec.Encoder(4096);
    this.decoder = new __lumenHttp2Codec.Decoder(4096);
    this.streams = new Map();
    this.nextStreamId = type === SESSION_CLIENT ? 1 : 2;
    this.nextPushId = 2;
    this.lastPeerStreamId = 0;
    this.lastProcStreamId = 0;
    this.activeOutgoing = 0;
    this.activeIncoming = 0;
    this.deferred = [];
    this.connSendWindow = 65535;
    this.connLocalWindow = 65535;
    this.connLocalTarget = 65535;
    this.connConsumed = 0;
    this.inBuf = Buffer.alloc(0);
    this.gotPreface = type === SESSION_CLIENT;
    this.sentPreface = type === SESSION_SERVER;
    this.outQ = [];
    this.flushScheduled = false;
    this.rstSent = 0;
    this.headerBlock = null;
    this.goawaySent = false;
    this.goawaySentLastId = 0;
    this.goawayReceived = false;
    this.terminated = false;
    this.destroyed = false;
    this.socket = null;
    this.invalidFrames = 0;
    this.rejectedStreams = 0;
    this.writeCompletions = [];
    this.writesInFlight = 0;
    this.pendingDone = false;
    this.ondata = (chunk) => this.receive(chunk);
  }

  getAsyncId() { return this._asyncId; }
  getProviderType() { return 0; }
  _field32(offset) {
    return new DataView(this.fields.buffer, this.fields.byteOffset).getUint32(offset, true);
  }

  consume(socket) {
    this.socket = socket;
    socket.on("data", this.ondata);
    // A socket the application had put in paused or 'readable' mode is read natively in Node.
    if (typeof socket.resume === "function") socket.resume();
  }
  receive(chunk) {
    if (this.destroyed || this.terminated) return;
    this.inBuf = this.inBuf.length === 0 ? Buffer.from(chunk) : Buffer.concat([this.inBuf, chunk]);
    this._process();
  }

  // ---- output ----------------------------------------------------------------------------------

  _queue(type, flags, streamId, payload) {
    if (this.destroyed) return;
    this.outQ.push(frameBytes(type, flags, streamId, payload));
    this._schedule();
  }
  _schedule() {
    if (this.flushScheduled || this.destroyed) return;
    this.flushScheduled = true;
    setImmediate(() => {
      this.flushScheduled = false;
      this._flush();
    });
  }

  _flush() {
    if (this.destroyed && this.outQ.length === 0) return;
    this._pumpData();
    if (this.outQ.length === 0) return;
    const parts = this.outQ;
    this.outQ = [];
    if (!this.sentPreface) {
      this.sentPreface = true;
      parts.unshift(CLIENT_PREFACE);
    }
    const completions = this.writeCompletions;
    this.writeCompletions = [];
    const socket = this.socket;
    if (socket === null || socket.destroyed || !socket.writable) {
      for (const req of completions) if (typeof req.oncomplete === "function") req.oncomplete(0);
      return;
    }
    this.chunksSent++;
    const finish = () => {
      for (const req of completions) if (typeof req.oncomplete === "function") req.oncomplete(0);
      if (this.pendingDone && this.writesInFlight === 0) this._done();
    };
    this.writesInFlight++;
    socket.write(parts.length === 1 ? parts[0] : Buffer.concat(parts), () => {
      this.writesInFlight--;
      finish();
    });
  }

  _maxPayload() {
    return this._remote.maxFrameSize;
  }

  _padded(type, flags, payload, padStrategyApplies) {
    if (!padStrategyApplies || this.paddingStrategy === 0) return [flags, payload];
    const max = this._maxPayload();
    const len = payload.length;
    let target = len;
    if (this.paddingStrategy === 2) target = max;
    else if (this.paddingStrategy === 1) {
      const r = (len + 9) % 8;
      if (r !== 0) target = Math.min(max, len + (8 - r));
    }
    const pad = target - len;
    if (pad <= 0) return [flags, payload];
    const out = Buffer.alloc(len + pad);
    out[0] = pad - 1;
    out.set(payload, 1);
    return [flags | FLAG_PADDED, out];
  }

  // DATA frames from the stream send queues, within the connection and stream send windows.
  _pumpData() {
    let progressed = true;
    while (progressed) {
      progressed = false;
      for (const stream of this.streams.values()) {
        if (!stream.headersSent || stream.localEnded || stream.closed) continue;
        if (this._pumpStream(stream)) progressed = true;
      }
    }
  }

  _pumpStream(stream) {
    const max = this._maxPayload();
    if (stream.sendQueueBytes === 0) {
      if (!stream.shutdownRequested) return false;
      if (stream.getTrailers) {
        if (!stream.trailersRequested) {
          stream.trailersRequested = true;
          if (callbacks !== null) makeCallback(callbacks.onStreamTrailers, stream, []);
        }
        return false;
      }
      this._queueDataFrame(stream, Buffer.alloc(0), true);
      stream._localEnd();
      return true;
    }
    const allowed = Math.min(stream.sendWindow, this.connSendWindow, max - (this.paddingStrategy === 2 ? 255 : 0));
    if (allowed <= 0) return false;
    const take = Math.min(allowed, stream.sendQueueBytes);
    let chunk;
    const first = stream.sendQueue[0];
    if (first.bytes.length - first.offset >= take) {
      chunk = first.bytes.subarray(first.offset, first.offset + take);
    } else {
      const parts = [];
      let need = take;
      for (let i = 0; need > 0; i++) {
        const entry = stream.sendQueue[i];
        const n = Math.min(need, entry.bytes.length - entry.offset);
        parts.push(entry.bytes.subarray(entry.offset, entry.offset + n));
        need -= n;
      }
      chunk = Buffer.concat(parts);
    }
    let left = take;
    while (left > 0) {
      const entry = stream.sendQueue[0];
      const n = Math.min(left, entry.bytes.length - entry.offset);
      entry.offset += n;
      left -= n;
      if (entry.offset === entry.bytes.length) {
        stream.sendQueue.shift();
        this.writeCompletions.push(entry.req);
      }
    }
    stream.sendQueueBytes -= take;
    stream.sendWindow -= take;
    this.connSendWindow -= take;
    const end = stream.sendQueueBytes === 0 && stream.shutdownRequested && !stream.getTrailers;
    this._queueDataFrame(stream, chunk, end);
    if (end) stream._localEnd();
    return true;
  }

  _queueDataFrame(stream, chunk, end) {
    const [flags, payload] = this._padded(FRAME_DATA, end ? FLAG_END_STREAM : 0, chunk, true);
    this.outQ.push(frameBytes(FRAME_DATA, flags, stream._id, payload));
  }

  _sendHeaderFrames(type, flags, streamId, prefix, block) {
    const max = this._maxPayload();
    const first = Math.min(block.length, max - prefix.length);
    const parts = [];
    let endHeaders = first === block.length ? FLAG_END_HEADERS : 0;
    let firstPayload = Buffer.concat([prefix, block.subarray(0, first)]);
    let frameFlags = flags | endHeaders;
    if (type === FRAME_HEADERS) {
      const padded = this._padded(type, frameFlags, firstPayload, true);
      frameFlags = padded[0];
      firstPayload = padded[1];
      if (frameFlags & FLAG_PADDED && (flags & FLAG_PRIORITY)) {
        // The padding length precedes the priority fields.
        const body = firstPayload.subarray(1, firstPayload.length - (firstPayload[0]));
        void body;
      }
    }
    parts.push(frameBytes(type, frameFlags, streamId, firstPayload));
    for (let off = first; off < block.length; off += max) {
      const part = block.subarray(off, Math.min(block.length, off + max));
      parts.push(frameBytes(FRAME_CONTINUATION, off + part.length >= block.length ? FLAG_END_HEADERS : 0, streamId, part));
    }
    for (const part of parts) this.outQ.push(part);
    this._schedule();
  }

  _encodeBlock(headers) {
    // nghttp2 strips header fields whose value holds control characters.
    const valid = headers.filter(([, value]) => {
      for (let i = 0; i < value.length; i++) {
        const c = value.charCodeAt(i) & 0xff;
        if ((c < 0x20 && c !== 0x09) || c === 0x7f) return false;
      }
      return true;
    });
    const block = this.encoder.encode(valid);
    return block;
  }

  _sendHeaders(stream, headers, flags) {
    return this._emitHeaders(stream, headers, flags);
  }

  // nghttp2 accepts the submission and reports a header block it cannot send when the frame goes
  // out: a frameError, then the stream closes with the matching error code.
  _frameNotSent(stream, type, code) {
    setImmediate(() => {
      if (stream.closed || stream.destroyed || this.destroyed) return;
      if (callbacks !== null) makeCallback(callbacks.onFrameError, this, [stream._id, type, code]);
      if (!stream.closed && !stream.destroyed) {
        stream._close(code, false);
      }
    });
  }

  _emitHeaders(stream, headers, flags, priority) {
    let fieldBytes = 0;
    for (const [name, value] of headers) fieldBytes += name.length + value.length;
    const block = fieldBytes > MAX_HEADERS_LENGTH ? null : this._encodeBlock(headers);
    if (block === null || (this.maxSendHeaderBlockLength !== 0 && block.length > this.maxSendHeaderBlockLength)) {
      stream.frameFailed = true;
      this._frameNotSent(stream, FRAME_HEADERS, ERR_FRAME_SIZE);
      return 0;
    }
    let prefix = Buffer.alloc(0);
    let frameFlags = flags;
    if (priority) {
      prefix = Buffer.alloc(5);
      prefix.writeUInt32BE(((priority.exclusive ? 0x80000000 : 0) | priority.parent) >>> 0, 0);
      prefix[4] = priority.weight - 1;
      frameFlags |= FLAG_PRIORITY;
    }
    stream.headersSent = true;
    this._sendHeaderFrames(FRAME_HEADERS, frameFlags, stream._id, prefix, block);
    if (flags & FLAG_END_STREAM) stream._localEnd();
    return 0;
  }

  _sendPushPromise(parent, pushed, headers) {
    const block = this._encodeBlock(headers);
    const prefix = Buffer.alloc(4);
    prefix.writeUInt32BE(pushed._id, 0);
    this._sendHeaderFrames(FRAME_PUSH_PROMISE, 0, parent._id, prefix, block);
  }

  _releaseOutgoing() {
    while (this.deferred.length !== 0 && this.activeOutgoing < this._remote.maxConcurrentStreams) {
      const stream = this.deferred.shift();
      if (stream.closed || stream.destroyed) continue;
      const pending = stream.pendingHeaders;
      stream.pendingHeaders = null;
      stream.counted = "out";
      this.activeOutgoing++;
      this._emitHeaders(stream, pending.headers, pending.flags, pending.priority);
    }
  }

  // ---- the JS-facing API -----------------------------------------------------------------------

  request(list, options, parent, weight, exclusive) {
    if (this.goawayReceived || this.terminated) return NG_ERR_START_STREAM_NOT_ALLOWED;
    if (this.nextStreamId > MAX_WINDOW) return NG_ERR_STREAM_ID_NOT_AVAILABLE;
    const id = this.nextStreamId;
    if (parent === id) return NG_ERR_INVALID_ARGUMENT;
    weight = Math.min(256, Math.max(1, weight));
    this.nextStreamId += 2;
    const stream = new Http2Stream(this, id);
    stream.state = 2;
    if (options & STREAM_OPTION_GET_TRAILERS) stream.getTrailers = true;
    this.streams.set(id, stream);
    const flags = (options & STREAM_OPTION_EMPTY_PAYLOAD) ? FLAG_END_STREAM : 0;
    const headers = parseHeaderList(list);
    stream.noBody = headers.some((h) => h[0] === ":method" && h[1] === "HEAD");
    const priority = parent !== 0 || weight !== 16 || exclusive ?
      { parent, weight, exclusive } : null;
    if (priority) {
      stream.parent = parent;
      stream.weight = weight;
    }
    if (this.activeOutgoing >= this._remote.maxConcurrentStreams) {
      stream.pendingHeaders = { headers, flags, priority };
      this.deferred.push(stream);
      return stream;
    }
    stream.counted = "out";
    this.activeOutgoing++;
    const ret = this._emitHeaders(stream, headers, flags, priority);
    if (ret !== 0) {
      this.streams.delete(id);
      stream.counted = null;
      this.activeOutgoing--;
      return ret;
    }
    return stream;
  }

  settings(callback) {
    if (this.pendingSettings.length >= this.maxOutstandingSettings) return false;
    const flags = settingsBuffer[7];
    const entries = [];
    for (const { id, key, idx } of SETTING_IDS) {
      if (flags & (1 << idx)) entries.push([id, key, settingsBuffer[idx]]);
    }
    const payload = Buffer.alloc(entries.length * 6);
    entries.forEach(([id, , value], i) => {
      payload.writeUInt16BE(id, i * 6);
      payload.writeUInt32BE(value >>> 0, i * 6 + 2);
      if (id === 3) this.pendingLocalMaxConcurrent = value;
    });
    this.pendingSettings.push({ entries, callback, start: performance.now() });
    this._queue(FRAME_SETTINGS, 0, 0, payload);
    return true;
  }

  fillSettings(source) {
    let flags = 0;
    for (const { key, idx } of SETTING_IDS) {
      settingsBuffer[idx] = Number(source[key]);
      flags |= 1 << idx;
    }
    settingsBuffer[7] = flags;
  }
  localSettings() { this.fillSettings(this._local); }
  remoteSettings() { this.fillSettings(this._remote); }

  ping(payload, callback) {
    if (this.pings.length >= this.maxOutstandingPings) return false;
    const data = Buffer.alloc(8);
    if (payload) data.set(payload.subarray ? payload.subarray(0, 8) : payload);
    this.pings.push(new Http2Ping(callback, data));
    this._queue(FRAME_PING, 0, 0, data);
    return true;
  }

  goaway(code, lastStreamID, opaque) {
    const last = lastStreamID > 0 ? lastStreamID : this.lastProcStreamId;
    this._sendGoaway(last, code, opaque);
  }
  _sendGoaway(last, code, opaque) {
    const data = opaque ? Buffer.from(opaque.buffer ? new Uint8Array(opaque.buffer, opaque.byteOffset, opaque.byteLength) : opaque) : Buffer.alloc(0);
    const payload = Buffer.alloc(8 + data.length);
    payload.writeUInt32BE(last, 0);
    payload.writeUInt32BE(code, 4);
    payload.set(data, 8);
    this.goawaySent = true;
    this.goawaySentLastId = last;
    this._queue(FRAME_GOAWAY, 0, 0, payload);
  }

  altsvc(stream, origin, alt) {
    if (stream !== 0 && !this.streams.has(stream)) return;
    const originBytes = Buffer.from(origin, "latin1");
    const altBytes = Buffer.from(alt, "latin1");
    const payload = Buffer.alloc(2 + originBytes.length + altBytes.length);
    payload.writeUInt16BE(originBytes.length, 0);
    payload.set(originBytes, 2);
    payload.set(altBytes, 2 + originBytes.length);
    this._queue(FRAME_ALTSVC, 0, stream, payload);
  }

  origin(text, count) {
    const origins = text.split("\0").slice(0, count);
    const parts = [];
    for (const origin of origins) {
      const bytes = Buffer.from(origin, "latin1");
      const head = Buffer.alloc(2);
      head.writeUInt16BE(bytes.length, 0);
      parts.push(head, bytes);
    }
    this._queue(FRAME_ORIGIN, 0, 0, Buffer.concat(parts));
  }

  setNextStreamID(id) {
    if (id < this.nextStreamId || id > MAX_WINDOW) return false;
    this.nextStreamId = id;
    return true;
  }

  setLocalWindowSize(size) {
    const delta = size - this.connLocalTarget;
    this.connLocalTarget = size;
    // The advertised window only ever grows by a WINDOW_UPDATE.
    if (delta > 0) {
      this.connLocalWindow += delta;
      this._windowUpdate(0, delta);
    }
    return 0;
  }

  refreshState() {
    sessionStateBuffer[0] = this.connLocalTarget;
    sessionStateBuffer[1] = this.connConsumed;
    sessionStateBuffer[2] = this.nextStreamId;
    sessionStateBuffer[3] = this.connLocalWindow;
    sessionStateBuffer[4] = this.lastProcStreamId;
    sessionStateBuffer[5] = this.connSendWindow;
    sessionStateBuffer[6] = this.outQ.length;
    sessionStateBuffer[7] = this.encoder.table.size;
    sessionStateBuffer[8] = this.decoder.table.size;
  }

  updateChunksSent() {
    this.chunksSentSinceLastWrite = this.chunksSent;
    return this.chunksSent;
  }

  destroy(code, socketDestroyed) {
    if (this.destroyed) return;
    this._pumpData();
    if (!socketDestroyed && !this.terminated && this.socket !== null) {
      this._sendGoaway(this.lastProcStreamId, code);
    }
    const alive = this.socket;
    this.flushScheduled = false;
    this._flush();
    this.destroyed = true;
    if (alive !== null) alive.removeListener("data", this.ondata);
    for (const stream of Array.from(this.streams.values())) stream.destroyed = true;
    this.streams.clear();
    setImmediate(() => {
      for (const ping of this.pings.splice(0)) ping.done(false);
      this.pendingSettings.length = 0;
    });
    this._done();
  }
  _done() {
    this.pendingDone = false;
    setImmediate(() => {
      if (typeof this.ondone === "function") this.ondone();
    });
  }

  // ---- input -----------------------------------------------------------------------------------

  _connError(code) {
    if (this.terminated) return;
    // nghttp2 starts no stream once the session is terminating.
    this.outQ = this.outQ.filter((f) => f[3] !== FRAME_HEADERS && f[3] !== FRAME_PUSH_PROMISE && f[3] !== FRAME_CONTINUATION);
    this._sendGoaway(this.lastProcStreamId, code);
    this._flush();
    this.terminated = true;
    for (const stream of Array.from(this.streams.values())) stream._close(code, true);
  }

  _internalError(code, custom) {
    this.terminated = true;
    if (callbacks !== null) makeCallback(callbacks.onSessionInternalError, this, custom === undefined ? [code] : [code, custom]);
  }

  _streamError(streamId, code, invalid = true) {
    this._queue(FRAME_RST_STREAM, 0, streamId, Buffer.from([code >>> 24, code >>> 16, code >>> 8, code]));
    const stream = this.streams.get(streamId);
    if (stream !== undefined) stream._close(code, true);
    if (invalid) this._countInvalid();
  }

  _countInvalid() {
    this.invalidFrames++;
    const limit = this._field32(kSessionMaxInvalidFrames) || 1000;
    if (this.invalidFrames > limit && !this.terminated) {
      this._connError(ERR_ENHANCE_YOUR_CALM);
      this._internalError(NG_ERR_PROTO, "ERR_HTTP2_TOO_MANY_INVALID_FRAMES");
    }
  }

  _process() {
    if (!this.gotPreface) {
      if (this.inBuf.length < CLIENT_PREFACE.length) {
        if (!CLIENT_PREFACE.subarray(0, this.inBuf.length).equals(this.inBuf)) this._badMagic();
        return;
      }
      if (!this.inBuf.subarray(0, CLIENT_PREFACE.length).equals(CLIENT_PREFACE)) { this._badMagic(); return; }
      this.inBuf = this.inBuf.subarray(CLIENT_PREFACE.length);
      this.gotPreface = true;
    }
    while (!this.terminated && !this.destroyed && this.inBuf.length >= 9) {
      const buf = this.inBuf;
      const length = (buf[0] << 16) | (buf[1] << 8) | buf[2];
      if (length > this._local.maxFrameSize) {
        this._connError(ERR_FRAME_SIZE);
        return;
      }
      if (buf.length < 9 + length) break;
      const type = buf[3];
      const flags = buf[4];
      const streamId = buf.readUInt32BE(5) & 0x7fffffff;
      const payload = buf.subarray(9, 9 + length);
      this.inBuf = buf.subarray(9 + length);
      this._frame(type, flags, streamId, payload);
    }
  }

  _badMagic() {
    this.inBuf = Buffer.alloc(0);
    this._internalError(NG_ERR_BAD_CLIENT_MAGIC);
  }

  _frame(type, flags, streamId, payload) {
    if (this.headerBlock !== null && type !== FRAME_CONTINUATION) return this._connError(ERR_PROTOCOL);
    switch (type) {
      case FRAME_DATA: return this._onData(flags, streamId, payload);
      case FRAME_HEADERS: return this._onHeaders(flags, streamId, payload);
      case FRAME_PRIORITY: return this._onPriority(streamId, payload);
      case FRAME_RST_STREAM: return this._onRst(streamId, payload);
      case FRAME_SETTINGS: return this._onSettings(flags, streamId, payload);
      case FRAME_PUSH_PROMISE: return this._onPushPromise(flags, streamId, payload);
      case FRAME_PING: return this._onPing(flags, streamId, payload);
      case FRAME_GOAWAY: return this._onGoaway(streamId, payload);
      case FRAME_WINDOW_UPDATE: return this._onWindowUpdate(streamId, payload);
      case FRAME_CONTINUATION: return this._onContinuation(flags, streamId, payload);
      case FRAME_ALTSVC: return this._onAltsvc(streamId, payload);
      case FRAME_ORIGIN: return this._onOrigin(streamId, payload);
      default: return undefined;
    }
  }

  _unpad(flags, payload) {
    if (!(flags & FLAG_PADDED)) return payload;
    if (payload.length < 1) return null;
    const pad = payload[0];
    if (pad >= payload.length) return null;
    return payload.subarray(1, payload.length - pad);
  }

  _nextLocalId() {
    return this.type === SESSION_CLIENT ? this.nextStreamId : this.nextPushId;
  }

  _isLocalId(id) {
    return id % 2 === (this.type === SESSION_CLIENT ? 1 : 0);
  }

  _consumeConnection(n) {
    this.connConsumed += n;
    if (this.connConsumed * 2 >= this.connLocalTarget) {
      const inc = this.connConsumed;
      this.connConsumed = 0;
      this.connLocalWindow += inc;
      this._windowUpdate(0, inc);
    }
  }
  _consumeStream(stream, n) {
    if (stream.closed) return;
    stream.consumed += n;
    if (stream.consumed * 2 >= this._local.initialWindowSize) {
      const inc = stream.consumed;
      stream.consumed = 0;
      stream.localWindow += inc;
      this._windowUpdate(stream._id, inc);
    }
  }
  _windowUpdate(streamId, inc) {
    if (inc <= 0) return;
    this._queue(FRAME_WINDOW_UPDATE, 0, streamId, Buffer.from([inc >>> 24, inc >>> 16, inc >>> 8, inc]));
  }

  _onData(flags, streamId, payload) {
    if (streamId === 0) return this._connError(ERR_PROTOCOL);
    const total = payload.length;
    this.connLocalWindow -= total;
    if (this.connLocalWindow < 0) return this._connError(ERR_FLOW_CONTROL);
    const stream = this.streams.get(streamId);
    if (stream === undefined || stream.closed) {
      const idle = this._isLocalId(streamId) ? streamId >= this._nextLocalId() : streamId > this.lastPeerStreamId;
      if (idle) return this._connError(ERR_PROTOCOL);
      this._consumeConnection(total);
      return undefined;
    }
    if (stream.remoteEnded) {
      this._consumeConnection(total);
      return this._streamError(streamId, ERR_STREAM_CLOSED);
    }
    const data = this._unpad(flags, payload);
    if (data === null) return this._connError(ERR_PROTOCOL);
    stream.localWindow -= total;
    if (stream.localWindow < 0) {
      this._consumeConnection(total);
      return this._streamError(streamId, ERR_FLOW_CONTROL);
    }
    this._consumeConnection(total);
    stream.receivedBytes += data.length;
    if (!stream.noBody && stream.contentLength >= 0 && (stream.receivedBytes > stream.contentLength ||
        ((flags & FLAG_END_STREAM) && stream.receivedBytes !== stream.contentLength))) {
      return this._streamError(streamId, ERR_PROTOCOL);
    }
    if (data.length > 0 && !stream.destroyed && typeof stream.onread === "function") {
      // The application consumes what it was handed after this call, so a peer that sends
      // faster than the window updates return can overrun the stream's window.
      if (stream.reading) setImmediate(() => this._consumeStream(stream, total));
      else stream.pausedConsumed += total;
      const copy = Buffer.from(data);
      streamBaseState[kReadBytesOrError] = copy.length;
      streamBaseState[kArrayBufferOffset] = copy.byteOffset;
      makeCallback(stream.onread, stream, [copy.buffer]);
    } else if (total > 0) {
      this._consumeStream(stream, total);
    }
    if (flags & FLAG_END_STREAM) this._endOfRemote(stream);
    return undefined;
  }

  _endOfRemote(stream) {
    if (!stream.destroyed && typeof stream.onread === "function") {
      streamBaseState[kReadBytesOrError] = UV_EOF;
      streamBaseState[kArrayBufferOffset] = 0;
      makeCallback(stream.onread, stream, [undefined]);
    }
    stream._remoteEnd();
  }

  _onHeaders(flags, streamId, payload) {
    if (streamId === 0) return this._connError(ERR_PROTOCOL);
    let data = this._unpad(flags, payload);
    if (data === null) return this._connError(ERR_PROTOCOL);
    let priority = null;
    if (flags & FLAG_PRIORITY) {
      if (data.length < 5) return this._connError(ERR_FRAME_SIZE);
      const dep = data.readUInt32BE(0);
      priority = { parent: dep & 0x7fffffff, exclusive: (dep & 0x80000000) !== 0, weight: data[4] + 1 };
      data = data.subarray(5);
    }
    const block = { kind: "headers", streamId, flags, priority, parts: [data], size: data.length };
    if (flags & FLAG_END_HEADERS) this._headersDone(block);
    else this.headerBlock = block;
    return undefined;
  }

  _onPushPromise(flags, streamId, payload) {
    if (this.type === SESSION_SERVER || streamId === 0 || this._local.enablePush === 0) {
      return this._connError(ERR_PROTOCOL);
    }
    let data = this._unpad(flags, payload);
    if (data === null || data.length < 4) return this._connError(ERR_PROTOCOL);
    const promised = data.readUInt32BE(0) & 0x7fffffff;
    data = data.subarray(4);
    const block = { kind: "push", streamId, promised, flags, parts: [data], size: data.length };
    if (flags & FLAG_END_HEADERS) this._headersDone(block);
    else this.headerBlock = block;
    return undefined;
  }

  _onContinuation(flags, streamId, payload) {
    const block = this.headerBlock;
    if (block === null || block.streamId !== streamId) return this._connError(ERR_PROTOCOL);
    block.parts.push(payload);
    block.size += payload.length;
    if (block.size > 4 * 1024 * 1024) return this._connError(ERR_ENHANCE_YOUR_CALM);
    if (flags & FLAG_END_HEADERS) {
      this.headerBlock = null;
      this._headersDone(block);
    }
    return undefined;
  }

  _headersDone(block) {
    this.headerBlock = null;
    let list;
    try {
      list = this.decoder.decode(block.parts.length === 1 ? block.parts[0] : Buffer.concat(block.parts));
    } catch {
      return this._connError(ERR_COMPRESSION);
    }
    if (block.kind === "push") return this._pushPromiseDone(block, list);
    return this._headersReceived(block, list);
  }

  _validate(list, isRequest, isTrailer, isResponse, isConnect) {
    let seenRegular = false;
    let status = 0, method = 0, path = 0, scheme = 0, authority = 0, protocol = 0;
    for (const [name, value] of list) {
      if (name.length === 0) return false;
      for (let i = 0; i < name.length; i++) {
        const c = name.charCodeAt(i);
        if ((c >= 65 && c <= 90) || c <= 32 || c === 127) return false;
      }
      if (name.charCodeAt(0) === 58) {
        if (seenRegular || isTrailer) return false;
        switch (name) {
          case ":status": if (!isResponse || status++) return false; break;
          case ":method": if (!isRequest || method++) return false; break;
          case ":path":
            if (!isRequest || path++ || value.length === 0) return false;
            for (let i = 0; i < value.length; i++) {
              const c = value.charCodeAt(i);
              if (c <= 32 || c === 127) return false;
            }
            break;
          case ":scheme": if (!isRequest || scheme++) return false; break;
          case ":authority": if (!isRequest || authority++) return false; break;
          case ":protocol": if (!isRequest || protocol++) return false; break;
          default: return false;
        }
      } else {
        seenRegular = true;
        if (isConnectionSpecific.has(name)) return false;
        if (name === "te" && value !== "trailers") return false;
      }
    }
    if (isTrailer) return true;
    if (isResponse) return status === 1;
    if (isRequest) {
      if (isConnect && protocol === 0) return method === 1 && authority === 1 && path === 0 && scheme === 0;
      return method === 1 && path === 1 && scheme === 1;
    }
    return true;
  }

  _headersReceived(block, list) {
    const { streamId, flags } = block;
    const endStream = (flags & FLAG_END_STREAM) !== 0;
    let stream = this.streams.get(streamId);
    let cat;
    if (stream === undefined) {
      if (this.type === SESSION_CLIENT) {
        if (!this._isLocalId(streamId) && streamId > this.lastPeerStreamId && streamId % 2 === 0) {
          return this._connError(ERR_PROTOCOL);
        }
        if (this._isLocalId(streamId) && streamId >= this._nextLocalId()) return this._connError(ERR_PROTOCOL);
        return undefined;
      }
      if (streamId % 2 === 0) return this._connError(ERR_PROTOCOL);
      if (streamId <= this.lastPeerStreamId) return this._connError(ERR_STREAM_CLOSED);
      this.lastPeerStreamId = streamId;
      if (this.goawaySent && streamId > this.goawaySentLastId) return undefined;
      this.lastProcStreamId = streamId;
      const limit = Math.min(this._local.maxConcurrentStreams, this.pendingLocalMaxConcurrent);
      if (this.activeIncoming >= limit) {
        this.rejectedStreams++;
        this._queue(FRAME_RST_STREAM, 0, streamId, Buffer.from([0, 0, 0, ERR_REFUSED_STREAM]));
        const maxRejected = this._field32(kSessionMaxRejectedStreams) || 100;
        if (this.rejectedStreams > maxRejected) {
          this._connError(ERR_ENHANCE_YOUR_CALM);
          this._internalError(NG_ERR_PROTO, "ERR_HTTP2_TOO_MANY_INVALID_FRAMES");
        }
        return undefined;
      }
      stream = new Http2Stream(this, streamId);
      stream.state = 2;
      stream.remoteEnded = false;
      this.streams.set(streamId, stream);
      stream.counted = "in";
      this.activeIncoming++;
      cat = HCAT_REQUEST;
      if (!this._validate(list, true, false, false, list.some((h) => h[0] === ":method" && h[1] === "CONNECT"))) {
        return this._streamError(streamId, ERR_PROTOCOL);
      }
    } else {
      if (stream.closed) return undefined;
      if (stream.remoteEnded) return this._streamError(streamId, ERR_STREAM_CLOSED);
      if (this.type === SESSION_CLIENT) {
        if (stream.isPush && stream.headersReceived === 0) {
          cat = HCAT_PUSH_RESPONSE;
          if (!this._validate(list, false, false, true)) return this._streamError(streamId, ERR_PROTOCOL);
        } else if (!stream.finalResponse) {
          cat = stream.headersReceived === 0 ? HCAT_RESPONSE : HCAT_HEADERS;
          if (!this._validate(list, false, false, true)) return this._streamError(streamId, ERR_PROTOCOL);
        } else {
          cat = HCAT_HEADERS;
          if (!endStream || !this._validate(list, false, true, false)) return this._streamError(streamId, ERR_PROTOCOL);
        }
      } else {
        cat = HCAT_HEADERS;
        if (!endStream || !this._validate(list, false, true, false)) return this._streamError(streamId, ERR_PROTOCOL);
      }
    }
    stream.headersReceived++;
    if (block.priority) {
      stream.parent = block.priority.parent;
      stream.weight = block.priority.weight;
    }
    let informational = false;
    if (cat === HCAT_RESPONSE || cat === HCAT_PUSH_RESPONSE) {
      const status = Number(list.find((h) => h[0] === ":status")?.[1]);
      informational = status >= 100 && status < 200;
      if (informational) stream.headersReceived = 0;
      else stream.finalResponse = true;
      if (status === 204 || status === 304) stream.noBody = true;
    }
    if (!informational && (cat === HCAT_REQUEST || cat === HCAT_RESPONSE || cat === HCAT_PUSH_RESPONSE)) {
      const cl = list.find((h) => h[0] === "content-length");
      if (cl !== undefined && /^\d+$/.test(cl[1])) stream.contentLength = Number(cl[1]);
    }
    if (list.length > this.maxHeaderListPairs) {
      return this._streamError(streamId, ERR_ENHANCE_YOUR_CALM, false);
    }
    let size = 0;
    for (const h of list) size += h[0].length + h[1].length + 32;
    if (size > this._local.maxHeaderListSize) {
      return this._streamError(streamId, ERR_ENHANCE_YOUR_CALM, false);
    }
    if (block.priority && this.fields[kSessionPriorityListenerCount] > 0 && cat === HCAT_REQUEST) {
      makeCallback(callbacks.onPriority, this, [streamId, block.priority.parent, block.priority.weight, block.priority.exclusive]);
    }
    if (endStream && !stream.noBody && stream.contentLength >= 0 && stream.receivedBytes !== stream.contentLength) {
      return this._streamError(streamId, ERR_PROTOCOL);
    }
    this._deliverHeaders(stream, cat, flags, list);
    if (endStream && !stream.closed) this._endOfRemote(stream);
    return undefined;
  }

  _deliverHeaders(stream, cat, flags, list) {
    const flat = [];
    const sensitive = [];
    for (const [name, value, never] of list) {
      flat.push(name, value);
      if (never) sensitive.push(name);
    }
    makeCallback(callbacks.onSessionHeaders, this, [stream, stream._id, cat, flags & (FLAG_END_STREAM | FLAG_END_HEADERS | FLAG_PRIORITY), flat, sensitive]);
  }

  _pushPromiseDone(block, list) {
    const { streamId, promised } = block;
    const parent = this.streams.get(streamId);
    if (promised % 2 !== 0 || promised <= this.lastPeerStreamId) return this._connError(ERR_PROTOCOL);
    this.lastPeerStreamId = promised;
    if (parent === undefined || parent.closed) {
      this._queue(FRAME_RST_STREAM, 0, promised, Buffer.from([0, 0, 0, ERR_REFUSED_STREAM]));
      return undefined;
    }
    let reserved = 0;
    for (const s of this.streams.values()) if (s.isPush) reserved++;
    if (reserved >= this.maxReservedRemoteStreams) {
      this._queue(FRAME_RST_STREAM, 0, promised, Buffer.from([0, 0, 0, ERR_CANCEL]));
      return undefined;
    }
    const stream = new Http2Stream(this, promised);
    stream.isPush = true;
    stream.localEnded = true;
    stream.state = 4;
    this.streams.set(promised, stream);
    this._deliverHeaders(stream, HCAT_PUSH_RESPONSE, FLAG_END_HEADERS, list);
    return undefined;
  }

  _onPriority(streamId, payload) {
    if (streamId === 0) return this._connError(ERR_PROTOCOL);
    if (payload.length !== 5) return this._streamError(streamId, ERR_FRAME_SIZE);
    const dep = payload.readUInt32BE(0);
    const parent = dep & 0x7fffffff;
    if (parent === streamId) return this._streamError(streamId, ERR_PROTOCOL);
    const weight = payload[4] + 1;
    const stream = this.streams.get(streamId);
    if (stream !== undefined) { stream.parent = parent; stream.weight = weight; }
    if (this.fields[kSessionPriorityListenerCount] > 0 && callbacks !== null) {
      makeCallback(callbacks.onPriority, this, [streamId, parent, weight, (dep & 0x80000000) !== 0]);
    }
    return undefined;
  }

  _onRst(streamId, payload) {
    if (streamId === 0) return this._connError(ERR_PROTOCOL);
    if (payload.length !== 4) return this._connError(ERR_FRAME_SIZE);
    const idle = this._isLocalId(streamId) ? streamId >= this._nextLocalId() : streamId > this.lastPeerStreamId;
    if (idle) return this._connError(ERR_PROTOCOL);
    const stream = this.streams.get(streamId);
    if (stream !== undefined) stream._close(payload.readUInt32BE(0));
    return undefined;
  }

  _onSettings(flags, streamId, payload) {
    if (streamId !== 0) return this._connError(ERR_PROTOCOL);
    if (flags & FLAG_ACK) {
      if (payload.length !== 0) return this._connError(ERR_FRAME_SIZE);
      const pending = this.pendingSettings.shift();
      if (pending === undefined) {
        this._internalError(NG_ERR_PROTO);
        return undefined;
      }
      this._applyLocal(pending.entries);
      if (typeof pending.callback === "function") pending.callback(true, performance.now() - pending.start);
      return undefined;
    }
    if (payload.length % 6 !== 0) return this._connError(ERR_FRAME_SIZE);
    const count = payload.length / 6;
    if (count > this.maxSettings) {
      this._connError(ERR_ENHANCE_YOUR_CALM);
      this._internalError(NG_ERR_PROTO, "ERR_HTTP2_TOO_MANY_CUSTOM_SETTINGS");
      return undefined;
    }
    // The initially assumed peer limit lapses once the peer's first SETTINGS arrives.
    if (!this.remoteSettingsKnown) this._remote.maxConcurrentStreams = DEFAULT_SETTINGS.maxConcurrentStreams;
    for (let i = 0; i < count; i++) {
      const id = payload.readUInt16BE(i * 6);
      const value = payload.readUInt32BE(i * 6 + 2);
      const error = this._applyRemote(id, value);
      if (error !== 0) return this._connError(error);
    }
    this.remoteSettingsKnown = true;
    this._queue(FRAME_SETTINGS, FLAG_ACK, 0, Buffer.alloc(0));
    this.fields[kBitfield] &= ~(1 << kSessionRemoteSettingsIsUpToDate);
    if ((this.fields[kBitfield] & (1 << kSessionHasRemoteSettingsListeners)) !== 0 && callbacks !== null) {
      makeCallback(callbacks.onSettings, this, []);
    }
    this._releaseOutgoing();
    this._schedule();
    return undefined;
  }

  _applyRemote(id, value) {
    const s = this._remote;
    switch (id) {
      case 1: s.headerTableSize = value; this.encoder.setLimit(Math.min(value, this.maxDeflateDynamicTableSize)); break;
      case 2:
        if (value > 1) return ERR_PROTOCOL;
        s.enablePush = value;
        break;
      case 3: s.maxConcurrentStreams = value; break;
      case 4: {
        if (value > MAX_WINDOW) return ERR_FLOW_CONTROL;
        const delta = value - s.initialWindowSize;
        s.initialWindowSize = value;
        for (const stream of this.streams.values()) {
          stream.sendWindow += delta;
          if (stream.sendWindow > MAX_WINDOW) return ERR_FLOW_CONTROL;
        }
        break;
      }
      case 5:
        if (value < 16384 || value > 16777215) return ERR_PROTOCOL;
        s.maxFrameSize = value;
        break;
      case 6: s.maxHeaderListSize = value; break;
      case 8:
        if (value > 1 || (value === 0 && s.enableConnectProtocol === 1)) return ERR_PROTOCOL;
        s.enableConnectProtocol = value;
        break;
      default: break;
    }
    return 0;
  }

  _applyLocal(entries) {
    const s = this._local;
    for (const [id, key, value] of entries) {
      if (id === 4) {
        const delta = value - s.initialWindowSize;
        for (const stream of this.streams.values()) stream.localWindow += delta;
      }
      if (id === 1) this.decoder.setLimit(value);
      s[key] = value;
    }
    this.pendingLocalMaxConcurrent = s.maxConcurrentStreams;
  }

  _onPing(flags, streamId, payload) {
    if (streamId !== 0) return this._connError(ERR_PROTOCOL);
    if (payload.length !== 8) return this._connError(ERR_FRAME_SIZE);
    if (flags & FLAG_ACK) {
      const ping = this.pings.shift();
      if (ping === undefined) {
        this._internalError(NG_ERR_PROTO);
        return undefined;
      }
      ping.done(true, Buffer.from(payload));
      return undefined;
    }
    this._queue(FRAME_PING, FLAG_ACK, 0, payload);
    if ((this.fields[kBitfield] & (1 << kSessionHasPingListeners)) !== 0 && callbacks !== null) {
      makeCallback(callbacks.onPing, this, [Buffer.from(payload)]);
    }
    return undefined;
  }

  _onGoaway(streamId, payload) {
    if (streamId !== 0) return this._connError(ERR_PROTOCOL);
    if (payload.length < 8) return this._connError(ERR_FRAME_SIZE);
    const last = payload.readUInt32BE(0) & 0x7fffffff;
    const code = payload.readUInt32BE(4);
    const opaque = payload.length > 8 ? Buffer.from(payload.subarray(8)) : undefined;
    this.goawayReceived = true;
    for (const stream of Array.from(this.streams.values())) {
      if (this._isLocalId(stream._id) && stream._id > last) stream._close(ERR_REFUSED_STREAM);
    }
    makeCallback(callbacks.onGoawayData, this, [code, last, opaque]);
    return undefined;
  }

  _onWindowUpdate(streamId, payload) {
    if (payload.length !== 4) return this._connError(ERR_FRAME_SIZE);
    const inc = payload.readUInt32BE(0) & 0x7fffffff;
    if (streamId === 0) {
      if (inc === 0) return this._connError(ERR_PROTOCOL);
      this.connSendWindow += inc;
      if (this.connSendWindow > MAX_WINDOW) return this._connError(ERR_FLOW_CONTROL);
      this._schedule();
      return undefined;
    }
    const stream = this.streams.get(streamId);
    if (stream === undefined) {
      const idle = this._isLocalId(streamId) ? streamId >= this._nextLocalId() : streamId > this.lastPeerStreamId;
      if (idle) return this._connError(ERR_PROTOCOL);
      return undefined;
    }
    if (inc === 0) return this._streamError(streamId, ERR_PROTOCOL);
    stream.sendWindow += inc;
    if (stream.sendWindow > MAX_WINDOW) return this._streamError(streamId, ERR_FLOW_CONTROL);
    this._schedule();
    return undefined;
  }

  _onAltsvc(streamId, payload) {
    if (this.type !== SESSION_CLIENT) return undefined;
    if (payload.length < 2) return this._connError(ERR_FRAME_SIZE);
    const originLength = payload.readUInt16BE(0);
    if (2 + originLength > payload.length) return this._connError(ERR_FRAME_SIZE);
    const origin = payload.subarray(2, 2 + originLength).toString("latin1");
    const alt = payload.subarray(2 + originLength).toString("latin1");
    if ((streamId === 0) === (origin.length === 0)) return undefined;
    if ((this.fields[kBitfield] & (1 << kSessionHasAltsvcListeners)) !== 0 && callbacks !== null) {
      makeCallback(callbacks.onAltSvc, this, [streamId, origin, alt]);
    }
    return undefined;
  }

  _onOrigin(streamId, payload) {
    if (this.type !== SESSION_CLIENT || streamId !== 0) return undefined;
    const origins = [];
    for (let p = 0; p + 2 <= payload.length;) {
      const len = payload.readUInt16BE(p);
      if (p + 2 + len > payload.length) return this._connError(ERR_FRAME_SIZE);
      origins.push(payload.subarray(p + 2, p + 2 + len).toString("latin1"));
      p += 2 + len;
    }
    if (callbacks !== null) makeCallback(callbacks.onOrigin, this, [origins]);
    return undefined;
  }
}

// A file as a read-only byte source for StreamPipe (respondWithFD / respondWithFile).
class NativeFileHandle {
  constructor(fd, offset, length) {
    this.fd = fd;
    this.offset = offset;
    this.length = length;
    this.onread = null;
    this.stream = null;
  }
  releaseFD() {}
  close() {
    return new Promise((resolve, reject) => {
      __builtins.get("fs").close(this.fd, (err) => (err ? reject(err) : resolve()));
    });
  }
}

class StreamPipe {
  constructor(source, sink) {
    this.source = source;
    this.sink = sink;
    this.onunpipe = null;
  }
  start() {
    const fs = __builtins.get("fs");
    const { source, sink } = this;
    let position = source.offset >= 0 ? source.offset : null;
    let remaining = source.length >= 0 ? source.length : Infinity;
    const finish = (status) => {
      streamBaseState[kReadBytesOrError] = status;
      streamBaseState[kArrayBufferOffset] = 0;
      if (typeof source.onread === "function") source.onread(undefined);
      if (typeof this.onunpipe === "function") this.onunpipe();
    };
    const end = () => {
      const req = new ShutdownWrap();
      req.oncomplete = () => finish(UV_EOF);
      req.handle = sink;
      if (sink.shutdown(req) === 1) finish(UV_EOF);
    };
    const next = () => {
      if (sink.destroyed || sink.closed) return finish(UV_EOF);
      if (remaining <= 0) return end();
      const buffer = Buffer.allocUnsafe(Math.min(65536, remaining));
      fs.read(source.fd, buffer, 0, buffer.length, position, (err, n) => {
        if (err) return finish(uvBinding[`UV_${err.code}`] ?? uvBinding.UV_EIO);
        if (n === 0) return end();
        if (position !== null) position += n;
        remaining -= n;
        const req = new WriteWrap();
        req.handle = sink;
        req.oncomplete = (status) => (status < 0 ? finish(status) : next());
        const ret = sink.writeBuffer(req, buffer.subarray(0, n));
        if (ret !== 0) finish(ret);
        return undefined;
      });
      return undefined;
    };
    next();
  }
}

bindings.fs = { FileHandle: NativeFileHandle };
bindings.stream_pipe = { StreamPipe };
bindings.http2 = {
  Http2Session,
  constants,
  nameForErrorCode,
  nghttp2ErrorString,
  settingsBuffer,
  optionsBuffer,
  sessionState: sessionStateBuffer,
  streamState: streamStateBuffer,
  kBitfield, kSessionPriorityListenerCount, kSessionFrameErrorListenerCount, kSessionMaxInvalidFrames,
  kSessionMaxRejectedStreams, kSessionUint8FieldCount, kSessionHasRemoteSettingsListeners,
  kSessionRemoteSettingsIsUpToDate, kSessionHasPingListeners, kSessionHasAltsvcListeners,
  setCallbackFunctions(onSessionInternalError, onPriority, onSettings, onPing, onSessionHeaders,
                       onFrameError, onGoawayData, onAltSvc, onOrigin, onStreamTrailers, onStreamClose) {
    callbacks = {
      onSessionInternalError, onPriority, onSettings, onPing, onSessionHeaders, onFrameError, onGoawayData,
      onAltSvc, onOrigin, onStreamTrailers, onStreamClose,
    };
  },
  refreshDefaultSettings() {
    let flags = 0;
    for (const { key, idx } of SETTING_IDS) {
      settingsBuffer[idx] = DEFAULT_SETTINGS[key];
      flags |= 1 << idx;
    }
    settingsBuffer[7] = flags;
  },
  packSettings() {
    const flags = settingsBuffer[7];
    const entries = SETTING_IDS.filter(({ idx }) => flags & (1 << idx));
    const out = Buffer.alloc(entries.length * 6);
    entries.forEach(({ id, idx }, i) => {
      out.writeUInt16BE(id, i * 6);
      out.writeUInt32BE(settingsBuffer[idx] >>> 0, i * 6 + 2);
    });
    return out;
  },
};

