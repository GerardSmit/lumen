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

  // --- Timeout handles ----------------------------------------------------------------------
  // Node's setTimeout/setInterval return a Timeout object, not an id: `ref()`/`unref()` decide
  // whether the timer keeps the process alive, `refresh()` restarts it, and the object coerces
  // to its numeric id so `clearTimeout(+timer)` and the raw ops still agree.
  const invalidCallback = () => {
    const err = new TypeError('The "callback" argument must be of type function. Received undefined');
    err.code = "ERR_INVALID_ARG_TYPE";
    return err;
  };
  class Timeout {
    constructor(callback, delay, args, repeat) {
      callback = __bindAsyncContext(callback);
      this._onTimeout = callback;
      this._idleTimeout = delay;
      this._timerArgs = args;
      this._repeat = repeat ? delay : null;
      this._destroyed = false;
      this._refed = true;
      this._id = repeat ? rawSetInterval(callback, delay, ...args) : rawSetTimeout(callback, delay, ...args);
    }
    ref() { if (!this._refed) { this._refed = true; timerSetRef(this._id, true); } return this; }
    unref() { if (this._refed) { this._refed = false; timerSetRef(this._id, false); } return this; }
    hasRef() { return this._refed; }
    refresh() {
      if (this._destroyed) return this;
      if (!timerRefresh(this._id)) {
        // Already fired: start it over with the same callback, delay and ref state.
        this._id = this._repeat !== null
          ? rawSetInterval(this._onTimeout, this._idleTimeout, ...this._timerArgs)
          : rawSetTimeout(this._onTimeout, this._idleTimeout, ...this._timerArgs);
        if (!this._refed) timerSetRef(this._id, false);
      }
      return this;
    }
    close() { this._destroyed = true; rawClearTimeout(this._id); return this; }
    [Symbol.toPrimitive]() { return this._id; }
    [Symbol.dispose]() { this.close(); }
  }
  const coerceDelay = (ms) => {
    const n = Number(ms);
    return Number.isFinite(n) && n >= 1 ? n : 1;
  };
  function setTimeout(callback, ms, ...args) {
    if (typeof callback !== "function") throw invalidCallback();
    return new Timeout(callback, coerceDelay(ms), args, false);
  }
  function setInterval(callback, ms, ...args) {
    if (typeof callback !== "function") throw invalidCallback();
    return new Timeout(callback, coerceDelay(ms), args, true);
  }
  function clearTimeout(timer) {
    if (timer == null) return;
    if (timer instanceof Timeout) { timer._destroyed = true; rawClearTimeout(timer._id); return; }
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
    constructor() { this._cleared = false; }
    ref() { return this; }
    unref() { return this; }
    hasRef() { return true; }
  }
  function setImmediate(callback, ...args) {
    if (typeof callback !== "function") throw new TypeError('The "callback" argument must be of type function');
    const handle = new Immediate();
    gSetImmediate(__bindAsyncContext(() => { if (!handle._cleared) callback(...args); }));
    return handle;
  }
  function clearImmediate(handle) {
    if (handle && typeof handle === "object") handle._cleared = true;
  }
  globalThis.setImmediate = setImmediate;
  globalThis.clearImmediate = clearImmediate;

  // --- legacy "unenrolled timer" API (deprecated, still exported) -------------------------------
  // Operates on an object carrying `_onTimeout` and `_idleTimeout` (ms). `active`/`_unrefActive`
  // (re)arm the timer; `enroll`/`unenroll` set/clear its duration.
  function enroll(item, msecs) {
    item._idleTimeout = msecs;
    if (item._idleTimeoutId != null) { gClearTimeout(item._idleTimeoutId); item._idleTimeoutId = null; }
    return item;
  }
  function unenroll(item) {
    if (item && item._idleTimeoutId != null) { gClearTimeout(item._idleTimeoutId); item._idleTimeoutId = null; }
    if (item) item._idleTimeout = -1;
    return item;
  }
  function active(item) {
    if (!item || typeof item._idleTimeout !== "number" || item._idleTimeout < 0) return;
    if (item._idleTimeoutId != null) gClearTimeout(item._idleTimeoutId);
    item._idleTimeoutId = gSetTimeout(() => {
      if (typeof item._onTimeout === "function") item._onTimeout();
    }, item._idleTimeout);
  }
  const _unrefActive = active;

  // --- timers/promises -------------------------------------------------------------------------
  const abortReason = (signal) => {
    if (signal && signal.reason !== undefined) return signal.reason;
    const e = new Error("The operation was aborted");
    e.name = "AbortError";
    e.code = "ABORT_ERR";
    return e;
  };

  function setTimeoutP(delay = 1, value, options = {}) {
    const signal = options.signal;
    return new Promise((resolve, reject) => {
      if (signal && signal.aborted) { reject(abortReason(signal)); return; }
      const onAbort = () => { gClearTimeout(id); reject(abortReason(signal)); };
      const id = gSetTimeout(() => {
        if (signal) signal.removeEventListener("abort", onAbort);
        resolve(value);
      }, delay);
      if (signal) signal.addEventListener("abort", onAbort, { once: true });
    });
  }

  function setImmediateP(value, options = {}) {
    const signal = options.signal;
    return new Promise((resolve, reject) => {
      if (signal && signal.aborted) { reject(abortReason(signal)); return; }
      const handle = setImmediate(() => {
        if (signal) signal.removeEventListener("abort", onAbort);
        resolve(value);
      });
      const onAbort = () => { clearImmediate(handle); reject(abortReason(signal)); };
      if (signal) signal.addEventListener("abort", onAbort, { once: true });
    });
  }

  async function* setIntervalP(delay = 1, value, options = {}) {
    const signal = options.signal;
    if (signal && signal.aborted) throw abortReason(signal);
    while (true) {
      await setTimeoutP(delay, undefined, { signal });
      yield value;
    }
  }

  const scheduler = {
    wait: (delay, options) => setTimeoutP(delay, undefined, options),
    yield: () => setImmediateP(),
  };

  const timersPromises = {
    setTimeout: setTimeoutP,
    setImmediate: setImmediateP,
    setInterval: setIntervalP,
    scheduler,
  };
  __builtins.set("timers/promises", timersPromises);

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
