// An llhttp-style HTTP/1.x parser (a port used by lumen-node's net.js), driven directly: one
// large method (`_run`, a state machine in a switch) whose loops and whole-function code are
// compiled while receivers are dictionary-mode objects. It once took seconds per compile
// (quadratic passes in the backend) and recompiled on every method-guard miss.
// Host shims: the script runs on the core engine (no Buffer / performance / console).
const performance = { now: () => Date.now() };
const Buffer = {
  from: (x, enc) => typeof x === "string" ? Uint8Array.from(x, (c) => c.charCodeAt(0)) : new Uint8Array(x),
  alloc: (n) => new Uint8Array(n),
};
const getOptionValue=()=>16384; const bindings={}; const asyncHookSymbols={async_id_symbol:Symbol()}; let __id=1; const newAsyncId=()=>__id++;
// ---- internalBinding('http_parser'): llhttp in JS ------------------------------------------------
// A port of what Node's http_parser binding (src/node_http_parser.cc) does over llhttp 8.1:
// the same incremental HTTP/1.x state machine, the same callback protocol into _http_common.js
// (kOnMessageBegin / kOnHeaders / kOnHeadersComplete / kOnBody / kOnMessageComplete), the same
// return value of onHeadersComplete (1 = skip the body, 2 = upgrade and skip the body), the same
// keep-alive / needs-EOF rules (llhttp's http.c), header flushing at 32 fields, the maxHeaderSize
// accounting (url, status, field and value bytes; reset at headers-complete and per chunk), the
// upgrade pause, and the "Parse Error" objects with bytesParsed / code / reason.

const HTTP_REQUEST = 1;
const HTTP_RESPONSE = 2;

const kOnMessageBegin = 0;
const kOnHeaders = 1;
const kOnHeadersComplete = 2;
const kOnBody = 3;
const kOnMessageComplete = 4;
const kOnExecute = 5;
const kOnTimeout = 6;

const kLenientNone = 0;
const kLenientHeaders = 1;
const kLenientChunkedLength = 2;
const kLenientKeepAlive = 4;
const kLenientAll = kLenientHeaders | kLenientChunkedLength | kLenientKeepAlive;

const F_CONNECTION_KEEP_ALIVE = 0x1;
const F_CONNECTION_CLOSE = 0x2;
const F_CONNECTION_UPGRADE = 0x4;
const F_CHUNKED = 0x8;
const F_UPGRADE = 0x10;
const F_CONTENT_LENGTH = 0x20;
const F_SKIPBODY = 0x40;
const F_TRAILING = 0x80;
const F_TRANSFER_ENCODING = 0x200;

const FINISH_SAFE = 0;
const FINISH_SAFE_WITH_CB = 1;
const FINISH_UNSAFE = 2;

const kMaxHeaderFieldsCount = 32;

const httpMethods = [
  "DELETE", "GET", "HEAD", "POST", "PUT", "CONNECT", "OPTIONS", "TRACE", "COPY", "LOCK", "MKCOL",
  "MOVE", "PROPFIND", "PROPPATCH", "SEARCH", "UNLOCK", "BIND", "REBIND", "UNBIND", "ACL", "REPORT",
  "MKACTIVITY", "CHECKOUT", "MERGE", "M-SEARCH", "NOTIFY", "SUBSCRIBE", "UNSUBSCRIBE", "PATCH",
  "PURGE", "MKCALENDAR", "LINK", "UNLINK", "SOURCE",
];
const HTTP_CONNECT = 5;
const methodIndex = new Map(httpMethods.map((m, i) => [m, i]));
const methodPrefixes = new Set();
for (const m of httpMethods) for (let i = 1; i <= m.length; i++) methodPrefixes.add(m.slice(0, i));

// RFC 7230 tchar.
const isToken = new Uint8Array(256);
for (const c of "!#$%&'*+-.^_`|~") isToken[c.charCodeAt(0)] = 1;
for (let c = 0x30; c <= 0x39; c++) isToken[c] = 1;
for (let c = 0x41; c <= 0x5a; c++) isToken[c] = 1;
for (let c = 0x61; c <= 0x7a; c++) isToken[c] = 1;

const CR = 13;
const LF = 10;
const SP = 32;
const HT = 9;

