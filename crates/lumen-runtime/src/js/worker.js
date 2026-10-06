(() => {
  "use strict";
  const __worker = globalThis.__worker;
  delete globalThis.__worker;
  // node:worker_threads (lumen-node glue) drives the same ops; hand them over via a hidden global.
  Object.defineProperty(globalThis, "__lumenWorkerOps", {
    value: __worker, configurable: true, enumerable: false, writable: false,
  });
  const __sharedWorker = globalThis.__lumenSharedWorker;
  delete globalThis.__lumenSharedWorker;
  const serialize = (value, transfer) => globalThis.__serializeForClone(value, transfer, true);
  const deserialize = (bytes) => globalThis.__deserializeClone(bytes);

  const defineWorker = () => {
  class Worker extends EventTarget {
    #id;
    #terminated = false;
    constructor(scriptURL, options = {}) {
      super();
      if (arguments.length === 0) throw new TypeError("Worker requires a scriptURL");
      options = options && typeof options === "object" ? options : {};
      const type = options.type === undefined ? "classic" : String(options.type);
      if (type !== "classic" && type !== "module") {
        throw new TypeError("Worker type must be 'classic' or 'module'");
      }
      const isModule = type === "module";
      const workerName = options.name === undefined ? "" : String(options.name);
      const location = globalThis.location;
      let path = String(scriptURL);
      let spawnOptions;
      if (location && typeof location.href === "string") {
        let url;
        try { url = new URL(path, location.href); }
        catch (error) { throw new DOMException(String(error?.message ?? error), "SyntaxError"); }
        if (url.origin !== location.origin) {
          throw new DOMException("Worker script must be same-origin", "SecurityError");
        }
        if (url.protocol === "http:" || url.protocol === "https:") {
          path = url.href;
          spawnOptions = { web: true, ownerOrigin: location.origin, name: workerName };
        } else if (url.protocol === "file:") {
          path = url.href;
          spawnOptions = { web: true, ownerOrigin: location.origin, name: workerName };
        } else {
          throw new DOMException(`Unsupported worker URL scheme '${url.protocol}'`, "NotSupportedError");
        }
      } else if (path.startsWith("file://")) {
        path = path.slice(7);
      }
      this.#id = __worker.spawn(path, isModule,
        (kind, ...args) => this.#onEvent(kind, args), spawnOptions).id;
    }
    postMessage(message, _transfer) {
      if (this.#terminated) return;
      let bytes;
      try { bytes = serialize(message); }
      catch (e) { throw e; } // DataCloneError surfaces to the caller
      __worker.post(this.#id, bytes);
    }
    terminate() {
      if (this.#terminated) return;
      this.#terminated = true;
      __worker.terminate(this.#id);
    }
    #fire(type, event) {
      const h = this["on" + type];
      if (typeof h === "function") { try { h.call(this, event); } catch (e) { reportError(e); } }
      this.dispatchEvent(event);
    }
    #onEvent(kind, args) {
      // A message the worker posted before `terminate()` reached it may still be in flight;
      // a terminated Worker dispatches nothing but its exit.
      if (this.#terminated && kind !== "exit") return;
      if (kind === "message") {
        let data;
        try { data = deserialize(args[0]); }
        catch { this.#fire("messageerror", new MessageEvent("messageerror", {})); return; }
        this.#fire("message", new MessageEvent("message", { data }));
      } else if (kind === "error") {
        this.#fire("error", new ErrorEvent("error", { message: args[0] }));
      } else if (kind === "exit") {
        this.#terminated = true;
      }
      // "online" is a node-mode event; the web Worker has no counterpart and ignores it.
    }
  }
  for (const name of ["message", "messageerror", "error"]) {
    Object.defineProperty(Worker.prototype, "on" + name, {
      configurable: true, enumerable: true, writable: true, value: null,
    });
  }
  Object.defineProperty(globalThis, "Worker", { value: Worker, writable: true, enumerable: true, configurable: true });
  };
  // The class extends EventTarget, so it is built when first used (`EventTarget` itself is a lazy native global).
  Object.defineProperty(globalThis, "Worker", {
    get() { defineWorker(); return globalThis.Worker; },
    set(value) { Object.defineProperty(globalThis, "Worker", { value, writable: true, enumerable: true, configurable: true }); },
    enumerable: true, configurable: true,
  });

  const defineSharedWorker = () => {
    class SharedWorker extends EventTarget {
      #id;
      #closed = false;
      constructor(scriptURL, options = {}) {
        super();
        if (arguments.length === 0) throw new TypeError("SharedWorker requires a scriptURL");
        if (typeof options === "string") options = { name: options };
        if (options === null || typeof options !== "object") options = {};
        const type = options.type === undefined ? "classic" : String(options.type);
        if (type !== "classic" && type !== "module") throw new TypeError("SharedWorker type must be 'classic' or 'module'");
        const location = globalThis.location;
        const process = globalThis.process;
        const base = location?.href ?? (process?.cwd ? `file://${process.cwd().replaceAll("\\", "/")}/` : "file:///" );
        const url = new URL(String(scriptURL), base);
        const origin = location?.origin ?? url.origin;
        if (location && url.origin !== location.origin) throw new DOMException("SharedWorker script must be same-origin", "SecurityError");
        const name = options.name === undefined ? "" : String(options.name);
        const connected = __sharedWorker.connect(url.href, origin, type === "module", name,
          (kind, ...args) => this.#onEvent(kind, args));
        this.#id = connected.id;
        Object.defineProperty(this, "port", {
          configurable: false,
          enumerable: true,
          writable: false,
          value: globalThis.__lumenSharedPorts.create(connected.port, () => {
            __sharedWorker.disconnect(this.#id);
          }),
        });
      }
      #fire(type, event) {
        const handler = this["on" + type];
        if (typeof handler === "function") { try { handler.call(this, event); } catch (error) { reportError(error); } }
        this.dispatchEvent(event);
      }
      #onEvent(kind, args) {
        if (this.#closed) return;
        if (kind === "error") this.#fire("error", new ErrorEvent("error", { message: args[0] }));
        else if (kind === "close") {
          this.#closed = true;
          this.#fire("close", new Event("close"));
        }
      }
    }
    for (const name of ["error", "close"]) {
      Object.defineProperty(SharedWorker.prototype, "on" + name, {
        configurable: true, enumerable: true, writable: true, value: null,
      });
    }
    Object.defineProperty(globalThis, "SharedWorker", { value: SharedWorker, writable: true, enumerable: true, configurable: true });
  };
  Object.defineProperty(globalThis, "SharedWorker", {
    get() { defineSharedWorker(); return globalThis.SharedWorker; },
    set(value) { Object.defineProperty(globalThis, "SharedWorker", { value, writable: true, enumerable: true, configurable: true }); },
    enumerable: true, configurable: true,
  });
})();
