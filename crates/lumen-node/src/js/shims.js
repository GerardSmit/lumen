// Small node: builtins the Express stack pulls in. Each is the practical subset its consumers
// use, not a full implementation; gaps throw clearly rather than silently misbehaving. Each one is
// built on its first `__builtins.get` (see `__lazyValue`), so touching one does not build the rest.

// ---- node:perf_hooks --------------------------------------------------------------------------
// The web `performance` global, extended with a real mark/measure entry buffer that dispatches to
// PerformanceObserver. lumen's global `performance` has now()/timeOrigin but no user-timing API, so
// we add mark/measure/getEntries here and wire them to observers — marks and measures are real.
// The observer machinery for entry types lumen cannot produce (gc, http, resource…) simply never
// fires; explicit resource timing from Node HTTP clients is recorded below.
__builtins.set("perf_hooks", __lazyValue(() => {
"lumen:run-once";
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
    constructor(name, timing, initiatorType, cacheMode, responseStatus, deliveryType) {
      super(name, "resource", timing.startTime, timing.endTime - timing.startTime);
      this.initiatorType = initiatorType;
      const fields = { workerStart: "finalServiceWorkerStartTime", redirectStart: "redirectStartTime",
        redirectEnd: "redirectEndTime", fetchStart: "postRedirectStartTime",
        requestStart: "finalNetworkRequestStartTime", responseStart: "finalNetworkResponseStartTime",
        responseEnd: "endTime", encodedBodySize: "encodedBodySize", decodedBodySize: "decodedBodySize" };
      for (const [key, source] of Object.entries(fields)) this[key] = timing[source];
      const connection = timing.finalConnectionTimingInfo;
      for (const [key, source] of Object.entries({domainLookupStart:"domainLookupStartTime",
        domainLookupEnd:"domainLookupEndTime", connectStart:"connectionStartTime",
        connectEnd:"connectionEndTime", secureConnectionStart:"secureConnectionStartTime",
        nextHopProtocol:"ALPNNegotiatedProtocol"})) this[key] = connection?.[source];
      this.transferSize = cacheMode === "local" ? 0 : timing.encodedBodySize + 300;
      this.responseStatus = responseStatus;
      this.deliveryType = deliveryType;
    }
    toJSON() {
      const result = super.toJSON();
      for (const key of ["initiatorType","nextHopProtocol","workerStart","redirectStart","redirectEnd",
        "fetchStart","domainLookupStart","domainLookupEnd","connectStart","connectEnd",
        "secureConnectionStart","requestStart","responseStart","responseEnd","transferSize",
        "encodedBodySize","decodedBodySize","deliveryType","responseStatus"]) result[key] = this[key];
      return result;
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

  // Drop the preamble's placeholder getters for these methods before checking for them.
  for (const name of Object.keys(perf)) {
    if (Object.getOwnPropertyDescriptor(perf, name).get) delete perf[name];
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

  // Explicit client measurements only: no fabricated DNS/TLS/network timestamps. Resource
  // retention is bounded independently of observer delivery (default Node buffer size: 250).
  let resourceLimit = 250;
  perf.setResourceTimingBufferSize = function setResourceTimingBufferSize(size) {
    if (!Number.isInteger(size) || size < 0 || size > 4096) throw new RangeError("Resource timing buffer size must be between 0 and 4096");
    resourceLimit = size;
  };
  perf.markResourceTiming = function markResourceTiming(timingInfo, requestedUrl, initiatorType,
    global, cacheMode, bodyInfo, responseStatus, deliveryType = "") {
    if (cacheMode !== "" && cacheMode !== "local") throw new TypeError("cache must be an empty string or 'local'");
    const entry = new PerformanceResourceTiming(requestedUrl, timingInfo, initiatorType,
      cacheMode, responseStatus, deliveryType);
    if (buffer.filter((e) => e.entryType === "resource").length < resourceLimit) buffer.push(entry);
    for (const observer of observers) if (observer._types.has("resource")) observer._deliver(entry);
    return entry;
  };

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

  return {
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
  };
}));

// node:querystring and node:url live in url.js (Node's lib sources over lumen-web's URL).

// node:net now lives in its own glue file (net.js) — its surface grew past the "small shim" bar
// (BlockList, SocketAddress, auto-select-family flags).

// node:assert lives in assert.js.

// ---- node:string_decoder ----------------------------------------------------------------------
// Streaming decode that never splits a character across chunks: UTF-8 through TextDecoder's
// streaming mode (WHATWG replacement semantics, as Node's decoder), UTF-16LE holding back an odd
// byte or a lone high surrogate, base64/base64url holding back a partial 3-byte group (so each
// chunk encodes on its own, as Node emits it), and the single-byte encodings chunk by chunk.
__builtins.set("string_decoder", __lazyValue(() => {
"lumen:run-once";
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
  return { StringDecoder };
}));
