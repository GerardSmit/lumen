// node:timers and node:timers/promises.
//
// The globals `setTimeout`/`setInterval`/`clearTimeout`/`clearInterval`/`setImmediate` exist (the
// timer op crate). `setTimeout`/`setInterval` return cancellable ids; `setImmediate` is fire-once
// and returns nothing, so here it is wrapped in a small cancellable handle so `clearImmediate`
// can actually cancel it (never a silent no-op), and the globals are replaced with these Node-
// shaped versions. `timers/promises` layers Node's promise/async-iterator forms — with
// `AbortSignal` support — over those same primitives.

{
  const rawSetTimeout = globalThis.setTimeout;
  const rawClearTimeout = globalThis.clearTimeout;
  const rawSetInterval = globalThis.setInterval;
  const rawClearInterval = globalThis.clearInterval;
  const gSetImmediate = globalThis.setImmediate;
  const timerSetRef = globalThis.__timerSetRef;
  const timerRefresh = globalThis.__timerRefresh;
  delete globalThis.__timerSetRef;
  delete globalThis.__timerRefresh;

  // Unref'd immediates only run while something else keeps the loop alive: they wait here until a
  // ref'd immediate or a timer fires.
  const unrefPending = [];
  const flushUnref = () => {
    for (const [handle, run] of unrefPending.splice(0)) if (!handle._cleared) run();
  };
  const scheduleUnrefFlush = () => { if (unrefPending.length) gSetImmediate(flushUnref); };

  // --- Timeout handles ----------------------------------------------------------------------
  // Node's setTimeout/setInterval return a Timeout object, not an id: `ref()`/`unref()` decide
  // whether the timer keeps the process alive, `refresh()` restarts it, and the object coerces
  // to its numeric id so `clearTimeout(+timer)` and the raw ops still agree.
  const describeReceived = (v) => {
    if (v == null) return " Received " + v;
    if (typeof v === "function") return ` Received function ${v.name}`;
    if (typeof v === "object") return v.constructor && v.constructor.name ? ` Received an instance of ${v.constructor.name}` : " Received [Object: null prototype] {}";
    let shown = typeof v === "string" ? `'${v.length > 28 ? v.slice(0, 25) + "..." : v}'` : typeof v === "symbol" ? v.toString() : String(v);
    return ` Received type ${typeof v} (${shown})`;
  };
  const invalidArg = (name, value) => {
    const err = new TypeError(`The "${name}" argument must be of type function.${describeReceived(value)}`);
    err.code = "ERR_INVALID_ARG_TYPE";
    return err;
  };
  const invalidCallback = (value) => invalidArg("callback", value);
  class Timeout {
    constructor(callback, delay, args, repeat) {
      callback = __bindAsyncContext(callback, "Timeout", this, repeat);
      this._onTimeout = callback;
      this._idleTimeout = delay;
      this._timerArgs = args;
      this._repeat = repeat ? delay : null;
      this._destroyed = false;
      this._refed = true;
      this._armed = false;
      // Node calls `timer._onTimeout()`: the callback's `this` is the Timeout (an interval
      // callback commonly stops itself with `clearInterval(this)`). A one-shot timer stays
      // active until its callback returns.
      this._converted = false;
      this._fire = repeat ? (...a) => {
        try {
          return typeof this._onTimeout === "function" ? this._onTimeout(...a) : undefined;
        } finally {
          if (!this._destroyed && this._idleTimeout < 0) this.close();
          scheduleUnrefFlush();
        }
      } : (...a) => {
        const id = this._id;
        try {
          return typeof this._onTimeout === "function" ? this._onTimeout(...a) : undefined;
        } finally {
          scheduleUnrefFlush();
          if (this._id === id && !this._destroyed) {
            if (this._repeat && !this._converted) {
              this._converted = true;
              this._id = rawSetInterval(this._fire, this._repeat, ...this._timerArgs);
              if (!this._refed) timerSetRef(this._id, false);
            } else if (!this._converted) {
              this._setArmed(false);
            }
          }
        }
      };
      this._id = repeat ? rawSetInterval(this._fire, delay, ...args) : rawSetTimeout(this._fire, delay, ...args);
      this._setArmed(true);
    }
    _setArmed(armed) {
      if (this._armed === armed) return;
      this._armed = armed;
      if (this._refed) __activeResources.timeouts += armed ? 1 : -1;
    }
    ref() {
      if (!this._refed) {
        this._refed = true;
        if (this._armed) __activeResources.timeouts++;
        timerSetRef(this._id, true);
      }
      return this;
    }
    unref() {
      if (this._refed) {
        this._refed = false;
        if (this._armed) __activeResources.timeouts--;
        timerSetRef(this._id, false);
      }
      return this;
    }
    hasRef() { return this._refed; }
    refresh() {
      if (this._destroyed) return this;
      this._setArmed(true);
      if (!timerRefresh(this._id)) {
        // Already fired: start it over with the same callback, delay and ref state.
        this._id = this._repeat !== null
          ? rawSetInterval(this._fire, this._idleTimeout, ...this._timerArgs)
          : rawSetTimeout(this._fire, this._idleTimeout, ...this._timerArgs);
        if (!this._refed) timerSetRef(this._id, false);
      }
      return this;
    }
    close() { this._destroyed = true; this._setArmed(false); rawClearTimeout(this._id); __destroyAsyncResource(this); return this; }
    [Symbol.toPrimitive]() { return this._id; }
    [Symbol.dispose]() { this.close(); }
  }
  const coerceDelay = (ms) => {
    const n = Number(ms);
    return Number.isFinite(n) && n >= 1 ? n : 1;
  };
  function setTimeout(callback, ms, ...args) {
    if (typeof callback !== "function") throw invalidCallback(callback);
    return new Timeout(callback, coerceDelay(ms), args, false);
  }
  function setInterval(callback, ms, ...args) {
    if (typeof callback !== "function") throw invalidCallback(callback);
    return new Timeout(callback, coerceDelay(ms), args, true);
  }
  function clearTimeout(timer) {
    if (timer == null) return;
    if (timer instanceof Timeout) { timer._destroyed = true; timer._setArmed(false); rawClearTimeout(timer._id); __destroyAsyncResource(timer); return; }
    const id = typeof timer === "object" ? timer._id : timer;
    if (typeof id === "number" || typeof id === "string") rawClearTimeout(id);
  }
  const clearInterval = clearTimeout;
  globalThis.setTimeout = setTimeout;
  globalThis.setInterval = setInterval;
  globalThis.clearTimeout = clearTimeout;
  globalThis.clearInterval = clearInterval;
  const gSetTimeout = setTimeout;
  const gClearTimeout = clearTimeout;
  const gSetInterval = setInterval;
  const gClearInterval = clearInterval;

  // --- setImmediate / clearImmediate as cancellable handles ------------------------------------
  class Immediate {
    constructor() { this._cleared = false; this._pending = true; this._destroyed = false; this._refed = true; __activeResources.immediates++; }
    _settle() { if (this._pending) { this._pending = false; __activeResources.immediates--; } }
    ref() { this._refed = true; return this; }
    unref() { this._refed = false; return this; }
    hasRef() { return this._refed; }
    [Symbol.dispose]() { clearImmediate(this); }
  }
  function setImmediate(callback, ...args) {
    if (typeof callback !== "function") throw invalidCallback(callback);
    const handle = new Immediate();
    const run = __bindAsyncContext(() => Reflect.apply(callback, handle, args), "Immediate", handle);
    gSetImmediate(() => {
      handle._settle();
      if (handle._cleared) return;
      if (!handle._refed) { unrefPending.push([handle, run]); return; }
      flushUnref();
      run();
    });
    return handle;
  }
  function clearImmediate(handle) {
    if (handle && typeof handle === "object") {
      handle._cleared = true;
      handle._destroyed = true;
      if (handle instanceof Immediate) handle._settle();
      __destroyAsyncResource(handle);
    }
  }
  globalThis.setImmediate = setImmediate;
  globalThis.clearImmediate = clearImmediate;

  // --- legacy "unenrolled timer" API (deprecated, still exported) -------------------------------
  // Operates on an object carrying `_onTimeout` and `_idleTimeout` (ms). `active`/`_unrefActive`
  // (re)arm the timer; `enroll`/`unenroll` set/clear its duration.
  function enroll(item, msecs) {
    if (typeof msecs !== "number") {
      const err = new TypeError(`The "msecs" argument must be of type number.${describeReceived(msecs)}`);
      err.code = "ERR_INVALID_ARG_TYPE";
      throw err;
    }
    if (msecs < 0 || !Number.isFinite(msecs)) {
      const err = new RangeError(`The value of "msecs" is out of range. It must be a non-negative finite number. Received ${msecs}`);
      err.code = "ERR_OUT_OF_RANGE";
      throw err;
    }
    item._idleTimeout = msecs;
    if (item._idleTimeoutId != null) { gClearTimeout(item._idleTimeoutId); item._idleTimeoutId = null; }
    return item;
  }
  function unenroll(item) {
    if (item instanceof Timeout) { clearTimeout(item); item._idleTimeout = -1; return item; }
    if (item && item._idleTimeoutId != null) { gClearTimeout(item._idleTimeoutId); item._idleTimeoutId = null; }
    if (item) item._idleTimeout = -1;
    return item;
  }
  const idleList = {};
  function active(item) {
    if (!item || typeof item._idleTimeout !== "number" || item._idleTimeout < 0) return;
    if (item._idleTimeoutId != null) gClearTimeout(item._idleTimeoutId);
    item._idleStart = Date.now();
    item._idleNext = item._idlePrev = idleList;
    item._idleTimeoutId = gSetTimeout(() => {
      if (typeof item._onTimeout === "function") item._onTimeout();
    }, item._idleTimeout);
  }
  const _unrefActive = active;

  // --- timers/promises -------------------------------------------------------------------------
  class AbortError extends Error {
    constructor(message = "The operation was aborted", options = undefined) {
      super(message, options);
      this.code = "ABORT_ERR";
      this.name = "AbortError";
    }
  }
  const abortError = (signal) => new AbortError(undefined, { cause: signal?.reason });
  const kResist = Symbol.for("nodejs.internal.kResistStopPropagation");

  function validateTimerOptions(options) {
    __validators.validateObject(options, "options");
    if (options.signal !== undefined) __validators.validateAbortSignal(options.signal, "options.signal");
    if (options.ref !== undefined) __validators.validateBoolean(options.ref, "options.ref");
  }

  function setTimeoutP(after, value, options = {}) {
    try {
      if (after !== undefined) __validators.validateNumber(after, "delay");
      validateTimerOptions(options);
    } catch (error) {
      return Promise.reject(error);
    }
    const { signal, ref = true } = options;
    if (signal?.aborted) return Promise.reject(abortError(signal));
    let onAbort;
    let timeout;
    const promise = new Promise((resolve, reject) => {
      timeout = gSetTimeout(() => resolve(value), after);
      if (!ref) timeout.unref();
      if (signal) {
        onAbort = () => {
          gClearTimeout(timeout);
          reject(abortError(signal));
        };
        signal.addEventListener("abort", onAbort, { __proto__: null, once: true, [kResist]: true });
      }
    });
    return signal ? promise.finally(() => signal.removeEventListener("abort", onAbort)) : promise;
  }

  function setImmediateP(value, options = {}) {
    try {
      validateTimerOptions(options);
    } catch (error) {
      return Promise.reject(error);
    }
    const { signal, ref = true } = options;
    if (signal?.aborted) return Promise.reject(abortError(signal));
    let onAbort;
    let immediate;
    const promise = new Promise((resolve, reject) => {
      immediate = setImmediate(() => resolve(value));
      if (!ref) immediate.unref();
      if (signal) {
        onAbort = () => {
          clearImmediate(immediate);
          reject(abortError(signal));
        };
        signal.addEventListener("abort", onAbort, { __proto__: null, once: true, [kResist]: true });
      }
    });
    return signal ? promise.finally(() => signal.removeEventListener("abort", onAbort)) : promise;
  }

  async function* setIntervalP(after, value, options = {}) {
    if (after !== undefined) __validators.validateNumber(after, "delay");
    validateTimerOptions(options);
    const { signal, ref = true } = options;
    if (signal?.aborted) throw abortError(signal);
    let onAbort;
    let interval;
    let notYielded = 0;
    let callback;
    try {
      interval = gSetInterval(() => {
        notYielded++;
        if (callback) {
          callback();
          callback = undefined;
        }
      }, after);
      if (!ref) interval.unref();
      if (signal) {
        onAbort = () => {
          gClearInterval(interval);
          if (callback) {
            callback(Promise.reject(abortError(signal)));
            callback = undefined;
          }
        };
        signal.addEventListener("abort", onAbort, { __proto__: null, once: true, [kResist]: true });
      }
      while (!signal?.aborted) {
        if (notYielded === 0) await new Promise((resolve) => (callback = resolve));
        for (; notYielded > 0; notYielded--) yield value;
      }
      throw abortError(signal);
    } finally {
      gClearInterval(interval);
      signal?.removeEventListener("abort", onAbort);
    }
  }

  const scheduler = {
    wait: (delay, options) => setTimeoutP(delay, undefined, options),
    yield: () => setImmediateP(),
  };

  Object.defineProperty(setIntervalP, "name", { value: "setInterval", configurable: true });
  const timersPromises = {
    setTimeout: setTimeoutP,
    setImmediate: setImmediateP,
    setInterval: setIntervalP,
    scheduler,
  };
  __builtins.set("timers/promises", timersPromises);
  const kCustomPromisified = Symbol.for("nodejs.util.promisify.custom");
  for (const [fn, promisified, name] of [
    [gSetTimeout, setTimeoutP, "setTimeout"],
    [setImmediate, setImmediateP, "setImmediate"],
    [globalThis.setTimeout, setTimeoutP, "setTimeout"],
  ]) {
    Object.defineProperty(promisified, "name", { value: name, configurable: true });
    if (typeof fn === "function") {
      Object.defineProperty(fn, kCustomPromisified, { enumerable: true, get: () => promisified, configurable: true });
    }
  }

  __builtins.set("timers", {
    Timeout,
    Immediate,
    setTimeout: gSetTimeout,
    clearTimeout: gClearTimeout,
    setInterval: gSetInterval,
    clearInterval: gClearInterval,
    setImmediate,
    clearImmediate,
    active,
    _unrefActive,
    enroll,
    unenroll,
    promises: timersPromises,
  });
}
