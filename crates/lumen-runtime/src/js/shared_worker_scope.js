(() => {
  "use strict";
  const globalScope = globalThis;
  const report = (message) => {
    try { globalThis.onerror(String(message)); }
    catch { /* an error reporter must not interrupt delivery of later client connections */ }
  };

  // SharedWorkerGlobalScope inherits the common WorkerGlobalScope EventTarget implementation.
  // Its event-handler IDL attribute is a listener on that same target, so addEventListener,
  // dispatchEvent, currentTarget, and `this` all observe the actual realm global.
  const assertSharedGlobal = (receiver) => {
    if (receiver !== globalScope) throw new TypeError("Illegal invocation");
  };
  const connectHandlers = new WeakMap();
  const connectListeners = new WeakMap();
  Object.defineProperty(SharedWorkerGlobalScope.prototype, "onconnect", {
    configurable: true,
    enumerable: true,
    get() {
      assertSharedGlobal(this);
      return connectHandlers.get(this) ?? null;
    },
    set(handler) {
      assertSharedGlobal(this);
      const old = connectListeners.get(this);
      if (old) this.removeEventListener("connect", old);
      connectHandlers.delete(this);
      connectListeners.delete(this);
      if (typeof handler === "function") {
        const listener = (event) => handler.call(this, event);
        connectHandlers.set(this, handler);
        connectListeners.set(this, listener);
        this.addEventListener("connect", listener);
      }
    },
  });
  // The engine's global environment can create an own data property for a bare assignment to an
  // inherited accessor. Keep the Web IDL accessor on the interface prototype and forward the
  // global binding through it so both `onconnect = handler` and `self.onconnect = handler` work.
  const onconnect = Object.getOwnPropertyDescriptor(SharedWorkerGlobalScope.prototype, "onconnect");
  Object.defineProperty(globalScope, "onconnect", {
    configurable: true,
    enumerable: true,
    get() { return Reflect.apply(onconnect.get, globalScope, []); },
    set(handler) { return Reflect.apply(onconnect.set, globalScope, [handler]); },
  });

  globalThis.__sharedWorkerDispatchEvent = (kind, portId) => {
    if (kind !== "connect") return;
    try {
      if (!(globalScope instanceof SharedWorkerGlobalScope)) {
        throw new TypeError("connect events require a SharedWorkerGlobalScope");
      }
      const port = globalThis.__lumenSharedPorts.create(portId);
      const init = { source: port, ports: [port] };
      const trustEvent = globalThis.__eventTargetInternals?.kTrustEvent;
      if (trustEvent) init[trustEvent] = true;
      const event = new MessageEvent("connect", init);
      globalScope.dispatchEvent(event);
    } catch (error) {
      report(error?.stack ?? error);
    }
  };
})();
