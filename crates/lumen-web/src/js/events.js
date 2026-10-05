// DOMException and tree-aware event dispatch, also used by standalone worker targets.

const domExceptionCodes = [
  "IndexSizeError", "DOMStringSizeError", "HierarchyRequestError", "WrongDocumentError",
  "InvalidCharacterError", "NoDataAllowedError", "NoModificationAllowedError", "NotFoundError",
  "NotSupportedError", "InUseAttributeError", "InvalidStateError", "SyntaxError",
  "InvalidModificationError", "NamespaceError", "InvalidAccessError", "ValidationError",
  "TypeMismatchError", "SecurityError", "NetworkError", "AbortError", "URLMismatchError",
  "QuotaExceededError", "TimeoutError", "InvalidNodeTypeError", "DataCloneError",
];
const domExceptionConstants = [
  "INDEX_SIZE_ERR", "DOMSTRING_SIZE_ERR", "HIERARCHY_REQUEST_ERR", "WRONG_DOCUMENT_ERR",
  "INVALID_CHARACTER_ERR", "NO_DATA_ALLOWED_ERR", "NO_MODIFICATION_ALLOWED_ERR", "NOT_FOUND_ERR",
  "NOT_SUPPORTED_ERR", "INUSE_ATTRIBUTE_ERR", "INVALID_STATE_ERR", "SYNTAX_ERR",
  "INVALID_MODIFICATION_ERR", "NAMESPACE_ERR", "INVALID_ACCESS_ERR", "VALIDATION_ERR",
  "TYPE_MISMATCH_ERR", "SECURITY_ERR", "NETWORK_ERR", "ABORT_ERR", "URL_MISMATCH_ERR",
  "QUOTA_EXCEEDED_ERR", "TIMEOUT_ERR", "INVALID_NODE_TYPE_ERR", "DATA_CLONE_ERR",
];

class DOMException extends Error {
  constructor(message = "", options = "Error") {
    if (options !== null && typeof options === "object") {
      super(message, "cause" in options ? { cause: options.cause } : undefined);
      this.name = "name" in options ? String(options.name) : "Error";
    } else {
      super(message);
      this.name = String(options);
    }
  }
  get code() {
    const i = domExceptionCodes.indexOf(this.name);
    return i === -1 ? 0 : i + 1;
  }
}
for (let i = 0; i < domExceptionConstants.length; i++) {
  const desc = { value: i + 1, writable: false, configurable: false, enumerable: true };
  Object.defineProperty(DOMException, domExceptionConstants[i], desc);
  Object.defineProperty(DOMException.prototype, domExceptionConstants[i], desc);
}

const kResistStopPropagation = Symbol.for("nodejs.internal.kResistStopPropagation");
const kWeakHandler = Symbol.for("nodejs.internal.kWeakHandler");
const kEmptyObject = Object.freeze({ __proto__: null });
const kTrustEvent = Symbol("kTrustEvent");
const kEvents = Symbol.for("lumen.kEvents");
const kMaxEventTargetListeners = Symbol.for("events.maxEventTargetListeners");
const kMaxEventTargetListenersWarned = Symbol.for("events.maxEventTargetListenersWarned");
const kIsNodeStyleListener = Symbol("kIsNodeStyleListener");
const kHybridDispatch = Symbol("kHybridDispatch");
const kCreateEvent = Symbol("kCreateEvent");
const kNewListener = Symbol("kNewListener");
const kRemoveListener = Symbol("kRemoveListener");
const kType = Symbol("kType");
const kTarget = Symbol("kTarget");
const kDispatching = Symbol("kIsBeingDispatched");
const kStop = Symbol("kStop");
const kTrusted = Symbol("kTrusted");
const kHandlers = Symbol("kHandlers");
const inspectCustom = Symbol.for("nodejs.util.inspect.custom");

function receivedText(value) {
  if (value == null) return ` Received ${value}`;
  if (typeof value === "function") return ` Received function ${value.name}`;
  if (typeof value === "object") {
    const name = value.constructor?.name;
    return name ? ` Received an instance of ${name}` : " Received [Object: null prototype] {}";
  }
  let shown;
  if (typeof value === "string") {
    shown = value.length > 25 ? `${value.slice(0, 25)}...` : value;
    shown = `'${shown}'`;
  } else if (typeof value === "bigint") {
    shown = `${value}n`;
  } else if (Object.is(value, -0)) {
    shown = "-0";
  } else {
    shown = String(value);
  }
  return ` Received type ${typeof value} (${shown})`;
}

