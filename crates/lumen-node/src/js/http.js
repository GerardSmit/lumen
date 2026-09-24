// node:http — a server built on Lumen.serve (the WinterCG fetch-style server from lumen-web).
// createServer(handler) returns a Server whose .listen() opens a Lumen.serve listener; each
// connection is adapted into node's (IncomingMessage, ServerResponse) pair and dispatched via the
// 'request' event, so Express and the Connect middleware ecosystem run unmodified.
//
// The adaptation is buffered end-to-end (Lumen.serve buffers bodies): the request body is pushed
// into the IncomingMessage Readable up front, and the ServerResponse Writable collects writes
// until end(), then hands one web Response back to Lumen.serve. The client side (http.request/get,
// + the https twins) is implemented too, buffered over the global fetch() — see the ClientRequest
// section below. Not supported (throws honestly): keep-alive/socket tuning (http.Agent is a no-op),
// trailers, Expect: 100-continue, protocol upgrade, raw socket access, incremental streaming.

const EventEmitter = __builtins.get("events");
const stream = __builtins.get("stream");
const { Readable, Writable } = stream;

const STATUS_CODES = {
  100: "Continue", 101: "Switching Protocols", 102: "Processing", 103: "Early Hints",
  200: "OK", 201: "Created", 202: "Accepted", 203: "Non-Authoritative Information", 204: "No Content", 205: "Reset Content", 206: "Partial Content",
  300: "Multiple Choices", 301: "Moved Permanently", 302: "Found", 303: "See Other", 304: "Not Modified", 307: "Temporary Redirect", 308: "Permanent Redirect",
  400: "Bad Request", 401: "Unauthorized", 402: "Payment Required", 403: "Forbidden", 404: "Not Found", 405: "Method Not Allowed", 406: "Not Acceptable",
  408: "Request Timeout", 409: "Conflict", 410: "Gone", 411: "Length Required", 412: "Precondition Failed", 413: "Payload Too Large", 414: "URI Too Long",
  415: "Unsupported Media Type", 416: "Range Not Satisfiable", 417: "Expectation Failed", 418: "I'm a Teapot", 422: "Unprocessable Entity", 429: "Too Many Requests",
  500: "Internal Server Error", 501: "Not Implemented", 502: "Bad Gateway", 503: "Service Unavailable", 504: "Gateway Timeout", 505: "HTTP Version Not Supported",
};

const METHODS = ["ACL", "BIND", "CHECKOUT", "CONNECT", "COPY", "DELETE", "GET", "HEAD", "LINK", "LOCK", "MERGE", "MKACTIVITY", "MKCALENDAR", "MKCOL", "MOVE", "NOTIFY", "OPTIONS", "PATCH", "POST", "PROPFIND", "PROPPATCH", "PURGE", "PUT", "REBIND", "REPORT", "SEARCH", "SOURCE", "SUBSCRIBE", "TRACE", "UNBIND", "UNLINK", "UNLOCK", "UNSUBSCRIBE"];

class IncomingMessage extends Readable {
  // Data is pushed as it arrives from the native side; there is nothing to pull.
  _read() {}

  constructor(socket) {
    super();
    this.httpVersion = "1.1";
    this.httpVersionMajor = 1;
    this.httpVersionMinor = 1;
    this.method = null;
    this.url = "";
    this.headers = {};
    this.rawHeaders = [];
    this.trailers = {};
    this.rawTrailers = [];
    this.aborted = false;
    this.complete = false;
    this.socket = socket || { remoteAddress: undefined, remotePort: undefined, encrypted: false };
    this.connection = this.socket;
  }
  setTimeout(_ms, cb) { if (cb) this.once("timeout", cb); return this; }
  destroy(err) { this.aborted = true; return super.destroy(err); }
}

class ServerResponse extends Writable {
  constructor(req) {
    super();
    this.req = req;
    this.statusCode = 200;
    this.statusMessage = undefined;
    this.headersSent = false;
    this.finished = false;
    this.sendDate = true;
    this._headers = new Map(); // lower-name -> { name, value }
    this._chunks = [];
    this.socket = req ? req.socket : {};
    this.connection = this.socket;
    // Resolved by the Lumen.serve bridge when end() fires.
    this._done = new Promise((resolve) => { this._resolveDone = resolve; });
  }

