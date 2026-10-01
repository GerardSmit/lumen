// node:cluster — Node's lib/internal/cluster/{primary,child,worker,utils,round_robin_handle,
// shared_handle}.js. Workers are forked processes; listening sockets are shared over the
// child_process channel (descriptor passing): round-robin, where the primary accepts and hands
// each connection to a worker, or shared, where every worker accepts on the primary's socket.
{
  const EventEmitter = __builtins.get("events");
  const kNoFailure = 0;
  const TIMEOUT_MAX = 2 ** 31 - 1;
  const assert = (value, message) => {
    if (!value) __builtins.get("assert").fail(message || "Assertion failed");
  };

  // ---- internal/cluster/utils ----
  const callbacks = new Map();
  let seq = 0;

  function sendHelper(proc, message, handle, cb) {
    if (!proc.connected) return false;
    // Mark message as internal. See INTERNAL_PREFIX in lib/internal/child_process.js
    message = { cmd: "NODE_CLUSTER", ...message, seq };
    if (typeof cb === "function") callbacks.set(seq, cb);
    seq += 1;
    return proc.send(message, handle);
  }

  // Returns an internalMessage listener that hands off normal messages to the callback but
  // intercepts and redirects ACK messages.
  function internal(worker, cb) {
    return function onInternalMessage(message, handle) {
      if (message.cmd !== "NODE_CLUSTER") return;
      let fn = cb;
      if (message.ack !== undefined) {
        const callback = callbacks.get(message.ack);
        if (callback !== undefined) {
          fn = callback;
          callbacks.delete(message.ack);
        }
      }
      fn.apply(worker, arguments);
    };
  }

  // ---- internal/cluster/worker ----
  function Worker(options) {
    if (!(this instanceof Worker)) return new Worker(options);
    EventEmitter.call(this);
    if (options === null || typeof options !== "object") options = {};
    this.exitedAfterDisconnect = undefined;
    this.state = options.state || "none";
    this.id = options.id | 0;
    if (options.process) {
      this.process = options.process;
      this.process.on("error", (code, signal) => this.emit("error", code, signal));
      this.process.on("message", (message, handle) => this.emit("message", message, handle));
    }
  }
  Object.setPrototypeOf(Worker.prototype, EventEmitter.prototype);
  Object.setPrototypeOf(Worker, EventEmitter);
  Worker.prototype.kill = function kill() {
    this.destroy.apply(this, arguments);
  };
  Worker.prototype.send = function send() {
    return this.process.send.apply(this.process, arguments);
  };
  Worker.prototype.isDead = function isDead() {
    return this.process.exitCode != null || this.process.signalCode != null;
  };
  Worker.prototype.isConnected = function isConnected() {
    return this.process.connected;
  };

  const ownerSymbol = () => __internals.get("netBinding")("symbols").owner_symbol;

  // ---- internal/cluster/round_robin_handle ----
  function RoundRobinHandle(key, address, { port, fd, flags, backlog, readableAll, writableAll }) {
    const net = __builtins.get("net");
    this.key = key;
    this.all = new Map();
    this.free = new Map();
    this.handles = [];
    this.handle = null;
    this.server = net.createServer(() => assert(false, "unexpected connection on a round-robin server"));
    if (fd >= 0) {
      this.server.listen({ fd, backlog });
    } else if (port >= 0) {
      this.server.listen({
        port,
        host: address,
        // Currently, net module only supports `ipv6Only` option in `flags`.
        ipv6Only: Boolean(flags & 1),
        backlog,
      });
    } else {
      this.server.listen({ path: address, backlog, readableAll, writableAll }); // UNIX socket path.
    }
    this.server.once("listening", () => {
      this.handle = this.server._handle;
      this.handle.onconnection = (err, handle) => this.distribute(err, handle);
      this.server._handle = null;
      this.server = null;
    });
  }
  RoundRobinHandle.prototype.add = function add(worker, send) {
    assert(this.all.has(worker.id) === false);
    this.all.set(worker.id, worker);
    const done = () => {
      if (this.handle.getsockname) {
        const out = {};
        this.handle.getsockname(out);
        send(null, { sockname: out }, null);
      } else {
        send(null, null, null); // UNIX socket.
      }
      this.handoff(worker); // In case there are connections pending.
    };
    if (this.server === null) return done();
    // Still busy binding.
    this.server.once("listening", done);
    this.server.once("error", (err) => {
      send(err.errno, null);
    });
  };
  RoundRobinHandle.prototype.remove = function remove(worker) {
    const existed = this.all.delete(worker.id);
    if (!existed) return false;
    this.free.delete(worker.id);
    if (this.all.size !== 0) return false;
    for (const handle of this.handles.splice(0)) handle.close();
    if (this.handle !== null) this.handle.close();
    else if (this.server !== null) this.server.close();
    this.handle = null;
    return true;
  };
  RoundRobinHandle.prototype.distribute = function distribute(err, handle) {
    // If `accept` fails just skip it (handle is undefined)
    if (err) return;
    this.handles.push(handle);
    const [workerEntry] = this.free;
    if (Array.isArray(workerEntry)) {
      const { 0: workerId, 1: worker } = workerEntry;
      this.free.delete(workerId);
      this.handoff(worker);
    }
  };
  RoundRobinHandle.prototype.handoff = function handoff(worker) {
    if (!this.all.has(worker.id)) return; // Worker is closing (or has closed) the server.
    const handle = this.handles.shift();
    if (handle === undefined) {
      this.free.set(worker.id, worker); // Add to ready queue again.
      return;
    }
    const message = { act: "newconn", key: this.key };
    sendHelper(worker.process, message, handle, (reply) => {
      if (reply.accepted) handle.close();
      else this.distribute(0, handle); // Worker is shutting down. Send to another.
      this.handoff(worker);
    });
  };

  // ---- internal/cluster/shared_handle ----
  function SharedHandle(key, address, { port, addressType, fd, flags }) {
    this.key = key;
    this.workers = new Map();
    this.handle = null;
    this.errno = 0;
    let rval;
    if (addressType === "udp4" || addressType === "udp6") {
      rval = __builtins.get("dgram")._createSocketHandle(address, port, addressType, fd, flags);
    } else {
      rval = __builtins.get("net")._createServerHandle(address, port, addressType, fd, flags);
    }
    if (typeof rval === "number") this.errno = rval;
    else this.handle = rval;
  }
  SharedHandle.prototype.add = function add(worker, send) {
    assert(!this.workers.has(worker.id));
    this.workers.set(worker.id, worker);
    send(this.errno, null, this.handle);
  };
  SharedHandle.prototype.remove = function remove(worker) {
    if (!this.workers.has(worker.id)) return false;
    this.workers.delete(worker.id);
    if (this.workers.size !== 0) return false;
    if (this.handle) this.handle.close();
    this.handle = null;
    return true;
  };

  // ---- internal/cluster/primary ----
  function createPrimary() {
    const cluster = new EventEmitter();
    const intercom = new EventEmitter();
    const SCHED_NONE = 1;
    const SCHED_RR = 2;
    const handles = new Map();
    cluster.isWorker = false;
    cluster.isMaster = true; // Deprecated alias. Must be same as isPrimary.
    cluster.isPrimary = true;
    cluster.Worker = Worker;
    cluster.workers = {};
    cluster.settings = {};
    cluster.SCHED_NONE = SCHED_NONE; // Leave it to the operating system.
    cluster.SCHED_RR = SCHED_RR; // Primary distributes connections.

    let ids = 0;
    let initialized = false;

    let schedulingPolicy = process.env.NODE_CLUSTER_SCHED_POLICY;
    if (schedulingPolicy === "rr") schedulingPolicy = SCHED_RR;
    else if (schedulingPolicy === "none") schedulingPolicy = SCHED_NONE;
    // Round-robin doesn't perform well on Windows due to the way IOCP is wired up.
    else if (process.platform === "win32") schedulingPolicy = SCHED_NONE;
    else schedulingPolicy = SCHED_RR;
    cluster.schedulingPolicy = schedulingPolicy;

    cluster.setupPrimary = function setupPrimary(options) {
      const settings = {
        args: process.argv.slice(2),
        exec: process.argv[1],
        execArgv: process.execArgv,
        silent: false,
        ...cluster.settings,
        ...options,
      };
      cluster.settings = settings;
      if (initialized === true) return process.nextTick(setupSettingsNT, settings);
      initialized = true;
      schedulingPolicy = cluster.schedulingPolicy; // Freeze policy.
      assert(schedulingPolicy === SCHED_NONE || schedulingPolicy === SCHED_RR,
        `Bad cluster.schedulingPolicy: ${schedulingPolicy}`);
      process.nextTick(setupSettingsNT, settings);
    };
    // Deprecated alias must be same as setupPrimary
    cluster.setupMaster = cluster.setupPrimary;

    function setupSettingsNT(settings) {
      cluster.emit("setup", settings);
    }

    function createWorkerProcess(id, env) {
      const workerEnv = { ...process.env, ...env, NODE_UNIQUE_ID: `${id}` };
      const execArgv = [...cluster.settings.execArgv];
      if (cluster.settings.inspectPort === null) {
        throw new __errors.ERR_SOCKET_BAD_PORT("Port", null, true);
      }
      return __builtins.get("child_process").fork(cluster.settings.exec, cluster.settings.args, {
        cwd: cluster.settings.cwd,
        env: workerEnv,
        serialization: cluster.settings.serialization,
        silent: cluster.settings.silent,
        windowsHide: cluster.settings.windowsHide,
        execArgv,
        stdio: cluster.settings.stdio,
        gid: cluster.settings.gid,
        uid: cluster.settings.uid,
      });
    }

    function removeWorker(worker) {
      assert(worker);
      delete cluster.workers[worker.id];
      if (Object.keys(cluster.workers).length === 0) {
        assert(handles.size === 0, "Resource leak detected.");
        intercom.emit("disconnect");
      }
    }

    function removeHandlesForWorker(worker) {
      assert(worker);
      for (const { 0: key, 1: handle } of handles) {
        if (handle.remove(worker)) handles.delete(key);
      }
    }

    cluster.fork = function fork(env) {
      cluster.setupPrimary();
      const id = ++ids;
      const workerProcess = createWorkerProcess(id, env);
      const worker = new Worker({ id, process: workerProcess });

      worker.on("message", function(message, handle) {
        cluster.emit("message", this, message, handle);
      });

      worker.process.once("exit", (exitCode, signalCode) => {
        // Remove the worker from the workers list only if it has disconnected, otherwise we
        // might still want to access it.
        if (!worker.isConnected()) {
          removeHandlesForWorker(worker);
          removeWorker(worker);
        }
        worker.exitedAfterDisconnect = !!worker.exitedAfterDisconnect;
        worker.state = "dead";
        worker.emit("exit", exitCode, signalCode);
        cluster.emit("exit", worker, exitCode, signalCode);
      });

      worker.process.once("disconnect", () => {
        // Now is a good time to remove the handles associated with this worker because it is
        // not connected to the primary anymore.
        removeHandlesForWorker(worker);
        // Remove the worker from the workers list only if its process has exited. Otherwise,
        // we might still want to access it.
        if (worker.isDead()) removeWorker(worker);
        worker.exitedAfterDisconnect = !!worker.exitedAfterDisconnect;
        worker.state = "disconnected";
        worker.emit("disconnect");
        cluster.emit("disconnect", worker);
      });

      worker.process.on("internalMessage", internal(worker, onmessage));
      process.nextTick(emitForkNT, worker);
      cluster.workers[worker.id] = worker;
      return worker;
    };

    function emitForkNT(worker) {
      cluster.emit("fork", worker);
    }

    cluster.disconnect = function disconnect(cb) {
      const workers = Object.keys(cluster.workers);
      if (workers.length === 0) {
        process.nextTick(() => intercom.emit("disconnect"));
      } else {
        for (const worker of Object.values(cluster.workers)) {
          if (worker.isConnected()) worker.disconnect();
        }
      }
      if (typeof cb === "function") intercom.once("disconnect", cb);
    };

    const methodMessageMapping = { close, exitedAfterDisconnect, listening, online, queryServer };

    function onmessage(message, handle) {
      const worker = this;
      const fn = methodMessageMapping[message.act];
      if (typeof fn === "function") fn(worker, message);
    }

    function online(worker) {
      worker.state = "online";
      worker.emit("online");
      cluster.emit("online", worker);
    }

    function exitedAfterDisconnect(worker, message) {
      worker.exitedAfterDisconnect = true;
      send(worker, { ack: message.seq });
    }

    function queryServer(worker, message) {
      // Stop processing if worker already disconnecting
      if (worker.exitedAfterDisconnect) return;
      const key = `${message.address}:${message.port}:${message.addressType}:${message.fd}:${message.index}`;
      let handle = handles.get(key);
      if (handle === undefined) {
        let address = message.address;
        // Find shortest path for unix sockets because of the ~100 byte limit
        if (message.port < 0 && typeof address === "string" && process.platform !== "win32") {
          address = __builtins.get("path").relative(process.cwd(), address);
          if (message.address.length < address.length) address = message.address;
        }
        // UDP is exempt from round-robin connection balancing for what should be obvious
        // reasons: it's connectionless. There is nothing to send to the workers except raw
        // datagrams and that's pointless.
        if (schedulingPolicy !== SCHED_RR || message.addressType === "udp4" || message.addressType === "udp6") {
          handle = new SharedHandle(key, address, message);
        } else {
          handle = new RoundRobinHandle(key, address, message);
        }
        handles.set(key, handle);
      }
      if (!handle.data) handle.data = message.data;
      // Set custom server data
      handle.add(worker, (errno, reply, handle) => {
        const { data } = handles.get(key);
        if (errno) handles.delete(key); // Gives other workers a chance to retry.
        send(worker, { errno, key, ack: message.seq, data, ...reply }, handle);
      });
    }

    function listening(worker, message) {
      const info = {
        addressType: message.addressType,
        address: message.address,
        port: message.port,
        fd: message.fd,
      };
      worker.state = "listening";
      worker.emit("listening", info);
      cluster.emit("listening", worker, info);
    }

    // Server in worker is closing, remove from list. The handle may have been removed by a
    // prior call to removeHandlesForWorker() so guard against that.
    function close(worker, message) {
      const key = message.key;
      const handle = handles.get(key);
      if (handle && handle.remove(worker)) handles.delete(key);
    }

    function send(worker, message, handle, cb) {
      return sendHelper(worker.process, message, handle, cb);
    }

    // Extend generic Worker with methods specific to the primary process.
    Worker.prototype.disconnect = function disconnect() {
      this.exitedAfterDisconnect = true;
      send(this, { act: "disconnect" });
      removeHandlesForWorker(this);
      removeWorker(this);
      return this;
    };

    Worker.prototype.destroy = function destroy(signo) {
      const proc = this.process;
      const signal = signo || "SIGTERM";
      if (this.isConnected()) {
        this.once("disconnect", () => proc.kill(signal));
        this.disconnect();
        return;
      }
      proc.kill(signal);
    };

    return cluster;
  }

  // ---- internal/cluster/child ----
  function createChild() {
    const path = __builtins.get("path");
    const cluster = new EventEmitter();
    const handles = new Map();
    const indexes = new Map();
    const noop = () => {};

    cluster.isWorker = true;
    cluster.isMaster = false; // Deprecated alias. Must be same as isPrimary.
    cluster.isPrimary = false;
    cluster.worker = null;
    cluster.Worker = Worker;

    cluster._setupWorker = function _setupWorker() {
      const worker = new Worker({
        id: +process.env.NODE_UNIQUE_ID | 0,
        process,
        state: "online",
      });
      cluster.worker = worker;

      process.once("disconnect", () => {
        worker.emit("disconnect");
        if (!worker.exitedAfterDisconnect) {
          // Unexpected disconnect, primary exited, or some such nastiness, so worker exits
          // immediately.
          process.exit(kNoFailure);
        }
      });

      process.on("internalMessage", internal(worker, onmessage));
      send({ act: "online" });

      function onmessage(message, handle) {
        if (message.act === "newconn") onconnection(message, handle);
        else if (message.act === "disconnect") _disconnect.call(worker, true);
      }
    };

    // `obj` is a net#Server or a dgram#Socket object.
    cluster._getServer = function _getServer(obj, options, cb) {
      let address = options.address;
      // Resolve unix socket paths to absolute paths
      if (options.port < 0 && typeof address === "string" && process.platform !== "win32") {
        address = path.resolve(address);
      }
      const indexesKey = [address, options.port, options.addressType, options.fd].join(":");
      let indexSet = indexes.get(indexesKey);
      if (indexSet === undefined) {
        indexSet = { nextIndex: 0, set: new Set() };
        indexes.set(indexesKey, indexSet);
      }
      const index = indexSet.nextIndex++;
      indexSet.set.add(index);

      const message = { act: "queryServer", index, data: null, ...options };
      message.address = address;
      // Set custom data on handle (i.e. tls tickets key)
      if (obj._getServerData) message.data = obj._getServerData();

      send(message, (reply, handle) => {
        if (typeof obj._setServerData === "function") obj._setServerData(reply.data);
        if (handle) {
          // Shared listen socket
          shared(reply, { handle, indexesKey, index }, cb);
        } else {
          // Round-robin.
          rr(reply, { indexesKey, index }, cb);
        }
      });

      obj.once("listening", () => {
        // short-lived sockets might have been closed
        if (!indexes.has(indexesKey)) return;
        cluster.worker.state = "listening";
        const address = obj.address();
        message.act = "listening";
        message.port = (address && address.port) || options.port;
        send(message);
      });
    };

    function removeIndexesKey(indexesKey, index) {
      const indexSet = indexes.get(indexesKey);
      if (!indexSet) return;
      indexSet.set.delete(index);
      if (indexSet.set.size === 0) indexes.delete(indexesKey);
    }

    // Shared listen socket.
    function shared(message, { handle, indexesKey, index }, cb) {
      const key = message.key;
      // Monkey-patch the close() method so we can keep track of when it's closed. Avoids
      // resource leaks when the handle is short-lived.
      const close = handle.close;
      handle.close = function() {
        send({ act: "close", key });
        handles.delete(key);
        removeIndexesKey(indexesKey, index);
        return close.apply(handle, arguments);
      };
      assert(handles.has(key) === false);
      handles.set(key, handle);
      cb(message.errno, handle);
    }

    // Round-robin. Primary distributes handles across workers.
    function rr(message, { indexesKey, index }, cb) {
      if (message.errno) return cb(message.errno, null);
      let key = message.key;
      let fakeHandle = null;

      function ref() {
        if (!fakeHandle) fakeHandle = setInterval(noop, TIMEOUT_MAX);
      }
      function unref() {
        if (fakeHandle) {
          clearInterval(fakeHandle);
          fakeHandle = null;
        }
      }
      function listen(backlog) {
        return 0;
      }
      function close() {
        // lib/net.js treats server._handle.close() as effectively synchronous. That means there
        // is a time window between the call to close() and the ack by the primary process in
        // which we can still receive handles. onconnection() below handles that by sending
        // those handles back to the primary.
        if (key === undefined) return;
        unref();
        // If the handle is the last handle in process, the parent process will delete the
        // handle when worker process exits. So it is ok if the close message get lost.
        send({ act: "close", key });
        handles.delete(key);
        removeIndexesKey(indexesKey, index);
        key = undefined;
      }
      function getsockname(out) {
        if (key) Object.assign(out, message.sockname);
        return 0;
      }

      // Faux handle. net.Server is not associated with handle, so we control its state (ref or
      // unref) by setInterval.
      const handle = { close, listen, ref, unref };
      handle.ref();
      if (message.sockname) handle.getsockname = getsockname; // TCP handles only.
      assert(handles.has(key) === false);
      handles.set(key, handle);
      cb(0, handle);
    }

    // Round-robin connection.
    function onconnection(message, handle) {
      const key = message.key;
      const server = handles.get(key);
      let accepted = server !== undefined;
      if (accepted && server[ownerSymbol()]) {
        const self = server[ownerSymbol()];
        if (self.maxConnections != null && self._connections >= self.maxConnections) accepted = false;
      }
      send({ ack: message.seq, accepted });
      if (accepted) server.onconnection(0, handle);
      else handle.close();
    }

    function send(message, cb) {
      return sendHelper(process, message, null, cb);
    }

    function _disconnect(primaryInitiated) {
      this.exitedAfterDisconnect = true;
      let waitingCount = 1;
      function checkWaitingCount() {
        waitingCount--;
        if (waitingCount === 0) {
          // If disconnect is worker initiated, wait for ack to be sure exitedAfterDisconnect is
          // properly set in the primary, otherwise, if it's primary initiated there's no need
          // to send the exitedAfterDisconnect message
          if (primaryInitiated) process.disconnect();
          else send({ act: "exitedAfterDisconnect" }, () => process.disconnect());
        }
      }
      handles.forEach((handle) => {
        waitingCount++;
        if (handle[ownerSymbol()]) handle[ownerSymbol()].close(checkWaitingCount);
        else handle.close(checkWaitingCount);
      });
      handles.clear();
      checkWaitingCount();
    }

    // Extend generic Worker with methods specific to worker processes.
    Worker.prototype.disconnect = function disconnect() {
      if (this.state !== "disconnecting" && this.state !== "destroying") {
        this.state = "disconnecting";
        _disconnect.call(this);
      }
      return this;
    };

    Worker.prototype.destroy = function destroy() {
      if (this.state === "destroying") return;
      this.exitedAfterDisconnect = true;
      if (!this.isConnected()) {
        process.exit(kNoFailure);
      } else {
        this.state = "destroying";
        send({ act: "exitedAfterDisconnect" }, () => process.disconnect());
        process.once("disconnect", () => process.exit(kNoFailure));
      }
    };

    return cluster;
  }

  const cluster = process.env && "NODE_UNIQUE_ID" in process.env ? createChild() : createPrimary();
  __builtins.set("cluster", cluster);
  // require('internal/cluster/...') under --expose-internals.
  __builtins.set("internal/cluster/round_robin_handle", RoundRobinHandle);
  __builtins.set("internal/cluster/shared_handle", SharedHandle);
  __builtins.set("internal/cluster/worker", Worker);
  __builtins.set("internal/cluster/utils", { sendHelper, internal });
}
