// TextEncoder/TextDecoder: shared native WHATWG codecs, base64 globals, structuredClone.

const encoderBrand = new WeakSet();
const encodeNativeText = __encoding.encode;
const encodingToString = String;
const encoderArrayPrototype = Object.getPrototypeOf(Uint8Array.prototype);
const encoderArrayBrand = Function.prototype.call.bind(Object.getOwnPropertyDescriptor(encoderArrayPrototype, Symbol.toStringTag).get);
const encoderArrayLength = Function.prototype.call.bind(Object.getOwnPropertyDescriptor(encoderArrayPrototype, 'length').get);
const encoderArraySet = Function.prototype.call.bind(Uint8Array.prototype.set);
function requireEncoder(value) {
  if (!encoderBrand.has(value)) throw new TypeError('TextEncoder receiver has an invalid brand');
}
function encodingString(value) {
  if (typeof value === 'symbol') throw new TypeError('Cannot convert a Symbol value to a string');
  return encodingToString(value);
}

class TextEncoder {
  constructor() { encoderBrand.add(this); }
  get encoding() {
    requireEncoder(this);
    return "utf-8";
  }
  encode(input = "") {
    requireEncoder(this);
    return encodeNativeText(encodingString(input));
  }
  // Encoding §8.1.2: as much of `source` as fits, whole UTF-8 sequences only; `read` counts
  // UTF-16 code units (a lone surrogate encodes as U+FFFD).
  encodeInto(source, destination) {
    requireEncoder(this);
    const s = encodingString(source);
    if (encoderArrayBrand(destination) !== 'Uint8Array') {
      throw new TypeError("TextEncoder.encodeInto: destination must be a Uint8Array");
    }
    const n = encoderArrayLength(destination);
    if (n === 0) return { read: 0, written: 0 };
    if (s.length <= n) {
      const b = encodeNativeText(s);
      if (b.length <= n) {
        encoderArraySet(destination, b);
        return { read: s.length, written: b.length };
      }
    }
    let read = 0;
    let written = 0;
    while (read < s.length) {
      let c = s.charCodeAt(read);
      let units = 1;
      if (c >= 0xd800 && c <= 0xdfff) {
        const d = read + 1 < s.length ? s.charCodeAt(read + 1) : 0;
        if (c <= 0xdbff && d >= 0xdc00 && d <= 0xdfff) {
          c = 0x10000 + ((c - 0xd800) << 10) + (d - 0xdc00);
          units = 2;
        } else {
          c = 0xfffd;
        }
      }
      if (c < 0x80) {
        if (written + 1 > n) break;
        destination[written++] = c;
      } else if (c < 0x800) {
        if (written + 2 > n) break;
        destination[written++] = 0xc0 | (c >> 6);
        destination[written++] = 0x80 | (c & 0x3f);
      } else if (c < 0x10000) {
        if (written + 3 > n) break;
        destination[written++] = 0xe0 | (c >> 12);
        destination[written++] = 0x80 | ((c >> 6) & 0x3f);
        destination[written++] = 0x80 | (c & 0x3f);
      } else {
        if (written + 4 > n) break;
        destination[written++] = 0xf0 | (c >> 18);
        destination[written++] = 0x80 | ((c >> 12) & 0x3f);
        destination[written++] = 0x80 | ((c >> 6) & 0x3f);
        destination[written++] = 0x80 | (c & 0x3f);
      }
      read += units;
    }
    return { read, written };
  }
}
Object.defineProperties(TextEncoder.prototype, {
  encoding: { enumerable: true },
  encode: { enumerable: true },
  encodeInto: { enumerable: true },
  [Symbol.toStringTag]: { value: 'TextEncoder', configurable: true },
});

const NativeTextDecoder = __lumenEncoding.Decoder;
const decodeNativeBytes = Function.prototype.call.bind(NativeTextDecoder.prototype.decode);
const nativeEncodingLabel = __lumenEncoding.label;
const decoderBytes = Uint8Array;
const decoderIsView = ArrayBuffer.isView;
const decoderGetter = (prototype, name) => Function.prototype.call.bind(Object.getOwnPropertyDescriptor(prototype, name).get);
const decoderBufferLength = decoderGetter(ArrayBuffer.prototype, 'byteLength');
const decoderSharedLength = typeof SharedArrayBuffer === 'undefined' ? null : decoderGetter(SharedArrayBuffer.prototype, 'byteLength');
const decoderViewBuffer = decoderGetter(encoderArrayPrototype, 'buffer');
const decoderViewOffset = decoderGetter(encoderArrayPrototype, 'byteOffset');
const decoderViewLength = decoderGetter(encoderArrayPrototype, 'byteLength');
const decoderDataBuffer = decoderGetter(DataView.prototype, 'buffer');
const decoderDataOffset = decoderGetter(DataView.prototype, 'byteOffset');
const decoderDataLength = decoderGetter(DataView.prototype, 'byteLength');

