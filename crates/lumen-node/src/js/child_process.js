// node:child_process over the __child native ops (real std::process subprocesses). The argument
// validation and the exec/execFile/spawnSync front ends follow Node's lib/child_process.js; the
// ChildProcess object streams stdout/stderr as Readables pumped by one-shot reads. `fork` (and any
// `stdio: [..., 'ipc']`) re-execs a Lumen binary and carries newline-framed IPC over a dedicated
// descriptor (a socketpair dup2'd onto the child's stdio slot) on Unix, or over the child's piped
// stdin/stdout on Windows.

const EventEmitter = __builtins.get("events");
const {
  ERR_INVALID_ARG_TYPE, ERR_INVALID_ARG_VALUE, ERR_OUT_OF_RANGE, ERR_MISSING_ARGS, ERR_UNKNOWN_SIGNAL,
  ERR_IPC_CHANNEL_CLOSED, ERR_IPC_DISCONNECTED, ERR_IPC_ONE_PIPE, ERR_IPC_SYNC_FORK,
  ERR_CHILD_PROCESS_IPC_REQUIRED, ERR_CHILD_PROCESS_STDIO_MAXBUFFER, ERR_INVALID_SYNC_FORK_INPUT,
} = __errors;
const {
  validateString, validateObject, validateArray, validateBoolean, validateFunction, validateAbortSignal,
  validateOneOf,
} = __validators;
const MAX_BUFFER = 1024 * 1024;
const IPC_PREFIX = "\x1eLUMEN_IPC ";
// The child's `process.disconnect()` closes its end of the channel with this control line.
const IPC_DISCONNECT = "\x1eLUMEN_IPC_DISCONNECT";

const __ipcWire = {
  limit: 64 * 1024 * 1024,
  encode(value, advanced) {
    const text = advanced ? "\x1eLUMEN_IPC_ADV " + __builtins.get("v8").serialize(value).toString("base64")
      : IPC_PREFIX + JSON.stringify(value === undefined ? null : value);
    const bytes = Buffer.from(text + "\n");
    if (bytes.length > this.limit) throw new RangeError("IPC frame exceeds 64 MiB");
    return bytes;
  },
  decode(line) {
    if (line.startsWith("\x1eLUMEN_IPC_ADV ")) return __builtins.get("v8").deserialize(Buffer.from(line.slice(15), "base64"));
    if (line.startsWith(IPC_PREFIX)) return JSON.parse(line.slice(IPC_PREFIX.length));
    throw new Error("Invalid Lumen IPC frame");
  },
  // Node's validation of what `send` accepts, shared with the child's `process.send`.
  validate(message) {
    if (message === undefined) throw new ERR_MISSING_ARGS("message");
    if (typeof message !== "string" && typeof message !== "object" && typeof message !== "number" && typeof message !== "boolean") {
      throw new ERR_INVALID_ARG_TYPE("message", ["string", "object", "number", "boolean"], message);
    }
  },
};

const isInt32 = (value) => value === (value | 0);

function validateArgumentNullCheck(arg, propName) {
  if (typeof arg === "string" && arg.includes("\u0000")) {
    throw new ERR_INVALID_ARG_VALUE(propName, arg, "must be a string without null bytes");
  }
}

function validateArgumentsNullCheck(args, propName) {
  for (let i = 0; i < args.length; ++i) validateArgumentNullCheck(args[i], `${propName}[${i}]`);
}

function validateTimeout(timeout) {
  if (timeout != null && !(Number.isInteger(timeout) && timeout >= 0)) {
    throw new ERR_OUT_OF_RANGE("timeout", "an unsigned integer", timeout);
  }
}

function validateMaxBuffer(maxBuffer) {
  if (maxBuffer != null && !(typeof maxBuffer === "number" && maxBuffer >= 0)) {
    throw new ERR_OUT_OF_RANGE("options.maxBuffer", "a positive number", maxBuffer);
  }
}

const SIGNALS = (() => {
  const common = { SIGHUP: 1, SIGINT: 2, SIGQUIT: 3, SIGILL: 4, SIGTRAP: 5, SIGABRT: 6, SIGIOT: 6, SIGFPE: 8, SIGKILL: 9, SIGSEGV: 11, SIGPIPE: 13, SIGALRM: 14, SIGTERM: 15 };
  if (process.platform === "darwin") {
    return { ...common, SIGBUS: 10, SIGSYS: 12, SIGURG: 16, SIGSTOP: 17, SIGTSTP: 18, SIGCONT: 19, SIGCHLD: 20, SIGTTIN: 21, SIGTTOU: 22, SIGIO: 23, SIGXCPU: 24, SIGXFSZ: 25, SIGVTALRM: 26, SIGPROF: 27, SIGWINCH: 28, SIGINFO: 29, SIGUSR1: 30, SIGUSR2: 31 };
  }
  if (process.platform === "win32") {
    return { SIGHUP: 1, SIGINT: 2, SIGILL: 4, SIGABRT: 22, SIGFPE: 8, SIGKILL: 9, SIGSEGV: 11, SIGTERM: 15, SIGBREAK: 21, SIGWINCH: 28 };
  }
  return { ...common, SIGBUS: 7, SIGUSR1: 10, SIGUSR2: 12, SIGSTKFLT: 16, SIGCHLD: 17, SIGCONT: 18, SIGSTOP: 19, SIGTSTP: 20, SIGTTIN: 21, SIGTTOU: 22, SIGURG: 23, SIGXCPU: 24, SIGXFSZ: 25, SIGVTALRM: 26, SIGPROF: 27, SIGWINCH: 28, SIGIO: 29, SIGPOLL: 29, SIGPWR: 30, SIGSYS: 31 };
})();
const SIGNAL_NAMES = {};
for (const [name, n] of Object.entries(SIGNALS)) if (!(n in SIGNAL_NAMES)) SIGNAL_NAMES[n] = name;

function convertToValidSignal(signal) {
  if (typeof signal === "number" && SIGNAL_NAMES[signal]) return signal;
  if (typeof signal === "string") {
    const n = SIGNALS[signal.toUpperCase()];
    if (n) return n;
  }
  throw new ERR_UNKNOWN_SIGNAL(signal);
}

function sanitizeKillSignal(killSignal) {
  if (typeof killSignal === "string" || typeof killSignal === "number") return convertToValidSignal(killSignal);
  if (killSignal != null) throw new ERR_INVALID_ARG_TYPE("options.killSignal", ["string", "number"], killSignal);
}

// A native spawn failure as Node reports it: `spawn foo ENOENT` with errno, syscall, path and
// spawnargs (the arguments after argv0).
function spawnError(err, syscall, file, args) {
  const code = err && err.code;
  if (typeof code !== "string" || !__uvCodes().has(code)) return null;
  return errnoError(__uvCodes().get(code), syscall, file, args);
}

function errnoError(errno, syscall, file, args) {
  const e = __builtins.get("util")._errnoException(errno, file ? `${syscall} ${file}` : syscall);
  if (file) {
    e.path = file;
    e.spawnargs = args;
  }
  return e;
}

const stdioNames = ["stdin", "stdout", "stderr"];

function stdioStringToArray(stdio, channel) {
  const options = [];
  switch (stdio) {
    case "ignore":
    case "overlapped":
    case "pipe": options.push(stdio, stdio, stdio); break;
    case "inherit": options.push("inherit", "inherit", "inherit"); break;
    default: throw new ERR_INVALID_ARG_VALUE("stdio", stdio);
  }
  if (channel) options.push(channel);
  return options;
}

