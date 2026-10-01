// DOMException + the DOM event model, flattened: one target, no tree, no capture phase.

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

class Event {
  constructor(type, init = {}) {
    if (arguments.length === 0) {
      throw new TypeError("Event constructor requires a type");
    }
    init = init && typeof init === "object" ? init : {};
    this.type = String(type);
    this.bubbles = !!init.bubbles;
    this.cancelable = !!init.cancelable;
    this.composed = !!init.composed;
    this.defaultPrevented = false;
    this.target = null;
    this.currentTarget = null;
    this.eventPhase = Event.AT_TARGET;
    this.isTrusted = false;
    this.timeStamp = performance.now();
    this._propagationStopped = false;
    this._immediateStopped = false;
  }
  preventDefault() {
    if (this.cancelable) this.defaultPrevented = true;
  }
  stopPropagation() {
    this._propagationStopped = true;
  }
  stopImmediatePropagation() {
    this._propagationStopped = true;
    this._immediateStopped = true;
  }
}
for (const [i, name] of ["NONE", "CAPTURING_PHASE", "AT_TARGET", "BUBBLING_PHASE"].entries()) {
  Object.defineProperty(Event, name, { value: i, writable: false, configurable: false, enumerable: true });
}

class CustomEvent extends Event {
  constructor(type, init = {}) {
    super(type, init);
    this.detail = init && "detail" in init ? init.detail : null;
  }
}

class EventTarget {
  constructor() {
    this._listeners = new Map();
  }
  addEventListener(type, callback, options = {}) {
    if (arguments.length < 2) {
      const err = new TypeError('The "type" and "listener" arguments must be specified');
      err.code = "ERR_MISSING_ARGS";
      throw err;
    }
    let once = false, capture = false, signal, resist = false;
    if (typeof options === "boolean") {
      capture = options;
    } else if (options !== null && (typeof options === "object" || typeof options === "function")) {
      // Option getters run in this order, even for a null listener.
      once = !!options.once;
      capture = !!options.capture;
      void options.passive;
      signal = options.signal;
      resist = options[kResistStopPropagation] === true;
      if (signal !== undefined && (signal === null || typeof signal !== "object" || !("aborted" in signal))) {
        const err = new TypeError(`The "options.signal" property must be an instance of AbortSignal.`);
        err.code = "ERR_INVALID_ARG_TYPE";
        throw err;
      }
    } else if (options !== undefined) {
      const err = new TypeError('The "options" argument must be of type object.');
      err.code = "ERR_INVALID_ARG_TYPE";
      throw err;
    }
    if (callback === null || callback === undefined) return;
    if (typeof callback !== "function" && (callback === null || typeof callback !== "object")) {
      const err = new TypeError('The "listener" argument must be an instance of EventListener.');
      err.code = "ERR_INVALID_ARG_TYPE";
      throw err;
    }
    const key = String(type);
    let list = this._listeners.get(key);
    if (!list) {
      list = [];
      this._listeners.set(key, list);
    }
    if (list.some((l) => l.callback === callback && l.capture === capture)) return;
    if (signal) {
      if (signal.aborted) return;
      signal.addEventListener("abort", () => {
        this.removeEventListener(key, callback, { capture });
      }, { once: true, [kResistStopPropagation]: true });
    }
    list.push({ callback, capture, once, resist, removed: false });
  }
  removeEventListener(type, callback, options = {}) {
    if (arguments.length < 2) {
      const err = new TypeError('The "type" and "listener" arguments must be specified');
      err.code = "ERR_MISSING_ARGS";
      throw err;
    }
    const capture = typeof options === "boolean" ? options
      : options !== null && typeof options === "object" ? !!options.capture : false;
    const list = this._listeners.get(String(type));
    if (!list) return;
    const i = list.findIndex((l) => l.callback === callback && l.capture === capture);
    if (i >= 0) {
      list[i].removed = true;
      list.splice(i, 1);
    }
  }
  dispatchEvent(event) {
    if (!(event instanceof Event)) {
      throw new TypeError("dispatchEvent expects an Event");
    }
    event.target = this;
    event.currentTarget = this;
    const list = this._listeners.get(event.type);
    if (list) {
      for (const entry of [...list]) {
        if (event._immediateStopped && !list.some((l) => l.resist)) break;
        if (entry.removed) continue;
        if (entry.once) {
          this.removeEventListener(event.type, entry.callback, { capture: entry.capture });
        }
        try {
          if (typeof entry.callback === "function") {
            entry.callback.call(this, event);
          } else if (entry.callback && typeof entry.callback.handleEvent === "function") {
            entry.callback.handleEvent(event);
          }
        } catch (e) {
          // A listener throwing must not break dispatch (the spec "reports" the exception).
          console.error("Uncaught (in event listener)", e instanceof Error ? `${e.name}: ${e.message}` : String(e));
        }
      }
    }
    event.currentTarget = null;
    return !event.defaultPrevented;
  }
}

