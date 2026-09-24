// node:events — a port of Node 20's lib/events.js. EventEmitter is a plain constructor function
// (so `EventEmitter.call(this)` and `util.inherits` work as in Node) whose prototype methods
// tolerate an uninitialized receiver; the module-level helpers (once/on/getEventListeners/
// setMaxListeners/addAbortListener/...) follow Node's validation and error codes.

const { ERR_INVALID_ARG_TYPE, ERR_OUT_OF_RANGE, ERR_UNHANDLED_ERROR } = __errors;
const { validateAbortSignal, validateBoolean, validateFunction, validateInteger, validateObject } = __validators;

const kCapture = Symbol("kCapture");
const kErrorMonitor = Symbol("events.errorMonitor");
const kShapeMode = Symbol("shapeMode");
const kMaxEventTargetListeners = Symbol("events.maxEventTargetListeners");
const kMaxEventTargetListenersWarned = Symbol("events.maxEventTargetListenersWarned");
const kWatermarkData = Symbol.for("nodejs.watermarkData");
const kRejection = Symbol.for("nodejs.rejection");
const kFirstEventParam = Symbol("nodejs.kFirstEventParam");
const SymbolDispose = Symbol.dispose ?? Symbol.for("nodejs.dispose");
const AsyncIteratorPrototype = Object.getPrototypeOf(Object.getPrototypeOf(async function* () {}).prototype);
const kEmptyObject = Object.freeze({ __proto__: null });

let defaultMaxListeners = 10;

const inspect = (v, o) => __builtins.get("util").inspect(v, o);

function EventEmitter(opts) {
  EventEmitter.init.call(this, opts);
}

EventEmitter.prototype._events = undefined;
EventEmitter.prototype._eventsCount = 0;
EventEmitter.prototype._maxListeners = undefined;
Object.defineProperty(EventEmitter.prototype, kCapture, {
  __proto__: null, value: false, writable: true, enumerable: false,
});
Object.defineProperty(EventEmitter.prototype, kShapeMode, {
  __proto__: null, value: false, writable: true, enumerable: false,
});

EventEmitter.EventEmitter = EventEmitter;
EventEmitter.usingDomains = false;
EventEmitter.captureRejectionSymbol = kRejection;
EventEmitter.errorMonitor = kErrorMonitor;

Object.defineProperty(EventEmitter, "captureRejections", {
  __proto__: null,
  get() { return EventEmitter.prototype[kCapture]; },
  set(value) {
    validateBoolean(value, "EventEmitter.captureRejections");
    EventEmitter.prototype[kCapture] = value;
  },
  enumerable: true,
});

Object.defineProperty(EventEmitter, "defaultMaxListeners", {
  __proto__: null,
  enumerable: true,
  get() { return defaultMaxListeners; },
  set(arg) {
    if (typeof arg !== "number" || arg < 0 || Number.isNaN(arg)) {
      throw new ERR_OUT_OF_RANGE("defaultMaxListeners", "a non-negative number", arg);
    }
    defaultMaxListeners = arg;
  },
});

Object.defineProperties(EventEmitter, {
  kMaxEventTargetListeners: { __proto__: null, value: kMaxEventTargetListeners, enumerable: false, configurable: false, writable: false },
  kMaxEventTargetListenersWarned: { __proto__: null, value: kMaxEventTargetListenersWarned, enumerable: false, configurable: false, writable: false },
});

function isEventTarget(obj) {
  return obj != null && typeof obj.addEventListener === "function" && typeof obj.dispatchEvent === "function"
    && typeof obj.on !== "function";
}

// setMaxListeners(n, ...eventTargets): the process default with no targets, else each target's.
EventEmitter.setMaxListeners = function setMaxListeners(n = defaultMaxListeners, ...eventTargets) {
  if (typeof n !== "number" || n < 0 || Number.isNaN(n)) {
    throw new ERR_OUT_OF_RANGE("n", "a non-negative number", n);
  }
  if (eventTargets.length === 0) {
    defaultMaxListeners = n;
  } else {
    for (let i = 0; i < eventTargets.length; i++) {
      const target = eventTargets[i];
      if (isEventTarget(target)) {
        target[kMaxEventTargetListeners] = n;
        target[kMaxEventTargetListenersWarned] = false;
      } else if (target != null && typeof target.setMaxListeners === "function") {
        target.setMaxListeners(n);
      } else {
        throw new ERR_INVALID_ARG_TYPE("eventTargets", ["EventEmitter", "EventTarget"], target);
      }
    }
  }
};