// Node's getValidStdio: each slot becomes { type, ... }. Slots lumen cannot hand to the child as
// an fd (a stream or another process's fd in a standard slot) become 'relay' entries that the
// parent pumps itself.
function getValidStdio(stdio, sync) {
  let ipc;
  let ipcFd;
  if (typeof stdio === "string") stdio = stdioStringToArray(stdio);
  else if (!Array.isArray(stdio)) throw new ERR_INVALID_ARG_VALUE("stdio", stdio);
  stdio = Array.from(stdio);
  while (stdio.length < 3) stdio.push(undefined);
  const out = stdio.map((value, i) => {
    if (value == null) value = i < 3 ? "pipe" : "ignore";
    if (value === "ignore") return { type: "ignore" };
    if (value === "pipe" || value === "overlapped" || (typeof value === "number" && value < 0)) return { type: "pipe" };
    if (value === "ipc") {
      if (sync || ipc !== undefined) {
        if (!sync) throw new ERR_IPC_ONE_PIPE();
        throw new ERR_IPC_SYNC_FORK();
      }
      ipc = true;
      ipcFd = i;
      return { type: "ipc" };
    }
    if (value === "inherit") return { type: "inherit", fd: i };
    if (i >= 3 && typeof value === "object" && process.platform !== "win32") {
      const wrap = value._handle && typeof value._handle.fd === "number" ? value._handle : value;
      if (typeof wrap.fd === "number" && wrap.fd >= 0 && (wrap !== value || typeof value.readStart === "function")) {
        return { type: "wrap", fd: wrap.fd };
      }
    }
    if (typeof value === "number" || (typeof value === "object" && typeof value.fd === "number")) {
      const fd = typeof value === "number" ? value : value.fd;
      if (fd === i) return { type: "inherit", fd };
      return { type: "relay", fd, stream: typeof value === "object" ? value : undefined };
    }
    if (typeof value === "object" && (typeof value.write === "function" || typeof value.pipe === "function")) {
      return { type: "relay", stream: value };
    }
    if (ArrayBuffer.isView(value) || typeof value === "string") {
      if (!sync) throw new ERR_INVALID_SYNC_FORK_INPUT(__builtins.get("util").inspect(value));
      return { type: "ignore" };
    }
    throw new ERR_INVALID_ARG_VALUE("stdio", value);
  });
  return { stdio: out, ipc, ipcFd };
}

// A process's own stdin/stdout/stderr object in the matching slot is inherited.
function isOwnStdio(v, i) {
  if (v === i) return true;
  const own = [process.stdin, process.stdout, process.stderr][i];
  return v !== null && typeof v === "object" && v === own;
}

function getValidatedCwd(cwd) {
  if (typeof cwd === "string") {
    validateArgumentNullCheck(cwd, "options.cwd");
    return cwd;
  }
  if (cwd instanceof Uint8Array) return Buffer.from(cwd).toString();
  if (cwd !== null && typeof cwd === "object" && typeof cwd.href === "string" && typeof cwd.protocol === "string") {
    return __builtins.get("url").fileURLToPath(cwd);
  }
  throw new ERR_INVALID_ARG_TYPE("options.cwd", ["string", "Buffer", "URL"], cwd);
}

function normalizeSpawnArguments(file, args, options) {
  validateString(file, "file");
  validateArgumentNullCheck(file, "file");
  if (file.length === 0) throw new ERR_INVALID_ARG_VALUE("file", file, "cannot be empty");
  if (Array.isArray(args)) args = args.slice();
  else if (args == null) args = [];
  else if (typeof args !== "object") throw new ERR_INVALID_ARG_TYPE("args", "object", args);
  else {
    options = args;
    args = [];
  }
  validateArgumentsNullCheck(args, "args");
  if (options === undefined) options = {};
  else validateObject(options, "options");
  let cwd = options.cwd;
  if (cwd != null) cwd = getValidatedCwd(cwd);
  if (options.detached != null) validateBoolean(options.detached, "options.detached");
  if (options.uid != null && !isInt32(options.uid)) throw new ERR_INVALID_ARG_TYPE("options.uid", "int32", options.uid);
  if (options.gid != null && !isInt32(options.gid)) throw new ERR_INVALID_ARG_TYPE("options.gid", "int32", options.gid);
  if (options.shell != null && typeof options.shell !== "boolean" && typeof options.shell !== "string") {
    throw new ERR_INVALID_ARG_TYPE("options.shell", ["boolean", "string"], options.shell);
  }
  if (options.argv0 != null) {
    validateString(options.argv0, "options.argv0");
    validateArgumentNullCheck(options.argv0, "options.argv0");
  }
  if (options.windowsHide != null) validateBoolean(options.windowsHide, "options.windowsHide");
  let { windowsVerbatimArguments } = options;
  if (windowsVerbatimArguments != null) validateBoolean(windowsVerbatimArguments, "options.windowsVerbatimArguments");
  if (options.shell) {
    validateArgumentNullCheck(options.shell, "options.shell");
    const command = [file, ...args].join(" ");
    if (process.platform === "win32") {
      file = typeof options.shell === "string" ? options.shell : process.env.comspec || "cmd.exe";
      if (/^(?:.*\\)?cmd(?:\.exe)?$/i.test(file)) {
        args = ["/d", "/s", "/c", `"${command}"`];
        windowsVerbatimArguments = true;
      } else {
        args = ["-c", command];
      }
    } else {
      file = typeof options.shell === "string" ? options.shell : "/bin/sh";
      args = ["-c", command];
    }
  }
  if (typeof options.argv0 === "string") args.unshift(options.argv0);
  else args.unshift(file);
  const env = options.env || process.env;
  const envPairs = [];
  for (const key in env) {
    const value = env[key];
    if (value !== undefined) {
      validateArgumentNullCheck(key, `options.env['${key}']`);
      validateArgumentNullCheck(value, `options.env['${key}']`);
      envPairs.push(`${key}=${value}`);
    }
  }
  return {
    ...options,
    args,
    cwd,
    detached: !!options.detached,
    envPairs,
    file,
    windowsHide: !!options.windowsHide,
    windowsVerbatimArguments: !!windowsVerbatimArguments,
  };
}