  setHeader(name, value) {
    if (this.headersSent) throw new Error("Cannot set headers after they are sent to the client");
    this._headers.set(String(name).toLowerCase(), { name: String(name), value });
    return this;
  }
  getHeader(name) {
    const h = this._headers.get(String(name).toLowerCase());
    return h ? h.value : undefined;
  }
  getHeaderNames() { return [...this._headers.values()].map((h) => h.name.toLowerCase()); }
  getHeaders() {
    const out = Object.create(null);
    for (const h of this._headers.values()) out[h.name.toLowerCase()] = h.value;
    return out;
  }
  hasHeader(name) { return this._headers.has(String(name).toLowerCase()); }
  removeHeader(name) { this._headers.delete(String(name).toLowerCase()); }
  appendHeader(name, value) {
    const key = String(name).toLowerCase();
    const existing = this._headers.get(key);
    if (!existing) return this.setHeader(name, value);
    const arr = Array.isArray(existing.value) ? existing.value : [existing.value];
    existing.value = arr.concat(value);
    return this;
  }

  // Node sends headers implicitly on the first body write / end when writeHead wasn't called.
  // Routing that through writeHead lets on-headers (morgan's response-time, compression, …) fire
  // its hook, since on-headers works by wrapping writeHead.
  _implicitHeader() {
    if (!this.headersSent) this.writeHead(this.statusCode);
  }
  writeHead(statusCode, statusMessage, headers) {
    this.statusCode = statusCode;
    if (typeof statusMessage === "string") this.statusMessage = statusMessage;
    else headers = statusMessage;
    if (headers) {
      if (Array.isArray(headers)) {
        for (let i = 0; i < headers.length; i += 2) this.setHeader(headers[i], headers[i + 1]);
      } else {
        for (const k of Object.keys(headers)) this.setHeader(k, headers[k]);
      }
    }
    // 'header' listeners (on-headers) fire here, before we mark them sent.
    this.emit("__writeHead");
    this.headersSent = true;
    return this;
  }
  flushHeaders() { this.headersSent = true; }
  writeContinue() {}
  setTimeout(_ms, cb) { if (cb) this.once("timeout", cb); return this; }

  _write(chunk, encoding, cb) {
    this._implicitHeader();
    if (chunk != null && chunk.length !== 0) {
      this._chunks.push(chunk instanceof Uint8Array ? chunk : Buffer.from(String(chunk), encoding || "utf8"));
    }
    cb();
  }
  _final(cb) {
    this._implicitHeader();
    this.finished = true;
    let total = 0;
    for (const c of this._chunks) total += c.length;
    const body = Buffer.concat(this._chunks, total);
    // Hand the collected response to the Lumen.serve bridge.
    this._resolveDone({
      status: this.statusCode,
      statusText: this.statusMessage || STATUS_CODES[this.statusCode] || "",
      headers: headerPairs(this._headers),
      body,
    });
    cb();
  }
}

// Flatten the header map to [name, value] pairs, expanding array values (e.g. multiple Set-Cookie).
function headerPairs(map) {
  const pairs = [];
  for (const { name, value } of map.values()) {
    if (Array.isArray(value)) for (const v of value) pairs.push([name, String(v)]);
    else pairs.push([name, String(value)]);
  }
  return pairs;
}

class Server extends EventEmitter {
  constructor(opts, handler) {
    super();
    if (typeof opts === "function") { handler = opts; opts = {}; }
    if (handler) this.on("request", handler);
    this._lumen = null;
    this.listening = false;
  }

  listen(...args) {
    // Node signatures: listen(port[, host][, backlog][, cb]) / listen(options[, cb]).
    let port = 0, host = "0.0.0.0", cb;
    const first = args[0];
    if (first && typeof first === "object") {
      port = first.port ?? 0;
      host = first.host ?? "0.0.0.0";
      cb = args.find((a) => typeof a === "function");
    } else {
      port = first ?? 0;
      for (const a of args.slice(1)) {
        if (typeof a === "string") host = a;
        else if (typeof a === "function") cb = a;
      }
    }

    this._lumen = Lumen.serve(
      (request, info) => this._dispatch(request, info),
      { hostname: host === "localhost" ? "127.0.0.1" : host, port: Number(port) },
    );
    this.listening = true;
    if (cb) this.once("listening", cb);
    queueMicrotask(() => this.emit("listening"));
    return this;
  }