function codedError(Base, code, message) {
  const err = new Base(message);
  Object.defineProperty(err, "toString", {
    value() { return `${this.name} [${code}]: ${this.message}`; },
    enumerable: false, writable: true, configurable: true,
  });
  err.code = code;
  if (typeof err.stack === "string" && err.stack.startsWith(`${err.name}: `)) {
    err.stack = `${err.name} [${code}]${err.stack.slice(err.name.length)}`;
  }
  return err;
}
function invalidArgType(name, expected, value) {
  const kind = name.includes(".") ? "property" : "argument";
  return codedError(TypeError, "ERR_INVALID_ARG_TYPE", `The "${name}" ${kind} must be ${expected}.${receivedText(value)}`);
}
const invalidThis = (name) => codedError(TypeError, "ERR_INVALID_THIS", `Value of "this" must be of type ${name}`);

function validateEventObject(options) {
  if (options === null || options === undefined) return;
  if (typeof options !== "object" && typeof options !== "function") {
    throw invalidArgType("options", "of type object", options);
  }
}

function inspectObject(self, fields, depth, options, inspect, named = false) {
  const name = self.constructor?.name ?? "Object";
  if (depth < 0) return named ? name : self;
  const opts = { ...options, depth: Number.isInteger(options.depth) ? options.depth - 1 : options.depth };
  return `${name} ${inspect(fields, opts)}`;
}

class Event {
  constructor(type, options = kEmptyObject) {
    if (arguments.length === 0) {
      throw codedError(TypeError, "ERR_MISSING_ARGS", 'The "type" argument must be specified');
    }
    validateEventObject(options);
    this[kType] = `${type}`;
    this[kStop] = false;
    this[kTrusted] = options?.[kTrustEvent] === true;
    this[kTarget] = null;
    this[kDispatching] = false;
    const bubbles = !!options?.bubbles;
    const cancelable = !!options?.cancelable;
    const composed = !!options?.composed;
    const state = { cancelable, bubbles, composed, defaultPrevented: false, propagationStopped: false, passive: false, currentTarget: null, phase: 0, path: [], timeStamp: performance.now() };
    Object.defineProperty(this, kEventState, { value: state, enumerable: false });
  }
  [inspectCustom](depth, options, inspect) {
    if (!isEvent(this)) throw invalidThis("Event");
    return inspectObject(this, { type: this[kType], defaultPrevented: this.defaultPrevented, cancelable: this.cancelable, timeStamp: this.timeStamp }, depth, options, inspect, true);
  }
  stopImmediatePropagation() {
    if (!isEvent(this)) throw invalidThis("Event");
    this[kStop] = true;
  }
  preventDefault() {
    if (!isEvent(this)) throw invalidThis("Event");
    if (!this[kEventState].passive) this[kEventState].defaultPrevented = true;
  }
  get target() {
    if (!isEvent(this)) throw invalidThis("Event");
    return this[kTarget];
  }
  get currentTarget() {
    if (!isEvent(this)) throw invalidThis("Event");
    return this[kEventState].currentTarget;
  }
  get srcElement() {
    if (!isEvent(this)) throw invalidThis("Event");
    return this[kTarget];
  }
  get type() {
    if (!isEvent(this)) throw invalidThis("Event");
    return this[kType];
  }
  get cancelable() {
    if (!isEvent(this)) throw invalidThis("Event");
    return this[kEventState].cancelable;
  }
  get defaultPrevented() {
    if (!isEvent(this)) throw invalidThis("Event");
    return this[kEventState].cancelable && this[kEventState].defaultPrevented;
  }
  get timeStamp() {
    if (!isEvent(this)) throw invalidThis("Event");
    return this[kEventState].timeStamp;
  }
  composedPath() {
    if (!isEvent(this)) throw invalidThis("Event");
    return this[kDispatching] ? this[kEventState].path.slice() : [];
  }
  get returnValue() {
    if (!isEvent(this)) throw invalidThis("Event");
    return !this.defaultPrevented;
  }
  get bubbles() {
    if (!isEvent(this)) throw invalidThis("Event");
    return this[kEventState].bubbles;
  }
  get composed() {
    if (!isEvent(this)) throw invalidThis("Event");
    return this[kEventState].composed;
  }
  get eventPhase() {
    if (!isEvent(this)) throw invalidThis("Event");
    return this[kEventState].phase;
  }
  get cancelBubble() {
    if (!isEvent(this)) throw invalidThis("Event");
    return this[kEventState].propagationStopped;
  }
  set cancelBubble(value) {
    if (!isEvent(this)) throw invalidThis("Event");
    if (value) this.stopPropagation();
  }
  stopPropagation() {
    if (!isEvent(this)) throw invalidThis("Event");
    this[kEventState].propagationStopped = true;
  }
  get isTrusted() {
    if (!isEvent(this)) throw invalidThis("Event");
    return this[kTrusted];
  }
}
const kEventState = Symbol("kEventState");
function isEvent(value) {
  return typeof value?.[kType] === "string" && value[kEventState] !== undefined;
}
Object.defineProperty(Event.prototype, Symbol.toStringTag, { value: "Event", configurable: true });
for (const [i, name] of ["NONE", "CAPTURING_PHASE", "AT_TARGET", "BUBBLING_PHASE"].entries()) {
  const desc = { value: i, writable: false, configurable: false, enumerable: true };
  Object.defineProperty(Event, name, desc);
  Object.defineProperty(Event.prototype, name, desc);
}
for (const name of ["target", "currentTarget", "srcElement", "type", "cancelable", "defaultPrevented", "timeStamp",
  "returnValue", "bubbles", "composed", "eventPhase", "isTrusted", "cancelBubble", "stopPropagation",
  "stopImmediatePropagation", "preventDefault", "composedPath"]) {
  Object.defineProperty(Event.prototype, name, { enumerable: true });
}