// "k=v" strings to the [k, v] pairs the native spawn takes (the first '=' splits).
function nativeEnv(envPairs) {
  if (envPairs === undefined) return undefined;
  return envPairs.map((pair) => {
    const eq = String(pair).indexOf("=", 1);
    return eq < 0 ? [String(pair), ""] : [pair.slice(0, eq), pair.slice(eq + 1)];
  });
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


function maybeClose(child) {
  child._closesGot++;
  if (child._closesGot === child._closesNeeded) child.emit("close", child.exitCode, child.signalCode);
}

// Streams nobody touched are drained after the exit, so their 'close' (and the child's) arrives.
function flushStdio(child) {
  const stdio = child.stdio;
  if (stdio == null) return;
  for (const stream of stdio) {
    if (!stream || !stream.readable) continue;
    stream.resume();
  }
}

const kPending = Symbol("lumen.child.pendingIpc");

class ChildProcess extends EventEmitter {
  constructor() {
    super();
    this._closesNeeded = 1;
    this._closesGot = 0;
    this.connected = false;
    this.signalCode = null;
    this.exitCode = null;
    this.killed = false;
    this.spawnfile = null;
    this._handle = null;
    this._id = null;
    this._ipcPipe = null;
  }

  spawn(options) {
    validateObject(options, "options");
    const { stdio: stdios, ipc, ipcFd } = getValidStdio(options.stdio || "pipe", false);
    validateOneOf(options.serialization, "options.serialization", [undefined, "json", "advanced"]);
    const serialization = options.serialization || "json";
    let envPairs = options.envPairs;
    if (ipc !== undefined) {
      if (envPairs === undefined) envPairs = [];
      else validateArray(envPairs, "options.envPairs");
    }
    const file = options.file;
    validateString(file, "options.file");
    this.spawnfile = file;
    if (options.args === undefined) {
      this.spawnargs = [];
    } else {
      validateArray(options.args, "options.args");
      this.spawnargs = options.args;
    }
    const legacyIpc = ipc !== undefined && process.platform === "win32";
    const native = stdios.map((s, i) => {
      switch (s.type) {
        case "pipe": return "pipe";
        case "inherit": return i < 3 ? "inherit" : "ignore";
        case "relay": return i < 3 ? "pipe" : "ignore";
        case "ipc": return !legacyIpc && i >= 3 ? "ipc" : "ignore";
        case "wrap": return `fd:${s.fd}`;
        default: return "ignore";
      }
    });
    if (ipc !== undefined) {
      envPairs = envPairs.slice();
      if (legacyIpc) {
        native[0] = "pipe";
        native[1] = "pipe";
        envPairs.push("LUMEN_FORK_IPC=1");
      } else {
        // An ipc slot among the standard three moves past them (lumen's stdio 0-2 are std pipes).
        const ipcSlot = ipcFd >= 3 ? ipcFd : Math.max(3, native.length);
        while (native.length <= ipcSlot) native.push("ignore");
        native[ipcSlot] = "ipc";
        envPairs.push(`NODE_CHANNEL_FD=${ipcSlot}`, `NODE_CHANNEL_SERIALIZATION_MODE=${serialization}`);
      }
    } else if (envPairs !== undefined) {
      validateArray(envPairs, "options.envPairs");
    }
    const argv = this.spawnargs.slice(1).map(String);
    const argv0 = this.spawnargs.length > 0 && String(this.spawnargs[0]) !== file ? String(this.spawnargs[0]) : undefined;
    let cwd = options.cwd;
    if (cwd === "" || cwd === null) cwd = undefined;
    let info;
    try {
      info = __child.spawn(file, argv, cwd, nativeEnv(envPairs), native, !!options.windowsVerbatimArguments,
        { argv0, uid: options.uid, gid: options.gid, detached: !!options.detached });
    } catch (e) {
      const errno = e && typeof e.code === "string" ? __uvCodes().get(e.code) : undefined;
      if (errno === undefined) throw e;
      if (!["EACCES", "EAGAIN", "EMFILE", "ENFILE", "ENOENT"].includes(e.code)) {
        throw errnoError(errno, "spawn");
      }
      this.pid = undefined;
      this._failed(stdios, errnoError(errno, "spawn", file, this.spawnargs.slice(1)));
      return errno;
    }
    this._id = info.childId;
    this._handle = { pid: info.pid };
    this.pid = info.pid;
    const id = info.childId;
    const streams = [];
    const closeOn = (stream) => {
      this._closesNeeded++;
      stream.once("close", () => maybeClose(this));
    };
    for (let i = 0; i < stdios.length; i++) {
      const s = stdios[i];
      if (s.type === "pipe") {
        const stream = i === 0 ? makeWritable(id) : i < 3 ? makeReadable(id, i) : makeExtraPipe(id, i);
        if (i > 0) closeOn(stream);
        streams.push(stream);
      } else if (s.type === "relay" && i < 3) {
        const target = s.stream ?? s.fd;
        const pipe = i === 0 ? makeWritable(id) : makeReadable(id, i);
        relayPipe(i, pipe, target);
        if (i > 0) closeOn(pipe);
        streams.push(null);
      } else {
        streams.push(null);
      }
    }
    this.stdin = streams[0] ?? null;
    this.stdout = streams[1] ?? null;
    this.stderr = streams[2] ?? null;
    this.stdio = streams;
    if (ipc !== undefined) {
      if (legacyIpc) {
        this._legacyIpc(stdios);
      } else {
        this._closesNeeded++;
        setupChannel(this, new IpcPipe(__net.adoptFd(info.ipcFd).desc[0]), serialization);
      }
    }
    queueMicrotask(() => this.emit("spawn"));
    new Promise((resolve, reject) => __child.wait(id, resolve, reject)).then(
      ([code, signalNumber]) => this._onexit(code, signalNumber),
      (err) => this.emit("error", err),
    );
    return 0;
  }

  _legacyIpc(stdios) {
    const out = stdios[1];
    Object.assign(this, legacyIpcMethods);
    this.connected = true;
    const onMessage = (message) => this.emit("message", message, null);
    onMessage.close = () => this._ipcClosed();
    this.stdout = makeReadable(this._id, 1, onMessage);
    this.stdio[1] = this.stdout;
    this._closesNeeded++;
    this.stdout.once("close", () => maybeClose(this));
    if (out.type !== "pipe" && out.type !== "ipc") {
      if (out.type === "ignore") {
        this.stdout.resume();
      } else {
        const target = out.type === "inherit" ? process.stdout : out.stream ?? out.fd;
        relayPipe(1, this.stdout, target);
      }
      this.stdout = this.stdio[1] = null;
    }
    this._ipcAdvanced = false;
  }

  // A spawn that failed (missing program, no permission): like Node, the object still has its
  // stdio streams but no pid, then emits 'error' and 'close' with the negative errno, never 'spawn'
  // or 'exit'.
  _failed(stdios, error) {
    const { Readable, Writable } = __builtins.get("stream");
    const ended = () => {
      const stream = new Readable({ read() {} });
      stream.push(null);
      return stream;
    };
    const streams = stdios.map((s, i) => {
      if (s.type !== "pipe" && !(s.type === "relay" && i < 3)) return null;
      return i === 0 ? new Writable({ write(chunk, enc, cb) { cb(); } }) : ended();
    });
    this._id = null;
    this.exitCode = error.errno;
    this.stdin = streams[0] ?? null;
    this.stdout = streams[1] ?? null;
    this.stderr = streams[2] ?? null;
    this.stdio = streams;
    this.connected = false;
    process.nextTick(() => {
      this.emit("error", error);
      this.emit("close", error.errno, null);
    });
  }

  _onexit(code, signalNumber) {
    let signal = signalNumber === null ? null : SIGNAL_NAMES[signalNumber] ?? `SIG${signalNumber}`;
    // Windows has no signals: libuv reports a process it killed by the signal it was sent.
    if (signal === null && this._killSignal !== undefined && process.platform === "win32") {
      code = null;
      signal = this._killSignal;
    }
    if (signal) this.signalCode = signal;
    else this.exitCode = code;
    if (this.stdin) this.stdin.destroy();
    this._handle = null;
    if (this.connected && this._ipcClosed) {
      const timer = setTimeout(() => this._ipcClosed(), 100);
      timer.unref();
    }
    this.emit("exit", this.exitCode, this.signalCode);
    process.nextTick(flushStdio, this);
    maybeClose(this);
  }

  kill(sig) {
    const signal = sig === 0 ? sig : convertToValidSignal(sig === undefined ? "SIGTERM" : sig);
    if (this._handle && this._id !== null) {
      if (__child.kill(this._id, signal)) {
        this.killed = true;
        if (signal !== 0 && this.exitCode === null && this.signalCode === null) this._killSignal = SIGNAL_NAMES[signal] ?? `SIG${signal}`;
        return true;
      }
    }
    return false;
  }

  [Symbol.dispose]() {
    if (!this.killed) this.kill();
  }

  ref() {
    if (this._id !== null) __child.ref(this._id);
  }

  unref() {
    // Detach from the event loop's keep-alive count (Node semantics), so a long-lived service
    // child (esbuild) doesn't block process exit once the main work is done. esbuild toggles
    // ref/unref per request, so both must be real.
    if (this._id !== null) __child.unref(this._id);
  }

}

// Windows: the channel rides the child's stdin/stdout (see _legacyIpc).
const legacyIpcMethods = {
  send(message, handle, options, callback) {
    if (typeof handle === "function") {
      callback = handle;
      handle = undefined;
      options = undefined;
    } else if (typeof options === "function") {
      callback = options;
      options = undefined;
    } else if (options !== undefined) {
      validateObject(options, "options");
    }
    if (this.connected) return this._send(message, handle, options, callback);
    const ex = new ERR_IPC_CHANNEL_CLOSED();
    if (typeof callback === "function") process.nextTick(callback, ex);
    else process.nextTick(() => this.emit("error", ex));
    return false;
  },

  _send(message, handle, options, callback) {
    __ipcWire.validate(message);
    if (handle != null) {
      const error = new Error("child_process.fork handle transfer is not supported in lumen");
      if (typeof callback === "function") process.nextTick(callback, error);
      else process.nextTick(() => this.emit("error", error));
      return false;
    }
    const target = this._ipcPipe || this.stdin;
    const frame = __ipcWire.encode(message, this._ipcAdvanced);
    const written = target.write(frame, typeof callback === "function" ? (error) => callback(error || null) : undefined);
    return written && target.writableLength < 131072;
  },

  disconnect() {
    if (!this.connected) {
      this.emit("error", new ERR_IPC_DISCONNECTED());
      return;
    }
    if (this._ipcPipe) this._ipcPipe.end(Buffer.from(IPC_DISCONNECT + "\n"));
    else if (this.stdin) this.stdin.end();
    this._ipcClosed();
  },

  // The channel went away (we disconnected, the child did, or it exited).
  _ipcClosed() {
    if (!this.connected) return;
    this.connected = false;
    this.channel = null;
    if (!this._ipcPipe && this.stdin && !this.stdin.writableEnded) this.stdin.end();
    process.nextTick(() => {
      this.emit("disconnect");
      if (this._ipcCounted) maybeClose(this);
    });
  }
};

// ---- the IPC channel (Node's lib/internal/child_process.js setupChannel) ----------------------
// On unix the channel is a Unix socket (a socketpair the spawn wires onto the child's
// NODE_CHANNEL_FD), framed as Node frames it: JSON lines, or length-prefixed v8 serialization
// for `serialization: 'advanced'`. Handles (net/dgram sockets and servers) travel as SCM_RIGHTS
// descriptors announced by a NODE_HANDLE message and acknowledged with NODE_HANDLE_ACK.

const kChannelHandle = Symbol("kChannelHandle");
const kPendingMessages = Symbol("kPendingMessages");
const kJSONBuffer = Symbol("kJSONBuffer");
const kStringDecoder = Symbol("kStringDecoder");
const kMessageBuffer = Symbol("kMessageBuffer");
const kMessageBufferSize = Symbol("kMessageBufferSize");
const MAX_HANDLE_RETRANSMISSIONS = 3;
const UV_EBADF = () => __uvCodes().get("EBADF");
const nop = () => {};

// The channel socket: libuv's ipc pipe over the net ops. Reads run one at a time, re-armed while
// reading is on; received descriptors queue up until their NODE_HANDLE message is parsed.
class IpcPipe {
  constructor(id) {
    this._id = id;
    this._closed = false;
    this._reading = false;
    this._inFlight = false;
    this._eof = false;
    this._held = [];
    this._fds = [];
    this._queue = [];
    this._writing = false;
    this._queuedBytes = 0;
    this._drainWaiters = [];
    this.onread = null;
    this.buffering = false;
    this.lastWriteWasAsync = false;
  }
  get fd() { return this._id === null ? -1 : __net.socketFd(this._id); }
  get writeQueueSize() { return this._queuedBytes; }
  readStart() {
    if (this._closed) return UV_EBADF();
    this._reading = true;
    if (this._held.length !== 0) process.nextTick(() => this._drain());
    else this._arm();
    return 0;
  }
  readStop() {
    this._reading = false;
    return 0;
  }
  _arm() {
    if (!this._reading || this._inFlight || this._closed || this._eof || this._held.length !== 0) return;
    this._inFlight = true;
    __net.readMsg(this._id, (bytes, fds) => this._onRead(bytes, fds), () => this._onRead(null, []));
  }
  _onRead(bytes, fds) {
    this._inFlight = false;
    if (this._closed) {
      for (const fd of fds) __net.closeFd(fd);
      return;
    }
    this._held.push([bytes, fds]);
    if (this._reading) this._drain();
  }
  _drain() {
    try {
      while (this._reading && !this._closed && this._held.length !== 0) {
        const [bytes, fds] = this._held.shift();
        for (const fd of fds) this._fds.push(fd);
        if (bytes === null) {
          this._eof = true;
          this.onread(null);
          return;
        }
        this.onread(Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength));
      }
    } finally {
      this._arm();
    }
  }
  // The handle for the next descriptor received, or null if none arrived (the sender then
  // gets a NODE_HANDLE_NACK).
  takeHandle() {
    const fd = this._fds.shift();
    if (fd === undefined) return null;
    try {
      return __internals.get("netHandleFromFd")(fd);
    } catch {
      __net.closeFd(fd);
      return null;
    }
  }
  writeUtf8String(req, string, handle) {
    return this.writeBuffer(req, Buffer.from(string, "utf8"), handle);
  }
  // LibuvStreamWrap's write: what the socket takes at once is written synchronously, the rest
  // queues in order. A handle's descriptor rides on the first byte that goes out.
  writeBuffer(req, bytes, handle) {
    if (this._closed || this._id === null) return UV_EBADF();
    let fd = -1;
    if (handle) {
      fd = handle.fd;
      if (typeof fd !== "number" || fd < 0) return UV_EBADF();
    }
    this.lastWriteWasAsync = false;
    if (!this._writing && this._queue.length === 0) {
      const n = __net.trySendMsg(this._id, bytes, fd);
      if (n > 0 && handle) handle._sent = true;
      if (n === bytes.length) return 0;
      if (n > 0) {
        bytes = bytes.subarray(n);
        fd = -1;
      }
    }
    if (fd >= 0) {
      fd = __net.dupFd(fd);
      if (fd < 0) return UV_EBADF();
      handle._sent = true;
    }
    this.lastWriteWasAsync = true;
    this._queue.push({ req, bytes, fd });
    this._queuedBytes += bytes.length;
    if (!this._writing) this._writeNext();
    return 0;
  }
  _writeNext() {
    const entry = this._queue[0];
    this._writing = entry !== undefined;
    if (entry === undefined) {
      const waiters = this._drainWaiters;
      this._drainWaiters = [];
      for (const fn of waiters) fn();
      return;
    }
    const done = (status) => {
      if (entry.fd >= 0) __net.closeFd(entry.fd);
      this._queue.shift();
      this._queuedBytes -= entry.bytes.length;
      this._writing = false;
      if (typeof entry.req.oncomplete === "function") entry.req.oncomplete(status);
      if (!this._writing) this._writeNext();
    };
    if (this._id === null) {
      process.nextTick(done, UV_EBADF());
      return;
    }
    __net.writeMsg(this._id, entry.bytes, entry.fd, () => done(0),
      (error) => done(__uvCodes().get(error && error.code) ?? __uvCodes().get("EPIPE")));
  }
  close(callback) {
    if (this._closed) return;
    this._closed = true;
    this._reading = false;
    const finish = () => {
      if (this._id !== null) __net.close(this._id);
      this._id = null;
      for (const fd of this._fds.splice(0)) __net.closeFd(fd);
      if (typeof callback === "function") setImmediate(callback);
    };
    if (this._queue.length !== 0) this._drainWaiters.push(finish);
    else finish();
  }
  ref() { if (this._id !== null) __net.socketRef(this._id, false); }
  unref() { if (this._id !== null) __net.socketRef(this._id, true); }
}

