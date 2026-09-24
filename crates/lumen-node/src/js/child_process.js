// node:child_process over the __child native ops (real std::process subprocesses). spawn returns a
// ChildProcess (EventEmitter) whose stdout/stderr are Readable node streams pumped by one-shot
// reads and whose stdin is a Writable; exec*/spawnSync build on that or the synchronous execSync
// op. `kill()` sends SIGKILL (std can't send arbitrary signals). `fork` re-execs the current Lumen
// binary and carries JSON-framed IPC over its piped stdin/stdout.

const EventEmitter = __builtins.get("events");
const IPC_PREFIX = "\x1eLUMEN_IPC ";
// The child's `process.disconnect()` closes its end of the channel with this control line.
const IPC_DISCONNECT = "\x1eLUMEN_IPC_DISCONNECT";

// Normalize the stdio option to a list of "pipe" | "inherit" | "ignore" — three entries for the
// standard fds, plus one per extra slot (`stdio[3]`...), which only "pipe" and "ignore" support.
function normalizeStdio(stdio) {
  if (stdio === "inherit") return ["inherit", "inherit", "inherit"];
  if (stdio === "ignore") return ["ignore", "ignore", "ignore"];
  if (Array.isArray(stdio)) {
    const out = [];
    for (let i = 0; i < Math.max(3, stdio.length); i++) {
      const v = stdio[i];
      out.push(i >= 3 ? (v === "pipe" ? "pipe" : "ignore") : v === "inherit" || v === "ignore" || isOwnStdio(v, i) ? (v === "ignore" ? v : "inherit") : "pipe");
    }
    return out;
  }
  return ["pipe", "pipe", "pipe"];
}

// The parent's own fd `i` (the number or process.stdin/stdout/stderr) in stdio slot `i` is inherited.
function isOwnStdio(v, i) {
  if (v === i) return true;
  const own = [process.stdin, process.stdout, process.stderr][i];
  return v !== null && typeof v === "object" && v === own;
}

// Any other stream or fd in a standard slot: Node hands its handle to the child; lumen spawns a
// pipe and relays it in JS, so the data still reaches the target (instead of a dead pipe).
function stdioRelayTargets(stdio) {
  if (!Array.isArray(stdio)) return null;
  let targets = null;
  for (let i = 0; i < 3; i++) {
    const v = stdio[i];
    if (v === null || v === undefined || typeof v === "string" || isOwnStdio(v, i)) continue;
    if (typeof v !== "number" && typeof v !== "object") continue;
    (targets ??= [])[i] = v;
  }
  return targets;
}

function relayStdio(child, targets) {
  const fs = __builtins.get("fs");
  const done = [];
  const names = ["stdin", "stdout", "stderr"];
  for (let i = 0; i < 3; i++) {
    const target = targets[i];
    const pipe = child.stdio[i];
    if (target === undefined || !pipe) continue;
    child.stdio[i] = null;
    child[names[i]] = null;
    if (i === 0) {
      if (typeof target === "number") fs.createReadStream(null, { fd: target, autoClose: false }).pipe(pipe);
      else if (typeof target.pipe === "function" && target.readable !== false) target.pipe(pipe);
      else pipe.end();
      continue;
    }
    if (typeof target === "number") pipe.on("data", chunk => fs.writeSync(target, chunk));
    else if (typeof target.write === "function") pipe.pipe(target, { end: false });
    else pipe.resume();
    // 'close' waits for the relayed output, as Node's waits for the child's stdio to close.
    done.push(new Promise(resolve => { pipe.once("end", resolve); pipe.once("close", resolve); pipe.once("error", resolve); }));
  }
  if (done.length) child._stdioRelays = (child._stdioRelays || []).concat(done);
}

const SIGNALS = { SIGHUP: 1, SIGINT: 2, SIGQUIT: 3, SIGABRT: 6, SIGKILL: 9, SIGUSR1: 10, SIGUSR2: 12, SIGPIPE: 13, SIGALRM: 14, SIGTERM: 15 };
const SIGNAL_NAMES = Object.fromEntries(Object.entries(SIGNALS).map(([name, n]) => [n, name]));
function signalNumber(signal) {
  if (signal === undefined || signal === null) return 0;
  if (typeof signal === "number") return signal;
  const n = SIGNALS[String(signal)];
  if (n === undefined) {
    const err = new TypeError(`Unknown signal: ${signal}`);
    err.code = "ERR_UNKNOWN_SIGNAL";
    throw err;
  }
  return n;
}

