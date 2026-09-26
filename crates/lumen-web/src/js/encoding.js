// TextEncoder/TextDecoder over the native utf-8 ops, base64 globals, structuredClone.

class TextEncoder {
  get encoding() {
    return "utf-8";
  }
  encode(input = "") {
    return __encoding.encode(String(input));
  }
  // Encoding §8.1.2: as much of `source` as fits, whole UTF-8 sequences only; `read` counts
  // UTF-16 code units (a lone surrogate encodes as U+FFFD).
  encodeInto(source, destination) {
    if (!(destination instanceof Uint8Array)) {
      throw new TypeError("TextEncoder.encodeInto: destination must be a Uint8Array");
    }
    const s = String(source);
    const n = destination.length;
    if (s.length <= n) {
      const b = __encoding.encode(s);
      if (b.length <= n) {
        destination.set(b);
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

class TextDecoder {
  constructor(label = "utf-8", options = {}) {
    const l = String(label).toLowerCase();
    if (l !== "utf-8" && l !== "utf8" && l !== "unicode-1-1-utf-8") {
      throw new RangeError(`TextDecoder: unsupported encoding '${label}' (utf-8 only for now)`);
    }
    options = options && typeof options === "object" ? options : {};
    this.encoding = "utf-8";
    this.fatal = !!options.fatal;
    this.ignoreBOM = !!options.ignoreBOM;
    this._pending = null; // an incomplete trailing sequence held back by a streaming decode
    this._bomSeen = false; // the stream's first bytes have been decoded (BOM handled)
  }
  decode(input, options) {
    const stream = !!(options && typeof options === "object" && options.stream);
    let bytes;
    if (input === undefined) bytes = new Uint8Array(0);
    else if (input instanceof ArrayBuffer) bytes = new Uint8Array(input);
    else if (ArrayBuffer.isView(input)) bytes = new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
    else bytes = input;
    if (this._pending !== null) {
      const joined = new Uint8Array(this._pending.length + bytes.length);
      joined.set(this._pending);
      joined.set(bytes, this._pending.length);
      bytes = joined;
      this._pending = null;
    }
    if (stream) {
      const keep = incompleteUtf8Tail(bytes);
      if (keep > 0) {
        this._pending = bytes.slice(bytes.length - keep);
        bytes = bytes.subarray(0, bytes.length - keep);
      }
    }
    let s = bytes.length === 0 ? "" : __encoding.decode(bytes, this.fatal);
    if (!this._bomSeen && s.length !== 0) {
      if (!this.ignoreBOM && s.charCodeAt(0) === 0xfeff) s = s.slice(1);
      this._bomSeen = true;
    }
    // A non-streaming call ends the stream: the next decode starts a new one.
    if (!stream) this._bomSeen = false;
    return s;
  }
}

// The length of a trailing UTF-8 sequence that is a valid but incomplete prefix (0 if none).
function incompleteUtf8Tail(bytes) {
  const n = bytes.length;
  for (let k = 1; k <= 3 && k <= n; k++) {
    const lead = bytes[n - k];
    if (lead >= 0x80 && lead <= 0xbf) continue; // a continuation byte: look further back
    let need;
    if (lead >= 0xc2 && lead <= 0xdf) need = 2;
    else if (lead >= 0xe0 && lead <= 0xef) need = 3;
    else if (lead >= 0xf0 && lead <= 0xf4) need = 4;
    else return 0;
    if (need <= k) return 0;
    if (k >= 2) {
      const second = bytes[n - k + 1];
      const lo = lead === 0xe0 ? 0xa0 : lead === 0xf0 ? 0x90 : 0x80;
      const hi = lead === 0xed ? 0x9f : lead === 0xf4 ? 0x8f : 0xbf;
      if (second < lo || second > hi) return 0;
    }
    return k;
  }
  return 0;
}

const B64_ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

function btoa(data) {
  const s = String(data);
  let out = "";
  for (let i = 0; i < s.length; i += 3) {
    const cs = [s.charCodeAt(i), s.charCodeAt(i + 1), s.charCodeAt(i + 2)];
    if (cs[0] > 255 || cs[1] > 255 || cs[2] > 255) {
      throw new DOMException("btoa: character beyond latin1 range", "InvalidCharacterError");
    }
    const n = (cs[0] << 16) | ((cs[1] || 0) << 8) | (cs[2] || 0);
    out += B64_ALPHABET[(n >> 18) & 63];
    out += B64_ALPHABET[(n >> 12) & 63];
    out += i + 1 < s.length ? B64_ALPHABET[(n >> 6) & 63] : "=";
    out += i + 2 < s.length ? B64_ALPHABET[n & 63] : "=";
  }
  return out;
}

function atob(data) {
  let s = String(data).replace(/[\t\n\f\r ]/g, "");
  if (s.length % 4 === 0) s = s.replace(/==?$/, "");
  if (s.length % 4 === 1 || /[^A-Za-z0-9+/]/.test(s)) {
    throw new DOMException("atob: invalid base64", "InvalidCharacterError");
  }
  let out = "";
  for (let i = 0; i < s.length; i += 4) {
    const bits = [0, 1, 2, 3].map((j) =>
      j + i < s.length ? B64_ALPHABET.indexOf(s[i + j]) : 0
    );
    const n = (bits[0] << 18) | (bits[1] << 12) | (bits[2] << 6) | bits[3];
    out += String.fromCharCode((n >> 16) & 255);
    if (i + 2 < s.length) out += String.fromCharCode((n >> 8) & 255);
    if (i + 3 < s.length) out += String.fromCharCode(n & 255);
  }
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
  if (arguments.length === 0) throw new TypeError("The \"value\" argument must be specified");
  let transfer = [];
  if (options !== undefined && options !== null) {
    if (typeof options !== "object" && typeof options !== "function") {
      throw new TypeError("The \"options\" argument must be of type object.");
    }
    if (options.transfer !== undefined) {
      if (options.transfer === null || typeof options.transfer[Symbol.iterator] !== "function") {
        throw new TypeError("The \"options.transfer\" property must be iterable.");
      }
      transfer = [...options.transfer];
    }
  }
  const seen = new Map();
  for (const t of transfer) {
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
    let out;
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
  for (const t of transfer) t.transfer(); // detach the originals
  return result;
}

globalThis.TextEncoder = TextEncoder;
globalThis.TextDecoder = TextDecoder;
globalThis.btoa = btoa;
globalThis.atob = atob;
globalThis.structuredClone = structuredClone;
