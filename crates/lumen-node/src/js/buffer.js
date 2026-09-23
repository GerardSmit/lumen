// Buffer — a Uint8Array subclass with Node's codec + accessor surface (the common slice of
// it). Encodings: utf8/utf-8, hex, base64, base64url, latin1/binary, ascii.

const B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

function normEncoding(enc) {
  enc = (enc || "utf8").toLowerCase();
  if (enc === "utf-8") return "utf8";
  if (enc === "ucs2" || enc === "ucs-2") return "utf16le";
  if (enc === "binary") return "latin1";
  return enc;
}

function bytesFromString(str, enc) {
  enc = normEncoding(enc);
  switch (enc) {
    case "utf8":
      return new TextEncoder().encode(str);
    case "ascii":
    case "latin1": {
      const out = new Uint8Array(str.length);
      for (let i = 0; i < str.length; i++) out[i] = str.charCodeAt(i) & 0xff;
      return out;
    }
    case "hex": {
      const clean = str.replace(/[^0-9a-fA-F]/g, "");
      const n = clean.length >> 1;
      const out = new Uint8Array(n);
      for (let i = 0; i < n; i++) out[i] = parseInt(clean.substr(i * 2, 2), 16);
      return out;
    }
    case "base64":
    case "base64url": {
      let s = str.replace(/[-_]/g, (c) => (c === "-" ? "+" : "/")).replace(/[^A-Za-z0-9+/]/g, "");
      const pad = s.length % 4;
      const bytes = [];
      for (let i = 0; i < s.length; i += 4) {
        const q = [0, 1, 2, 3].map((j) => (i + j < s.length ? B64.indexOf(s[i + j]) : 0));
        const n = (q[0] << 18) | (q[1] << 12) | (q[2] << 6) | q[3];
        bytes.push((n >> 16) & 0xff);
        if (i + 2 < s.length) bytes.push((n >> 8) & 0xff);
        if (i + 3 < s.length) bytes.push(n & 0xff);
      }
      void pad;
      return new Uint8Array(bytes);
    }
    case "utf16le": {
      const out = new Uint8Array(str.length * 2);
      for (let i = 0; i < str.length; i++) {
        const c = str.charCodeAt(i);
        out[i * 2] = c & 0xff;
        out[i * 2 + 1] = c >> 8;
      }
      return out;
    }
    default:
      throw new TypeError(`Unknown encoding: ${enc}`);
  }
}

function stringFromBytes(bytes, enc, start, end) {
  start = start || 0;
  end = end === undefined ? bytes.length : end;
  const view = bytes.subarray(start, end);
  enc = normEncoding(enc);
  switch (enc) {
    case "utf8":
      return new TextDecoder().decode(view);
    case "ascii": {
      let s = "";
      for (const b of view) s += String.fromCharCode(b & 0x7f);
      return s;
    }
    case "latin1": {
      let s = "";
      for (const b of view) s += String.fromCharCode(b);
      return s;
    }
    case "hex": {
      let s = "";
      for (const b of view) s += b.toString(16).padStart(2, "0");
      return s;
    }
    case "base64":
    case "base64url": {
      let s = "";
      for (let i = 0; i < view.length; i += 3) {
        const n = (view[i] << 16) | ((view[i + 1] || 0) << 8) | (view[i + 2] || 0);
        s += B64[(n >> 18) & 63] + B64[(n >> 12) & 63];
        s += i + 1 < view.length ? B64[(n >> 6) & 63] : "=";
        s += i + 2 < view.length ? B64[n & 63] : "=";
      }
      if (enc === "base64url") s = s.replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
      return s;
    }
    case "utf16le": {
      let s = "";
      for (let i = 0; i + 1 < view.length; i += 2) s += String.fromCharCode(view[i] | (view[i + 1] << 8));
      return s;
    }
    default:
      throw new TypeError(`Unknown encoding: ${enc}`);
  }
}