// libuv errno values (the table util.getSystemErrorName reads) for the ways a spawn can fail.
const SPAWN_ERRNO = Object.fromEntries(["ENOENT", "EACCES", "ENOTDIR", "EINVAL"].map((c) => [c, __uvCodes.get(c)]));

// A native spawn failure as Node reports it: `spawn foo ENOENT` with errno, syscall, path and
// spawnargs (the arguments after the file). Anything else is rethrown unchanged.
function spawnError(err, syscall, file, args) {
  if (!err || !(err.code in SPAWN_ERRNO)) return null;
  const e = new Error(`${syscall} ${file} ${err.code}`);
  e.errno = SPAWN_ERRNO[err.code];
  e.code = err.code;
  e.syscall = `${syscall} ${file}`;
  e.path = file;
  e.spawnargs = args;
  return e;
}

// Node's `shell` option: `/bin/sh -c <line>` on Unix; on Windows `cmd.exe /d /s /c "<line>"`,
// passed verbatim because cmd.exe parses its own command line, or `<shell> -c <line>` for any
// other shell (bash, pwsh).
function shellCommand(command, args, shell) {
  const line = [command, ...args].join(" ");
  if (process.platform === "win32") {
    const file = typeof shell === "string" ? shell : process.env.comspec || "cmd.exe";
    if (/^(?:.*[\\/])?cmd(?:\.exe)?$/i.test(file)) {
      return { file, args: ["/d", "/s", "/c", `"${line}"`], verbatim: true };
    }
    return { file, args: ["-c", line], verbatim: false };
  }
  return { file: typeof shell === "string" ? shell : "/bin/sh", args: ["-c", line], verbatim: false };
}

// The program, arguments and verbatim flag a spawn call resolves to.
function resolveCommand(command, args, options) {
  const file = String(command);
  const argv = (args || []).map(String);
  if (options.shell) return shellCommand(file, argv, options.shell);
  return { file, args: argv, verbatim: !!options.windowsVerbatimArguments };
}

// env object -> array of [key, value] pairs (or undefined to inherit).
function envPairs(env) {
  if (!env || typeof env !== "object") return undefined;
  return Object.keys(env).map((k) => [k, String(env[k])]);
}

function makeReadable(childId, which, onIpcMessage) {
  const { Readable } = __builtins.get("stream");
  const stream = new Readable({ read() {} });
  let pending = "";
  (async () => {
    for (;;) {
      const chunk = await new Promise((resolve, reject) => __child.read(childId, which, resolve, reject));
      if (chunk === null) {
        if (pending) stream.push(Buffer.from(pending));
        stream.push(null);
        // The child's stdout closing is its channel closing (it exited or closed it).
        if (onIpcMessage) onIpcMessage.close();
        return;
      }
      if (!onIpcMessage) {
        stream.push(Buffer.from(chunk));
        continue;
      }
      pending += Buffer.from(chunk).toString("utf8");
      for (;;) {
        const newline = pending.indexOf("\n");
        if (newline < 0) break;
        const line = pending.slice(0, newline);
        pending = pending.slice(newline + 1);
        if (line === IPC_DISCONNECT) {
          onIpcMessage.close();
        } else if (line.startsWith(IPC_PREFIX)) {
          let message;
          try {
            message = JSON.parse(line.slice(IPC_PREFIX.length));
          } catch (error) {
            stream.destroy(error);
            return;
          }
          // A throwing 'message' listener is the program's uncaught exception, not a channel
          // failure: swallowing it here left the parent running with the error lost.
          try {
            onIpcMessage(message);
          } catch (error) {
            process.nextTick(() => { throw error; });
          }
        } else {
          stream.push(Buffer.from(line + "\n"));
        }
      }
    }
  })().catch((e) => stream.destroy(e));
  return stream;
}

