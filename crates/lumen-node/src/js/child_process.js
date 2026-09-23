// node:child_process over the __child native ops (real std::process subprocesses). spawn returns a
// ChildProcess (EventEmitter) whose stdout/stderr are Readable node streams pumped by one-shot
// reads and whose stdin is a Writable; exec*/spawnSync build on that or the synchronous execSync
// op. `kill()` sends SIGKILL (std can't send arbitrary signals). `fork` re-execs the current Lumen
// binary and carries JSON-framed IPC over its piped stdin/stdout.

const EventEmitter = __builtins.get("events");
const IPC_PREFIX = "\x1eLUMEN_IPC ";

// Normalize the stdio option to a list of "pipe" | "inherit" | "ignore" — three entries for the
// standard fds, plus one per extra slot (`stdio[3]`...), which only "pipe" and "ignore" support.
function normalizeStdio(stdio) {
  if (stdio === "inherit") return ["inherit", "inherit", "inherit"];
  if (stdio === "ignore") return ["ignore", "ignore", "ignore"];
  if (Array.isArray(stdio)) {
    const out = [];
    for (let i = 0; i < Math.max(3, stdio.length); i++) {
      const v = stdio[i];
      out.push(i >= 3 ? (v === "pipe" ? "pipe" : "ignore") : v === "inherit" || v === "ignore" ? v : "pipe");
    }
    return out;
  }
  return ["pipe", "pipe", "pipe"];
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
const SPAWN_ERRNO = { ENOENT: -2, EACCES: -13, ENOTDIR: -20, EINVAL: -22 };

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
        if (line.startsWith(IPC_PREFIX)) {
          try {
            onIpcMessage(JSON.parse(line.slice(IPC_PREFIX.length)));
          } catch (error) {
            stream.destroy(error);
            return;
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
        const signal = signalNumber === null ? null : SIGNAL_NAMES[signalNumber] ?? `SIG${signalNumber}`;
        this.exitCode = code;
        this.signalCode = signal;
        this.emit("exit", code, signal);
        // 'close' should follow once stdio has flushed; a microtask is close enough here.
        queueMicrotask(() => this.emit("close", code, signal));
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
    if (ok) this.killed = true;
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
  child = new ChildProcess(info.childId, info.pid, stdio, onIpcMessage);
  return child;
}

// Collect a ChildProcess's stdout/stderr and invoke a Node-style callback.
function collect(child, encoding, callback) {
  const out = [];
  const err = [];
  if (child.stdout) child.stdout.on("data", (c) => out.push(c));
  if (child.stderr) child.stderr.on("data", (c) => err.push(c));
  let done = false;
  const finish = (error, code) => {
    if (done) return;
    done = true;
    const stdout = Buffer.concat(out);
    const stderr = Buffer.concat(err);
    const asText = (b) => (encoding && encoding !== "buffer" ? b.toString(encoding) : b);
    if (error) return callback(error, asText(stdout), asText(stderr));
    if (code !== 0) {
      const e = new Error(`Command failed with exit code ${code}`);
      e.code = code;
      return callback(e, asText(stdout), asText(stderr));
    }
    callback(null, asText(stdout), asText(stderr));
  };
  child.on("error", (e) => finish(e));
  child.on("close", (code) => finish(null, code));
}

function exec(command, options, callback) {
  if (typeof options === "function") {
    callback = options;
    options = {};
  }
  options = options || {};
  const child = spawn(command, { ...options, shell: options.shell || true });
  if (callback) collect(child, options.encoding ?? "utf8", callback);
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
  if (callback) collect(child, (options && options.encoding) ?? "utf8", callback);
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
    const res = __child.execSync(file, argv, input, options.cwd, envPairs(options.env || process.env), verbatim);
    return syncResult(res, null, options.encoding);
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
  return spawn(process.execPath, [String(modulePath), ...(args || []).map(String)], {
    ...options,
    env,
    stdio: ["pipe", "pipe", options.silent ? "pipe" : "inherit"],
    _ipc: true,
  });
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