let lazyNet;
let lazyDgram;
const net = () => (lazyNet ??= __builtins.get("net"));
const dgram = () => (lazyDgram ??= __builtins.get("dgram"));
const wrapClass = (binding, name) => __internals.get("netBinding")(binding)[name];

// internal/socket_list: a net.Server's connections that were sent to (or received from) another
// process, so server.getConnections()/close() can ask that process about them.
class SocketListSend extends EventEmitter {
  constructor(child, key) {
    super();
    this.key = key;
    this.child = child;
    child.once("exit", () => this.emit("exit", this));
  }
  _request(msg, cmd, swallowErrors, callback) {
    const self = this;
    if (!this.child.connected) return onclose();
    this.child._send(msg, undefined, swallowErrors);
    function onclose() {
      self.child.removeListener("internalMessage", onreply);
      callback(new __errors.ERR_CHILD_CLOSED_BEFORE_REPLY());
    }
    function onreply(msg) {
      if (!(msg.cmd === cmd && msg.key === self.key)) return;
      self.child.removeListener("disconnect", onclose);
      self.child.removeListener("internalMessage", onreply);
      callback(null, msg);
    }
    this.child.once("disconnect", onclose);
    this.child.on("internalMessage", onreply);
  }
  close(callback) {
    this._request({ cmd: "NODE_SOCKET_NOTIFY_CLOSE", key: this.key }, "NODE_SOCKET_ALL_CLOSED", true, callback);
  }
  getConnections(callback) {
    this._request({ cmd: "NODE_SOCKET_GET_COUNT", key: this.key }, "NODE_SOCKET_COUNT", false, (err, msg) => {
      if (err) return callback(err);
      callback(null, msg.count);
    });
  }
}

class SocketListReceive extends EventEmitter {
  constructor(child, key) {
    super();
    this.connections = 0;
    this.key = key;
    this.child = child;
    function onempty(self) {
      if (!self.child.connected) return;
      self.child._send({ cmd: "NODE_SOCKET_ALL_CLOSED", key: self.key }, undefined, true);
    }
    this.child.on("internalMessage", (msg) => {
      if (msg.key !== this.key) return;
      if (msg.cmd === "NODE_SOCKET_NOTIFY_CLOSE") {
        if (this.connections === 0) return onempty(this);
        this.once("empty", onempty);
      } else if (msg.cmd === "NODE_SOCKET_GET_COUNT") {
        if (!this.child.connected) return;
        this.child._send({ cmd: "NODE_SOCKET_COUNT", key: this.key, count: this.connections });
      }
    });
  }
  add(obj) {
    this.connections++;
    obj.socket.once("close", () => {
      this.connections--;
      if (this.connections === 0) this.emit("empty", this);
    });
  }
}

