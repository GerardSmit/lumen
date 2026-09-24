// Small node: builtins the Express stack pulls in. Each is the practical subset its consumers
// use, not a full implementation; gaps throw clearly rather than silently misbehaving.

// ---- node:perf_hooks --------------------------------------------------------------------------
// The web `performance` global, extended with a real mark/measure entry buffer that dispatches to
// PerformanceObserver. lumen's global `performance` has now()/timeOrigin but no user-timing API, so
// we add mark/measure/getEntries here and wire them to observers — marks and measures are real.
// The observer machinery for entry types lumen cannot produce (gc, http, resource…) simply never
// fires, which is the honest behavior for a runtime that emits no such entries.
{
  const perf = globalThis.performance;
  const now = () => perf.now();

  const buffer = []; // all recorded PerformanceEntry objects
  const observers = new Set(); // live PerformanceObserver instances

  class PerformanceEntry {
    constructor(name, entryType, startTime, duration) {
      this.name = name;
      this.entryType = entryType;
      this.startTime = startTime;
      this.duration = duration;
    }
    toJSON() {
      return { name: this.name, entryType: this.entryType, startTime: this.startTime, duration: this.duration };
    }
  }
  class PerformanceMark extends PerformanceEntry {
    constructor(name, options) {
      super(name, "mark", options && options.startTime !== undefined ? options.startTime : now(), 0);
      this.detail = (options && options.detail) ?? null;
    }
  }
  class PerformanceMeasure extends PerformanceEntry {
    constructor(name, startTime, duration, detail) {
      super(name, "measure", startTime, duration);
      this.detail = detail ?? null;
    }
  }
  class PerformanceResourceTiming extends PerformanceEntry {
    constructor(name, startTime, duration) {
      super(name, "resource", startTime ?? 0, duration ?? 0);
    }
  }

  class PerformanceObserverEntryList {
    constructor(entries) { this._entries = entries; }
    getEntries() { return this._entries.slice(); }
    getEntriesByName(name, type) {
      return this._entries.filter((e) => e.name === name && (type === undefined || e.entryType === type));
    }
    getEntriesByType(type) { return this._entries.filter((e) => e.entryType === type); }
  }

  class PerformanceObserver {
    constructor(callback) {
      this._callback = callback;
      this._types = new Set();
      this._pending = [];
    }
    observe(options = {}) {
      const types = options.entryTypes || (options.type ? [options.type] : []);
      for (const t of types) this._types.add(t);
      observers.add(this);
      if (options.buffered) {
        const matching = buffer.filter((e) => this._types.has(e.entryType));
        if (matching.length) {
          this._pending.push(...matching);
          queueMicrotask(() => this._flush());
        }
      }
    }
    disconnect() {
      observers.delete(this);
      this._types.clear();
      this._pending = [];
    }
    takeRecords() {
      const records = this._pending;
      this._pending = [];
      return records;
    }
    _deliver(entry) {
      this._pending.push(entry);
      queueMicrotask(() => this._flush());
    }
    _flush() {
      if (this._pending.length === 0) return;
      const list = new PerformanceObserverEntryList(this._pending);
      this._pending = [];
      this._callback(list, this);
    }
  }
  PerformanceObserver.supportedEntryTypes = ["mark", "measure", "resource"];

  function record(entry) {
    buffer.push(entry);
    for (const obs of observers) {
      if (obs._types.has(entry.entryType)) obs._deliver(entry);
    }
    return entry;
  }

  // Augment the global `performance` with the user-timing API if it isn't already present.
  if (typeof perf.mark !== "function") {
    perf.mark = function mark(name, options) {
      return record(new PerformanceMark(name, options));
    };
    perf.measure = function measure(name, startOrOptions, endMark) {
      let start, end;
      if (startOrOptions && typeof startOrOptions === "object") {
        start = resolveMark(startOrOptions.start);
        end = startOrOptions.end !== undefined ? resolveMark(startOrOptions.end) : now();
        if (startOrOptions.duration !== undefined && startOrOptions.start !== undefined) {
          end = start + startOrOptions.duration;
        }
        return record(new PerformanceMeasure(name, start, end - start, startOrOptions.detail));
      }
      start = startOrOptions !== undefined ? resolveMark(startOrOptions) : 0;
      end = endMark !== undefined ? resolveMark(endMark) : now();
      return record(new PerformanceMeasure(name, start, end - start));
    };
    perf.clearMarks = function clearMarks(name) {
      for (let i = buffer.length - 1; i >= 0; i--) {
        if (buffer[i].entryType === "mark" && (name === undefined || buffer[i].name === name)) buffer.splice(i, 1);
      }
    };
    perf.clearMeasures = function clearMeasures(name) {
      for (let i = buffer.length - 1; i >= 0; i--) {
        if (buffer[i].entryType === "measure" && (name === undefined || buffer[i].name === name)) buffer.splice(i, 1);
      }
    };
    perf.getEntries = () => buffer.slice();
    perf.getEntriesByName = (name, type) =>
      buffer.filter((e) => e.name === name && (type === undefined || e.entryType === type));
    perf.getEntriesByType = (type) => buffer.filter((e) => e.entryType === type);
    perf.clearResourceTimings = () => {
      for (let i = buffer.length - 1; i >= 0; i--) if (buffer[i].entryType === "resource") buffer.splice(i, 1);
    };
  }

  function resolveMark(nameOrTime) {
    if (typeof nameOrTime === "number") return nameOrTime;
    for (let i = buffer.length - 1; i >= 0; i--) {
      if (buffer[i].entryType === "mark" && buffer[i].name === nameOrTime) return buffer[i].startTime;
    }
    throw new Error(`The "${nameOrTime}" performance mark has not been set`);
  }

  // A simple, real recordable histogram (used by monitorEventLoopDelay and createHistogram).
  function makeHistogram() {
    let samples = [];
    return {
      record(value) { samples.push(Number(value)); },
      recordDelta() {},
      enable() { return true; },
      disable() { return true; },
      reset() { samples = []; },
      get count() { return samples.length; },
      get min() { return samples.length ? Math.min(...samples) : 0; },
      get max() { return samples.length ? Math.max(...samples) : 0; },
      get mean() { return samples.length ? samples.reduce((a, b) => a + b, 0) / samples.length : 0; },
      get stddev() {
        if (samples.length < 2) return 0;
        const m = samples.reduce((a, b) => a + b, 0) / samples.length;
        return Math.sqrt(samples.reduce((a, b) => a + (b - m) ** 2, 0) / samples.length);
      },
      get exceeds() { return 0; },
      percentile(p) {
        if (samples.length === 0) return 0;
        const sorted = samples.slice().sort((a, b) => a - b);
        const idx = Math.min(sorted.length - 1, Math.ceil((p / 100) * sorted.length) - 1);
        return sorted[Math.max(0, idx)];
      },
      get percentiles() { return new Map(); },
    };
  }

  // The V8 perf-milestone constants Node exposes. lumen isn't V8, so these are the documented
  // enum values (their numeric identity is what code compares against), with no live milestones.
  const constants = {
    NODE_PERFORMANCE_GC_MAJOR: 4,
    NODE_PERFORMANCE_GC_MINOR: 1,
    NODE_PERFORMANCE_GC_INCREMENTAL: 8,
    NODE_PERFORMANCE_GC_WEAKCB: 16,
    NODE_PERFORMANCE_GC_FLAGS_NO: 0,
    NODE_PERFORMANCE_GC_FLAGS_CONSTRUCT_RETAINED: 2,
    NODE_PERFORMANCE_GC_FLAGS_FORCED: 4,
    NODE_PERFORMANCE_GC_FLAGS_SYNCHRONOUS_PHANTOM_PROCESSING: 8,
    NODE_PERFORMANCE_GC_FLAGS_ALL_AVAILABLE_GARBAGE: 16,
    NODE_PERFORMANCE_GC_FLAGS_ALL_EXTERNAL_MEMORY: 32,
    NODE_PERFORMANCE_GC_FLAGS_SCHEDULE_IDLE: 64,
  };

  __builtins.set("perf_hooks", {
    performance: perf,
    Performance: perf.constructor,
    PerformanceEntry,
    PerformanceMark,
    PerformanceMeasure,
    PerformanceResourceTiming,
    PerformanceObserver,
    PerformanceObserverEntryList,
    constants,
    createHistogram: () => makeHistogram(),
    monitorEventLoopDelay: () => {
      // lumen exposes no loop-lag signal, so this histogram stays empty; enable/disable/reset are
      // real, the recorded delay is honestly zero.
      const h = makeHistogram();
      return h;
    },
  });
}