// Parser states.
const S_START = 0;
const S_METHOD = 1;
const S_SPACES_BEFORE_URL = 2;
const S_URL = 3;
const S_REQ_VERSION = 4; // "HTTP/x.y" after the url
const S_REQ_LINE_ALMOST_DONE = 5;
const S_RES_VERSION = 6; // "HTTP/x.y" at the start of a response
const S_RES_STATUS_CODE = 7;
const S_RES_STATUS = 8;
const S_RES_LINE_ALMOST_DONE = 9;
const S_HEADER_FIELD_START = 10;
const S_HEADER_FIELD = 11;
const S_HEADER_VALUE_DISCARD_WS = 12;
const S_HEADER_VALUE = 13;
const S_HEADER_VALUE_LF = 14;
const S_HEADER_VALUE_LWS = 15; // after a header line: a fold, or the next field
const S_HEADERS_ALMOST_DONE = 16;
const S_BODY_IDENTITY = 17;
const S_BODY_EOF = 18;
const S_CHUNK_SIZE_START = 19;
const S_CHUNK_SIZE = 20;
const S_CHUNK_EXTENSIONS = 21;
const S_CHUNK_SIZE_ALMOST_DONE = 22;
const S_CHUNK_DATA = 23;
const S_CHUNK_DATA_CR = 24;
const S_CHUNK_DATA_LF = 25;
const S_CLOSED = 26;
const S_DEAD = 27;

class ParseErrorSignal {
  constructor(code, reason, pos) {
    this.code = code;
    this.reason = reason;
    this.pos = pos;
  }
}

function latin1(bytes, start, end) {
  if (end <= start) return "";
  let s = "";
  for (let i = start; i < end; i += 4096) {
    s += String.fromCharCode.apply(null, bytes.subarray(i, Math.min(end, i + 4096)));
  }
  return s;
}

function trimOWS(s) {
  let end = s.length;
  while (end > 0) {
    const c = s.charCodeAt(end - 1);
    if (c !== SP && c !== HT) break;
    end--;
  }
  return end === s.length ? s : s.slice(0, end);
}

const hrNow = () => performance.now();

class HTTPParser {
  constructor() {
    this._type = HTTP_REQUEST;
    this._connections = null;
    this._lastMessageStart = 0;
    this._headersCompleted = false;
    this._currentBuffer = null;
    this._reset(HTTP_REQUEST, 0, 0);
  }

  _reset(type, maxHeaderSize, lenient) {
    this._type = type;
    this._maxHeaderSize = maxHeaderSize || getOptionValue("--max-http-header-size");
    this._lenient = lenient | 0;
    this._state = S_START;
    this._flags = 0;
    this._finish = FINISH_SAFE;
    this._upgrade = false;
    this._method = 0;
    this._methodText = "";
    this._versionText = "";
    this._major = 0;
    this._minor = 0;
    this._statusCode = 0;
    this._contentLength = 0;
    this._remaining = 0;
    this._headerNread = 0;
    this._url = "";
    this._status = "";
    this._fields = [];
    this._values = [];
    this._field = null; // the field being read (string)
    this._value = null; // the value being read (string)
    this._haveFlushed = false;
    this._headersCompleted = false;
    this._paused = false;
    this._error = null;
  }

  // ---- the binding surface ----

  initialize(type, resource, maxHeaderSize, lenient, connectionsList) {
    if (this._connections !== null) this._connections._remove(this);
    this._reset(type, maxHeaderSize, lenient);
    this[asyncHookSymbols.async_id_symbol] = newAsyncId();
    if (connectionsList != null) {
      this._connections = connectionsList;
      this._lastMessageStart = hrNow();
      connectionsList._push(this);
      connectionsList._pushActive(this);
    } else {
      this._connections = null;
    }
  }
  close() {
    this.remove();
    this._state = S_DEAD;
  }
  free() {}
  remove() {
    if (this._connections !== null) this._connections._remove(this);
  }
  pause() {
    this._paused = true;
  }
  resume() {
    this._paused = false;
  }
  consume() {}
  unconsume() {}
  getCurrentBuffer() {
    const buf = this._currentBuffer;
    return buf === null ? Buffer.alloc(0) : Buffer.from(buf);
  }
  duration() {
    return this._lastMessageStart === 0 ? 0 : hrNow() - this._lastMessageStart;
  }
  headersCompleted() {
    return this._headersCompleted;
  }
  getAsyncId() {
    return this[asyncHookSymbols.async_id_symbol] ?? 0;
  }

  execute(data) {
    const bytes = data instanceof Uint8Array ? data :
      new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    this._currentBuffer = bytes;
    try {
      const pos = this._run(bytes);
      return pos;
    } catch (e) {
      if (e instanceof ParseErrorSignal) return this._parseError(e);
      throw e;
    } finally {
      this._currentBuffer = null;
    }
  }