class NodeCustomEvent extends Event {
  constructor(type, options) {
    super(type, options);
    if (options?.detail) this.detail = options.detail;
  }
}

function defaultMaxListeners() {
  return globalThis[Symbol.for("lumen.EventEmitter")]?.defaultMaxListeners ?? 10;
}

function initEventTarget(self) {
  self[kEvents] = new Map();
  self[kMaxEventTargetListeners] = defaultMaxListeners();
  self[kMaxEventTargetListenersWarned] = false;
}

function isEventTarget(value) {
  return value !== null && typeof value === "object" && value[kEvents] instanceof Map;
}

const keepAlive = new WeakMap();

function entryCallback(entry) {
  return entry.weak ? entry.callback.deref() : entry.callback;
}

function reportUncaught(error) {
  const rethrow = () => {
    throw error;
  };
  if (typeof process === "object" && typeof process?.nextTick === "function") process.nextTick(rethrow);
  else setTimeout(rethrow, 0);
}

function validateEventListener(listener, name) {
  if (typeof listener === "function" || typeof listener?.handleEvent === "function") return true;
  if (listener === null || listener === undefined) return false;
  if (typeof listener === "object") return true;
  throw invalidArgType(name, "an instance of EventListener", listener);
}

function eventListenerOptions(options) {
  if (typeof options === "boolean") return { capture: options };
  if (options === null || options === undefined) return {};
  if (typeof options !== "object" && typeof options !== "function") {
    throw invalidArgType("options", "of type object", options);
  }
  return {
    once: !!options.once,
    capture: !!options.capture,
    passive: !!options.passive,
    signal: options.signal,
    weak: options[kWeakHandler],
    nodeStyle: !!options[kIsNodeStyleListener],
    resist: options[kResistStopPropagation] === true,
  };
}

function missingArgs(names) {
  const list = names.map((n) => `"${n}"`);
  const text = list.length === 1 ? `The ${list[0]} argument` : `The ${list.join(" and ")} arguments`;
  return codedError(TypeError, "ERR_MISSING_ARGS", `${text} must be specified`);
}

