// Browser-facing ServiceWorkerContainer surface. State changes and script
// execution are owned by the host registry and isolated worker realms.
function serviceWorkerTransferList(value) {
  if (value === undefined) return undefined;
  if (value !== null && (typeof value === "object" || typeof value === "function") &&
      typeof value[Symbol.iterator] !== "function") return value.transfer;
  return value;
}
if (typeof __serviceWorker !== "undefined" && globalThis.__bitnestIsServiceWorker === true) {
  globalThis.__bitnestServiceWorkerOps = __serviceWorker;
  globalThis.__bitnestWorkerPostMessage = (data, transfer) => __serviceWorker.workerPostMessage(data, serviceWorkerTransferList(transfer));
  globalThis.__bitnestWorkerFetchResponse = (id, dataJSON) => __serviceWorker.workerFetchResponse(id, dataJSON);
  globalThis.__bitnestWorkerFetchFallback = id => __serviceWorker.workerFetchFallback(id);
  globalThis.__bitnestWorkerFetchError = (id, message) => __serviceWorker.workerFetchError(id, message);
  globalThis.postMessage = function(data, transfer) {
    return globalThis.__bitnestWorkerPostMessage(data, transfer);
  };
}
if (typeof __serviceWorker !== "undefined" && globalThis.navigator) {
  const serviceWorkerInstances = new Map();
  class ServiceWorker extends EventTarget {
    constructor(registration) {
      super();
      this.scriptURL = registration.scriptURL;
      this._registrationId = Number(registration.id);
      this.state = registration.state === "activated" ? "activated" : registration.state;
      this.onstatechange = null;
    }
    postMessage(data, transfer) {
      return __serviceWorker.postMessage(this._registrationId, data, serviceWorkerTransferList(transfer));
    }
  }
  function serviceWorker(record) {
    if (!record) return null;
    const key = Number(record.id);
    let worker = serviceWorkerInstances.get(key);
    if (!worker) {
      worker = new ServiceWorker(record);
      serviceWorkerInstances.set(key, worker);
    } else {
      const nextState = record.state === "activated" ? "activated" : record.state;
      if (worker.state !== nextState) {
        worker.state = nextState;
        queueMicrotask(() => {
          const event = new Event("statechange");
          worker.dispatchEvent(event);
          if (typeof worker.onstatechange === "function") worker.onstatechange.call(worker, event);
        });
      }
    }
    return worker;
  }

  class ServiceWorkerRegistration extends EventTarget {
    constructor(record) {
      super();
      this._id = Number(record.id);
      this.scope = record.scope;
      this.installing = serviceWorker(record.installing);
      this.waiting = serviceWorker(record.waiting);
      this.active = serviceWorker(record.active);
      this.updateViaCache = "imports";
      this.onupdatefound = null;
    }
    unregister() { return __serviceWorker.unregister(this._id); }
    update() {
      return __serviceWorker.update(this._id).then(() => {
        const record = JSON.parse(__serviceWorker.registrations()).find(item => item.scope === this.scope);
        if (record) refreshRegistration(this, record);
        return this;
      });
    }
  }

  const registrationInstances = new Map();
  const observedInstallers = new Map();
  function refreshRegistration(instance, record) {
    instance._id = Number(record.id);
    instance.installing = serviceWorker(record.installing);
    instance.waiting = serviceWorker(record.waiting);
    instance.active = serviceWorker(record.active);
    instance.updateViaCache = record.updateViaCache || instance.updateViaCache || "imports";
    return instance;
  }
  function registration(record) {
    if (!record) return undefined;
    let instance = registrationInstances.get(record.scope);
    if (!instance) {
      instance = new ServiceWorkerRegistration(record);
      registrationInstances.set(record.scope, instance);
    } else {
      refreshRegistration(instance, record);
    }
    return instance;
  }

  class ServiceWorkerContainer extends EventTarget {
    constructor() {
      super();
      this._readyWaiters = [];
    }
    register(scriptURL, options = {}) {
      if (arguments.length === 0) return Promise.reject(new TypeError("register requires a script URL"));
      if (options === null || typeof options !== "object") return Promise.reject(new TypeError("register options must be an object"));
      const script = new URL(String(scriptURL), location.href).href;
      const scope = options.scope === undefined ? undefined : new URL(String(options.scope), location.href).href;
      const type = options.type === undefined ? "classic" : String(options.type);
      if (type !== "classic" && type !== "module") return Promise.reject(new TypeError("invalid ServiceWorker script type"));
      return __serviceWorker.register(script, scope, type).then(id => {
        const records = JSON.parse(__serviceWorker.registrations());
        const record = records.find(item => Number(item.id) === Number(id));
        return registration(record || {
          id, scriptURL: script, scope: scope || script.slice(0, script.lastIndexOf("/") + 1), state: "activated"
        });
      });
    }
    getRegistration(clientURL = location.href) {
      const url = new URL(String(clientURL), location.href).href;
      const records = JSON.parse(__serviceWorker.registrations())
        .filter(item => url.startsWith(item.scope))
        .sort((a, b) => b.scope.length - a.scope.length);
      return Promise.resolve(records.length ? registration(records[0]) : undefined);
    }
    getRegistrations() {
      return Promise.resolve(JSON.parse(__serviceWorker.registrations()).map(registration));
    }
    get ready() {
      const records = JSON.parse(__serviceWorker.registrations())
        .filter(item => item.active && location.href.startsWith(item.scope))
        .sort((a, b) => b.scope.length - a.scope.length);
      if (records.length) return Promise.resolve(registration(records[0]));
      return new Promise(resolve => this._readyWaiters.push(resolve));
    }
    get controller() {
      const record = JSON.parse(__serviceWorker.controller());
      return serviceWorker(record);
    }
  }

  Object.defineProperty(globalThis.navigator, "serviceWorker", {
    configurable: true,
    enumerable: true,
    value: new ServiceWorkerContainer()
  });
  // Called by the shell's bounded browser-services pump, so an idle page
  // does not wake periodically just to check for worker messages.
  globalThis.__bitnestPumpServiceWorkerContainer = () => {
    const container = globalThis.navigator.serviceWorker;
    let messages;
    try { messages = JSON.parse(__serviceWorker.takeMessages()); } catch (_) { messages = []; }
    for (const message of messages) {
      let data;
      try { data = __serviceWorker.takeMessageData(message.messageId); } catch (_) { continue; }
      const source = serviceWorker(message.source);
      container.dispatchEvent(new MessageEvent("message", { data, source }));
    }
    for (const record of JSON.parse(__serviceWorker.registrations())) {
      const installingId = record.installing ? Number(record.installing.id) : null;
      const previousId = observedInstallers.get(record.scope);
      if (installingId !== null && installingId !== previousId) {
        const target = registration(record);
        const event = new Event("updatefound");
        target.dispatchEvent(event);
        if (typeof target.onupdatefound === "function") target.onupdatefound.call(target, event);
      }
      if (installingId === null) observedInstallers.delete(record.scope);
      else observedInstallers.set(record.scope, installingId);
    }
    if (container._readyWaiters.length) {
      const records = JSON.parse(__serviceWorker.registrations())
        .filter(item => item.active && location.href.startsWith(item.scope))
        .sort((a, b) => b.scope.length - a.scope.length);
      if (records.length) {
        const readyRegistration = registration(records[0]);
        for (const resolve of container._readyWaiters.splice(0)) resolve(readyRegistration);
      }
    }
  };
  Object.defineProperty(globalThis, "ServiceWorker", { configurable: true, value: ServiceWorker });
  Object.defineProperty(globalThis, "ServiceWorkerRegistration", { configurable: true, value: ServiceWorkerRegistration });
  Object.defineProperty(globalThis, "ServiceWorkerContainer", { configurable: true, value: ServiceWorkerContainer });
}