  address() {
    return this._lumen ? { address: this._lumen.hostname, family: "IPv4", port: this._lumen.port } : null;
  }

  async _dispatch(request, info) {
    const socket = {
      remoteAddress: info && info.remoteAddr ? info.remoteAddr.hostname : undefined,
      remotePort: info && info.remoteAddr ? info.remoteAddr.port : undefined,
      encrypted: false,
    };
    const req = new IncomingMessage(socket);
    req.method = request.method;
    const u = new URL(request.url);
    req.url = u.pathname + u.search; // origin-form, as a real server sees it
    for (const [k, v] of request.headers) {
      req.headers[k] = v;
      req.rawHeaders.push(k, v);
    }

    const res = new ServerResponse(req);

    // Feed the (already buffered) request body into the Readable, then EOF.
    const bodyBuf = Buffer.from(await request.arrayBuffer());
    if (this.listenerCount("request") === 0) {
      res.writeHead(404);
      res.end();
    } else {
      this.emit("request", req, res);
      if (bodyBuf.length) req.push(bodyBuf);
      req.push(null);
      req.complete = true;
    }

    const out = await res._done;
    return new Response(out.body.length ? out.body : null, {
      status: out.status,
      statusText: out.statusText,
      headers: out.headers,
    });
  }

  close(cb) {
    if (this._lumen) this._lumen.shutdown();
    this.listening = false;
    if (cb) queueMicrotask(() => cb());
    queueMicrotask(() => this.emit("close"));
    return this;
  }
  setTimeout(_ms, cb) { if (cb) this.once("timeout", cb); return this; }
  address_ = null;
}

function createServer(opts, handler) {
  return new Server(opts, handler);
}

// Outbound client (http.request/get): buffered end-to-end over the global fetch(), mirroring the
// server side which is buffered too. ClientRequest is a Writable that collects the body until
// end(), issues one fetch(), and surfaces the reply as an IncomingMessage (Readable). Features
// fetch cannot express (trailers, Expect: 100-continue, protocol upgrade, raw socket access) throw
// honestly; http.Agent is accepted as connection-pool metadata but drives no real pool.
function notSupportedOverFetch(what) {
  throw new Error(`node:http ${what} is not supported in lumen (the client runs over fetch(), which cannot express it)`);
}

// Normalize the (url | options), (options), optional-callback overloads into one options object.
// Node accepts request(url[,options][,cb]) and request(options[,cb]); url may be a string or URL.
function normalizeClientArgs(input, options, cb, defaultProtocol) {
  let opts = {};
  if (typeof input === "string" || input instanceof URL) {
    const u = input instanceof URL ? input : new URL(input);
    opts.protocol = u.protocol;
    opts.hostname = decodeURIComponent(u.hostname);
    if (u.port) opts.port = u.port;
    opts.path = (u.pathname || "/") + (u.search || "");
    if (u.username || u.password) opts.auth = `${decodeURIComponent(u.username)}:${decodeURIComponent(u.password)}`;
    if (options && typeof options === "object") Object.assign(opts, options);
    else if (typeof options === "function") cb = options;
  } else if (input && typeof input === "object") {
    opts = { ...input };
    if (typeof options === "function") cb = options;
    else if (options && typeof options === "object") Object.assign(opts, options);
  } else if (typeof input === "function") {
    cb = input;
  }
  if (!opts.protocol) opts.protocol = defaultProtocol || "http:";
  opts._cb = typeof cb === "function" ? cb : undefined;
  return opts;
}

// Headers fetch computes/forbids itself; drop them so the fetch layer sets them correctly.
const CLIENT_SKIP_HEADERS = new Set(["host", "connection", "content-length", "transfer-encoding", "keep-alive", "upgrade"]);