function getSocketList(type, worker, key) {
  const sockets = worker[kChannelHandle].sockets[type];
  let socketList = sockets[key];
  if (!socketList) {
    const Construct = type === "send" ? SocketListSend : SocketListReceive;
    socketList = sockets[key] = new Construct(worker, key);
  }
  return socketList;
}

// This object keeps track of the sockets that are sent, per handle type.
const handleConversion = {
  "net.Native": {
    simultaneousAccepts: true,
    send(message, handle, options) { return handle; },
    got(message, handle, emit) { emit(handle); },
  },
  "net.Server": {
    simultaneousAccepts: true,
    send(message, server, options) { return server._handle; },
    got(message, handle, emit) {
      const server = new (net().Server)();
      server.listen(handle, () => { emit(server); });
    },
  },
  "net.Socket": {
    send(message, socket, options) {
      if (!socket._handle) return;
      // If the socket was created by net.Server
      if (socket.server) {
        // The worker should keep track of the socket
        message.key = socket.server._connectionKey;
        const firstTime = !this[kChannelHandle].sockets.send[message.key];
        const socketList = getSocketList("send", this, message.key);
        // The server should no longer expose a .connection property and when asked to close
        // it should query the socket status from the workers
        if (firstTime) socket.server._setupWorker(socketList);
        // Act like socket is detached
        if (!options.keepOpen) socket.server._connections--;
      }
      const handle = socket._handle;
      // Remove handle from socket object, it will be closed when the socket will be sent
      if (!options.keepOpen) {
        handle.onread = nop;
        socket._handle = null;
        socket.setTimeout(0);
      }
      return handle;
    },
    postSend(message, handle, options, callback, target) {
      // Store the handle after successfully sending it, so it can be closed when the
      // NODE_HANDLE_ACK is received. If the handle could not be sent, just close it.
      if (handle && !options.keepOpen) {
        if (target) {
          // There can only be one _pendingMessage as passing handles are processed one at a
          // time: handles are stored in _handleQueue while waiting for the NODE_HANDLE_ACK of
          // the current passing handle.
          target._pendingMessage = { callback, message, handle, options, retransmissions: 0 };
        } else {
          handle.close();
        }
      }
    },
    got(message, handle, emit) {
      const socket = new (net().Socket)({ handle, readable: true, writable: true });
      // If the socket was created by net.Server we will track the socket
      if (message.key) {
        const socketList = getSocketList("got", this, message.key);
        socketList.add({ socket });
      }
      emit(socket);
    },
  },
  "dgram.Native": {
    simultaneousAccepts: false,
    send(message, handle, options) { return handle; },
    got(message, handle, emit) { emit(handle); },
  },
  "dgram.Socket": {
    simultaneousAccepts: false,
    send(message, socket, options) {
      message.dgramType = socket.type;
      return socket[__internals.get("netRequire")("internal/dgram").kStateSymbol].handle;
    },
    got(message, handle, emit) {
      const socket = new (dgram().Socket)(message.dgramType);
      socket.bind(handle, () => { emit(socket); });
    },
  },
};

// The message framing, as internal/child_process/serialization frames it.
const channelSerialization = {
  json: {
    initMessageChannel(channel) {
      channel[kJSONBuffer] = "";
      channel[kStringDecoder] = undefined;
    },
    *parseChannelMessages(channel, readData) {
      if (readData.length === 0) return;
      if (channel[kStringDecoder] === undefined) {
        channel[kStringDecoder] = new (__builtins.get("string_decoder").StringDecoder)("utf8");
      }
      const chunks = channel[kStringDecoder].write(readData).split("\n");
      const numCompleteChunks = chunks.length - 1;
      // Last line does not have trailing linebreak
      const incompleteChunk = chunks[numCompleteChunks];
      if (numCompleteChunks === 0) {
        channel[kJSONBuffer] += incompleteChunk;
        return;
      }
      chunks[0] = channel[kJSONBuffer] + chunks[0];
      for (let i = 0; i < numCompleteChunks; i++) yield JSON.parse(chunks[i]);
      channel[kJSONBuffer] = incompleteChunk;
    },
    writeChannelMessage(channel, req, message, handle) {
      const string = JSON.stringify(message) + "\n";
      return channel.writeUtf8String(req, string, handle);
    },
  },
  advanced: {
    initMessageChannel(channel) {
      channel[kMessageBuffer] = [];
      channel[kMessageBufferSize] = 0;
      channel.buffering = false;
    },
    *parseChannelMessages(channel, readData) {
      if (readData.length === 0) return;
      channel[kMessageBuffer].push(readData);
      channel[kMessageBufferSize] += readData.length;
      let messageBufferHead = channel[kMessageBuffer][0];
      while (messageBufferHead.length >= 4) {
        const fullMessageSize = ((messageBufferHead[0] << 24) | (messageBufferHead[1] << 16) |
          (messageBufferHead[2] << 8) | messageBufferHead[3]) + 4;
        if (channel[kMessageBufferSize] < fullMessageSize) break;
        const concatenatedBuffer = channel[kMessageBuffer].length === 1
          ? channel[kMessageBuffer][0]
          : Buffer.concat(channel[kMessageBuffer], channel[kMessageBufferSize]);
        const payload = concatenatedBuffer.subarray(4, fullMessageSize);
        messageBufferHead = concatenatedBuffer.subarray(fullMessageSize);
        channel[kMessageBufferSize] = messageBufferHead.length;
        channel[kMessageBuffer] = channel[kMessageBufferSize] !== 0 ? [messageBufferHead] : [];
        yield __builtins.get("v8").deserialize(payload);
      }
      channel.buffering = channel[kMessageBufferSize] > 0;
    },
    writeChannelMessage(channel, req, message, handle) {
      const payload = __builtins.get("v8").serialize(message);
      const framed = Buffer.allocUnsafe(payload.length + 4);
      framed.writeUInt32BE(payload.length, 0);
      payload.copy(framed, 4);
      return channel.writeBuffer(req, framed, handle);
    },
  },
};

function isInternal(message) {
  return message !== null && typeof message === "object" && typeof message.cmd === "string" &&
    message.cmd.length > 5 && message.cmd.startsWith("NODE_");
}

