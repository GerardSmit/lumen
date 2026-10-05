// SharedWorker facades. The native host owns identity and routes structured
// clone parcels between isolated document/worker Engines.
const sharedWorkerParseJSON = JSON.parse;
function sharedWorkerTransferList(value) {
  if (value === undefined) return undefined;
  if (value !== null && (typeof value === "object" || typeof value === "function") &&
      typeof value[Symbol.iterator] !== "function") return value.transfer;
  return value;
}
function sharedWorkerPortPost(port, data, transfer) {
  const rawList = sharedWorkerTransferList(transfer);
  const list = [];
  if (rawList !== undefined) for (const item of rawList) list.push(item);
  const result = __sharedWorker.port_post(port._endpoint, port._side, data, list);
  if (list && typeof list[Symbol.iterator] === "function") {
    for (const item of list) if (item && typeof item._detach === "function") item._detach();
  }
  return result;
}
function sharedWorkerRevivePorts(value, attachments, createPort) {
  const ports = [];
  for (let index = 0; index < attachments.length; index++) {
    const attachment = attachments[index];
    ports.push(createPort(attachment[0], attachment[1]));
  }
  const seen = new Map();
  function revive(current) {
    if (current === null || (typeof current !== "object" && typeof current !== "function")) return current;
    if (Object.prototype.hasOwnProperty.call(current, "__lumenHostAttachmentIndex")) {
      const index = current.__lumenHostAttachmentIndex;
      if (Number.isInteger(index) && index >= 0 && index < ports.length) return ports[index];
    }
    if (seen.has(current)) return seen.get(current);
    seen.set(current, current);
    if (current instanceof Map) {
      for (const [key, item] of current) {
        const nextKey = revive(key), nextItem = revive(item);
        if (nextKey !== key || nextItem !== item) { current.delete(key); current.set(nextKey, nextItem); }
      }
    } else if (current instanceof Set) {
      const items = Array.from(current, revive); current.clear(); for (const item of items) current.add(item);
    } else {
      for (const key of Object.keys(current)) {
        const descriptor = Object.getOwnPropertyDescriptor(current, key);
        if (descriptor && "value" in descriptor) current[key] = revive(descriptor.value);
      }
    }
    return current;
  }
  return { data: revive(value), ports };
}

