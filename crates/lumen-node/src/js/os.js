// node:os over the __os native op. Facts are snapshotted at first access, like Node's.

const __osInfo = __os.info();
const EOL = __osInfo.platform === "win32" ? "\r\n" : "\n";

// libuv errno descriptions for the codes getpriority/setpriority can raise.
const __ERRNO_DESC = { EPERM: "operation not permitted", ESRCH: "no such process", EACCES: "permission denied", EINVAL: "invalid argument", UNKNOWN: "unknown error" };
function systemError(syscall, info) {
  const code = info.code || "UNKNOWN";
  const desc = __ERRNO_DESC[code] || "unknown error";
  const err = new Error(`A system error occurred: ${syscall} returned ${code} (${desc})`);
  err.code = "ERR_SYSTEM_ERROR";
  err.errno = info.errno;
  Object.defineProperty(err, "name", { value: "SystemError", writable: true, configurable: true, enumerable: false });
  err.syscall = syscall;
  err.info = { errno: info.errno, code, message: desc, syscall };
  return err;
}
function validatePid(pid, name) {
  if (typeof pid !== "number") {
    const e = new TypeError(`The "${name}" argument must be of type number. Received ${pid === null ? "null" : typeof pid === "object" ? "an instance of " + (pid.constructor?.name ?? "Object") : `type ${typeof pid} (${String(pid)})`}`);
    e.code = "ERR_INVALID_ARG_TYPE";
    throw e;
  }
  if (!Number.isInteger(pid) || pid < -2147483648 || pid > 2147483647) {
    const e = new RangeError(`The value of "${name}" is out of range. It must be an integer >= -2147483648 && <= 2147483647. Received ${pid}`);
    e.code = "ERR_OUT_OF_RANGE";
    throw e;
  }
  return pid;
}

const os = {
  platform: () => __osInfo.platform,
  arch: () => __osInfo.arch,
  type: () => __osInfo.type,
  release: () => __osInfo.release,
  version: () => __osInfo.version,
  homedir: () => __osInfo.homedir,
  tmpdir: () => {
    const env = process.env;
    let path = env.TMPDIR || env.TMP || env.TEMP || (__osInfo.platform === "win32" ? "" : "/tmp");
    if (path.length > 1 && path.endsWith("/")) path = path.slice(0, -1);
    return path;
  },
  hostname: () => __os.hostname(),
  endianness: () => __osInfo.endianness,
  // cpus(): count, model and speed are real (one model for every core); per-core times aren't
  // reachable from std and read as zero.
  cpus: () =>
    Array.from({ length: __osInfo.cpus }, () => ({
      model: __osInfo.cpuModel || "unknown",
      speed: __osInfo.cpuSpeed,
      times: { user: 0, nice: 0, sys: 0, idle: 0, irq: 0 },
    })),
  availableParallelism: () => __osInfo.cpus,
  // uname -m spelling of the arch (Node reports "arm64" on darwin but "aarch64" on linux).
  machine: () =>
    __osInfo.arch === "x64" ? "x86_64"
    : __osInfo.arch === "arm64" ? (__osInfo.platform === "darwin" ? "arm64" : "aarch64")
    : __osInfo.arch === "ia32" ? "i686"
    : __osInfo.arch,
  // Enumerating real interfaces needs getifaddrs(), which std doesn't expose; loopback is the one
  // interface every host has, so report just it (correct, if incomplete) rather than {}.
  networkInterfaces: () => ({
    [__osInfo.platform === "darwin" ? "lo0" : "lo"]: [
      { address: "127.0.0.1", netmask: "255.0.0.0", family: "IPv4", mac: "00:00:00:00:00:00", internal: true, cidr: "127.0.0.1/8" },
      { address: "::1", netmask: "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff", family: "IPv6", mac: "00:00:00:00:00:00", internal: true, cidr: "::1/128", scopeid: 0 },
    ],
  }),
  // getPriority/setPriority over getpriority(2)/setpriority(2) (see __os in lib.rs). The native op
  // returns the value/undefined on success, or { errno, code } on failure, which we wrap in Node's
  // ERR_SYSTEM_ERROR exactly as libuv does.
  getPriority: (pid = 0) => {
    const r = __os.getPriority(validatePid(pid, "pid"));
    if (r && typeof r === "object") throw systemError("uv_os_getpriority", r);
    return r;
  },
  setPriority: (...args) => {
    // Node: setPriority(priority) or setPriority(pid, priority).
    let pid = 0, priority;
    if (args.length >= 2) { pid = validatePid(args[0], "pid"); priority = args[1]; }
    else priority = args[0];
    if (typeof priority !== "number") {
      const e = new TypeError(`The "priority" argument must be of type number. Received ${priority === null ? "null" : typeof priority === "object" ? "an instance of " + (priority.constructor?.name ?? "Object") : `type ${typeof priority} (${String(priority)})`}`);
      e.code = "ERR_INVALID_ARG_TYPE";
      throw e;
    }
    if (!Number.isInteger(priority) || priority < -20 || priority > 19) {
      const e = new RangeError(`The value of "priority" is out of range. It must be >= -20 && <= 19. Received ${priority}`);
      e.code = "ERR_OUT_OF_RANGE";
      throw e;
    }
    const r = __os.setPriority(pid, priority);
    if (r && typeof r === "object") throw systemError("uv_os_setpriority", r);
  },
  totalmem: () => __osInfo.totalmem,
  freemem: () => __os.sysinfo().freemem,
  uptime: () => __os.sysinfo().uptime,
  loadavg: () => { const i = __os.sysinfo(); return [i.load1, i.load5, i.load15]; },
  userInfo: (options) => {
    const encoding = options == null ? undefined : options.encoding;
    const { uid, gid, username, shell, homedir } = __os.sysinfo();
    if (encoding === "buffer") {
      return { uid, gid, username: Buffer.from(username), homedir: Buffer.from(homedir), shell: shell === null ? null : Buffer.from(shell) };
    }
    return { uid, gid, username, homedir, shell };
  },
  devNull: __osInfo.platform === "win32" ? "\\\\.\\nul" : "/dev/null",
};

