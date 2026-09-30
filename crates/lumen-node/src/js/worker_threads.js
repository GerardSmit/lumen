// node:worker_threads — REAL workers: one OS thread + one fresh engine realm per Worker, over the
// runtime's __worker/__wself ops (lumen-runtime/src/worker.rs). Messages cross the thread boundary
// as structured-clone wire bytes (__serializeForClone, lumen-web/src/js/serialize.js).
//
// Real: new Worker(filename | URL | source-with-{eval:true}) with workerData/argv/env options,
// postMessage both ways, 'online'/'message'/'messageerror'/'error'/'exit', terminate() (resolves
// with the exit code), worker.ref()/unref(), threadId, parentPort (postMessage/'message'/ref/
// unref/close with Node's keep-alive-while-listening semantics), isMainThread, workerData,
// get/setEnvironmentData (snapshot inherited by new workers), receiveMessageOnPort, and the
// native transferable MessageChannel/MessagePort pair and shared ArrayBuffer backing. process.exit(code) in a worker stops only that
// worker; an uncaught exception emits 'error' on the parent and exits the worker with code 1.
//
// Honest throws (semantics lumen cannot honor): SHARE_ENV (realms snapshot the env; there is no
// shared store), unsupported transfer types/resizable buffers, stdin/stdout/stderr capture options (workers share the process stdio),
// moveMessagePortToContext, postMessageToThread. BroadcastChannel is the same-realm web one.
{
  const EventEmitter = __builtins.get("events");
  const pathMod = __builtins.get("path");

  const SHARE_ENV = Symbol.for("nodejs.worker_threads.SHARE_ENV");
  const environmentData = new Map();
  const untransferable = new WeakSet();
  const uncloneable = new WeakSet();

  const ser = (v, transfer) => globalThis.__serializeForClone(v, transfer, true);
  const deser = (b) => globalThis.__deserializeClone(b);

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

  // Worker-side uncaught errors travel as "Name: message" text; rebuild a matching Error here.
  function reviveError(text) {
    const m = /^([A-Za-z][A-Za-z0-9_$]*(?:Error|Exception)): ([\s\S]*)$/.exec(String(text));
    const Ctor = m && typeof globalThis[m[1]] === "function" ? globalThis[m[1]] : Error;
    return m ? new Ctor(m[2]) : new Error(String(text));
  }

  let terminateCallbackWarned = false;
  class Worker extends EventEmitter {
    #id;
    #exited = false;
    #exitCode = null;
    #exitResolvers = [];
    constructor(filename, options = {}) {
      super();
      options = options && typeof options === "object" ? options : {};
      const ops = getWorkerOps();
      for (const opt of ["stdin", "stdout", "stderr"]) {
        if (options[opt]) {
          throw new Error(`worker_threads option '${opt}' is not supported in lumen (workers share the process stdio)`);
        }
      }
      const shareEnv = options.env === SHARE_ENV;
      let entry;
      let isModule = false;
      if (options.eval) {
        entry = String(filename);
      } else {
        let p = filename;
        if (typeof URL === "function" && p instanceof URL) p = p.href;
        p = String(p);
        if (p.startsWith("file:")) p = __builtins.get("url").fileURLToPath(p);
        // Node's check: absolute for this platform, or explicitly relative (`./x`, `..\x`).
        if (!pathMod.isAbsolute(p) && !/^\.\.?[\\/]/.test(p)) {
          const error = new TypeError(
            "The worker script or module filename must be an absolute path or a relative path " +
              `starting with './' or '../'. Received ${JSON.stringify(p)}`,
          );
          error.code = "ERR_WORKER_PATH";
          throw error;
        }
        p = pathMod.resolve(p);
        isModule = p.endsWith(".mjs");
        entry = p;
      }
      // Snapshot the environment (Node copies the parent's process.env unless overridden).
      const envSrc = options.env == null || shareEnv ? process.env : options.env;
      if (typeof envSrc !== "object") throw new TypeError("options.env must be an object");
      const env = shareEnv ? null : {};
      if (!shareEnv) for (const k of Object.keys(envSrc)) {
        const v = envSrc[k];
        if (v !== undefined) env[k] = String(v);
      }
      const argv = Array.isArray(options.argv) ? options.argv.map(String) : [];
      // One structured-clone payload carries everything the worker realm needs at boot; a
      // DataCloneError from workerData surfaces to the caller, like Node.
      const init = ser({
        workerData: options.workerData,
        argv,
        env,
        envData: environmentData,
        entry: options.eval ? "[worker eval]" : entry,
      }, options.transferList);
      const res = ops.spawn(entry, isModule, (kind, a) => this.#onEvent(kind, a), {
        node: true,
        eval: !!options.eval,
        shareEnv,
        init,
      });
      this.#id = res.id;
      this.threadId = res.threadId;
      this.resourceLimits = {};
      this.performance = { eventLoopUtilization: () => ({ idle: 0, active: 0, utilization: 0 }) };
      // stdio capture is unsupported (the options throw above); worker console output goes
      // straight to the shared process streams.
      this.stdin = null;
      this.stdout = null;
      this.stderr = null;
    }
    #onEvent(kind, a) {
      if (kind === "online") {
        this.emit("online");
      } else if (kind === "message") {
        let data;
        try {
          data = deser(a);
        } catch (e) {
          this.emit("messageerror", e);
          return;
        }
        this.emit("message", data);
      } else if (kind === "error") {
        this.emit("error", reviveError(a));
      } else if (kind === "exit") {
        this.#exited = true;
        this.#exitCode = a;
        for (const resolve of this.#exitResolvers.splice(0)) resolve(a);
        this.emit("exit", a);
      }
    }
    postMessage(value, transferList) {
      if (this.#exited) return;
      getWorkerOps().post(this.#id, ser(value, transferList));
    }
    terminate(callback) {
      // Legacy callback form, still honored by Node alongside the promise.
      if (typeof callback === "function") {
        if (!terminateCallbackWarned) {
          terminateCallbackWarned = true;
          process.emitWarning("Passing a callback to worker.terminate() is deprecated. It returns a Promise instead.",
            "DeprecationWarning", "DEP0132");
        }
        this.once("exit", (code) => callback(null, code));
      }
      if (this.#exited) return Promise.resolve(this.#exitCode);
      // The returned promise keeps the loop alive until the exit, even for an unref()'d worker.
      getWorkerOps().setRef(this.#id, true);
      getWorkerOps().terminate(this.#id);
      return new Promise((resolve) => this.#exitResolvers.push(resolve));
    }
    ref() {
      getWorkerOps().setRef(this.#id, true);
      return this;
    }
    unref() {
      getWorkerOps().setRef(this.#id, false);
      return this;
    }
    getHeapSnapshot() {
      return Promise.reject(new Error("worker.getHeapSnapshot is not supported in lumen"));
    }
    getHeapStatistics() {
      return Promise.reject(new Error("worker.getHeapStatistics is not supported in lumen"));
    }
  }

  const WebPort = globalThis.MessagePort;
  const portState = new WeakMap();
  const localPorts = new Map();
  function portOps() { return globalThis.__lumenPorts; }
  class NodeMessagePort extends WebPort {
    constructor() { throw new TypeError("Illegal constructor"); }
    postMessage(value, transferList) {
      const state = portState.get(this);
      if (!state || state.closed || state.detached) return;
      if (transferList?.includes(this)) throw new DOMException("Cannot transfer the source port", "DataCloneError");
      const bytes = ser(value, transferList);
      portOps().post(state.id, bytes);
    }
    addEventListener(type, callback, options) {
      EventTarget.prototype.addEventListener.call(this,type,callback,options);
      if (type === "message" && callback != null) { this.start(); this.ref(); }
    }
    removeEventListener(type, callback, options) {
      EventTarget.prototype.removeEventListener.call(this,type,callback,options);
      if(type === "message" && this.listenerCount("message")===0 && !(this._listeners.get("message")?.length)) this.unref();
    }
    start() { const state=portState.get(this);if(state)state.started=true;this._arm(); }
    _arm() {
      const state = portState.get(this);
      if (!state || state.closed || state.detached || state.timer !== null) return;
      state.timer = setInterval(() => {
        // A peer close follows its already-queued messages. Poll drains the queue before
        // returning the close sentinel; observing the closed flag first would lose receipts.
        if(!state.started){if(portOps().isClosed(state.id))this._finishClose();return;}
        const bytes = portOps().poll(state.id);
        if (bytes === false) { this._finishClose(); return; }
        if (bytes === undefined) return;
        let data;
        try { data = deser(bytes); } catch(error) { this.emit("messageerror", error); return; }
        this.dispatchEvent(new MessageEvent("message", {data}));
        this.emit("message", data);
      }, 1);
      if (!state.ref) state.timer.unref();
    }
    ref() { const state=portState.get(this); if (state && !state.closed && !state.detached) {state.ref=true;this._arm();state.timer?.ref();} return this; }
    unref() { const state=portState.get(this); if (state) {state.ref=false;state.timer?.unref();} return this; }
    hasRef() { const state=portState.get(this); return !!state && !state.closed && !state.detached && state.ref; }
    _finishClose() { const state=portState.get(this); if(!state || state.closed)return;state.closed=true;portOps().detach(state.id);localPorts.delete(state.id);if(state.timer!==null)clearInterval(state.timer);state.timer=null;queueMicrotask(()=>this.emit("close")); }
    close() { const state=portState.get(this); if(!state||state.closed||state.detached)return;const peer=portOps().close(state.id);this._finishClose();localPorts.get(peer)?._arm(); }
  }
  for (const name of ["on", "addListener", "once", "off", "removeListener", "removeAllListeners", "prependListener", "prependOnceListener", "emit", "listeners", "rawListeners", "listenerCount", "eventNames", "setMaxListeners", "getMaxListeners"]) {
    Object.defineProperty(NodeMessagePort.prototype,name,{configurable:true,writable:true,value:function(...args){
      const result=EventEmitter.prototype[name].apply(this,args);
      if (["on","addListener","once","prependListener","prependOnceListener"].includes(name)) {if(args[0]==="message"){this.start();this.ref();}else if(args[0]==="close")this._arm();}
      else if (["off","removeListener","removeAllListeners"].includes(name)&&this.listenerCount("message")===0)this.unref();
      return result;
    }});
  }
  function nodePort(id) {
    const port=new EventTarget();
    Object.setPrototypeOf(port,NodeMessagePort.prototype);
    EventEmitter.call(port);
    portState.set(port,{id,timer:null,closed:false,detached:false,ref:false,started:false});
    localPorts.set(id,port);
    return port;
  }
  class NodeMessageChannel {constructor(){const pair=portOps().pair();this.port1=nodePort(pair.a);this.port2=nodePort(pair.b);}}
  function receiveMessageOnPort(port) {
    const state=portState.get(port);
    if(!state)throw new TypeError("receiveMessageOnPort expects a MessagePort");
    if(state.closed||state.detached)return undefined;
    const bytes=portOps().poll(state.id);
    if(bytes===false){port._finishClose();return undefined;}
    return bytes===undefined?undefined:{message:deser(bytes)};
  }
  Object.defineProperty(globalThis,"__lumenPortClone",{value:{
    isPort:value=>portState.has(value),
    isUntransferable:value=>untransferable.has(value),
    isUncloneable:value=>uncloneable.has(value),
    validate:port=>{const state=portState.get(port);if(!state || state.closed || state.detached || portOps().isClosed(state.id))throw new DOMException("MessagePort is detached or closed", "DataCloneError");},
    export:port=>{const state=portState.get(port);if(state.closed||state.detached)throw new DOMException("MessagePort is detached", "DataCloneError");return portOps().export(state.id);},
    detach:port=>{const state=portState.get(port);portOps().detach(state.id);state.detached=true;localPorts.delete(state.id);if(state.timer!==null)clearInterval(state.timer);state.timer=null;queueMicrotask(()=>port.emit("close"));},
    import:index=>nodePort(portOps().import(index)),
  },configurable:true});

  const notSupported = (name) =>
    function () {
      throw new Error(`worker_threads ${name} is not supported in lumen`);
    };

  const wt = {
    isMainThread: true,
    isInternalThread: false,
    threadId: 0,
    parentPort: null,
    workerData: undefined,
    SHARE_ENV,
    resourceLimits: {},
    Worker,
    // Native queues and exclusive transferred endpoint ownership across real worker realms.
    MessageChannel: NodeMessageChannel,
    MessagePort: NodeMessagePort,
    BroadcastChannel: globalThis.BroadcastChannel,
    markAsUntransferable: value => { if(value !== null && (typeof value === "object" || typeof value === "function")) untransferable.add(value); },
    isMarkedAsUntransferable: value => untransferable.has(value),
    markAsUncloneable: value => { if(value !== null && (typeof value === "object" || typeof value === "function")) uncloneable.add(value); },
    moveMessagePortToContext: notSupported("moveMessagePortToContext"),
    postMessageToThread: notSupported("postMessageToThread"),
    receiveMessageOnPort,
    setEnvironmentData: (key, value) => {
      if (value === undefined) environmentData.delete(key);
      else environmentData.set(key, value);
    },
    getEnvironmentData: (key) => environmentData.get(key),
  };

  // The worker-realm bootstrap. NODE_WORKER_SCOPE_JS (lumen-runtime/src/worker.rs) calls this in
  // each node worker realm BEFORE the worker entry runs: it flips this module to worker-side
  // state (parentPort/workerData/threadId), patches process (argv/env/exit), wires uncaught-error
  // fatality, and returns the message-inbox dispatcher.
  Object.defineProperty(globalThis, "__lumenInitWorkerThread", {
    configurable: true,
    enumerable: false,
    writable: false,
    value: function __lumenInitWorkerThread(wself, threadId, initBytes) {
      let init = {};
      try {
        if (initBytes) init = deser(initBytes) || {};
      } catch {
        init = {};
      }

      wt.isMainThread = false;
      wt.threadId = typeof threadId === "number" ? threadId : -1;
      wt.workerData = init.workerData;
      if (init.envData instanceof Map) {
        for (const [k, v] of init.envData) environmentData.set(k, v);
      }
      if (init.env && typeof init.env === "object") globalThis.__lumenResetWorkerEnvironment(init.env);
      delete globalThis.__lumenResetWorkerEnvironment;
      process.argv = [
        process.execPath,
        init.entry ?? "[worker]",
        ...(Array.isArray(init.argv) ? init.argv : []),
      ];
      // Tell the cluster glue this realm is a worker *thread*, never a cluster worker's main
      // realm — it must not adopt the process's cluster IPC channel (see cluster.js).
      Object.defineProperty(globalThis, "__lumenWorkerThreadRealm", {
        configurable: true, enumerable: false, writable: false, value: true,
      });
      // process.exit in a worker stops this thread's loop, never the whole process. Cooperative:
      // the current synchronous JS runs to its end first (documented in worker.rs).
      const exitWorker = (code) => {
        const n = code == null ? (process.exitCode ?? 0) : Number(code);
        wself.exit(Number.isFinite(n) ? Math.trunc(n) : 0);
      };
      Object.defineProperty(process, "exit", {
        value: exitWorker, enumerable: true, configurable: true, writable: true,
      });
      process.reallyExit = exitWorker;

      class ParentPort extends EventEmitter {
        #closed = false;
        constructor() {
          super();
          // Node's port-ref semantics: the port keeps the worker alive only while a 'message'
          // listener is attached.
          this.on("newListener", (ev) => {
            if (ev === "message" && !this.#closed) wself.setRef(true);
          });
          this.on("removeListener", (ev) => {
            if (ev === "message" && this.listenerCount("message") === 0) wself.setRef(false);
          });
        }
        postMessage(value, transferList) {
          if (this.#closed) return;
          wself.post(ser(value, transferList));
        }
        // Node's MessagePort is also an EventTarget; alias the listener API.
        addEventListener(type, fn) {
          this.on(type, fn);
        }
        removeEventListener(type, fn) {
          this.off(type, fn);
        }
        start() {}
        ref() {
          wself.setRef(true);
          return this;
        }
        unref() {
          wself.setRef(false);
          return this;
        }
        close() {
          this.#closed = true;
          wself.setRef(false);
        }
      }
      const parentPort = new ParentPort();
      wt.parentPort = parentPort;

      // Node kills a worker on an uncaught exception or unhandled rejection: the parent Worker
      // gets 'error', then 'exit' with code 1.
      const reportFatal = (err) => {
        let text;
        try {
          text =
            err instanceof Error
              ? `${err.name}: ${err.message}`
              : String(err).replace(/^Uncaught /, "");
        } catch {
          text = "Error: uncaught";
        }
        try {
          wself.report(text);
        } catch {}
        wself.exit(1);
      };
      globalThis.onerror = (message, _file, _line, _col, error) => {
        reportFatal(error !== undefined ? error : message);
        return true;
      };
      globalThis.onunhandledrejection = (event) => {
        event.preventDefault();
        reportFatal(event.reason);
      };

      return (bytes) => {
        if (bytes === false) return; // channel-closed sentinel (terminate)
        let data;
        try {
          data = deser(bytes);
        } catch (e) {
          parentPort.emit("messageerror", e);
          return;
        }
        parentPort.emit("message", data);
      };
    },
  });

  __builtins.set("worker_threads", wt);
}