if (typeof __sharedWorker !== "undefined" && globalThis.__bitnestIsSharedWorker === true) {
  globalThis.__bitnestSharedWorkerOps = __sharedWorker;
  const scopeTarget = new EventTarget();
  globalThis.self = globalThis;
  globalThis.addEventListener = scopeTarget.addEventListener.bind(scopeTarget);
  globalThis.removeEventListener = scopeTarget.removeEventListener.bind(scopeTarget);
  globalThis.dispatchEvent = scopeTarget.dispatchEvent.bind(scopeTarget);
  const ports = new Map();
  const portToken = Symbol("MessagePort");
  class MessagePort extends EventTarget {
    constructor(token) { super(); if (token !== portToken) throw new TypeError("Illegal constructor"); }
  }
  class SharedWorkerPort extends MessagePort {
    constructor(endpoint, side) { super(portToken); this._endpoint = endpoint; this._side = side; this._onmessage = null; this._onmessageerror = null; this._started = false; __sharedWorker.register_port_object(this, endpoint, side); }
    get onmessage() { return this._onmessage; }
    set onmessage(handler) { this._onmessage = typeof handler === "function" ? handler : null; if (this._onmessage) this.start(); }
    get onmessageerror() { return this._onmessageerror; }
    set onmessageerror(handler) { this._onmessageerror = typeof handler === "function" ? handler : null; if (this._onmessageerror) this.start(); }
    postMessage(data, transfer) { return sharedWorkerPortPost(this, data, transfer); }
    start() { if (!this._started) { this._started = true; __sharedWorker.port_start(this._endpoint, this._side); } }
    close() { if (this._endpoint !== null) { __sharedWorker.port_close(this._endpoint, this._side); ports.delete(`${this._endpoint}:${this._side}`); this._endpoint = null; } }
    _dispatch(data, attachments = []) { let restored; try { restored = sharedWorkerRevivePorts(data, attachments, portFor); } catch (_) { this._dispatchError(); return; } const event = new Event("message"); event.data = restored.data; event.ports = restored.ports; this.dispatchEvent(event); if (typeof this.onmessage === "function") this.onmessage.call(this, event); }
    _dispatchError() { const event = new Event("messageerror"); this.dispatchEvent(event); if (typeof this.onmessageerror === "function") this.onmessageerror.call(this, event); }
    _detach() { if (this._endpoint !== null) ports.delete(`${this._endpoint}:${this._side}`); this._endpoint = null; this._started = false; }
  }
  function portFor(endpoint, side) {
    const key = `${endpoint}:${side}`;
    let port = ports.get(key);
    if (!port) { port = new SharedWorkerPort(endpoint, side); ports.set(key, port); }
    return port;
  }
  Object.defineProperty(globalThis, "MessagePort", { configurable: true, value: MessagePort });
  globalThis.__bitnestDispatchSharedPortMessage = (endpoint, side, data, attachments = []) => {
    const port = portFor(endpoint, side);
    if (!port || typeof port._dispatch !== "function") throw new TypeError("MessagePort receiver is unavailable");
    return port._dispatch(data, attachments);
  };
  globalThis.__bitnestDispatchSharedPortMessageError = (endpoint, side) => ports.get(`${endpoint}:${side}`)?._dispatchError();
  globalThis.__bitnestSharedWorkerPort = (endpoint, side) => {
    return portFor(endpoint, side);
  };
  globalThis.__bitnestDispatchSharedConnect = endpoint => {
    const port = globalThis.__bitnestSharedWorkerPort(endpoint, 1);
    __sharedWorker.port_start(endpoint, 1);
    const event = new Event("connect"); event.ports = [port];
    globalThis.dispatchEvent(event);
    if (typeof globalThis.onconnect === "function") globalThis.onconnect.call(globalThis, event);
  };
  globalThis.close = () => __sharedWorker.worker_close();
} else if (typeof __sharedWorker !== "undefined") {
  const workers = new Map();
  const ports = new Map();
  const portToken = Symbol("MessagePort");
  class MessagePort extends EventTarget {
    constructor(token) { super(); if (token !== portToken) throw new TypeError("Illegal constructor"); }
  }
  class SharedWorkerPort extends MessagePort {
    constructor(endpoint, side) { super(portToken); this._endpoint = endpoint; this._side = side; this._onmessage = null; this._onmessageerror = null; this._started = false; __sharedWorker.register_port_object(this, endpoint, side); }
    get onmessage() { return this._onmessage; }
    set onmessage(handler) { this._onmessage = typeof handler === "function" ? handler : null; if (this._onmessage) this.start(); }
    get onmessageerror() { return this._onmessageerror; }
    set onmessageerror(handler) { this._onmessageerror = typeof handler === "function" ? handler : null; if (this._onmessageerror) this.start(); }
    postMessage(data, transfer) { return sharedWorkerPortPost(this, data, transfer); }
    start() { if (!this._started) { this._started = true; __sharedWorker.port_start(this._endpoint, this._side); } }
    close() { if (this._endpoint !== null) { __sharedWorker.port_close(this._endpoint, this._side); ports.delete(`${this._endpoint}:${this._side}`); this._endpoint = null; } }
    _dispatch(data, attachments = []) { let restored; try { restored = sharedWorkerRevivePorts(data, attachments, portFor); } catch (_) { this._dispatchError(); return; } const event = new Event("message"); event.data = restored.data; event.ports = restored.ports; this.dispatchEvent(event); if (typeof this.onmessage === "function") this.onmessage.call(this, event); }
    _dispatchError() { const event = new Event("messageerror"); this.dispatchEvent(event); if (typeof this.onmessageerror === "function") this.onmessageerror.call(this, event); }
    _detach() { if (this._endpoint !== null) ports.delete(`${this._endpoint}:${this._side}`); this._endpoint = null; this._started = false; }
  }
  function portFor(endpoint, side) {
    const key = `${endpoint}:${side}`;
    let port = ports.get(key);
    if (!port) { port = new SharedWorkerPort(endpoint, side); ports.set(key, port); }
    return port;
  }
  Object.defineProperty(globalThis, "MessagePort", { configurable: true, value: MessagePort });
  globalThis.__bitnestSharedWorkerPort = (endpoint, side) => portFor(endpoint, side);
  class SharedWorker extends EventTarget {
    constructor(scriptURL, options = {}) {
      super();
      this.onerror = null;
      if (arguments.length === 0) throw new TypeError("SharedWorker requires a script URL");
      if (options === null || (typeof options !== "string" && typeof options !== "object")) throw new TypeError("SharedWorker options must be a string or object");
      const normalized = typeof options === "string" ? { name: options } : options;
      const type = normalized.type === undefined ? "classic" : String(normalized.type);
      if (type !== "classic" && type !== "module") throw new TypeError("invalid SharedWorker script type");
      const credentials = normalized.credentials === undefined ? "same-origin" : String(normalized.credentials);
      if (credentials !== "omit" && credentials !== "same-origin" && credentials !== "include") throw new TypeError("invalid SharedWorker credentials mode");
      const url = new URL(String(scriptURL), location.href).href;
      const name = normalized.name === undefined ? "" : String(normalized.name);
      const connected = JSON.parse(__sharedWorker.connect(url, name, type === "module", credentials));
      this.port = globalThis.__bitnestSharedWorkerPort(connected.endpoint, 0);
      this.port.start();
      workers.set(Number(connected.id), this);
    }
  }
  globalThis.__bitnestDispatchSharedPortMessage = (endpoint, side, data, attachments = []) => {
    const port = portFor(endpoint, side);
    if (!port || typeof port._dispatch !== "function") throw new TypeError("MessagePort receiver is unavailable");
    return port._dispatch(data, attachments);
  };
  globalThis.__bitnestDispatchSharedPortMessageError = (endpoint, side) => ports.get(`${endpoint}:${side}`)?._dispatchError();
  globalThis.__bitnestDispatchSharedWorkerError = (id, message) => {
    const worker = workers.get(Number(id));
    if (worker) { const event = new Event("error"); event.message = String(message); worker.dispatchEvent(event); if (typeof worker.onerror === "function") worker.onerror.call(worker, event); }
  };
  Object.defineProperty(globalThis, "SharedWorker", { configurable: true, value: SharedWorker });
}

if (typeof __sharedWorker !== "undefined" && typeof globalThis.__bitnestSharedWorkerPort === "function") {
  const createChannel = __sharedWorker.message_channel;
  const createPort = globalThis.__bitnestSharedWorkerPort;
  const parseChannel = sharedWorkerParseJSON;
  class MessageChannel {
    constructor() {
      const channel = parseChannel(createChannel.call(__sharedWorker));
      this.port1 = createPort(channel.endpoint, 0);
      this.port2 = createPort(channel.endpoint, 1);
    }
  }
  Object.defineProperty(globalThis, "MessageChannel", { configurable: true, value: MessageChannel });
}