EventEmitter.init = function init(opts) {
  if (this._events === undefined || this._events === Object.getPrototypeOf(this)._events) {
    this._events = { __proto__: null };
    this._eventsCount = 0;
    this[kShapeMode] = false;
  } else {
    this[kShapeMode] = true;
  }

  this._maxListeners = this._maxListeners || undefined;

  if (opts?.captureRejections) {
    validateBoolean(opts.captureRejections, "options.captureRejections");
    this[kCapture] = Boolean(opts.captureRejections);
  } else {
    // Assigning the kCapture property directly saves an expensive lookup in the prototype chain.
    this[kCapture] = EventEmitter.prototype[kCapture];
  }
};

function addCatch(that, promise, type, args) {
  if (!that[kCapture]) return;
  // Handle Promises/A+ spec: then could be a getter that throws on second use.
  try {
    const then = promise.then;
    if (typeof then === "function") {
      then.call(promise, undefined, function (err) {
        // The callback is called with nextTick to avoid a follow-up rejection from this promise.
        process.nextTick(emitUnhandledRejectionOrErr, that, err, type, args);
      });
    }
  } catch (err) {
    that.emit("error", err);
  }
}

function emitUnhandledRejectionOrErr(ee, err, type, args) {
  if (typeof ee[kRejection] === "function") {
    ee[kRejection](err, type, ...args);
  } else {
    // We have to disable the capture rejections mechanism, otherwise we might end up in an
    // infinite loop.
    const prev = ee[kCapture];
    try {
      ee[kCapture] = false;
      ee.emit("error", err);
    } finally {
      ee[kCapture] = prev;
    }
  }
}

EventEmitter.prototype.setMaxListeners = function setMaxListeners(n) {
  if (typeof n !== "number" || n < 0 || Number.isNaN(n)) {
    throw new ERR_OUT_OF_RANGE("n", "a non-negative number", n);
  }
  this._maxListeners = n;
  return this;
};

function _getMaxListeners(that) {
  if (that._maxListeners === undefined) return defaultMaxListeners;
  return that._maxListeners;
}

EventEmitter.prototype.getMaxListeners = function getMaxListeners() {
  return _getMaxListeners(this);
};

EventEmitter.prototype.emit = function emit(type, ...args) {
  let doError = type === "error";

  const events = this._events;
  if (events !== undefined) {
    if (doError && events[kErrorMonitor] !== undefined) this.emit(kErrorMonitor, ...args);
    doError = doError && events.error === undefined;
  } else if (!doError) {
    return false;
  }

  // If there is no 'error' event listener then throw.
  if (doError) {
    let er;
    if (args.length > 0) er = args[0];
    if (er instanceof Error) {
      throw er; // Unhandled 'error' event
    }

    let stringifiedEr;
    try {
      stringifiedEr = inspect(er);
    } catch {
      stringifiedEr = er;
    }

    // At least give some kind of context to the user
    const err = new ERR_UNHANDLED_ERROR(stringifiedEr);
    err.context = er;
    throw err; // Unhandled 'error' event
  }

  const handler = events[type];

  if (handler === undefined) return false;

  if (typeof handler === "function") {
    const result = handler.apply(this, args);

    // We check if result is undefined first because that is the most common case so we do not
    // pay any perf penalty.
    if (result !== undefined && result !== null) {
      addCatch(this, result, type, args);
    }
  } else {
    const len = handler.length;
    const listeners = arrayClone(handler);
    for (let i = 0; i < len; ++i) {
      const result = listeners[i].apply(this, args);
      if (result !== undefined && result !== null) {
        addCatch(this, result, type, args);
      }
    }
  }

  return true;
};

function checkListener(listener) {
  validateFunction(listener, "listener");
}