class EventTarget {
  constructor() {
    initEventTarget(this);
  }
  [kNewListener](size, type) {
    const max = this[kMaxEventTargetListeners];
    if (max > 0 && size > max && !this[kMaxEventTargetListenersWarned]) {
      this[kMaxEventTargetListenersWarned] = true;
      const inspect = globalThis[Symbol.for("lumen.inspect")];
      const shown = inspect ? inspect(this, { depth: -1 }) : (this.constructor?.name ?? "EventTarget");
      const w = new Error(`Possible EventTarget memory leak detected. ${size} ${type} listeners added to ${shown}. Use events.setMaxListeners() to increase limit`);
      w.name = "MaxListenersExceededWarning";
      w.target = this;
      w.type = type;
      w.count = size;
      process.emitWarning(w);
    }
  }
  [kRemoveListener]() {}
  addEventListener(type, listener, options = {}) {
    if (!isEventTarget(this)) throw invalidThis("EventTarget");
    if (arguments.length < 2) throw missingArgs(["type", "listener"]);
    const { once, capture, passive, signal, weak, nodeStyle, resist } = eventListenerOptions(options);
    if (signal !== undefined && (signal === null || typeof signal !== "object" || !("aborted" in signal))) {
      throw invalidArgType("options.signal", "an instance of AbortSignal", signal);
    }
    if (!validateEventListener(listener, "listener")) {
      const w = new Error(`addEventListener called with ${listener} which has no effect.`);
      w.name = "AddEventListenerArgumentTypeWarning";
      w.target = this;
      w.type = type;
      process.emitWarning(w);
      return;
    }
    const key = `${type}`;
    if (signal) {
      if (signal.aborted) return;
      signal.addEventListener("abort", () => {
        this.removeEventListener(key, listener, { capture });
      }, { once: true, [kWeakHandler]: this, [kResistStopPropagation]: true });
    }
    let list = this[kEvents].get(key);
    if (list !== undefined) {
      for (let i = list.length - 1; i >= 0; i--) {
        if (list[i].weak && list[i].callback.deref() === undefined) {
          list[i].removed = true;
          list.splice(i, 1);
        }
      }
    }
    if (list === undefined || list.length === 0) {
      list = [];
      this[kEvents].set(key, list);
    } else if (list.some((l) => entryCallback(l) === listener && l.capture === capture)) {
      return;
    }
    const entry = {
      callback: weak ? new WeakRef(listener) : listener,
      weak: weak !== undefined && weak !== null,
      capture, once, passive, nodeStyle, resist, removed: false,
    };
    if (entry.weak) {
      const owner = Object(weak);
      let held = keepAlive.get(owner);
      if (held === undefined) keepAlive.set(owner, (held = new Set()));
      held.add(listener);
    }
    list.push(entry);
    this[kNewListener](list.length, key, listener, once, capture, passive, entry.weak);
  }
  removeEventListener(type, listener, options = {}) {
    if (!isEventTarget(this)) throw invalidThis("EventTarget");
    if (arguments.length < 2) throw missingArgs(["type", "listener"]);
    const key = `${type}`;
    const capture = typeof options === "boolean" ? options : !!options?.capture;
    const list = this[kEvents].get(key);
    if (list === undefined) return;
    const i = list.findIndex((l) => entryCallback(l) === listener && l.capture === capture);
    if (i < 0) return;
    list[i].removed = true;
    list.splice(i, 1);
    if (list.length === 0) this[kEvents].delete(key);
    this[kRemoveListener](list.length, key, listener, capture);
  }
  dispatchEvent(event) {
    if (!isEventTarget(this)) throw invalidThis("EventTarget");
    if (arguments.length < 1) throw missingArgs(["event"]);
    if (!(event instanceof Event)) throw invalidArgType("event", "an instance of Event", event);
    if (event[kDispatching]) {
      throw codedError(Error, "ERR_EVENT_RECURSION", `The event "${event.type}" is already being dispatched`);
    }
    this[kHybridDispatch](event, event.type, event);
    return event.defaultPrevented !== true;
  }
  [kCreateEvent](nodeValue, type) {
    return new NodeCustomEvent(type, { detail: nodeValue });
  }
  [kHybridDispatch](nodeValue, type, event) {
    const path = [this];
    let parent = this.parentNode;
    while (parent && isEventTarget(parent)) {
      if (path.includes(parent)) throw new DOMException("Cyclic event target tree", "HierarchyRequestError");
      path.push(parent);
      parent = parent.parentNode;
    }
    const view = path[path.length - 1]?.defaultView;
    if (view && isEventTarget(view) && !path.includes(view)) path.push(view);
    const createEvent = () => {
      if (event === undefined) {
        event = this[kCreateEvent](nodeValue, type);
        event[kTarget] = this;
        event[kDispatching] = true;
      }
      event[kEventState].path = path;
      return event;
    };
    if (event !== undefined) {
      event[kTarget] = this;
      event[kDispatching] = true;
      event[kStop] = false;
      event[kEventState].propagationStopped = false;
      event[kEventState].path = path;
    }
    const invoke = (target, capture, phase) => {
    const list = target[kEvents].get(type);
    if (list === undefined || list.length === 0) return;
    for (const entry of [...list]) {
      if (event?.[kStop] === true && !entry.resist) break;
      if (entry.removed) continue;
      if (entry.capture !== capture) continue;
      const callback = entryCallback(entry);
      if (callback === undefined) {
        target.removeEventListener(type, undefined, { capture: entry.capture });
        continue;
      }
      if (entry.once) {
        entry.removed = true;
        const live = target[kEvents].get(type);
        const at = live?.indexOf(entry) ?? -1;
        if (at >= 0) {
          live.splice(at, 1);
          if (live.length === 0) target[kEvents].delete(type);
          target[kRemoveListener](live.length, type, callback, entry.capture);
        }
      }
      try {
        const arg = entry.nodeStyle ? nodeValue : createEvent();
        if (event !== undefined) {
          event[kEventState].currentTarget = target;
          event[kEventState].phase = phase;
          event[kEventState].passive = entry.passive;
        }
        let result;
        if (typeof callback === "function") result = Reflect.apply(callback, target, [arg]);
        else result = callback.handleEvent(arg);
        if (result !== undefined && result !== null && typeof result.then === "function") {
          result.then(undefined, reportUncaught);
        }
      } catch (error) {
        reportUncaught(error);
      } finally {
        if (event !== undefined) event[kEventState].passive = false;
      }
    }
    };
    try {
      for (let i = path.length - 1; i > 0; i--) {
        invoke(path[i], true, Event.CAPTURING_PHASE);
        if (event?.[kEventState].propagationStopped) break;
      }
      if (!event?.[kEventState].propagationStopped) {
        invoke(this, true, Event.AT_TARGET);
        if (!event?.[kStop]) invoke(this, false, Event.AT_TARGET);
      }
      if (event?.bubbles && !event[kEventState].propagationStopped) {
        for (let i = 1; i < path.length; i++) {
          invoke(path[i], false, Event.BUBBLING_PHASE);
          if (event[kEventState].propagationStopped) break;
        }
      }
    } finally {
      if (event !== undefined) {
        event[kDispatching] = false;
        event[kEventState].currentTarget = null;
        event[kEventState].phase = Event.NONE;
        event[kEventState].path = [];
      }
    }
    return true;
  }
  [inspectCustom](depth, options, inspect) {
    if (!isEventTarget(this)) throw invalidThis("EventTarget");
    return inspectObject(this, {}, depth, options, inspect, true);
  }
}
Object.defineProperty(EventTarget.prototype, Symbol.toStringTag, { value: "EventTarget", configurable: true });
for (const name of ["addEventListener", "removeEventListener", "dispatchEvent"]) {
  Object.defineProperty(EventTarget.prototype, name, { enumerable: true });
}

