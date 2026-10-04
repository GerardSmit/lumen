(function () {
  function abortError(message, code) {
    const error = new Error(message || "The operation was aborted");
    error.name = "AbortError";
    if (code) error.code = code;
    return error;
  }
  if (typeof globalThis.AbortSignal === "undefined") {
    class AbortSignal {
      constructor() { this.aborted = false; this.reason = undefined; this.onabort = null; this._listeners = []; }
      addEventListener(type, listener, options) {
        if (type === "abort" && typeof listener === "function" && !this._listeners.some(entry => entry.fn === listener)) this._listeners.push({fn: listener, once: !!(options && options.once)});
      }
      removeEventListener(type, listener) { if (type === "abort") this._listeners = this._listeners.filter(entry => entry.fn !== listener); }
      throwIfAborted() { if (this.aborted) throw this.reason; }
      _abort(reason) {
        if (this.aborted) return;
        this.aborted = true; this.reason = reason === undefined ? abortError() : reason;
        const event = {type: "abort", target: this};
        const listeners = this._listeners.slice();
        this._listeners = this._listeners.filter(entry => !entry.once);
        for (const entry of listeners) { try { entry.fn.call(this, event); } catch (_) {} }
        if (typeof this.onabort === "function") { try { this.onabort(event); } catch (_) {} }
      }
      static abort(reason) { const controller = new AbortController(); controller.abort(reason); return controller.signal; }
      static any(signals) {
        const controller = new AbortController(), listeners = [];
        const finish = reason => {
          controller.abort(reason);
          for (let i = 0; i < listeners.length; i++) {
            const pair = listeners[i];
            pair[0].removeEventListener("abort", pair[1]);
          }
        };
        for (const signal of signals) {
          if (signal.aborted) { finish(signal.reason); break; }
          const listener = () => finish(signal.reason);
          signal.addEventListener("abort", listener, {once: true}); listeners.push([signal, listener]);
        }
        return controller.signal;
      }
    }
    globalThis.AbortSignal = AbortSignal;
    if (typeof globalThis.setTimeout === "function") AbortSignal.timeout = function (delay) {
      if (!Number.isFinite(delay) || delay < 0) throw new RangeError("timeout must be finite and nonnegative");
      const controller = new AbortController();
      setTimeout(() => { const error = new Error("The operation timed out"); error.name = "TimeoutError"; controller.abort(error); }, delay);
      return controller.signal;
    };
  }
  if (typeof globalThis.AbortController === "undefined") {
    globalThis.AbortController = class AbortController {
      constructor() { this.signal = new AbortSignal(); }
      abort(reason) { this.signal._abort(reason); }
    };
  }
  class Port {
    constructor(id) { this._id = id; this._messages = []; this._waiters = []; this._ended = false; this.onmessage = null; }
    postMessage(value, options) {
      const transfer = options && options.transfer !== undefined ? options.transfer : [];
      return __parallelPost(this._id, value, transfer);
    }
    receive() {
      if (this._messages.length) { __parallelConsumed(this._id); return Promise.resolve(this._messages.shift()); }
      if (this._ended) return Promise.reject(abortError("port is closed"));
      return new Promise((resolve, reject) => this._waiters.push({resolve: resolve, reject: reject, iterator: false}));
    }
    [Symbol.asyncIterator]() {
      return {next: () => {
        if (this._messages.length) { __parallelConsumed(this._id); return Promise.resolve({value: this._messages.shift(), done: false}); }
        if (this._ended) return Promise.resolve({value: undefined, done: true});
        return new Promise((resolve, reject) => this._waiters.push({resolve: resolve, reject: reject, iterator: true}));
      }};
    }
    close() { __parallelClose(this._id); }
    migrate(fn, args, options) {
      if (this._id !== 0 || this._ended || this._waiters.length || this._migrating) throw new TypeError("migration requires an idle worker port");
      if (args === undefined) args = [];
      if (options === undefined) options = {};
      if (!options || typeof options !== "object") throw new TypeError("migration options must be an object");
      this._migrating = true;
      try { __parallelMigrate(fn, args, options.cpu === undefined ? "any" : options.cpu, options.transfer === undefined ? [] : options.transfer, this._messages); }
      catch (error) { this._migrating = false; throw error; }
      return new Promise(() => {});
    }
    _message(value) {
      if (this._ended) return;
      if (this._waiters.length) {
        __parallelConsumed(this._id);
        const waiter = this._waiters.shift(); waiter.resolve(waiter.iterator ? {value: value, done: false} : value);
      } else if (typeof this.onmessage === "function") __parallelConsumed(this._id);
      else this._messages.push(value);
      if (typeof this.onmessage === "function") this.onmessage({data: value});
    }
    _end() {
      if (this._ended) return;
      this._ended = true;
      while (this._waiters.length) {
        const waiter = this._waiters.shift();
        if (waiter.iterator) waiter.resolve({value: undefined, done: true}); else waiter.reject(abortError("port is closed"));
      }
    }
  }
  const tasks = new Map(), realmController = new AbortController();
  const lumen = globalThis.Lumen || {};
  if (!globalThis.Lumen) Object.defineProperty(globalThis, "Lumen", {value: lumen, writable: true, configurable: true});
  lumen.signal = realmController.signal;
  class Task extends Port {
    constructor(id, placement, grace) {
      super(id); this.placement = placement; this._grace = grace; this._settled = false; this._cleanup = null;
      this.result = new Promise((resolve, reject) => { this._resolve = resolve; this._reject = reject; });
    }
    terminate(reason) {
      if (this._settled) return;
      if (reason === undefined) reason = abortError();
      __parallelCancel(this._id, reason, this._grace);
      this._settle(reason, true);
    }
    _settle(value, failed) {
      if (this._settled) return;
      this._settled = true;
      if (this._cleanup) { this._cleanup(); this._cleanup = null; }
      if (failed) this._reject(value); else this._resolve(value);
    }
  }
  function start(fn, args, options, spawning) {
    if (typeof fn !== "function") throw new TypeError("parallel function must be callable");
    if (args === undefined) args = [];
    if (!Array.isArray(args)) throw new TypeError("parallel args must be an array");
    if (options === undefined) options = {};
    if (!options || typeof options !== "object") throw new TypeError("parallel options must be an object");
    const grace = options.grace === undefined ? 0 : options.grace;
    if (typeof grace !== "number" || !Number.isFinite(grace) || grace < 0) throw new RangeError("grace must be finite and nonnegative");
    const signal = options.signal;
    if (signal !== undefined && (!signal || typeof signal !== "object" || !("aborted" in signal) || (typeof signal.addEventListener !== "function" && !("onabort" in signal)))) throw new TypeError("invalid AbortSignal");
    if (lumen.signal.aborted || (signal && signal.aborted)) {
      const task = new Task(0, null, grace); task._settle(lumen.signal.aborted ? lumen.signal.reason : signal.reason, true); task._end(); return task;
    }
    const placement = __parallelStart(fn, args, options.cpu === undefined ? "any" : options.cpu, options.transfer === undefined ? [] : options.transfer, spawning);
    const task = new Task(placement.id, {core: placement.core, class: placement.class, fallback: placement.fallback}, grace);
    tasks.set(placement.id, task);
    const lifetimeAbort = () => task.terminate(lumen.signal.reason);
    lumen.signal.addEventListener("abort", lifetimeAbort, {once: true});
    task._cleanup = () => lumen.signal.removeEventListener("abort", lifetimeAbort);
    if (signal) {
      const lifetimeCleanup = task._cleanup;
      const abort = () => task.terminate(signal.reason);
      if (typeof signal.addEventListener === "function") {
        signal.addEventListener("abort", abort, {once: true});
        task._cleanup = () => { lifetimeCleanup(); if (signal.removeEventListener) signal.removeEventListener("abort", abort); };
      } else {
        const previous = signal.onabort;
        const handler = event => { if (typeof previous === "function") previous.call(signal, event); abort(); };
        signal.onabort = handler;
        task._cleanup = () => { lifetimeCleanup(); if (signal.onabort === handler) signal.onabort = previous; };
      }
      if (signal.aborted) abort();
    }
    return task;
  }
  lumen.parallel = {
    run: function (fn, args, options) { return start(fn, args, options, false).result; },
    spawn: function (fn, args, options) { return start(fn, args, options, true); },
    signal: realmController.signal
  };
  globalThis.__parallelDispatch = function (id, kind, value) {
    const task = tasks.get(id); if (!task) return;
    if (kind === 0) task._message(value);
    else if (kind === 6) task.placement = value;
    else if (kind === 3) task._settle(value, true);
    else if (kind === 4) task._end();
    else {
      if (kind === 1 || kind === 2) task._settle(value, kind === 2);
      else if (!task._settled) task._settle(abortError("task stopped", "ERR_TASK_CANCELLED"), true);
      task._end(); tasks.delete(id);
    }
  };
  let workerPort, workerController;
  globalThis.__parallelWorkerStart = function (root, spawning) {
    workerController = new AbortController();
    workerPort = new Port(0); workerPort.signal = workerController.signal;
    if (root[2]) workerPort._messages = root[2];
    lumen.signal = workerController.signal;
    lumen.parallel.signal = workerController.signal;
    const args = spawning ? [workerPort].concat(root[1]) : root[1];
    Promise.resolve().then(() => root[0](...args)).then(value => __parallelComplete(value, false), error => __parallelComplete(error, true));
  };
  globalThis.__parallelWorkerMessage = value => workerPort._message(value);
  globalThis.__parallelWorkerClose = () => workerPort._end();
  globalThis.__parallelWorkerAbort = reason => { workerController.abort(reason); workerPort._end(); };
  globalThis.__parallelRealmAbort = reason => { realmController.abort(reason); if (workerController) workerController.abort(reason); };
})();