function _addListener(target, type, listener, prepend) {
  let m;
  let events;
  let existing;

  checkListener(listener);

  events = target._events;
  if (events === undefined) {
    events = target._events = { __proto__: null };
    target._eventsCount = 0;
  } else {
    // To avoid recursion in the case that type === "newListener"! Before adding it to the
    // listeners, first emit "newListener".
    if (events.newListener !== undefined) {
      target.emit("newListener", type, listener.listener ?? listener);

      // Re-assign `events` because a newListener handler could have caused the this._events to
      // be assigned to a new object.
      events = target._events;
    }
    existing = events[type];
  }

  if (existing === undefined) {
    // Optimize the case of one listener. Don't need the extra array object.
    events[type] = listener;
    ++target._eventsCount;
  } else {
    if (typeof existing === "function") {
      // Adding the second element, need to change to array.
      existing = events[type] = prepend ? [listener, existing] : [existing, listener];
      // If we've already got an array, just append.
    } else if (prepend) {
      existing.unshift(listener);
    } else {
      existing.push(listener);
    }

    // Check for listener leak
    m = _getMaxListeners(target);
    if (m > 0 && existing.length > m && !existing.warned) {
      existing.warned = true;
      // No error code for this since it is a Warning
      const w = new Error("Possible EventEmitter memory leak detected. " +
        `${existing.length} ${String(type)} listeners ` +
        `added to ${inspect(target, { depth: -1 })}. Use ` +
        "emitter.setMaxListeners() to increase limit");
      w.name = "MaxListenersExceededWarning";
      w.emitter = target;
      w.type = type;
      w.count = existing.length;
      process.emitWarning(w);
    }
  }

  return target;
}

EventEmitter.prototype.addListener = function addListener(type, listener) {
  return _addListener(this, type, listener, false);
};

EventEmitter.prototype.on = EventEmitter.prototype.addListener;

EventEmitter.prototype.prependListener = function prependListener(type, listener) {
  return _addListener(this, type, listener, true);
};

function onceWrapper() {
  if (!this.fired) {
    this.target.removeListener(this.type, this.wrapFn);
    this.fired = true;
    if (arguments.length === 0) return this.listener.call(this.target);
    return this.listener.apply(this.target, arguments);
  }
}

function _onceWrap(target, type, listener) {
  const state = { fired: false, wrapFn: undefined, target, type, listener };
  const wrapped = onceWrapper.bind(state);
  wrapped.listener = listener;
  state.wrapFn = wrapped;
  return wrapped;
}

EventEmitter.prototype.once = function once(type, listener) {
  checkListener(listener);
  this.on(type, _onceWrap(this, type, listener));
  return this;
};

EventEmitter.prototype.prependOnceListener = function prependOnceListener(type, listener) {
  checkListener(listener);
  this.prependListener(type, _onceWrap(this, type, listener));
  return this;
};

EventEmitter.prototype.removeListener = function removeListener(type, listener) {
  checkListener(listener);

  const events = this._events;
  if (events === undefined) return this;

  const list = events[type];
  if (list === undefined) return this;

  if (list === listener || list.listener === listener) {
    this._eventsCount -= 1;

    if (this[kShapeMode]) {
      events[type] = undefined;
    } else if (this._eventsCount === 0) {
      this._events = { __proto__: null };
    } else {
      delete events[type];
      if (events.removeListener) this.emit("removeListener", type, list.listener || listener);
    }
  } else if (typeof list !== "function") {
    let position = -1;

    for (let i = list.length - 1; i >= 0; i--) {
      if (list[i] === listener || list[i].listener === listener) {
        position = i;
        break;
      }
    }

    if (position < 0) return this;

    if (position === 0) list.shift();
    else list.splice(position, 1);

    if (list.length === 1) events[type] = list[0];

    if (events.removeListener !== undefined) this.emit("removeListener", type, listener);
  }

  return this;
};

EventEmitter.prototype.off = EventEmitter.prototype.removeListener;

EventEmitter.prototype.removeAllListeners = function removeAllListeners(type) {
  const events = this._events;
  if (events === undefined) return this;

  // Not listening for removeListener, no need to emit
  if (events.removeListener === undefined) {
    if (arguments.length === 0) {
      this._events = { __proto__: null };
      this._eventsCount = 0;
    } else if (events[type] !== undefined) {
      if (--this._eventsCount === 0) this._events = { __proto__: null };
      else delete events[type];
    }
    this[kShapeMode] = false;
    return this;
  }

  // Emit removeListener for all listeners on all events
  if (arguments.length === 0) {
    for (const key of Reflect.ownKeys(events)) {
      if (key === "removeListener") continue;
      this.removeAllListeners(key);
    }
    this.removeAllListeners("removeListener");
    this._events = { __proto__: null };
    this._eventsCount = 0;
    this[kShapeMode] = false;
    return this;
  }

  const listeners = events[type];

  if (typeof listeners === "function") {
    this.removeListener(type, listeners);
  } else if (listeners !== undefined) {
    // LIFO order
    for (let i = listeners.length - 1; i >= 0; i--) {
      this.removeListener(type, listeners[i]);
    }
  }

  return this;
};