function makeWritable(childId) {
  const { Writable } = __builtins.get("stream");
  return new Writable({
    write(chunk, enc, cb) {
      const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk, typeof enc === "string" ? enc : "utf8");
      __child.write(childId, bytes, () => cb(), (e) => cb(e));
    },
    final(cb) {
      __child.closeStdin(childId);
      cb();
    },
  });
}

// An extra stdio slot is duplex (a socketpair, as in Node): reads pump like stdout, writes go
// to the same fd.
function makeExtraPipe(childId, fd) {
  const { Duplex } = __builtins.get("stream");
  const readable = makeReadable(childId, fd);
  const pipe = new Duplex({
    read() {},
    write(chunk, enc, cb) {
      const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk, typeof enc === "string" ? enc : "utf8");
      __child.writeFd(childId, fd, bytes, () => cb(), (e) => cb(e));
    },
    final(cb) {
      __child.closeFd(childId, fd);
      cb();
    },
  });
  readable.on("data", (chunk) => pipe.push(chunk));
  // Like a non-allowHalfOpen net.Socket: the child closing its end ends ours, so 'close' follows.
  readable.on("end", () => { pipe.push(null); if (!pipe.writableEnded) pipe.end(); });
  readable.on("error", (e) => pipe.destroy(e));
  return pipe;
}

class ChildProcess extends EventEmitter {
  constructor(childId, pid, stdio, onIpcMessage, error) {
    super();
    if (error) {
      this._failed(stdio, error);
      return;
    }
    this._id = childId;
    this.pid = pid;
    this.killed = false;
    this.exitCode = null;
    this.signalCode = null;
    this.stdin = stdio[0] === "pipe" ? makeWritable(childId) : null;
    this.stdout = stdio[1] === "pipe" ? makeReadable(childId, 1, onIpcMessage) : null;
    this.stderr = stdio[2] === "pipe" ? makeReadable(childId, 2) : null;
    this.stdio = [this.stdin, this.stdout, this.stderr];
    for (let fd = 3; fd < stdio.length; fd++) this.stdio.push(stdio[fd] === "pipe" ? makeExtraPipe(childId, fd) : null);
    this.connected = typeof onIpcMessage === "function";
    queueMicrotask(() => this.emit("spawn"));
    new Promise((resolve, reject) => __child.wait(childId, resolve, reject)).then(
      ([code, signalNumber]) => {
        let signal = signalNumber === null ? null : SIGNAL_NAMES[signalNumber] ?? `SIG${signalNumber}`;
        // Windows has no signals: libuv reports a process it killed by the signal it was sent.
        if (signal === null && this._killSignal !== undefined && process.platform === "win32") {
          code = null;
          signal = this._killSignal;
        }
        this.exitCode = code;
        this.signalCode = signal;
        this._ipcClosed();
        this.emit("exit", code, signal);
        // 'close' should follow once stdio has flushed; a microtask is close enough here.
        const emitClose = () => this.emit("close", code, signal);
        if (this._stdioRelays) Promise.all(this._stdioRelays).then(emitClose);
        else queueMicrotask(emitClose);
      },
      (err) => this.emit("error", err),
    );
  }
  // A spawn that failed (missing program, no permission): like Node, the object still has its
  // stdio streams but no pid, then emits 'error' and 'close' with the negative errno, never 'spawn'
  // or 'exit'.
  _failed(stdio, error) {
    const { Readable, Writable } = __builtins.get("stream");
    const ended = () => {
      const stream = new Readable({ read() {} });
      stream.push(null);
      return stream;
    };
    this._id = null;
    this.pid = undefined;
    this.killed = false;
    this.exitCode = null;
    this.signalCode = null;
    this.stdin = stdio[0] === "pipe" ? new Writable({ write(chunk, enc, cb) { cb(); } }) : null;
    this.stdout = stdio[1] === "pipe" ? ended() : null;
    this.stderr = stdio[2] === "pipe" ? ended() : null;
    this.stdio = [this.stdin, this.stdout, this.stderr];
    this.connected = false;
    process.nextTick(() => {
      this.exitCode = error.errno;
      this.emit("error", error);
      this.emit("close", error.errno, null);
    });
  }
  kill(signal) {
    if (this._id === null) return false;
    const ok = __child.kill(this._id, signalNumber(signal));
    if (ok) {
      this.killed = true;
      const number = signalNumber(signal === undefined || signal === null ? "SIGTERM" : signal);
      if (number !== 0 && this.exitCode === null && this.signalCode === null) this._killSignal = SIGNAL_NAMES[number] ?? `SIG${number}`;
    }
    return ok;
  }
  ref() {
    __child.ref(this._id);
  }
  unref() {
    // Detach from the event loop's keep-alive count (Node semantics), so a long-lived service
    // child (esbuild) doesn't block process exit once the main work is done. esbuild toggles
    // ref/unref per request, so both must be real.
    __child.unref(this._id);
  }
  send(message, sendHandle, options, callback) {
    if (typeof sendHandle === "function") callback = sendHandle;
    else if (typeof options === "function") callback = options;
    if (sendHandle != null && typeof sendHandle !== "function") {
      const error = new Error("child_process.fork handle transfer is not supported in lumen");
      if (callback) queueMicrotask(() => callback(error));
      else throw error;
      return false;
    }
    if (!this.connected || !this.stdin) {
      const error = new Error("IPC channel is closed");
      error.code = "ERR_IPC_CHANNEL_CLOSED";
      if (callback) queueMicrotask(() => callback(error));
      else this.emit("error", error);
      return false;
    }
    let frame;
    try {
      frame = IPC_PREFIX + JSON.stringify(message === undefined ? null : message) + "\n";
    } catch (error) {
      if (callback) queueMicrotask(() => callback(error));
      else throw error;
      return false;
    }
    this.stdin.write(frame, callback);
    return true;
  }
  disconnect() {
    if (!this.connected) return;
    this.connected = false;
    if (this.stdin) this.stdin.end();
    queueMicrotask(() => this.emit("disconnect"));
  }
  // The child closed the channel (process.disconnect(), or its stdout ended / it exited).
  _ipcClosed() {
    if (!this.connected) return;
    this.connected = false;
    if (this.stdin && !this.stdin.writableEnded) this.stdin.end();
    this.emit("disconnect");
  }
}

