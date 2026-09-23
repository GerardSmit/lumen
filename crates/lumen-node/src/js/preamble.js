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
delete globalThis.__node;
delete globalThis.__os;
delete globalThis.__zlib;
delete globalThis.__bunhash;
delete globalThis.__child;
delete globalThis.__ffi;
delete globalThis.__crypto;
delete globalThis.__password;

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
