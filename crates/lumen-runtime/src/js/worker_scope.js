(() => {
  "use strict";
  const post = __wself.post;
  const closeSelf = __wself.close;
  const report = __wself.report;
  const loadClassicScript = __wself.loadClassicScript;
  const workerType = globalThis.__lumenWorkerType === "module" ? "module" : "classic";
  const workerName = String(globalThis.__lumenWorkerName ?? "");
  const isSharedWorker = globalThis.__lumenWorkerShared === true;
  const globalEval = globalThis.eval;
  let locationUrl;
  try {
    locationUrl = new URL(globalThis.__lumenWorkerLocationHref || "file:///", "file:///" );
  } catch {
    locationUrl = new URL("file:///" );
  }
  delete globalThis.__lumenWorkerType;
  delete globalThis.__lumenWorkerName;
  delete globalThis.__lumenWorkerShared;
  delete globalThis.__lumenWorkerLocationHref;
  delete globalThis.__wself;
  const serialize = (value, transfer) => globalThis.__serializeForClone(value, transfer, true);
  const deserialize = (bytes) => globalThis.__deserializeClone(bytes);

  // These interface objects form the prototype chain of this realm's actual global object, so
  // interface checks and Web IDL prototype inspection see the selected worker global scope.
  const defaultHasInstance = Function.prototype[Symbol.hasInstance];
  class WorkerGlobalScope extends EventTarget {
    constructor() { throw new TypeError("Illegal constructor"); }
  }
  class DedicatedWorkerGlobalScope extends WorkerGlobalScope {
    constructor() { throw new TypeError("Illegal constructor"); }
  }
  class SharedWorkerGlobalScope extends WorkerGlobalScope {
    constructor() { throw new TypeError("Illegal constructor"); }
  }
  const globalScopeInterface = isSharedWorker ? SharedWorkerGlobalScope : DedicatedWorkerGlobalScope;
  for (const interfaceObject of [WorkerGlobalScope, DedicatedWorkerGlobalScope, SharedWorkerGlobalScope]) {
    Object.defineProperty(interfaceObject, Symbol.hasInstance, {
      configurable: true,
      value(instance) {
        return (instance === globalThis && (this === WorkerGlobalScope || this === globalScopeInterface)) ||
          defaultHasInstance.call(this, instance);
      },
    });
  }

  const workerLocationValues = new WeakMap();
  class WorkerLocation {
    constructor() { throw new TypeError("Illegal constructor"); }
    toString() { return workerLocationValues.get(this)?.href ?? ""; }
  }
  for (const [property, read] of [
    ["href", (url) => url.href],
    ["origin", (url) => url.origin],
    ["protocol", (url) => url.protocol],
    ["host", (url) => url.host],
    ["hostname", (url) => url.hostname],
    ["port", (url) => url.port],
    ["pathname", (url) => url.pathname],
    ["search", (url) => url.search],
    ["hash", (url) => url.hash],
  ]) {
    Object.defineProperty(WorkerLocation.prototype, property, {
      configurable: true,
      enumerable: true,
      get() {
        const url = workerLocationValues.get(this);
        return url ? read(url) : "";
      },
    });
  }
  Object.defineProperty(WorkerLocation.prototype, Symbol.toStringTag, { value: "WorkerLocation" });
  const workerLocation = Object.create(WorkerLocation.prototype);
  workerLocationValues.set(workerLocation, locationUrl);
  Object.freeze(workerLocation);

  // `EventTarget` keeps its brand and listener table on private symbols. Initialize those same
  // slots on the engine-owned global so the inherited methods work with `self` as their receiver.
  const eventTargetSeed = new EventTarget();
  for (const key of Reflect.ownKeys(eventTargetSeed)) {
    if (typeof key === "symbol") {
      Object.defineProperty(globalThis, key, {
        configurable: true, enumerable: false, writable: true, value: eventTargetSeed[key],
      });
    }
  }

  // EventTarget is the parent interface of WorkerGlobalScope. Teach the constructor's brand
  // check about the engine-owned global while leaving every other EventTarget subclass intact.
  if (typeof EventTarget === "function") {
    Object.defineProperty(EventTarget, Symbol.hasInstance, {
      configurable: true,
      value(instance) {
        return (this === EventTarget && instance === globalThis) || defaultHasInstance.call(this, instance);
      },
    });
  }
  // Preserve the runtime's prior global prototype at the tail of the worker interface chain.
  // This makes the actual realm global inherit the same interface prototypes as a browser worker.
  try {
    const priorGlobalPrototype = Object.getPrototypeOf(globalThis);
    if (Object.getPrototypeOf(EventTarget.prototype) !== priorGlobalPrototype) {
      Object.setPrototypeOf(EventTarget.prototype, priorGlobalPrototype);
    }
    Object.setPrototypeOf(globalThis, globalScopeInterface.prototype);
  } catch {}
  Object.defineProperties(WorkerGlobalScope.prototype, {
    self: { configurable: true, enumerable: true, get: () => globalThis },
    location: { configurable: true, enumerable: true, get: () => workerLocation },
    type: { configurable: true, enumerable: true, get: () => workerType },
    close: { configurable: true, writable: true, value: () => closeSelf() },
    importScripts: {
      configurable: true,
      writable: true,
      value: (...urls) => {
        if (workerType === "module") throw new TypeError("importScripts is unavailable in module workers");
        const resolved = urls.map((input) => {
          try { return new URL(String(input), locationUrl.href).href; }
          catch (error) { throw new DOMException(String(error?.message ?? error), "SyntaxError"); }
        });
        for (const url of resolved) {
          let resource;
          try { resource = loadClassicScript(url); }
          catch (error) {
            throw new DOMException(String(error?.message ?? error), "NetworkError");
          }
          let source = String(resource.source);
          if (source.startsWith("\uFEFF")) source = source.slice(1);
          // Indirect eval runs as a global classic script in this realm. That shares the engine's
          // normal script evaluator while the native op above reuses lumen-web's HTTP/TLS loader.
          globalEval(`${source}\n//# sourceURL=${String(resource.url)}`);
        }
      },
    },
  });
  Object.defineProperties(DedicatedWorkerGlobalScope.prototype, {
    name: { configurable: true, enumerable: true, get: () => workerName },
    postMessage: { configurable: true, writable: true, value: (message, _transfer) => post(serialize(message)) },
  });
  Object.defineProperties(SharedWorkerGlobalScope.prototype, {
    name: {
      configurable: true,
      enumerable: true,
      get() {
        if (this !== globalThis) throw new TypeError("Illegal invocation");
        return workerName;
      },
      set(value) {
        if (this !== globalThis) throw new TypeError("Illegal invocation");
        Object.defineProperty(this, "name", {
          configurable: true,
          enumerable: true,
          writable: true,
          value,
        });
      },
    },
  });
  Object.defineProperty(WorkerGlobalScope.prototype, Symbol.toStringTag, { configurable: true, value: "WorkerGlobalScope" });
  Object.defineProperty(DedicatedWorkerGlobalScope.prototype, Symbol.toStringTag, { configurable: true, value: "DedicatedWorkerGlobalScope" });
  Object.defineProperty(SharedWorkerGlobalScope.prototype, Symbol.toStringTag, { configurable: true, value: "SharedWorkerGlobalScope" });
  // The engine's global environment can otherwise create an own data property when an
  // unqualified assignment targets an inherited accessor. Install the readonly global
  // binding directly as an accessor so `self = value` keeps resolving to this worker scope.
  Object.defineProperty(globalThis, "self", {
    configurable: true,
    enumerable: true,
    get: () => globalThis,
  });
  // Global interface operations are also exposed as properties of the global object. A bare
  // call from a strict worker script supplies `undefined` as `this`, even though the operation
  // belongs to this WorkerGlobalScope. Default only a nullish receiver to this worker global;
  // forwarding every other receiver preserves EventTarget's ordinary brand check.
  const eventTargetOperations = {
    addEventListener: EventTarget.prototype.addEventListener,
    removeEventListener: EventTarget.prototype.removeEventListener,
    dispatchEvent: EventTarget.prototype.dispatchEvent,
  };
  const workerGlobalOperations = {
    addEventListener(type, listener, options = undefined) {
      const receiver = this === undefined || this === null ? globalThis : this;
      return Reflect.apply(eventTargetOperations.addEventListener, receiver, arguments);
    },
    removeEventListener(type, listener, options = undefined) {
      const receiver = this === undefined || this === null ? globalThis : this;
      return Reflect.apply(eventTargetOperations.removeEventListener, receiver, arguments);
    },
    dispatchEvent(event) {
      const receiver = this === undefined || this === null ? globalThis : this;
      return Reflect.apply(eventTargetOperations.dispatchEvent, receiver, arguments);
    },
  };
  for (const name of Object.keys(workerGlobalOperations)) {
    Object.defineProperty(globalThis, name, {
      configurable: true,
      enumerable: true,
      writable: true,
      value: workerGlobalOperations[name],
    });
  }
  const exposedInterfaces = [
    ["WorkerGlobalScope", WorkerGlobalScope],
    [isSharedWorker ? "SharedWorkerGlobalScope" : "DedicatedWorkerGlobalScope", globalScopeInterface],
    ["WorkerLocation", WorkerLocation],
  ];
  for (const [name, interfaceObject] of exposedInterfaces) {
    Object.defineProperty(globalThis, name, {
      configurable: true, enumerable: false, writable: true, value: interfaceObject,
    });
  }
  try {
    Object.defineProperty(globalThis, Symbol.toStringTag, {
      configurable: true, value: isSharedWorker ? "SharedWorkerGlobalScope" : "DedicatedWorkerGlobalScope",
    });
  } catch {}

  // Dedicated-worker message events target its actual scope. Shared workers exchange messages
  // through per-connection MessagePorts and receive only the shared-scope connect event.
  const target = globalThis;

  if (isSharedWorker) {
    delete globalThis.onmessage;
    delete globalThis.onmessageerror;
  } else {
    globalThis.onmessage = null;
    globalThis.onmessageerror = null;
  }

  const fire = (type, event) => {
    const h = globalThis["on" + type];
    if (typeof h === "function") { try { h.call(globalThis, event); } catch (e) { reportError(e); } }
    target.dispatchEvent(event);
  };

  if (!isSharedWorker) {
    globalThis.__workerDispatchMessage = (bytes) => {
      if (bytes === false) return; // channel-closed sentinel
      let data;
      try { data = deserialize(bytes); }
      catch { fire("messageerror", new MessageEvent("messageerror", {})); return; }
      fire("message", new MessageEvent("message", { data }));
    };
  }

  // A worker-side uncaught error propagates to the parent's Worker.onerror.
  globalThis.onerror = (message) => { report(String(message)); return true; };
})();