function _listeners(target, type, unwrap) {
  const events = target._events;

  if (events === undefined) return [];

  const evlistener = events[type];
  if (evlistener === undefined) return [];

  if (typeof evlistener === "function") return unwrap ? [evlistener.listener || evlistener] : [evlistener];

  return unwrap ? unwrapListeners(evlistener) : arrayClone(evlistener);
}

EventEmitter.prototype.listeners = function listeners(type) {
  return _listeners(this, type, true);
};

EventEmitter.prototype.rawListeners = function rawListeners(type) {
  return _listeners(this, type, false);
};

EventEmitter.listenerCount = function (emitter, type) {
  if (typeof emitter.listenerCount === "function") {
    return emitter.listenerCount(type);
  }
  return listenerCount.call(emitter, type);
};

EventEmitter.prototype.listenerCount = listenerCount;
function listenerCount(type) {
  const events = this._events;

  if (events !== undefined) {
    const evlistener = events[type];

    if (typeof evlistener === "function") {
      return 1;
    } else if (evlistener !== undefined) {
      return evlistener.length;
    }
  }

  return 0;
}

EventEmitter.prototype.eventNames = function eventNames() {
  return this._eventsCount > 0 ? Reflect.ownKeys(this._events) : [];
};

function arrayClone(arr) {
  // At least since V8 8.3, this implementation is faster than the previous which always used a
  // simple for-loop
  switch (arr.length) {
    case 2: return [arr[0], arr[1]];
    case 3: return [arr[0], arr[1], arr[2]];
    case 4: return [arr[0], arr[1], arr[2], arr[3]];
    case 5: return [arr[0], arr[1], arr[2], arr[3], arr[4]];
    case 6: return [arr[0], arr[1], arr[2], arr[3], arr[4], arr[5]];
  }
  return Array.prototype.slice.call(arr);
}

function unwrapListeners(arr) {
  const ret = arrayClone(arr);
  for (let i = 0; i < ret.length; ++i) {
    const orig = ret[i].listener;
    if (typeof orig === "function") ret[i] = orig;
  }
  return ret;
}

// lumen-web's EventTarget keeps `_listeners`: a Map of type -> [{ callback, ... }].
function eventTargetListeners(target, type) {
  const map = target._listeners;
  if (!(map instanceof Map)) return [];
  const list = map.get(String(type));
  return list ? list.filter((l) => !l.removed).map((l) => l.callback) : [];
}

function getEventListeners(emitterOrTarget, type) {
  // First check if EventEmitter
  if (emitterOrTarget != null && typeof emitterOrTarget.listeners === "function") {
    return emitterOrTarget.listeners(type);
  }
  if (isEventTarget(emitterOrTarget)) return eventTargetListeners(emitterOrTarget, type);
  throw new ERR_INVALID_ARG_TYPE("emitter", ["EventEmitter", "EventTarget"], emitterOrTarget);
}

function getMaxListeners(emitterOrTarget) {
  if (typeof emitterOrTarget?.getMaxListeners === "function") {
    return _getMaxListeners(emitterOrTarget);
  } else if (isEventTarget(emitterOrTarget)) {
    return emitterOrTarget[kMaxEventTargetListeners] ?? defaultMaxListeners;
  }
  throw new ERR_INVALID_ARG_TYPE("emitter", ["EventEmitter", "EventTarget"], emitterOrTarget);
}

class AbortError extends Error {
  constructor(message = "The operation was aborted", options = undefined) {
    super(message, options);
    this.code = "ABORT_ERR";
    this.name = "AbortError";
  }
}

async function once(emitter, name, options = kEmptyObject) {
  validateObject(options, "options");
  const signal = options?.signal;
  validateAbortSignal(signal, "options.signal");
  if (signal?.aborted) throw new AbortError(undefined, { cause: signal?.reason });
  return new Promise((resolve, reject) => {
    const errorListener = (err) => {
      emitter.removeListener(name, resolver);
      if (signal != null) {
        eventTargetAgnosticRemoveListener(signal, "abort", abortListener);
      }
      reject(err);
    };
    const resolver = (...args) => {
      if (typeof emitter.removeListener === "function") {
        emitter.removeListener("error", errorListener);
      }
      if (signal != null) {
        eventTargetAgnosticRemoveListener(signal, "abort", abortListener);
      }
      resolve(args);
    };

    eventTargetAgnosticAddListener(emitter, name, resolver, { __proto__: null, once: true });
    if (name !== "error" && typeof emitter.once === "function") {
      // EventTarget does not have `error` event semantics like Node EventEmitters, we listen to
      // `error` events only on EventEmitters.
      emitter.once("error", errorListener);
    }
    function abortListener() {
      eventTargetAgnosticRemoveListener(emitter, name, resolver);
      eventTargetAgnosticRemoveListener(emitter, "error", errorListener);
      reject(new AbortError(undefined, { cause: signal?.reason }));
    }
    if (signal != null) {
      eventTargetAgnosticAddListener(signal, "abort", abortListener, { __proto__: null, once: true });
    }
  });
}

