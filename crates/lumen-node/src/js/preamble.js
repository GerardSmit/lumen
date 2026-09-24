// Capture the raw op namespaces; everything below closes over these. Runs in one IIFE.
"use strict";
const __node = globalThis.__node;
const __os = globalThis.__os;
const __zlib = globalThis.__zlib;
const __bunhash = globalThis.__bunhash;
const __child = globalThis.__child;
const __ffi = globalThis.__ffi;
const __crypto = globalThis.__crypto;
const __password = globalThis.__password;
// Macro-bound native ops (src/native.rs): digests, Buffer codecs, bufferutil, async fs.
const __native = __node.native();
delete globalThis.__node;
delete globalThis.__os;
delete globalThis.__zlib;
delete globalThis.__bunhash;
delete globalThis.__child;
delete globalThis.__ffi;
delete globalThis.__crypto;
delete globalThis.__password;
// The runtime installs process.platform/arch after the glue runs, but the glue ported from Node
// reads them while loading (`const isWindows = process.platform === 'win32'`).
if (typeof process === "object" && process !== null && process.platform === undefined) {
  const info = __os.info();
  process.platform = info.platform;
  process.arch = info.arch;
}

// Async context: an opaque value the engine carries along promise reactions (captured at
// `then`/`await`, restored around the handler). Everything else that defers a callback — timers,
// nextTick, setImmediate — binds the context at scheduling time with this helper, and
// `AsyncLocalStorage` (shims.js) keys its stores off the value. A frame is an immutable Map
// from storage to store; `undefined` is the empty context, which is also what a fresh loop
// turn (an I/O completion) runs in.
const __asyncContextGet = __node.asyncContextGet;
const __asyncContextSet = __node.asyncContextSet;
function __bindAsyncContext(fn) {
  const context = __asyncContextGet();
  if (context === undefined) return fn;
  return function boundWithAsyncContext(...args) {
    const previous = __asyncContextSet(context);
    try {
      return fn.apply(this, args);
    } finally {
      __asyncContextSet(previous);
    }
  };
}
function __runInAsyncContext(context, fn, thisArg, args) {
  const previous = __asyncContextSet(context);
  try {
    return Reflect.apply(fn, thisArg, args);
  } finally {
    __asyncContextSet(previous);
  }
}

// Node's `global` is an alias for the global object.
if (typeof globalThis.global === "undefined") {
  globalThis.global = globalThis;
}

// V8's `Error.captureStackTrace` / `Error.prepareStackTrace` / CallSite API. The engine does not
// implement these (grep confirms: no such symbol), and this preamble runs once — so we define them
// outright rather than guarding on `typeof`. They are pervasive in the Node ecosystem: http-errors
// and depd build Error subclasses with captureStackTrace, and depd reads *structured* frames
// (a `prepareStackTrace` hook receiving CallSite objects it calls `.getFileName()`/`.getLineNumber()`
// on). lumen has no per-frame source info, so we hand back placeholder CallSites — enough that the
// deprecation machinery runs (with a `<lumen>` location) instead of crashing. `.stack` stays a
// normal string when no custom `prepareStackTrace` hook is installed.
Error.stackTraceLimit = 10;
const makeCallSite = () => ({
  getThis: () => undefined,
  getTypeName: () => null,
  getFunction: () => undefined,
  getFunctionName: () => null,
  getMethodName: () => null,
  getFileName: () => "<lumen>",
  getLineNumber: () => 0,
  getColumnNumber: () => 0,
  getEvalOrigin: () => undefined,
  isToplevel: () => true,
  isEval: () => false,
  isNative: () => false,
  isConstructor: () => false,
  isAsync: () => false,
  toString: () => "<lumen>:0:0",
});
Error.captureStackTrace = function (target, _ctorOpt) {
  const sites = Array.from({ length: Error.stackTraceLimit || 10 }, makeCallSite);
  Object.defineProperty(target, "stack", {
    configurable: true,
    get() {
      const prepare = Error.prepareStackTrace;
      if (typeof prepare === "function") return prepare(target, sites);
      return `${target.name || "Error"}: ${target.message || ""}\n    at <lumen>:0:0`;
    },
    set(value) {
      Object.defineProperty(target, "stack", { value, writable: true, configurable: true });
    },
  });
  return target;
};

// A registry the module system fills in; each builtin registers itself as it is defined.
const __builtins = new Map();
// Glue-internal values shared between the wrapped builtin files (never user-requirable).
const __internals = new Map();