Object.defineProperty(os, "EOL", { get: () => EOL, enumerable: true, configurable: true });

for (const name of ["arch", "availableParallelism", "endianness", "freemem", "homedir", "hostname", "platform", "release", "tmpdir", "totalmem", "type", "version", "machine", "uptime"]) {
  const fn = os[name];
  fn[Symbol.toPrimitive] = () => fn();
}

const part = (data) => Object.freeze(Object.assign({ __proto__: null }, data));
const osConstants = Object.freeze({
  __proto__: null,
  UV_UDP_REUSEADDR: 4,
  dlopen: part({ RTLD_LAZY: 1, RTLD_NOW: 2, RTLD_GLOBAL: 8, RTLD_LOCAL: 4 }),
  errno: part({ E2BIG: 7, EACCES: 13, EADDRINUSE: 48, EADDRNOTAVAIL: 49, EAFNOSUPPORT: 47,
  EAGAIN: 35, EALREADY: 37, EBADF: 9, EBADMSG: 94, EBUSY: 16, ECANCELED: 89,
  ECHILD: 10, ECONNABORTED: 53, ECONNREFUSED: 61, ECONNRESET: 54, EDEADLK: 11,
  EDESTADDRREQ: 39, EDOM: 33, EDQUOT: 69, EEXIST: 17, EFAULT: 14, EFBIG: 27,
  EHOSTUNREACH: 65, EIDRM: 90, EILSEQ: 92, EINPROGRESS: 36, EINTR: 4, EINVAL: 22,
  EIO: 5, EISCONN: 56, EISDIR: 21, ELOOP: 62, EMFILE: 24, EMLINK: 31, EMSGSIZE: 40,
  EMULTIHOP: 95, ENAMETOOLONG: 63, ENETDOWN: 50, ENETRESET: 52, ENETUNREACH: 51,
  ENFILE: 23, ENOBUFS: 55, ENODATA: 96, ENODEV: 19, ENOENT: 2, ENOEXEC: 8,
  ENOLCK: 77, ENOLINK: 97, ENOMEM: 12, ENOMSG: 91, ENOPROTOOPT: 42, ENOSPC: 28,
  ENOSR: 98, ENOSTR: 99, ENOSYS: 78, ENOTCONN: 57, ENOTDIR: 20, ENOTEMPTY: 66,
  ENOTSOCK: 38, ENOTSUP: 45, ENOTTY: 25, ENXIO: 6, EOPNOTSUPP: 102, EOVERFLOW: 84,
  EPERM: 1, EPIPE: 32, EPROTO: 100, EPROTONOSUPPORT: 43, EPROTOTYPE: 41, ERANGE: 34,
  EROFS: 30, ESPIPE: 29, ESRCH: 3, ESTALE: 70, ETIME: 101, ETIMEDOUT: 60,
  ETXTBSY: 26, EWOULDBLOCK: 35, EXDEV: 18 }),
  signals: part({ SIGHUP: 1, SIGINT: 2, SIGQUIT: 3, SIGILL: 4, SIGTRAP: 5, SIGABRT: 6, SIGIOT: 6,
  SIGBUS: 10, SIGFPE: 8, SIGKILL: 9, SIGUSR1: 30, SIGSEGV: 11, SIGUSR2: 31,
  SIGPIPE: 13, SIGALRM: 14, SIGTERM: 15, SIGCHLD: 20, SIGCONT: 19, SIGSTOP: 17,
  SIGTSTP: 18, SIGTTIN: 21, SIGTTOU: 22, SIGURG: 16, SIGXCPU: 24, SIGXFSZ: 25,
  SIGVTALRM: 26, SIGPROF: 27, SIGWINCH: 28, SIGIO: 23, SIGINFO: 29, SIGSYS: 12 }),
  priority: part({ PRIORITY_LOW: 19, PRIORITY_BELOW_NORMAL: 10, PRIORITY_NORMAL: 0,
  PRIORITY_ABOVE_NORMAL: -7, PRIORITY_HIGH: -14, PRIORITY_HIGHEST: -20 }),
});
Object.defineProperty(os, "constants", { value: osConstants, enumerable: true, configurable: true, writable: true });

__builtins.set("os", os);