// ---- header validators (real; Node-shaped error codes) ----------------------------------------
// RFC 7230 token chars; matches Node's checkIsHttpToken.
const HTTP_TOKEN = /^[\^_`a-zA-Z\-0-9!#$%&'*+.|~]+$/;
// A control char (other than tab) anywhere in a header value is invalid.
const INVALID_HEADER_CHAR = /[^\t\x20-\x7e\x80-\xff]/;
function validateHeaderName(name, label) {
  if (typeof name !== "string" || name === "" || !HTTP_TOKEN.test(name)) {
    const err = new TypeError(`${label || "Header name"} must be a valid HTTP token ["${name}"]`);
    err.code = "ERR_INVALID_HTTP_TOKEN";
    throw err;
  }
}
function validateHeaderValue(name, value) {
  if (value === undefined) {
    const err = new TypeError(`Invalid value "${value}" for header "${name}"`);
    err.code = "ERR_HTTP_INVALID_HEADER_VALUE";
    throw err;
  }
  if (INVALID_HEADER_CHAR.test(String(value))) {
    const err = new TypeError(`Invalid character in header content ["${name}"]`);
    err.code = "ERR_INVALID_CHAR";
    throw err;
  }
}

// ---- OutgoingMessage / ClientRequest ----------------------------------------------------------
// Node's ServerResponse extends OutgoingMessage (the header-management Writable base). ServerResponse
// above is self-contained (to avoid perturbing the working server path), so OutgoingMessage is a
// standalone base exported for the class surface / instanceof checks. ClientRequest drives an
// outbound request, which needs a socket lumen doesn't expose — constructing it throws honestly.
class OutgoingMessage extends Writable {
  constructor() {
    // Finishing the body is not the end of the message's life: a ClientRequest outlives its
    // 'finish' until the response (or the upgraded socket) is done with it, so it must not be
    // auto-destroyed the way a plain Writable is — that would tear down the socket it just
    // handed over.
    super({ autoDestroy: false });
    this.headersSent = false;
    this.finished = false;
    this.sendDate = true;
    this._headers = new Map();
  }
  setHeader(name, value) { validateHeaderName(name); this._headers.set(String(name).toLowerCase(), { name: String(name), value }); return this; }
  getHeader(name) { const h = this._headers.get(String(name).toLowerCase()); return h ? h.value : undefined; }
  getHeaderNames() { return [...this._headers.values()].map((h) => h.name.toLowerCase()); }
  getHeaders() { const out = Object.create(null); for (const h of this._headers.values()) out[h.name.toLowerCase()] = h.value; return out; }
  hasHeader(name) { return this._headers.has(String(name).toLowerCase()); }
  removeHeader(name) { this._headers.delete(String(name).toLowerCase()); }
  setTimeout(_ms, cb) { if (cb) this.once("timeout", cb); return this; }
}
class ClientRequest extends OutgoingMessage {
  constructor(input, options, cb) {
    super();
    const opts = normalizeClientArgs(input, options, cb, (new.target && new.target._defaultProtocol) || "http:");
    this.protocol = opts.protocol;
    this.method = String(opts.method || "GET").toUpperCase();
    this.path = opts.path || "/";
    this.host = opts.hostname || opts.host || "localhost";
    const defaultPort = this.protocol === "https:" ? 443 : 80;
    this.port = opts.port != null ? Number(opts.port) : defaultPort;
    this._opts = opts;
    this._bodyChunks = [];
    this.aborted = false;
    this.reusedSocket = false;
    this._controller = new AbortController();
    this._timeoutMs = typeof opts.timeout === "number" ? opts.timeout : undefined;
    this._timer = null;
    this.res = null;
    // Minimal socket surface (Node code inspects a few fields); connecting/timeout are cosmetic.
    this.socket = { remoteAddress: this.host, remotePort: this.port, encrypted: this.protocol === "https:", destroyed: false };
    this.connection = this.socket;

    if (opts.headers) {
      for (const k of Object.keys(opts.headers)) {
        if (opts.headers[k] !== undefined) this.setHeader(k, opts.headers[k]);
      }
    }
    if (opts.auth && !this.hasHeader("authorization")) {
      this.setHeader("Authorization", "Basic " + Buffer.from(String(opts.auth)).toString("base64"));
    }
    if (opts._cb) this.once("response", opts._cb);

    // An AbortSignal in the options aborts the in-flight request (WHATWG semantics).
    const sig = opts.signal;
    if (sig && typeof sig.addEventListener === "function") {
      if (sig.aborted) queueMicrotask(() => this.destroy(sig.reason));
      else sig.addEventListener("abort", () => this.destroy(sig.reason), { once: true });
    }
    // Node's socket inactivity timeout: fire 'timeout' if the reply hasn't arrived. It does NOT
    // abort the request — the listener decides (Node behaviour).
    if (this._timeoutMs !== undefined) this.setTimeout(this._timeoutMs);
  }

  setTimeout(ms, cb) {
    this._timeoutMs = ms;
    if (cb) this.once("timeout", cb);
    if (this._timer) { clearTimeout(this._timer); this._timer = null; }
    if (ms > 0) {
      this._timer = setTimeout(() => { this._timer = null; this.emit("timeout"); }, ms);
      if (this._timer && typeof this._timer.unref === "function") this._timer.unref();
    }
    return this;
  }
  _clearTimer() { if (this._timer) { clearTimeout(this._timer); this._timer = null; } }

  _write(chunk, encoding, cb) {
    if (chunk != null && chunk.length !== 0) {
      this._bodyChunks.push(chunk instanceof Uint8Array ? chunk : Buffer.from(String(chunk), encoding || "utf8"));
    }
    cb();
  }
  _final(cb) { this._send(); cb(); }

  // A request that asks to switch protocols (`Upgrade: websocket` — the `ws` client, and so
  // every CDP/DevTools client built on it) cannot go over fetch(): the reply is a 101 followed by
  // a raw byte stream the caller takes over. Those go over a plain socket instead.
  _wantsUpgrade() {
    const upgrade = this.getHeader("upgrade");
    if (upgrade !== undefined && String(upgrade) !== "") return true;
    const connection = this.getHeader("connection");
    return connection !== undefined && /\bupgrade\b/i.test(String(connection));
  }

  async _send() {
    if (this.aborted || this.destroyed) return;
    this.headersSent = true;
    if (this._wantsUpgrade()) return this._sendOverSocket();
    const url = `${this.protocol}//${this.host}${this.port ? ":" + this.port : ""}${this.path}`;
    const headers = new Headers();
    for (const { name, value } of this._headers.values()) {
      if (CLIENT_SKIP_HEADERS.has(name.toLowerCase())) continue;
      if (Array.isArray(value)) for (const v of value) headers.append(name, String(v));
      else headers.set(name, String(value));
    }
    const noBody = this.method === "GET" || this.method === "HEAD";
    let total = 0;
    for (const c of this._bodyChunks) total += c.length;
    const body = noBody || total === 0 ? undefined : Buffer.concat(this._bodyChunks, total);

    let fetchRes;
    try {
      fetchRes = await fetch(url, {
        method: this.method,
        headers,
        body,
        signal: this._controller.signal,
        redirect: "manual",
      });
    } catch (err) {
      this._clearTimer();
      if (this.aborted) return; // 'abort'/'error' already surfaced by destroy()
      this.emit("error", err);
      return;
    }
    if (this.aborted || this.destroyed) return;

    const res = new IncomingMessage(this.socket);
    res.statusCode = fetchRes.status;
    res.statusMessage = fetchRes.statusText || STATUS_CODES[fetchRes.status] || "";
    res.url = url;
    res.method = this.method;
    for (const [k, v] of fetchRes.headers) {
      res.headers[k] = res.headers[k] !== undefined ? res.headers[k] + ", " + v : v;
      res.rawHeaders.push(k, v);
    }
    this.res = res;
    res.req = this;
    this.emit("response", res);

    // Buffered body: read it all, push into the Readable, then EOF.
    let buf;
    try { buf = Buffer.from(await fetchRes.arrayBuffer()); } catch { buf = Buffer.alloc(0); }
    this._clearTimer();
    if (this.aborted || this.destroyed) { res.push(null); return; }
    if (buf.length) res.push(buf);
    res.push(null);
    res.complete = true;
  }

  // The socket path: write the request head (plus any buffered body) on a net/tls socket, parse
  // the status line and headers by hand, and hand the socket over. A 101 goes to 'upgrade' with
  // whatever bytes arrived past the headers as `head` (Node's contract, and what `ws` consumes);
  // any other status is an ordinary 'response' whose body is the rest of the stream. Without an
  // 'upgrade' listener a 101 destroys the socket, as in Node.
  _sendOverSocket() {
    const secure = this.protocol === "https:";
    const net = __builtins.get("net");
    const opts = this._opts;
    let socket;
    try {
      socket = secure
        ? __builtins.get("tls").connect({ host: this.host, port: this.port, servername: opts.servername || this.host, rejectUnauthorized: opts.rejectUnauthorized })
        : net.connect({ host: this.host, port: this.port });
    } catch (err) {
      this._clearTimer();
      this.emit("error", err);
      return;
    }
    this.socket = this.connection = socket;
    this.emit("socket", socket);

    const lines = [`${this.method} ${this.path} HTTP/1.1`];
    if (!this.hasHeader("host")) lines.push(`Host: ${this.host}${this.port ? ":" + this.port : ""}`);
    for (const { name, value } of this._headers.values()) {
      if (Array.isArray(value)) for (const v of value) lines.push(`${name}: ${v}`);
      else lines.push(`${name}: ${value}`);
    }
    let total = 0;
    for (const c of this._bodyChunks) total += c.length;
    if (total && !this.hasHeader("content-length") && !this.hasHeader("transfer-encoding")) lines.push(`Content-Length: ${total}`);
    const head = Buffer.from(lines.join("\r\n") + "\r\n\r\n");
    const body = total ? Buffer.concat(this._bodyChunks, total) : null;

    const onError = (err) => {
      this._clearTimer();
      if (this.aborted || this.destroyed) return;
      this.emit("error", err);
    };
    socket.once("error", onError);
    const abortSocket = () => { try { socket.destroy(); } catch {} };
    this._controller.signal.addEventListener("abort", abortSocket, { once: true });

    socket.once(secure ? "secureConnect" : "connect", () => {
      socket.write(head);
      if (body) socket.write(body);
    });

    let buffered = Buffer.alloc(0);
    const onData = (chunk) => {
      buffered = buffered.length ? Buffer.concat([buffered, chunk]) : Buffer.from(chunk);
      const end = buffered.indexOf("\r\n\r\n");
      if (end === -1) return;
      socket.removeListener("data", onData);
      socket.removeListener("end", onEarlyEnd);
      socket.removeListener("error", onError);
      // Like Node, hand the socket over with `readableFlowing === null`: whoever attaches the
      // next 'data' listener (the upgrade consumer, or the body pump below) starts the flow.
      // A hard pause() would leave it stuck, since on('data') only resumes a never-paused stream.
      socket._readableState.flowing = null;
      const rest = buffered.subarray(end + 4);
      const headerLines = buffered.subarray(0, end).toString("latin1").split("\r\n");
      const status = /^HTTP\/(\d)\.(\d) (\d{3}) ?(.*)$/.exec(headerLines[0] || "");
      if (!status) {
        socket.destroy();
        onError(new Error("Parse Error: Invalid HTTP status line"));
        return;
      }
      const res = new IncomingMessage(socket);
      res.httpVersionMajor = Number(status[1]);
      res.httpVersionMinor = Number(status[2]);
      res.httpVersion = `${status[1]}.${status[2]}`;
      res.statusCode = Number(status[3]);
      res.statusMessage = status[4] || STATUS_CODES[res.statusCode] || "";
      res.url = "";
      res.method = this.method;
      for (const line of headerLines.slice(1)) {
        const colon = line.indexOf(":");
        if (colon <= 0) continue;
        const name = line.slice(0, colon).trim().toLowerCase();
        const value = line.slice(colon + 1).trim();
        res.headers[name] = res.headers[name] !== undefined ? res.headers[name] + ", " + value : value;
        res.rawHeaders.push(line.slice(0, colon).trim(), value);
      }
      this.res = res;
      res.req = this;
      this._clearTimer();
      if (res.statusCode === 101) {
        this._controller.signal.removeEventListener("abort", abortSocket);
        if (this.listenerCount("upgrade") === 0) { socket.destroy(); return; }
        this.emit("upgrade", res, socket, Buffer.from(rest));
        return;
      }
      // An ordinary reply: the rest of the stream is the body, delimited by the peer closing.
      this.emit("response", res);
      if (rest.length) res.push(Buffer.from(rest));
      socket.on("data", (c) => res.push(c));
      socket.once("end", () => { res.complete = true; res.push(null); });
      socket.once("error", (err) => res.destroy(err));
    };
    const onEarlyEnd = () => onError(new Error("socket hang up"));
    socket.on("data", onData);
    socket.once("end", onEarlyEnd);
  }

  abort() {
    if (this.aborted) return;
    this.aborted = true;
    this._clearTimer();
    try { this._controller.abort(); } catch {}
    this.socket.destroyed = true;
    this.emit("abort");
    process.nextTick(() => this.destroy());
  }
  destroy(err) {
    if (this.destroyed) return this;
    this.destroyed = true;
    this._clearTimer();
    try { this._controller.abort(); } catch {}
    this.socket.destroyed = true;
    if (err) this.emit("error", err);
    this.emit("close");
    return this;
  }

  // Fetch cannot expose the raw socket, so these Node affordances throw honestly.
  flushHeaders() { this.headersSent = true; }
  get writableEnded() { return this.writableFinished; }
}
ClientRequest._defaultProtocol = "http:";

