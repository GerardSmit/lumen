// node:perf_hooks — the Performance Timeline over lumen's process clock. Marks, measures,
// resource timings, `timerify` and GC notifications are recorded for real; the observer
// machinery, histograms (HdrHistogram's bucketing), event-loop utilization and nodeTiming
// milestones follow Node's lib/perf. Entry types lumen has no source for (http, http2, net, dns)
// are accepted by observers but never fire.
//
// Everything here is built on the first use of node:perf_hooks or of one of the `performance`
// methods that need it (see the placeholders in preamble.js), and the GC log is only armed while
// a 'gc' observer exists.

__builtins.set("perf_hooks", __lazyValue(() => {
"lumen:run-once";
  const {
    ERR_ILLEGAL_CONSTRUCTOR, ERR_INVALID_ARG_TYPE, ERR_INVALID_ARG_VALUE, ERR_MISSING_ARGS,
    ERR_OUT_OF_RANGE,
  } = __errors;
  const { validateFunction, validateInteger, validateNumber, validateObject, validateString } = __validators;

  const kSkipThrow = Symbol("skip-throw");
  const kEmptyObject = Object.freeze({ __proto__: null });
  const inspectCustom = Symbol.for("nodejs.util.inspect.custom");
  const inspect = (value, options) => __builtins.get("util").inspect(value, options);
  const Performance = globalThis.performance.constructor;
  const perf = globalThis.performance;
  const now = () => perf.now();

  const invalidMark = (name) => __nodeError(Error, "ERR_INVALID_PERFORMANCE_MARK", `The "${name}" performance mark has not been set`);
  const invalidTimestamp = (value) => __nodeError(TypeError, "ERR_PERFORMANCE_INVALID_TIMESTAMP", `${value} is not a valid timestamp`);

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

  // ---- entries ----------------------------------------------------------------------------
  const entryInspect = function (depth, options) {
    if (typeof depth === "number" && depth < 0) return this;
    const opts = { ...options, depth: options.depth == null ? null : options.depth - 1 };
    return `${this.constructor.name} ${inspect(this.toJSON(), opts)}`;
  };

  class PerformanceEntry {
    #name;
    #type;
    #start;
    #duration;
    #detail;
    constructor(skip, name, type, start, duration, detail) {
      if (skip !== kSkipThrow) throw new ERR_ILLEGAL_CONSTRUCTOR();
      this.#name = name;
      this.#type = type;
      this.#start = start;
      this.#duration = duration;
      this.#detail = detail;
    }
    get name() { return this.#name; }
    get entryType() { return this.#type; }
    get startTime() { return this.#start; }
    get duration() { return this.#duration; }
    get detail() { return this.#detail; }
    toJSON() {
      return {
        name: this.name,
        entryType: this.entryType,
        startTime: this.startTime,
        duration: this.duration,
        detail: this.detail,
      };
    }
    [inspectCustom](depth, options) { return entryInspect.call(this, depth, options); }
  }
  const tag = (Class) => Object.defineProperty(Class.prototype, Symbol.toStringTag, {
    __proto__: null, configurable: true, enumerable: false, writable: false, value: Class.name,
  });
  for (const name of ["name", "entryType", "startTime", "duration", "detail"]) {
    Object.defineProperty(PerformanceEntry.prototype, name, { enumerable: true });
  }
  tag(PerformanceEntry);

  const cloneDetail = (detail) => (detail != null ? structuredClone(detail) : null);

  class PerformanceMark extends PerformanceEntry {
    constructor(name, options) {
      name = `${name}`;
      if (options != null) validateObject(options, "options");
      const startTime = options?.startTime ?? now();
      validateNumber(startTime, "startTime");
      if (startTime < 0) throw invalidTimestamp(startTime);
      super(kSkipThrow, name, "mark", startTime, 0, cloneDetail(options?.detail));
    }
  }
  tag(PerformanceMark);

  class PerformanceMeasure extends PerformanceEntry {
    constructor(skip, name, start, duration, detail) {
      if (skip !== kSkipThrow) throw new ERR_ILLEGAL_CONSTRUCTOR();
      super(kSkipThrow, name, "measure", start, duration, detail);
    }
  }
  tag(PerformanceMeasure);

  class PerformanceNodeEntry extends PerformanceEntry {
    constructor(name, type, start, duration, detail) {
      super(kSkipThrow, name, type, start, duration, detail);
    }
  }
  Object.defineProperties(PerformanceNodeEntry.prototype, {
    kind: { configurable: true, enumerable: true, get() { return this.detail?.kind; } },
    flags: { configurable: true, enumerable: true, get() { return this.detail?.flags; } },
  });

  // The resource-timing fields a client records (WHATWG fetch's "fetch timing info").
  class PerformanceResourceTiming extends PerformanceEntry {
    #initiatorType;
    #cacheMode;
    #timing;
    #connection;
    #responseStatus;
    #deliveryType;
    constructor(skip, name, initiatorType, timingInfo, cacheMode = "", bodyInfo, responseStatus, deliveryType = "") {
      if (skip !== kSkipThrow) throw new ERR_ILLEGAL_CONSTRUCTOR();
      super(kSkipThrow, name, "resource", timingInfo.startTime, timingInfo.endTime - timingInfo.startTime, undefined);
      this.#initiatorType = initiatorType;
      this.#cacheMode = cacheMode === "local" ? "local" : "";
      this.#timing = timingInfo;
      this.#connection = timingInfo.finalConnectionTimingInfo;
      this.#responseStatus = responseStatus;
      this.#deliveryType = deliveryType;
    }
    get initiatorType() { return this.#initiatorType; }
    get nextHopProtocol() { return this.#connection?.ALPNNegotiatedProtocol; }
    get workerStart() { return this.#timing.finalServiceWorkerStartTime; }
    get redirectStart() { return this.#timing.redirectStartTime; }
    get redirectEnd() { return this.#timing.redirectEndTime; }
    get fetchStart() { return this.#timing.postRedirectStartTime; }
    get domainLookupStart() { return this.#connection?.domainLookupStartTime; }
    get domainLookupEnd() { return this.#connection?.domainLookupEndTime; }
    get connectStart() { return this.#connection?.connectionStartTime; }
    get connectEnd() { return this.#connection?.connectionEndTime; }
    get secureConnectionStart() { return this.#connection?.secureConnectionStartTime; }
    get requestStart() { return this.#timing.finalNetworkRequestStartTime; }
    get responseStart() { return this.#timing.finalNetworkResponseStartTime; }
    get responseEnd() { return this.#timing.endTime; }
    get transferSize() { return this.#cacheMode === "local" ? 0 : this.#timing.encodedBodySize + 300; }
    get encodedBodySize() { return this.#timing.encodedBodySize; }
    get decodedBodySize() { return this.#timing.decodedBodySize; }
    get responseStatus() { return this.#responseStatus; }
    get deliveryType() { return this.#deliveryType; }
    toJSON() {
      return {
        name: this.name,
        entryType: this.entryType,
        startTime: this.startTime,
        duration: this.duration,
        initiatorType: this.initiatorType,
        nextHopProtocol: this.nextHopProtocol,
        workerStart: this.workerStart,
        redirectStart: this.redirectStart,
        redirectEnd: this.redirectEnd,
        fetchStart: this.fetchStart,
        domainLookupStart: this.domainLookupStart,
        domainLookupEnd: this.domainLookupEnd,
        connectStart: this.connectStart,
        connectEnd: this.connectEnd,
        secureConnectionStart: this.secureConnectionStart,
        requestStart: this.requestStart,
        responseStart: this.responseStart,
        responseEnd: this.responseEnd,
        transferSize: this.transferSize,
        encodedBodySize: this.encodedBodySize,
        decodedBodySize: this.decodedBodySize,
      };
    }
  }
  tag(PerformanceResourceTiming);

  // ---- timing milestones and the loop's idle time ------------------------------------------
  const kMilestones = ["nodeStart", "v8Start", "environment", "bootstrapComplete", "loopStart", "loopExit"];
  const timingSnapshot = () => __node.perfTiming();

  class PerformanceNodeTiming extends PerformanceEntry {
    constructor() {
      super(kSkipThrow, "node", "node", 0, 0, undefined);
      const props = {
        name: { enumerable: true, configurable: true, value: "node" },
        entryType: { enumerable: true, configurable: true, value: "node" },
        startTime: { enumerable: true, configurable: true, value: 0 },
        duration: { enumerable: true, configurable: true, get: now },
        idleTime: { enumerable: true, configurable: true, get: () => timingSnapshot()[6] },
      };
      kMilestones.forEach((name, index) => {
        props[name] = { enumerable: true, configurable: true, get: () => timingSnapshot()[index] };
      });
      Object.defineProperties(this, props);
    }
    toJSON() {
      return {
        name: "node",
        entryType: "node",
        startTime: 0,
        duration: this.duration,
        nodeStart: this.nodeStart,
        v8Start: this.v8Start,
        bootstrapComplete: this.bootstrapComplete,
        environment: this.environment,
        loopStart: this.loopStart,
        loopExit: this.loopExit,
        idleTime: this.idleTime,
      };
    }
  }
  tag(PerformanceNodeTiming);
  let nodeTiming;

  function eventLoopUtilization(util1, util2) {
    const snapshot = timingSnapshot();
    const loopStart = snapshot[4];
    if (loopStart <= 0) return { idle: 0, active: 0, utilization: 0 };
    if (util2) {
      const idle = util1.idle - util2.idle;
      const active = util1.active - util2.active;
      return { idle, active, utilization: active / (idle + active) };
    }
    const idle = snapshot[6];
    const active = now() - loopStart - idle;
    if (!util1) return { idle, active, utilization: active / (idle + active) };
    const idleDelta = idle - util1.idle;
    const activeDelta = active - util1.active;
    return { idle: idleDelta, active: activeDelta, utilization: activeDelta / (idleDelta + activeDelta) };
  }

  // ---- observers and the timeline buffers ----------------------------------------------------
  const kSupportedEntryTypes = Object.freeze(["dns", "function", "gc", "http", "http2", "mark", "measure", "net", "resource"]);
  const kBuffer = Symbol("kBuffer");
  const kEntryTypes = Symbol("kEntryTypes");
  const kType = Symbol("kType");
  const kCallback = Symbol("kCallback");
  const kDispatch = Symbol("kDispatch");

  let markBuffer = [];
  let measureBuffer = [];
  let resourceBuffer = [];
  let resourceSecondary = [];
  let resourceLimit = 250;
  let resourceFullPending = false;

  const observers = new Set();
  const observerCounts = new Map();
  const pending = new Set();
  let flushQueued = false;

  const hasObserver = (type) => (observerCounts.get(type) ?? 0) > 0;

  function queuePending() {
    if (flushQueued) return;
    flushQueued = true;
    globalThis.setImmediate(() => {
      flushQueued = false;
      const list = [...pending];
      pending.clear();
      for (const observer of list) observer[kDispatch]();
    });
  }

  function enqueue(entry) {
    const type = entry.entryType;
    for (const observer of observers) {
      if (observer[kEntryTypes].has(type)) {
        observer[kBuffer].push(entry);
        pending.add(observer);
      }
    }
    if (pending.size) queuePending();
  }

  function countObserver(type, delta) {
    const count = (observerCounts.get(type) ?? 0) + delta;
    observerCounts.set(type, count);
    if (type === "gc") {
      if (delta > 0 && count === 1) __node.gcObserve(drainGc);
      else if (count === 0) __node.gcObserve(null);
    }
  }

  function drainGc() {
    const events = __node.gcTake();
    for (let i = 0; i < events.length; i += 3) {
      const detail = {
        kind: constants.NODE_PERFORMANCE_GC_MAJOR,
        flags: events[i + 2] ? constants.NODE_PERFORMANCE_GC_FLAGS_FORCED : constants.NODE_PERFORMANCE_GC_FLAGS_NO,
      };
      enqueue(new PerformanceNodeEntry("gc", "gc", events[i], events[i + 1], detail));
    }
  }

  function filterTimeline(name, type) {
    let entries;
    switch (type) {
      case undefined: entries = [...markBuffer, ...measureBuffer, ...resourceBuffer]; break;
      case "mark": entries = markBuffer.slice(); break;
      case "measure": entries = measureBuffer.slice(); break;
      case "resource": entries = resourceBuffer.slice(); break;
      default: return [];
    }
    if (name !== undefined) entries = entries.filter((entry) => entry.name === name);
    return entries.sort((a, b) => a.startTime - b.startTime);
  }

  class PerformanceObserverEntryList {
    #entries;
    constructor(entries) {
      this.#entries = entries.slice().sort((a, b) => a.startTime - b.startTime);
    }
    getEntries() { return this.#entries.slice(); }
    getEntriesByType(type) {
      if (arguments.length === 0) throw new ERR_MISSING_ARGS("type");
      type = `${type}`;
      return this.#entries.filter((entry) => entry.entryType === type);
    }
    getEntriesByName(name, type) {
      if (arguments.length === 0) throw new ERR_MISSING_ARGS("name");
      name = `${name}`;
      if (type !== undefined) type = `${type}`;
      return this.#entries.filter((entry) => entry.name === name && (type === undefined || entry.entryType === type));
    }
    [inspectCustom](depth, options) {
      if (typeof depth === "number" && depth < 0) return this;
      const opts = { ...options, depth: options.depth == null ? null : options.depth - 1 };
      return `PerformanceObserverEntryList ${inspect(this.#entries, opts)}`;
    }
  }
  tag(PerformanceObserverEntryList);

  class PerformanceObserver {
    constructor(callback) {
      validateFunction(callback, "callback");
      this[kBuffer] = [];
      this[kEntryTypes] = new Set();
      this[kType] = undefined;
      this[kCallback] = callback;
    }
    observe(options = kEmptyObject) {
      validateObject(options, "options");
      const { entryTypes, type, buffered } = options;
      if (entryTypes === undefined && type === undefined) {
        throw new ERR_MISSING_ARGS("options.entryTypes", "options.type");
      }
      if (entryTypes != null && type != null) {
        throw new ERR_INVALID_ARG_VALUE("options.entryTypes", entryTypes,
          "options.entryTypes can not set with options.type together");
      }
      switch (this[kType]) {
        case undefined: break;
        case "single":
          if (entryTypes !== undefined) {
            throw new ERR_INVALID_ARG_VALUE("options.entryTypes", entryTypes,
              "options.entryTypes can not set when type previously used");
          }
          break;
        case "multiple":
          if (type !== undefined) {
            throw new ERR_INVALID_ARG_VALUE("options.type", type,
              "options.type can not be used when entryTypes previously used");
          }
          break;
      }
      if (entryTypes) {
        __validators.validateArray(entryTypes, "options.entryTypes");
        const types = entryTypes.filter((t) => kSupportedEntryTypes.includes(t));
        if (types.length === 0) return;
        this[kType] = "multiple";
        for (const old of this[kEntryTypes]) countObserver(old, -1);
        this[kEntryTypes].clear();
        for (const t of types) {
          if (!this[kEntryTypes].has(t)) {
            this[kEntryTypes].add(t);
            countObserver(t, 1);
          }
        }
        observers.add(this);
        return;
      }
      validateString(type, "options.type");
      this[kType] = "single";
      if (!kSupportedEntryTypes.includes(type)) return;
      if (!this[kEntryTypes].has(type)) {
        this[kEntryTypes].add(type);
        countObserver(type, 1);
      }
      observers.add(this);
      if (buffered) {
        const entries = filterTimeline(undefined, type);
        if (entries.length) {
          this[kBuffer].push(...entries);
          pending.add(this);
          queuePending();
        }
      }
    }
    disconnect() {
      for (const t of this[kEntryTypes]) countObserver(t, -1);
      observers.delete(this);
      pending.delete(this);
      this[kEntryTypes].clear();
      this[kBuffer] = [];
      this[kType] = undefined;
    }
    takeRecords() {
      const records = this[kBuffer];
      this[kBuffer] = [];
      return records;
    }
    static get supportedEntryTypes() { return kSupportedEntryTypes; }
    [kDispatch]() {
      const entries = this.takeRecords();
      if (entries.length === 0) return;
      Reflect.apply(this[kCallback], this, [new PerformanceObserverEntryList(entries), this]);
    }
    [inspectCustom](depth, options) {
      if (typeof depth === "number" && depth < 0) return this;
      const opts = { ...options, depth: options.depth == null ? null : options.depth - 1 };
      return `PerformanceObserver ${inspect({ connected: observers.has(this), pending: pending.has(this), entryTypes: [...this[kEntryTypes]], buffer: this[kBuffer] }, opts)}`;
    }
  }
  tag(PerformanceObserver);

  // ---- user timing -------------------------------------------------------------------------
  const getMark = (mark) => {
    if (typeof mark === "number") {
      if (mark < 0) throw invalidTimestamp(mark);
      return mark;
    }
    mark = `${mark}`;
    const index = kMilestones.indexOf(mark);
    if (index >= 0) return timingSnapshot()[index];
    for (let i = markBuffer.length - 1; i >= 0; i--) {
      if (markBuffer[i].name === mark) return markBuffer[i].startTime;
    }
    throw invalidMark(mark);
  };

  function mark(name, options = kEmptyObject) {
    const entry = new PerformanceMark(name, options);
    enqueue(entry);
    markBuffer.push(entry);
    return entry;
  }

  function measure(name, startOrMeasureOptions, endMark) {
    name = `${name}`;
    let start, end, duration, detail;
    let optionsValid = false;
    startOrMeasureOptions ??= 0;
    if (typeof startOrMeasureOptions === "object") {
      ({ start, end, duration, detail } = startOrMeasureOptions);
      optionsValid = start !== undefined || end !== undefined;
    }
    if (optionsValid) {
      if (endMark !== undefined) throw new ERR_INVALID_ARG_VALUE("endMark", endMark, "must not be specified");
      if (start !== undefined && end !== undefined && duration !== undefined) {
        throw new ERR_INVALID_ARG_VALUE("options", startOrMeasureOptions, "must not have duration with start and end");
      }
    } else {
      start = startOrMeasureOptions;
      end = endMark;
      duration = undefined;
      detail = undefined;
    }
    let endTime;
    if (end !== undefined) endTime = getMark(end);
    else if (start !== undefined && duration !== undefined) endTime = getMark(start) + getMark(duration);
    else endTime = now();
    let startTime;
    if (start !== undefined) startTime = getMark(start);
    else if (duration !== undefined) startTime = endTime - getMark(duration);
    else startTime = 0;
    const entry = new PerformanceMeasure(kSkipThrow, name, startTime, endTime - startTime, cloneDetail(detail));
    enqueue(entry);
    measureBuffer.push(entry);
    return entry;
  }

  function clearMarks(name) {
    if (name !== undefined) {
      name = `${name}`;
      markBuffer = markBuffer.filter((entry) => entry.name !== name);
    } else {
      markBuffer = [];
    }
  }

  function clearMeasures(name) {
    if (name !== undefined) {
      name = `${name}`;
      measureBuffer = measureBuffer.filter((entry) => entry.name !== name);
    } else {
      measureBuffer = [];
    }
  }

  // ---- resource timing -----------------------------------------------------------------------
  function dispatchBufferFull() {
    perf.dispatchEvent(new Event("resourcetimingbufferfull"));
  }

  function fireBufferFull() {
    while (resourceSecondary.length > 0) {
      const excessBefore = resourceSecondary.length;
      dispatchBufferFull();
      const preserve = Math.max(Math.min(resourceLimit - resourceBuffer.length, resourceSecondary.length), 0);
      const excessAfter = resourceSecondary.length - preserve;
      for (let i = 0; i < preserve; i++) resourceBuffer.push(resourceSecondary[i]);
      if (excessBefore <= excessAfter) {
        resourceSecondary = [];
        break;
      }
      resourceSecondary.splice(0, preserve);
    }
    resourceFullPending = false;
  }

  function bufferResourceTiming(entry) {
    if (resourceBuffer.length < resourceLimit && !resourceFullPending) {
      resourceBuffer.push(entry);
      return;
    }
    if (!resourceFullPending) {
      resourceFullPending = true;
      globalThis.setImmediate(fireBufferFull);
    }
    resourceSecondary.push(entry);
  }

  function markResourceTiming(timingInfo, requestedUrl, initiatorType, global, cacheMode, bodyInfo,
    responseStatus, deliveryType = "") {
    const entry = new PerformanceResourceTiming(kSkipThrow, requestedUrl, initiatorType, timingInfo,
      cacheMode, bodyInfo, responseStatus, deliveryType);
    enqueue(entry);
    bufferResourceTiming(entry);
    return entry;
  }

  function setResourceTimingBufferSize(maxSize) {
    resourceLimit = typeof maxSize === "number" && Number.isInteger(maxSize) && maxSize >= 0 && maxSize <= 0xffffffff
      ? maxSize : 0;
  }

  function clearResourceTimings() {
    resourceBuffer = [];
  }

  // ---- timerify --------------------------------------------------------------------------------
  function processComplete(name, start, args, histogram) {
    const duration = now() - start;
    if (histogram !== undefined) histogram.record(Math.ceil(duration * 1e6));
    if (hasObserver("function")) {
      const entry = new PerformanceNodeEntry(name, "function", start, duration, args);
      for (let i = 0; i < args.length; i++) entry[i] = args[i];
      enqueue(entry);
    }
  }

  function timerify(fn, options = kEmptyObject) {
    validateFunction(fn, "fn");
    validateObject(options, "options");
    const { histogram } = options;
    if (histogram !== undefined && (typeof histogram?.record !== "function" || typeof histogram?.recordDelta !== "function")) {
      throw new ERR_INVALID_ARG_TYPE("options.histogram", "RecordableHistogram", histogram);
    }
    function timerified(...args) {
      const isConstructorCall = new.target !== undefined;
      const start = now();
      const result = isConstructorCall ? Reflect.construct(fn, args, fn) : Reflect.apply(fn, this, args);
      if (!isConstructorCall && typeof result?.finally === "function") {
        return result.finally(() => processComplete(fn.name, start, args, histogram));
      }
      processComplete(fn.name, start, args, histogram);
      return result;
    }
    Object.defineProperties(timerified, {
      length: { __proto__: null, configurable: true, enumerable: false, value: fn.length },
      name: { __proto__: null, configurable: true, enumerable: false, value: `timerified ${fn.name}` },
    });
    return timerified;
  }

  // ---- histograms (HdrHistogram bucketing) --------------------------------------------------------
  const bitLength = (n) => {
    if (n === 0) return 0;
    if (n <= 0xffffffff) return 32 - Math.clz32(n);
    return 64 - Math.clz32(Math.floor(n / 4294967296));
  };

  class HdrCounts {
    constructor(lowest, highest, figures) {
      this.lowest = lowest;
      this.highest = highest;
      this.figures = figures;
      const largest = 2 * 10 ** figures;
      const magnitude = Math.ceil(Math.log2(largest));
      this.half = Math.max(magnitude, 1) - 1;
      this.unit = Math.floor(Math.log2(lowest));
      this.subBucketCount = 2 ** (this.half + 1);
      this.buckets = new Map();
      this.total = 0;
      this.min = Number.MAX_SAFE_INTEGER;
      this.minSet = false;
      this.max = 0;
      this.exceeds = 0;
    }
    bucketShift(value) {
      const index = Math.max(bitLength(value), this.unit + this.half + 1) - this.unit - (this.half + 1);
      return index + this.unit;
    }
    lowestEquivalent(value) {
      const size = 2 ** this.bucketShift(value);
      return Math.floor(value / size) * size;
    }
    rangeSize(value) { return 2 ** this.bucketShift(value); }
    highestEquivalent(value) { return this.lowestEquivalent(value) + this.rangeSize(value) - 1; }
    medianEquivalent(value) { return this.lowestEquivalent(value) + Math.floor(this.rangeSize(value) / 2); }
    record(value, count = 1) {
      if (value < 0 || value > this.highest) {
        this.exceeds += count;
        return false;
      }
      const key = this.lowestEquivalent(value);
      this.buckets.set(key, (this.buckets.get(key) ?? 0) + count);
      this.total += count;
      if (!this.minSet || value < this.min) { this.min = value; this.minSet = true; }
      if (value > this.max) this.max = value;
      return true;
    }
    reset() {
      this.buckets.clear();
      this.total = 0;
      this.min = Number.MAX_SAFE_INTEGER;
      this.minSet = false;
      this.max = 0;
      this.exceeds = 0;
    }
    sorted() { return [...this.buckets].sort((a, b) => a[0] - b[0]); }
    minValue() { return this.minSet ? this.lowestEquivalent(this.min) : 9223372036854775807n; }
    maxValue() { return this.max === 0 ? 0 : this.highestEquivalent(this.max); }
    mean() {
      if (this.total === 0) return NaN;
      let sum = 0;
      for (const [value, count] of this.buckets) sum += count * this.medianEquivalent(value);
      return sum / this.total;
    }
    stddev() {
      if (this.total === 0) return NaN;
      const mean = this.mean();
      let sum = 0;
      for (const [value, count] of this.buckets) sum += count * (this.medianEquivalent(value) - mean) ** 2;
      return Math.sqrt(sum / this.total);
    }
    valueAtPercentile(percentile) {
      const requested = Math.min(percentile, 100);
      const target = Math.max(1, Math.floor((requested / 100) * this.total + 0.5));
      let seen = 0;
      for (const [value, count] of this.sorted()) {
        seen += count;
        if (seen >= target) return this.highestEquivalent(value);
      }
      return 0;
    }
    percentiles() {
      const result = new Map();
      if (this.total === 0) return result.set(100, 0);
      let next = 0;
      let seen = 0;
      let last = 0;
      for (const [value, count] of this.sorted()) {
        seen += count;
        const reached = (100 * seen) / this.total;
        last = this.highestEquivalent(value);
        while (next <= reached) {
          result.set(next, last);
          if (next >= 100) { next = Infinity; break; }
          const half = 2 ** (Math.trunc(Math.log2(100 / (100 - next))) + 1);
          next += 100 / half;
        }
      }
      result.set(100, last);
      return result;
    }
    add(other) {
      let dropped = 0;
      for (const [value, count] of other.buckets) {
        if (!this.record(value, count)) dropped += count;
      }
      this.exceeds += other.exceeds;
      return dropped;
    }
    snapshot() {
      return { lowest: this.lowest, highest: this.highest, figures: this.figures, buckets: [...this.buckets],
        min: this.min, minSet: this.minSet, max: this.max, exceeds: this.exceeds };
    }
  }

  const kHandle = Symbol("kHandle");
  const kCreate = Symbol("kCreate");
  const histogramRegistry = new Map();
  let nextHistogramId = 1;

  const big = (n) => BigInt(n);
  const normalizeValue = (value) => (typeof value === "bigint" ? Number(value) : value);

  class Histogram {
    constructor(token, handle) {
      if (token !== kCreate) throw new ERR_ILLEGAL_CONSTRUCTOR();
      this[kHandle] = handle;
    }
    get count() { return this[kHandle].total; }
    get countBigInt() { return big(this[kHandle].total); }
    get min() { return Number(this[kHandle].minValue()); }
    get minBigInt() { return big(this[kHandle].minValue()); }
    get max() { return this[kHandle].maxValue(); }
    get maxBigInt() { return big(this[kHandle].maxValue()); }
    get mean() { return this[kHandle].mean(); }
    get exceeds() { return this[kHandle].exceeds; }
    get exceedsBigInt() { return big(this[kHandle].exceeds); }
    get stddev() { return this[kHandle].stddev(); }
    percentile(percentile) {
      validateNumber(percentile, "percentile");
      if (Number.isNaN(percentile) || percentile <= 0 || percentile > 100) {
        throw new ERR_OUT_OF_RANGE("percentile", "> 0 && <= 100", percentile);
      }
      return this[kHandle].valueAtPercentile(percentile);
    }
    percentileBigInt(percentile) { return big(this.percentile(percentile)); }
    get percentiles() { return this[kHandle].percentiles(); }
    get percentilesBigInt() {
      const result = new Map();
      for (const [key, value] of this[kHandle].percentiles()) result.set(key, big(value));
      return result;
    }
    reset() { this[kHandle].reset(); }
    [inspectCustom](depth, options) {
      if (typeof depth === "number" && depth < 0) return this;
      const opts = { ...options, depth: options.depth == null ? null : options.depth - 1 };
      return `Histogram ${inspect({
        min: this.min, max: this.max, mean: this.mean, exceeds: this.exceeds, stddev: this.stddev,
        count: this.count, percentiles: this.percentiles,
      }, opts)}`;
    }
    [Symbol.for("lumen.transferable.clone")]() {
      const handle = this[kHandle];
      if (handle.id === undefined) {
        handle.id = nextHistogramId++;
        histogramRegistry.set(handle.id, new WeakRef(handle));
      }
      return {
        data: { id: handle.id, state: handle.snapshot() },
        deserializeInfo: `internal/histogram:${this instanceof RecordableHistogram ? "InternalRecordableHistogram" : "InternalHistogram"}`,
      };
    }
  }
  tag(Histogram);

  class RecordableHistogram extends Histogram {
    constructor(token, handle) {
      if (token !== kCreate) throw new ERR_ILLEGAL_CONSTRUCTOR();
      super(token, handle);
      this.previousDelta = 0;
    }
    record(value) {
      if (typeof value !== "bigint") validateInteger(value, "val", 1);
      else if (value < 1n) throw new ERR_OUT_OF_RANGE("val", ">= 1", value);
      this[kHandle].record(normalizeValue(value));
    }
    recordDelta() {
      const time = Number(process.hrtime.bigint());
      if (this.previousDelta > 0) this[kHandle].record(Math.max(1, time - this.previousDelta));
      this.previousDelta = time;
    }
    add(other) {
      if (!(other instanceof Histogram)) throw new ERR_INVALID_ARG_TYPE("other", "Histogram", other);
      this[kHandle].add(other[kHandle]);
    }
    [Symbol.for("lumen.transferable.clone")]() { return super[Symbol.for("lumen.transferable.clone")](); }
  }
  Object.defineProperty(RecordableHistogram.prototype, "previousDelta", { enumerable: false, writable: true, value: 0 });
  tag(RecordableHistogram);

  function newHandle(lowest = 1, highest = Number.MAX_SAFE_INTEGER, figures = 3) {
    return new HdrCounts(lowest, highest, figures);
  }

  function createHistogram(options = kEmptyObject) {
    validateObject(options, "options");
    const { lowest = 1, highest = Number.MAX_SAFE_INTEGER, figures = 3 } = options;
    if (typeof lowest !== "bigint") validateInteger(lowest, "options.lowest", 1, Number.MAX_SAFE_INTEGER);
    if (typeof highest !== "bigint") validateInteger(highest, "options.highest", 2 * normalizeValue(lowest), Number.MAX_SAFE_INTEGER);
    validateInteger(figures, "options.figures", 1, 5);
    return new RecordableHistogram(kCreate, newHandle(normalizeValue(lowest), normalizeValue(highest), figures));
  }

  // A clone shares the original's counts when it lands on the thread that owns them; on another
  // thread it is a copy of the counts as they were sent.
  const reviveHistogram = (Class) => (data) => {
    let handle = histogramRegistry.get(data.id)?.deref();
    if (handle === undefined) {
      const { lowest, highest, figures, buckets, min, minSet, max, exceeds } = data.state;
      handle = newHandle(lowest, highest, figures);
      for (const [value, count] of buckets) handle.record(value, count);
      Object.assign(handle, { min, minSet, max, exceeds });
    }
    return new Class(kCreate, handle);
  };
  class InternalHistogram extends Histogram {}
  class InternalRecordableHistogram extends RecordableHistogram {}
  __internals.set("cloneModule:internal/histogram", (_id, name) => reviveHistogram(
    name === "InternalRecordableHistogram" ? InternalRecordableHistogram : InternalHistogram));

  class ELDHistogram extends Histogram {
    #timer = null;
    #resolution;
    constructor(resolution) {
      super(kCreate, newHandle(1, 3600 * 1e9, 3));
      this.#resolution = resolution;
    }
    enable() {
      if (this.#timer !== null) return false;
      let last = process.hrtime.bigint();
      this.#timer = setInterval(() => {
        const time = process.hrtime.bigint();
        this[kHandle].record(Number(time - last));
        last = time;
      }, this.#resolution);
      this.#timer.unref();
      return true;
    }
    disable() {
      if (this.#timer === null) return false;
      clearInterval(this.#timer);
      this.#timer = null;
      return true;
    }
  }
  tag(ELDHistogram);
  Object.defineProperty(ELDHistogram, "name", { value: "ELDHistogram" });

  function monitorEventLoopDelay(options = kEmptyObject) {
    validateObject(options, "options");
    const { resolution = 10 } = options;
    validateInteger(resolution, "options.resolution", 1);
    return new ELDHistogram(resolution);
  }

  // ---- the `performance` object ---------------------------------------------------------------
  for (const name of Object.keys(perf)) {
    if (Object.getOwnPropertyDescriptor(perf, name).get) delete perf[name];
  }
  if (!(Performance.prototype instanceof EventTarget)) {
    Object.setPrototypeOf(Performance.prototype, EventTarget.prototype);
    Object.defineProperty(perf, "_listeners", { value: new Map(), writable: true, configurable: true });
  }

  const method = (fn, name = fn.name) => Object.defineProperty(fn, "name", { value: name, configurable: true });
  const define = (target, table) => {
    for (const [name, value] of Object.entries(table)) {
      Object.defineProperty(target, name, { configurable: true, enumerable: true, writable: true, value });
    }
  };
  define(Performance.prototype, {
    clearMarks: method(clearMarks),
    clearMeasures: method(clearMeasures),
    clearResourceTimings: method(clearResourceTimings),
    eventLoopUtilization: method(eventLoopUtilization),
    getEntries: method(function getEntries() { return filterTimeline(); }),
    getEntriesByName: method(function getEntriesByName(name) {
      if (arguments.length === 0) throw new ERR_MISSING_ARGS("name");
      return filterTimeline(`${name}`, arguments.length > 1 && arguments[1] !== undefined ? `${arguments[1]}` : undefined);
    }),
    getEntriesByType: method(function getEntriesByType(type) {
      if (arguments.length === 0) throw new ERR_MISSING_ARGS("type");
      return filterTimeline(undefined, `${type}`);
    }),
    mark: method(mark),
    measure: method(measure),
    markResourceTiming: method(markResourceTiming),
    setResourceTimingBufferSize: method(setResourceTimingBufferSize),
    timerify: method(timerify),
    toJSON: method(function toJSON() {
      return { nodeTiming: this.nodeTiming, timeOrigin: this.timeOrigin, eventLoopUtilization: eventLoopUtilization() };
    }),
  });
  Object.defineProperty(Performance.prototype, "nodeTiming", {
    configurable: true,
    enumerable: true,
    get() { return nodeTiming ??= new PerformanceNodeTiming(); },
  });

  return {
    performance: perf,
    Performance,
    PerformanceEntry,
    PerformanceMark,
    PerformanceMeasure,
    PerformanceObserver,
    PerformanceObserverEntryList,
    PerformanceResourceTiming,
    monitorEventLoopDelay,
    createHistogram,
    constants,
  };
}));

globalThis.PerformanceEntry = __builtins.get("perf_hooks").PerformanceEntry;
globalThis.PerformanceMark = __builtins.get("perf_hooks").PerformanceMark;
globalThis.PerformanceMeasure = __builtins.get("perf_hooks").PerformanceMeasure;
globalThis.PerformanceObserver = __builtins.get("perf_hooks").PerformanceObserver;
globalThis.PerformanceObserverEntryList = __builtins.get("perf_hooks").PerformanceObserverEntryList;
globalThis.PerformanceResourceTiming = __builtins.get("perf_hooks").PerformanceResourceTiming;