function makeEventHandler(handler) {
  function eventHandler(...args) {
    if (typeof eventHandler.handler !== "function") return undefined;
    return Reflect.apply(eventHandler.handler, this, args);
  }
  eventHandler.handler = handler;
  return eventHandler;
}

function defineNodeEventHandler(emitter, name, event = name) {
  Object.defineProperty(emitter, `on${name}`, {
    __proto__: null,
    get() {
      return this[kHandlers]?.get(event)?.handler ?? null;
    },
    set(value) {
      if (!this[kHandlers]) {
        Object.defineProperty(this, kHandlers, { value: new Map(), writable: true, configurable: true });
      }
      const handler = typeof value === "function" ? value : null;
      let wrapped = this[kHandlers].get(event);
      if (wrapped) {
        wrapped.handler = handler;
      } else {
        wrapped = makeEventHandler(handler);
        this[kHandlers].set(event, wrapped);
        this.addEventListener(event, wrapped);
      }
    },
    configurable: true,
    enumerable: true,
  });
}

const isNodeEventTarget = (value) => isEventTarget(value) && typeof value.emit === "function" && typeof value.on === "function";

class NodeEventTarget extends EventTarget {
  static get defaultMaxListeners() {
    return defaultMaxListeners();
  }
  setMaxListeners(n) {
    if (!isNodeEventTarget(this)) throw invalidThis("NodeEventTarget");
    const emitter = globalThis[Symbol.for("lumen.EventEmitter")];
    if (emitter) emitter.setMaxListeners(n, this);
    else this[kMaxEventTargetListeners] = n;
    return this;
  }
  getMaxListeners() {
    if (!isNodeEventTarget(this)) throw invalidThis("NodeEventTarget");
    return this[kMaxEventTargetListeners];
  }
  eventNames() {
    if (!isNodeEventTarget(this)) throw invalidThis("NodeEventTarget");
    return [...this[kEvents].keys()];
  }
  listenerCount(type) {
    if (!isNodeEventTarget(this)) throw invalidThis("NodeEventTarget");
    return this[kEvents].get(String(type))?.length ?? 0;
  }
  off(type, listener, options) {
    if (!isNodeEventTarget(this)) throw invalidThis("NodeEventTarget");
    this.removeEventListener(type, listener, options);
    return this;
  }
  removeListener(type, listener, options) {
    if (!isNodeEventTarget(this)) throw invalidThis("NodeEventTarget");
    this.removeEventListener(type, listener, options);
    return this;
  }
  on(type, listener) {
    if (!isNodeEventTarget(this)) throw invalidThis("NodeEventTarget");
    this.addEventListener(type, listener, { [kIsNodeStyleListener]: true });
    return this;
  }
  addListener(type, listener) {
    if (!isNodeEventTarget(this)) throw invalidThis("NodeEventTarget");
    this.addEventListener(type, listener, { [kIsNodeStyleListener]: true });
    return this;
  }
  emit(type, arg) {
    if (!isNodeEventTarget(this)) throw invalidThis("NodeEventTarget");
    if (typeof type !== "string") throw invalidArgType("type", "of type string", type);
    const hadListeners = this.listenerCount(type) > 0;
    this[kHybridDispatch](arg, type);
    return hadListeners;
  }
  once(type, listener) {
    if (!isNodeEventTarget(this)) throw invalidThis("NodeEventTarget");
    this.addEventListener(type, listener, { once: true, [kIsNodeStyleListener]: true });
    return this;
  }
  removeAllListeners(type) {
    if (!isNodeEventTarget(this)) throw invalidThis("NodeEventTarget");
    const keys = type !== undefined ? [String(type)] : [...this[kEvents].keys()];
    for (const key of keys) {
      for (const entry of this[kEvents].get(key) ?? []) entry.removed = true;
      this[kEvents].delete(key);
    }
    return this;
  }
}
for (const name of ["setMaxListeners", "getMaxListeners", "eventNames", "listenerCount", "off", "removeListener",
  "on", "addListener", "emit", "once", "removeAllListeners"]) {
  Object.defineProperty(NodeEventTarget.prototype, name, { enumerable: true });
}