class Buffer extends Uint8Array {
  static from(value, encOrOffset, length) {
    if (typeof value === "string") return new Buffer(bytesFromString(value, encOrOffset));
    // A view over the same memory, as in Node (the Uint8Array constructor's ArrayBuffer form).
    if (value instanceof ArrayBuffer || (typeof SharedArrayBuffer === "function" && value instanceof SharedArrayBuffer)) {
      return length === undefined ? new Buffer(value, encOrOffset || 0) : new Buffer(value, encOrOffset || 0, length);
    }
    if (ArrayBuffer.isView(value)) return new Buffer(new Uint8Array(value));
    if (Array.isArray(value) || (value && typeof value.length === "number")) {
      const b = new Buffer(value.length);
      for (let i = 0; i < value.length; i++) b[i] = value[i] & 0xff;
      return b;
    }
    throw new TypeError("Buffer.from: unsupported value");
  }
  static alloc(size, fill, enc) {
    const b = new Buffer(size);
    if (fill !== undefined && fill !== 0) {
      if (typeof fill === "string") {
        const src = bytesFromString(fill, enc);
        for (let i = 0; i < size; i++) b[i] = src.length ? src[i % src.length] : 0;
      } else {
        b.fill(fill & 0xff);
      }
    }
    return b;
  }
  static allocUnsafe(size) {
    return new Buffer(size);
  }
  // Node distinguishes allocUnsafeSlow (un-pooled) from allocUnsafe; we don't pool, so they are
  // the same. Its presence also matters for feature-detection: safe-buffer only passes the real
  // Buffer through when all four of from/alloc/allocUnsafe/allocUnsafeSlow exist.
  static allocUnsafeSlow(size) {
    return new Buffer(size);
  }
  static isBuffer(x) {
    return x instanceof Buffer;
  }
  static isEncoding(enc) {
    return typeof enc === "string" && ["utf8", "utf-8", "hex", "base64", "base64url", "latin1", "binary", "ascii", "ucs2", "ucs-2", "utf16le", "utf-16le"].includes(enc.toLowerCase());
  }
  static compare(a, b) {
    if (!Buffer.isBuffer(a) || !Buffer.isBuffer(b)) throw new TypeError("Arguments must be Buffers");
    const len = Math.min(a.length, b.length);
    for (let i = 0; i < len; i++) {
      if (a[i] !== b[i]) return a[i] < b[i] ? -1 : 1;
    }
    return a.length === b.length ? 0 : a.length < b.length ? -1 : 1;
  }
  static byteLength(str, enc) {
    if (typeof str !== "string") return str.byteLength ?? str.length ?? 0;
    return bytesFromString(str, enc).length;
  }
  static concat(list, totalLength) {
    if (totalLength === undefined) {
      totalLength = 0;
      for (const b of list) totalLength += b.length;
    }
    const out = new Buffer(totalLength);
    let offset = 0;
    for (const b of list) {
      if (offset >= totalLength) break;
      out.set(b.subarray(0, Math.min(b.length, totalLength - offset)), offset);
      offset += b.length;
    }
    return out;
  }
  toString(enc, start, end) {
    return stringFromBytes(this, enc, start, end);
  }
  write(string, offset, length, enc) {
    if (typeof offset === "string") {
      enc = offset;
      offset = 0;
      length = this.length;
    } else if (typeof length === "string") {
      enc = length;
      length = this.length - offset;
    }
    offset = offset || 0;
    const src = bytesFromString(string, enc);
    const n = Math.min(src.length, length === undefined ? this.length - offset : length, this.length - offset);
    this.set(src.subarray(0, n), offset);
    return n;
  }
  slice(start, end) {
    return new Buffer(this.subarray(start, end));
  }
  equals(other) {
    if (!(other instanceof Uint8Array) || other.length !== this.length) return false;
    for (let i = 0; i < this.length; i++) if (this[i] !== other[i]) return false;
    return true;
  }
  compare(other) {
    const n = Math.min(this.length, other.length);
    for (let i = 0; i < n; i++) {
      if (this[i] !== other[i]) return this[i] < other[i] ? -1 : 1;
    }
    return this.length === other.length ? 0 : this.length < other.length ? -1 : 1;
  }
  // Node's Buffer search takes a string (encoded), a byte sequence or a single byte;
  // Uint8Array's own indexOf only knows the last, which every header/frame parser trips over.
  _needle(value, encoding) {
    if (typeof value === "string") return bytesFromString(value, encoding);
    if (typeof value === "number") return new Uint8Array([value & 0xff]);
    if (value instanceof Uint8Array) return value;
    throw new TypeError('The "value" argument must be one of type number or string or an instance of Buffer or Uint8Array.');
  }
  indexOf(value, byteOffset, encoding) {
    if (typeof byteOffset === "string") { encoding = byteOffset; byteOffset = 0; }
    const needle = this._needle(value, encoding);
    let from = byteOffset === undefined ? 0 : Math.trunc(Number(byteOffset)) || 0;
    if (from < 0) from = Math.max(0, this.length + from);
    if (needle.length === 0) return Math.min(from, this.length);
    const last = this.length - needle.length;
    outer: for (let i = from; i <= last; i++) {
      for (let j = 0; j < needle.length; j++) if (this[i + j] !== needle[j]) continue outer;
      return i;
    }
    return -1;
  }
  lastIndexOf(value, byteOffset, encoding) {
    if (typeof byteOffset === "string") { encoding = byteOffset; byteOffset = undefined; }
    const needle = this._needle(value, encoding);
    let from = byteOffset === undefined ? this.length : Math.trunc(Number(byteOffset));
    if (Number.isNaN(from)) from = this.length;
    if (from < 0) from = this.length + from;
    from = Math.min(from, this.length - needle.length);
    if (needle.length === 0) return Math.max(0, Math.min(from, this.length));
    outer: for (let i = from; i >= 0; i--) {
      for (let j = 0; j < needle.length; j++) if (this[i + j] !== needle[j]) continue outer;
      return i;
    }
    return -1;
  }
  includes(value, byteOffset, encoding) {
    return this.indexOf(value, byteOffset, encoding) !== -1;
  }
  toJSON() {
    return { type: "Buffer", data: Array.from(this) };
  }
  copy(target, targetStart = 0, sourceStart = 0, sourceEnd = this.length) {
    if (!(target instanceof Uint8Array)) throw new TypeError('The "target" argument must be an instance of Buffer or Uint8Array.');
    targetStart = Math.max(0, Math.trunc(targetStart) || 0);
    sourceStart = Math.max(0, Math.trunc(sourceStart) || 0);
    sourceEnd = Math.min(this.length, Math.trunc(sourceEnd) || 0);
    if (sourceStart >= sourceEnd || targetStart >= target.length) return 0;
    const n = Math.min(sourceEnd - sourceStart, target.length - targetStart);
    target.set(this.subarray(sourceStart, sourceStart + n), targetStart);
    return n;
  }
  swap16() { return swapBytes(this, 2); }
  swap32() { return swapBytes(this, 4); }
  swap64() { return swapBytes(this, 8); }
  // Node's `parent`/`offset` legacy aliases for the backing store.
  get parent() { return this.buffer; }
  get offset() { return this.byteOffset; }
}

