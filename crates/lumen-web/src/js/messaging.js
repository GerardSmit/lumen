// Channel messaging (HTML §9.4) + the web event classes beyond the DOM core: MessageEvent,
// CloseEvent, PromiseRejectionEvent, MessageChannel/MessagePort (in-process
// entangled pair), BroadcastChannel (same-realm), and AbortSignal.any. Message data is
// serialized SYNCHRONOUSLY at postMessage time via structuredClone and delivered as a task;
// each BroadcastChannel receiver gets its own clone (mutation isolation, like the spec's
// per-destination deserialize). The subclasses extend the native Event and EventTarget
// (`lumen_host::events`); a browser embedder may replace those globals first.

function isMessagePortLike(value) {
  if (value === null || typeof value !== "object") return false;
  if (globalThis.__lumenPortClone?.isPort(value)) return true;
  return value instanceof MessagePort || (typeof globalThis.MessagePort === "function" && value instanceof globalThis.MessagePort);
}
function invalidPortArg(name, value) {
  const shown = value === null ? "null" : typeof value === "object" ? "an instance of Object" : `type ${typeof value} (${String(value)})`;
  const err = new TypeError(`The "${name}" property must be an instance of MessagePort. Received ${shown}`);
  err.code = "ERR_INVALID_ARG_TYPE";
  return err;
}

class MessageEvent extends globalThis.Event {
  constructor(type, init = {}) {
    super(type, init);
    init = init && typeof init === "object" ? init : {};
    this.data = init.data === undefined ? null : init.data;
    this.origin = init.origin === undefined ? "" : `${init.origin}`;
    this.lastEventId = init.lastEventId === undefined ? "" : `${init.lastEventId}`;
    const source = init.source === undefined ? null : init.source;
    if (source !== null && !isMessagePortLike(source)) throw invalidPortArg("init.source", source);
    this.source = source;
    const portsInit = init.ports;
    if (portsInit !== undefined && (portsInit === null || typeof portsInit[Symbol.iterator] !== "function")) {
      throw new TypeError("ports is not iterable");
    }
    const ports = portsInit === undefined ? [] : [...portsInit];
    for (let i = 0; i < ports.length; i++) {
      if (!isMessagePortLike(ports[i])) throw invalidPortArg(`init.ports[${i}]`, ports[i]);
    }
    this.ports = Object.freeze(ports);
  }
}

class CloseEvent extends globalThis.Event {
  constructor(type, init = {}) {
    super(type, init);
    init = init && typeof init === "object" ? init : {};
    const code = Number(init.code);
    this.wasClean = !!init.wasClean;
    this.code = Number.isFinite(code) ? code & 0xffff : 0;
    this.reason = "reason" in init ? String(init.reason) : "";
  }
}

class PromiseRejectionEvent extends globalThis.Event {
  constructor(type, init) {
    if (!init || typeof init !== "object" || !("promise" in init)) {
      throw new TypeError("PromiseRejectionEvent requires an init with a promise");
    }
    super(type, init);
    this.promise = init.promise;
    this.reason = "reason" in init ? init.reason : undefined;
  }
}

// Event-handler IDL attribute (`onmessage` and friends): the assigned function participates in
// dispatch as a real listener, so handler + addEventListener fire in registration order.
function defineEventHandler(proto, name, afterSet) {
  const listeners = new WeakMap();
  const handlers = new WeakMap();
  Object.defineProperty(proto, `on${name}`, {
    configurable: true,
    get() {
      return handlers.get(this) ?? null;
    },
    set(fn) {
      const old = listeners.get(this);
      if (old) this.removeEventListener(name, old);
      if (typeof fn === "function") {
        handlers.set(this, fn);
        const wrapped = (e) => fn.call(this, e);
        listeners.set(this, wrapped);
        this.addEventListener(name, wrapped);
      } else {
        handlers.delete(this);
        listeners.delete(this);
      }
      if (afterSet) afterSet(this);
    },
  });
}

const kPortCreate = Symbol("MessagePort-create");

