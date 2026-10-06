// structuredClone. TextEncoder, TextDecoder, atob and btoa are native (lumen-host `encoding`).

function codedError(Ctor, code, message) {
  const err = new Ctor(message);
  err.code = code;
  if (typeof err.stack === "string") {
    err.stack = `${err.name} [${code}]${err.stack.slice(err.name.length)}`;
  }
  return err;
}

// HTML's StructuredSerializeWithTransfer + deserialize, done in one in-realm pass. Throws
// DataCloneError exactly where the spec does; transferred ArrayBuffers are detached afterwards.
const CLONE_ERROR_NAMES = new Set(["Error", "EvalError", "RangeError", "ReferenceError", "SyntaxError", "TypeError", "URIError"]);
const cloneAbByteLength = Object.getOwnPropertyDescriptor(ArrayBuffer.prototype, "byteLength").get;
const cloneTypedArrayTag = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(Uint8Array.prototype), Symbol.toStringTag).get;
const cloneBrand = (fn, v) => { try { fn.call(v); return true; } catch { return false; } };
const cloneIsArrayBuffer = (v) => cloneBrand(cloneAbByteLength, v);
function cloneDataCloneError(message) {
  return new DOMException(message, "DataCloneError");
}

function structuredClone(value, options) {
  if (arguments.length === 0) throw codedError(TypeError, "ERR_MISSING_ARGS", "The \"value\" argument must be specified");
  let transfer = [];
  if (options !== undefined && options !== null) {
    if (typeof options !== "object" && typeof options !== "function") {
      throw codedError(TypeError, "ERR_INVALID_ARG_TYPE", "The \"options\" argument must be of type object.");
    }
    const list = options.transfer;
    if (list !== undefined && list !== null) {
      if ((typeof list !== "object" && typeof list !== "function") || typeof list[Symbol.iterator] !== "function") {
        throw codedError(TypeError, "ERR_INVALID_ARG_TYPE", "The \"options.transfer\" property must be of type object.");
      }
      transfer = [...list];
    }
  }
  // Native Node ports need ownership attachments even for an in-realm clone. All transfer
  // validation and getters run before the native serializer commits detachment.
  if (globalThis.__cloneTransfer && transfer.some(t => globalThis.__lumenPortClone?.isPort(t))) {
    const bytes=globalThis.__serializeForClone(value, transfer, true);
    return globalThis.__deserializeClone(globalThis.__cloneTransfer.local(bytes));
  }
  const seen = new Map();
  for (const t of transfer) {
    if (t !== null && typeof t === "object" && t[Symbol.for("nodejs.untransferable")] === true) continue;
    if (t instanceof AbortSignal && t[Symbol.for("nodejs.abortsignal.transferable")] === true) {
      seen.set(t, globalThis.__cloneTransferableSignal(t));
      continue;
    }
    if (!cloneIsArrayBuffer(t)) throw cloneDataCloneError("Found invalid value in transferList.");
    if (seen.has(t)) throw cloneDataCloneError("ArrayBuffer at index 1 is a duplicate of an earlier ArrayBuffer. Duplicate array buffers are not allowed.");
    if (t.detached) throw cloneDataCloneError("An ArrayBuffer is detached and could not be cloned.");
    seen.set(t, t.slice(0));
  }
  const clone = (v) => {
    if (typeof v === "function") throw cloneDataCloneError(`${Function.prototype.toString.call(v)} could not be cloned.`);
    if (typeof v === "symbol") throw cloneDataCloneError(`${String(v)} could not be cloned.`);
    if (v === null || typeof v !== "object") return v;
    if (seen.has(v)) return seen.get(v);
    if (globalThis.__lumenPortClone?.isPort(v)) throw cloneDataCloneError("MessagePort must be listed in transferList.");
    if (v instanceof Blob && globalThis.__lumenIsFileBackedBlob(v)) {
      throw codedError(Error, "ERR_INVALID_STATE", "Invalid state: File-backed Blobs are not cloneable");
    }
    let out;
    if (globalThis.__cloneTransfer && (out=globalThis.__cloneTransfer.cloneShared(v)) !== undefined) {
      seen.set(v,out);
      return out;
    }
    if (cloneBrand(Date.prototype.getTime, v)) {
      out = new Date(Date.prototype.getTime.call(v));
    } else if (cloneBrand(Boolean.prototype.valueOf, v)) {
      out = Object(Boolean.prototype.valueOf.call(v));
    } else if (cloneBrand(Number.prototype.valueOf, v)) {
      out = Object(Number.prototype.valueOf.call(v));
    } else if (cloneBrand(String.prototype.valueOf, v)) {
      out = Object(String.prototype.valueOf.call(v));
    } else if (cloneBrand(BigInt.prototype.valueOf, v)) {
      out = Object(BigInt.prototype.valueOf.call(v));
    } else if (cloneBrand(Symbol.prototype.valueOf, v)) {
      throw cloneDataCloneError("Symbol object could not be cloned.");
    } else if (v instanceof RegExp) {
      out = new RegExp(v.source, v.flags);
    } else if (v instanceof Promise || v instanceof WeakMap || v instanceof WeakSet || v instanceof WeakRef) {
      throw cloneDataCloneError(`#<${v.constructor && v.constructor.name || "Object"}> could not be cloned.`);
    } else if (cloneIsArrayBuffer(v)) {
      if (v.detached) throw cloneDataCloneError("An ArrayBuffer is detached and could not be cloned.");
      out = v.slice(0);
    } else if (cloneTypedArrayTag.call(v) !== undefined) {
      const Ctor = globalThis[cloneTypedArrayTag.call(v)];
      out = new Ctor(clone(v.buffer), v.byteOffset, v.length);
    } else if (v instanceof DataView) {
      out = new DataView(clone(v.buffer), v.byteOffset, v.byteLength);
    } else if (v instanceof Map) {
      out = new Map();
      seen.set(v, out);
      for (const [k, val] of v) out.set(clone(k), clone(val));
      return out;
    } else if (v instanceof Set) {
      out = new Set();
      seen.set(v, out);
      for (const item of v) out.add(clone(item));
      return out;
    } else if (typeof v[Symbol.for("lumen.transferable.clone")] === "function") {
      const { data, deserializeInfo } = v[Symbol.for("lumen.transferable.clone")]();
      out = globalThis.__reviveHostObject(deserializeInfo)(clone(data));
    } else if (v instanceof Error) {
      let name = v.name;
      if (!CLONE_ERROR_NAMES.has(name)) name = "Error";
      const Ctor = globalThis[name];
      out = Object.create(Ctor.prototype);
      seen.set(v, out);
      const msg = Object.getOwnPropertyDescriptor(v, "message");
      if (msg && "value" in msg) {
        Object.defineProperty(out, "message", { value: String(msg.value), writable: true, configurable: true, enumerable: false });
      }
      if (typeof v.stack === "string") {
        Object.defineProperty(out, "stack", { value: v.stack, writable: true, configurable: true, enumerable: false });
      }
      if (Object.prototype.hasOwnProperty.call(v, "cause")) {
        Object.defineProperty(out, "cause", { value: clone(v.cause), writable: true, configurable: true, enumerable: false });
      }
      return out;
    } else if (Array.isArray(v)) {
      out = new Array(v.length);
      seen.set(v, out);
      for (const k of Object.keys(v)) out[k] = clone(v[k]);
      return out;
    } else {
      out = {};
      seen.set(v, out);
      for (const k of Object.keys(v)) out[k] = clone(v[k]);
      return out;
    }
    seen.set(v, out);
    return out;
  };
  const result = clone(value);
  for (const t of transfer) if (!(t instanceof AbortSignal)) t.transfer(); // detach the originals
  return result;
}

globalThis.structuredClone = structuredClone;