function spawn(command, args, options) {
  if (!Array.isArray(args)) {
    options = args;
    args = [];
  }
  options = options || {};
  const { file, args: argv, verbatim } = resolveCommand(command, args, options);
  const stdio = normalizeStdio(options.stdio);
  let info;
  try {
    // Node hands a child `process.env` when no env is given: the live object, not the OS table
    // (they differ once the program edits it, and always for a realm embedded in a host).
    info = __child.spawn(file, argv, options.cwd, envPairs(options.env || process.env), stdio, verbatim);
  } catch (e) {
    const error = spawnError(e, "spawn", file, argv);
    if (!error) throw e;
    return new ChildProcess(null, undefined, stdio, undefined, error);
  }
  let child;
  const onIpcMessage = options._ipc
    ? (message) => child.emit("message", message, null)
    : undefined;
  if (onIpcMessage) onIpcMessage.close = () => child._ipcClosed();
  child = new ChildProcess(info.childId, info.pid, stdio, onIpcMessage);
  const relayTargets = stdioRelayTargets(options.stdio);
  if (relayTargets) relayStdio(child, relayTargets);
  if (options.signal !== undefined && options.signal !== null) abortChildOnSignal(child, options.signal, options.killSignal);
  return child;
}

// `options.signal`: aborting kills the child (with `killSignal`) and emits an AbortError.
function abortChildOnSignal(child, signal, killSignal) {
  if (typeof signal !== "object" || typeof signal.aborted !== "boolean") {
    throw new __errors.ERR_INVALID_ARG_TYPE("options.signal", "AbortSignal", signal);
  }
  const onAbort = () => {
    if (child.exitCode !== null || child.signalCode !== null) return;
    if (child.kill(killSignal)) {
      const { AbortError } = __builtins.get("events");
      const error = AbortError
        ? new AbortError(undefined, { cause: signal.reason })
        : Object.assign(new Error("The operation was aborted", { cause: signal.reason }), { name: "AbortError", code: "ABORT_ERR" });
      child.emit("error", error);
    }
  };
  if (signal.aborted) {
    process.nextTick(onAbort);
  } else {
    signal.addEventListener("abort", onAbort, { once: true });
    child.once("exit", () => signal.removeEventListener("abort", onAbort));
  }
}

