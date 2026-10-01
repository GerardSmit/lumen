// node:util/types. Kept apart from util.js so a module that needs only the type predicates
// (fs, net) does not load all of util.
// The engine gives every builtin an accurate `Object.prototype.toString` tag (Map, Promise,
// GeneratorFunction, Map Iterator, boxed Number/String/…, ArrayBuffer vs SharedArrayBuffer, …),
// so most of these predicates are exact. The few the engine cannot observe are noted inline and
// return false honestly rather than guessing.

const objToString = Object.prototype.toString;
const tagOf = (v) => {
  const s = objToString.call(v);
  return s.slice(8, s.length - 1); // "[object X]" -> "X"
};

// Brand checks, as V8's: an internal-slot probe (a prototype getter or method that throws on a
// foreign receiver), so neither Symbol.toStringTag nor a borrowed prototype can spoof them.
const isObjectLike = (v) => v !== null && (typeof v === "object" || typeof v === "function");
const brandGetter = (C, prop) => {
  const get = Object.getOwnPropertyDescriptor(C.prototype, prop).get;
  return (v) => {
    if (!isObjectLike(v)) return false;
    try { get.call(v); return true; } catch { return false; }
  };
};
const brandMethod = (fn, ...args) => (v) => {
  if (!isObjectLike(v)) return false;
  try { fn.call(v, ...args); return true; } catch { return false; }
};
// %TypedArray%.prototype[@@toStringTag] is the spec's typed-array brand probe.
const typedArrayName = (() => {
  const get = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(Uint8Array.prototype), Symbol.toStringTag).get;
  return (v) => get.call(v);
})();
const isTypedArrayOf = (name) => (v) => typedArrayName(v) === name;
const isArrayBufferBrand = brandGetter(ArrayBuffer, "byteLength");
const isSharedArrayBufferBrand = typeof SharedArrayBuffer === "function"
  ? brandGetter(SharedArrayBuffer, "byteLength") : () => false;
const isRegExpBrand = brandGetter(RegExp, "source");

const types = {
  // C++ external pointers have no JS representation in lumen, so nothing is ever an External.
  isExternal: () => false,
  isProxy: (v) => __node.isProxy(v),
  // crypto loads after util, so resolve the constructor lazily when the predicate is called.
  isKeyObject: (v) => {
    const crypto = __builtins.get("crypto");
    return !!crypto && typeof crypto.KeyObject === "function" && v instanceof crypto.KeyObject;
  },

  isDate: brandMethod(Date.prototype.getTime),
  isRegExp: (v) => v !== RegExp.prototype && isRegExpBrand(v),
  isArgumentsObject: (v) => tagOf(v) === "Arguments",
  isNativeError: (v) => v instanceof Error,
  isMap: brandGetter(Map, "size"),
  isSet: brandGetter(Set, "size"),
  isMapIterator: (v) => tagOf(v) === "Map Iterator",
  isSetIterator: (v) => tagOf(v) === "Set Iterator",
  isWeakMap: brandMethod(WeakMap.prototype.has, {}),
  isWeakSet: brandMethod(WeakSet.prototype.has, {}),
  isPromise: (v) => isObjectLike(v) && __node.promiseState(v) !== undefined,
  isGeneratorFunction: (v) => tagOf(v) === "GeneratorFunction" || tagOf(v) === "AsyncGeneratorFunction",
  isAsyncFunction: (v) => tagOf(v) === "AsyncFunction" || tagOf(v) === "AsyncGeneratorFunction",
  isGeneratorObject: (v) => tagOf(v) === "Generator",
  isModuleNamespaceObject: (v) => tagOf(v) === "Module",

  isNumberObject: brandMethod(Number.prototype.valueOf),
  isStringObject: brandMethod(String.prototype.valueOf),
  isBooleanObject: brandMethod(Boolean.prototype.valueOf),
  isSymbolObject: brandMethod(Symbol.prototype.valueOf),
  isBigIntObject: brandMethod(BigInt.prototype.valueOf),
  isBoxedPrimitive: (v) => types.isNumberObject(v) || types.isStringObject(v) || types.isBooleanObject(v)
    || types.isSymbolObject(v) || types.isBigIntObject(v),

  isArrayBuffer: isArrayBufferBrand,
  isSharedArrayBuffer: isSharedArrayBufferBrand,
  isAnyArrayBuffer: (v) => isArrayBufferBrand(v) || isSharedArrayBufferBrand(v),
  isDataView: brandGetter(DataView, "byteLength"),
  isArrayBufferView: (v) => ArrayBuffer.isView(v),
  isTypedArray: (v) => typedArrayName(v) !== undefined,
  isUint8Array: isTypedArrayOf("Uint8Array"),
  isUint8ClampedArray: isTypedArrayOf("Uint8ClampedArray"),
  isUint16Array: isTypedArrayOf("Uint16Array"),
  isUint32Array: isTypedArrayOf("Uint32Array"),
  isInt8Array: isTypedArrayOf("Int8Array"),
  isInt16Array: isTypedArrayOf("Int16Array"),
  isInt32Array: isTypedArrayOf("Int32Array"),
  isFloat16Array: isTypedArrayOf("Float16Array"),
  isFloat32Array: isTypedArrayOf("Float32Array"),
  isFloat64Array: isTypedArrayOf("Float64Array"),
  isBigInt64Array: isTypedArrayOf("BigInt64Array"),
  isBigUint64Array: isTypedArrayOf("BigUint64Array"),

  // CryptoKey is a WebCrypto global in lumen, so this one is observable.
  isCryptoKey: (v) => typeof CryptoKey !== "undefined" && v instanceof CryptoKey,
};

__builtins.set("util/types", types);