function createIterResult(value, done) {
  return { value, done };
}

function eventTargetAgnosticRemoveListener(emitter, name, listener, flags) {
  if (typeof emitter.removeListener === "function") {
    emitter.removeListener(name, listener);
  } else if (typeof emitter.removeEventListener === "function") {
    emitter.removeEventListener(name, listener, flags);
  } else {
    throw new ERR_INVALID_ARG_TYPE("emitter", "EventEmitter", emitter);
  }
}

function eventTargetAgnosticAddListener(emitter, name, listener, flags) {
  if (typeof emitter.on === "function") {
    if (flags?.once) {
      emitter.once(name, listener);
    } else {
      emitter.on(name, listener);
    }
  } else if (typeof emitter.addEventListener === "function") {
    emitter.addEventListener(name, listener, flags);
  } else {
    throw new ERR_INVALID_ARG_TYPE("emitter", "EventEmitter", emitter);
  }
}

// on(emitter, event[, options]): an async iterator over `event`'s argument arrays.
function on(emitter, event, options = kEmptyObject) {
  // Parameters validation
  validateObject(options, "options");
  const signal = options.signal;
  validateAbortSignal(signal, "options.signal");
  if (signal?.aborted) throw new AbortError(undefined, { cause: signal?.reason });
  // Support both highWaterMark and highWatermark for backward compatibility
  const highWatermark = options.highWaterMark ?? options.highWatermark ?? Number.MAX_SAFE_INTEGER;
  validateInteger(highWatermark, "options.highWaterMark", 1);
  // Support both lowWaterMark and lowWatermark for backward compatibility
  const lowWatermark = options.lowWaterMark ?? options.lowWatermark ?? 1;
  validateInteger(lowWatermark, "options.lowWaterMark", 1);

  // Preparing controlling queues and variables
  const unconsumedEvents = [];
  const unconsumedPromises = [];
  let paused = false;
  let error = null;
  let finished = false;
  let size = 0;

  const iterator = Object.setPrototypeOf({
    next() {
      // First, we consume all unread events
      if (size) {
        const value = unconsumedEvents.shift();
        size--;
        if (paused && size < lowWatermark) {
          emitter.resume();
          paused = false;
        }
        return Promise.resolve(createIterResult(value, false));
      }

      // Then we error, if an error happened. This happens one time if at all, because after
      // 'error' we stop listening.
      if (error) {
        const p = Promise.reject(error);
        // Only the first element errors
        error = null;
        return p;
      }

      // If the iterator is finished, resolve to done
      if (finished) return closeHandler();

      // Wait until an event happens
      return new Promise(function (resolve, reject) {
        unconsumedPromises.push({ resolve, reject });
      });
    },

    return() {
      return closeHandler();
    },

    throw(err) {
      if (!err || !(err instanceof Error)) {
        throw new ERR_INVALID_ARG_TYPE("EventEmitter.AsyncIterator", "Error", err);
      }
      errorHandler(err);
    },
    [Symbol.asyncIterator]() {
      return this;
    },
    [kWatermarkData]: {
      get size() { return size; },
      get low() { return lowWatermark; },
      get high() { return highWatermark; },
      get isPaused() { return paused; },
    },
  }, AsyncIteratorPrototype);

  // Adding event handlers
  const { addEventListener, removeAll } = listenersController();
  addEventListener(emitter, event, options[kFirstEventParam] ? eventHandler : function (...args) {
    return eventHandler(args);
  });
  if (event !== "error" && typeof emitter.on === "function") {
    addEventListener(emitter, "error", errorHandler);
  }
  const closeEvents = options?.close;
  if (closeEvents?.length) {
    for (let i = 0; i < closeEvents.length; i++) {
      addEventListener(emitter, closeEvents[i], closeHandler);
    }
  }

  const abortListenerDisposable = signal ? addAbortListener(signal, abortListener) : null;

  return iterator;

  function abortListener() {
    errorHandler(new AbortError(undefined, { cause: signal?.reason }));
  }

  function eventHandler(value) {
    if (unconsumedPromises.length === 0) {
      size++;
      if (!paused && size > highWatermark) {
        paused = true;
        emitter.pause();
      }
      unconsumedEvents.push(value);
    } else {
      unconsumedPromises.shift().resolve(createIterResult(value, false));
    }
  }

  function errorHandler(err) {
    if (unconsumedPromises.length === 0) error = err;
    else unconsumedPromises.shift().reject(err);

    closeHandler();
  }

  function closeHandler() {
    abortListenerDisposable?.[SymbolDispose]();
    removeAll();
    finished = true;
    const doneResult = createIterResult(undefined, true);
    while (unconsumedPromises.length !== 0) {
      unconsumedPromises.shift().resolve(doneResult);
    }

    return Promise.resolve(doneResult);
  }
}