// ---- node:querystring -------------------------------------------------------------------------
// Classic (non-percent-strict) parse/stringify. `qs`/body-parser can use `querystring` for simple
// bodies; Express's default query parser is `qs` (its own package), so this is the fallback path.
{
  const qsUnescape = (s) => { try { return decodeURIComponent(s.replace(/\+/g, " ")); } catch { return s; } };
  const qsEscape = (s) => encodeURIComponent(s);

  function parse(str, sep = "&", eq = "=") {
    const obj = Object.create(null);
    if (typeof str !== "string" || str.length === 0) return obj;
    for (const part of str.split(sep)) {
      if (part === "") continue;
      const idx = part.indexOf(eq);
      let k, v;
      if (idx < 0) { k = qsUnescape(part); v = ""; }
      else { k = qsUnescape(part.slice(0, idx)); v = qsUnescape(part.slice(idx + eq.length)); }
      if (k in obj) {
        if (Array.isArray(obj[k])) obj[k].push(v);
        else obj[k] = [obj[k], v];
      } else obj[k] = v;
    }
    return obj;
  }

  function stringify(obj, sep = "&", eq = "=") {
    if (obj === null || typeof obj !== "object") return "";
    const pairs = [];
    for (const k of Object.keys(obj)) {
      const ek = qsEscape(k);
      const v = obj[k];
      if (Array.isArray(v)) for (const item of v) pairs.push(`${ek}${eq}${qsEscape(String(item))}`);
      else pairs.push(`${ek}${eq}${qsEscape(v == null ? "" : String(v))}`);
    }
    return pairs.join(sep);
  }

  // Node's unescapeBuffer: %XX pairs become bytes, '+' becomes a space only when `decodeSpaces`,
  // malformed escapes stay literal, and char codes are written raw (Uint8Array truncates >0xFF).
  const hexVal = (c) =>
    c >= 0x30 && c <= 0x39 ? c - 0x30 : c >= 0x41 && c <= 0x46 ? c - 0x37 : c >= 0x61 && c <= 0x66 ? c - 0x57 : -1;
  function unescapeBuffer(s, decodeSpaces = false) {
    s = String(s);
    const out = Buffer.alloc(s.length);
    let n = 0;
    for (let i = 0; i < s.length; i++) {
      const c = s.charCodeAt(i);
      if (c === 0x2b /* + */ && decodeSpaces) { out[n++] = 0x20; continue; }
      if (c === 0x25 /* % */ && i + 2 < s.length) {
        const hi = hexVal(s.charCodeAt(i + 1));
        const lo = hexVal(s.charCodeAt(i + 2));
        if (hi >= 0 && lo >= 0) { out[n++] = (hi << 4) | lo; i += 2; continue; }
      }
      out[n++] = c;
    }
    return out.slice(0, n);
  }

  __builtins.set("querystring", { parse, stringify, decode: parse, encode: stringify, escape: qsEscape, unescape: qsUnescape, unescapeBuffer });
}