// libuv's error table (uv_err_name / uv_strerror): name -> description, and the negative errno
// each platform's libuv reports (Windows uses libuv's own -40xx range; Unix negates errno). Names
// missing from a platform's override list use libuv's portable value (the Windows column).
const __uvErrmap = (() => {
  const desc = {E2BIG:"argument list too long",EACCES:"permission denied",EADDRINUSE:"address already in use",EADDRNOTAVAIL:"address not available",EAFNOSUPPORT:"address family not supported",EAGAIN:"resource temporarily unavailable",EAI_ADDRFAMILY:"address family not supported",EAI_AGAIN:"temporary failure",EAI_BADFLAGS:"bad ai_flags value",EAI_BADHINTS:"invalid value for hints",EAI_CANCELED:"request canceled",EAI_FAIL:"permanent failure",EAI_FAMILY:"ai_family not supported",EAI_MEMORY:"out of memory",EAI_NODATA:"no address",EAI_NONAME:"unknown node or service",EAI_OVERFLOW:"argument buffer overflow",EAI_PROTOCOL:"resolved protocol is unknown",EAI_SERVICE:"service not available for socket type",EAI_SOCKTYPE:"socket type not supported",EALREADY:"connection already in progress",EBADF:"bad file descriptor",EBUSY:"resource busy or locked",ECANCELED:"operation canceled",ECHARSET:"invalid Unicode character",ECONNABORTED:"software caused connection abort",ECONNREFUSED:"connection refused",ECONNRESET:"connection reset by peer",EDESTADDRREQ:"destination address required",EEXIST:"file already exists",EFAULT:"bad address in system call argument",EFBIG:"file too large",EHOSTUNREACH:"host is unreachable",EINTR:"interrupted system call",EINVAL:"invalid argument",EIO:"i/o error",EISCONN:"socket is already connected",EISDIR:"illegal operation on a directory",ELOOP:"too many symbolic links encountered",EMFILE:"too many open files",EMSGSIZE:"message too long",ENAMETOOLONG:"name too long",ENETDOWN:"network is down",ENETUNREACH:"network is unreachable",ENFILE:"file table overflow",ENOBUFS:"no buffer space available",ENODEV:"no such device",ENOENT:"no such file or directory",ENOMEM:"not enough memory",ENONET:"machine is not on the network",ENOPROTOOPT:"protocol not available",ENOSPC:"no space left on device",ENOSYS:"function not implemented",ENOTCONN:"socket is not connected",ENOTDIR:"not a directory",ENOTEMPTY:"directory not empty",ENOTSOCK:"socket operation on non-socket",ENOTSUP:"operation not supported on socket",EOVERFLOW:"value too large for defined data type",EPERM:"operation not permitted",EPIPE:"broken pipe",EPROTO:"protocol error",EPROTONOSUPPORT:"protocol not supported",EPROTOTYPE:"protocol wrong type for socket",ERANGE:"result too large",EROFS:"read-only file system",ESHUTDOWN:"cannot send after transport endpoint shutdown",ESPIPE:"invalid seek",ESRCH:"no such process",ETIMEDOUT:"connection timed out",ETXTBSY:"text file is busy",EXDEV:"cross-device link not permitted",UNKNOWN:"unknown error",EOF:"end of file",ENXIO:"no such device or address",EMLINK:"too many links",EHOSTDOWN:"host is down",EREMOTEIO:"remote I/O error",ENOTTY:"inappropriate ioctl for device",EFTYPE:"inappropriate file type or format",EILSEQ:"illegal byte sequence",ESOCKTNOSUPPORT:"socket type not supported",ENODATA:"no data available",EUNATCH:"protocol driver not attached"};
  const base = {E2BIG:-4093,EACCES:-4092,EADDRINUSE:-4091,EADDRNOTAVAIL:-4090,EAFNOSUPPORT:-4089,EAGAIN:-4088,EAI_ADDRFAMILY:-3000,EAI_AGAIN:-3001,EAI_BADFLAGS:-3002,EAI_BADHINTS:-3013,EAI_CANCELED:-3003,EAI_FAIL:-3004,EAI_FAMILY:-3005,EAI_MEMORY:-3006,EAI_NODATA:-3007,EAI_NONAME:-3008,EAI_OVERFLOW:-3009,EAI_PROTOCOL:-3014,EAI_SERVICE:-3010,EAI_SOCKTYPE:-3011,EALREADY:-4084,EBADF:-4083,EBUSY:-4082,ECANCELED:-4081,ECHARSET:-4080,ECONNABORTED:-4079,ECONNREFUSED:-4078,ECONNRESET:-4077,EDESTADDRREQ:-4076,EEXIST:-4075,EFAULT:-4074,EFBIG:-4036,EHOSTUNREACH:-4073,EINTR:-4072,EINVAL:-4071,EIO:-4070,EISCONN:-4069,EISDIR:-4068,ELOOP:-4067,EMFILE:-4066,EMSGSIZE:-4065,ENAMETOOLONG:-4064,ENETDOWN:-4063,ENETUNREACH:-4062,ENFILE:-4061,ENOBUFS:-4060,ENODEV:-4059,ENOENT:-4058,ENOMEM:-4057,ENONET:-4056,ENOPROTOOPT:-4035,ENOSPC:-4055,ENOSYS:-4054,ENOTCONN:-4053,ENOTDIR:-4052,ENOTEMPTY:-4051,ENOTSOCK:-4050,ENOTSUP:-4049,EOVERFLOW:-4026,EPERM:-4048,EPIPE:-4047,EPROTO:-4046,EPROTONOSUPPORT:-4045,EPROTOTYPE:-4044,ERANGE:-4034,EROFS:-4043,ESHUTDOWN:-4042,ESPIPE:-4041,ESRCH:-4040,ETIMEDOUT:-4039,ETXTBSY:-4038,EXDEV:-4037,UNKNOWN:-4094,EOF:-4095,ENXIO:-4033,EMLINK:-4032,EHOSTDOWN:-4031,EREMOTEIO:-4030,ENOTTY:-4029,EFTYPE:-4028,EILSEQ:-4027,ESOCKTNOSUPPORT:-4025,ENODATA:-4024,EUNATCH:-4023};
  const darwin = {E2BIG:-7,EACCES:-13,EADDRINUSE:-48,EADDRNOTAVAIL:-49,EAFNOSUPPORT:-47,EAGAIN:-35,EALREADY:-37,EBADF:-9,EBUSY:-16,ECANCELED:-89,ECONNABORTED:-53,ECONNREFUSED:-61,ECONNRESET:-54,EDESTADDRREQ:-39,EEXIST:-17,EFAULT:-14,EFBIG:-27,EHOSTUNREACH:-65,EINTR:-4,EINVAL:-22,EIO:-5,EISCONN:-56,EISDIR:-21,ELOOP:-62,EMFILE:-24,EMSGSIZE:-40,ENAMETOOLONG:-63,ENETDOWN:-50,ENETUNREACH:-51,ENFILE:-23,ENOBUFS:-55,ENODEV:-19,ENOENT:-2,ENOMEM:-12,ENOPROTOOPT:-42,ENOSPC:-28,ENOSYS:-78,ENOTCONN:-57,ENOTDIR:-20,ENOTEMPTY:-66,ENOTSOCK:-38,ENOTSUP:-45,EOVERFLOW:-84,EPERM:-1,EPIPE:-32,EPROTO:-100,EPROTONOSUPPORT:-43,EPROTOTYPE:-41,ERANGE:-34,EROFS:-30,ESHUTDOWN:-58,ESPIPE:-29,ESRCH:-3,ETIMEDOUT:-60,ETXTBSY:-26,EXDEV:-18,ENXIO:-6,EMLINK:-31,EHOSTDOWN:-64,ENOTTY:-25,EFTYPE:-79,EILSEQ:-92,ESOCKTNOSUPPORT:-44,ENODATA:-96};
  const linux = {E2BIG:-7,EACCES:-13,EADDRINUSE:-98,EADDRNOTAVAIL:-99,EAFNOSUPPORT:-97,EAGAIN:-11,EALREADY:-114,EBADF:-9,EBUSY:-16,ECANCELED:-125,ECONNABORTED:-103,ECONNREFUSED:-111,ECONNRESET:-104,EDESTADDRREQ:-89,EEXIST:-17,EFAULT:-14,EFBIG:-27,EHOSTUNREACH:-113,EINTR:-4,EINVAL:-22,EIO:-5,EISCONN:-106,EISDIR:-21,ELOOP:-40,EMFILE:-24,EMSGSIZE:-90,ENAMETOOLONG:-36,ENETDOWN:-100,ENETUNREACH:-101,ENFILE:-23,ENOBUFS:-105,ENODEV:-19,ENOENT:-2,ENOMEM:-12,ENONET:-64,ENOPROTOOPT:-92,ENOSPC:-28,ENOSYS:-38,ENOTCONN:-107,ENOTDIR:-20,ENOTEMPTY:-39,ENOTSOCK:-88,ENOTSUP:-95,EOVERFLOW:-75,EPERM:-1,EPIPE:-32,EPROTO:-71,EPROTONOSUPPORT:-93,EPROTOTYPE:-91,ERANGE:-34,EROFS:-30,ESHUTDOWN:-108,ESPIPE:-29,ESRCH:-3,ETIMEDOUT:-110,ETXTBSY:-26,EXDEV:-18,ENXIO:-6,EMLINK:-31,EHOSTDOWN:-112,EREMOTEIO:-121,ENOTTY:-25,EILSEQ:-84,ESOCKTNOSUPPORT:-94,ENODATA:-61,EUNATCH:-49};
  const platform = __os.info().platform;
  const over = platform === "win32" ? {} : platform === "linux" || platform === "android" ? linux : darwin;
  const map = new Map();
  for (const name of Object.keys(base)) map.set(over[name] ?? base[name], [name, desc[name]]);
  return map;
})();
const __uvCodes = new Map([...__uvErrmap].map(([errno, [name]]) => [name, errno]));