const kSignalCreate = Symbol("AbortSignal-internal-create");
const kSourceSignals = Symbol("kSourceSignals");
const kDependantSignals = Symbol("kDependantSignals");
const kComposite = Symbol("kComposite");
const kAborted = Symbol("kAborted");
const kReason = Symbol("kReason");

const kTimeout = Symbol("kTimeout");
const gcPersistentSignals = new Set();

const isAbortSignal = (value) => typeof value === "object" && value !== null && kAborted in value;

function abortSignal(signal, reason) {
  if (signal[kAborted]) return;
  signal[kAborted] = true;
  signal[kReason] = reason;
  gcPersistentSignals.delete(signal);
  const event = new Event("abort", { [kTrustEvent]: true });
  signal.dispatchEvent(event);
  const dependants = signal[kDependantSignals];
  if (dependants) for (const dependant of [...dependants]) abortSignal(dependant, reason);
}

function defaultAbortReason() {
  return new DOMException("This operation was aborted", "AbortError");
}

class AbortSignal extends EventTarget {
  constructor(token) {
    if (token !== kSignalCreate) throw codedError(TypeError, "ERR_ILLEGAL_CONSTRUCTOR", "Illegal constructor");
    super();
    Object.defineProperty(this, kAborted, { value: false, writable: true, configurable: true });
    Object.defineProperty(this, kReason, { value: undefined, writable: true, configurable: true });
    Object.defineProperty(this, kComposite, { value: false, writable: true, configurable: true });
  }
  get aborted() {
    if (!isAbortSignal(this)) throw invalidThis("AbortSignal");
    return !!this[kAborted];
  }
  get reason() {
    if (!isAbortSignal(this)) throw invalidThis("AbortSignal");
    return this[kReason];
  }
  throwIfAborted() {
    if (!isAbortSignal(this)) throw invalidThis("AbortSignal");
    if (this[kAborted]) throw this[kReason];
  }
  [kNewListener](size, type, listener, once, capture, passive, weak) {
    super[kNewListener](size, type, listener, once, capture, passive, weak);
    if (this[kTimeout] === true && type === "abort" && !this[kAborted] && !weak && size === 1) {
      gcPersistentSignals.add(this);
    }
  }
  [kRemoveListener](size, type, listener, capture) {
    super[kRemoveListener](size, type, listener, capture);
    if (this[kTimeout] === true && type === "abort" && size === 0) gcPersistentSignals.delete(this);
  }
  [inspectCustom](depth, options, inspect) {
    return inspectObject(this, { aborted: this.aborted }, depth, options, inspect);
  }
  static abort(reason = defaultAbortReason()) {
    const signal = new AbortSignal(kSignalCreate);
    abortSignal(signal, reason);
    return signal;
  }
  static timeout(delay) {
    if (typeof delay !== "number") throw invalidArgType("delay", "of type number", delay);
    if (!Number.isInteger(delay) || delay < 0 || delay > 4294967295) {
      throw codedError(RangeError, "ERR_OUT_OF_RANGE",
        `The value of "delay" is out of range. It must be >= 0 && <= 4294967295. Received ${String(delay)}`);
    }
    const signal = new AbortSignal(kSignalCreate);
    signal[kTimeout] = true;
    const ref = new WeakRef(signal);
    const timer = setTimeout(() => {
      const target = ref.deref();
      if (target !== undefined) {
        abortSignal(target, new DOMException("The operation was aborted due to timeout", "TimeoutError"));
      }
    }, delay);
    if (typeof timer?.unref === "function") timer.unref();
    return signal;
  }
  static any(signals) {
    if (!Array.isArray(signals)) {
      throw invalidArgType("signals", "an instance of Array", signals);
    }
    signals.forEach((s, i) => {
      if (!(s instanceof AbortSignal)) throw invalidArgType(`signals[${i}]`, "an instance of AbortSignal", s);
    });
    const result = new AbortSignal(kSignalCreate);
    result[kComposite] = true;
    if (signals.length === 0) return result;
    result[kSourceSignals] = new Set();
    for (const signal of signals) {
      if (signal.aborted) {
        abortSignal(result, signal.reason);
        return result;
      }
      signal[kDependantSignals] ??= new Set();
      if (!signal[kComposite]) {
        result[kSourceSignals].add(signal);
        signal[kDependantSignals].add(result);
      } else if (signal[kSourceSignals]) {
        for (const source of signal[kSourceSignals]) {
          if (result[kSourceSignals].has(source)) continue;
          result[kSourceSignals].add(source);
          source[kDependantSignals].add(result);
        }
      }
    }
    return result;
  }
}
Object.defineProperty(AbortSignal.prototype, Symbol.toStringTag, { value: "AbortSignal", configurable: true });
defineNodeEventHandler(AbortSignal.prototype, "abort");
for (const name of ["aborted", "reason", "throwIfAborted"]) {
  Object.defineProperty(AbortSignal.prototype, name, { enumerable: true });
}
for (const name of ["abort", "timeout", "any"]) {
  Object.defineProperty(AbortSignal, name, { enumerable: true });
}