  finish() {
    if (this._error !== null) return undefined;
    switch (this._finish) {
      case FINISH_SAFE_WITH_CB:
        this._messageComplete();
        return undefined;
      case FINISH_SAFE:
        return undefined;
      default:
        return this._parseError(new ParseErrorSignal("HPE_INVALID_EOF_STATE", "Invalid EOF state", 0));
    }
  }

  // ---- internals ----

  _parseError(signal) {
    this._error = signal;
    this._state = S_DEAD;
    const e = new Error("Parse Error");
    e.bytesParsed = signal.pos;
    e.code = signal.code;
    e.reason = signal.reason;
    return e;
  }

  _fail(code, reason, pos) {
    throw new ParseErrorSignal(code, reason, pos);
  }

  _trackHeader(len, pos) {
    this._headerNread += len;
    if (this._headerNread >= this._maxHeaderSize) {
      this._fail("HPE_HEADER_OVERFLOW", "Header overflow", pos);
    }
  }

  _messageBegin() {
    const list = this._connections;
    if (list !== null) list._remove(this);
    this._fields = [];
    this._values = [];
    this._field = null;
    this._value = null;
    this._headersCompleted = false;
    this._lastMessageStart = hrNow();
    this._url = "";
    this._status = "";
    this._flags = 0;
    this._upgrade = false;
    this._contentLength = 0;
    this._statusCode = 0;
    this._methodText = "";
    this._versionText = "";
    this._headerNread = 0;
    this._finish = FINISH_UNSAFE;
    if (list !== null) {
      list._push(this);
      list._pushActive(this);
    }
    const cb = this[kOnMessageBegin];
    if (typeof cb === "function") cb.call(this);
  }

  _headers() {
    const out = [];
    const n = this._values.length;
    for (let i = 0; i < n; i++) {
      out.push(this._fields[i], trimOWS(this._values[i]));
    }
    return out;
  }

  _flush() {
    const cb = this[kOnHeaders];
    if (typeof cb === "function") cb.call(this, this._headers(), this._url);
    this._url = "";
    this._haveFlushed = true;
  }

  // A header line is complete: record it (flushing to JS every 32 fields) and apply the
  // semantics llhttp gives Connection / Content-Length / Transfer-Encoding / Upgrade.
  _headerDone(pos) {
    const field = this._field;
    const value = this._value ?? "";
    this._field = null;
    this._value = null;
    if (this._fields.length === kMaxHeaderFieldsCount - 1) {
      // Node flushes when the 32nd field starts; the pair being completed goes in the next batch.
      this._flush();
      this._fields = [];
      this._values = [];
    }
    this._fields.push(field);
    this._values.push(value);
    if ((this._flags & F_TRAILING) !== 0) return;
    const name = field.length <= 17 ? field.toLowerCase() : "";
    switch (name) {
      case "connection": {
        for (const token of value.split(",")) {
          const t = token.trim().toLowerCase();
          if (t === "close") this._flags |= F_CONNECTION_CLOSE;
          else if (t === "keep-alive") this._flags |= F_CONNECTION_KEEP_ALIVE;
          else if (t === "upgrade") this._flags |= F_CONNECTION_UPGRADE;
        }
        break;
      }
      case "content-length": {
        if ((this._flags & F_CONTENT_LENGTH) !== 0) {
          this._fail("HPE_UNEXPECTED_CONTENT_LENGTH", "Duplicate Content-Length", pos);
        }
        if ((this._flags & F_TRANSFER_ENCODING) !== 0 && (this._lenient & kLenientChunkedLength) === 0) {
          this._fail("HPE_UNEXPECTED_CONTENT_LENGTH", "Content-Length can't be present with Transfer-Encoding", pos);
        }
        const v = trimOWS(value);
        if (v.length === 0) this._fail("HPE_INVALID_CONTENT_LENGTH", "Empty Content-Length", pos);
        let n = 0;
        for (let i = 0; i < v.length; i++) {
          const c = v.charCodeAt(i);
          if (c < 0x30 || c > 0x39) this._fail("HPE_INVALID_CONTENT_LENGTH", "Invalid character in Content-Length", pos);
          n = n * 10 + (c - 0x30);
          if (n > Number.MAX_SAFE_INTEGER) this._fail("HPE_INVALID_CONTENT_LENGTH", "Content-Length overflow", pos);
        }
        this._contentLength = n;
        this._flags |= F_CONTENT_LENGTH;
        break;
      }
      case "transfer-encoding": {
        if ((this._flags & F_CONTENT_LENGTH) !== 0 && (this._lenient & kLenientChunkedLength) === 0) {
          this._fail("HPE_UNEXPECTED_CONTENT_LENGTH", "Content-Length can't be present with Transfer-Encoding", pos);
        }
        this._flags |= F_TRANSFER_ENCODING;
        const tokens = value.split(",");
        const last = tokens[tokens.length - 1].trim().toLowerCase();
        if (last === "chunked") this._flags |= F_CHUNKED;
        else this._flags &= ~F_CHUNKED;
        break;
      }
      case "upgrade":
        this._flags |= F_UPGRADE;
        break;
    }
  }