class MessagePort extends globalThis.EventTarget {
  constructor(token) {
    if (token !== kPortCreate) throw new TypeError("Illegal constructor");
    super();
    this._other = null;
    this._queue = [];
    this._started = false;
    this._closed = false;
    this._nativeId = null;
    this._onNativeClose = null;
  }
  postMessage(message, options) {
    if (this._closed) return;
    if (this._nativeId !== null) {
      const transfer = Array.isArray(options)
        ? options
        : options && typeof options === "object" && options.transfer
          ? options.transfer
          : [];
      const bytes = withWebPortClone(() => globalThis.__serializeForClone(message, transfer, true));
      globalThis.__lumenPorts.post(this._nativeId, bytes);
      return;
    }
    const transfer = Array.isArray(options)
      ? options
      : options && typeof options === "object" && options.transfer
        ? options.transfer
        : [];
    // Serialize NOW (spec order — later mutations of `message` are invisible to the receiver);
    // a DataCloneError propagates to the caller.
    const data = transfer.length ? structuredClone(message, { transfer }) : structuredClone(message);
    const target = this._other;
    if (!target || target._closed) return;
    target._queue.push(data);
    target._schedule();
  }
  start() {
    if (this._started || this._closed) return;
    this._started = true;
    if (this._nativeId !== null) {
      globalThis.__lumenPorts.listen(this._nativeId, () => this._flushNative());
      return;
    }
    this._schedule();
  }
  close() {
    this._closed = true;
    this._queue.length = 0;
    if (this._nativeId !== null) {
      const id = this._nativeId;
      this._nativeId = null;
      globalThis.__lumenPorts.close(id);
      globalThis.__lumenPorts.detach(id);
      this._onNativeClose?.();
      this._onNativeClose = null;
      return;
    }
    if (this._other) this._other._other = null;
    this._other = null;
  }
  _schedule() { setTimeout(() => this._flush(), 0); }
  _flush() {
    if (this._closed || !this._started || !this._queue.length) return;
    this._dispatch(this._queue.shift());
    if (this._queue.length && !this._closed) this._schedule();
  }
  _flushNative() {
    if (this._closed || !this._started || this._nativeId === null) return;
    const id = this._nativeId;
    const bytes = globalThis.__lumenPorts.poll(id);
    if (bytes === false) {
      this._closed = true;
      this._nativeId = null;
      globalThis.__lumenPorts.detach(id);
      this._onNativeClose?.();
      this._onNativeClose = null;
      this.dispatchEvent(new globalThis.Event("close"));
      return;
    }
    if (bytes === undefined) return;
    globalThis.__lumenPorts.wake(id);
    try {
      const data = withWebPortClone(() => globalThis.__deserializeClone(bytes));
      this.dispatchEvent(new MessageEvent("message", { data }));
    } catch (error) {
      this.dispatchEvent(new MessageEvent("messageerror", { data: error }));
    }
  }
  _dispatch(data) {
    if (this._closed) return;
    this.dispatchEvent(new MessageEvent("message", { data }));
  }
}
defineEventHandler(MessagePort.prototype, "message", (port) => port.start());
defineEventHandler(MessagePort.prototype, "messageerror");

class MessageChannel {
  constructor() {
    let port1;
    let port2;
    if (globalThis.__lumenPorts?.pair) {
      const pair = globalThis.__lumenPorts.pair();
      port1 = globalThis.__lumenSharedPorts.create(pair.a);
      port2 = globalThis.__lumenSharedPorts.create(pair.b);
    } else {
      port1 = new MessagePort(kPortCreate);
      port2 = new MessagePort(kPortCreate);
      port1._other = port2;
      port2._other = port1;
    }
    this.port1 = port1;
    this.port2 = port2;
  }
}

Object.defineProperty(globalThis, "__lumenSharedPorts", {
  configurable: true,
  enumerable: false,
  value: {
    create(id, onClose = null) {
      const port = new MessagePort(kPortCreate);
      port._nativeId = Number(id);
      port._onNativeClose = onClose;
      return port;
    },
  },
});