// ---- Node-style coded errors ------------------------------------------------------------------

function __nodeError(Base, code, message) {
  const err = new Base(message);
  Object.defineProperty(err, "toString", {
    value() { return `${this.name} [${code}]: ${this.message}`; },
    enumerable: false, writable: true, configurable: true,
  });
  err.code = code;
  if (typeof err.stack === "string") {
    const name = err.name;
    if (err.stack.startsWith(`${name}: `) || err.stack === name) {
      err.stack = `${name} [${code}]${err.stack.slice(name.length)}`;
    }
  }
  return err;
}

const __kTypes = ["string", "function", "number", "object", "Function", "Object", "boolean", "bigint", "symbol"];

function __determineSpecificType(value) {
  const inspect = (v, o) => __builtins.get("util").inspect(v, o);
  if (value == null) return "" + value;
  if (typeof value === "function") return `function ${value.name}`; // Node 20: no name check
  if (typeof value === "object") {
    if (value.constructor && value.constructor.name) return `an instance of ${value.constructor.name}`;
    return `${inspect(value, { depth: -1 })}`;
  }
  let inspected = inspect(value, { colors: false });
  if (inspected.length > 28) inspected = `${inspected.slice(0, 25)}...`;
  return `type ${typeof value} (${inspected})`;
}

function __invalidArgTypeMessage(name, expected, actual) {
  const inspect = (v, o) => __builtins.get("util").inspect(v, o);
  if (!Array.isArray(expected)) expected = [expected];
  let msg = "The ";
  if (name.endsWith(" argument")) {
    msg += `${name} `;
  } else {
    const type = String.prototype.includes.call(name, ".") ? "property" : "argument";
    msg += `"${name}" ${type} `;
  }
  msg += "must be ";
  const types = [];
  const instances = [];
  const other = [];
  for (const value of expected) {
    if (__kTypes.includes(value)) types.push(value.toLowerCase());
    else if (/^([A-Z][a-z0-9]*)+$/.test(value)) instances.push(value);
    else other.push(value);
  }
  // Special handle `object` in case other instances are allowed to outline the differences
  // between each other.
  if (instances.length > 0) {
    const pos = types.indexOf("object");
    if (pos !== -1) {
      types.splice(pos, 1);
      instances.push("Object");
    }
  }
  if (types.length > 0) {
    if (types.length > 2) {
      const last = types.pop();
      msg += `one of type ${types.join(", ")}, or ${last}`;
    } else if (types.length === 2) {
      msg += `one of type ${types[0]} or ${types[1]}`;
    } else {
      msg += `of type ${types[0]}`;
    }
    if (instances.length > 0 || other.length > 0) msg += " or ";
  }
  if (instances.length > 0) {
    if (instances.length > 2) {
      const last = instances.pop();
      msg += `an instance of ${instances.join(", ")}, or ${last}`;
    } else {
      msg += `an instance of ${instances[0]}`;
      if (instances.length === 2) msg += ` or ${instances[1]}`;
    }
    if (other.length > 0) msg += " or ";
  }
  if (other.length > 0) {
    if (other.length > 2) {
      const last = other.pop();
      msg += `one of ${other.join(", ")}, or ${last}`;
    } else if (other.length === 2) {
      msg += `one of ${other[0]} or ${other[1]}`;
    } else {
      if (other[0].toLowerCase() !== other[0]) msg += "an ";
      msg += `${other[0]}`;
    }
  }
  msg += `. Received ${__determineSpecificType(actual)}`;
  return msg;
}