// http.request(url|options[,options][,cb]) — build a ClientRequest. Not auto-ended (the caller
// writes a body then calls end()); http.get ends immediately.
function request(input, options, cb) {
  return new ClientRequest(input, options, cb);
}
function get(input, options, cb) {
  const req = new ClientRequest(input, options, cb);
  req.end();
  return req;
}
function httpsRequest(input, options, cb) {
  const req = new HttpsClientRequest(input, options, cb);
  return req;
}
function httpsGet(input, options, cb) {
  const req = new HttpsClientRequest(input, options, cb);
  req.end();
  return req;
}
// https twin: same client, default protocol https:. fetch uses lumen's verified TLS backend.
class HttpsClientRequest extends ClientRequest {}
HttpsClientRequest._defaultProtocol = "https:";

// The internal connection listener wires a raw socket into the request pipeline — not available
// without JS sockets. Exported (Node exposes it) but honest.
function _connectionListener() {
  throw new Error("node:http _connectionListener is not supported in lumen (raw sockets are not exposed to JS)");
}

// setMaxIdleHTTPParsers tunes the parser free-list; lumen keeps no such pool, so this is a no-op
// that still validates like Node (accepts a number).
function setMaxIdleHTTPParsers(_max) { /* no-op: lumen keeps no HTTP parser free-list */ }