// ---- node:url ---------------------------------------------------------------------------------
// The web `URL`/`URLSearchParams` globals, plus the legacy `url.parse()` that server middleware
// (parseurl, serve-static) calls on request targets like "/path?query" (origin-form, no host).
{
  const querystring = __builtins.get("querystring");

  function legacyParse(urlStr, parseQueryString = false, slashesDenoteHost = false) {
    const url = { protocol: null, slashes: null, auth: null, host: null, port: null, hostname: null, hash: null, search: null, query: null, pathname: null, path: null, href: urlStr };
    let rest = String(urlStr);

    const hashIdx = rest.indexOf("#");
    if (hashIdx >= 0) { url.hash = rest.slice(hashIdx); rest = rest.slice(0, hashIdx); }

    const protoMatch = /^([a-z0-9.+-]+:)/i.exec(rest);
    if (protoMatch) { url.protocol = protoMatch[1].toLowerCase(); rest = rest.slice(protoMatch[1].length); }

    if ((url.protocol && rest.startsWith("//")) || (slashesDenoteHost && rest.startsWith("//"))) {
      url.slashes = true;
      rest = rest.slice(2);
      let hostEnd = rest.length;
      for (const ch of ["/", "?", "#"]) { const i = rest.indexOf(ch); if (i >= 0 && i < hostEnd) hostEnd = i; }
      let host = rest.slice(0, hostEnd);
      rest = rest.slice(hostEnd);
      const at = host.lastIndexOf("@");
      if (at >= 0) { url.auth = host.slice(0, at); host = host.slice(at + 1); }
      url.host = host;
      const colon = host.lastIndexOf(":");
      if (colon >= 0) { url.hostname = host.slice(0, colon); url.port = host.slice(colon + 1); }
      else url.hostname = host;
    }

    const qIdx = rest.indexOf("?");
    if (qIdx >= 0) { url.search = rest.slice(qIdx); url.pathname = rest.slice(0, qIdx); }
    else url.pathname = rest;
    if (url.search) url.query = parseQueryString ? querystring.parse(url.search.slice(1)) : url.search.slice(1);
    else url.query = parseQueryString ? Object.create(null) : null;
    if (url.pathname === "" && url.host) url.pathname = "/";
    url.path = url.pathname + (url.search || "");
    return url;
  }

  function format(urlObj) {
    if (typeof urlObj === "string") return urlObj;
    if (urlObj instanceof URL) return urlObj.href;
    let out = "";
    if (urlObj.protocol) out += urlObj.protocol + (urlObj.slashes || urlObj.host ? "//" : "");
    if (urlObj.auth) out += urlObj.auth + "@";
    if (urlObj.host) out += urlObj.host;
    else if (urlObj.hostname) out += urlObj.hostname + (urlObj.port ? ":" + urlObj.port : "");
    out += urlObj.pathname || "";
    out += urlObj.search || (urlObj.query && typeof urlObj.query === "object" ? "?" + querystring.stringify(urlObj.query) : "") || "";
    out += urlObj.hash || "";
    return out;
  }

  const resolve = (from, to) => new URL(to, new URL(from, "http://localhost")).href;

  // Legacy resolveObject: resolve, then hand back a parse()-shaped object.
  function resolveObject(source, relative) {
    if (!source) return relative;
    return legacyParse(resolve(typeof source === "string" ? source : format(source), relative));
  }

  // What http.request(new URL(...)) uses. Mirrors Node: bracket-free IPv6 hostname, numeric port
  // (only when present), decoded `user:pass` auth.
  function urlToHttpOptions(url) {
    const options = {
      protocol: url.protocol,
      hostname: typeof url.hostname === "string" && url.hostname.startsWith("[") ? url.hostname.slice(1, -1) : url.hostname,
      hash: url.hash,
      search: url.search,
      pathname: url.pathname,
      path: `${url.pathname || ""}${url.search || ""}`,
      href: url.href,
    };
    if (url.port !== "") options.port = Number(url.port);
    if (url.username || url.password) options.auth = `${decodeURIComponent(url.username)}:${decodeURIComponent(url.password)}`;
    return options;
  }

  // Punycode-backed domain converters. Node's use full IDNA/UTS46; lowercasing first covers the
  // mapping step that matters in practice.
  const punycode = __builtins.get("punycode");

  // fileURLToPath / pathToFileURL: Node's lib/internal/url.js algorithms (drive letters, UNC
  // hosts, the percent-encoding of %, \, newlines, tabs, ? and #).
  const isWindowsUrl = __os.info().platform === "win32";
  const { ERR_INVALID_ARG_TYPE, ERR_INVALID_ARG_VALUE, ERR_INVALID_URL_SCHEME, ERR_INVALID_FILE_URL_PATH,
    ERR_INVALID_FILE_URL_HOST } = __errors;
  function isURLObject(self) {
    return Boolean(self?.href && self.protocol && self.auth === undefined && self.path === undefined);
  }
  function getPathFromURLWin32(url) {
    const hostname = url.hostname;
    let pathname = url.pathname;
    for (let n = 0; n < pathname.length; n++) {
      if (pathname[n] === "%") {
        const third = pathname.codePointAt(n + 2) | 0x20;
        if ((pathname[n + 1] === "2" && third === 102) || (pathname[n + 1] === "5" && third === 99)) {
          throw new ERR_INVALID_FILE_URL_PATH("must not include encoded \\ or / characters");
        }
      }
    }
    pathname = pathname.replace(/\//g, "\\");
    pathname = decodeURIComponent(pathname);
    if (hostname !== "") {
      return `\\${punycode.toUnicode(hostname)}${pathname}`;
    }
    const letter = pathname.codePointAt(1) | 0x20;
    const sep = pathname.charAt(2);
    if (letter < 97 || letter > 122 || sep !== ":") {
      throw new ERR_INVALID_FILE_URL_PATH("must be absolute");
    }
    return pathname.slice(1);
  }
  function getPathFromURLPosix(url) {
    if (url.hostname !== "") {
      throw new ERR_INVALID_FILE_URL_HOST(__os.info().platform);
    }
    const pathname = url.pathname;
    for (let n = 0; n < pathname.length; n++) {
      if (pathname[n] === "%") {
        const third = pathname.codePointAt(n + 2) | 0x20;
        if (pathname[n + 1] === "2" && third === 102) {
          throw new ERR_INVALID_FILE_URL_PATH("must not include encoded / characters");
        }
      }
    }
    return decodeURIComponent(pathname);
  }
  function fileURLToPath(path) {
    if (typeof path === "string") path = new URL(path);
    else if (!isURLObject(path)) throw new ERR_INVALID_ARG_TYPE("path", ["string", "URL"], path);
    if (path.protocol !== "file:") throw new ERR_INVALID_URL_SCHEME("file");
    return isWindowsUrl ? getPathFromURLWin32(path) : getPathFromURLPosix(path);
  }
  function encodePathChars(filepath) {
    if (filepath.includes("%")) filepath = filepath.replace(/%/g, "%25");
    if (!isWindowsUrl && filepath.includes("\\")) filepath = filepath.replace(/\\/g, "%5C");
    if (filepath.includes("\n")) filepath = filepath.replace(/\n/g, "%0A");
    if (filepath.includes("\r")) filepath = filepath.replace(/\r/g, "%0D");
    if (filepath.includes("\t")) filepath = filepath.replace(/\t/g, "%09");
    return filepath;
  }
  function pathToFileURL(filepath) {
    const pathMod = __builtins.get("path");
    if (isWindowsUrl && filepath.startsWith("\\\\")) {
      const outURL = new URL("file://");
      const hostnameEndIndex = filepath.indexOf("\\", 2);
      if (hostnameEndIndex === -1) throw new ERR_INVALID_ARG_VALUE("path", filepath, "Missing UNC resource path");
      if (hostnameEndIndex === 2) throw new ERR_INVALID_ARG_VALUE("path", filepath, "Empty UNC servername");
      const hostname = filepath.slice(2, hostnameEndIndex);
      outURL.hostname = punycode.toASCII(hostname.toLowerCase());
      outURL.pathname = encodePathChars(filepath.slice(hostnameEndIndex).replace(/\\/g, "/"));
      return outURL;
    }
    let resolved = pathMod.resolve(filepath);
    const filePathLast = filepath.charCodeAt(filepath.length - 1);
    if ((filePathLast === 47 || (isWindowsUrl && filePathLast === 92)) && resolved[resolved.length - 1] !== pathMod.sep) {
      resolved += "/";
    }
    resolved = encodePathChars(resolved);
    if (resolved.includes("?")) resolved = resolved.replace(/\?/g, "%3F");
    if (resolved.includes("#")) resolved = resolved.replace(/#/g, "%23");
    return new URL(`file://${resolved}`);
  }


  __builtins.set("url", {
    parse: legacyParse,
    format,
    resolve,
    resolveObject,
    urlToHttpOptions,
    URL,
    URLSearchParams,
    Url: function Url() {},
    domainToASCII: (d) => punycode.toASCII(String(d).toLowerCase()),
    domainToUnicode: (d) => punycode.toUnicode(String(d).toLowerCase()),
    fileURLToPath,
    pathToFileURL,
  });
}

// node:net now lives in its own glue file (net.js) — its surface grew past the "small shim" bar
// (BlockList, SocketAddress, auto-select-family flags).

// node:assert lives in assert.js.

// ---- node:string_decoder ----------------------------------------------------------------------
// Streaming decode that never splits a character across chunks: UTF-8 through TextDecoder's
// streaming mode (WHATWG replacement semantics, as Node's decoder), UTF-16LE holding back an odd
// byte or a lone high surrogate, base64/base64url holding back a partial 3-byte group (so each
// chunk encodes on its own, as Node emits it), and the single-byte encodings chunk by chunk.
{
  const { ERR_INVALID_ARG_TYPE, ERR_UNKNOWN_ENCODING } = __errors;
  function normalizeEncoding(enc) {
    const raw = enc === undefined || enc === null ? "utf8" : `${enc}`;
    switch (raw.toLowerCase()) {
      case "": case "utf8": case "utf-8": return "utf8";
      case "ucs2": case "ucs-2": case "utf16le": case "utf-16le": return "utf16le";
      case "latin1": case "binary": return "latin1";
      case "base64": return "base64";
      case "base64url": return "base64url";
      case "hex": return "hex";
      case "ascii": return "ascii";
    }
    throw new ERR_UNKNOWN_ENCODING(enc);
  }
  function toBytes(buf) {
    if (buf instanceof Uint8Array) return buf;
    if (ArrayBuffer.isView(buf)) return new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength);
    throw new ERR_INVALID_ARG_TYPE("buf", ["Buffer", "TypedArray", "DataView"], buf);
  }
  // A function constructor, NOT a class: iconv-lite inherits via `StringDecoder.call(this, enc)`
  // + `Child.prototype = StringDecoder.prototype`, which a class constructor rejects ("cannot be
  // invoked without new").
  function StringDecoder(encoding) {
    this.encoding = normalizeEncoding(encoding);
    this._dec = this.encoding === "utf8" ? new TextDecoder("utf-8") : null;
    this._rest = null; // held-back bytes (utf16le / base64)
  }
  StringDecoder.prototype.write = function (buf) {
    if (typeof buf === "string") return buf;
    const bytes = toBytes(buf);
    switch (this.encoding) {
      case "utf8":
        return this._dec.decode(bytes, { stream: true });
      case "utf16le": {
        let all = this._rest ? Buffer.concat([this._rest, bytes]) : Buffer.from(bytes);
        let keep = all.length % 2;
        // Hold back a high surrogate whose low half has not arrived.
        if (all.length - keep >= 2) {
          const last = all[all.length - keep - 1];
          if (last >= 0xd8 && last <= 0xdb) keep += 2;
        }
        this._rest = keep ? all.subarray(all.length - keep) : null;
        return all.subarray(0, all.length - keep).toString("utf16le");
      }
      case "base64":
      case "base64url": {
        const all = this._rest ? Buffer.concat([this._rest, bytes]) : Buffer.from(bytes);
        const keep = all.length % 3;
        this._rest = keep ? all.subarray(all.length - keep) : null;
        return all.subarray(0, all.length - keep).toString(this.encoding);
      }
      default:
        return Buffer.from(bytes).toString(this.encoding);
    }
  };
  StringDecoder.prototype.end = function (buf) {
    let out = buf === undefined ? "" : this.write(buf);
    if (this._dec) {
      out += this._dec.decode();
    } else if (this._rest) {
      out += this._rest.toString(this.encoding);
      this._rest = null;
    }
    return out;
  };
  StringDecoder.prototype.text = function (buf, offset) {
    this._rest = null;
    if (this._dec) this._dec = new TextDecoder("utf-8");
    return this.write(toBytes(buf).subarray(offset));
  };
  Object.defineProperties(StringDecoder.prototype, {
    lastNeed: { get() { return this._rest ? (this.encoding === "utf16le" ? 2 : 3) - this._rest.length : 0; }, configurable: true },
    lastTotal: { get() { return this._rest ? (this.encoding === "utf16le" ? 2 : 3) : 0; }, configurable: true },
    lastChar: { get() { return Buffer.from(this._rest ?? []); }, configurable: true },
  });
  __builtins.set("string_decoder", { StringDecoder });
}

// ---- node:tty ---------------------------------------------------------------------------------
// We run behind pipes, never a terminal — isatty is always false (debug uses it for colors).
__builtins.set("tty", {
  isatty: () => false,
  ReadStream: function () { throw new Error("node:tty streams are not supported"); },
  WriteStream: function () { throw new Error("node:tty streams are not supported"); },
});

// ---- node:async_hooks -------------------------------------------------------------------------
// AsyncLocalStorage and AsyncResource over the engine's async context (see preamble.js): a
// context is an immutable Map from storage to store, propagated along promise reactions by the
// engine and bound into timers/nextTick by the glue. The async_hooks *hook* API (createHook,
// executionAsyncId) stays a no-op surface: there is no async-id graph to report.
{
  let nextId = 1;
  class AsyncResource {
    constructor(type) {
      this.type = type;
      this._id = nextId++;
      this._context = __asyncContextGet();
    }
    runInAsyncScope(fn, thisArg, ...args) { return __runInAsyncContext(this._context, fn, thisArg, args); }
    bind(fn) {
      const resource = this;
      const bound = function (...args) { return resource.runInAsyncScope(fn, this, ...args); };
      Object.defineProperty(bound, "asyncResource", { value: resource, enumerable: true, configurable: true, writable: true });
      return bound;
    }
    emitDestroy() { return this; }
    asyncId() { return this._id; }
    triggerAsyncId() { return 0; }
    static bind(fn, type, thisArg) {
      const bound = new AsyncResource(type ?? fn.name ?? "bound-anonymous-fn").bind(fn);
      return thisArg === undefined ? bound : bound.bind(thisArg);
    }
  }
  const withStore = (storage, store) => {
    const next = new Map(__asyncContextGet());
    next.set(storage, store);
    return next;
  };
  class AsyncLocalStorage {
    constructor() { this._enabled = true; }
    run(store, cb, ...args) { return __runInAsyncContext(withStore(this, store), cb, undefined, args); }
    getStore() {
      if (!this._enabled) return undefined;
      const context = __asyncContextGet();
      return context === undefined ? undefined : context.get(this);
    }
    enterWith(store) { this._enabled = true; __asyncContextSet(withStore(this, store)); }
    exit(cb, ...args) {
      const next = new Map(__asyncContextGet());
      next.delete(this);
      return __runInAsyncContext(next.size === 0 ? undefined : next, cb, undefined, args);
    }
    disable() { this._enabled = false; }
    static bind(fn) { return AsyncResource.bind(fn); }
    static snapshot() {
      const context = __asyncContextGet();
      return (fn, ...args) => __runInAsyncContext(context, fn, undefined, args);
    }
  }
  __builtins.set("async_hooks", {
    AsyncResource,
    // Node's async_wrap provider table (v22). The ids are static names, not live counters, so
    // exposing the real list keeps feature-detecting consumers working.
    asyncWrapProviders: Object.freeze({
      NONE: 0, DIRHANDLE: 1, DNSCHANNEL: 2, ELDHISTOGRAM: 3, FILEHANDLE: 4, FILEHANDLECLOSEREQ: 5,
      BLOBREADER: 6, FSEVENTWRAP: 7, FSREQCALLBACK: 8, FSREQPROMISE: 9, GETADDRINFOREQWRAP: 10,
      GETNAMEINFOREQWRAP: 11, HEAPSNAPSHOT: 12, HTTP2SESSION: 13, HTTP2STREAM: 14, HTTP2PING: 15,
      HTTP2SETTINGS: 16, HTTPINCOMINGMESSAGE: 17, HTTPCLIENTREQUEST: 18, JSSTREAM: 19, JSUDPWRAP: 20,
      MESSAGEPORT: 21, PIPECONNECTWRAP: 22, PIPESERVERWRAP: 23, PIPEWRAP: 24, PROCESSWRAP: 25,
      PROMISE: 26, QUERYWRAP: 27, QUIC_ENDPOINT: 28, QUIC_LOGSTREAM: 29, QUIC_PACKET: 30,
      QUIC_SESSION: 31, QUIC_STREAM: 32, QUIC_UDP: 33, SHUTDOWNWRAP: 34, SIGNALWRAP: 35,
      STATWATCHER: 36, STREAMPIPE: 37, TCPCONNECTWRAP: 38, TCPSERVERWRAP: 39, TCPWRAP: 40,
      TTYWRAP: 41, UDPSENDWRAP: 42, UDPWRAP: 43, SIGINTWATCHDOG: 44, WORKER: 45,
      WORKERHEAPSNAPSHOT: 46, WORKERHEAPSTATISTICS: 47, WRITEWRAP: 48, ZLIB: 49,
      CHECKPRIMEREQUEST: 50, PBKDF2REQUEST: 51, KEYPAIRGENREQUEST: 52, KEYGENREQUEST: 53,
      KEYEXPORTREQUEST: 54, CIPHERREQUEST: 55, DERIVEBITSREQUEST: 56, HASHREQUEST: 57,
      RANDOMBYTESREQUEST: 58, RANDOMPRIMEREQUEST: 59, SCRYPTREQUEST: 60, SIGNREQUEST: 61,
      TLSWRAP: 62, VERIFYREQUEST: 63,
    }),
    executionAsyncId: () => 0,
    triggerAsyncId: () => 0,
    executionAsyncResource: () => ({}),
    createHook: () => ({ enable() { return this; }, disable() { return this; } }),
    AsyncLocalStorage,
  });
}

// ---- node:zlib --------------------------------------------------------------------------------
// Real gzip/deflate/Brotli/Zstd/crc32 over shared native codec ops: sync, async-callback, and
// Transform-stream forms, plus the full constants/codes tables.
{
  const codecs = {
    gzip: __zlib.gzip, gunzip: __zlib.gunzip,
    deflate: __zlib.deflate, inflate: __zlib.inflate,
    deflateRaw: __zlib.deflateRaw, inflateRaw: __zlib.inflateRaw,
    brotliCompress: __zlib.brotliCompress, brotliDecompress: __zlib.brotliDecompress,
    zstdCompress: __zlib.zstdCompress, zstdDecompress: __zlib.zstdDecompress,
  };

  const toBuf = (input, enc) => (input instanceof Uint8Array ? input : Buffer.from(input, enc));
  const sync = (fn) => (input) => Buffer.from(fn(toBuf(input)));
  const asyncOf = (syncFn) => (input, opts, cb) => {
    if (typeof opts === "function") cb = opts;
    queueMicrotask(() => {
      try {
        cb(null, syncFn(input));
      } catch (e) {
        cb(e);
      }
    });
  };

  const zlib = {};

  const Z_NO_FLUSH = 0;
  const Z_FINISH = 4;

  // A Transform subclass over a codec. Raw deflate/inflate (`streamKind` set) run on a real
  // incremental codec — input may be split anywhere, `flush()` emits a sync-flushed block and
  // the 32K window carries across flushes, which websocket permessage-deflate depends on. The
  // framed codecs (gzip/zlib/brotli/zstd) are one-shot: they buffer input and (de)compress it
  // whole on end. `stream` (node:stream) is resolved lazily on first construction: shims.js loads
  // before stream.js, so the class can't `extends Transform` until the user actually needs it.
  const defineStreamClass = (className, syncFn, streamKind) => {
    Object.defineProperty(zlib, className, {
      enumerable: true,
      configurable: true,
      get() {
        const Transform = __builtins.get("stream").Transform;
        const cls = class extends Transform {
          constructor(options) {
            super(options);
            this._chunks = [];
            this._handle = streamKind ? __zlib.streamOpen(streamKind) : 0;
            this._closed = false;
          }
          _transform(chunk, enc, next) {
            const buf = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk, enc);
            if (!streamKind) {
              this._chunks.push(buf);
              return next();
            }
            if (this._closed) return next(new Error("zlib stream is closed"));
            try {
              const out = __zlib.streamWrite(this._handle, buf);
              next(null, out.length ? Buffer.from(out) : null);
            } catch (e) {
              next(e);
            }
          }
          _flush(done) {
            try {
              if (streamKind) {
                if (streamKind === "deflate" && !this._closed) {
                  const out = __zlib.streamFlush(this._handle, true);
                  if (out.length) this.push(Buffer.from(out));
                }
              } else {
                this.push(syncFn(Buffer.concat(this._chunks)));
              }
              done();
            } catch (e) {
              done(e);
            }
          }
          // zlib.flush([kind], cb): runs once the queued writes have gone through; a deflater
          // emits what it holds as a sync-flushed (or, for Z_FINISH, final) block first.
          flush(kind, cb) {
            if (typeof kind === "function") { cb = kind; kind = undefined; }
            __builtins.get("stream")._afterWrites(this, () => {
              if (streamKind === "deflate" && !this._closed && kind !== Z_NO_FLUSH) {
                try {
                  const out = __zlib.streamFlush(this._handle, kind === Z_FINISH);
                  if (out.length) this.push(Buffer.from(out));
                } catch (e) {
                  this.emit("error", e);
                }
              }
              // The 'data' events for that push have fired by now; the callback follows them.
              if (cb) return Promise.resolve().then(cb);
            });
          }
          reset() {
            if (streamKind && !this._closed) __zlib.streamReset(this._handle);
            this._chunks = [];
          }
          _release() {
            if (this._closed) return;
            this._closed = true;
            if (streamKind) __zlib.streamClose(this._handle);
          }
          close(cb) {
            this._release();
            this.destroy();
            if (cb) queueMicrotask(cb);
          }
          destroy(err) {
            this._release();
            return super.destroy(err);
          }
        };
        // Named like Node's (`Gzip`, not `cls`) and callable without `new` (see __legacyConstructor).
        Object.defineProperty(cls, "name", { value: className });
        Object.defineProperty(zlib, className, {
          value: __legacyConstructor(cls),
          enumerable: true,
          configurable: true,
          writable: true,
        });
        return zlib[className];
      },
    });
  };

  for (const [name, fn] of Object.entries(codecs)) {
    const cap = name[0].toUpperCase() + name.slice(1);
    const syncFn = sync(fn);
    zlib[`${name}Sync`] = syncFn;
    zlib[name] = asyncOf(syncFn);
    // Gzip / Gunzip / Deflate / Inflate / DeflateRaw / InflateRaw (+ their create* factories).
    const streamKind = name === "deflateRaw" ? "deflate" : name === "inflateRaw" ? "inflate" : null;
    defineStreamClass(cap, syncFn, streamKind);
    zlib[`create${cap}`] = (options) => new zlib[cap](options);
  }
  // Aliases Node exposes: `unzip` auto-detects gzip vs. zlib framing — our gunzip covers gzip.
  zlib.unzipSync = zlib.gunzipSync;
  zlib.unzip = zlib.gunzip;
  defineStreamClass("Unzip", zlib.gunzipSync);
  zlib.createUnzip = (options) => new zlib.Unzip(options);

  // Real CRC-32 (Node's zlib.crc32(data[, value])), chainable via the optional seed.
  zlib.crc32 = (data, value = 0) => __zlib.crc32(toBuf(data), value >>> 0);

  // The full constants table Node exposes (copied verbatim from Node v22), and the bidirectional
  // return-code map (`codes[Z_OK] === 0` and `codes[0] === "Z_OK"`).
  zlib.constants = {
    Z_NO_FLUSH: 0, Z_PARTIAL_FLUSH: 1, Z_SYNC_FLUSH: 2, Z_FULL_FLUSH: 3, Z_FINISH: 4, Z_BLOCK: 5,
    Z_OK: 0, Z_STREAM_END: 1, Z_NEED_DICT: 2, Z_ERRNO: -1, Z_STREAM_ERROR: -2, Z_DATA_ERROR: -3,
    Z_MEM_ERROR: -4, Z_BUF_ERROR: -5, Z_VERSION_ERROR: -6, Z_NO_COMPRESSION: 0, Z_BEST_SPEED: 1,
    Z_BEST_COMPRESSION: 9, Z_DEFAULT_COMPRESSION: -1, Z_FILTERED: 1, Z_HUFFMAN_ONLY: 2, Z_RLE: 3,
    Z_FIXED: 4, Z_DEFAULT_STRATEGY: 0, ZLIB_VERNUM: 4865, DEFLATE: 1, INFLATE: 2, GZIP: 3,
    GUNZIP: 4, DEFLATERAW: 5, INFLATERAW: 6, UNZIP: 7, BROTLI_DECODE: 8, BROTLI_ENCODE: 9,
    ZSTD_DECOMPRESS: 11, ZSTD_COMPRESS: 10, Z_MIN_WINDOWBITS: 8, Z_MAX_WINDOWBITS: 15,
    Z_DEFAULT_WINDOWBITS: 15, Z_MIN_CHUNK: 64, Z_MAX_CHUNK: null, Z_DEFAULT_CHUNK: 16384,
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
  zlib.codes = {
    "-6": "Z_VERSION_ERROR", "-5": "Z_BUF_ERROR", "-4": "Z_MEM_ERROR", "-3": "Z_DATA_ERROR",
    "-2": "Z_STREAM_ERROR", "-1": "Z_ERRNO", 0: "Z_OK", 1: "Z_STREAM_END", 2: "Z_NEED_DICT",
    Z_OK: 0, Z_STREAM_END: 1, Z_NEED_DICT: 2, Z_ERRNO: -1, Z_STREAM_ERROR: -2, Z_DATA_ERROR: -3,
    Z_MEM_ERROR: -4, Z_BUF_ERROR: -5, Z_VERSION_ERROR: -6,
  };

  __builtins.set("zlib", zlib);
}