  _messageNeedsEof() {
    if (this._type === HTTP_REQUEST) return false;
    const status = this._statusCode;
    if ((status / 100 | 0) === 1 || status === 204 || status === 304 || (this._flags & F_SKIPBODY) !== 0) {
      return false;
    }
    if ((this._flags & F_TRANSFER_ENCODING) !== 0 && (this._flags & F_CHUNKED) === 0) return true;
    if ((this._flags & (F_CHUNKED | F_CONTENT_LENGTH)) !== 0) return false;
    return true;
  }

  _shouldKeepAlive() {
    if (this._major > 0 && this._minor > 0) {
      if ((this._flags & F_CONNECTION_CLOSE) !== 0) return false;
    } else if ((this._flags & F_CONNECTION_KEEP_ALIVE) === 0) {
      return false;
    }
    return !this._messageNeedsEof();
  }

  // Headers are complete: the onHeadersComplete callback, then where the body goes.
  // Returns the next state, or -1 to stop parsing (upgrade).
  _headersComplete(pos) {
    if ((this._flags & F_UPGRADE) !== 0 && (this._flags & F_CONNECTION_UPGRADE) !== 0) {
      this._upgrade = this._type === HTTP_REQUEST || this._statusCode === 101;
    } else {
      this._upgrade = this._type === HTTP_REQUEST && this._method === HTTP_CONNECT;
    }
    this._headersCompleted = true;
    this._headerNread = 0;
    const cb = this[kOnHeadersComplete];
    const fieldsBefore = this._fields;
    this._fields = [];
    const valuesBefore = this._values;
    this._values = [];
    if (typeof cb === "function") {
      let headers, url;
      if (this._haveFlushed) {
        this._fields = fieldsBefore;
        this._values = valuesBefore;
        this._flush();
        this._fields = [];
        this._values = [];
      } else {
        headers = [];
        for (let i = 0; i < valuesBefore.length; i++) headers.push(fieldsBefore[i], trimOWS(valuesBefore[i]));
        if (this._type === HTTP_REQUEST) url = this._url;
      }
      const isRequest = this._type === HTTP_REQUEST;
      const ret = cb.call(this, this._major, this._minor, headers,
                          isRequest ? this._method : undefined, url,
                          isRequest ? undefined : this._statusCode,
                          isRequest ? undefined : this._status,
                          this._upgrade, this._shouldKeepAlive());
      const rv = Number(ret) | 0;
      if (rv === 1) {
        this._flags |= F_SKIPBODY;
      } else if (rv === 2) {
        this._upgrade = true;
        this._flags |= F_SKIPBODY;
      } else if (rv !== 0) {
        this._fail("HPE_CB_HEADERS_COMPLETE", "User callback error", pos);
      }
    }
    // llhttp__after_headers_complete
    const hasBody = (this._flags & F_CHUNKED) !== 0 || this._contentLength > 0;
    if (this._upgrade && (this._method === HTTP_CONNECT || (this._flags & F_SKIPBODY) !== 0 || !hasBody)) {
      this._messageComplete();
      return -1;
    }
    if ((this._flags & F_SKIPBODY) !== 0) return this._messageComplete();
    if ((this._flags & F_CHUNKED) !== 0) return S_CHUNK_SIZE_START;
    if ((this._flags & F_TRANSFER_ENCODING) !== 0) {
      if (this._type === HTTP_REQUEST && (this._lenient & kLenientChunkedLength) === 0) {
        this._fail("HPE_INVALID_TRANSFER_ENCODING", "Request has invalid `Transfer-Encoding`", pos);
      }
      this._finish = FINISH_SAFE_WITH_CB;
      return S_BODY_EOF;
    }
    if ((this._flags & F_CONTENT_LENGTH) === 0) {
      if (!this._messageNeedsEof()) return this._messageComplete();
      this._finish = FINISH_SAFE_WITH_CB;
      return S_BODY_EOF;
    }
    if (this._contentLength === 0) return this._messageComplete();
    this._remaining = this._contentLength;
    return S_BODY_IDENTITY;
  }