// ERR_* codes: `new __errors.ERR_X(...args)` builds the error Node's `internal/errors` would —
// the right base class, `.code`, the exact message, `toString()` and a `Name [CODE]: message`
// stack header. Message builders take Node's argument lists.
const __errors = (() => {
  const inspect = (value, opts) => __builtins.get("util").inspect(value, opts);
  const format = (...args) => __builtins.get("util").format(...args);
  const addNumericalSeparator = (val) => {
    let res = "";
    let i = val.length;
    const start = val[0] === "-" ? 1 : 0;
    for (; i >= start + 4; i -= 3) res = `_${val.slice(i - 3, i)}${res}`;
    return `${val.slice(0, i)}${res}`;
  };
  const codes = {};
  function E(key, msg, Base, ...otherBases) {
    const make = (B) => ({
      [key]: function (...args) {
        const message = typeof msg === "function" ? msg(...args) : args.length ? format(msg, ...args) : msg;
        return __nodeError(B, key, message);
      },
    })[key];
    codes[key] = make(Base);
    for (const B of otherBases) codes[key][B.name] = make(B);
  }
  E("ERR_INVALID_ARG_TYPE", __invalidArgTypeMessage, TypeError);
  E("ERR_INVALID_ARG_VALUE", (name, value, reason = "is invalid") => {
    let inspected = inspect(value);
    if (inspected.length > 128) inspected = `${inspected.slice(0, 128)}...`;
    const type = String.prototype.includes.call(name, ".") ? "property" : "argument";
    return `The ${type} '${name}' ${reason}. Received ${inspected}`;
  }, TypeError, RangeError);
  E("ERR_OUT_OF_RANGE", (str, range, input, replaceDefaultBoolean = false) => {
    const msg = replaceDefaultBoolean ? str : `The value of "${str}" is out of range.`;
    let received;
    if (Number.isInteger(input) && Math.abs(input) > 2 ** 32) {
      received = addNumericalSeparator(String(input));
    } else if (typeof input === "bigint") {
      received = String(input);
      if (input > 2n ** 32n || input < -(2n ** 32n)) received = addNumericalSeparator(received);
      received += "n";
    } else {
      received = inspect(input);
    }
    return `${msg} It must be ${range}. Received ${received}`;
  }, RangeError);
  E("ERR_MISSING_ARGS", (...args) => {
    let msg = "The ";
    const len = args.length;
    const wrap = (a) => `"${a}"`;
    args = args.map((a) => (Array.isArray(a) ? a.map(wrap).join(" or ") : wrap(a)));
    switch (len) {
      case 1: msg += `${args[0]} argument`; break;
      case 2: msg += `${args[0]} and ${args[1]} arguments`; break;
      default: {
        const last = args.pop();
        msg += `${args.join(", ")}, and ${last} arguments`;
      }
    }
    return `${msg} must be specified`;
  }, TypeError);
  E("ERR_INVALID_RETURN_VALUE", (input, name, value) => {
    let type;
    if (value && value.constructor && value.constructor.name) type = `instance of ${value.constructor.name}`;
    else type = `type ${typeof value}`;
    return `Expected ${input} to be returned from the "${name}" function but got ${type}.`;
  }, TypeError, RangeError);
  E("ERR_AMBIGUOUS_ARGUMENT", 'The "%s" argument is ambiguous. %s', TypeError);
  E("ERR_INVALID_THIS", 'Value of "this" must be of type %s', TypeError);
  E("ERR_INVALID_STATE", "Invalid state: %s", Error, TypeError, RangeError);
  E("ERR_ILLEGAL_CONSTRUCTOR", "Illegal constructor", TypeError);
  E("ERR_METHOD_NOT_IMPLEMENTED", "The %s method is not implemented", Error);
  E("ERR_MISSING_OPTION", "%s is required", TypeError);
  E("ERR_UNKNOWN_ENCODING", "Unknown encoding: %s", TypeError);
  E("ERR_BUFFER_OUT_OF_BOUNDS", (name = undefined) => {
    if (name) return `"${name}" is outside of buffer bounds`;
    return "Attempt to access memory outside buffer bounds";
  }, RangeError);
  E("ERR_INVALID_BUFFER_SIZE", "Buffer size must be a multiple of %s", RangeError);
  E("ERR_UNKNOWN_BUILTIN_MODULE", "No such built-in module: %s", Error);
  E("ERR_UNHANDLED_ERROR", (err = undefined) => {
    const msg = "Unhandled error.";
    if (err === undefined) return msg;
    return `${msg} (${err})`;
  }, Error);
  E("ERR_MULTIPLE_CALLBACK", "Callback called multiple times", Error);
  E("ERR_STREAM_WRITE_AFTER_END", "write after end", Error);
  E("ERR_STREAM_PREMATURE_CLOSE", "Premature close", Error);
  E("ERR_STREAM_DESTROYED", "Cannot call %s after a stream was destroyed", Error);
  E("ERR_STREAM_NULL_VALUES", "May not write null values to stream", TypeError);
  E("ERR_STREAM_ALREADY_FINISHED", "Cannot call %s after a stream was finished", Error);
  E("ERR_STREAM_CANNOT_PIPE", "Cannot pipe, not readable", Error);
  E("ERR_STREAM_PUSH_AFTER_EOF", "stream.push() after EOF", Error);
  E("ERR_STREAM_UNSHIFT_AFTER_END_EVENT", "stream.unshift() after end event", Error);
  E("ERR_INVALID_URL", "Invalid URL", TypeError);
  E("ERR_INVALID_URL_SCHEME", (expected) => {
    if (typeof expected === "string") expected = [expected];
    const res = expected.length === 2 ? `one of scheme ${expected[0]} or ${expected[1]}` : `of scheme ${expected[0]}`;
    return `The URL must be ${res}`;
  }, TypeError);
  E("ERR_INVALID_FILE_URL_PATH", "File URL path %s", TypeError);
  E("ERR_INVALID_FILE_URL_HOST", 'File URL host must be "localhost" or empty on %s', TypeError);
  E("ERR_SOCKET_BAD_PORT", (name, port, allowZero = true) => {
    const operator = allowZero ? ">=" : ">";
    return `${name} should be ${operator} 0 and < 65536. Received ${__determineSpecificType(port)}.`;
  }, RangeError);
  E("ERR_INVALID_FD", '"fd" must be a positive integer: %s', RangeError);
  E("ERR_INVALID_CURSOR_POS", "Cannot set cursor row without setting its column", TypeError);
  E("ERR_INVALID_FD_TYPE", "Unsupported fd type: %s", TypeError);
  E("ERR_FS_FILE_TOO_LARGE", "File size (%s) is greater than 2 GiB", RangeError);
  E("ERR_FS_EISDIR", "Path is a directory", Error);
  E("ERR_DIR_CLOSED", "Directory handle was closed", Error);
  E("ERR_DIR_CONCURRENT_OPERATION", "Cannot do synchronous work on directory handle with concurrent asynchronous operations", Error);
  E("ERR_FALSY_VALUE_REJECTION", "Promise was rejected with falsy value", Error);
  E("ERR_INVALID_CHAR", (name, field = undefined) => {
    let msg = `Invalid character in ${name}`;
    if (field !== undefined) msg += ` ["${field}"]`;
    return msg;
  }, TypeError);
  E("ERR_INVALID_HTTP_TOKEN", '%s must be a valid HTTP token ["%s"]', TypeError);
  E("ERR_HTTP_HEADERS_SENT", "Cannot %s headers after they are sent to the client", Error);
  E("ERR_HTTP_INVALID_HEADER_VALUE", 'Invalid value "%s" for header "%s"', TypeError);
  E("ERR_UNKNOWN_SIGNAL", "Unknown signal: %s", TypeError);
  E("ERR_INVALID_IP_ADDRESS", "Invalid IP address: %s", TypeError);
  E("ERR_ENCODING_NOT_SUPPORTED", 'The "%s" encoding is not supported', RangeError);
  E("ERR_ENCODING_INVALID_ENCODED_DATA", (encoding) => `The encoded data was not valid for encoding ${encoding}`, TypeError);
  E("ERR_CRYPTO_INVALID_DIGEST", "Invalid digest: %s", TypeError);
  E("ERR_CRYPTO_HASH_FINALIZED", "Digest already called", Error);
  E("ERR_CRYPTO_HASH_UPDATE_FAILED", "Hash update failed", Error);
  E("ERR_CRYPTO_INVALID_STATE", "Invalid state for operation %s", Error);
  E("ERR_SOCKET_DGRAM_IS_CONNECTED", "Already connected", Error);
  E("ERR_SOCKET_DGRAM_NOT_CONNECTED", "Not connected", Error);
  E("ERR_SOCKET_DGRAM_NOT_RUNNING", "Not running", Error);
  E("ERR_SOCKET_CLOSED", "Socket is closed", Error);
  E("ERR_SERVER_NOT_RUNNING", "Server is not running.", Error);
  E("ERR_SERVER_ALREADY_LISTEN", "Listen method has been called more than once without closing.", Error);
  E("ERR_USE_AFTER_CLOSE", "%s was closed", Error);
  E("ERR_IPC_CHANNEL_CLOSED", "Channel closed", Error);
  E("ERR_INVALID_ADDRESS_FAMILY", (addressType, host, port) => `Invalid address family: ${addressType} ${host}:${port}`, RangeError);
  E("ERR_CHILD_PROCESS_STDIO_MAXBUFFER", "%s maxBuffer length exceeded", RangeError);
  E("ERR_ASYNC_CALLBACK", "%s must be a function", TypeError);
  E("ERR_ASYNC_TYPE", 'Invalid name for async "type": %s', TypeError);
  E("ERR_OPERATION_FAILED", "Operation failed: %s", Error, TypeError);
  E("ERR_INVALID_ARGUMENT", "%s", TypeError);
  E("ERR_UNAVAILABLE_DURING_EXIT", "Cannot call function in process exit handler", Error);
  E("ERR_EVENT_RECURSION", 'The event "%s" is already being dispatched', Error);
  E("ERR_INVALID_OBJECT_DEFINE_PROPERTY", "%s", TypeError);
  E("ERR_UNESCAPED_CHARACTERS", "%s contains unescaped characters", TypeError);
  E("ERR_TLS_CERT_ALTNAME_INVALID", (reason) => `Hostname/IP does not match certificate's altnames: ${reason}`, Error);
  E("ERR_WORKER_UNSUPPORTED_OPERATION", "%s is not supported in workers", TypeError);
  E("ERR_NO_CRYPTO", "Node.js is not compiled with OpenSSL crypto support", Error);
  E("ERR_FEATURE_UNAVAILABLE_ON_PLATFORM", "The feature %s is unavailable on the current platform, which is being used to run Node.js", TypeError);
  return codes;
})();