// Collect a ChildProcess's stdout/stderr and invoke a Node-style callback.
// exec/execFile's callback plumbing, with Node's `timeout`, `killSignal` and `maxBuffer`.
function collect(child, options, callback, cmd) {
  const encoding = options.encoding ?? "utf8";
  const maxBuffer = options.maxBuffer ?? 1024 * 1024;
  const out = [];
  const err = [];
  let outLength = 0, errLength = 0;
  let done = false;
  let killed = false;
  let failure = null;
  let timer = null;
  // Node's execFile kill(): drop our ends of the pipes and kill the child. A grandchild (say
  // the program a shell started) may still hold the pipes, so once the child itself has exited
  // it no longer keeps the event loop alive.
  const kill = () => {
    if (child.stdout) child.stdout.destroy();
    if (child.stderr) child.stderr.destroy();
    killed = true;
    child.once("exit", () => child.unref());
    try { child.kill(options.killSignal); } catch (e) { finish(e); }
  };
  const onData = (chunks, which) => (chunk) => {
    if (done || killed) return;
    const length = (which === 1 ? (outLength += chunk.length) : (errLength += chunk.length));
    if (maxBuffer !== Infinity && length > maxBuffer) {
      const name = which === 1 ? "stdout" : "stderr";
      failure = new RangeError(`${name} maxBuffer length exceeded`);
      failure.code = "ERR_CHILD_PROCESS_STDIO_MAXBUFFER";
      chunks.push(chunk.subarray(0, chunk.length - (length - maxBuffer)));
      kill();
      return;
    }
    chunks.push(chunk);
  };
  if (child.stdout) child.stdout.on("data", onData(out, 1));
  if (child.stderr) child.stderr.on("data", onData(err, 2));
  const finish = (error, code, signal) => {
    if (done) return;
    done = true;
    if (timer) clearTimeout(timer);
    const stdout = Buffer.concat(out);
    const stderr = Buffer.concat(err);
    const asText = (b) => (encoding && encoding !== "buffer" ? b.toString(encoding) : b);
    if (!error && failure) error = failure;
    if (!error && (code !== 0 || signal !== null)) {
      error = new Error(`Command failed: ${cmd}\n${asText(stderr)}`);
      error.code = code;
      error.killed = child.killed || killed;
      error.signal = signal;
    }
    if (error) {
      if (error.cmd === undefined) error.cmd = cmd;
      return callback(error, asText(stdout), asText(stderr));
    }
    callback(null, asText(stdout), asText(stderr));
  };
  if (options.timeout > 0) timer = setTimeout(() => { timer = null; kill(); }, options.timeout);
  child.on("error", (e) => finish(e));
  child.on("close", (code, signal) => finish(null, code, signal ?? null));
}

function exec(command, options, callback) {
  if (typeof options === "function") {
    callback = options;
    options = {};
  }
  options = options || {};
  const child = spawn(command, { ...options, shell: options.shell || true });
  if (callback) collect(child, options, callback, command);
  return child;
}

function execFile(file, args, options, callback) {
  if (typeof args === "function") {
    callback = args;
    args = [];
    options = {};
  } else if (typeof options === "function") {
    callback = options;
    options = {};
  }
  const child = spawn(file, args || [], options || {});
  if (callback) collect(child, options || {}, callback, [file, ...(args || [])].join(" "));
  return child;
}

// ---- synchronous variants ---------------------------------------------------------------------

function makeSyncResult(res, encoding) {
  const enc = encoding && encoding !== "buffer" ? encoding : null;
  const stdout = Buffer.from(res.stdout);
  const stderr = Buffer.from(res.stderr);
  return {
    pid: 0,
    status: res.status,
    signal: null,
    stdout: enc ? stdout.toString(enc) : stdout,
    stderr: enc ? stderr.toString(enc) : stderr,
    output: [null, enc ? stdout.toString(enc) : stdout, enc ? stderr.toString(enc) : stderr],
  };
}