  // on_message_complete + llhttp__after_message_complete. Returns the next state.
  _messageComplete() {
    const list = this._connections;
    if (list !== null) list._remove(this);
    this._lastMessageStart = 0;
    if (list !== null) list._push(this);
    if (this._fields.length !== 0) this._flush(); // trailers
    const keepAlive = this._shouldKeepAlive();
    this._finish = FINISH_SAFE;
    const cb = this[kOnMessageComplete];
    if (typeof cb === "function") cb.call(this);
    this._flags = 0;
    if (this._upgrade) return S_START;
    return keepAlive || (this._lenient & kLenientKeepAlive) !== 0 ? S_START : S_CLOSED;
  }

  _body(bytes, start, end) {
    if (end <= start) return;
    const cb = this[kOnBody];
    if (typeof cb === "function") cb.call(this, Buffer.from(bytes.subarray(start, end)));
  }

  _run(bytes) {
    const len = bytes.length;
    let state = this._state;
    let p = 0;
    if (state === S_DEAD) {
      const e = this._error;
      if (e !== null) throw new ParseErrorSignal(e.code, e.reason, 0);
      return 0;
    }
    const lenientHeaders = (this._lenient & kLenientHeaders) !== 0;
    while (p < len) {
      const c = bytes[p];
      switch (state) {
        case S_START: {
          if (c === CR || c === LF) {
            p++;
            break;
          }
          this._messageBegin();
          if (this._type === HTTP_REQUEST) {
            state = S_METHOD;
          } else {
            state = S_RES_VERSION;
          }
          break;
        }
        case S_METHOD: {
          if (c === SP) {
            const idx = methodIndex.get(this._methodText);
            if (idx === undefined) this._fail("HPE_INVALID_METHOD", "Invalid method encountered", p);
            this._method = idx;
            state = S_SPACES_BEFORE_URL;
            p++;
            break;
          }
          const next = this._methodText + String.fromCharCode(c);
          if (!methodPrefixes.has(next)) {
            this._fail("HPE_INVALID_METHOD", this._methodText.length === 0 ? "Invalid method encountered" : "Expected space after method", p);
          }
          this._methodText = next;
          p++;
          break;
        }
        case S_SPACES_BEFORE_URL: {
          if (c === SP) {
            p++;
            break;
          }
          if (c === CR || c === LF) this._fail("HPE_INVALID_URL", "Unexpected start char in url", p);
          state = S_URL;
          break;
        }
        case S_URL: {
          const start = p;
          let q = p;
          while (q < len) {
            const b = bytes[q];
            if (b === SP || b === CR || b === LF) break;
            if ((b < 0x20 && b !== HT && b !== 0x0c) || b === 0x7f) {
              this._trackHeader(q - start, q);
              this._url += latin1(bytes, start, q);
              this._fail("HPE_INVALID_URL", "Invalid characters in url", q);
            }
            q++;
          }
          this._trackHeader(q - start, q);
          this._url += latin1(bytes, start, q);
          p = q;
          if (q < len) {
            if (bytes[q] !== SP) this._fail("HPE_INVALID_CONSTANT", "Expected HTTP/", q);
            p++;
            state = S_REQ_VERSION;
            this._versionText = "";
          }
          break;
        }
        case S_REQ_VERSION:
        case S_RES_VERSION: {
          const text = this._versionText;
          const i = text.length;
          if (i < 5) {
            if (c !== "HTTP/".charCodeAt(i)) {
              if (state === S_RES_VERSION && i === 0) this._fail("HPE_INVALID_CONSTANT", "Expected HTTP/", p);
              this._fail("HPE_INVALID_CONSTANT", "Expected HTTP/", p);
            }
          } else if (i === 5 || i === 7) {
            if (c < 0x30 || c > 0x39) {
              this._fail("HPE_INVALID_VERSION", i === 5 ? "Invalid major version" : "Invalid minor version", p);
            }
          } else if (i === 6) {
            if (c !== 0x2e) this._fail("HPE_INVALID_VERSION", "Expected dot", p);
          }
          if (i < 8) {
            this._versionText = text + String.fromCharCode(c);
            p++;
            if (i === 7) {
              this._major = this._versionText.charCodeAt(5) - 0x30;
              this._minor = this._versionText.charCodeAt(7) - 0x30;
              const ok = (this._major === 1 && (this._minor === 0 || this._minor === 1)) ||
                (this._major === 0 && this._minor === 9) || (this._major === 2 && this._minor === 0);
              if (!ok) this._fail("HPE_INVALID_VERSION", "Invalid HTTP version", p - 1);
              if (state === S_REQ_VERSION) {
                state = S_REQ_LINE_ALMOST_DONE;
              } else {
                state = S_RES_STATUS_CODE;
                this._statusDigits = -1; // expecting the space before the status code
              }
            }
          }
          break;
        }
        case S_REQ_LINE_ALMOST_DONE: {
          if (c === CR) {
            if (this._sawCR) this._fail("HPE_INVALID_VERSION", "Expected CRLF after version", p);
            this._sawCR = true;
            p++;
            break;
          }
          if (c === LF) {
            this._sawCR = false;
            p++;
            state = S_HEADER_FIELD_START;
            break;
          }
          this._fail("HPE_INVALID_VERSION", "Expected CRLF after version", p);
          break;
        }
        case S_RES_STATUS_CODE: {
          if (this._statusDigits === -1) {
            if (c !== SP) this._fail("HPE_INVALID_VERSION", "Expected space after version", p);
            this._statusDigits = 0;
            this._statusCode = 0;
            p++;
            break;
          }
          if (c >= 0x30 && c <= 0x39) {
            if (this._statusDigits === 3) this._fail("HPE_INVALID_STATUS", "Invalid response status", p);
            this._statusCode = this._statusCode * 10 + (c - 0x30);
            this._statusDigits++;
            p++;
            break;
          }
          if (this._statusDigits !== 3) this._fail("HPE_INVALID_STATUS", "Invalid response status", p);
          if (c === SP) {
            p++;
            state = S_RES_STATUS;
          } else if (c === CR || c === LF) {
            state = S_RES_LINE_ALMOST_DONE;
          } else {
            this._fail("HPE_INVALID_STATUS", "Invalid response status", p);
          }
          break;
        }
        case S_RES_STATUS: {
          const start = p;
          let q = p;
          while (q < len && bytes[q] !== CR && bytes[q] !== LF) q++;
          this._trackHeader(q - start, q);
          this._status += latin1(bytes, start, q);
          p = q;
          if (q < len) state = S_RES_LINE_ALMOST_DONE;
          break;
        }
        case S_RES_LINE_ALMOST_DONE: {
          if (c === CR) {
            p++;
            this._sawCR = true;
            break;
          }
          if (c === LF) {
            this._sawCR = false;
            p++;
            state = S_HEADER_FIELD_START;
            break;
          }
          this._fail("HPE_STRICT", "Expected LF after CR", p);
          break;
        }
        case S_HEADER_FIELD_START: {
          if (c === CR) {
            p++;
            state = S_HEADERS_ALMOST_DONE;
            break;
          }
          if (c === LF) {
            state = S_HEADERS_ALMOST_DONE;
            break;
          }
          if (!isToken[c]) this._fail("HPE_INVALID_HEADER_TOKEN", "Invalid header token", p);
          this._field = "";
          state = S_HEADER_FIELD;
          break;
        }
        case S_HEADER_FIELD: {
          const start = p;
          let q = p;
          while (q < len && isToken[bytes[q]]) q++;
          this._trackHeader(q - start, q);
          this._field += latin1(bytes, start, q);
          p = q;
          if (q < len) {
            if (bytes[q] !== 0x3a) this._fail("HPE_INVALID_HEADER_TOKEN", "Invalid header token", q);
            p++;
            this._value = "";
            state = S_HEADER_VALUE_DISCARD_WS;
          }
          break;
        }
        case S_HEADER_VALUE_DISCARD_WS: {
          if (c === SP || c === HT) {
            p++;
            break;
          }
          state = S_HEADER_VALUE;
          break;
        }
        case S_HEADER_VALUE: {
          const start = p;
          let q = p;
          while (q < len) {
            const b = bytes[q];
            if (b === CR || b === LF) break;
            if (!lenientHeaders && ((b < 0x20 && b !== HT) || b === 0x7f)) {
              this._trackHeader(q - start, q);
              this._fail("HPE_INVALID_HEADER_TOKEN", "Invalid header value char", q);
            }
            q++;
          }
          this._trackHeader(q - start, q);
          this._value += latin1(bytes, start, q);
          p = q;
          if (q < len) {
            if (bytes[q] === CR) {
              p++;
              state = S_HEADER_VALUE_LF;
            } else {
              p++;
              state = S_HEADER_VALUE_LWS;
            }
          }
          break;
        }
        case S_HEADER_VALUE_LF: {
          if (c !== LF) this._fail("HPE_LF_EXPECTED", "Missing expected LF after header value", p);
          p++;
          state = S_HEADER_VALUE_LWS;
          break;
        }
        case S_HEADER_VALUE_LWS: {
          if (c === SP || c === HT) {
            // obs-fold: the value continues on this line.
            this._value += " ";
            p++;
            state = S_HEADER_VALUE_DISCARD_WS;
            break;
          }
          this._headerDone(p);
          state = S_HEADER_FIELD_START;
          break;
        }
        case S_HEADERS_ALMOST_DONE: {
          if (c !== LF) this._fail("HPE_STRICT", "Expected LF after headers", p);
          p++;
          if ((this._flags & F_TRAILING) !== 0) {
            state = this._messageComplete();
            if (this._upgrade) {
              this._state = state;
              return p;
            }
            break;
          }
          const next = this._headersComplete(p);
          if (next === -1) {
            this._state = S_START;
            return p;
          }
          state = next;
          if (this._upgrade && state === S_START) {
            this._state = state;
            return p;
          }
          break;
        }
        case S_BODY_IDENTITY: {
          const n = Math.min(this._remaining, len - p);
          this._remaining -= n;
          this._state = state;
          this._body(bytes, p, p + n);
          p += n;
          if (this._remaining === 0) {
            state = this._messageComplete();
            if (this._upgrade) {
              this._state = state;
              return p;
            }
          }
          break;
        }
        case S_BODY_EOF: {
          this._state = state;
          this._body(bytes, p, len);
          p = len;
          break;
        }
        case S_CHUNK_SIZE_START: {
          const v = hexValue(c);
          if (v < 0) this._fail("HPE_INVALID_CHUNK_SIZE", "Invalid character in chunk size", p);
          this._contentLength = v;
          p++;
          state = S_CHUNK_SIZE;
          break;
        }
        case S_CHUNK_SIZE: {
          const v = hexValue(c);
          if (v >= 0) {
            this._contentLength = this._contentLength * 16 + v;
            if (this._contentLength > Number.MAX_SAFE_INTEGER) {
              this._fail("HPE_INVALID_CHUNK_SIZE", "Chunk size overflow", p);
            }
            p++;
            break;
          }
          if (c === 0x3b || c === SP || c === HT) {
            p++;
            state = S_CHUNK_EXTENSIONS;
            break;
          }
          if (c === CR) {
            p++;
            state = S_CHUNK_SIZE_ALMOST_DONE;
            break;
          }
          if (c === LF) {
            state = S_CHUNK_SIZE_ALMOST_DONE;
            break;
          }
          this._fail("HPE_INVALID_CHUNK_SIZE", "Invalid character in chunk size", p);
          break;
        }
        case S_CHUNK_EXTENSIONS: {
          if (c === CR) {
            p++;
            state = S_CHUNK_SIZE_ALMOST_DONE;
            break;
          }
          if (c === LF) {
            state = S_CHUNK_SIZE_ALMOST_DONE;
            break;
          }
          if (c < 0x20 && c !== HT) this._fail("HPE_STRICT", "Invalid character in chunk extensions", p);
          p++;
          break;
        }
        case S_CHUNK_SIZE_ALMOST_DONE: {
          if (c !== LF) this._fail("HPE_STRICT", "Expected LF after chunk size", p);
          p++;
          this._headerNread = 0; // on_chunk_header
          if (this._contentLength === 0) {
            this._flags |= F_TRAILING;
            state = S_HEADER_FIELD_START;
          } else {
            this._remaining = this._contentLength;
            state = S_CHUNK_DATA;
          }
          break;
        }
        case S_CHUNK_DATA: {
          const n = Math.min(this._remaining, len - p);
          this._remaining -= n;
          this._state = state;
          this._body(bytes, p, p + n);
          p += n;
          if (this._remaining === 0) state = S_CHUNK_DATA_CR;
          break;
        }
        case S_CHUNK_DATA_CR: {
          if (c === CR) {
            p++;
            state = S_CHUNK_DATA_LF;
          } else if (c === LF) {
            state = S_CHUNK_DATA_LF;
          } else {
            this._fail("HPE_STRICT", "Expected CRLF after chunk", p);
          }
          break;
        }
        case S_CHUNK_DATA_LF: {
          if (c !== LF) this._fail("HPE_STRICT", "Expected LF after chunk data", p);
          p++;
          this._headerNread = 0; // on_chunk_complete
          state = S_CHUNK_SIZE_START;
          break;
        }
        case S_CLOSED: {
          if (c === CR || c === LF) {
            p++;
            break;
          }
          this._fail("HPE_CLOSED_CONNECTION", "Data after `Connection: close`", p);
          break;
        }
        default:
          this._fail("HPE_INTERNAL", "Invalid parser state", p);
      }
    }
    this._state = state;
    return p;
  }
}

