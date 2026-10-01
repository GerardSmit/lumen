// node:async_hooks — async ids, the hook API, v8.promiseHooks and AsyncLocalStorage/AsyncResource.
// Ids and the execution stack live in preamble.js (`__asyncTracking` turns them on when this file
// loads). Promises take part through the engine's promise hooks, installed only while a hook is
// enabled (or a v8.promiseHooks callback is registered). AsyncLocalStorage keys its stores in the
// engine's async context (an immutable Map the engine carries along promise reactions).

const {
  ERR_ASYNC_CALLBACK, ERR_ASYNC_TYPE, ERR_INVALID_ASYNC_ID, ERR_INVALID_ARG_TYPE,
} = __errors;
const { validateString, validateFunction } = __validators;

__asyncTracking = true;
const topLevelResource = {};

function executionAsyncId() {
  return __executionAsyncId();
}
function triggerAsyncId() {
  const n = __asyncTriggers.length;
  return n === 0 ? 0 : __asyncTriggers[n - 1];
}
function executionAsyncResource() {
  const n = __asyncResources.length;
  return n === 0 ? topLevelResource : __asyncResources[n - 1];
}
const newAsyncId = () => ++__asyncIdCounter;

// Node's async_wrap provider table (v22). The ids are static names, not live counters.
const asyncWrapProviders = Object.freeze({
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
});

// ---- v8.promiseHooks ---------------------------------------------------------------------------
// Each hook kind keeps a list; the engine gets one dispatcher per non-empty list. A throwing hook
// does not stop the others: its error is raised as an uncaught exception from a tick.
const promiseHookLists = { init: [], before: [], after: [], settled: [] };

function promiseHookDispatcher(list) {
  if (list.length === 0) return undefined;
  return function promiseHook(promise, parent) {
    for (const hook of list.slice()) {
      try {
        hook(promise, parent);
      } catch (error) {
        process.nextTick(() => {
          throw error;
        });
      }
    }
  };
}
function updatePromiseHooks() {
  __node.setPromiseHooks(
    promiseHookDispatcher(promiseHookLists.init),
    promiseHookDispatcher(promiseHookLists.before),
    promiseHookDispatcher(promiseHookLists.after),
    promiseHookDispatcher(promiseHookLists.settled),
  );
}
const isPlainFunction = (value) => {
  if (typeof value !== "function") return false;
  const tag = Object.prototype.toString.call(value);
  return tag !== "[object AsyncFunction]" && tag !== "[object AsyncGeneratorFunction]";
};
function usePromiseHook(name, hook) {
  if (!isPlainFunction(hook)) throw new ERR_INVALID_ARG_TYPE(`${name}Hook`, "function", hook);
  const list = promiseHookLists[name];
  list.push(hook);
  updatePromiseHooks();
  return function stop() {
    const index = list.indexOf(hook);
    if (index >= 0) {
      list.splice(index, 1);
      updatePromiseHooks();
    }
  };
}
const promiseHooks = {
  onInit: (hook) => usePromiseHook("init", hook),
  onBefore: (hook) => usePromiseHook("before", hook),
  onAfter: (hook) => usePromiseHook("after", hook),
  onSettled: (hook) => usePromiseHook("settled", hook),
  createHook({ init, before, after, settled } = {}) {
    const stops = [];
    if (init) stops.push(usePromiseHook("init", init));
    if (before) stops.push(usePromiseHook("before", before));
    if (after) stops.push(usePromiseHook("after", after));
    if (settled) stops.push(usePromiseHook("settled", settled));
    return () => {
      for (const stop of stops) stop();
    };
  },
};
__internals.set("promiseHooks", promiseHooks);

// ---- hooks ---------------------------------------------------------------------------------
let active = [];
const has = (name) => active.some((hook) => hook[name] !== undefined);

// An exception in a hook callback is fatal (Node's async_hooks fatalError).
function fatalError(error) {
  let stack;
  if (typeof error?.stack === "string") {
    stack = error.stack;
  } else {
    const o = { message: error };
    Error.captureStackTrace(o, fatalError);
    stack = typeof o.stack === "string" && !o.stack.startsWith("Error: [object")
      ? o.stack
      : `Error: ${String(error)}`;
  }
  process._rawDebug(stack);
  if (process.execArgv.includes("--abort-on-uncaught-exception")) process.abort();
  process.exit(1);
}
function emit(name, ...args) {
  for (const hook of active) {
    const fn = hook[name];
    if (fn === undefined) continue;
    try {
      Reflect.apply(fn, hook, args);
    } catch (error) {
      fatalError(error);
    }
  }
}
const emitInit = (id, type, trigger, resource) => emit("init", id, type, trigger, resource);
const emitBefore = (id) => emit("before", id);
const emitAfter = (id) => emit("after", id);
function emitDestroy(id) {
  if (!has("destroy")) return;
  __internals.get("rawQueueMicrotask")(() => emit("destroy", id));
}