class Control extends EventEmitter {
  #channel = null;
  #refs = 0;
  #refExplicitlySet = false;
  constructor(channel) {
    super();
    this.#channel = channel;
    this[kPendingMessages] = [];
  }
  // The methods keeping track of the counter are being used to track the listener count on the
  // child process object as well as when writes are in progress. Once the user has explicitly
  // requested a certain state, these methods become no-ops in order to not interfere with the
  // user's intentions.
  refCounted() {
    if (++this.#refs === 1 && !this.#refExplicitlySet) this.#channel.ref();
  }
  unrefCounted() {
    if (--this.#refs === 0 && !this.#refExplicitlySet) {
      this.#channel.unref();
      this.emit("unref");
    }
  }
  ref() {
    this.#refExplicitlySet = true;
    this.#channel.ref();
  }
  unref() {
    this.#refExplicitlySet = true;
    this.#channel.unref();
  }
  get fd() {
    return this.#channel ? this.#channel.fd : undefined;
  }
}

const channelDeprecationMsg = "_channel is deprecated. Use ChildProcess.channel instead.";
let channelDeprecationWarned = false;

function setupChannel(target, channel, serializationMode) {
  const control = new Control(channel);
  target.channel = control;
  target[kChannelHandle] = channel;
  Object.defineProperty(target, "_channel", {
    __proto__: null,
    configurable: true,
    enumerable: false,
    get() {
      if (!channelDeprecationWarned) {
        channelDeprecationWarned = true;
        process.emitWarning(channelDeprecationMsg, "DeprecationWarning", "DEP0129");
      }
      return this.channel;
    },
    set(val) { this.channel = val; },
  });
  target._handleQueue = null;
  target._pendingMessage = null;

  const { initMessageChannel, parseChannelMessages, writeChannelMessage } = channelSerialization[serializationMode];
  initMessageChannel(channel);

  channel.onread = function(pool) {
    if (pool) {
      for (const message of parseChannelMessages(channel, pool)) {
        // There will be at most one NODE_HANDLE message in every chunk we read because SCM_RIGHTS
        // messages don't get coalesced, but the descriptors queue up in order on the channel
        // all the same.
        if (isInternal(message)) {
          if (message.cmd === "NODE_HANDLE") {
            handleMessage(message, channel.takeHandle(), true);
          } else {
            handleMessage(message, undefined, true);
          }
        } else {
          handleMessage(message, undefined, false);
        }
      }
    } else {
      this.buffering = false;
      target.disconnect();
      channel.onread = nop;
      channel.close();
      target._channel = null;
      countChannelClose();
    }
  };

  // Object where socket lists will live
  channel.sockets = { got: {}, send: {} };

  // Handlers will go through this
  target.on("internalMessage", function(message, handle) {
    // Once acknowledged - continue sending handles.
    if (message.cmd === "NODE_HANDLE_ACK") {
      if (target._pendingMessage) closePendingHandle(target);
      if (!target._handleQueue) return;
      const queue = target._handleQueue;
      target._handleQueue = null;
      for (let i = 0; i < queue.length; i++) {
        const args = queue[i];
        target._send(args.message, args.handle, args.options, args.callback);
      }
      // Process a pending disconnect (if any).
      if (!target.connected && target.channel && !target._handleQueue) target._disconnect();
      return;
    }

    if (message.cmd === "NODE_HANDLE_NACK") {
      const pending = target._pendingMessage;
      if (pending) {
        target._pendingMessage = null;
        if (pending.retransmissions++ === MAX_HANDLE_RETRANSMISSIONS) {
          pending.handle.close();
        } else {
          target._handleQueue = null;
          target._send(pending.message, pending.handle, pending.options, pending.callback);
          if (target._pendingMessage) target._pendingMessage.retransmissions = pending.retransmissions;
        }
      }
      return;
    }

    if (message.cmd !== "NODE_HANDLE") return;

    // It is possible that the handle is not received because of some error on ancillary data
    // reception such as MSG_CTRUNC. In this case, report the sender about it by sending a
    // NODE_HANDLE_NACK message.
    if (!handle) return target._send({ cmd: "NODE_HANDLE_NACK" }, null, true);

    // Acknowledge handle receival. Don't emit error events (for example if the other side has
    // disconnected) because this call to send() is not initiated by the user and it shouldn't be
    // fatal to be unable to ACK a message.
    target._send({ cmd: "NODE_HANDLE_ACK" }, null, true);

    const obj = handleConversion[message.type];
    // Convert handle object
    obj.got.call(this, message, handle, (handle) => {
      handleMessage(message.msg, handle, isInternal(message.msg));
    });
  });

  target.on("newListener", function() {
    process.nextTick(() => {
      if (!target.channel || !target.listenerCount("message")) return;
      const ch = target.channel;
      const messages = ch[kPendingMessages];
      const { length } = messages;
      if (!length) return;
      for (let i = 0; i < length; i++) target.emit(...messages[i]);
      ch[kPendingMessages] = [];
    });
  });

  target.send = function(message, handle, options, callback) {
    if (typeof handle === "function") {
      callback = handle;
      handle = undefined;
      options = undefined;
    } else if (typeof options === "function") {
      callback = options;
      options = undefined;
    } else if (options !== undefined) {
      validateObject(options, "options");
    }
    options = { swallowErrors: false, ...options };
    if (this.connected) return this._send(message, handle, options, callback);
    const ex = new ERR_IPC_CHANNEL_CLOSED();
    if (typeof callback === "function") process.nextTick(callback, ex);
    else process.nextTick(() => this.emit("error", ex));
    return false;
  };

  target._send = function(message, handle, options, callback) {
    if (message === undefined) throw new ERR_MISSING_ARGS("message");
    // Non-serializable messages should not reach the remote end point; as any failure in the
    // stringification there will result in error message that is weakly consumable. So perform
    // a final check on message prior to sending.
    if (typeof message !== "string" && typeof message !== "object" && typeof message !== "number" &&
        typeof message !== "boolean") {
      throw new ERR_INVALID_ARG_TYPE("message", ["string", "object", "number", "boolean"], message);
    }
    // Support legacy function signature
    if (typeof options === "boolean") options = { swallowErrors: options };
    else if (options == null) options = { swallowErrors: false };

    let obj;
    // Package messages with a handle object
    if (handle) {
      // This message will be handled by an internalMessage event handler
      message = { cmd: "NODE_HANDLE", type: null, msg: message };
      if (handle instanceof net().Socket) {
        message.type = "net.Socket";
      } else if (handle instanceof net().Server) {
        message.type = "net.Server";
      } else if (handle instanceof wrapClass("tcp_wrap", "TCP") || handle instanceof wrapClass("pipe_wrap", "Pipe")) {
        message.type = "net.Native";
      } else if (handle instanceof dgram().Socket) {
        message.type = "dgram.Socket";
      } else if (handle instanceof wrapClass("udp_wrap", "UDP")) {
        message.type = "dgram.Native";
      } else {
        throw new __errors.ERR_INVALID_HANDLE_TYPE();
      }
      // Queue-up message and handle if we haven't received ACK yet.
      if (this._handleQueue) {
        this._handleQueue.push({ callback, handle, options, message: message.msg });
        return this._handleQueue.length === 1;
      }
      obj = handleConversion[message.type];
      // convert TCP object to native handle object
      handle = obj.send.call(target, message, handle, options);
      // If handle was sent twice, or it is impossible to get native handle out of it - just
      // send a text without the handle.
      if (!handle) message = message.msg;
    } else if (this._handleQueue && !(message && (message.cmd === "NODE_HANDLE_ACK" || message.cmd === "NODE_HANDLE_NACK"))) {
      // Queue request anyway to avoid out-of-order messages.
      this._handleQueue.push({ callback, handle: null, options, message });
      return this._handleQueue.length === 1;
    }

    const req = {};
    const err = writeChannelMessage(channel, req, message, handle);
    const wasAsyncWrite = channel.lastWriteWasAsync;
    if (err === 0) {
      if (handle) {
        if (!this._handleQueue) this._handleQueue = [];
        if (obj && obj.postSend) obj.postSend(message, handle, options, callback, target);
      }
      if (wasAsyncWrite) {
        req.oncomplete = () => {
          control.unrefCounted();
          if (typeof callback === "function") callback(null);
        };
        control.refCounted();
      } else if (typeof callback === "function") {
        process.nextTick(callback, null);
      }
    } else {
      // Cleanup handle on error
      if (obj && obj.postSend) obj.postSend(message, handle, options, callback);
      if (!options.swallowErrors) {
        const ex = __builtins.get("util")._errnoException(err, "write");
        if (typeof callback === "function") process.nextTick(callback, ex);
        else process.nextTick(() => this.emit("error", ex));
      }
    }
    // If the primary is > 2 read() calls behind, please stop sending.
    return channel.writeQueueSize < (65536 * 2);
  };

  // Connected will be set to false immediately when a disconnect() is requested, even though
  // the channel might still be alive internally to process queued messages.
  target.connected = true;

  let channelCounted = false;
  function countChannelClose() {
    if (channelCounted || typeof target._closesNeeded !== "number") return;
    channelCounted = true;
    maybeClose(target);
  }

  target.disconnect = function() {
    if (!this.connected) {
      this.emit("error", new ERR_IPC_DISCONNECTED());
      return;
    }
    // Do not allow any new messages to be written.
    this.connected = false;
    // If there are no queued messages, disconnect immediately. Otherwise, postpone the
    // disconnect so that it happens internally after the queue is flushed.
    if (!this._handleQueue) this._disconnect();
  };

  target._disconnect = function() {
    // This marks the fact that the channel is actually disconnected.
    this.channel = null;
    this[kChannelHandle] = null;
    if (this._pendingMessage) closePendingHandle(this);
    let fired = false;
    function finish() {
      if (fired) return;
      fired = true;
      channel.close();
      target.emit("disconnect");
      countChannelClose();
    }
    // If a message is being read, then wait for it to complete.
    if (channel.buffering) {
      this.once("message", finish);
      this.once("internalMessage", finish);
      return;
    }
    process.nextTick(finish);
  };

  function emit(event, message, handle) {
    if (event === "internalMessage" || target.listenerCount("message")) {
      target.emit(event, message, handle);
      return;
    }
    target.channel[kPendingMessages].push([event, message, handle]);
  }

  function handleMessage(message, handle, internal) {
    if (!target.channel) return;
    const eventName = internal ? "internalMessage" : "message";
    process.nextTick(emit, eventName, message, handle);
  }

  channel.readStart();
  return control;
}

function closePendingHandle(target) {
  target._pendingMessage.handle.close();
  target._pendingMessage = null;
}

// The child's end of the channel its parent opened (NODE_CHANNEL_FD).
function _forkChild(fd, serializationMode) {
  const id = __net.adoptFd(fd).desc[0];
  const p = new IpcPipe(id);
  p.unref();
  for (const name of ["send", "connected", "disconnect", "channel"]) delete process[name];
  const control = setupChannel(process, p, serializationMode);
  process.on("newListener", function onNewListener(name) {
    if (name === "message" || name === "disconnect") control.refCounted();
  });
  process.on("removeListener", function onRemoveListener(name) {
    if (name === "message" || name === "disconnect") control.unrefCounted();
  });
}

// Pump a relayed standard slot: the parent forwards the pipe to (or from) the stream or fd the
// caller named, so the data still reaches it instead of a dead pipe.
function relayPipe(i, pipe, target) {
  const fs = __builtins.get("fs");
  if (i === 0) {
    if (typeof target === "number") fs.createReadStream(null, { fd: target, autoClose: false }).pipe(pipe);
    else if (target && typeof target.pipe === "function" && target.readable !== false) target.pipe(pipe);
    else pipe.end();
    return;
  }
  if (typeof target === "number") pipe.on("data", (chunk) => fs.writeSync(target, chunk));
  else if (target && typeof target.write === "function") pipe.pipe(target, { end: false });
  else pipe.resume();
}

function abortChildProcess(child, killSignal, reason) {
  try {
    if (child.kill(killSignal)) {
      const { AbortError } = __builtins.get("events");
      const error = AbortError
        ? new AbortError(undefined, { cause: reason })
        : Object.assign(new Error("The operation was aborted", { cause: reason }), { name: "AbortError", code: "ABORT_ERR" });
      child.emit("error", error);
    }
  } catch (err) {
    child.emit("error", err);
  }
}

function spawn(file, args, options) {
  options = normalizeSpawnArguments(file, args, options);
  validateOneOf(options.serialization, "options.serialization", [undefined, "json", "advanced"]);
  validateTimeout(options.timeout);
  validateAbortSignal(options.signal, "options.signal");
  const killSignal = sanitizeKillSignal(options.killSignal);
  const child = new ChildProcess();
  child.spawn(options);
  const channel = __builtins.get("diagnostics_channel").channel("child_process");
  if (channel.hasSubscribers) channel.publish({ process: child });
  if (options.timeout > 0) {
    let timeoutId = setTimeout(() => {
      if (timeoutId) {
        try {
          child.kill(killSignal);
        } catch (err) {
          child.emit("error", err);
        }
        timeoutId = null;
      }
    }, options.timeout);
    child.once("exit", () => {
      if (timeoutId) {
        clearTimeout(timeoutId);
        timeoutId = null;
      }
    });
  }
  if (options.signal) {
    const signal = options.signal;
    const onAbort = () => abortChildProcess(child, killSignal, signal.reason);
    if (signal.aborted) {
      process.nextTick(onAbort);
    } else {
      signal.addEventListener("abort", onAbort, { once: true });
      child.once("exit", () => signal.removeEventListener("abort", onAbort));
    }
  }
  return child;
}

function normalizeExecArgs(command, options, callback) {
  validateString(command, "command");
  validateArgumentNullCheck(command, "command");
  if (typeof options === "function") {
    callback = options;
    options = undefined;
  }
  options = { ...options };
  options.shell = typeof options.shell === "string" ? options.shell : true;
  return { file: command, options, callback };
}

function normalizeExecFileArgs(file, args, options, callback) {
  if (Array.isArray(args)) {
    args = args.slice();
  } else if (args != null && typeof args === "object") {
    callback = options;
    options = args;
    args = null;
  } else if (typeof args === "function") {
    callback = args;
    options = null;
    args = null;
  }
  if (args == null) args = [];
  if (typeof options === "function") {
    callback = options;
    options = null;
  } else if (options != null) {
    validateObject(options, "options");
  }
  if (options == null) options = {};
  if (callback != null) validateFunction(callback, "callback");
  if (options.argv0 != null) validateString(options.argv0, "options.argv0");
  return { file, args, options, callback };
}

function exec(command, options, callback) {
  const opts = normalizeExecArgs(command, options, callback);
  return __childProcessExports.execFile(opts.file, opts.options, opts.callback);
}

function execFile(file, args, options, callback) {
  ({ file, args, options, callback } = normalizeExecFileArgs(file, args, options, callback));
  options = {
    encoding: "utf8",
    timeout: 0,
    maxBuffer: MAX_BUFFER,
    killSignal: "SIGTERM",
    cwd: null,
    env: null,
    shell: false,
    ...options,
  };
  validateTimeout(options.timeout);
  validateMaxBuffer(options.maxBuffer);
  options.killSignal = sanitizeKillSignal(options.killSignal);
  const child = spawn(file, args, {
    cwd: options.cwd,
    env: options.env,
    gid: options.gid,
    shell: options.shell,
    signal: options.signal,
    uid: options.uid,
    windowsHide: !!options.windowsHide,
    windowsVerbatimArguments: !!options.windowsVerbatimArguments,
  });
  let encoding;
  const _stdout = [];
  const _stderr = [];
  if (options.encoding !== "buffer" && Buffer.isEncoding(options.encoding)) encoding = options.encoding;
  else encoding = null;
  let stdoutLen = 0;
  let stderrLen = 0;
  let killed = false;
  let exited = false;
  let timeoutId;
  let ex = null;
  let cmd = file;

  function exithandler(code, signal) {
    if (exited) return;
    exited = true;
    if (timeoutId) {
      clearTimeout(timeoutId);
      timeoutId = null;
    }
    if (!callback) return;
    let stdout;
    let stderr;
    if (encoding || (child.stdout && child.stdout.readableEncoding)) stdout = _stdout.join("");
    else stdout = Buffer.concat(_stdout);
    if (encoding || (child.stderr && child.stderr.readableEncoding)) stderr = _stderr.join("");
    else stderr = Buffer.concat(_stderr);
    if (!ex && code === 0 && signal === null) {
      callback(null, stdout, stderr);
      return;
    }
    if (args?.length) cmd += ` ${args.join(" ")}`;
    if (!ex) {
      ex = new Error(`Command failed: ${cmd}\n${stderr}`);
      ex.code = code < 0 ? __builtins.get("util").getSystemErrorName(code) : code;
      ex.killed = child.killed || killed;
      ex.signal = signal;
    }
    ex.cmd = cmd;
    callback(ex, stdout, stderr);
  }

  function errorhandler(e) {
    ex = e;
    if (child.stdout) child.stdout.destroy();
    if (child.stderr) child.stderr.destroy();
    exithandler();
  }

  function kill() {
    if (child.stdout) child.stdout.destroy();
    if (child.stderr) child.stderr.destroy();
    killed = true;
    try {
      child.kill(options.killSignal);
    } catch (e) {
      ex = e;
      exithandler();
    }
  }

  if (options.timeout > 0) {
    timeoutId = setTimeout(function delayedKill() {
      kill();
      timeoutId = null;
    }, options.timeout);
  }

  const collect = (stream, chunks, name, count) => {
    if (encoding) stream.setEncoding(encoding);
    stream.on("data", function onChildOutput(chunk) {
      if (options.maxBuffer === Infinity) {
        chunks.push(chunk);
        return;
      }
      const enc = stream.readableEncoding;
      const length = enc ? Buffer.byteLength(chunk, enc) : chunk.length;
      const total = count.n += length;
      if (total > options.maxBuffer) {
        const truncatedLen = options.maxBuffer - (total - length);
        chunks.push(enc ? chunk.slice(0, truncatedLen) : chunk.subarray(0, truncatedLen));
        ex = new ERR_CHILD_PROCESS_STDIO_MAXBUFFER(name);
        kill();
      } else {
        chunks.push(chunk);
      }
    });
  };
  const outCount = { get n() { return stdoutLen; }, set n(v) { stdoutLen = v; } };
  const errCount = { get n() { return stderrLen; }, set n(v) { stderrLen = v; } };
  if (child.stdout) collect(child.stdout, _stdout, "stdout", outCount);
  if (child.stderr) collect(child.stderr, _stderr, "stderr", errCount);
  child.addListener("close", exithandler);
  child.addListener("error", errorhandler);
  return child;
}

// ---- synchronous variants ---------------------------------------------------------------------

const internalChildProcess = {
  ChildProcess,
  kChannelHandle,
  setupChannel,
  getValidStdio,
  stdioStringToArray,
  spawnSync: spawnSyncImpl,
};

function spawnSync(file, args, options) {
  options = { maxBuffer: MAX_BUFFER, ...normalizeSpawnArguments(file, args, options) };
  validateTimeout(options.timeout);
  validateMaxBuffer(options.maxBuffer);
  options.killSignal = sanitizeKillSignal(options.killSignal);
  return internalChildProcess.spawnSync(options);
}

function spawnSyncImpl(options) {
  const { stdio } = getValidStdio(options.stdio || "pipe", true);
  let input = options.input;
  if (input) {
    if (ArrayBuffer.isView(input)) input = Buffer.from(input.buffer, input.byteOffset, input.byteLength);
    else if (typeof input === "string") input = Buffer.from(input, options.encoding);
    else throw new ERR_INVALID_ARG_TYPE("options.stdio[0]", ["Buffer", "TypedArray", "DataView", "string"], input);
  } else {
    input = null;
  }
  const modes = stdio.map((s) => (s.type === "pipe" || s.type === "inherit" ? s.type : "ignore"));
  const argv = options.args.slice(1);
  const argv0 = String(options.args[0]) !== options.file ? String(options.args[0]) : undefined;
  const cwd = options.cwd === "" ? undefined : options.cwd;
  const killSignal = options.killSignal === undefined ? SIGNALS.SIGTERM : options.killSignal;
  let res;
  try {
    res = __child.execSync(options.file, argv, modes[0] === "pipe" ? input : null, cwd, nativeEnv(options.envPairs),
      options.windowsVerbatimArguments, options.timeout || undefined,
      { stdio: modes, argv0, uid: options.uid, gid: options.gid, killSignal, maxBuffer: options.maxBuffer, detached: options.detached });
  } catch (e) {
    const error = spawnError(e, "spawnSync", options.file, argv);
    if (!error) throw e;
    return { error, status: null, signal: null, output: null, pid: 0, stdout: null, stderr: null };
  }
  const text = (bytes) => {
    if (bytes === null) return null;
    const buffer = Buffer.from(bytes);
    return options.encoding && options.encoding !== "buffer" ? buffer.toString(options.encoding) : buffer;
  };
  const stdout = text(res.stdout);
  const stderr = text(res.stderr);
  const result = {
    status: res.status,
    signal: res.signal === null ? null : SIGNAL_NAMES[res.signal] ?? `SIG${res.signal}`,
    output: [null, stdout, stderr],
    pid: res.pid,
    stdout,
    stderr,
  };
  const failure = res.timedOut ? "ETIMEDOUT" : res.maxBufferExceeded ? "ENOBUFS" : null;
  if (failure) {
    result.error = errnoError(__uvCodes().get(failure), "spawnSync", options.file, argv);
    if (result.signal === null && result.status === null) result.signal = SIGNAL_NAMES[killSignal] ?? "SIGTERM";
  }
  return result;
}

function checkExecSyncError(ret, args, cmd) {
  let err;
  if (ret.error) {
    err = ret.error;
    Object.assign(err, ret);
  } else if (ret.status !== 0) {
    let msg = "Command failed: ";
    msg += cmd || args.join(" ");
    if (ret.stderr && ret.stderr.length > 0) msg += `\n${ret.stderr.toString()}`;
    err = new Error(msg);
    Object.assign(err, ret);
  }
  return err;
}

function execFileSync(command, args, options) {
  const opts = normalizeExecFileArgs(command, args, options);
  const inheritStderr = !opts.options.stdio;
  const ret = spawnSync(opts.file, opts.args, opts.options);
  if (inheritStderr && ret.stderr) process.stderr.write(ret.stderr);
  const err = checkExecSyncError(ret, [opts.options.argv0 || command, ...opts.args], undefined);
  if (err) throw err;
  return ret.stdout;
}

function execSync(command, options) {
  const opts = normalizeExecArgs(command, options, null);
  const inheritStderr = !opts.options.stdio;
  const ret = spawnSync(opts.file, opts.options);
  if (inheritStderr && ret.stderr) process.stderr.write(ret.stderr);
  const err = checkExecSyncError(ret, undefined, command);
  if (err) throw err;
  return ret.stdout;
}

function fork(modulePath, args, options) {
  // getValidatedPath: a string, Buffer or file: URL without null bytes.
  if (modulePath !== null && typeof modulePath === "object" && typeof modulePath.href === "string" && typeof modulePath.protocol === "string") {
    modulePath = __builtins.get("url").fileURLToPath(modulePath);
  } else if (modulePath instanceof Uint8Array) {
    modulePath = Buffer.from(modulePath).toString();
  } else if (typeof modulePath !== "string") {
    throw new ERR_INVALID_ARG_TYPE("modulePath", ["string", "Buffer", "URL"], modulePath);
  }
  if (modulePath.includes("\u0000")) {
    throw new ERR_INVALID_ARG_VALUE("modulePath", modulePath, "must be a string, Uint8Array, or URL without null bytes");
  }
  let pos = 1;
  if (pos < arguments.length && Array.isArray(arguments[pos])) {
    args = arguments[pos++];
  } else if (pos < arguments.length && arguments[pos] == null) {
    pos++;
    args = [];
  } else {
    args = [];
  }
  if (pos < arguments.length && arguments[pos] != null) {
    validateObject(arguments[pos], "options");
    options = { ...arguments[pos] };
  } else {
    options = {};
  }
  options.shell = false;
  const execArgv = options.execArgv || process.execArgv;
  validateArgumentsNullCheck(execArgv, "options.execArgv");
  args = [...execArgv, modulePath, ...args];
  if (typeof options.stdio === "string") {
    options.stdio = stdioStringToArray(options.stdio, "ipc");
  } else if (!Array.isArray(options.stdio)) {
    options.stdio = stdioStringToArray(options.silent ? "pipe" : "inherit", "ipc");
  } else if (!options.stdio.includes("ipc")) {
    throw new ERR_CHILD_PROCESS_IPC_REQUIRED("options.stdio");
  }
  options.execPath = options.execPath || process.execPath;
  return spawn(options.execPath, args, options);
}

const customPromiseExec = (orig) => (...args) => {
  let resolve;
  let reject;
  const promise = new Promise((res, rej) => {
    resolve = res;
    reject = rej;
  });
  promise.child = orig(...args, (err, stdout, stderr) => {
    if (err !== null) {
      err.stdout = stdout;
      err.stderr = stderr;
      reject(err);
    } else {
      resolve({ stdout, stderr });
    }
  });
  return promise;
};
const kCustomPromisify = Symbol.for("nodejs.util.promisify.custom");
Object.defineProperty(exec, kCustomPromisify, { enumerable: false, value: customPromiseExec(exec) });
Object.defineProperty(execFile, kCustomPromisify, { enumerable: false, value: customPromiseExec(execFile) });

const __childProcessExports = {
  _forkChild,
  ChildProcess,
  exec,
  execFile,
  execFileSync,
  execSync,
  fork,
  spawn,
  spawnSync,
};
Object.defineProperty(__childProcessExports, "_ipcWire", { value: __ipcWire, configurable: true });
__builtins.set("child_process", __childProcessExports);
// require('internal/child_process') under --expose-internals.
__builtins.set("internal/child_process", internalChildProcess);
