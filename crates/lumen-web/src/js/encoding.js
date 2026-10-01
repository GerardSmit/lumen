// TextEncoder/TextDecoder: native UTF-8 and WHATWG Windows-1252, base64 globals, structuredClone.

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

// Encoding Standard labels and single-byte index. All 256 Windows-1252 bytes have
// mappings, including C1 control bytes 81/8D/8F/90/9D; fatal mode does not reject them.
const WINDOWS_1252_LABELS = new Set([
  "ansi_x3.4-1968", "ascii", "cp1252", "cp819", "csisolatin1", "ibm819", "iso-8859-1",
  "iso-ir-100", "iso8859-1", "iso88591", "iso_8859-1", "iso_8859-1:1987", "l1",
  "latin1", "us-ascii", "windows-1252", "x-cp1252",
]);
const WINDOWS_1252_C1 = [
  0x20ac, 0x81, 0x201a, 0x192, 0x201e, 0x2026, 0x2020, 0x2021,
  0x2c6, 0x2030, 0x160, 0x2039, 0x152, 0x8d, 0x17d, 0x8f,
  0x90, 0x2018, 0x2019, 0x201c, 0x201d, 0x2022, 0x2013, 0x2014,
  0x2dc, 0x2122, 0x161, 0x203a, 0x153, 0x9d, 0x17e, 0x178,
];
const UTF8_LABELS = new Set([
  "utf-8", "utf8", "unicode-1-1-utf-8", "unicode11utf8", "unicode20utf8", "x-unicode20utf8",
]);
const UTF16LE_LABELS = new Set(["csunicode", "iso-10646-ucs-2", "ucs-2", "unicode", "unicodefeff", "utf-16", "utf-16le"]);
const UTF16BE_LABELS = new Set(["unicodefffe", "utf-16be"]);

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

function unitsToString(units) {
  let out = "";
  for (let i = 0; i < units.length; i += 4096) {
    out += String.fromCharCode.apply(null, units.slice(i, i + 4096));
  }
  return out;
}