// Argument validators with Node's exact error codes and messages (internal/validators).
const __validators = (() => {
  const { ERR_INVALID_ARG_TYPE, ERR_INVALID_ARG_VALUE, ERR_OUT_OF_RANGE } = __errors;
  const validateString = (value, name) => {
    if (typeof value !== "string") throw new ERR_INVALID_ARG_TYPE(name, "string", value);
  };
  const validateNumber = (value, name, min = undefined, max) => {
    if (typeof value !== "number") throw new ERR_INVALID_ARG_TYPE(name, "number", value);
    if ((min != null && value < min) || (max != null && value > max) ||
        ((min != null || max != null) && Number.isNaN(value))) {
      throw new ERR_OUT_OF_RANGE(
        name,
        `${min != null ? `>= ${min}` : ""}${min != null && max != null ? " && " : ""}${max != null ? `<= ${max}` : ""}`,
        value);
    }
  };
  const validateInteger = (value, name, min = Number.MIN_SAFE_INTEGER, max = Number.MAX_SAFE_INTEGER) => {
    if (typeof value !== "number") throw new ERR_INVALID_ARG_TYPE(name, "number", value);
    if (!Number.isInteger(value)) throw new ERR_OUT_OF_RANGE(name, "an integer", value);
    if (value < min || value > max) throw new ERR_OUT_OF_RANGE(name, `>= ${min} && <= ${max}`, value);
  };
  const validateInt32 = (value, name, min = -2147483648, max = 2147483647) => {
    if (typeof value !== "number") throw new ERR_INVALID_ARG_TYPE(name, "number", value);
    if (!Number.isInteger(value)) throw new ERR_OUT_OF_RANGE(name, "an integer", value);
    if (value < min || value > max) throw new ERR_OUT_OF_RANGE(name, `>= ${min} && <= ${max}`, value);
  };
  const validateUint32 = (value, name, positive = false) => {
    if (typeof value !== "number") throw new ERR_INVALID_ARG_TYPE(name, "number", value);
    if (!Number.isInteger(value)) throw new ERR_OUT_OF_RANGE(name, "an integer", value);
    const min = positive ? 1 : 0;
    const max = 4294967295;
    if (value < min || value > max) throw new ERR_OUT_OF_RANGE(name, `>= ${min} && <= ${max}`, value);
  };
  const validateBoolean = (value, name) => {
    if (typeof value !== "boolean") throw new ERR_INVALID_ARG_TYPE(name, "boolean", value);
  };
  const validateFunction = (value, name) => {
    if (typeof value !== "function") throw new ERR_INVALID_ARG_TYPE(name, "Function", value);
  };
  const kValidateObjectNone = 0;
  const kValidateObjectAllowNullable = 1;
  const kValidateObjectAllowArray = 2;
  const kValidateObjectAllowFunction = 4;
  const validateObject = (value, name, options = kValidateObjectNone) => {
    if (options === kValidateObjectNone) {
      if (value === null || Array.isArray(value) || typeof value !== "object") {
        throw new ERR_INVALID_ARG_TYPE(name, "Object", value);
      }
      return;
    }
    const possiblyNull = (options & kValidateObjectAllowNullable) !== 0;
    const allowArray = (options & kValidateObjectAllowArray) !== 0;
    const allowFunction = (options & kValidateObjectAllowFunction) !== 0;
    if (value === null) {
      if (!possiblyNull) throw new ERR_INVALID_ARG_TYPE(name, "Object", value);
      return;
    }
    if (!allowArray && Array.isArray(value)) throw new ERR_INVALID_ARG_TYPE(name, "Object", value);
    const type = typeof value;
    if (type !== "object" && !(allowFunction && type === "function")) {
      throw new ERR_INVALID_ARG_TYPE(name, "Object", value);
    }
  };
  const validateArray = (value, name, minLength = 0) => {
    if (!Array.isArray(value)) throw new ERR_INVALID_ARG_TYPE(name, "Array", value);
    if (value.length < minLength) throw new ERR_INVALID_ARG_VALUE(name, value, `must be longer than ${minLength}`);
  };
  const validateOneOf = (value, name, oneOf) => {
    if (!oneOf.includes(value)) {
      const allowed = oneOf.map((v) => (typeof v === "string" ? `'${v}'` : String(v))).join(", ");
      throw new ERR_INVALID_ARG_VALUE(name, value, "must be one of: " + allowed);
    }
  };
  const validateAbortSignal = (signal, name) => {
    if (signal !== undefined && (signal === null || typeof signal !== "object" || !("aborted" in signal))) {
      throw new ERR_INVALID_ARG_TYPE(name, "AbortSignal", signal);
    }
  };
  const validateBuffer = (buffer, name = "buffer") => {
    if (!ArrayBuffer.isView(buffer)) throw new ERR_INVALID_ARG_TYPE(name, ["Buffer", "TypedArray", "DataView"], buffer);
  };
  return {
    validateString, validateNumber, validateInteger, validateInt32, validateUint32, validateBoolean,
    validateFunction, validateObject, validateArray, validateOneOf, validateAbortSignal, validateBuffer,
    kValidateObjectNone, kValidateObjectAllowNullable, kValidateObjectAllowArray, kValidateObjectAllowFunction,
  };
})();