function hexValue(c) {
  if (c >= 0x30 && c <= 0x39) return c - 0x30;
  if (c >= 0x41 && c <= 0x46) return c - 0x41 + 10;
  if (c >= 0x61 && c <= 0x66) return c - 0x61 + 10;
  return -1;
}

HTTPParser.REQUEST = HTTP_REQUEST;
HTTPParser.RESPONSE = HTTP_RESPONSE;
HTTPParser.kOnMessageBegin = kOnMessageBegin;
HTTPParser.kOnHeaders = kOnHeaders;
HTTPParser.kOnHeadersComplete = kOnHeadersComplete;
HTTPParser.kOnBody = kOnBody;
HTTPParser.kOnMessageComplete = kOnMessageComplete;
HTTPParser.kOnExecute = kOnExecute;
HTTPParser.kOnTimeout = kOnTimeout;
HTTPParser.kLenientNone = kLenientNone;
HTTPParser.kLenientHeaders = kLenientHeaders;
HTTPParser.kLenientChunkedLength = kLenientChunkedLength;
HTTPParser.kLenientKeepAlive = kLenientKeepAlive;
HTTPParser.kLenientAll = kLenientAll;

// The server's connection bookkeeping: every parser (idle ones first, then by message start) and
// the active ones (a message in progress), for the headers/request timeout sweep.
class ConnectionsList {
  constructor() {
    this._all = new Set();
    this._active = new Set();
  }
  _push(parser) { this._all.add(parser); }
  _pushActive(parser) { this._active.add(parser); }
  _remove(parser) {
    this._all.delete(parser);
    this._active.delete(parser);
  }
  _sorted(set) {
    return [...set].sort((a, b) => {
      if (a._lastMessageStart === 0 && b._lastMessageStart === 0) return 0;
      if (a._lastMessageStart === 0) return -1;
      if (b._lastMessageStart === 0) return 1;
      return a._lastMessageStart - b._lastMessageStart;
    });
  }
  all() { return this._sorted(this._all); }
  idle() { return this._sorted(this._all).filter((p) => p._lastMessageStart === 0); }
  active() { return this._sorted(this._active); }
  expired(headersTimeout, requestTimeout) {
    headersTimeout >>>= 0;
    requestTimeout >>>= 0;
    if (headersTimeout === 0 && requestTimeout === 0) return [];
    if (requestTimeout > 0 && headersTimeout > requestTimeout) {
      [headersTimeout, requestTimeout] = [requestTimeout, headersTimeout];
    }
    const now = hrNow();
    const headersDeadline = headersTimeout > 0 && now > headersTimeout ? now - headersTimeout : 0;
    const requestDeadline = requestTimeout > 0 && now > requestTimeout ? now - requestTimeout : 0;
    if (headersDeadline === 0 && requestDeadline === 0) return [];
    const result = [];
    for (const parser of this._sorted(this._active)) {
      if ((!parser._headersCompleted && headersDeadline > 0 && parser._lastMessageStart < headersDeadline) ||
          (requestDeadline > 0 && parser._lastMessageStart < requestDeadline)) {
        result.push(parser);
        this._active.delete(parser);
      }
    }
    return result;
  }
}

