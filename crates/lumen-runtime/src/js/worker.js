(() => {
  "use strict";
  const __worker = globalThis.__worker;
  delete globalThis.__worker;
  // node:worker_threads (lumen-node glue) drives the same ops; hand them over via a hidden global.
  Object.defineProperty(globalThis, "__lumenWorkerOps", {
    value: __worker, configurable: true, enumerable: false, writable: false,
  });
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
      const isModule = options.type === "module";
      let path = String(scriptURL);
      if (path.startsWith("file://")) path = path.slice(7);
      this.#id = __worker.spawn(path, isModule, (kind, ...args) => this.#onEvent(kind, args)).id;
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
  // The class extends EventTarget, so it is built when first used (see lumen-web's `__lazyWeb`).
  Object.defineProperty(globalThis, "Worker", {
    get() { defineWorker(); return globalThis.Worker; },
    set(value) { Object.defineProperty(globalThis, "Worker", { value, writable: true, enumerable: true, configurable: true }); },
    enumerable: true, configurable: true,
  });
})();
