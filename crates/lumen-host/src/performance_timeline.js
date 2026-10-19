// Shared Performance Timeline facade, factored from node:perf_hooks. Rust owns all
// timeline entries, mark lookup and observer queues. Each factory call owns one traced
// native store in the invoking realm. Node adds its diagnostics through explicit hooks.
(() => {
  const binding = globalThis.__performance_timeline;
  binding.createProvider = function createProvider(config = {}) {
  const node = !!config.node;
  const perf = globalThis.performance;
  const now = () => perf.now();
  const store = binding.create();
  const kSkipThrow = Symbol("skip-throw");
  const inspectCustom = config.inspectCustom || Symbol("inspect");
  const inspect = config.inspect || String;
  const entryInspect = function(depth, options) {
    return `${this.constructor.name} ${inspect(this.toJSON(), options)}`;
  };
  const illegalConstructor = () => new TypeError("Illegal constructor");
  const domString = value => {
    if (typeof value === "symbol") throw new TypeError("Cannot convert a Symbol to a string");
    return String(value);
  };
  const dictionary = value => {
    if (value == null) return {};
    if (typeof value !== "object" && typeof value !== "function") throw new TypeError("Options must be a dictionary");
    return value;
  };
  const timestamp = value => {
    const number = node ? value : +value;
    if (typeof number !== "number" || !Number.isFinite(number) || number < 0) {
      throw config.invalidTimestamp ? config.invalidTimestamp(value) : new TypeError("Invalid performance timestamp");
    }
    return number;
  };
  const timingNames = ["navigationStart", "unloadEventStart", "unloadEventEnd", "redirectStart", "redirectEnd",
    "fetchStart", "domainLookupStart", "domainLookupEnd", "connectStart", "connectEnd", "secureConnectionStart",
    "requestStart", "responseStart", "responseEnd", "domLoading", "domInteractive", "domContentLoadedEventStart",
    "domContentLoadedEventEnd", "domComplete", "loadEventStart", "loadEventEnd"];
  const rejectReserved = name => {
    if (!node && typeof globalThis.document !== "undefined" && timingNames.includes(name))
      throw new DOMException("Reserved navigation timing name", "SyntaxError");
  };
  class PerformanceEntry {
    #name;
    #type;
    #start;
    #duration;
    #detail;
    constructor(skip, name, type, start, duration, detail) {
      if (skip !== kSkipThrow) throw illegalConstructor();
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
    _detail() { return this.#detail; }
    toJSON() {
      // Brand-check before reading overridable public attributes.
      void this.#name;
      return {
        name: this.name,
        entryType: this.entryType,
        startTime: this.startTime,
        duration: this.duration,
      };
    }
    [inspectCustom](depth, options) { return entryInspect.call(this, depth, options); }
  }
  const tag = (Class) => Object.defineProperty(Class.prototype, Symbol.toStringTag, {
    __proto__: null, configurable: true, enumerable: false, writable: false, value: Class.name,
  });
  for (const name of ["name", "entryType", "startTime", "duration"]) {
    Object.defineProperty(PerformanceEntry.prototype, name, { enumerable: true });
  }
  tag(PerformanceEntry);

  class PerformanceResourceTiming extends PerformanceEntry {
    #initiator;
    #end;
    #encoded;
    #decoded;
    #allowed;
    constructor(token, name, initiator, start, end, encoded, decoded, allowed) {
      super(token, name, "resource", start, Math.max(0, end - start), null);
      this.#initiator = initiator;
      this.#end = end;
      this.#encoded = encoded;
      this.#decoded = decoded;
      this.#allowed = allowed;
    }
    get initiatorType() { return this.#initiator; }
    get responseEnd() { return this.#end; }
    get fetchStart() { return this.startTime; }
    get encodedBodySize() { return this.#allowed ? this.#encoded : 0; }
    get decodedBodySize() { return this.#allowed ? this.#decoded : 0; }
    toJSON() {
      return {...super.toJSON(), initiatorType:this.initiatorType, responseEnd:this.responseEnd,
        fetchStart:this.fetchStart, encodedBodySize:this.encodedBodySize, decodedBodySize:this.decodedBodySize};
    }
  }
  tag(PerformanceResourceTiming);

  const cloneDetail = detail => detail === undefined ? null : store.cloneDetail(detail);

  class PerformanceMark extends PerformanceEntry {
    constructor(name, options = undefined) {
      if (arguments.length === 0) throw new TypeError("A mark name is required");
      name = domString(name);
      options = dictionary(options);
      rejectReserved(name);
      const detail = options.detail;
      const raw = options.startTime;
      const startTime = raw === undefined ? now() : timestamp(raw);
      super(kSkipThrow, name, "mark", startTime, 0, cloneDetail(detail));
    }
  }
  Object.defineProperty(PerformanceMark.prototype, "detail", {
    configurable: true, enumerable: true, get() { return this._detail(); }
  });
  PerformanceMark.prototype.toJSON = function toJSON() {
    return { name: this.name, entryType: this.entryType, startTime: this.startTime,
      duration: this.duration, detail: this.detail };
  };
  tag(PerformanceMark);

  class PerformanceMeasure extends PerformanceEntry {
    constructor(skip, name, start, duration, detail) {
      if (skip !== kSkipThrow) throw illegalConstructor();
      super(kSkipThrow, name, "measure", start, duration, detail);
    }
  }
  Object.defineProperty(PerformanceMeasure.prototype, "detail", {
    configurable: true, enumerable: true, get() { return this._detail(); }
  });
  PerformanceMeasure.prototype.toJSON = function toJSON() {
    return { name: this.name, entryType: this.entryType, startTime: this.startTime,
      duration: this.duration, detail: this.detail };
  };
  tag(PerformanceMeasure);

  const supported = Object.freeze(config.supported || ["mark", "measure", "resource"]);
  let flushQueued = false;
  const task = config.task || (callback => setTimeout(callback, 0));
  function queuePending() {
    if (flushQueued) return;
    flushQueued = true;
    task(() => {
      flushQueued = false;
      const pending = store.pending();
      for (const observer of pending) {
        try { observer._dispatch(); }
        catch (error) { setTimeout(() => { throw error; }, 0); }
      }
    });
  }
  function enqueue(entry) {
    if (store.add(entry.name, entry.entryType, entry.startTime, entry, false)) queuePending();
  }
  function record(entry) {
    if (store.add(entry.name, entry.entryType, entry.startTime, entry, true)) queuePending();
    return entry;
  }
  function filterTimeline(name, type) {
    let entries = store.entries(name, type);
    if (config.resources && (type === undefined || type === "resource")) {
      const resources = config.resources();
      entries = entries.concat(name === undefined ? resources : resources.filter(entry => entry.name === name));
      entries.sort((a, b) => a.startTime - b.startTime);
    }
    return entries;
  }
  class PerformanceObserverEntryList {
    #entries;
    constructor(token, entries) {
      if (token !== kSkipThrow) throw illegalConstructor();
      this.#entries = entries;
    }
    getEntries() { return this.#entries.slice(); }
    getEntriesByType(type) {
      if (!arguments.length) throw new TypeError("A type is required");
      type = domString(type);
      return this.#entries.filter(entry => entry.entryType === type);
    }
    getEntriesByName(name, type = undefined) {
      if (!arguments.length) throw new TypeError("A name is required");
      name = domString(name);
      if (type !== undefined) type = domString(type);
      return this.#entries.filter(entry => entry.name === name && (type === undefined || entry.entryType === type));
    }
  }
  tag(PerformanceObserverEntryList);
  class PerformanceObserver {
    #id = 0;
    #mode;
    #types = [];
    #callback;
    constructor(callback) {
      if (typeof callback !== "function") throw new TypeError("Callback must be callable");
      this.#callback = callback;
    }
    observe(options) {
      if (!arguments.length) throw new TypeError("Options are required");
      options = dictionary(options);
      let entryTypes = options.entryTypes;
      let type = options.type;
      const buffered = !!options.buffered;
      if (entryTypes === undefined && type === undefined) throw new TypeError("An entry type is required");
      if (entryTypes !== undefined && type !== undefined) throw new TypeError("Cannot combine type and entryTypes");
      const mode = entryTypes !== undefined ? "multiple" : "single";
      if (this.#mode !== undefined && this.#mode !== mode)
        throw new DOMException("Cannot change observer mode", "InvalidModificationError");
      this.#mode = mode;
      let types;
      if (mode === "multiple") {
        if (entryTypes == null || (typeof entryTypes !== "object" && typeof entryTypes !== "function")
          || typeof entryTypes[Symbol.iterator] !== "function") throw new TypeError("entryTypes must be a sequence");
        types = Array.from(entryTypes, domString).filter(type => supported.includes(type));
      } else {
        type = domString(type);
        types = supported.includes(type) ? [type] : [];
      }
      if (!types.length) return;
      if (config.countObserver && mode === "multiple") for (const old of this.#types) config.countObserver(old, -1);
      if (mode === "multiple") this.#types = [];
      for (const type of types) if (!this.#types.includes(type)) {
        this.#types.push(type);
        if (config.countObserver) config.countObserver(type, 1);
      }
      this.#id = store.observe(this.#id, this, types, mode === "multiple", mode === "single" && buffered);
      if (mode === "single" && buffered && type === "resource" && config.resources) {
        for (const entry of config.resources()) store.buffer(this.#id, entry.name, entry.entryType, entry.startTime, entry);
      }
      // Buffered records live in Rust, and delivery always occurs as a realm-owned task.
      if (mode === "single" && buffered) queuePending();
    }
    disconnect() {
      store.disconnect(this.#id);
      this.#id = 0;
      for (const type of this.#types) if (config.countObserver) config.countObserver(type, -1);
      this.#types = [];
      if (node) this.#mode = undefined;
      // The observer's mode survives disconnect (Performance Timeline's observer type).
    }
    takeRecords() { return store.take(this.#id); }
    static get supportedEntryTypes() { return supported; }
    _dispatch() {
      const entries = this.takeRecords();
      if (!entries.length) return;
      Reflect.apply(this.#callback, this, [new PerformanceObserverEntryList(kSkipThrow, entries), this,
        { droppedEntriesCount: store.dropped(this.#id) }]);
    }
  }
  tag(PerformanceObserver);
  Object.defineProperty(PerformanceObserver, "supportedEntryTypes", { enumerable: true });
  function getMark(mark) {
    if (typeof mark === "number") return timestamp(mark);
    mark = domString(mark);
    if (config.resolveSpecial) {
      const special = config.resolveSpecial(mark);
      if (special !== undefined) return special;
    }
    if (!node && timingNames.includes(mark)) {
      if (typeof globalThis.document === "undefined")
        throw new TypeError("Navigation timestamps require a Window global");
      const timing = perf.timing;
      const value = timing && timing[mark];
      if (!value) throw new DOMException("Navigation timestamp is unavailable", "InvalidAccessError");
      return value - timing.navigationStart;
    }
    const time = store.resolve(mark);
    if (time != null) return time;
    throw config.invalidMark ? config.invalidMark(mark) : new DOMException("The mark does not exist", "SyntaxError");
  }
  function mark(name, options = undefined) {
    if (!arguments.length) throw new TypeError("A mark name is required");
    return record(new PerformanceMark(name, options));
  }
  function measure(name, startOrMeasureOptions = undefined, endMark = undefined) {
    if (!arguments.length) throw new TypeError("A measure name is required");
    name = domString(name);
    let start, end, duration, detail;
    const value = startOrMeasureOptions;
    const options = value !== null && (typeof value === "object" || typeof value === "function") ? value : null;
    if (options) {
      // Dictionary member order is observable when options contain getters.
      detail = options.detail;
      duration = options.duration;
      end = options.end;
      start = options.start;
      if (start !== undefined || end !== undefined || duration !== undefined) {
        if (endMark !== undefined || (start !== undefined && end !== undefined && duration !== undefined)
          || (duration !== undefined && start === undefined && end === undefined)) throw new TypeError("Invalid measure options");
        if (duration !== undefined) duration = timestamp(duration);
      } else {
        if (!node && detail !== undefined) throw new TypeError("Measure detail requires start or end");
        end = endMark;
      }
    } else {
      start = value == null ? undefined : domString(value);
      end = endMark;
    }
    const endTime = end !== undefined ? getMark(end) :
      start !== undefined && duration !== undefined ? getMark(start) + duration : now();
    const startTime = start !== undefined ? getMark(start) : duration !== undefined ? endTime - duration : 0;
    return record(new PerformanceMeasure(kSkipThrow, name, startTime, endTime - startTime, cloneDetail(detail)));
  }
  function clearMarks(name = undefined) { store.clear("mark", name === undefined ? undefined : domString(name)); }
  function clearMeasures(name = undefined) { store.clear("measure", name === undefined ? undefined : domString(name)); }
  const methods = {
    mark, measure, clearMarks, clearMeasures,
    getEntries() { return filterTimeline(); },
    getEntriesByType(type) {
      if (!arguments.length) throw new TypeError("A type is required");
      return filterTimeline(undefined, domString(type));
    },
    getEntriesByName(name, type = undefined) {
      if (!arguments.length) throw new TypeError("A name is required");
      return filterTimeline(domString(name), type === undefined ? undefined : domString(type));
    },
  };
  for (const Class of [PerformanceEntry, PerformanceMeasure, PerformanceObserverEntryList])
    Object.defineProperty(Class, "length", { value: 0, configurable: true });
  for (const Class of [PerformanceEntry, PerformanceMark, PerformanceMeasure, PerformanceObserver, PerformanceObserverEntryList]) {
    for (const name of Object.getOwnPropertyNames(Class.prototype)) {
      if (name !== "constructor" && !name.startsWith("_")) Object.defineProperty(Class.prototype, name, { enumerable: true });
    }
  }
  return { PerformanceEntry, PerformanceMark, PerformanceMeasure, PerformanceResourceTiming,
    PerformanceObserver, PerformanceObserverEntryList,
    kSkipThrow, tag, methods, filterTimeline, enqueue,
    recordResource(name, initiator, start, end, encoded, decoded, allowed) {
      return record(new PerformanceResourceTiming(kSkipThrow, name, initiator, start, end, encoded, decoded, allowed));
    } };
  };
  const api = binding.createProvider();
  for (const name of ["PerformanceEntry", "PerformanceMark", "PerformanceMeasure", "PerformanceResourceTiming", "PerformanceObserver", "PerformanceObserverEntryList"])
    Object.defineProperty(globalThis, name, { value: api[name], writable: true, configurable: true });
  for (const [name, value] of Object.entries(api.methods))
    Object.defineProperty(Performance.prototype, name, { value, writable: true, enumerable: true, configurable: true });
  binding.install_resource_publisher(api.recordResource);
})();