// What preamble.js calls while any hook is enabled (`__asyncHooks`).
const hookRuntime = {
  init: emitInit,
  before: emitBefore,
  after: emitAfter,
  destroy: emitDestroy,
};
__internals.set("asyncHookRuntime", hookRuntime);

// Promises: ids are assigned on first sight (init, or a hook seeing a promise made earlier).
const promiseIds = new WeakMap();
function trackPromise(promise, parent) {
  let info = promiseIds.get(promise);
  if (info === undefined) {
    const trigger = parent === undefined ? __executionAsyncId() : trackPromise(parent, undefined).id;
    info = { id: newAsyncId(), trigger };
    promiseIds.set(promise, info);
  }
  return info;
}
const promiseDestroyRegistry = new FinalizationRegistry((id) => emitDestroy(id));
function promiseInitHook(promise, parent) {
  const info = trackPromise(promise, parent);
  if (has("init")) emitInit(info.id, "PROMISE", info.trigger, promise);
  if (has("destroy")) promiseDestroyRegistry.register(promise, info.id);
}
function promiseBeforeHook(promise) {
  const info = trackPromise(promise, undefined);
  __asyncIds.push(info.id);
  __asyncTriggers.push(info.trigger);
  __asyncResources.push(promise);
  emitBefore(info.id);
}
function promiseAfterHook(promise) {
  const info = trackPromise(promise, undefined);
  emitAfter(info.id);
  // Not on the stack when the hooks were enabled during the reaction.
  const at = __asyncIds.length - 1;
  if (at >= 0 && __asyncResources[at] === promise) {
    __asyncIds.length = at;
    __asyncTriggers.length = at;
    __asyncResources.length = at;
  }
}
function promiseResolveHook(promise) {
  const info = trackPromise(promise, undefined);
  emit("promiseResolve", info.id);
}

let stopAsyncPromiseHooks = null;
function stopPromiseHooksIfUnused() {
  if (active.length === 0 && stopAsyncPromiseHooks !== null) {
    stopAsyncPromiseHooks();
    stopAsyncPromiseHooks = null;
  }
}
function syncHooks() {
  __setAsyncHooks(active.length === 0 ? null : hookRuntime);
  if (active.length === 0) {
    // Later: a promise reaction running now still gets its `after`, which pops its frame.
    __internals.get("rawQueueMicrotask")(stopPromiseHooksIfUnused);
    return;
  }
  if (stopAsyncPromiseHooks !== null) stopAsyncPromiseHooks();
  stopAsyncPromiseHooks = promiseHooks.createHook({
    init: has("init") || has("destroy") ? promiseInitHook : undefined,
    before: promiseBeforeHook,
    after: promiseAfterHook,
    settled: has("promiseResolve") ? promiseResolveHook : undefined,
  });
}

class AsyncHook {
  constructor({ init, before, after, destroy, promiseResolve } = {}) {
    if (init !== undefined && typeof init !== "function") throw new ERR_ASYNC_CALLBACK("hook.init");
    if (before !== undefined && typeof before !== "function") throw new ERR_ASYNC_CALLBACK("hook.before");
    if (after !== undefined && typeof after !== "function") throw new ERR_ASYNC_CALLBACK("hook.after");
    if (destroy !== undefined && typeof destroy !== "function") throw new ERR_ASYNC_CALLBACK("hook.destroy");
    if (promiseResolve !== undefined && typeof promiseResolve !== "function") {
      throw new ERR_ASYNC_CALLBACK("hook.promiseResolve");
    }
    this.init = init;
    this.before = before;
    this.after = after;
    this.destroy = destroy;
    this.promiseResolve = promiseResolve;
    this.enabled = false;
  }

  enable() {
    if (!this.enabled) {
      this.enabled = true;
      active = [...active, this];
      syncHooks();
    }
    return this;
  }

  disable() {
    if (this.enabled) {
      this.enabled = false;
      active = active.filter((hook) => hook !== this);
      syncHooks();
    }
    return this;
  }
}

