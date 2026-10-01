(() => {
  const proc = globalThis.__proc;
  const readStdin = proc.readStdin;
  const stdinRef = proc.stdinRef;
  const rawExecve = proc.execve;
  const metrics = proc.metrics;
  delete globalThis.__proc;
  const process = globalThis.process;

  // Node's `write(chunk[, encoding][, callback])` / `end([chunk][, encoding][, callback])`: the
  // raw op writes synchronously, so the callback runs on the next tick with the write done.
  const writeArgs = (chunk, encoding, callback) => {
    if (typeof encoding === "function") { callback = encoding; encoding = undefined; }
    if (typeof chunk === "function") { callback = chunk; chunk = undefined; }
    return [chunk, callback];
  };
  const makeStream = (writeFn, fd) => ({
    write(chunk, encoding, callback) {
      const [data, cb] = writeArgs(chunk, encoding, callback);
      if (data != null) writeFn(data);
      if (cb) queueMicrotask(() => cb());
      return true;
    },
    end(chunk, encoding, callback) {
      const [data, cb] = writeArgs(chunk, encoding, callback);
      if (data != null) writeFn(data);
      if (cb) queueMicrotask(() => cb());
      return this;
    },
    isTTY: false,
    fd,
    columns: 80,
    rows: 24,
    // morgan/debug attach listeners; accept and ignore them (nothing emits).
    on() { return this; },
    once() { return this; },
    removeListener() { return this; },
    cork() {},
    uncork() {},
  });
  Object.defineProperty(process, "stdout", { value: makeStream(proc.writeStdout, 1), enumerable: true, configurable: true });
  Object.defineProperty(process, "stderr", { value: makeStream(proc.writeStderr, 2), enumerable: true, configurable: true });
  Object.defineProperty(process, "_readStdin", { value: readStdin, configurable: true });
  Object.defineProperty(process, "_stdinRef", { value: stdinRef, configurable: true });
  Object.defineProperty(process, "_nativeMetrics", { value: metrics, configurable: true });
  Object.defineProperty(process, "_isatty", { value: proc.isatty, configurable: true });
  Object.defineProperty(process, "_ttySize", { value: proc.ttySize, configurable: true });

  const raw = proc.hrtime;
  const hrtime = (prev) => {
    const t = raw();
    if (prev) {
      let s = t[0] - prev[0], n = t[1] - prev[1];
      if (n < 0) { s -= 1; n += 1e9; }
      return [s, n];
    }
    return t;
  };
  hrtime.bigint = () => { const t = raw(); return BigInt(t[0]) * 1000000000n + BigInt(t[1]); };
  process.hrtime = hrtime;

  // Seconds (fractional) since process start, from the same monotonic clock hrtime uses.
  process.uptime = () => { const t = raw(); return t[0] + t[1] / 1e9; };

  process.version = "v20.11.0";
  // The component versions Node 20.11.0 reports. Packages and Node's own test/common probe
  // these (`hasCrypto` is `Boolean(process.versions.openssl)`); lumen implements the matching
  // surfaces (node:crypto, Intl, zlib, N-API 9) natively, so it reports the versions it is
  // compatible with rather than leaving them empty.
  process.versions = {
    node: "20.11.0", lumen: "0.1.1", acorn: "8.11.2", ada: "2.7.4", ares: "1.20.1",
    base64: "0.5.1", brotli: "1.0.9", cjs_module_lexer: "1.2.2", cldr: "44.0", icu: "74.1",
    llhttp: "8.1.1", modules: "115", napi: "9", nghttp2: "1.58.0", openssl: "3.0.12+quic",
    simdutf: "4.0.4", tz: "2023c", undici: "5.27.2", unicode: "15.1", uv: "1.46.0",
    uvwasi: "0.0.19", v8: "11.3.244.8-node.17", zlib: "1.2.13.1-motley",
  };

  // Real OS-identity / control surface over the native ops. These need the (about-to-be-deleted)
  // `__proc` namespace, so they are wired here rather than in the lumen-node JS glue.
  process.chdir = proc.chdir;
  process.abort = proc.abort;
  process.umask = proc.umask;
  process.ppid = proc.getppid();
  process.execve = (file, args, env) => {
    if (typeof file !== "string") throw new TypeError('The "file" argument must be of type string');
    if (!Array.isArray(args)) throw new TypeError('The "args" argument must be an Array');
    if (env === null || typeof env !== "object" || Array.isArray(env)) throw new TypeError('The "env" argument must be an object');
    const argv = args.map(value => String(value));
    const entries = Object.keys(env).filter(key => env[key] !== undefined).map(key => `${key}=${String(env[key])}`);
    if ([file, ...argv, ...entries].some(value => value.includes("\0"))) throw new TypeError("execve arguments may not contain null bytes");
    return rawExecve(file, argv.join("\0"), entries.join("\0"));
  };
  // getuid/getgid/geteuid/getegid are POSIX-only; the ops return undefined off unix (where Node
  // omits these entirely). We keep them defined but honest — `undefined` when the OS can't answer.
  const uid = proc.getuid(), gid = proc.getgid();
  if (uid !== undefined) {
    process.getuid = proc.getuid;
    process.geteuid = proc.geteuid;
    process.getgid = proc.getgid;
    process.getegid = proc.getegid;
    process.setuid = proc.setuid;
    process.seteuid = proc.seteuid;
    process.setgid = proc.setgid;
    process.setegid = proc.setegid;
    process.getgroups = proc.getgroups;
    process.setgroups = groups => {
      if (!Array.isArray(groups)) throw new TypeError('The "groups" argument must be an Array');
      return proc.setgroups(groups.map(group => Number(group)).join(","));
    };
    process.initgroups = (user, extraGroup) => proc.initgroups(String(user), Number(extraGroup));
  }
  // Portable signal numbers (identical on Linux/macOS); named signals outside this set fall back
  // to SIGTERM's number so `process.kill(pid)` still delivers a terminating signal.
  const SIGNALS = { SIGHUP: 1, SIGINT: 2, SIGQUIT: 3, SIGILL: 4, SIGABRT: 6, SIGFPE: 8,
    SIGKILL: 9, SIGSEGV: 11, SIGPIPE: 13, SIGALRM: 14, SIGTERM: 15 };
  const rawKill = proc.kill;
  process.kill = (pid, sig = "SIGTERM") => {
    const n = typeof sig === "number" ? sig : (SIGNALS[sig] ?? 15);
    rawKill(pid | 0, n);
    return true;
  };
})();
