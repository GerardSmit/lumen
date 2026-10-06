(function () {
  function abortError(message, code) {
    const error = new Error(message || "The operation was aborted");
    error.name = "AbortError";
    if (code) error.code = code;
    return error;
  }
  const abortable = typeof globalThis.AbortController === "function";
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
  const tasks = new Map(), realmController = abortable ? new AbortController() : null;
  const lumen = globalThis.Lumen || {};
  if (!globalThis.Lumen) Object.defineProperty(globalThis, "Lumen", {value: lumen, writable: true, configurable: true});
  if (realmController) lumen.signal = realmController.signal;
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
    if (signal !== undefined && (!signal || typeof signal !== "object" || !("aborted" in signal) || typeof signal.addEventListener !== "function")) throw new TypeError("invalid AbortSignal");
    const lifetime = lumen.signal;
    if ((lifetime && lifetime.aborted) || (signal && signal.aborted)) {
      const task = new Task(0, null, grace); task._settle(lifetime && lifetime.aborted ? lifetime.reason : signal.reason, true); task._end(); return task;
    }
    const placement = __parallelStart(fn, args, options.cpu === undefined ? "any" : options.cpu, options.transfer === undefined ? [] : options.transfer, spawning);
    const task = new Task(placement.id, {core: placement.core, class: placement.class, fallback: placement.fallback}, grace);
    tasks.set(placement.id, task);
    if (lifetime) {
      const lifetimeAbort = () => task.terminate(lifetime.reason);
      lifetime.addEventListener("abort", lifetimeAbort, {once: true});
      task._cleanup = () => lifetime.removeEventListener("abort", lifetimeAbort);
    }
    if (signal) {
      const lifetimeCleanup = task._cleanup || (() => {});
      const abort = () => task.terminate(signal.reason);
      signal.addEventListener("abort", abort, {once: true});
      task._cleanup = () => { lifetimeCleanup(); signal.removeEventListener("abort", abort); };
      if (signal.aborted) abort();
    }
    return task;
  }
  lumen.parallel = {
    run: function (fn, args, options) { return start(fn, args, options, false).result; },
    spawn: function (fn, args, options) { return start(fn, args, options, true); },
    signal: realmController ? realmController.signal : undefined
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
    workerController = abortable ? new AbortController() : null;
    workerPort = new Port(0);
    if (root[2]) workerPort._messages = root[2];
    if (workerController) {
      workerPort.signal = workerController.signal;
      lumen.signal = workerController.signal;
      lumen.parallel.signal = workerController.signal;
    }
    const args = spawning ? [workerPort].concat(root[1]) : root[1];
    Promise.resolve().then(() => root[0](...args)).then(value => __parallelComplete(value, false), error => __parallelComplete(error, true));
  };
  globalThis.__parallelWorkerMessage = value => workerPort._message(value);
  globalThis.__parallelWorkerClose = () => workerPort._end();
  globalThis.__parallelWorkerAbort = reason => { if (workerController) workerController.abort(reason); workerPort._end(); };
  globalThis.__parallelRealmAbort = reason => { if (realmController) realmController.abort(reason); if (workerController) workerController.abort(reason); };
})();