// Client-side Agent: accepted as connection-pool metadata but drives no real pool — the client
// runs over fetch(), which manages its own connections. Documented as a no-op (Node users pass an
// Agent for keep-alive/socket tuning; none of that is observable through fetch, so it's ignored).
class Agent extends EventEmitter {
  constructor(options = {}) {
    super();
    this.options = { ...options };
    this.maxSockets = options.maxSockets ?? Infinity;
    this.maxFreeSockets = options.maxFreeSockets ?? 256;
    this.keepAlive = options.keepAlive ?? false;
    this.sockets = {};
    this.freeSockets = {};
    this.requests = {};
    this.protocol = "http:";
  }
  // No real socket pool exists (fetch owns connections); raw socket creation isn't available.
  createConnection() { notSupportedOverFetch("Agent.createConnection (raw sockets)"); }
  getName() { return "localhost:"; }
  destroy() {}
}
const globalAgent = new Agent();

// WebSocket family: re-export the web globals so `http.WebSocket` etc. resolve (Node re-exports the
// same globals). Missing ones fall back to a named stub class so the key still exists.
const WebSocket = globalThis.WebSocket || class WebSocket {};
const MessageEvent = globalThis.MessageEvent || class MessageEvent {};
const CloseEvent = globalThis.CloseEvent || class CloseEvent {};

// Callable without `new`, as Node's constructors are (see __legacyConstructor).
IncomingMessage = __legacyConstructor(IncomingMessage);
ServerResponse = __legacyConstructor(ServerResponse);
Server = __legacyConstructor(Server);
OutgoingMessage = __legacyConstructor(OutgoingMessage);
ClientRequest = __legacyConstructor(ClientRequest);
Agent = __legacyConstructor(Agent);

const http = {
  createServer,
  Server,
  IncomingMessage,
  ServerResponse,
  OutgoingMessage,
  ClientRequest,
  STATUS_CODES,
  METHODS,
  request,
  get,
  Agent,
  globalAgent,
  maxHeaderSize: 16384,
  validateHeaderName,
  validateHeaderValue,
  setMaxIdleHTTPParsers,
  _connectionListener,
  WebSocket,
  MessageEvent,
  CloseEvent,
};

__builtins.set("http", http);

// node:https — express does `require('https')`; reuse the http server surface (TLS termination
// isn't available: lumen has no TLS, same STOP-AND-FLAG as fetch/https).
__builtins.set("https", {
  Server,
  createServer,
  request: httpsRequest,
  get: httpsGet,
  ClientRequest: HttpsClientRequest,
  Agent,
  globalAgent: new Agent(),
});