function decoderBufferSource(input) {
  if (input === undefined) return new decoderBytes(0);
  if (decoderIsView(input)) {
    if (encoderArrayBrand(input) !== undefined) {
      return new decoderBytes(decoderViewBuffer(input), decoderViewOffset(input), decoderViewLength(input));
    }
    return new decoderBytes(decoderDataBuffer(input), decoderDataOffset(input), decoderDataLength(input));
  }
  let length;
  try { length = decoderBufferLength(input); }
  catch {
    if (decoderSharedLength === null) throw new TypeError('TextDecoder input must be a BufferSource');
    length = decoderSharedLength(input);
  }
  return new decoderBytes(input, 0, length);
}

function codedError(Ctor, code, message) {
  const err = new Ctor(message);
  err.code = code;
  if (typeof err.stack === "string") {
    err.stack = `${err.name} [${code}]${err.stack.slice(err.name.length)}`;
  }
  return err;
}

function receivedSuffix(value) {
  if (value === null || value === undefined) return ` Received ${value}`;
  if (typeof value === "function") return ` Received function ${value.name}`;
  if (typeof value === "object") {
    const name = value.constructor?.name;
    return name ? ` Received an instance of ${name}` : " Received [Object: null prototype] {}";
  }
  let shown = typeof value === "string" ? `'${value.length > 28 ? value.slice(0, 25) + "..." : value}'` : String(value);
  if (typeof value === "bigint") shown += "n";
  return ` Received type ${typeof value} (${shown})`;
}

const decoderState = new WeakMap();

function decoderOf(self) {
  const state = decoderState.get(self);
  if (state === undefined) throw codedError(TypeError, "ERR_INVALID_THIS", 'Value of "this" must be of type TextDecoder');
  return state;
}

function fatalDecodeError(encoding) {
  return codedError(TypeError, "ERR_ENCODING_INVALID_ENCODED_DATA", `The encoded data was not valid for encoding ${encoding}`);
}

class TextDecoder {
  constructor(label = "utf-8", options = {}) {
    const labelText = encodingString(label);
    const encoding = nativeEncodingLabel(labelText);
    if (encoding === undefined || encoding === null) {
      throw codedError(RangeError, "ERR_ENCODING_NOT_SUPPORTED", `The "${labelText}" encoding is not supported`);
    }
    if (options !== null && typeof options !== "object" && typeof options !== "function") {
      throw codedError(TypeError, "ERR_INVALID_ARG_TYPE", `The "options" argument must be of type object.${receivedSuffix(options)}`);
    }
    options = options ?? {};
    const fatal = !!options.fatal;
    const ignoreBOM = !!options.ignoreBOM;
    decoderState.set(this, {
      encoding, fatal, ignoreBOM,
      native: new NativeTextDecoder(encoding, fatal, ignoreBOM),
    });
  }
  get encoding() { return decoderOf(this).encoding; }
  get fatal() { return decoderOf(this).fatal; }
  get ignoreBOM() { return decoderOf(this).ignoreBOM; }
  [Symbol.for("nodejs.util.inspect.custom")](depth, options, inspect) {
    const state = decoderOf(this);
    if (typeof depth === "number" && depth < 0) return this;
    const shown = { encoding: state.encoding, fatal: state.fatal, ignoreBOM: state.ignoreBOM };
    return `${this.constructor.name} ${inspect ? inspect(shown, options) : JSON.stringify(shown)}`;
  }
  decode(input = undefined, options = {}) {
    const state = decoderOf(this);
    const bytes = decoderBufferSource(input);
    if (options !== undefined && options !== null && typeof options !== "object" && typeof options !== "function") {
      throw codedError(TypeError, "ERR_INVALID_ARG_TYPE", `The "options" argument must be of type object.${receivedSuffix(options)}`);
    }
    const stream = !!(options && options.stream);
    try {
      return decodeNativeBytes(state.native, bytes, stream);
    } catch (error) {
      if (state.fatal && error instanceof TypeError) throw fatalDecodeError(state.encoding);
      throw error;
    }
  }
}
Object.defineProperties(TextDecoder.prototype, {
  encoding: { enumerable: true },
  fatal: { enumerable: true },
  ignoreBOM: { enumerable: true },
  decode: { enumerable: true },
  [Symbol.toStringTag]: { value: "TextDecoder", configurable: true },
});

function btoa(data) {
  if (arguments.length === 0) throw codedError(TypeError, "ERR_MISSING_ARGS", 'The "input" argument must be specified');
  const out = __encoding.btoa(data);
  if (out === null) throw new DOMException("btoa: character beyond latin1 range", "InvalidCharacterError");
  return out;
}

function atob(data) {
  if (arguments.length === 0) throw codedError(TypeError, "ERR_MISSING_ARGS", 'The "input" argument must be specified');
  const out = __encoding.atob(data);
  if (out === null) throw new DOMException("atob: invalid base64", "InvalidCharacterError");
  return out;
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
    if (v instanceof Blob && v[kBlobFile]) {
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

globalThis.TextEncoder = TextEncoder;
globalThis.TextDecoder = TextDecoder;
globalThis.btoa = btoa;
globalThis.atob = atob;
globalThis.structuredClone = structuredClone;