const kControllerSignal = Symbol("kControllerSignal");
const isAbortController = (value) => typeof value === "object" && value !== null && kControllerSignal in value;

class AbortController {
  constructor() {
    Object.defineProperty(this, kControllerSignal, { value: new AbortSignal(kSignalCreate) });
  }
  get signal() {
    if (!isAbortController(this)) throw invalidThis("AbortController");
    return this[kControllerSignal];
  }
  abort(reason = defaultAbortReason()) {
    if (!isAbortController(this)) throw invalidThis("AbortController");
    abortSignal(this[kControllerSignal], reason);
  }
  [inspectCustom](depth, options, inspect) {
    return inspectObject(this, { signal: this.signal }, depth, options, inspect);
  }
}
Object.defineProperty(AbortController.prototype, Symbol.toStringTag, { value: "AbortController", configurable: true });
for (const name of ["signal", "abort"]) {
  Object.defineProperty(AbortController.prototype, name, { enumerable: true });
}

const kTransferableSignal = Symbol.for("nodejs.abortsignal.transferable");

// The copy of a transferable signal that arrives through a MessagePort: aborted already if the
// original is, otherwise aborted by a later turn of the loop (as the port message would be).
function cloneTransferableSignal(signal) {
  const clone = new AbortSignal(kSignalCreate);
  Object.defineProperty(clone, kTransferableSignal, { value: true });
  if (signal.aborted) {
    abortSignal(clone, signal.reason);
  } else {
    signal.addEventListener("abort", () => {
      const timer = typeof setImmediate === "function"
        ? setImmediate(() => abortSignal(clone, signal.reason))
        : setTimeout(() => abortSignal(clone, signal.reason), 0);
      if (typeof timer?.unref === "function") timer.unref();
    }, { once: true });
  }
  return clone;
}

