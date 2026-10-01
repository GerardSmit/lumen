// node:async_hooks — async ids, the hook API and AsyncLocalStorage/AsyncResource over the engine's
// async context (see preamble.js). A context is an immutable Map: AsyncLocalStorage instances key
// their stores in it, and one extra entry (`kFrame`) records the async resource that is executing
// (id, trigger id, resource). The engine carries the context along promise reactions; timers,
// nextTick and immediates bind it at scheduling time through `__bindAsyncContext`, which — only
// while hooks are enabled — also emits init/before/after/destroy for those resources.

const {
  ERR_ASYNC_CALLBACK, ERR_ASYNC_TYPE, ERR_INVALID_ASYNC_ID,
} = __errors;
const { validateString, validateFunction } = __validators;

const kFrame = Symbol("asyncFrame");
// Id 1 is the main script's execution; resources get ids from 2.
let idCounter = 1;
const newAsyncId = () => ++idCounter;
const topLevelResource = {};

function currentFrame() {
  const context = __asyncContextGet();
  return context === undefined ? undefined : context.get(kFrame);
}
function executionAsyncId() {
  return currentFrame()?.id ?? 1;
}
function triggerAsyncId() {
  return currentFrame()?.trigger ?? 0;
}
function executionAsyncResource() {
  return currentFrame()?.resource ?? topLevelResource;
}

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

// ---- hooks ---------------------------------------------------------------------------------
let active = [];
const resourceIds = new WeakMap();

function syncHooks() {
  __setAsyncHooks(active.length === 0 ? null : hookRuntime);
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

function emitInit(id, type, trigger, resource) {
  for (const hook of active) {
    if (hook.init !== undefined) hook.init(id, type, trigger, resource);
  }
}
function emitBefore(id) {
  for (const hook of active) {
    if (hook.before !== undefined) hook.before(id);
  }
}
function emitAfter(id) {
  for (const hook of active) {
    if (hook.after !== undefined) hook.after(id);
  }
}
function destroyHooksExist() {
  return active.some((hook) => hook.destroy !== undefined);
}
function emitDestroy(id) {
  if (!destroyHooksExist()) return;
  queueMicrotask(() => {
    for (const hook of active) {
      if (hook.destroy !== undefined) hook.destroy(id);
    }
  });
}

function runScope(context, frame, id, fn, thisArg, args) {
  const next = new Map(context);
  next.set(kFrame, frame);
  const previous = __asyncContextSet(next);
  emitBefore(id);
  try {
    return Reflect.apply(fn, thisArg, args);
  } catch (error) {
    __noteThrown(error);
    throw error;
  } finally {
    emitAfter(id);
    __asyncContextSet(previous);
  }
}

// What preamble.js calls while any hook is enabled (`__asyncHooks`).
const hookRuntime = {
  wrap(fn, type, resource, repeat) {
    const id = newAsyncId();
    const trigger = executionAsyncId();
    if (resource !== undefined) resourceIds.set(resource, id);
    emitInit(id, type, trigger, resource);
    const context = __asyncContextGet();
    const frame = { id, trigger, resource };
    return function boundWithAsyncHooks(...args) {
      try {
        return runScope(context, frame, id, fn, this, args);
      } finally {
        if (!repeat) {
          if (resource !== undefined) resourceIds.delete(resource);
          emitDestroy(id);
        }
      }
    };
  },
  destroyOf(resource) {
    const id = resourceIds.get(resource);
    if (id === undefined) return;
    resourceIds.delete(resource);
    emitDestroy(id);
  },
};

// ---- AsyncResource -------------------------------------------------------------------------
const kAsyncId = Symbol("asyncId");
const kTriggerId = Symbol("triggerAsyncId");
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
    this[kAsyncId] = id;
    this[kTriggerId] = triggerId;
    this[kContext] = __asyncContextGet();
    if (active.some((hook) => hook.init !== undefined)) {
      if (type.length === 0) throw new ERR_ASYNC_TYPE(type);
      emitInit(id, type, triggerId, this);
    }
    if (!requireManualDestroy && destroyHooksExist()) {
      const destroyed = { destroyed: false };
      this[kDestroyed] = destroyed;
      destroyRegistry.register(this, { id, destroyed });
    }
  }

  runInAsyncScope(fn, thisArg, ...args) {
    const frame = { id: this[kAsyncId], trigger: this[kTriggerId], resource: this };
    return runScope(this[kContext], frame, this[kAsyncId], fn, thisArg, args);
  }

  emitDestroy() {
    if (this[kDestroyed] !== undefined) this[kDestroyed].destroyed = true;
    emitDestroy(this[kAsyncId]);
    return this;
  }

  asyncId() {
    return this[kAsyncId];
  }

  triggerAsyncId() {
    return this[kTriggerId];
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