function createHook(fns) {
  return new AsyncHook(fns);
}

// ---- AsyncResource -------------------------------------------------------------------------
const kDestroyed = Symbol("destroyed");
const kContext = Symbol("asyncContext");

const destroyRegistry = new FinalizationRegistry(({ id, destroyed }) => {
  if (!destroyed.destroyed) emitDestroy(id);
});

class AsyncResource {
  constructor(type, opts = {}) {
    validateString(type, "type");
    let triggerId = opts;
    let requireManualDestroy = false;
    if (typeof opts !== "number") {
      triggerId = opts.triggerAsyncId === undefined ? executionAsyncId() : opts.triggerAsyncId;
      requireManualDestroy = !!opts.requireManualDestroy;
    }
    if (!Number.isSafeInteger(triggerId) || triggerId < -1) {
      throw new ERR_INVALID_ASYNC_ID("triggerAsyncId", triggerId);
    }
    const id = newAsyncId();
    this[__asyncIdSymbol] = id;
    this[__triggerIdSymbol] = triggerId;
    this[kContext] = __asyncContextGet();
    if (has("init")) {
      if (type.length === 0) throw new ERR_ASYNC_TYPE(type);
      emitInit(id, type, triggerId, this);
    }
    if (!requireManualDestroy && has("destroy")) {
      const destroyed = { destroyed: false };
      this[kDestroyed] = destroyed;
      destroyRegistry.register(this, { id, destroyed });
    }
  }

  runInAsyncScope(fn, thisArg, ...args) {
    return __runAsyncCallback(this, this[kContext], fn, thisArg, args);
  }

  emitDestroy() {
    if (this[kDestroyed] !== undefined) this[kDestroyed].destroyed = true;
    emitDestroy(this[__asyncIdSymbol]);
    return this;
  }

  asyncId() {
    return this[__asyncIdSymbol];
  }

  triggerAsyncId() {
    return this[__triggerIdSymbol];
  }

  bind(fn, thisArg) {
    validateFunction(fn, "fn");
    let bound;
    if (thisArg === undefined) {
      const resource = this;
      bound = function (...args) {
        args.unshift(fn, this);
        return Reflect.apply(resource.runInAsyncScope, resource, args);
      };
    } else {
      bound = Function.prototype.bind.call(this.runInAsyncScope, this, fn, thisArg);
    }
    const self = this;
    Object.defineProperties(bound, {
      length: { __proto__: null, configurable: true, enumerable: false, value: fn.length, writable: false },
      asyncResource: {
        __proto__: null,
        configurable: true,
        enumerable: true,
        get() { return self; },
        set(value) {
          Object.defineProperty(this, "asyncResource", {
            __proto__: null, configurable: true, enumerable: true, value, writable: true,
          });
        },
      },
    });
    return bound;
  }

  static bind(fn, type, thisArg) {
    type = type || fn.name;
    return new AsyncResource(type || "bound-anonymous-fn").bind(fn, thisArg);
  }
}

// ---- AsyncLocalStorage ---------------------------------------------------------------------
const withStore = (storage, store) => {
  const next = new Map(__asyncContextGet());
  next.set(storage, store);
  return next;
};

class AsyncLocalStorage {
  constructor() {
    this._enabled = true;
  }

  _propagate() {}

  run(store, cb, ...args) {
    this._enabled = true;
    return __runInAsyncContext(withStore(this, store), cb, undefined, args);
  }

  getStore() {
    if (!this._enabled) return undefined;
    const context = __asyncContextGet();
    return context === undefined ? undefined : context.get(this);
  }

  enterWith(store) {
    this._enabled = true;
    __asyncContextSet(withStore(this, store));
  }

  exit(cb, ...args) {
    const next = new Map(__asyncContextGet());
    next.delete(this);
    return __runInAsyncContext(next.size === 0 ? undefined : next, cb, undefined, args);
  }

  disable() {
    this._enabled = false;
  }

  static bind(fn) {
    return AsyncResource.bind(fn);
  }

  static snapshot() {
    const context = __asyncContextGet();
    return (fn, ...args) => __runInAsyncContext(context, fn, undefined, args);
  }
}

__builtins.set("async_hooks", {
  AsyncLocalStorage,
  createHook,
  executionAsyncId,
  triggerAsyncId,
  executionAsyncResource,
  asyncWrapProviders,
  AsyncResource,
});