const kSignalCreate = Symbol("AbortSignal-internal-create");
const kSourceSignals = Symbol("kSourceSignals");
const kDependantSignals = Symbol("kDependantSignals");
const kComposite = Symbol("kComposite");

class AbortSignal extends EventTarget {
  constructor(token) {
    if (token !== kSignalCreate) throw new TypeError("Illegal constructor");
    super();
    this.aborted = false;
    this.reason = undefined;
    this.onabort = null;
    this[kComposite] = false;
  }
  throwIfAborted() {
    if (this.aborted) throw this.reason;
  }
  _doAbort(reason) {
    if (this.aborted) return;
    this.aborted = true;
    this.reason =
      reason !== undefined
        ? reason
        : new DOMException("This operation was aborted", "AbortError");
    const event = new Event("abort");
    if (typeof this.onabort === "function") {
      try {
        this.onabort.call(this, event);
      } catch (e) {
        console.error("Uncaught (in onabort)", e instanceof Error ? `${e.name}: ${e.message}` : String(e));
      }
    }
    this.dispatchEvent(event);
    const dependants = this[kDependantSignals];
    if (dependants) for (const dependant of [...dependants]) dependant._doAbort(this.reason);
  }
  static abort(reason) {
    const signal = new AbortSignal(kSignalCreate);
    signal._doAbort(reason);
    return signal;
  }
  static timeout(ms) {
    if (typeof ms !== "number") {
      const err = new TypeError('The "delay" argument must be of type number.');
      err.code = "ERR_INVALID_ARG_TYPE";
      throw err;
    }
    const controller = new AbortController();
    const timer = setTimeout(
      () => controller.abort(new DOMException("The operation was aborted due to timeout", "TimeoutError")),
      ms,
    );
    if (typeof timer?.unref === "function") timer.unref();
    return controller.signal;
  }
  static any(signals) {
    if (!Array.isArray(signals)) {
      const err = new TypeError('The "signals" argument must be an instance of Array.');
      err.code = "ERR_INVALID_ARG_TYPE";
      throw err;
    }
    signals.forEach((s, i) => {
      if (!(s instanceof AbortSignal)) {
        const shown = s === undefined ? "undefined" : s === null ? "null"
          : typeof s === "object" ? `an instance of ${s.constructor?.name ?? "Object"}`
          : typeof s === "function" ? `function ${s.name}` : `type ${typeof s} (${String(s)})`;
        const err = new TypeError(`The "signals[${i}]" argument must be an instance of AbortSignal. Received ${shown}`);
        err.code = "ERR_INVALID_ARG_TYPE";
        throw err;
      }
    });
    const result = new AbortSignal(kSignalCreate);
    result[kComposite] = true;
    if (signals.length === 0) return result;
    result[kSourceSignals] = new Set();
    for (const signal of signals) {
      if (signal.aborted) {
        result._doAbort(signal.reason);
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

class AbortController {
  constructor() {
    this.signal = new AbortSignal(kSignalCreate);
  }
  abort(reason) {
    this.signal._doAbort(reason);
  }
}

const kTransferableSignal = Symbol.for("nodejs.abortsignal.transferable");

// The copy of a transferable signal that arrives through a MessagePort: aborted already if the
// original is, otherwise aborted by a later turn of the loop (as the port message would be).
function cloneTransferableSignal(signal) {
  const clone = new AbortSignal(kSignalCreate);
  Object.defineProperty(clone, kTransferableSignal, { value: true });
  if (signal.aborted) {
    clone._doAbort(signal.reason);
  } else {
    signal.addEventListener("abort", () => {
      const timer = setTimeout(() => clone._doAbort(signal.reason), 0);
      if (typeof timer?.unref === "function") timer.unref();
    }, { once: true });
  }
  return clone;
}

globalThis.DOMException = DOMException;
globalThis.Event = Event;
globalThis.CustomEvent = CustomEvent;
globalThis.EventTarget = EventTarget;
globalThis.AbortSignal = AbortSignal;
globalThis.AbortController = AbortController;