// ---- numeric accessors ----------------------------------------------------------------------
// The read*/write* family over a DataView, with Node's argument checks: an `offset` that would
// run past the end throws ERR_OUT_OF_RANGE (not a silent NaN), and a value outside the type's
// range on write does too.

function outOfRange(name, range, actual) {
  const err = new RangeError(`The value of "${name}" is out of range. It must be ${range}. Received ${typeof actual === "bigint" ? actual + "n" : actual}`);
  err.code = "ERR_OUT_OF_RANGE";
  throw err;
}
function checkOffset(buf, offset, width) {
  if (offset === undefined) return 0;
  if (typeof offset !== "number") {
    const err = new TypeError(`The "offset" argument must be of type number. Received ${typeof offset}`);
    err.code = "ERR_INVALID_ARG_TYPE";
    throw err;
  }
  if (!Number.isInteger(offset)) outOfRange("offset", "an integer", offset);
  const max = buf.length - width;
  if (offset < 0 || offset > max) {
    if (max < 0) {
      const err = new RangeError("Attempt to access memory outside buffer bounds");
      err.code = "ERR_BUFFER_OUT_OF_BOUNDS";
      throw err;
    }
    outOfRange("offset", `>= 0 and <= ${max}`, offset);
  }
  return offset;
}
function checkInt(value, min, max, width) {
  if (typeof value !== "number") {
    const err = new TypeError(`The "value" argument must be of type number. Received ${typeof value}`);
    err.code = "ERR_INVALID_ARG_TYPE";
    throw err;
  }
  if (value < min || value > max || !Number.isInteger(value)) {
    const range = width > 4
      ? `>= ${min} and <= ${max}`
      : min === 0 ? `>= 0 and < 2 ** ${width * 8}` : `>= -(2 ** ${width * 8 - 1}) and < 2 ** ${width * 8 - 1}`;
    outOfRange("value", range, value);
  }
}
function checkBigInt(value, min, max) {
  if (typeof value !== "bigint") {
    const err = new TypeError(`The "value" argument must be of type bigint. Received ${typeof value}`);
    err.code = "ERR_INVALID_ARG_TYPE";
    throw err;
  }
  if (value < min || value > max) {
    outOfRange("value", min === 0n ? ">= 0n and < 2n ** 64n" : ">= -(2n ** 63n) and < 2n ** 63n", value);
  }
}
function view(buf) {
  return new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
}
function checkByteLength(byteLength) {
  if (typeof byteLength !== "number" || !Number.isInteger(byteLength) || byteLength < 1 || byteLength > 6) {
    outOfRange("byteLength", ">= 1 and <= 6", byteLength);
  }
}
function swapBytes(buf, width) {
  if (buf.length % width !== 0) {
    const err = new RangeError(`Buffer size must be a multiple of ${width * 8}-bits`);
    err.code = "ERR_INVALID_BUFFER_SIZE";
    throw err;
  }
  for (let i = 0; i < buf.length; i += width) {
    for (let a = i, b = i + width - 1; a < b; a++, b--) {
      const t = buf[a];
      buf[a] = buf[b];
      buf[b] = t;
    }
  }
  return buf;
}

