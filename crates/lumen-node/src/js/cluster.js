// node:cluster over child_process.fork. Workers are real child Lumen processes with JSON IPC.
// Socket/server handle transfer is intentionally unsupported: the IPC transport cannot pass OS
// descriptors, so applications that depend on shared listening handles get an explicit error.
{
  const EventEmitter = __builtins.get("events");
  const childProcess = __builtins.get("child_process");

  const SCHED_NONE = 1;
  const SCHED_RR = 2;
  // Decided once process.env is usable; the markers are then removed from the environment (as
  // Node deletes NODE_UNIQUE_ID) so processes this worker forks are not cluster workers too.
  let workerProcess;
  let workerId = 0;
  const isWorkerProcess = () => {
    if (workerProcess === undefined && process.env) {
      workerProcess = process.env.LUMEN_CLUSTER_WORKER === "1";
      if (workerProcess) {
        workerId = Number(process.env.NODE_UNIQUE_ID) || 0;
        process._lumenClusterWorker = true;
        delete process.env.LUMEN_CLUSTER_WORKER;
        delete process.env.NODE_UNIQUE_ID;
      }
    }
    return workerProcess === true;
  };
  // Worker -> primary notifications ride the IPC channel as marked messages, never surfaced as
  // user 'message' events.
  const INTERNAL = "__lumenCluster";

  // Worker-side wiring. This file is evaluated with the bootstrap glue, before process.env is
  // usable, so it runs on first use of the cluster API instead (every worker script asks
  // cluster.isPrimary / isWorker / worker before doing anything).
  let workerSideReady = false;
  const setupWorkerSide = () => {
    if (workerSideReady || !isWorkerProcess()) return;
    workerSideReady = true;
    // Listen to the channel so the worker notices its primary going away and exits. As in Node
    // (whose cluster child holds a 'disconnect' listener), the connected channel keeps a worker
    // alive until the primary disconnects or kills it.
    void process.send;
    process.once("disconnect", () => {});
    // Each worker's server listens on its own socket (no handle sharing), but the primary still
    // learns about it: 'listening' fires on the Worker and the cluster, as in Node.
    const net = __builtins.get("net");
    const listen = net && net.Server && net.Server.prototype.listen;
    const servers = new Set();
    // The primary's worker.disconnect(): like Node's worker._disconnect, close this worker's
    // servers and stay alive only while something else holds the loop.
    const closeWorkerSide = () => {
      process._clusterExitedAfterDisconnect = true;
      if (currentWorker) currentWorker.exitedAfterDisconnect = true;
      for (const server of servers) { try { server.close(); } catch {} }
      servers.clear();
    };
    process.on("internalMessage", message => {
      if (message[INTERNAL] === "disconnect") closeWorkerSide();
    });
    // In a worker, process.disconnect() is cluster.worker.disconnect(): servers close too.
    const disconnectChannel = process.disconnect;
    if (typeof disconnectChannel === "function") {
      process.disconnect = function disconnect() {
        closeWorkerSide();
        return disconnectChannel.apply(this, arguments);
      };
    }
    if (typeof listen === "function") {
      net.Server.prototype.listen = function (...args) {
        servers.add(this);
        this.once("close", () => servers.delete(this));
        this.once("listening", () => {
          const address = this.address();
          const info = address && typeof address === "object"
            ? { addressType: address.family === "IPv6" ? 6 : 4, address: address.address, port: address.port }
            : { addressType: -1, address, port: undefined };
          try { process.send({ [INTERNAL]: "listening", info }); } catch {}
        });
        return listen.apply(this, args);
      };
    }
  };
  const isWorkerSide = () => {
    setupWorkerSide();
    return isWorkerProcess();
  };

  class Worker extends EventEmitter {
    constructor(id, child, childSide = false) {
      super();
      this.id = id;
      this.process = child;
      this.exitedAfterDisconnect = undefined;
      if (childSide) return;
      child.on("message", (message, handle) => {
        if (message !== null && typeof message === "object" && message[INTERNAL] !== undefined) {
          if (message[INTERNAL] === "listening") {
            this.state = "listening";
            this.emit("listening", message.info);
            cluster.emit("listening", this, message.info);
          }
          return;
        }
        this.emit("message", message, handle);
        cluster.emit("message", this, message, handle);
      });
      child.on("disconnect", () => this.emit("disconnect"));
      child.on("error", error => this.emit("error", error));
      child.on("exit", (code, signal) => {
        delete cluster.workers[this.id];
        this.emit("exit", code, signal);
        cluster.emit("exit", this, code, signal);
      });
    }
    send(message, sendHandle, options, callback) {
      return this.process.send(message, sendHandle, options, callback);
    }
    kill(signal) {
      this.process.kill(signal);
      return this;
    }
    destroy(signal) { return this.kill(signal); }
    disconnect() {
      this.exitedAfterDisconnect = true;
      if (this.process.connected) {
        try { this.process.send({ [INTERNAL]: "disconnect" }); } catch {}
      }
      this.process.disconnect();
      return this;
    }
    isConnected() { return !!this.process.connected; }
    isDead() { return this.process.exitCode !== null || this.process.killed; }
  }

  const cluster = new EventEmitter();
  let nextWorkerId = 1;
  let currentWorker;

  Object.defineProperties(cluster, {
    isPrimary: { enumerable: true, get: () => !isWorkerSide() },
    isMaster: { enumerable: true, get: () => !isWorkerSide() },
    isWorker: { enumerable: true, get: isWorkerSide },
    worker: {
      enumerable: false,
      get() {
        if (!isWorkerSide()) return undefined;
        if (!currentWorker) {
          currentWorker = new Worker(workerId, process, true);
          process.on("message", (message, handle) => currentWorker.emit("message", message, handle));
          process.on("disconnect", () => currentWorker.emit("disconnect"));
        }
        return currentWorker;
      },
    },
  });
  cluster.workers = {};
  cluster.settings = {};
  cluster.SCHED_NONE = SCHED_NONE;
  cluster.SCHED_RR = SCHED_RR;
  cluster.schedulingPolicy = SCHED_RR;
  cluster.Worker = Worker;

  cluster.setupPrimary = function setupPrimary(settings = {}) {
    cluster.settings = Object.assign(
      {
        args: process.argv.slice(2),
        exec: process.argv[1],
        execArgv: process.execArgv || [],
        silent: false,
      },
      settings,
    );
    cluster.emit("setup", cluster.settings);
  };
  cluster.setupMaster = cluster.setupPrimary;

  cluster.fork = function fork(env = {}) {
    if (isWorkerProcess()) throw new Error("cluster.fork may only be called from the primary process");
    if (!cluster.settings.exec) cluster.setupPrimary();
    const id = nextWorkerId++;
    const child = childProcess.fork(cluster.settings.exec, cluster.settings.args, {
      execArgv: cluster.settings.execArgv,
      silent: cluster.settings.silent,
      env: {
        ...process.env,
        ...env,
        NODE_UNIQUE_ID: String(id),
        LUMEN_CLUSTER_WORKER: "1",
      },
    });
    const worker = new Worker(id, child);
    cluster.workers[id] = worker;
    queueMicrotask(() => cluster.emit("fork", worker));
    child.on("spawn", () => {
      worker.emit("online");
      cluster.emit("online", worker);
    });
    return worker;
  };

  cluster.disconnect = function disconnect(callback) {
    const workers = Object.values(cluster.workers);
    if (workers.length === 0) {
      if (typeof callback === "function") queueMicrotask(callback);
      return;
    }
    let remaining = workers.length;
    const done = () => {
      if (--remaining === 0 && typeof callback === "function") callback();
    };
    for (const worker of workers) {
      worker.once("disconnect", done);
      worker.disconnect();
    }
  };

  // Callable without `new`, as Node's constructors are (see __legacyConstructor).
  Worker = __legacyConstructor(Worker);
  cluster.Worker = Worker;

  __builtins.set("cluster", cluster);
}