// Node's `primordials` for glue ported verbatim from Node's lib/: `ArrayPrototypePush(arr, x)`,
// `ObjectKeys(o)`, `SymbolAsyncIterator`, `ArrayBufferPrototypeGetByteLength(ab)`, `SafeMap`, ...
// Resolved by name on first use (a Proxy over a cache) instead of being built eagerly at boot:
// `<Global>` is the intrinsic, `<Global><Key>` a static (functions bound to their owner),
// `<Global>Prototype<Key>` an uncurried prototype method, and `Get`/`Set` + key an uncurried
// accessor. `Symbol<Name>` keys name well-known symbols. The `Safe*` classes are the plain ones:
// the guarantee they add (immunity to user monkey-patching of the prototypes) is not observable
// to the tests that exercise this glue.
const __primordials = (() => {
  const cache = { __proto__: null };
  const uncurryThis = (fn) => function (thisArg, ...args) { return Reflect.apply(fn, thisArg, args); };
  const applyBind = (fn) => function (thisArg, args) { return Reflect.apply(fn, thisArg, args); };
  const TypedArray = Object.getPrototypeOf(Uint8Array);
  const AsyncIteratorPrototype = Object.getPrototypeOf(Object.getPrototypeOf(async function* () {}).prototype);
  const IteratorPrototype = Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]()));
  const ArrayIteratorPrototype = Object.getPrototypeOf([][Symbol.iterator]());
  const StringIteratorPrototype = Object.getPrototypeOf(""[Symbol.iterator]());
  const intrinsics = {
    __proto__: null, TypedArray, AsyncIteratorPrototype, IteratorPrototype, ArrayIteratorPrototype,
    StringIteratorPrototype,
  };
  const globalNames = [
    "AggregateError", "Array", "ArrayBuffer", "BigInt", "BigInt64Array", "BigUint64Array", "Boolean",
    "DataView", "Date", "Error", "EvalError", "FinalizationRegistry", "Float32Array", "Float64Array",
    "Function", "Int16Array", "Int32Array", "Int8Array", "Map", "Number", "Object", "RangeError",
    "ReferenceError", "RegExp", "Set", "String", "Symbol", "SyntaxError", "TypeError", "URIError",
    "Uint16Array", "Uint32Array", "Uint8Array", "Uint8ClampedArray", "WeakMap", "WeakRef", "WeakSet",
    "Promise", "Reflect", "Math", "JSON", "Atomics", "SharedArrayBuffer",
    "decodeURI", "decodeURIComponent", "encodeURI", "encodeURIComponent", "escape", "unescape",
    "eval", "isFinite", "isNaN", "parseFloat", "parseInt",
  ];
  const owners = [...globalNames, ...Object.keys(intrinsics)].sort((a, b) => b.length - a.length);
  const ownerValue = (name) => (name in intrinsics ? intrinsics[name] : globalThis[name]);
  const lcfirst = (s) => s[0].toLowerCase() + s.slice(1);
  function findKey(target, rest) {
    if (rest.startsWith("Symbol")) {
      const sym = Symbol[lcfirst(rest.slice(6))];
      if (typeof sym === "symbol") return { key: sym };
    }
    for (const key of [rest, lcfirst(rest)]) {
      if (key in target) return { key };
    }
    for (const kind of ["Get", "Set"]) {
      if (!rest.startsWith(kind)) continue;
      const inner = findKey(target, rest.slice(3));
      if (!inner) continue;
      let o = target, d;
      while (o && !(d = Object.getOwnPropertyDescriptor(o, inner.key))) o = Object.getPrototypeOf(o);
      const accessor = d && (kind === "Get" ? d.get : d.set);
      if (accessor) return { accessor };
    }
    return null;
  }
  function makeSafePromise(fn) {
    return fn;
  }
  const special = {
    __proto__: null,
    globalThis: () => globalThis,
    uncurryThis: () => uncurryThis,
    applyBind: () => applyBind,
    hardenRegExp: () => (re) => re,
    makeSafe: () => (unsafe, safe) => safe,
    SafeMap: () => Map, SafeSet: () => Set, SafeWeakMap: () => WeakMap, SafeWeakSet: () => WeakSet,
    SafeWeakRef: () => WeakRef, SafeFinalizationRegistry: () => FinalizationRegistry,
    SafeArrayIterator: () => function SafeArrayIterator(array) { return array[Symbol.iterator](); },
    SafeStringIterator: () => function SafeStringIterator(str) { return str[Symbol.iterator](); },
    SafePromiseAll: () => (promises, mapFn) => Promise.all(mapFn ? Array.from(promises, mapFn) : promises),
    SafePromiseAllReturnArrayLike: () => (promises, mapFn) => Promise.all(mapFn ? Array.from(promises, mapFn) : promises),
    SafePromiseAllReturnVoid: () => (promises, mapFn) =>
      Promise.all(mapFn ? Array.from(promises, mapFn) : promises).then(() => undefined),
    SafePromiseAllSettled: () => (promises, mapFn) => Promise.allSettled(mapFn ? Array.from(promises, mapFn) : promises),
    SafePromiseAllSettledReturnVoid: () => (promises, mapFn) =>
      Promise.allSettled(mapFn ? Array.from(promises, mapFn) : promises).then(() => undefined),
    SafePromiseAny: () => (promises, mapFn) => Promise.any(mapFn ? Array.from(promises, mapFn) : promises),
    SafePromiseRace: () => (promises, mapFn) => Promise.race(mapFn ? Array.from(promises, mapFn) : promises),
    SafePromisePrototypeFinally: () => (p, fn) => Promise.prototype.finally.call(p, fn),
    PromisePrototypeThen: () => (p, a, b) => Promise.prototype.then.call(p, a, b),
    PromisePrototypeCatch: () => (p, b) => Promise.prototype.then.call(p, undefined, b),
    ArrayPrototypeSymbolIterator: () => uncurryThis(Array.prototype[Symbol.iterator]),
    ArrayIteratorPrototypeNext: () => uncurryThis(ArrayIteratorPrototype.next),
    StringIteratorPrototypeNext: () => uncurryThis(StringIteratorPrototype.next),
    AsyncIteratorPrototype: () => AsyncIteratorPrototype,
    IteratorPrototype: () => IteratorPrototype,
    RegExpPrototypeSymbolReplace: () => uncurryThis(RegExp.prototype[Symbol.replace]),
    RegExpPrototypeSymbolSplit: () => uncurryThis(RegExp.prototype[Symbol.split]),
    SymbolDispose: () => Symbol.dispose ?? Symbol.for("nodejs.dispose"),
    SymbolAsyncDispose: () => Symbol.asyncDispose ?? Symbol.for("nodejs.asyncDispose"),
    DateNow: () => Date.now,
    ErrorCaptureStackTrace: () => (target, ctor) => Error.captureStackTrace(target, ctor),
  };
  special.makeSafePromise = () => makeSafePromise;
  function resolve(name) {
    if (name in special) return special[name]();
    for (const owner of owners) {
      if (!name.startsWith(owner)) continue;
      const G = ownerValue(owner);
      if (G === undefined) continue;
      let rest = name.slice(owner.length);
      if (rest === "") return G;
      let target = G, proto = false;
      if (rest.startsWith("Prototype") && G.prototype !== undefined) {
        target = G.prototype;
        proto = true;
        rest = rest.slice(9);
        if (rest === "") return target;
      }
      let found = findKey(target, rest);
      if (!found && rest.endsWith("Apply")) {
        const inner = findKey(target, rest.slice(0, -5));
        if (inner && inner.key !== undefined && typeof target[inner.key] === "function") {
          return proto ? applyBind(target[inner.key]) : (args) => Reflect.apply(target[inner.key], G, args);
        }
      }
      if (!found) continue;
      if (found.accessor) return uncurryThis(found.accessor);
      const value = target[found.key];
      if (typeof value !== "function") return value;
      return proto ? uncurryThis(value) : value.bind(G);
    }
    throw new TypeError(`primordials: unknown intrinsic ${name}`);
  }
  return new Proxy(cache, {
    get(target, name) {
      if (typeof name !== "string") return undefined;
      if (!(name in target)) target[name] = resolve(name);
      return target[name];
    },
  });
})();