// Node may have installed its own MessagePort bridge before this web module is loaded. For a
// browser MessagePort operation, temporarily present a bridge that recognizes web ports and
// delegates Node ports to the active bridge. Deserialization then creates the web wrapper in a
// web MessagePort's receive path, while Node serialization remains unchanged everywhere else.
let webPortClone;
const otherPortClone = () => globalThis.__lumenPortClone === webPortClone ? null : globalThis.__lumenPortClone;
const isWebPort = (value) => value instanceof MessagePort && value._nativeId !== null;
webPortClone = {
  isPort(value) { return isWebPort(value) || !!otherPortClone()?.isPort(value); },
  isUntransferable(value) { return !!otherPortClone()?.isUntransferable?.(value); },
  isUncloneable(value) { return !!otherPortClone()?.isUncloneable?.(value); },
  validate(port) {
    if (!isWebPort(port)) return otherPortClone()?.validate(port);
    if (port._closed || globalThis.__lumenPorts.isClosed(port._nativeId)) {
      throw new globalThis.DOMException("MessagePort in transfer list is already detached", "DataCloneError");
    }
  },
  export(port) { return isWebPort(port) ? globalThis.__lumenPorts.export(port._nativeId) : otherPortClone()?.export(port); },
  detach(port) {
    if (!isWebPort(port)) return otherPortClone()?.detach(port);
    const id = port._nativeId;
    port._nativeId = null;
    port._closed = true;
    port._queue.length = 0;
    port._onNativeClose = null;
    globalThis.__lumenPorts.detach(id);
  },
  import(index) { return globalThis.__lumenSharedPorts.create(globalThis.__lumenPorts.import(index)); },
};
if (!Object.getOwnPropertyDescriptor(globalThis, "__lumenPortClone")) {
  Object.defineProperty(globalThis, "__lumenPortClone", {
    configurable: true,
    enumerable: false,
    value: webPortClone,
  });
}
function withWebPortClone(callback) {
  const previous = Object.getOwnPropertyDescriptor(globalThis, "__lumenPortClone");
  if (previous?.value === webPortClone) return callback();
  Object.defineProperty(globalThis, "__lumenPortClone", {
    configurable: true,
    enumerable: false,
    writable: true,
    value: webPortClone,
  });
  try {
    return callback();
  } finally {
    if (previous) Object.defineProperty(globalThis, "__lumenPortClone", previous);
    else delete globalThis.__lumenPortClone;
  }
}

// Same-realm broadcast registry: name -> Set of live channels.
const broadcastChannels = new Map();

class BroadcastChannel extends globalThis.EventTarget {
  constructor(name) {
    if (arguments.length === 0) {
      throw new TypeError("BroadcastChannel requires a name");
    }
    super();
    this.name = String(name);
    this._closed = false;
    let set = broadcastChannels.get(this.name);
    if (!set) {
      set = new Set();
      broadcastChannels.set(this.name, set);
    }
    set.add(this);
  }
  postMessage(message) {
    if (this._closed) {
      throw new globalThis.DOMException("BroadcastChannel is closed", "InvalidStateError");
    }
    const data = structuredClone(message); // serialize now, once
    const peers = [...(broadcastChannels.get(this.name) ?? [])].filter(
      (c) => c !== this && !c._closed,
    );
    setTimeout(() => {
      for (const peer of peers) {
        if (peer._closed) continue;
        peer.dispatchEvent(new MessageEvent("message", { data: structuredClone(data) }));
      }
    }, 0);
  }
  close() {
    this._closed = true;
    const set = broadcastChannels.get(this.name);
    if (set) {
      set.delete(this);
      if (set.size === 0) broadcastChannels.delete(this.name);
    }
  }
}
defineEventHandler(BroadcastChannel.prototype, "message");
defineEventHandler(BroadcastChannel.prototype, "messageerror");

globalThis.MessageEvent = MessageEvent;
globalThis.CloseEvent = CloseEvent;
globalThis.PromiseRejectionEvent = PromiseRejectionEvent;
globalThis.MessagePort = MessagePort;
globalThis.MessageChannel = MessageChannel;
globalThis.BroadcastChannel = BroadcastChannel;