bindings.http_parser = { methods: httpMethods, allMethods: httpMethods, HTTPParser, ConnectionsList };

const resp = Buffer.from("HTTP/1.1 200 OK\r\nDate: Thu, 24 Sep 2026 07:26:57 GMT\r\nConnection: keep-alive\r\nKeep-Alive: timeout=5\r\nContent-Length: 2\r\n\r\nhi", 'latin1');
const req = Buffer.from("GET / HTTP/1.1\r\nHost: localhost:12420\r\nConnection: keep-alive\r\n\r\n", 'latin1');
let got = 0, body = 0, done = 0, sig = "";
for (let i = 0; i < 200; i++) {
  for (const [type, data] of [[HTTPParser.RESPONSE, resp], [HTTPParser.REQUEST, req]]) {
    const p = new HTTPParser();
    p.initialize(type, {}, 16384, 0, null);
    p[HTTPParser.kOnHeadersComplete] = (major, minor, headers, method, url, status, text, upgrade, keepAlive) => {
      got++;
      if (i === 199) sig += [major, minor, headers.join("|"), method, url, status, text, upgrade, keepAlive].join(",") + ";";
      return 0;
    };
    p[HTTPParser.kOnBody] = (b) => { body += b.length; };
    p[HTTPParser.kOnMessageComplete] = () => { done++; };
    const n = p.execute(data);
    if (n !== data.length) throw new Error("parsed " + n + " of " + data.length);
  }
}
[got, body, done, sig].join(" ");