const numeric = {
  UInt8: [1, (v, o) => v.getUint8(o), (v, o, x) => v.setUint8(o, x), 0, 0xff],
  Int8: [1, (v, o) => v.getInt8(o), (v, o, x) => v.setInt8(o, x), -0x80, 0x7f],
  UInt16LE: [2, (v, o) => v.getUint16(o, true), (v, o, x) => v.setUint16(o, x, true), 0, 0xffff],
  UInt16BE: [2, (v, o) => v.getUint16(o, false), (v, o, x) => v.setUint16(o, x, false), 0, 0xffff],
  Int16LE: [2, (v, o) => v.getInt16(o, true), (v, o, x) => v.setInt16(o, x, true), -0x8000, 0x7fff],
  Int16BE: [2, (v, o) => v.getInt16(o, false), (v, o, x) => v.setInt16(o, x, false), -0x8000, 0x7fff],
  UInt32LE: [4, (v, o) => v.getUint32(o, true), (v, o, x) => v.setUint32(o, x, true), 0, 0xffffffff],
  UInt32BE: [4, (v, o) => v.getUint32(o, false), (v, o, x) => v.setUint32(o, x, false), 0, 0xffffffff],
  Int32LE: [4, (v, o) => v.getInt32(o, true), (v, o, x) => v.setInt32(o, x, true), -0x80000000, 0x7fffffff],
  Int32BE: [4, (v, o) => v.getInt32(o, false), (v, o, x) => v.setInt32(o, x, false), -0x80000000, 0x7fffffff],
  FloatLE: [4, (v, o) => v.getFloat32(o, true), (v, o, x) => v.setFloat32(o, x, true)],
  FloatBE: [4, (v, o) => v.getFloat32(o, false), (v, o, x) => v.setFloat32(o, x, false)],
  DoubleLE: [8, (v, o) => v.getFloat64(o, true), (v, o, x) => v.setFloat64(o, x, true)],
  DoubleBE: [8, (v, o) => v.getFloat64(o, false), (v, o, x) => v.setFloat64(o, x, false)],
  BigUInt64LE: [8, (v, o) => v.getBigUint64(o, true), (v, o, x) => v.setBigUint64(o, x, true), 0n, 0xffffffffffffffffn],
  BigUInt64BE: [8, (v, o) => v.getBigUint64(o, false), (v, o, x) => v.setBigUint64(o, x, false), 0n, 0xffffffffffffffffn],
  BigInt64LE: [8, (v, o) => v.getBigInt64(o, true), (v, o, x) => v.setBigInt64(o, x, true), -0x8000000000000000n, 0x7fffffffffffffffn],
  BigInt64BE: [8, (v, o) => v.getBigInt64(o, false), (v, o, x) => v.setBigInt64(o, x, false), -0x8000000000000000n, 0x7fffffffffffffffn],
};
for (const [name, [width, get, set, min, max]] of Object.entries(numeric)) {
  Buffer.prototype["read" + name] = function (offset) {
    return get(view(this), checkOffset(this, offset, width));
  };
  Buffer.prototype["write" + name] = function (value, offset) {
    if (typeof min === "bigint") checkBigInt(value, min, max);
    else if (min !== undefined) checkInt(value, min, max, width);
    else if (typeof value !== "number") checkInt(value, -Infinity, Infinity, width);
    const o = checkOffset(this, offset, width);
    set(view(this), o, value);
    return o + width;
  };
}
// Node's lowercase aliases (readUint8, writeUint32LE, readBigUint64BE, ...).
for (const name of Object.keys(numeric)) {
  if (!name.includes("UInt")) continue;
  const alias = name.replace("UInt", "Uint");
  Buffer.prototype["read" + alias] = Buffer.prototype["read" + name];
  Buffer.prototype["write" + alias] = Buffer.prototype["write" + name];
}

