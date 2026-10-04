// Headers/Request/Response + fetch over the shared transport. Body ownership and
// disturbance use lumen-node's WHATWG streams, including asynchronous sources.

function normalizeHeaderName(name) {
  name = String(name);
  if (name === "" || /[^\x21-\x7e]/.test(name) || /[()<>@,;:\\"/[\]?={} \t]/.test(name)) {
    throw new TypeError(`invalid header name '${name}'`);
  }
  return name.toLowerCase();
}

class Headers {
  constructor(init) {
    this._map = new Map(); // lower-name -> { name, value }
    this._guard = "none";
    if (init instanceof Headers) {
      for (const [k, v] of init) this.append(k, v);
    } else if (Array.isArray(init)) {
      for (const pair of init) {
        if (!pair || pair.length !== 2) throw new TypeError("Headers: init pair needs two items");
        this.append(pair[0], pair[1]);
      }
    } else if (init && typeof init === "object") {
      for (const k of Object.keys(init)) this.append(k, init[k]);
    }
  }
  append(name, value) {
    const key = normalizeHeaderName(name);
    value = String(value);
    this._assertMutable();
    if (/[\r\n\0]/.test(value)) throw new TypeError("invalid header value");
    value = value.trim();
    const existing = this._map.get(key);
    this._map.set(key, { name: key, value: existing ? `${existing.value}, ${value}` : value });
  }
  delete(name) {
    this._assertMutable();
    this._map.delete(normalizeHeaderName(name));
  }
  get(name) {
    const hit = this._map.get(normalizeHeaderName(name));
    return hit ? hit.value : null;
  }
  has(name) {
    return this._map.has(normalizeHeaderName(name));
  }
  set(name, value) {
    const key = normalizeHeaderName(name);
    value = String(value);
    this._assertMutable();
    if (/[\r\n\0]/.test(value)) throw new TypeError("invalid header value");
    value = value.trim();
    this._map.set(key, { name: key, value });
  }
  _assertMutable() {
    if (this._guard === "immutable") throw new TypeError("immutable Headers object");
  }
  forEach(fn, thisArg) {
    for (const [k, v] of this) fn.call(thisArg, v, k, this);
  }
  *entries() {
    // Sorted by name, per spec.
    const keys = [...this._map.keys()].sort();
    for (const k of keys) yield [k, this._map.get(k).value];
  }
  *keys() {
    for (const [k] of this) yield k;
  }
  *values() {
    for (const [, v] of this) yield v;
  }
  [Symbol.iterator]() {
    return this.entries();
  }
  _pairs() {
    return [...this].map(([k, v]) => [k, v]);
  }
}

const kConsumed = Symbol("bodyConsumed");
const kBodyStream = Symbol("bodyStream");
// A user-supplied ReadableStream body, kept un-drained until the body is actually consumed or
// sent. Draining at construction would break feature-detection code (e.g. ky) that builds — but
// never sends — a `new Request(url, { body: new ReadableStream() })` just to probe support.
const kSourceStream = Symbol("bodySourceStream");
function browserPolicyAvailable() {
  if (__http && (__http.policyHandledByHost === true ||
      (typeof __http.policyHandledByHost === "function" && __http.policyHandledByHost()))) return false;
  const nativeOrigin = typeof __http.browserOrigin === "function" ? __http.browserOrigin() : null;
  return nativeOrigin != null || !!(globalThis.document || globalThis.location);
}
function requestOrigin() {
  if (typeof __http.browserOrigin === "function") {
    const origin = __http.browserOrigin();
    if (origin != null) return origin;
  }
  const value = globalThis.document && globalThis.document.URL ||
    globalThis.location && globalThis.location.href;
  const record = value ? __url.parse(String(value), undefined) : null;
  return record ? record[10] : "null";
}
function sameOrigin(a, b) {
  const first = __url.parse(String(a), undefined), second = __url.parse(String(b), undefined);
  return first !== null && second !== null && first[10] !== "null" && first[10] === second[10];
}
function corsCheck(headers, origin, credentials) {
  const allowed = headers.get("access-control-allow-origin");
  if (allowed === null) return false;
  if (credentials === "include") return allowed === origin && headers.get("access-control-allow-credentials") === "true";
  return allowed === "*" || allowed === origin;
}
const corsSafelistedMethods = new Set(["GET", "HEAD", "POST"]);
const corsSafelistedHeaders = new Set(["accept", "accept-language", "content-language", "content-type"]);
const corsSafelistedContentTypes = new Set(["application/x-www-form-urlencoded", "multipart/form-data", "text/plain"]);
const forbiddenRequestHeaders = new Set(["accept-charset", "accept-encoding", "access-control-request-headers", "access-control-request-method", "connection", "content-length", "cookie", "cookie2", "date", "dnt", "expect", "host", "keep-alive", "origin", "referer", "set-cookie", "te", "trailer", "transfer-encoding", "upgrade", "via"]);
const corsExposedHeaders = new Set(["cache-control", "content-language", "content-length", "content-type", "expires", "last-modified", "pragma"]);
function corsUnsafeHeaderNames(headers) {
  const unsafe = [];
  const potentiallyUnsafe = [];
  let safelistValueSize = 0;
  for (const [name, value] of headers) {
    if (forbiddenRequestHeaders.has(name) || name.startsWith("proxy-") || name.startsWith("sec-")) continue;
    const unsafeByte = /[\x00-\x08\x0a-\x1f\x7f"():<>?@[\\\]{}]/.test(value);
    let safelisted = false;
    if (value.length <= 128 && corsSafelistedHeaders.has(name)) {
      const unsafeValue = /[^\x09\x20-\x7e]/.test(value) ||
        ((name === "accept" || name === "content-type") && unsafeByte) ||
        ((name === "accept-language" || name === "content-language") && /[^0-9A-Za-z *,-.;=]/.test(value));
      safelisted = !unsafeValue;
      if (safelisted && name === "content-type") {
        const essence = value.split(";", 1)[0].trim().toLowerCase();
        if (!corsSafelistedContentTypes.has(essence)) safelisted = false;
      }
    } else if (name === "range" && value.length <= 128) {
      const range = /^bytes=([0-9]+)-([0-9]*)$/.exec(value);
      safelisted = !!range && (!range[2] || BigInt(range[1]) <= BigInt(range[2]));
    }
    if (safelisted) {
      potentiallyUnsafe.push(name);
      safelistValueSize += value.length;
    } else unsafe.push(name);
  }
  if (safelistValueSize > 1024) unsafe.push(...potentiallyUnsafe);
  return [...new Set(unsafe)].sort();
}
function visibleCorsHeaders(headers, credentials) {
  const result = new Headers();
  const exposed = (headers.get("access-control-expose-headers") || "").split(",").map(x => x.trim().toLowerCase()).filter(Boolean);
  const wildcard = credentials !== "include" && exposed.includes("*");
  for (const [name, value] of headers) {
    if (name === "set-cookie" || name === "set-cookie2") continue;
    if (corsExposedHeaders.has(name) || wildcard || exposed.includes(name)) result.set(name, value);
  }
  return result;
}
function opaqueResponse(raw = undefined, type = "opaque") {
  const response = new Response(null);
  response.status = 0;
  response.statusText = "";
  response.type = raw && raw.type || type;
  response.url = "";
  response.headers = new Headers();
  response.headers._guard = "immutable";
  response._bodyBytes = undefined;
  response.redirected = !!(raw && raw.redirected);
  return response;
}

// Set `owner`'s body from a BodyInit, and (per spec) a default Content-Type when the body implies
// one and none is already set. A ReadableStream is stored, not drained (see kSourceStream).
function initBody(owner, body) {
  let contentType;
  if (body === undefined || body === null) {
    owner._bodyBytes = undefined;
  } else if (body instanceof globalThis.ReadableStream) {
    if (body.locked || body[Symbol.for("nodejs.stream.kIsDisturbed")]) {
      throw new TypeError("body stream is locked or already disturbed");
    }
    owner[kSourceStream] = body;
    owner._bodyBytes = undefined;
  } else if (body instanceof Blob) {
    owner._bodyBytes = body[kBlobBytes].slice();
    if (body.type) contentType = body.type;
  } else if (body instanceof FormData) {
    const encoded = encodeFormData(body);
    owner._bodyBytes = encoded.bytes;
    contentType = encoded.contentType;
  } else if (typeof body === "string") {
    owner._bodyBytes = new TextEncoder().encode(body);
    contentType = "text/plain;charset=UTF-8";
  } else if (body instanceof URLSearchParams) {
    owner._bodyBytes = new TextEncoder().encode(body.toString());
    contentType = "application/x-www-form-urlencoded;charset=UTF-8";
  } else {
    owner._bodyBytes = toBodyBytes(body);
  }
  if (contentType && owner.headers && !owner.headers.has("content-type")) {
    owner.headers.set("content-type", contentType);
  }
}

function bodyMixin(proto) {
  proto.text = async function () {
    return new TextDecoder().decode(await this._consume());
  };
  proto.json = async function () {
    return JSON.parse(await this.text());
  };
  proto.arrayBuffer = async function () {
    const bytes = await this._consume();
    return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
  };
  proto.bytes = async function () {
    return this._consume();
  };
  proto.blob = async function () {
    const bytes = await this._consume();
    const type = (this.headers && this.headers.get("content-type")) || "";
    return new Blob([bytes], { type });
  };
  proto.formData = async function () {
    const ct = (this.headers && this.headers.get("content-type")) || "";
    if (ct.startsWith("application/x-www-form-urlencoded")) {
      const form = new FormData();
      for (const [k, v] of new URLSearchParams(await this.text())) form.append(k, v);
      return form;
    }
    const m = /boundary=([^;]+)/i.exec(ct);
    if (ct.startsWith("multipart/form-data") && m) {
      return decodeMultipart(await this._consume(), m[1].trim().replace(/^"|"$/g, ""));
    }
    throw new TypeError(`formData(): unsupported content-type '${ct}'`);
  };
  proto._consume = async function (signal) {
    if (this.bodyUsed || (this.body && this.body.locked)) throw new TypeError("body already consumed or locked");
    const stream = this.body;
    // A null body is usable repeatedly; it never becomes disturbed.
    if (stream === null) return new Uint8Array(0);
    this[kConsumed] = true;
    return drainStreamAsync(stream, signal);
  };
  Object.defineProperty(proto, "bodyUsed", {
    get() {
      const stream = this[kSourceStream] || this[kBodyStream];
      return !!this[kConsumed] || !!(stream && stream[Symbol.for("nodejs.stream.kIsDisturbed")]);
    },
  });
  // `.body` is the user's ReadableStream if one was given (un-drained), else a stream over the
  // buffered bytes, or `null` when there is no body. The same stream instance is handed out on
  // repeated access (per spec); reading it consumes the body.
  Object.defineProperty(proto, "body", {
    configurable: true,
    get() {
      if (this[kSourceStream] !== undefined) return this[kSourceStream];
      if (this._bodyBytes === undefined) return null;
      if (this[kBodyStream] === undefined) this[kBodyStream] = makeBodyStream(this);
      return this[kBodyStream];
    },
  });
}

function toBodyBytes(body) {
  if (body === undefined || body === null) return undefined;
  if (typeof body === "string") return new TextEncoder().encode(body);
  if (body instanceof Uint8Array) return body;
  if (body instanceof ArrayBuffer) return new Uint8Array(body);
  if (ArrayBuffer.isView(body)) return new Uint8Array(body.buffer, body.byteOffset, body.byteLength);
  if (body instanceof URLSearchParams) return new TextEncoder().encode(body.toString());
  return new TextEncoder().encode(String(body));
}

// Body consumption may await the transport or a user supplied source.
async function drainStreamAsync(stream, signal) {
  if (stream.locked) throw new TypeError("cannot consume a locked ReadableStream");
  const reader = stream.getReader();
  const parts = [];
  let total = 0;
  const aborted = () => signal.reason || new DOMException("The operation was aborted", "AbortError");
  const onAbort = () => { reader.cancel(aborted()).catch(()=>{}); };
  if (signal) signal.addEventListener("abort", onAbort);
  try {
    if (signal && signal.aborted) { onAbort(); throw aborted(); }
    for (;;) {
      const result = await reader.read();
      if (signal && signal.aborted) throw aborted();
      if (result.done) break;
      const chunk = result.value;
      if (!(chunk instanceof Uint8Array)) throw new TypeError("body stream chunk must be a Uint8Array");
      parts.push(chunk);
      total += chunk.byteLength;
    }
  } finally {
    if (signal) signal.removeEventListener("abort", onAbort);
    reader.releaseLock();
  }
  return concatenateBodyChunks(parts, total);
}

function concatenateBodyChunks(parts, total) {
  if (parts.length === 1) return parts[0];
  const bytes = new Uint8Array(total);
  let offset = 0;
  for (const part of parts) {
    bytes.set(part, offset);
    offset += part.byteLength;
  }
  return bytes;
}

// The `.body` ReadableStream for a Request/Response: it lazily hands out the buffered bytes as a
// single chunk (marking the body consumed, shared with `text()`/`json()`), or is empty when there
// is no body.
function makeBodyStream(owner) {
  return new globalThis.ReadableStream({
    type: "bytes",
    pull(controller) {
      const bytes = owner._bodyBytes;
      // Byte controllers transfer buffers. Keep BodyInit's original bytes intact.
      if (bytes && bytes.length) controller.enqueue(bytes.slice());
      controller.close();
    },
  });
}

function cloneBody(owner) {
  const body = owner.body;
  if (owner.bodyUsed || (body && body.locked)) throw new TypeError("cannot clone a used or locked body");
  if (body === null) return null;
  const branches = body[Symbol.for("lumen.cloneBody")]();
  owner[kSourceStream] = branches[0];
  owner[kBodyStream] = undefined;
  owner._bodyBytes = undefined;
  return branches[1];
}

// A streaming request body needs `duplex: "half"` (the only value the Fetch standard defines).
function requireDuplex(init) {
  if (init.duplex !== undefined && init.duplex !== "half") {
    throw new TypeError(`RequestInit: duplex option must be 'half', got '${init.duplex}'`);
  }
  if (init.body instanceof globalThis.ReadableStream && init.duplex !== "half") {
    throw new TypeError("RequestInit: duplex option is required when sending a body.");
  }
}

class Request {
  constructor(input, init = {}) {
    init = init && typeof init === "object" ? init : {};
    if (input instanceof Request) {
      this.url = input.url;
      this.method = init.method ? String(init.method).toUpperCase() : input.method;
      this.headers = new Headers(init.headers || input.headers);
      this.mode = init.mode === undefined ? input.mode : String(init.mode);
      this.credentials = init.credentials === undefined ? input.credentials : String(init.credentials);
      this.redirect = init.redirect === undefined ? input.redirect : String(init.redirect);
      if ("body" in init) {
        requireDuplex(init);
        initBody(this, init.body);
      } else {
        const source = input.body;
        if (input.bodyUsed || (source && source.locked)) throw new TypeError("source Request body is already used or locked");
        // An identity transform transfers body ownership and disturbs the source.
        initBody(this, source === null ? null : source.pipeThrough(new TransformStream()));
      }
      this.signal = init.signal || input.signal || null;
    } else {
      const base = globalThis.document?.baseURI || globalThis.location?.href;
      this.url = new URL(String(input), base).href;
      this.method = init.method ? String(init.method).toUpperCase() : "GET";
      this.headers = new Headers(init.headers);
      this.mode = init.mode === undefined ? (browserPolicyAvailable() ? "cors" : "cors") : String(init.mode);
      this.credentials = init.credentials === undefined ? "same-origin" : String(init.credentials);
      this.redirect = init.redirect === undefined ? "follow" : String(init.redirect);
      requireDuplex(init);
      initBody(this, init.body);
      this.signal = init.signal || null;
    }
    if (!["cors", "no-cors", "same-origin"].includes(this.mode)) throw new TypeError(`invalid request mode '${this.mode}'`);
    if (!["omit", "same-origin", "include"].includes(this.credentials)) throw new TypeError(`invalid credentials mode '${this.credentials}'`);
    if (!["follow", "error", "manual"].includes(this.redirect)) throw new TypeError(`invalid redirect mode '${this.redirect}'`);
    if (!this.method || /[^!#$%&'*+.^_`|~0-9A-Za-z-]/.test(this.method)) throw new TypeError(`invalid method '${this.method}'`);
    if (["CONNECT", "TRACE", "TRACK"].includes(this.method)) throw new TypeError(`forbidden method '${this.method}'`);
    if (
      (this.method === "GET" || this.method === "HEAD") &&
      (this._bodyBytes !== undefined || this[kSourceStream] !== undefined)
    ) {
      throw new TypeError(`${this.method} request cannot have a body`);
    }
    this[kConsumed] = false;
  }
  get duplex() {
    return "half";
  }
  clone() {
    return new Request(this.url, {
      method: this.method,
      headers: this.headers,
      body: cloneBody(this),
      signal: this.signal,
      mode: this.mode,
      credentials: this.credentials,
      redirect: this.redirect,
      duplex: "half",
    });
  }
}
bodyMixin(Request.prototype);

class Response {
  constructor(body = null, init = {}) {
    init = init && typeof init === "object" ? init : {};
    // A dictionary member set to `undefined` counts as absent (WebIDL), so `{ status: undefined }`
    // takes the default 200 rather than coercing to `Number(undefined)` → NaN.
    this.status = init.status !== undefined ? Number(init.status) : 200;
    if (!Number.isInteger(this.status) || this.status < 200 || this.status > 599) {
      throw new RangeError(`invalid response status ${this.status}`);
    }
    this.statusText = init.statusText !== undefined ? String(init.statusText) : "";
    if (/[^\x09\x20-\x7e\x80-\xff]/.test(this.statusText)) throw new TypeError("invalid response status text");
    if (body !== null && body !== undefined && [204, 205, 304].includes(this.status)) throw new TypeError("response status cannot have a body");
    this.headers = new Headers(init.headers);
    this.url = "";
    this.redirected = false;
    initBody(this, body);
    this[kConsumed] = false;
  }
  get ok() {
    return this.status >= 200 && this.status < 300;
  }
  clone() {
    const body = cloneBody(this);
    const r = new Response(body, {
      status: this.status || 200,
      statusText: this.statusText,
      headers: this.headers,
    });
    r.url = this.url;
    r.redirected = this.redirected;
    r.status = this.status;
    r.type = this.type;
    if (this.headers._guard === "immutable") r.headers._guard = "immutable";
    return r;
  }
  static json(data, init) {
    const r = new Response(JSON.stringify(data), init);
    if (!r.headers.has("content-type")) r.headers.set("content-type", "application/json");
    return r;
  }
  static error() {
    const r = new Response(null, { status: 200 });
    r.status = 0;
    r.type = "error";
    return r;
  }
}
bodyMixin(Response.prototype);

async function fetch(input, init = {}) {
  const request = new Request(input, init);
  const signal = request.signal;
  const abortReason = () => signal && signal.aborted ? signal.reason : new DOMException("The operation was aborted", "AbortError");
  if (signal && signal.aborted) throw abortReason();
  const browser = browserPolicyAvailable();
  const origin = requestOrigin();
  let corsOrigin = origin;
  const mode = request.mode;
  const crossOrigin = browser && origin !== null && !sameOrigin(request.url, origin);
  if (browser && mode === "same-origin" && crossOrigin) throw new TypeError("Fetch blocked by same-origin mode");
  if (browser && mode === "no-cors") {
    if (!corsSafelistedMethods.has(request.method)) throw new TypeError("no-cors requests require GET, HEAD, or POST");
    if (request.redirect !== "follow") throw new TypeError("no-cors requests require redirect mode follow");
  }
  const hasBody = request._bodyBytes !== undefined || request[kSourceStream] !== undefined;
  let bodyBytes = hasBody ? await request._consume(signal) : undefined;
  if (signal && signal.aborted) throw abortReason();
  const sendRaw = (method, url, headers, bytes, redirect = "follow") => new Promise((resolve, reject) => {
    let done = false;
    let handle;
    const onAbort = () => { if (handle) handle.abort(); if (!done) { done = true; reject(abortReason()); } };
    if (signal) signal.addEventListener("abort", onAbort);
    const finish = fn => value => { if (done) { if (value && value.bodyReader) value.bodyReader.cancel(); return; } done = true; if (signal) signal.removeEventListener("abort", onAbort); fn(value); };
    try {
      handle = __http.request(method, url, headers, bytes, finish(raw => {
        if (raw && !(raw.headers instanceof Headers)) raw.headers = new Headers(raw.headers || []);
        resolve(raw);
      }), finish(error => reject(error instanceof Error ? error : new TypeError(String(error)))), redirect, {mode: request.mode, credentials: request.credentials, redirect: request.redirect});
      if (signal && signal.aborted) onAbort();
    } catch (error) { finish(reject)(error); }
  });
  const requestHeaders = request.headers._pairs().filter(([name, value]) => {
    if (!browser) return true;
    if (forbiddenRequestHeaders.has(name) || name.startsWith("proxy-") || name.startsWith("sec-")) return false;
    if (mode === "no-cors") {
      if (!corsSafelistedHeaders.has(name) || corsUnsafeHeaderNames(new Headers([[name, value]])).length) return false;
    }
    return true;
  });
  const browserCrossCors = browser && mode === "cors" && crossOrigin;
  const preflightFor = async targetUrl => {
    if (!browser || mode !== "cors" || (sameOrigin(targetUrl, origin) && corsOrigin === origin)) return;
    const currentHeaders = new Headers(headers);
    const unsafe = corsUnsafeHeaderNames(currentHeaders);
    if (!corsSafelistedMethods.has(method) || unsafe.length) {
      const preflightHeaders = [["accept", "*/*"], ["origin", corsOrigin], ["access-control-request-method", method]];
      if (unsafe.length) preflightHeaders.push(["access-control-request-headers", unsafe.join(", ")]);
      const preflight = await sendRaw("OPTIONS", targetUrl, preflightHeaders, undefined, "manual");
      if (preflight.bodyReader) preflight.bodyReader.cancel();
      const methods = (preflight.headers.get("access-control-allow-methods") || "").split(",").map(x => x.trim().toUpperCase());
      const allowedHeaders = (preflight.headers.get("access-control-allow-headers") || "").split(",").map(x => x.trim().toLowerCase());
      const wildcardHeaders = request.credentials !== "include" && allowedHeaders.includes("*");
      if (!corsCheck(preflight.headers, corsOrigin, request.credentials) || preflight.status < 200 || preflight.status >= 300 || (!methods.includes(method) && !corsSafelistedMethods.has(method) && !(request.credentials !== "include" && methods.includes("*"))) || unsafe.some(name => !allowedHeaders.includes(name) && !(wildcardHeaders && name !== "authorization"))) throw new TypeError("CORS preflight failed");
    }
  };
  let method = request.method;
  let url = request.url;
  let headers = requestHeaders;
  await preflightFor(url);
  if (browserCrossCors && !headers.some(([name]) => name === "origin")) headers = [...headers, ["origin", corsOrigin]];
  let redirected = false;
  let sawCrossOrigin = crossOrigin;
  for (let hops = 0; ; hops++) {
    if (browser && mode === "cors" && (!sameOrigin(url, origin) || corsOrigin !== origin)) {
      if (headers.some(([name]) => name.toLowerCase() === "origin")) headers = headers.map(([name, value]) => name.toLowerCase() === "origin" ? [name, corsOrigin] : [name, value]);
      else headers = [...headers, ["origin", corsOrigin]];
    }
    const transportRedirect = browser || request.redirect !== "follow" ? "manual" : "follow";
    const raw = await sendRaw(method, url, headers, bodyBytes, transportRedirect);
    if (raw.type === "opaque" || raw.type === "opaqueredirect" || raw.status === 0) return opaqueResponse(raw);
    const isRedirect = [301, 302, 303, 307, 308].includes(raw.status) && raw.headers.get("location");
    const hopCrossOrigin = browser && mode === "cors" && (corsOrigin !== origin || !sameOrigin(url, origin));
    if (hopCrossOrigin && !corsCheck(raw.headers, corsOrigin, request.credentials)) {
      if (raw.bodyReader) raw.bodyReader.cancel();
      throw new TypeError("CORS check failed");
    }
    if (isRedirect) {
      if (request.redirect === "error" || (mode === "no-cors" && request.redirect !== "follow")) { if (raw.bodyReader) raw.bodyReader.cancel(); throw new TypeError("redirect is disallowed"); }
      if (request.redirect === "manual") { if (raw.bodyReader) raw.bodyReader.cancel(); const r = opaqueResponse(); r.type = "opaqueredirect"; return r; }
      if (hops >= 19) { if (raw.bodyReader) raw.bodyReader.cancel(); throw new TypeError("too many redirects"); }
      const next = new URL(raw.headers.get("location"), url);
      if (next.username || next.password) { if (raw.bodyReader) raw.bodyReader.cancel(); throw new TypeError("redirect URL contains credentials"); }
      if (browser && mode === "same-origin" && !sameOrigin(next.href, origin)) { if (raw.bodyReader) raw.bodyReader.cancel(); throw new TypeError("redirect violates same-origin mode"); }
      if (raw.status === 303 && method !== "GET" && method !== "HEAD" || (raw.status === 301 || raw.status === 302) && method === "POST") {
        method = "GET";
        bodyBytes = undefined;
        headers = headers.filter(([name]) => !["content-encoding", "content-language", "content-location", "content-type"].includes(name.toLowerCase()));
      }
      if (!sameOrigin(url, next.href)) headers = headers.filter(([name]) => name.toLowerCase() !== "authorization");
      if (!sameOrigin(url, next.href) && !sameOrigin(url, origin)) {
        corsOrigin = "null";
        headers = headers.map(([name, value]) => name.toLowerCase() === "origin" ? [name, corsOrigin] : [name, value]);
      }
      url = next.href;
      redirected = true;
      if (browser && origin !== null && !sameOrigin(url, origin)) sawCrossOrigin = true;
      if (raw.bodyReader) raw.bodyReader.cancel();
      await preflightFor(url);
      continue;
    }
    const noCorsTainted = browser && mode === "no-cors" && sawCrossOrigin;
    if (noCorsTainted) { if (raw.bodyReader) raw.bodyReader.cancel(); return opaqueResponse(); }
    const reader = raw.bodyReader;
    let body = raw.body;
    if (reader) {
      let finished = false;
      let streamController = null;
      const finish = () => { finished = true; if (signal) signal.removeEventListener("abort", onAbort); };
      const onAbort = () => {
        if (finished) return;
        finish();
        if (streamController) streamController.error(abortReason());
        reader.cancel();
      };
      if (signal) signal.addEventListener("abort", onAbort);
      body = new ReadableStream({
        type: "bytes",
        start(controller) { streamController = controller; if (signal && signal.aborted) onAbort(); },
        async pull(controller) {
          try { const chunk = await reader.read(); if (finished) return; if (chunk === null) { controller.close(); finish(); } else controller.enqueue(new Uint8Array(chunk)); }
          catch (error) { if (!finished) controller.error(signal && signal.aborted ? abortReason() : error); finish(); reader.cancel(); }
        },
        cancel() { finish(); return reader.cancel(); },
      });
    }
    const response = new Response(body, {status: raw.status, statusText: raw.statusText, headers: raw.headers});
    response.url = raw.url || url;
    response.redirected = raw.redirected === undefined ? redirected : !!raw.redirected;
    if (raw.type === "basic" || raw.type === "cors") response.type = raw.type;
    if (browser) {
      response.type = sawCrossOrigin && mode === "cors" ? "cors" : "basic";
      if (sawCrossOrigin && mode === "cors") response.headers = visibleCorsHeaders(response.headers, request.credentials);
    }
    response.headers._guard = "immutable";
    return response;
  }
}

globalThis.Headers = Headers;
globalThis.Request = Request;
globalThis.Response = Response;
globalThis.fetch = fetch;