// Node defines most of its public classes as plain constructor functions, and two legacy idioms
// depend on that: calling one without `new` (`Buffer(4)`, `net.Socket()`, `zlib.Gzip()`), and
// old-style inheritance, `Parent.call(this, opts)` plus `util.inherits`. lumen writes them as ES
// classes, which throw on both. This wraps a finished class (statics assigned) in a constructor
// function that shares its prototype:
//   - `new X()` and `class Y extends X` construct the class exactly as before;
//   - `X()` constructs one, as Node's `if (!(this instanceof X)) return new X(...)` does;
//   - `X.call(obj, ...)` on an object that already inherits `X.prototype` runs `init(obj, args)`.
//     Without an `init` it throws: a class whose constructor closes over `this` cannot initialize
//     an object it did not allocate.
// `construct(args)`, when given, replaces the class constructor for `X(...)` and `new X(...)` (not
// for subclasses): Buffer's public constructor means `Buffer.alloc`/`Buffer.from`, while lumen's own
// code builds Buffers with the Uint8Array constructor.
// The class's own statics are mirrored as accessors, so `X.defaultMaxListeners = n` still reaches
// the class that its methods read.
function __legacyConstructor(Class, init, construct) {
  const Legacy = {
    [Class.name]: function (...args) {
      if (construct && (new.target === undefined || new.target === Legacy)) {
        if (new.target === undefined && this instanceof Legacy && init) {
          init(this, args);
          return undefined;
        }
        return construct(args);
      }
      if (new.target) return Reflect.construct(Class, args, new.target);
      if (this !== null && typeof this === "object" && this instanceof Legacy) {
        if (!init) {
          throw new TypeError(
            `${Class.name}.call(this) inheritance is not supported in lumen; extend it with \`class ... extends ${Class.name}\``,
          );
        }
        init(this, args);
        return undefined;
      }
      return Reflect.construct(Class, args, Legacy);
    },
  }[Class.name];
  Legacy.prototype = Class.prototype;
  Object.defineProperty(Class.prototype, "constructor", { value: Legacy, writable: true, configurable: true });
  Object.defineProperty(Legacy, "length", { value: Class.length });
  Object.setPrototypeOf(Legacy, Class);
  for (const key of Reflect.ownKeys(Class)) {
    if (key === "prototype" || key === "length" || key === "name") continue;
    const desc = Object.getOwnPropertyDescriptor(Class, key);
    if (!("value" in desc)) continue; // accessors are inherited and run against the class already
    Object.defineProperty(Legacy, key, {
      get: () => Class[key],
      set: (value) => { Class[key] = value; },
      enumerable: desc.enumerable,
      configurable: true,
    });
  }
  return Legacy;
}

// An `init` for __legacyConstructor when the constructor only sets state on `this` (no closures
// over it): construct a twin with the target's prototype and copy the twin's own properties over.
function __initByCopy(Class) {
  return (self, args) => {
    const Twin = function () {};
    Twin.prototype = Object.getPrototypeOf(self);
    Object.defineProperties(self, Object.getOwnPropertyDescriptors(Reflect.construct(Class, args, Twin)));
  };
}