// Node's result shape; a failed spawn carries `error` and has no output or status.
function syncResult(res, error, encoding) {
  if (error) return { error, status: null, signal: null, output: null, pid: 0, stdout: null, stderr: null };
  return makeSyncResult(res, encoding);
}

function spawnSync(command, args, options) {
  if (!Array.isArray(args)) {
    options = args;
    args = [];
  }
  options = options || {};
  const { file, args: argv, verbatim } = resolveCommand(command, args, options);
  const input = options.input ? (Buffer.isBuffer(options.input) ? options.input : Buffer.from(options.input)) : null;
  try {
    const res = __child.execSync(file, argv, input, options.cwd, envPairs(options.env || process.env), verbatim, options.timeout);
    const ret = syncResult(res, null, options.encoding);
    if (res.timedOut) {
      // Node: the child was killed with killSignal and the result carries ETIMEDOUT.
      const error = new Error(`spawnSync ${file} ETIMEDOUT`);
      error.errno = __uvCodes.get("ETIMEDOUT");
      error.code = "ETIMEDOUT";
      error.syscall = `spawnSync ${file}`;
      error.path = file;
      error.spawnargs = argv;
      ret.error = error;
      ret.status = null;
      ret.signal = typeof options.killSignal === "string" ? options.killSignal : "SIGTERM";
    }
    return ret;
  } catch (e) {
    const error = spawnError(e, "spawnSync", file, argv);
    if (!error) throw e;
    return syncResult(null, error, options.encoding);
  }
}

// execFileSync/execSync: the spawnSync result, thrown as an error if the spawn failed or the
// child exited non-zero; otherwise its stdout.
function checkedOutput(ret, what, options) {
  if (ret.error) throw ret.error;
  if (ret.status !== 0 && ret.status !== null) {
    const e = new Error(`Command failed: ${what}`);
    e.status = ret.status;
    e.stdout = ret.stdout;
    e.stderr = ret.stderr;
    throw e;
  }
  return ret.stdout;
}

function execFileSync(file, args, options) {
  if (!Array.isArray(args)) {
    options = args;
    args = [];
  }
  options = options || {};
  const ret = spawnSync(file, args, options);
  return checkedOutput(ret, [file, ...(args || [])].join(" "), options);
}

function execSync(command, options) {
  options = options || {};
  const ret = spawnSync(command, [], { ...options, shell: options.shell || true });
  return checkedOutput(ret, command, options);
}

function fork(modulePath, args, options) {
  if (!Array.isArray(args)) {
    options = args;
    args = [];
  }
  options = options || {};
  const env = { ...process.env, ...(options.env || {}), LUMEN_FORK_IPC: "1" };
  // Node: `stdio` wins over `silent`; `silent` pipes all three, the default inherits them.
  let stdio = options.stdio ?? (options.silent ? "pipe" : "inherit");
  if (typeof stdio === "string") stdio = [stdio, stdio, stdio];
  stdio = Array.from(stdio).filter(v => v !== "ipc");
  // The IPC channel rides the child's stdin/stdout, so those two are always pipes here; what
  // the caller asked for stdout is honoured by relaying its non-IPC output below.
  const child = spawn(process.execPath, [String(modulePath), ...(args || []).map(String)], {
    ...options,
    env,
    stdio: ["pipe", "pipe", stdio[2] ?? "pipe", ...stdio.slice(3)],
    _ipc: true,
  });
  const out = stdio[1] ?? "pipe";
  if (child.stdout && out !== "pipe") {
    if (out === "ignore") {
      child.stdout.resume();
      child.stdout = child.stdio[1] = null;
    } else {
      relayStdio(child, [undefined, out === "inherit" ? process.stdout : out]);
    }
  }
  return child;
}

// The child-side channel is installed by the process bootstrap in stdlib_extras.js.
function _forkChild() {}

__builtins.set("child_process", {
  spawn,
  exec,
  execFile,
  execFileSync,
  execSync,
  spawnSync,
  fork,
  _forkChild,
  ChildProcess,
});