// Variable-width integers (1..6 bytes), which DataView has no shape for.
Buffer.prototype.readUIntLE = function (offset, byteLength) {
  checkByteLength(byteLength);
  const o = checkOffset(this, offset, byteLength);
  let val = 0;
  for (let i = byteLength - 1; i >= 0; i--) val = val * 0x100 + this[o + i];
  return val;
};
Buffer.prototype.readUIntBE = function (offset, byteLength) {
  checkByteLength(byteLength);
  const o = checkOffset(this, offset, byteLength);
  let val = 0;
  for (let i = 0; i < byteLength; i++) val = val * 0x100 + this[o + i];
  return val;
};
Buffer.prototype.readIntLE = function (offset, byteLength) {
  const val = this.readUIntLE(offset, byteLength);
  const limit = 2 ** (byteLength * 8 - 1);
  return val >= limit ? val - limit * 2 : val;
};
Buffer.prototype.readIntBE = function (offset, byteLength) {
  const val = this.readUIntBE(offset, byteLength);
  const limit = 2 ** (byteLength * 8 - 1);
  return val >= limit ? val - limit * 2 : val;
};
Buffer.prototype.writeUIntLE = function (value, offset, byteLength) {
  checkByteLength(byteLength);
  checkInt(value, 0, 2 ** (byteLength * 8) - 1, byteLength);
  const o = checkOffset(this, offset, byteLength);
  let v = value;
  for (let i = 0; i < byteLength; i++) { this[o + i] = v % 0x100; v = Math.floor(v / 0x100); }
  return o + byteLength;
};
Buffer.prototype.writeUIntBE = function (value, offset, byteLength) {
  checkByteLength(byteLength);
  checkInt(value, 0, 2 ** (byteLength * 8) - 1, byteLength);
  const o = checkOffset(this, offset, byteLength);
  let v = value;
  for (let i = byteLength - 1; i >= 0; i--) { this[o + i] = v % 0x100; v = Math.floor(v / 0x100); }
  return o + byteLength;
};
Buffer.prototype.writeIntLE = function (value, offset, byteLength) {
  checkByteLength(byteLength);
  const limit = 2 ** (byteLength * 8 - 1);
  checkInt(value, -limit, limit - 1, byteLength);
  return this.writeUIntLE(value < 0 ? value + limit * 2 : value, offset, byteLength);
};
Buffer.prototype.writeIntBE = function (value, offset, byteLength) {
  checkByteLength(byteLength);
  const limit = 2 ** (byteLength * 8 - 1);
  checkInt(value, -limit, limit - 1, byteLength);
  return this.writeUIntBE(value < 0 ? value + limit * 2 : value, offset, byteLength);
};
for (const name of ["UIntLE", "UIntBE"]) {
  Buffer.prototype["read" + name.replace("UInt", "Uint")] = Buffer.prototype["read" + name];
  Buffer.prototype["write" + name.replace("UInt", "Uint")] = Buffer.prototype["write" + name];
}