Object.defineProperty(globalThis, "__cloneTransferableSignal", { value: cloneTransferableSignal, configurable: true });
const eventTargetInternals = {
  Event, get CustomEvent() { return globalThis.CustomEvent; }, EventTarget, NodeEventTarget, kEvents, kWeakHandler, kResistStopPropagation, kTrustEvent,
  kNewListener, kRemoveListener, kCreateEvent, kHybridDispatch, kIsNodeStyleListener, kMaxEventTargetListeners,
  kMaxEventTargetListenersWarned, defineEventHandler: defineNodeEventHandler, initEventTarget, isEventTarget, isNodeEventTarget,
  isAbortSignal, isEvent, kTarget, kDispatching, kStop, createAbortSignal: () => new AbortSignal(kSignalCreate),
  abortSignal,
  hasListeners(target) {
    if (!target[kEvents] && typeof __dom_event_targets !== 'undefined') {
      const inactive = [];
      if (target[kHandlers]) for (const handler of target[kHandlers].values())
        if (typeof handler.handler !== 'function') inactive.push(handler);
      return __dom_event_targets.hasListeners(target, inactive);
    }
    for (const [type, entries] of target[kEvents]) {
      const handler = target[kHandlers]?.get(type);
      for (const entry of entries) {
        if (entry.removed) continue;
        const callback = entryCallback(entry);
        if (callback && (callback !== handler || typeof handler.handler === "function")) return true;
      }
    }
    return false;
  },
};
Object.defineProperties(eventTargetInternals, {
  codedError: { value: codedError },
  invalidThis: { value: invalidThis },
});
Object.defineProperty(globalThis, "__eventTargetInternals", {
  value: eventTargetInternals,
  configurable: true,
});
globalThis.DOMException = DOMException;
globalThis.Event = Event;
globalThis.EventTarget = EventTarget;
globalThis.AbortSignal = AbortSignal;
globalThis.AbortController = AbortController;