class TextDecoder {
  constructor(label = "utf-8", options = {}) {
    const l = String(label).replace(/^[\x09\x0a\x0c\x0d\x20]+|[\x09\x0a\x0c\x0d\x20]+$/g, "").toLowerCase();
    let encoding;
    if (UTF8_LABELS.has(l)) encoding = "utf-8";
    else if (UTF16LE_LABELS.has(l)) encoding = "utf-16le";
    else if (UTF16BE_LABELS.has(l)) encoding = "utf-16be";
    else if (WINDOWS_1252_LABELS.has(l)) encoding = "windows-1252";
    else throw codedError(RangeError, "ERR_ENCODING_NOT_SUPPORTED", `The "${label}" encoding is not supported`);
    if (options !== null && typeof options !== "object" && typeof options !== "function") {
      throw codedError(TypeError, "ERR_INVALID_ARG_TYPE", `The "options" argument must be of type object.${receivedSuffix(options)}`);
    }
    options = options ?? {};
    decoderState.set(this, {
      encoding,
      fatal: !!options.fatal,
      ignoreBOM: !!options.ignoreBOM,
      pending: null, // an incomplete trailing sequence held back by a streaming decode
      lead: -1, // a UTF-16 high surrogate awaiting its pair
      bomSeen: false, // the stream's first bytes have been decoded (BOM handled)
    });
  }
  get encoding() {
    return decoderOf(this).encoding;
  }
  get fatal() {
    return decoderOf(this).fatal;
  }
  get ignoreBOM() {
    return decoderOf(this).ignoreBOM;
  }
  [Symbol.for("nodejs.util.inspect.custom")](depth, options, inspect) {
    const state = decoderOf(this);
    if (typeof depth === "number" && depth < 0) return this;
    const shown = { encoding: state.encoding, fatal: state.fatal, ignoreBOM: state.ignoreBOM };
    return `${this.constructor.name} ${inspect ? inspect(shown, options) : JSON.stringify(shown)}`;
  }
  decode(input, options) {
    const state = decoderOf(this);
    if (options !== undefined && options !== null && typeof options !== "object" && typeof options !== "function") {
      throw codedError(TypeError, "ERR_INVALID_ARG_TYPE", `The "options" argument must be of type object.${receivedSuffix(options)}`);
    }
    const stream = !!(options && options.stream);
    let bytes;
    if (input === undefined) bytes = new Uint8Array(0);
    else if (input instanceof ArrayBuffer || (typeof SharedArrayBuffer !== "undefined" && input instanceof SharedArrayBuffer)) bytes = new Uint8Array(input);
    else if (ArrayBuffer.isView(input)) bytes = new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
    else {
      throw codedError(TypeError, "ERR_INVALID_ARG_TYPE",
        `The "input" argument must be an instance of ArrayBuffer or ArrayBufferView.${receivedSuffix(input)}`);
    }
    const { encoding } = state;
    if (encoding === "windows-1252") {
      let result = "";
      for (let j = 0; j < bytes.length; j++) {
        const byte = bytes[j];
        result += String.fromCharCode(byte >= 0x80 && byte <= 0x9f ? WINDOWS_1252_C1[byte - 0x80] : byte);
      }
      // A single-byte encoding has no pending multibyte tail or BOM to remove.
      return result;
    }
    if (state.pending !== null) {
      const joined = new Uint8Array(state.pending.length + bytes.length);
      joined.set(state.pending);
      joined.set(bytes, state.pending.length);
      bytes = joined;
      state.pending = null;
    }
    let s;
    if (encoding === "utf-8") {
      if (stream) {
        const keep = incompleteUtf8Tail(bytes);
        if (keep > 0) {
          state.pending = bytes.slice(bytes.length - keep);
          bytes = bytes.subarray(0, bytes.length - keep);
        }
      }
      try {
        s = bytes.length === 0 ? "" : __encoding.decode(bytes, state.fatal);
      } catch (e) {
        state.pending = null;
        state.bomSeen = false;
        throw state.fatal ? fatalDecodeError(encoding) : e;
      }
    } else {
      s = this._decodeUtf16(state, bytes, stream);
    }
    if (!state.bomSeen && s.length !== 0) {
      if (!state.ignoreBOM && s.charCodeAt(0) === 0xfeff) s = s.slice(1);
      state.bomSeen = true;
    }
    // A non-streaming call ends the stream: the next decode starts a new one.
    if (!stream) state.bomSeen = false;
    return s;
  }
  _decodeUtf16(state, bytes, stream) {
    const be = state.encoding === "utf-16be";
    const units = [];
    const bad = () => {
      if (state.fatal) {
        state.pending = null;
        state.lead = -1;
        state.bomSeen = false;
        throw fatalDecodeError(state.encoding);
      }
      units.push(0xfffd);
    };
    const even = bytes.length & ~1;
    for (let k = 0; k < even; k += 2) {
      const u = be ? (bytes[k] << 8) | bytes[k + 1] : (bytes[k + 1] << 8) | bytes[k];
      if (state.lead !== -1) {
        if (u >= 0xdc00 && u <= 0xdfff) {
          units.push(state.lead, u);
          state.lead = -1;
          continue;
        }
        state.lead = -1;
        bad();
      }
      if (u >= 0xd800 && u <= 0xdbff) state.lead = u;
      else if (u >= 0xdc00 && u <= 0xdfff) bad();
      else units.push(u);
    }
    if (stream) {
      if (even !== bytes.length) state.pending = bytes.slice(even);
    } else {
      if (state.lead !== -1) {
        state.lead = -1;
        bad();
      }
      if (even !== bytes.length) bad();
    }
    return unitsToString(units);
  }
}
Object.defineProperties(TextDecoder.prototype, {
  encoding: { enumerable: true },
  fatal: { enumerable: true },
  ignoreBOM: { enumerable: true },
  decode: { enumerable: true },
  [Symbol.toStringTag]: { value: "TextDecoder", configurable: true },
});

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