// The per-encoding slice/write methods Node exposes (`buf.utf8Slice(start, end)`,
// `buf.hexWrite(string, offset, length)`) that some npm packages call directly.
for (const [method, enc] of [["utf8", "utf8"], ["hex", "hex"], ["latin1", "latin1"], ["ascii", "ascii"], ["base64", "base64"], ["base64url", "base64url"], ["ucs2", "utf16le"], ["utf16le", "utf16le"]]) {
  Buffer.prototype[method + "Slice"] = function (start, end) { return this.toString(enc, start, end); };
  Buffer.prototype[method + "Write"] = function (string, offset, length) { return this.write(string, offset, length, enc); };
}

// Node deprecated SlowBuffer (it used to return an un-pooled Buffer); we never pool, so it is just
// an un-pooled allocation.
function SlowBuffer(length) {
  return Buffer.allocUnsafeSlow(length);
}

// Coerce the ArrayBuffer/TypedArray/DataView inputs that isAscii/isUtf8 accept into a byte view,
// throwing Node's ERR_INVALID_ARG_TYPE for anything else (notably strings, which Node rejects).
function bytesOf(input, name) {
  if (input instanceof ArrayBuffer) return new Uint8Array(input);
  if (ArrayBuffer.isView(input)) return new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
  const err = new TypeError(
    `The "${name}" argument must be an instance of ArrayBuffer, Buffer, TypedArray, or DataView. Received ${typeof input}`,
  );
  err.code = "ERR_INVALID_ARG_TYPE";
  throw err;
}

function isAscii(input) {
  const b = bytesOf(input, "input");
  for (let i = 0; i < b.length; i++) if (b[i] > 0x7f) return false;
  return true;
}

