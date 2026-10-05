// node:worker_threads — REAL workers: one OS thread + one fresh engine realm per Worker, over the
// runtime's __worker/__wself ops (lumen-runtime/src/worker.rs). Messages cross thread boundaries
// as structured-clone wire bytes (__serializeForClone, lumen-web/src/js/serialize.js) through the
// native MessagePort endpoints (lumen-runtime/src/ports.rs), which also carry transferred ports.
//
// Each Worker is wired like Node's: a public channel (worker 'message' <-> parentPort) and an
// internal one carrying the worker's stdio, its uncaught errors and its stdin. The worker's
// process.stdout/stderr are streams over that channel, piped to the parent's process streams
// unless the stdout/stderr options ask for them as worker.stdout/stderr.
//
// Honest throws (semantics lumen cannot honor): SHARE_ENV (realms snapshot the env; there is no
// shared store), unsupported transfer types/resizable buffers, moving a port into another vm
// context, postMessageToThread.
{
  const EventEmitter = __builtins.get("events");
  const pathMod = __builtins.get("path");
  const inspectCustom = Symbol.for("nodejs.util.inspect.custom");
  const kUntransferable = Symbol.for("nodejs.untransferable");
  const lazyUtil = () => __builtins.get("util");
  const lazyStream = () => __builtins.get("stream");

  const SHARE_ENV = Symbol.for("nodejs.worker_threads.SHARE_ENV");
  const environmentData = new Map();
  const uncloneable = new WeakSet();

  const nodeError = (Base, code, message) => __nodeError(Base, code, message);
  const ser = (v, transfer) => globalThis.__serializeForClone(v, transfer, true);
  const deser = (b) => globalThis.__deserializeClone(b);
  const ops = () => globalThis.__lumenPorts;

  // The runtime's worker extension installs after this glue and stashes its ops in a hidden
  // global (see WORKER_JS in lumen-runtime/src/worker.rs); grab them lazily on first use.
  let workerOps = null;
  function getWorkerOps() {
    if (workerOps === null) workerOps = globalThis.__lumenWorkerOps ?? null;
    if (workerOps === null) {
      throw new Error("worker_threads Worker requires the lumen runtime (worker ops not installed)");
    }
    return workerOps;
  }

  // ---- MessagePort ------------------------------------------------------------------------------
  // A NodeEventTarget: one listener list per type holds both DOM listeners (called with the
  // event) and Node-style ones (called with the event's value: a message's data, an emit's
  // argument). A listener that throws is an uncaught exception, as in Node.

  // Browser embedders can replace the global EventTarget after this Node module loads. Node
  // MessagePorts must keep using the constructor whose prototype their methods and listener
  // internals were built against; a later DOM EventTarget has different instance state.
  // The DOM adapter may replace these globals before node:worker_threads is first required. Keep
  // Node ports tied to the constructors and private state created by the shared event unit.
  const internals = __eventTargetInternals;
  const NodeEvent = internals.Event;
  const NodeEventTarget = internals.NodeEventTarget;
  // Events must satisfy `instanceof MessageEvent` whenever the global MessageEvent is built on
  // the same Event this module uses; a browser adapter may have replaced the global one.
  const WebMessageEvent = globalThis.MessageEvent;
  const MessageEventBase =
    typeof WebMessageEvent === "function" && Object.getPrototypeOf(WebMessageEvent) === NodeEvent
      ? WebMessageEvent
      : NodeEvent;
  class NodeMessageEvent extends MessageEventBase {
    constructor(type, init = {}) {
      init = init && typeof init === "object" ? init : {};
      super(type, init.bubbles === undefined ? {} : { bubbles: init.bubbles });
      this.data = init.data === undefined ? null : init.data;
      this.origin = init.origin === undefined ? "" : `${init.origin}`;
      this.lastEventId = init.lastEventId === undefined ? "" : `${init.lastEventId}`;
      this.source = init.source === undefined ? null : init.source;
      this.ports = Object.freeze(init.ports === undefined ? [] : [...init.ports]);
    }
  }
  const ET = NodeEventTarget.prototype;
  const kEvents = internals.kEvents;
  const nodeStyle = new WeakMap(); // registered wrapper -> the user's listener
  const onceRemoved = new WeakMap(); // `once` listener -> the target's listener-removed hook
  const portState = new WeakMap();
  let receivedPorts = null; // ports created by the deserialization running now

  function listenerList(target, type) {
    return target[kEvents].get(String(type));
  }
  function rethrowAsync(error) {
    process.nextTick(() => {
      throw error;
    });
  }
  function hybridDispatch(target, type, nodeValue, makeEvent) {
    const list = listenerList(target, type);
    if (!list || list.length === 0) return false;
    let event;
    for (const entry of [...list]) {
      if (entry.removed) continue;
      if (entry.once) {
        ET.removeEventListener.call(target, type, entry.callback, { capture: entry.capture });
        onceRemoved.get(entry.callback)?.(target, String(type));
      }
      const original = nodeStyle.get(entry.callback);
      try {
        if (original !== undefined) {
          Reflect.apply(original, target, [nodeValue]);
        } else {
          if (event === undefined) {
            event = makeEvent();
            event[internals.kTarget] = target;
            event[internals.kDispatching] = true;
          }
          if (typeof entry.callback === "function") Reflect.apply(entry.callback, target, [event]);
          else entry.callback.handleEvent(event);
        }
      } catch (error) {
        rethrowAsync(error);
      }
      if (event !== undefined && event[internals.kStop]) break;
    }
    if (event !== undefined) event[internals.kDispatching] = false;
    return true;
  }
  function validateListener(fn) {
    if (typeof fn !== "function") throw new __errors.ERR_INVALID_ARG_TYPE("listener", "Function", fn);
  }
  function makeNodeTargetMethods(proto, onAdd, onRemove) {
    const addNode = function (type, listener, once) {
      validateListener(listener);
      const wrapper = function () {};
      nodeStyle.set(wrapper, listener);
      ET.addEventListener.call(this, type, wrapper, { once });
      if (once) onceRemoved.set(wrapper, onRemove);
      onAdd(this, String(type));
      return this;
    };
    const removeNode = function (type, listener) {
      const list = listenerList(this, type);
      if (!list) return this;
      for (let i = list.length - 1; i >= 0; i--) {
        if (nodeStyle.get(list[i].callback) === listener) {
          ET.removeEventListener.call(this, type, list[i].callback, { capture: list[i].capture });
          onRemove(this, String(type));
          break;
        }
      }
      return this;
    };
    const methods = {
      addEventListener(type, callback, options) {
        Reflect.apply(ET.addEventListener, this, arguments);
        if (callback != null) onAdd(this, String(type));
      },
      removeEventListener(type, callback, options) {
        Reflect.apply(ET.removeEventListener, this, arguments);
        onRemove(this, String(type));
      },
      dispatchEvent(event) {
        if (!(event instanceof NodeEvent)) throw new __errors.ERR_INVALID_ARG_TYPE("event", "Event", event);
        hybridDispatch(this, event.type, event, () => event);
        return !event.defaultPrevented;
      },
      on(type, listener) { return addNode.call(this, type, listener, false); },
      addListener(type, listener) { return addNode.call(this, type, listener, false); },
      once(type, listener) { return addNode.call(this, type, listener, true); },
      off(type, listener) { return removeNode.call(this, type, listener); },
      removeListener(type, listener) { return removeNode.call(this, type, listener); },
      emit(type, arg) {
        return hybridDispatch(this, type, arg, () => {
          const event = new NodeEvent(type);
          event.detail = arg;
          return event;
        });
      },
      removeAllListeners(type) {
        const types = type === undefined ? [...this[kEvents].keys()] : [String(type)];
        for (const t of types) {
          const list = this[kEvents].get(t);
          if (!list) continue;
          for (const entry of [...list]) ET.removeEventListener.call(this, t, entry.callback, { capture: entry.capture });
          onRemove(this, t);
        }
        return this;
      },
      listenerCount(type) {
        return listenerList(this, type)?.length ?? 0;
      },
      listeners(type) {
        return (listenerList(this, type) ?? []).map((e) => nodeStyle.get(e.callback) ?? e.callback);
      },
      eventNames() {
        return [...this[kEvents]].filter(([, list]) => list.length > 0).map(([t]) => t);
      },
      setMaxListeners(n) {
        portStateOf(this).maxListeners = n;
        return this;
      },
      getMaxListeners() {
        return portStateOf(this).maxListeners ?? EventEmitter.defaultMaxListeners;
      },
    };
    for (const name of Object.keys(methods)) {
      Object.defineProperty(proto, name, { value: methods[name], writable: true, configurable: true, enumerable: false });
    }
  }
  // `onmessage`-style attributes: a real listener, so it orders with addEventListener ones.
  function defineEventHandler(proto, name) {
    const handlers = new WeakMap();
    Object.defineProperty(proto, `on${name}`, {
      configurable: true,
      enumerable: true,
      get() {
        return handlers.get(this)?.fn ?? null;
      },
      set(fn) {
        const old = handlers.get(this);
        if (old) {
          handlers.delete(this);
          this.removeEventListener(name, old.wrapped);
        }
        if (typeof fn === "function" || (fn !== null && typeof fn === "object")) {
          const self = this;
          const wrapped = function (event) {
            return typeof fn === "function" ? Reflect.apply(fn, self, [event]) : undefined;
          };
          handlers.set(this, { fn, wrapped });
          this.addEventListener(name, wrapped);
        }
      },
    });
  }

  function portStateOf(port) {
    const state = portState.get(port);
    if (state === undefined) throw new __errors.ERR_INVALID_THIS("MessagePort");
    return state;
  }

  const MessagePortMethods = class MessagePort extends NodeEventTarget {
    postMessage(message, transfer) {
      const state = portStateOf(this);
      if (arguments.length === 0) {
        throw nodeError(TypeError, "ERR_MISSING_ARGS", "Not enough arguments to MessagePort.postMessage");
      }
      const list = readTransferList(transfer);
      if (list.includes(this)) throw new DOMException("Transfer list contains source port", "DataCloneError");
      if (state.destroyed || state.detached) {
        // Nothing reaches a closed port, but the message is still serialized (and its transfer
        // list detached), like Node.
        ser(message, list);
        return;
      }
      const result = ops().post(state.id, ser(message, list));
      if (result === "lost") {
        process.emitWarning("The target port was posted to itself, and the communication channel was lost");
      }
    }
    start() {
      const state = portStateOf(this);
      if (state.destroyed || state.started) return;
      state.started = true;
      ops().wake(state.id);
    }
    close(callback) {
      const state = portStateOf(this);
      if (typeof callback === "function") this.once("close", callback);
      if (state.destroyed || state.closing || state.detached) return;
      state.closingLocal = true;
      ops().close(state.id);
    }
    ref() {
      const state = portStateOf(this);
      if (state.destroyed || state.closing) return this;
      state.refed = true;
      ops().setRef(state.id, true);
      return this;
    }
    unref() {
      const state = portStateOf(this);
      if (state.destroyed) return this;
      state.refed = false;
      ops().setRef(state.id, false);
      return this;
    }
    hasRef() {
      const state = portStateOf(this);
      return !state.destroyed && !state.closing && !state.closingLocal && state.refed;
    }
    [inspectCustom](depth, options) {
      const state = portState.get(this);
      if (state === undefined) return this;
      const shown = state.destroyed ? { active: false } : { active: true, refed: this.hasRef() };
      for (const key of Object.keys(this)) shown[key] = this[key];
      if (depth < 0) return this;
      const opts = { ...options, depth: options.depth == null ? null : options.depth - 1 };
      return `${this.constructor.name} [EventTarget] ${lazyUtil().inspect(shown, opts)}`;
    }
  }
  function messagePortListenerAdded(port, type) {
    if (type !== "message") return;
    const state = portState.get(port);
    if (state === undefined || state.destroyed) return;
    if (port.listenerCount("message") === 1) {
      port.ref();
      port.start();
    }
  }
  function messagePortListenerRemoved(port, type) {
    if (type !== "message") return;
    const state = portState.get(port);
    if (state === undefined || state.destroyed) return;
    if (port.listenerCount("message") === 0) port.unref();
  }
  // Node: MessagePort.prototype -> NodeEventTarget.prototype -> EventTarget.prototype, and the
  // constructor is not callable at all (instances come from MessageChannel or a transfer).
  const nodeEventTargetProto = Object.create(NodeEventTarget.prototype);
  makeNodeTargetMethods(nodeEventTargetProto, messagePortListenerAdded, messagePortListenerRemoved);
  function MessagePort() {
    throw nodeError(TypeError, "ERR_CONSTRUCT_CALL_INVALID", "Constructor cannot be called");
  }
  Object.setPrototypeOf(MessagePort, NodeEventTarget);
  Object.setPrototypeOf(MessagePortMethods.prototype, nodeEventTargetProto);
  Object.defineProperty(MessagePortMethods.prototype, "constructor", { value: MessagePort, writable: true, configurable: true });
  MessagePort.prototype = MessagePortMethods.prototype;
  defineEventHandler(MessagePort.prototype, "message");
  defineEventHandler(MessagePort.prototype, "messageerror");

  function readIterable(value) {
    if (Array.isArray(value)) return [...value];
    const method = value[Symbol.iterator];
    if (typeof method !== "function") return null;
    const iterator = Reflect.apply(method, value, []);
    if (iterator === null || (typeof iterator !== "object" && typeof iterator !== "function")) return null;
    const next = iterator.next;
    if (typeof next !== "function") return null;
    const out = [];
    for (;;) {
      const result = Reflect.apply(next, iterator, []);
      if (result === null || (typeof result !== "object" && typeof result !== "function")) return null;
      if (result.done) break;
      out.push(result.value);
    }
    return out;
  }
  // postMessage's second argument: a transfer list (any iterable) or `{ transfer }`.
  function readTransferList(arg) {
    if (arg === undefined || arg === null) return [];
    if (typeof arg !== "object" && typeof arg !== "function") {
      throw nodeError(TypeError, "ERR_INVALID_ARG_TYPE", "Optional transferList argument must be an iterable");
    }
    const list = readIterable(arg);
    if (list !== null) return list;
    const transfer = arg.transfer;
    if (transfer === undefined) return [];
    const fromOption = transfer !== null && (typeof transfer === "object" || typeof transfer === "function")
      ? readIterable(transfer) : null;
    if (fromOption === null) {
      throw nodeError(TypeError, "ERR_INVALID_ARG_TYPE", "Optional options.transfer argument must be an iterable");
    }
    return fromOption;
  }

  // One wake per message: each delivery is its own macrotask, so microtasks queued by a
  // listener run before the next message is dispatched.
  function onPortWake(port) {
    const state = portState.get(port);
    if (state === undefined || state.destroyed) return;
    if (state.broadcast) {
      drainBroadcastPorts();
      return;
    }
    if (!state.started) {
      if (state.closingLocal || ops().peek(state.id) < 0) closeFromWake(port, state);
      return;
    }
    const bytes = ops().poll(state.id);
    if (bytes === false) {
      closeFromWake(port, state);
      return;
    }
    if (bytes === undefined) return;
    ops().wake(state.id);
    deliver(port, bytes);
  }
  // The side that called close() sees its 'close' first; the peer's follows a turn later.
  function closeFromWake(port, state) {
    if (!state.closingLocal && !state.closeDeferred) {
      state.closeDeferred = true;
      ops().wake(state.id);
      return;
    }
    finishClose(port);
  }
  // Like Node's (libuv async handles), every BroadcastChannel with mail is drained in turn, in
  // creation order.
  const broadcastPorts = new Set();
  function drainBroadcastPorts() {
    for (const port of [...broadcastPorts]) {
      const state = portState.get(port);
      while (!state.destroyed) {
        const bytes = ops().poll(state.id);
        if (bytes === undefined) break;
        if (bytes === false) {
          broadcastPorts.delete(port);
          finishClose(port);
          break;
        }
        deliver(port, bytes);
      }
    }
  }
  function decodeMessage(bytes) {
    const outer = receivedPorts;
    receivedPorts = [];
    try {
      return { data: deser(bytes), ports: receivedPorts };
    } finally {
      receivedPorts = outer;
    }
  }
  function deliver(port, bytes) {
    let decoded;
    try {
      decoded = decodeMessage(bytes);
    } catch (error) {
      hybridDispatch(port, "messageerror", error, () => new NodeMessageEvent("messageerror", { data: error }));
      return;
    }
    const { data, ports } = decoded;
    hybridDispatch(port, "message", data, () => new NodeMessageEvent("message", { data, ports }));
  }
  function finishClose(port) {
    const state = portState.get(port);
    if (state === undefined || state.destroyed || state.closing) return;
    state.closing = true;
    try {
      port.emit("close");
    } finally {
      state.destroyed = true;
      ops().detach(state.id);
      __destroyAsyncResource(port);
    }
  }
  // Drain whatever is queued right now, synchronously (a Worker's exit, receiveMessageOnPort).
  function drainPort(port) {
    const state = portState.get(port);
    if (state === undefined || state.destroyed) return;
    for (;;) {
      const bytes = ops().poll(state.id);
      if (bytes === undefined || bytes === false) return;
      deliver(port, bytes);
    }
  }

  function createPort(id, hooks = true) {
    const port = Reflect.construct(NodeEventTarget, [], MessagePort);
    portState.set(port, {
      id, refed: false, started: false, closing: false, closingLocal: false, destroyed: false, detached: false,
    });
    ops().listen(id, () => onPortWake(port));
    if (hooks && __asyncTracking) __initAsyncResource(port, "MESSAGEPORT");
    return port;
  }

  function MessageChannel() {
    if (!new.target) {
      throw nodeError(TypeError, "ERR_CONSTRUCT_CALL_REQUIRED", "Class constructor MessageChannel cannot be invoked without 'new'");
    }
    const pair = ops().pair();
    this.port1 = createPort(pair.a);
    this.port2 = createPort(pair.b);
  }
  Object.defineProperty(MessageChannel, "name", { value: "MessageChannel" });

  function isMessagePort(value) {
    return value !== null && typeof value === "object" && portState.has(value);
  }

  function receiveMessageOnPort(port) {
    const target = port?.[kBroadcastPort] ?? port;
    if (!isMessagePort(target)) {
      throw nodeError(TypeError, "ERR_INVALID_ARG_TYPE", 'The "port" argument must be a MessagePort instance');
    }
    const state = portState.get(target);
    if (state.destroyed || state.detached) return undefined;
    const bytes = ops().poll(state.id);
    if (bytes === false) {
      finishClose(target);
      return undefined;
    }
    if (bytes === undefined) return undefined;
    return { message: decodeMessage(bytes).data };
  }

  function moveMessagePortToContext(port, contextifiedSandbox) {
    if (!isMessagePort(port)) {
      throw new __errors.ERR_INVALID_ARG_TYPE("port", "MessagePort", port);
    }
    const state = portState.get(port);
    if (state.destroyed || state.closing || state.closingLocal || state.detached) {
      throw nodeError(Error, "ERR_CLOSED_MESSAGE_PORT", "Cannot send data on closed MessagePort");
    }
    throw new Error("worker_threads moveMessagePortToContext is not supported in lumen");
  }

  // The serializer's view of ports (lumen-web serialize.js).
  Object.defineProperty(globalThis, "__lumenPortClone", {
    value: {
      isPort: isMessagePort,
      isUntransferable: (value) => value !== null && typeof value === "object" && value[kUntransferable] === true,
      isUncloneable: (value) => uncloneable.has(value),
      validate(port) {
        const state = portState.get(port);
        if (state.destroyed || state.closing || state.closingLocal || state.detached || ops().isClosed(state.id)) {
          throw new DOMException("MessagePort in transfer list is already detached", "DataCloneError");
        }
      },
      export(port) {
        return ops().export(portState.get(port).id);
      },
      detach(port) {
        const state = portState.get(port);
        state.detached = true;
        state.closing = true;
        ops().detach(state.id);
        process.nextTick(() => {
          try {
            port.emit("close");
          } finally {
            state.destroyed = true;
            __destroyAsyncResource(port);
          }
        });
      },
      import(index) {
        const port = createPort(ops().import(index));
        if (receivedPorts !== null) receivedPorts.push(port);
        return port;
      },
    },
    configurable: true,
  });

  // ---- BroadcastChannel ---------------------------------------------------------------------------
  // Every same-name channel in the process (any thread) receives a copy, except the sender.

  const kBroadcastPort = Symbol("kHandle");
  const broadcastState = new WeakMap();
  function broadcastStateOf(bc) {
    const state = broadcastState.get(bc);
    if (state === undefined) throw new __errors.ERR_INVALID_THIS("BroadcastChannel");
    return state;
  }
  class BroadcastChannel extends NodeEventTarget {
    constructor(name) {
      if (arguments.length === 0) throw new __errors.ERR_MISSING_ARGS("name");
      super();
      const state = { name: `${name}`, port: null, refed: true };
      broadcastState.set(this, state);
      const port = createPort(ops().broadcast(state.name), false);
      portState.get(port).broadcast = true;
      broadcastPorts.add(port);
      state.port = port;
      Object.defineProperty(this, kBroadcastPort, { value: port, configurable: true });
      const self = this;
      port.on("message", (data) => {
        if (state.port === null) return;
        hybridDispatch(self, "message", data, () => new NodeMessageEvent("message", { data }));
      });
      port.on("messageerror", (error) => {
        if (state.port === null) return;
        hybridDispatch(self, "messageerror", error, () => new NodeMessageEvent("messageerror", { data: error }));
      });
      port.ref();
    }
    get name() {
      return broadcastStateOf(this).name;
    }
    close() {
      const state = broadcastStateOf(this);
      if (state.port === null) return;
      const port = state.port;
      state.port = null;
      port.close();
    }
    postMessage(message) {
      const state = broadcastStateOf(this);
      if (arguments.length === 0) throw new __errors.ERR_MISSING_ARGS("message");
      if (state.port === null) throw new DOMException("BroadcastChannel is closed.", "InvalidStateError");
      ops().post(portState.get(state.port).id, ser(message, []));
    }
    ref() {
      const state = broadcastStateOf(this);
      state.port?.ref();
      return this;
    }
    unref() {
      const state = broadcastStateOf(this);
      state.port?.unref();
      return this;
    }
    [inspectCustom](depth, options) {
      const state = broadcastStateOf(this);
      if (depth < 0) return "BroadcastChannel";
      const opts = { ...options, depth: options.depth == null ? null : options.depth - 1 };
      return `BroadcastChannel ${lazyUtil().inspect({ name: state.name, active: state.port !== null }, opts)}`;
    }
  }
  makeNodeTargetMethods(BroadcastChannel.prototype, () => {}, () => {});
  for (const name of ["on", "addListener", "once", "off", "removeListener", "emit", "removeAllListeners",
    "listenerCount", "listeners", "eventNames", "setMaxListeners", "getMaxListeners"]) {
    delete BroadcastChannel.prototype[name];
  }
  defineEventHandler(BroadcastChannel.prototype, "message");
  defineEventHandler(BroadcastChannel.prototype, "messageerror");

  // ---- errors across threads ----------------------------------------------------------------------
  // Node's internal/error_serdes: an Error travels as its constructor name plus every property
  // (own and inherited, getters resolved, functions skipped), so the parent rebuilds an Error
  // of the same class with the same name/message/stack/code/...; anything else as its clone.

  const ERROR_CTORS = ["Error", "EvalError", "RangeError", "ReferenceError", "SyntaxError", "TypeError", "URIError"];
  function errorProperties(object, target, depth = 0) {
    const all = {};
    if (object === null || object === Object.prototype || depth > 32) return all;
    Object.assign(all, errorProperties(Object.getPrototypeOf(object), target, depth + 1));
    for (const key of Object.getOwnPropertyNames(object)) {
      if (key === "constructor" || key === "__proto__") continue;
      let desc;
      try {
        desc = Object.getOwnPropertyDescriptor(object, key);
      } catch {
        continue;
      }
      let value = desc.value;
      if (desc.get) {
        try {
          value = Reflect.apply(desc.get, target, []);
        } catch {
          continue;
        }
      } else if (!("value" in desc)) {
        continue;
      }
      if (typeof value === "function" || typeof value === "symbol") continue;
      if (key === "cause") value = { [kSerializedCause]: serializeError(value) };
      all[key] = { value, enumerable: !!desc.enumerable };
    }
    return all;
  }
  const kSerializedCause = "\u0000lumen.cause";
  function serializeError(error) {
    try {
      if (error !== null && typeof error === "object" && Object.prototype.toString.call(error) === "[object Error]") {
        for (let proto = Object.getPrototypeOf(error); proto !== null; proto = Object.getPrototypeOf(proto)) {
          const ctor = Object.prototype.hasOwnProperty.call(proto, "constructor") ? proto.constructor : undefined;
          const name = typeof ctor === "function" ? ctor.name : undefined;
          if (ERROR_CTORS.includes(name)) {
            return { kind: "error", bytes: ser({ constructor: name, properties: errorProperties(error, error) }) };
          }
        }
      }
    } catch {}
    try {
      return { kind: "value", bytes: ser(error) };
    } catch {}
    let text;
    try {
      text = lazyUtil().inspect(error);
    } catch {
      text = "[unserializable error]";
    }
    return { kind: "inspected", text };
  }
  function deserializeError(serialized) {
    if (serialized.kind === "inspected") return serialized.text;
    const value = deser(serialized.bytes);
    if (serialized.kind !== "error") return value;
    const Ctor = globalThis[value.constructor] ?? Error;
    const error = Object.create(Ctor.prototype);
    for (const key of Object.keys(value.properties)) {
      const { value: v, enumerable } = value.properties[key];
      const restored = v !== null && typeof v === "object" && kSerializedCause in v ? deserializeError(v[kSerializedCause]) : v;
      Object.defineProperty(error, key, { value: restored, enumerable, writable: true, configurable: true });
    }
    return error;
  }

  // ---- Worker (parent side) -----------------------------------------------------------------------

  const kResourceLimitKeys = ["maxYoungGenerationSizeMb", "maxOldGenerationSizeMb", "codeRangeSizeMb", "stackSizeMb"];
  const kDefaultResourceLimits = {
    maxYoungGenerationSizeMb: 48, maxOldGenerationSizeMb: 2048, codeRangeSizeMb: 0, stackSizeMb: 4,
  };
  // The limits a worker runs with: what the caller gave (Node keeps the old generation at 2 MiB
  // or more), the defaults for the rest, and the subset the engine enforces.
  function parseResourceLimits(limits) {
    const given = {};
    if (limits !== undefined && limits !== null && typeof limits === "object") {
      for (const key of kResourceLimitKeys) {
        if (typeof limits[key] === "number") given[key] = limits[key];
      }
      if (given.maxOldGenerationSizeMb !== undefined) {
        given.maxOldGenerationSizeMb = Math.max(given.maxOldGenerationSizeMb, 2);
      }
    }
    return { given, effective: { ...kDefaultResourceLimits, ...given } };
  }

  // Node's worker execArgv/NODE_OPTIONS check: per-process options are not allowed in a worker.
  const kPerProcessOptions = new Set([
    "--title", "--v8-options", "--version", "-v", "--help", "-h", "--icu-data-dir", "--openssl-config",
    "--tls-cipher-list", "--use-openssl-ca", "--use-bundled-ca", "--enable-fips", "--force-fips",
    "--secure-heap", "--secure-heap-min", "--disable-proto", "--build-snapshot", "--snapshot-blob",
    "--abort-on-uncaught-exception", "--max-old-space-size", "--max-semi-space-size", "--stack-size",
    "--perf-basic-prof", "--perf-prof", "--interpreted-frames-native-stack", "--prof",
    "--report-on-signal", "--report-signal", "--v8-pool-size", "--zero-fill-buffers", "--debug-arraybuffer-allocations",
  ]);
  const kOptionsWithValue = new Set([
    "--require", "-r", "--import", "--loader", "--experimental-loader", "--input-type", "--conditions", "-C",
    "--redirect-warnings", "--unhandled-rejections", "--trace-event-categories", "--trace-event-file-pattern",
    "--diagnostic-dir", "--heapsnapshot-signal", "--dns-result-order", "--es-module-specifier-resolution",
    "--experimental-specifier-resolution", "--inspect-publish-uid", "--max-http-header-size", "--stack-trace-limit",
    "--secure-heap", "--title", "--report-dir", "--report-directory", "--report-filename", "--env-file",
    "--disable-warning", "--watch-path", "--test-reporter", "--test-reporter-destination", "--test-name-pattern",
    "--experimental-policy", "--policy-integrity", "--heap-prof-dir", "--heap-prof-name", "--cpu-prof-dir",
    "--cpu-prof-name", "--icu-data-dir", "--openssl-config", "--tls-cipher-list",
  ]);
  const kKnownFlag = /^--(?:no-)?(?:experimental-[a-z0-9-]+|trace-[a-z0-9-]+|expose[-_][a-z0-9_-]+|harmony[a-z0-9_-]*|allow-[a-z0-9-]+|frozen-intrinsics|pending-deprecation|no-deprecation|throw-deprecation|trace-deprecation|no-warnings|warnings|preserve-symlinks(?:-main)?|insecure-http-parser|enable-source-maps|abort-on-uncaught-exception|addons|global-search-paths|force-context-aware|deprecation|verify-base-objects|report-[a-z-]+|inspect(?:-brk)?(?:=.*)?|debug-port|max-http-header-size|use-largepages|node-memory-debug|heapsnapshot-near-heap-limit|force-async-hooks-checks|async-context-frame|extra-info-on-fatal-exception|network-family-autoselection|test|test-only|watch|permission|stack-trace-limit|zero-fill-buffers|jitless|expose-internals|gc-interval|max-lazy|lazy|opt|no-opt|sparkplug|always-sparkplug|predictable|random-seed|single-threaded|allow-natives-syntax|interrupt-budget|cpu-prof|heap-prof)(?:=.*)?$/;
  function invalidExecArgv(argv) {
    const bad = [];
    for (let i = 0; i < argv.length; i++) {
      const arg = String(argv[i]);
      if (!arg.startsWith("-") || arg === "-" || arg === "--") continue;
      const eq = arg.indexOf("=");
      const name = (eq === -1 ? arg : arg.slice(0, eq)).replace(/_/g, "-");
      if (kPerProcessOptions.has(name)) {
        bad.push(arg);
      } else if (kOptionsWithValue.has(name)) {
        if (eq === -1) {
          if (i + 1 >= argv.length) bad.push(arg);
          else i++;
        }
      } else if (!kKnownFlag.test(arg.replace(/_/g, "-"))) {
        bad.push(arg);
      }
    }
    return bad;
  }
  function splitNodeOptions(text) {
    const out = [];
    const re = /"((?:\\.|[^"\\])*)"|(\S+)/g;
    let m;
    while ((m = re.exec(text)) !== null) out.push(m[1] !== undefined ? m[1].replace(/\\(.)/g, "$1") : m[2]);
    return out;
  }

  // Node caches a worker's cwd and re-reads it when the main thread's chdir() bumps this
  // counter (shared with every worker).
  let cwdCounter = null;
  function sharedCwdCounter() {
    if (cwdCounter !== null) return cwdCounter;
    cwdCounter = new Int32Array(new SharedArrayBuffer(4));
    if (wt.isMainThread) {
      const chdir = process.chdir;
      process.chdir = function chdir_(directory) {
        const result = Reflect.apply(chdir, this, arguments);
        Atomics.add(cwdCounter, 0, 1);
        return result;
      };
    }
    return cwdCounter;
  }

  const workerState = new WeakMap();
  let terminateCallbackWarned = false;
  class Worker extends EventEmitter {
    constructor(filename, options = {}) {
      super();
      if (options === null || options === undefined) options = {};
      if (options.execArgv !== undefined && !Array.isArray(options.execArgv)) {
        throw new __errors.ERR_INVALID_ARG_TYPE("options.execArgv", "Array", options.execArgv);
      }
      let argv = [];
      if (options.argv !== undefined) {
        if (!Array.isArray(options.argv)) throw new __errors.ERR_INVALID_ARG_TYPE("options.argv", "Array", options.argv);
        argv = options.argv.map(String);
      }
      const isURL = filename !== null && typeof filename === "object" && typeof URL === "function" && filename instanceof URL;
      let entry;
      let mode; // "file" | "eval" | "module-eval"
      if (options.eval) {
        if (typeof filename !== "string") {
          throw new __errors.ERR_INVALID_ARG_VALUE("options.eval", options.eval, "must be false when 'filename' is not a string");
        }
        entry = filename;
        mode = "eval";
      } else if (isURL && filename.protocol === "data:") {
        entry = filename.href;
        mode = "module-eval";
      } else {
        let p;
        if (isURL) {
          if (filename.protocol !== "file:") throw new __errors.ERR_INVALID_URL_SCHEME(["file:", "data:"]);
          p = __builtins.get("url").fileURLToPath(filename);
        } else if (typeof filename !== "string") {
          throw new __errors.ERR_INVALID_ARG_TYPE("filename", ["string", "URL"], filename);
        } else if (pathMod.isAbsolute(filename) || /^\.\.?[\\/]/.test(filename)) {
          p = filename;
        } else {
          throw nodeError(TypeError, "ERR_WORKER_PATH",
            "The worker script or module filename must be an absolute path or a relative path starting with './' or '../'." +
            (filename.startsWith("file://") ? " Wrap file:// URLs with `new URL`." : "") +
            (filename.startsWith("data:text/javascript") ? " Wrap data: URLs with `new URL`." : "") +
            ` Received "${filename}"`);
        }
        entry = pathMod.resolve(p);
        mode = "file";
      }

      const shareEnv = options.env === SHARE_ENV;
      let env = null;
      if (options.env !== null && typeof options.env === "object") {
        env = {};
        for (const [k, v] of Object.entries(options.env)) env[k] = `${v}`;
      } else if (options.env === undefined || options.env === null) {
        env = {};
        for (const k of Object.keys(process.env)) {
          const v = process.env[k];
          if (v !== undefined) env[k] = String(v);
        }
      } else if (!shareEnv) {
        throw new __errors.ERR_INVALID_ARG_TYPE("options.env", ["object", "undefined", "null", "worker_threads.SHARE_ENV"], options.env);
      }
      if (options.name !== undefined && options.name !== "" && typeof options.name !== "string") {
        throw new __errors.ERR_INVALID_ARG_TYPE("options.name", "string", options.name);
      }
      const execArgv = options.execArgv !== undefined ? options.execArgv.map(String) : [...process.execArgv];
      if (options.execArgv !== undefined) {
        const bad = invalidExecArgv(execArgv);
        if (bad.length) {
          throw nodeError(Error, "ERR_WORKER_INVALID_EXEC_ARGV", `Initiated Worker with invalid execArgv flags: ${bad.join(", ")}`);
        }
      }
      const nodeOptions = env !== null ? env.NODE_OPTIONS : process.env.NODE_OPTIONS;
      if (typeof nodeOptions === "string" && nodeOptions.trim() !== "") {
        const bad = invalidExecArgv(splitNodeOptions(nodeOptions));
        if (bad.length) {
          throw nodeError(Error, "ERR_WORKER_INVALID_EXEC_ARGV", `Initiated Worker with invalid NODE_OPTIONS env variable: ${bad.join(", ")}`);
        }
      }
      const { given: givenLimits, effective: resourceLimits } = parseResourceLimits(options.resourceLimits);
      const preload = [];
      for (let i = 0; i < execArgv.length; i++) {
        const arg = execArgv[i];
        if ((arg === "--require" || arg === "-r") && i + 1 < execArgv.length) preload.push(execArgv[++i]);
        else if (arg.startsWith("--require=")) preload.push(arg.slice(10));
      }

      // Everything the worker realm needs at boot travels as one structured clone; a
      // DataCloneError from workerData (or its transferList) surfaces here, like Node.
      const transferList = options.transferList === undefined ? [] : readTransferList(options.transferList);
      const init = ser({
        workerData: options.workerData,
        argv,
        env,
        envData: environmentData,
        entry: mode === "file" ? entry : "[worker eval]",
        mode,
        execArgv,
        resourceLimits,
        hasStdin: !!options.stdin,
        trace: __traceEvent === null ? undefined : __internals.get("trace_events").groups(),
        cwdCounter: sharedCwdCounter(),
        preload,
        trackUnmanagedFds: options.trackUnmanagedFds ?? true,
      }, transferList);
      const res = getWorkerOps().spawn(
        entry,
        mode === "module-eval" || (mode === "file" && entry.endsWith(".mjs")),
        (kind, a) => this.#onEvent(kind, a),
        {
          node: true, eval: mode === "eval", moduleEval: mode === "module-eval", shareEnv, init,
          maxOldMb: givenLimits.maxOldGenerationSizeMb, stackMb: givenLimits.stackSizeMb,
        },
      );

      const handle = {
        hasRef: () => (state.handleDestroyed ? undefined : state.refed),
      };
      const state = {
        id: res.id,
        threadId: res.threadId,
        exited: false,
        exitCode: null,
        refed: true,
        handle,
        handleDestroyed: false,
        resourceLimits,
        stdin: null,
        stdout: null,
        stderr: null,
        stdoutOption: !!options.stdout,
        stderrOption: !!options.stderr,
        internal: createPort(res.internal, false),
        publicPort: null,
      };
      workerState.set(this, state);
      if (__traceEvent !== null) {
        const name = options.name === undefined ? "" : ` ${options.name}`;
        __internals.get("trace_events").threadName(res.threadId, `[worker ${res.threadId}]${name}`);
      }
      const channel = __builtins.get("diagnostics_channel").channel("worker_threads");
      if (channel.hasSubscribers) channel.publish({ worker: this });
      if (__asyncTracking) __initAsyncResource(handle, "WORKER");

      state.internal.on("message", (message) => this.#onInternalMessage(message));
      state.internal.unref();
      if (options.stdin) state.stdin = createParentStdin(state.internal);
      if (state.stdoutOption) state.stdout = createParentStdio();
      if (state.stderrOption) state.stderr = createParentStdio();

      const publicPort = createPort(res.port);
      state.publicPort = publicPort;
      publicPort.on("message", (message) => this.emit("message", message));
      publicPort.on("messageerror", (error) => this.emit("messageerror", error));
      publicPort.unref();
      this.on("newListener", (name) => {
        if (name === "message" && this.listenerCount("message") === 0 && !state.exited) {
          publicPort.ref();
        }
      });
      this.on("removeListener", (name) => {
        if (name === "message" && this.listenerCount("message") === 0 && !state.exited) publicPort.unref();
      });

      this.performance = {
        eventLoopUtilization: (util1, util2) => workerEventLoopUtilization(this, util1, util2),
      };
      process.nextTick(() => process.emit("worker", this));
    }
    get threadId() {
      const state = workerState.get(this);
      return state === undefined || state.exited ? -1 : state.threadId;
    }
    get resourceLimits() {
      const state = workerState.get(this);
      return state === undefined || state.exited ? {} : { ...state.resourceLimits };
    }
    get stdin() {
      return workerState.get(this)?.stdin ?? null;
    }
    get stdout() {
      const state = workerState.get(this);
      if (state === undefined) return null;
      if (state.stdout === null) {
        state.stdout = createParentStdio();
        pipeToProcess(state.stdout, "stdout");
        if (state.exited) state.stdout.push(null);
      }
      return state.stdout;
    }
    get stderr() {
      const state = workerState.get(this);
      if (state === undefined) return null;
      if (state.stderr === null) {
        state.stderr = createParentStdio();
        pipeToProcess(state.stderr, "stderr");
        if (state.exited) state.stderr.push(null);
      }
      return state.stderr;
    }
    #stdioReadable(stream) {
      const state = workerState.get(this);
      if (stream === "stdout") return state.stdout ?? (state.stdoutOption ? null : this.stdout);
      return state.stderr ?? (state.stderrOption ? null : this.stderr);
    }
    #onInternalMessage(message) {
      if (message === null || typeof message !== "object") return;
      const state = workerState.get(this);
      switch (message.type) {
        case "stdio": {
          const readable = state[message.stream];
          if (readable !== null) readable.push(message.chunk);
          else (message.stream === "stderr" ? process.stderr : process.stdout).write(message.chunk);
          return;
        }
        case "error":
          this.emit("error", deserializeError(message.error));
          return;
        case "heapSnapshot": {
          const pending = state.heapSnapshots?.get(message.id);
          if (pending === undefined) return;
          state.heapSnapshots.delete(message.id);
          if (message.error !== undefined) pending.reject(deserializeError(message.error));
          else {
            const { Readable } = lazyStream();
            const stream = new Readable({ read() {} });
            stream.push(message.data);
            stream.push(null);
            pending.resolve(stream);
          }
          return;
        }
        case "elu":
          state.elu = message.elu;
          return;
        case "trace":
          __internals.get("trace_events").addWorkerEvents(message.events);
          return;
      }
    }
    #onEvent(kind, a) {
      const state = workerState.get(this);
      if (kind === "online") {
        state.online = true;
        this.emit("online");
      } else if (kind === "oom") {
        state.oom = true;
      } else if (kind === "error") {
        // A failure before the worker's own error reporting was in place (its bootstrap).
        this.emit("error", reviveError(a));
      } else if (kind === "exit") {
        this.#onExit(a);
      }
    }
    #onExit(code) {
      const state = workerState.get(this);
      drainPort(state.internal);
      drainPort(state.publicPort);
      this.removeAllListeners("message");
      this.removeAllListeners("messageerrors");
      state.exited = true;
      state.exitCode = code;
      state.publicPort.unref();
      if (!portState.get(state.publicPort).destroyed) finishClose(state.publicPort);
      if (!portState.get(state.internal).destroyed) finishClose(state.internal);
      for (const name of ["stdout", "stderr"]) {
        const readable = state[name];
        if (readable !== null && !readable.readableEnded) readable.push(null);
      }
      for (const { reject } of state.heapSnapshots?.values() ?? []) {
        reject(nodeError(Error, "ERR_WORKER_NOT_RUNNING", "Worker instance not running"));
      }
      state.heapSnapshots = undefined;
      if (state.oom) {
        this.emit("error", nodeError(Error, "ERR_WORKER_OUT_OF_MEMORY",
          "Worker terminated due to reaching memory limit: JS heap out of memory"));
      }
      this.emit("exit", code);
      this.removeAllListeners();
      process.nextTick(() => {
        state.handleDestroyed = true;
        __destroyAsyncResource(state.handle);
      });
    }
    postMessage(...args) {
      const state = workerState.get(this);
      if (state === undefined || state.exited) return;
      Reflect.apply(MessagePort.prototype.postMessage, state.publicPort, args);
    }
    terminate(callback) {
      const state = workerState.get(this);
      this.ref();
      if (typeof callback === "function") {
        if (!terminateCallbackWarned) {
          terminateCallbackWarned = true;
          process.emitWarning("Passing a callback to worker.terminate() is deprecated. It returns a Promise instead.",
            "DeprecationWarning", "DEP0132");
        }
        if (state.exited) return Promise.resolve();
        this.once("exit", (code) => callback(null, code));
      }
      if (state.exited) return Promise.resolve();
      getWorkerOps().terminate(state.id);
      return new Promise((resolve) => this.once("exit", resolve));
    }
    ref() {
      const state = workerState.get(this);
      if (state === undefined || state.exited) return;
      state.refed = true;
      getWorkerOps().setRef(state.id, true);
      state.publicPort.ref();
    }
    unref() {
      const state = workerState.get(this);
      if (state === undefined || state.exited) return;
      state.refed = false;
      getWorkerOps().setRef(state.id, false);
      state.publicPort.unref();
    }
    getHeapSnapshot(options) {
      if (options !== undefined && (options === null || typeof options !== "object")) {
        throw new __errors.ERR_INVALID_ARG_TYPE("options", "Object", options);
      }
      const state = workerState.get(this);
      if (state.exited) {
        return Promise.reject(nodeError(Error, "ERR_WORKER_NOT_RUNNING", "Worker instance not running"));
      }
      state.heapSnapshots ??= new Map();
      const id = (state.nextSnapshot = (state.nextSnapshot ?? 0) + 1);
      return new Promise((resolve, reject) => {
        state.heapSnapshots.set(id, { resolve, reject });
        state.internal.postMessage({ type: "heapSnapshot", id, options: options ?? {} });
        state.internal.ref();
      }).finally(() => {
        if (state.heapSnapshots?.size === 0 && !state.exited) state.internal.unref();
      });
    }
    getHeapStatistics() {
      return Promise.reject(new Error("worker.getHeapStatistics is not supported in lumen"));
    }
  }

  function workerEventLoopUtilization(worker, util1, util2) {
    const state = workerState.get(worker);
    if (state === undefined || state.exited || !state.online) return { idle: 0, active: 0, utilization: 0 };
    const now = getWorkerOps().elu?.(state.id);
    if (now == null) return { idle: 0, active: 0, utilization: 0 };
    return __builtins.get("perf_hooks").__computeELU?.(now, util1, util2) ?? { idle: 0, active: 0, utilization: 0 };
  }

  // Worker-side uncaught errors that predate the worker's own reporting travel as text.
  function reviveError(text) {
    const m = /^([A-Za-z][A-Za-z0-9_$]*(?:Error|Exception)): ([\s\S]*)$/.exec(String(text));
    const Ctor = m && typeof globalThis[m[1]] === "function" ? globalThis[m[1]] : Error;
    return m ? new Ctor(m[2]) : new Error(String(text));
  }

  function createParentStdio() {
    const { Readable } = lazyStream();
    return new Readable({ read() {} });
  }
  function pipeToProcess(readable, name) {
    readable.on("data", (chunk) => process[name].write(chunk));
  }
  function createParentStdin(internal) {
    const { Writable } = lazyStream();
    return new Writable({
      write(chunk, encoding, cb) {
        const bytes = typeof chunk === "string" ? Buffer.from(chunk, encoding) : chunk;
        internal.postMessage({ type: "stdin", chunk: new Uint8Array(bytes.buffer, bytes.byteOffset, bytes.byteLength).slice() });
        cb();
      },
      final(cb) {
        internal.postMessage({ type: "stdin", chunk: null });
        cb();
      },
    });
  }

  const wt = {
    isMainThread: true,
    isInternalThread: false,
    threadId: 0,
    parentPort: null,
    workerData: null,
    SHARE_ENV,
    resourceLimits: {},
    Worker,
    MessageChannel,
    MessagePort,
    BroadcastChannel,
    markAsUntransferable(value) {
      if (value === null || (typeof value !== "object" && typeof value !== "function")) return;
      Object.defineProperty(value, kUntransferable, { value: true, configurable: true, enumerable: false, writable: false });
    },
    isMarkedAsUntransferable(value) {
      return value !== null && (typeof value === "object" || typeof value === "function") && value[kUntransferable] === true;
    },
    markAsUncloneable(value) {
      if (value !== null && (typeof value === "object" || typeof value === "function")) uncloneable.add(value);
    },
    moveMessagePortToContext,
    postMessageToThread() {
      throw new Error("worker_threads postMessageToThread is not supported in lumen");
    },
    receiveMessageOnPort,
    setEnvironmentData(key, value) {
      if (value === undefined) environmentData.delete(key);
      else environmentData.set(key, value);
    },
    getEnvironmentData(key) {
      return environmentData.get(key);
    },
  };

  globalThis.MessagePort = MessagePort;
  globalThis.MessageChannel = MessageChannel;
  globalThis.BroadcastChannel = BroadcastChannel;

  // ---- the worker realm's bootstrap ---------------------------------------------------------------
  // NODE_WORKER_SCOPE_JS (lumen-runtime/src/js/node_worker_scope.js) calls this in each node worker
  // realm BEFORE the entry runs: it flips this module to worker-side state, patches process, and
  // returns the hooks the runtime drives the entry and the end of the thread with.
  Object.defineProperty(globalThis, "__lumenInitWorkerThread", {
    configurable: true,
    enumerable: false,
    writable: false,
    value: function __lumenInitWorkerThread(wself, threadId, initBytes, portIds) {
      const internal = createPort(portIds[1], false);
      let init = {};
      if (initBytes) init = deser(initBytes) || {};

      wt.isMainThread = false;
      wt.threadId = typeof threadId === "number" ? threadId : -1;
      wt.workerData = init.workerData;
      wt.resourceLimits = { ...(init.resourceLimits ?? {}) };
      if (init.envData instanceof Map) {
        for (const [k, v] of init.envData) environmentData.set(k, v);
      }
      if (init.env && typeof init.env === "object") globalThis.__lumenResetWorkerEnvironment?.(init.env);
      delete globalThis.__lumenResetWorkerEnvironment;
      process.argv = [process.execPath, init.entry ?? "[worker eval]", ...(Array.isArray(init.argv) ? init.argv : [])];
      process.execArgv = Array.isArray(init.execArgv) ? init.execArgv : [];
      if (process.execArgv.includes("--trace-warnings")) process.traceProcessWarnings = true;
      for (const arg of process.execArgv) {
        const nearLimit = /^--heapsnapshot-near-heap-limit=(\d+)$/.exec(arg);
        if (nearLimit !== null) __builtins.get("v8").setHeapSnapshotNearHeapLimit(Number(nearLimit[1]));
      }
      if (Array.isArray(init.trace)) {
        __internals.get("trace_events").startInWorker(init.trace, wt.threadId, (events) => {
          internal.postMessage({ type: "trace", events });
        });
      }
      if (process.execArgv.some((a) => a === "--expose-gc" || a === "--expose_gc") && typeof globalThis.gc !== "function") {
        globalThis.gc = function gc() { __node.collectGarbage(); };
      }
      // The cluster glue must not adopt the process's cluster IPC channel in a worker thread.
      Object.defineProperty(globalThis, "__lumenWorkerThreadRealm", {
        configurable: true, enumerable: false, writable: false, value: true,
      });

      // process.exit() ends this thread only; the code is process.exitCode's.
      const endThread = (code) => {
        if (code !== undefined) process.exitCode = code;
        const exitCode = () => {
          const n = Number(process.exitCode ?? 0);
          return Number.isFinite(n) ? Math.trunc(n) | 0 : 0;
        };
        if (!process._exiting) {
          process._exiting = true;
          try {
            process.emit("exit", exitCode());
          } catch {}
        }
        wself.exit(exitCode());
      };
      const exitWorker = function exit(code) {
        if (process.execArgv.includes("--trace-exit")) {
          __internals.get("traceExit")(code === undefined ? Number(process.exitCode ?? 0) | 0 : code, threadId);
        }
        endThread(code);
      };
      Object.defineProperty(process, "exit", { value: exitWorker, enumerable: true, configurable: true, writable: true });
      process.reallyExit = (code) => wself.exit(Number(code) | 0);

      // Uncaught errors: the worker's own handlers first (Node's onGlobalUncaughtException),
      // then the error goes to the parent's 'error' event and the thread exits.
      const onGlobalUncaughtException = process._fatalException;
      process._fatalException = function workerOnGlobalUncaughtException(error, fromPromise) {
        let handled = false;
        let handlerThrew = false;
        try {
          handled = Reflect.apply(onGlobalUncaughtException, process, [error, fromPromise]);
        } catch (e) {
          error = e;
          handlerThrew = true;
        }
        if (handled) return true;
        if (!process._exiting) {
          try {
            process._exiting = true;
            process.exitCode = 1;
            if (!handlerThrew) process.emit("exit", process.exitCode);
          } catch {}
        }
        try {
          internal.postMessage({ type: "error", error: serializeError(error) });
        } catch {}
        endThread();
        return true;
      };

      patchWorkerProcess(init);
      setupWorkerStdio(internal, !!init.hasStdin);

      // The internal channel: stdin data and heap snapshot requests from the parent.
      internal.on("message", (message) => {
        if (message === null || typeof message !== "object") return;
        if (message.type === "stdin") workerStdinPush(message.chunk);
        else if (message.type === "heapSnapshot") sendHeapSnapshot(internal, message);
      });
      internal.unref();

      const parentPort = createPort(portIds[0]);
      wt.parentPort = parentPort;

      for (const file of Array.isArray(init.preload) ? init.preload : []) {
        __builtins.get("module").createRequire(pathMod.join(process.cwd(), "[worker preload]"))(file);
      }

      let entryPending = false;
      return {
        dispatch() {},
        // `eval: true`: a classic script with CommonJS-like globals, as Node's worker eval.
        runEval(source) {
          const cwd = process.cwd();
          const Module = __builtins.get("module");
          const mod = new Module("[worker eval]", null);
          mod.filename = pathMod.join(cwd, "[worker eval]");
          mod.paths = Module._nodeModulePaths(cwd);
          globalThis.module = mod;
          globalThis.exports = mod.exports;
          globalThis.__dirname = ".";
          globalThis.__filename = "[worker eval]";
          globalThis.require = Module.createRequire(mod.filename);
          return (0, eval)(source);
        },
        runModule(specifier) {
          const url = specifier.startsWith("data:") ? specifier : __builtins.get("url").pathToFileURL(specifier).href;
          entryPending = true;
          import(url).then(
            () => {
              entryPending = false;
            },
            (error) => {
              entryPending = false;
              process.nextTick(() => {
                throw error;
              });
            },
          );
        },
        beforeExit() {
          if (entryPending) {
            // Node: a module entry whose top-level await never settles ends with code 13.
            process.exitCode = 13;
            endThread();
            return;
          }
          process.emit("beforeExit", Number(process.exitCode ?? 0) | 0);
        },
        exit() {
          endThread();
        },
      };
    },
  });

  function patchWorkerProcess(init) {
    const unsupported = (name) => {
      const fn = function () {
        throw new __errors.ERR_WORKER_UNSUPPORTED_OPERATION(`process.${name}()`);
      };
      fn.disabled = true;
      return fn;
    };
    const hasIpc = Boolean(process.env.NODE_CHANNEL_FD);
    const stubs = ["abort", "chdir"];
    if (hasIpc) stubs.push("send", "disconnect");
    else for (const name of ["send", "disconnect", "channel", "connected"]) delete process[name];
    if (process.platform !== "win32") stubs.push("setuid", "seteuid", "setgid", "setegid", "setgroups", "initgroups");
    for (const name of stubs) {
      Object.defineProperty(process, name, { value: unsupported(name), writable: true, configurable: true, enumerable: true });
    }
    for (const name of hasIpc ? ["channel", "connected"] : []) {
      Object.defineProperty(process, name, {
        configurable: true,
        enumerable: true,
        get() {
          throw new __errors.ERR_WORKER_UNSUPPORTED_OPERATION(`process.${name}`);
        },
      });
    }
    const umask = process.umask;
    process.umask = function umask_(mask) {
      if (mask !== undefined) throw new __errors.ERR_WORKER_UNSUPPORTED_OPERATION("Setting process.umask()");
      return Reflect.apply(umask, this, []);
    };
    const title = process.title;
    Object.defineProperty(process, "title", { configurable: true, enumerable: true, get: () => title, set() {} });
    const debugPort = process.debugPort;
    Object.defineProperty(process, "debugPort", { configurable: true, enumerable: true, get: () => debugPort, set() {} });
    for (const name of ["_startProfilerIdleNotifier", "_stopProfilerIdleNotifier", "_debugProcess", "_debugPause", "_debugEnd"]) {
      delete process[name];
    }
    if (init.cwdCounter instanceof Int32Array) {
      cwdCounter = init.cwdCounter;
      const rawCwd = process.cwd;
      let cachedCwd = "";
      let lastCounter = -1;
      process.cwd = function cwd() {
        const current = Atomics.load(cwdCounter, 0);
        if (current === lastCounter && cachedCwd !== "") return cachedCwd;
        lastCounter = current;
        cachedCwd = Reflect.apply(rawCwd, process, []);
        return cachedCwd;
      };
    }
  }

  // The worker's stdio: process.stdout/stderr write to the parent over the internal channel;
  // process.stdin reads what the parent writes to worker.stdin (empty without `stdin: true`).
  let workerStdin = null;
  let workerStdinPending = [];
  function workerStdinPush(chunk) {
    if (workerStdin === null) {
      workerStdinPending.push(chunk);
      return;
    }
    workerStdin.push(chunk === null ? null : Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength));
  }
  function setupWorkerStdio(internal, hasStdin) {
    const makeWritable = (stream) => {
      const { Writable } = lazyStream();
      const writable = new Writable({
        write(chunk, encoding, cb) {
          const bytes = typeof chunk === "string" ? Buffer.from(chunk, encoding) : chunk;
          internal.postMessage({
            type: "stdio",
            stream,
            chunk: new Uint8Array(bytes.buffer, bytes.byteOffset, bytes.byteLength).slice(),
          });
          cb();
        },
      });
      writable.fd = stream === "stdout" ? 1 : 2;
      return writable;
    };
    let stdout = null;
    let stderr = null;
    Object.defineProperty(process, "stdout", {
      configurable: true, enumerable: true,
      get: () => (stdout ??= makeWritable("stdout")),
    });
    Object.defineProperty(process, "stderr", {
      configurable: true, enumerable: true,
      get: () => (stderr ??= makeWritable("stderr")),
    });
    Object.defineProperty(process, "stdin", {
      configurable: true, enumerable: true,
      get() {
        if (workerStdin !== null) return workerStdin;
        const { Readable } = lazyStream();
        workerStdin = new Readable({
          read() {
            if (hasStdin && !this.readableEnded) internal.ref();
          },
        });
        workerStdin.fd = 0;
        workerStdin.on("end", () => internal.unref());
        workerStdin.on("pause", () => internal.unref());
        workerStdin.on("resume", () => {
          if (hasStdin && !workerStdin.readableEnded) internal.ref();
        });
        if (!hasStdin) workerStdin.push(null);
        const pending = workerStdinPending;
        workerStdinPending = [];
        for (const chunk of pending) workerStdinPush(chunk);
        return workerStdin;
      },
    });
  }

  function sendHeapSnapshot(internal, message) {
    let reply;
    try {
      const stream = __builtins.get("v8").getHeapSnapshot(message.options);
      const chunks = [];
      let chunk;
      while ((chunk = stream.read()) !== null) chunks.push(typeof chunk === "string" ? Buffer.from(chunk) : chunk);
      reply = { type: "heapSnapshot", id: message.id, data: Buffer.concat(chunks).toString() };
    } catch (error) {
      reply = { type: "heapSnapshot", id: message.id, error: serializeError(error) };
    }
    internal.postMessage(reply);
  }

  __builtins.set("worker_threads", wt);
}