function listenersController() {
  const listeners = [];

  return {
    addEventListener(emitter, event, handler, flags) {
      eventTargetAgnosticAddListener(emitter, event, handler, flags);
      listeners.push([emitter, event, handler, flags]);
    },
    removeAll() {
      while (listeners.length > 0) {
        Reflect.apply(eventTargetAgnosticRemoveListener, undefined, listeners.pop());
      }
    },
  };
}

// addAbortListener(signal, listener): a one-shot 'abort' listener, returned as a Disposable.
function addAbortListener(signal, listener) {
  if (signal === undefined) {
    throw new ERR_INVALID_ARG_TYPE("signal", "AbortSignal", signal);
  }
  validateAbortSignal(signal, "signal");
  validateFunction(listener, "listener");

  let removeEventListener;
  if (signal.aborted) {
    queueMicrotask(() => listener());
  } else {
    signal.addEventListener("abort", listener, { __proto__: null, once: true });
    removeEventListener = () => {
      signal.removeEventListener("abort", listener);
    };
  }
  return {
    __proto__: null,
    [SymbolDispose]() {
      removeEventListener?.();
    },
  };
}

EventEmitter.once = once;
EventEmitter.on = on;
EventEmitter.getEventListeners = getEventListeners;
EventEmitter.getMaxListeners = getMaxListeners;
EventEmitter.addAbortListener = addAbortListener;

// EventEmitterAsyncResource — an EventEmitter that carries an AsyncResource so listeners run in the
// emitter's async context. lumen has no async-context tracking, so the resource is inert, but the
// documented surface (asyncResource/asyncId/triggerAsyncId/emitDestroy) is present and callbacks run.
const kAsyncResource = Symbol("kAsyncResource");
const kEventEmitter = Symbol("kEventEmitter");
let earNextId = 1;
class EventEmitterAsyncResource extends EventEmitter {
  constructor(options = undefined) {
    let name;
    if (typeof options === "string") {
      name = options;
      options = undefined;
    } else {
      if (new.target === EventEmitterAsyncResource) {
        validateString(options?.name, "options.name");
      }
      name = options?.name || new.target.name;
    }
    super(options);
    const id = earNextId++;
    const triggerAsyncId = options?.triggerAsyncId ?? 0;
    this[kAsyncResource] = {
      type: name,
      [kEventEmitter]: this,
      get eventEmitter() { return this[kEventEmitter]; },
      runInAsyncScope(fn, thisArg, ...args) { return Reflect.apply(fn, thisArg, args); },
      asyncId() { return id; },
      triggerAsyncId() { return triggerAsyncId; },
      emitDestroy() { return this; },
      bind(fn) { return fn; },
    };
  }
  emit(event, ...args) {
    const { asyncResource } = this;
    args.unshift(super.emit, this, event);
    return Reflect.apply(asyncResource.runInAsyncScope, asyncResource, args);
  }
  emitDestroy() {
    this.asyncResource.emitDestroy();
  }
  get asyncId() { return this.asyncResource.asyncId(); }
  get triggerAsyncId() { return this.asyncResource.triggerAsyncId(); }
  get asyncResource() {
    if (this[kAsyncResource] === undefined) {
      throw new __errors.ERR_INVALID_THIS("EventEmitterAsyncResource");
    }
    return this[kAsyncResource];
  }
}
function validateString(value, name) {
  if (typeof value !== "string") throw new ERR_INVALID_ARG_TYPE(name, "string", value);
}
Object.defineProperty(EventEmitter, "EventEmitterAsyncResource", {
  __proto__: null, enumerable: true, value: EventEmitterAsyncResource, writable: true, configurable: true,
});

__builtins.set("events", EventEmitter);