// A real UTF-8 validator: walks the byte stream, rejecting overlong forms, surrogate code points,
// out-of-range code points, and truncated/misaligned continuation bytes.
function isUtf8(input) {
  const b = bytesOf(input, "input");
  const n = b.length;
  let i = 0;
  while (i < n) {
    const c = b[i];
    if (c < 0x80) { i++; continue; }
    let extra, min, cp;
    if ((c & 0xe0) === 0xc0) { extra = 1; min = 0x80; cp = c & 0x1f; }
    else if ((c & 0xf0) === 0xe0) { extra = 2; min = 0x800; cp = c & 0x0f; }
    else if ((c & 0xf8) === 0xf0) { extra = 3; min = 0x10000; cp = c & 0x07; }
    else return false;
    if (i + extra >= n) return false;
    for (let j = 1; j <= extra; j++) {
      const cc = b[i + j];
      if ((cc & 0xc0) !== 0x80) return false;
      cp = (cp << 6) | (cc & 0x3f);
    }
    if (cp < min || cp > 0x10ffff || (cp >= 0xd800 && cp <= 0xdfff)) return false;
    i += extra + 1;
  }
  return true;
}

// Re-encode bytes from one encoding to another by round-tripping through a JS string. Covers every
// pair the Buffer codec already supports; unknown encodings throw a Node-shaped ERR_UNKNOWN_ENCODING
// (Node itself surfaces an ICU error here, but an explicit unknown-encoding throw is the honest
// signal on an engine without ICU's transcoder).
function transcode(source, fromEnc, toEnc) {
  if (!ArrayBuffer.isView(source) && !(source instanceof ArrayBuffer)) {
    const err = new TypeError('The "source" argument must be an instance of Buffer or Uint8Array.');
    err.code = "ERR_INVALID_ARG_TYPE";
    throw err;
  }
  for (const enc of [fromEnc, toEnc]) {
    if (!Buffer.isEncoding(enc)) {
      const err = new Error(`Unknown encoding: ${enc}`);
      err.code = "ERR_UNKNOWN_ENCODING";
      throw err;
    }
  }
  const bytes =
    source instanceof ArrayBuffer
      ? new Uint8Array(source)
      : new Uint8Array(source.buffer, source.byteOffset, source.byteLength);
  return new Buffer(bytesFromString(stringFromBytes(bytes, fromEnc), toEnc));
}

// lumen has no URL.createObjectURL registry, so every blob: id is unknown — which is exactly the
// value Node returns for an unregistered/expired id.
function resolveObjectURL(_id) {
  return undefined;
}

const kMaxLength = 9007199254740991;
const kStringMaxLength = 536870888;

// The public constructor is Node's deprecated `Buffer(arg)` / `new Buffer(arg)`: a number
// allocates (zero-filled), anything else goes through Buffer.from. Subclasses still get the
// Uint8Array constructor, and so do TypedArray methods that build a Buffer through the species
// constructor (`subarray` passes an ArrayBuffer, which Buffer.from turns into a view).
Buffer = __legacyConstructor(Buffer, undefined, ([arg, encodingOrOffset, length]) => {
  if (typeof arg === "number") {
    if (typeof encodingOrOffset === "string") {
      const error = new TypeError(
        `The "string" argument must be of type string. Received type number (${arg})`,
      );
      error.code = "ERR_INVALID_ARG_TYPE";
      throw error;
    }
    return Buffer.alloc(arg);
  }
  return Buffer.from(arg, encodingOrOffset, length);
});

globalThis.Buffer = Buffer;
__builtins.set("buffer", {
  Buffer,
  SlowBuffer,
  // Node exposes MAX_LENGTH/MAX_STRING_LENGTH here too (mirrors the k* aliases below).
  constants: { MAX_LENGTH: kMaxLength, MAX_STRING_LENGTH: kStringMaxLength },
  kMaxLength,
  kStringMaxLength,
  INSPECT_MAX_BYTES: 50,
  // Reuse the web globals by identity rather than redefining them (lumen-web installs these).
  atob: globalThis.atob,
  btoa: globalThis.btoa,
  Blob: globalThis.Blob,
  File: globalThis.File,
  isAscii,
  isUtf8,
  transcode,
  resolveObjectURL,
});
