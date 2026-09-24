// Buffer — a Uint8Array subclass with Node's codec + accessor surface. The codecs (utf8, utf16le,
// latin1/binary, ascii, hex, base64, base64url), search, compare, fill and swap are native
// (src/codec.rs + src/native.rs, Node's exact semantics, including its lenient decoders); the
// numeric accessors are plain JS arithmetic on the bytes, as in Node.

// Encoding name -> codec id (src/codec.rs). Null-prototype: "constructor" is not an encoding.
const ENC = Object.assign(Object.create(null), {
  utf8: 0, "utf-8": 0,
  latin1: 1, binary: 1,
  ascii: 2,
  hex: 3,
  base64: 4,
  base64url: 5,
  utf16le: 6, "utf-16le": 6, ucs2: 6, "ucs-2": 6,
});
const ENC_UTF8 = 0;
const ENC_LATIN1 = 1;
const ENC_ASCII = 2;

// The codec id of a known encoding name (any case), else undefined.
function encodingId(enc) {
  const id = ENC[enc];
  if (id !== undefined || typeof enc !== "string") return id;
  return ENC[enc.toLowerCase()];
}

function bufError(Ctor, code, message) {
  return __nodeError(Ctor, code, message);
}
function bufReceived(value) {
  return `Received ${__determineSpecificType(value)}`;
}
function bufInvalidArgType(name, expected, value) {
  const subject = name.endsWith(" argument") ? `The ${name}` : `The "${name}" argument`;
  return bufError(TypeError, "ERR_INVALID_ARG_TYPE", `${subject} must be ${expected}. ${bufReceived(value)}`);
}
// 1234567 -> "1_234_567" (Node prints large out-of-range values this way).
function addNumericalSeparator(val) {
  let res = "";
  let i = val.length;
  const start = val[0] === "-" ? 1 : 0;
  for (; i >= start + 4; i -= 3) res = `_${val.slice(i - 3, i)}${res}`;
  return `${val.slice(0, i)}${res}`;
}
function outOfRange(name, range, actual) {
  let received;
  if (Number.isInteger(actual) && Math.abs(actual) > 2 ** 32) {
    received = addNumericalSeparator(String(actual));
  } else if (typeof actual === "bigint") {
    received = String(actual);
    if (actual > 2n ** 32n || actual < -(2n ** 32n)) received = addNumericalSeparator(received);
    received += "n";
  } else {
    received = __builtins.get("util").inspect(actual);
  }
  throw bufError(RangeError, "ERR_OUT_OF_RANGE", `The value of "${name}" is out of range. It must be ${range}. Received ${received}`);
}
function unknownEncoding(enc) {
  return bufError(TypeError, "ERR_UNKNOWN_ENCODING", `Unknown encoding: ${enc}`);
}
function bufferOutOfBounds(name) {
  return bufError(RangeError, "ERR_BUFFER_OUT_OF_BOUNDS",
    name ? `"${name}" is outside of buffer bounds` : "Attempt to access memory outside buffer bounds");
}
// Node's validateOffset: an integer Number in [min, max].
function validateOffset(value, name, min = 0, max = kMaxLength) {
  if (typeof value !== "number") throw bufInvalidArgType(name, "of type number", value);
  if (!Number.isInteger(value)) outOfRange(name, "an integer", value);
  if (value < min || value > max) outOfRange(name, `>= ${min} && <= ${max}`, value);
}
const U8_ARG = "an instance of Buffer or Uint8Array";

// Adopt a fresh Uint8Array (an op result) as a Buffer without copying.
function adopt(u8) {
  return Object.setPrototypeOf(u8, Buffer.prototype);
}

function bytesFromString(str, enc) {
  let id = ENC_UTF8;
  if (typeof enc === "string" && enc.length !== 0) {
    id = encodingId(enc);
    if (id === undefined) throw unknownEncoding(enc);
  }
  return __native.encode(str, id);
}

// `u8.subarray(start, end)` clamped like it, as a plain Uint8Array over the same memory.
function plainView(u8, start, end) {
  const len = u8.length;
  start = Math.trunc(start) || 0;
  end = Math.trunc(end);
  if (start < 0) start = Math.max(len + start, 0);
  if (end < 0) end = Math.max(len + end, 0);
  start = Math.min(start, len);
  end = Math.max(start, Math.min(Number.isNaN(end) ? len : end, len));
  return new Uint8Array(u8.buffer, u8.byteOffset + start, end - start);
}

// Node's toString range rules: negative start is 0, start past the end is "", end clamps.
function stringFromBytes(bytes, enc, start, end) {
  const len = bytes.length;
  if (start === undefined || start <= 0) start = 0;
  else if (start >= len) return "";
  else start = Math.trunc(start) || 0;
  if (end === undefined || end > len) end = len;
  else end = Math.trunc(end) || 0;
  if (end <= start) return "";
  let id = ENC_UTF8;
  if (enc !== undefined) {
    id = encodingId(enc);
    if (id === undefined) {
      id = encodingId(`${enc}`);
      if (id === undefined) throw unknownEncoding(enc);
    }
  }
  if (decodedLength(id, end - start) > kStringMaxLength) {
    throw bufError(Error, "ERR_STRING_TOO_LONG",
      `Cannot create a string longer than 0x${kStringMaxLength.toString(16)} characters`);
  }
  return __native.decode(bytes, id, start, end);
}

// Upper bound on the length of the string decoding `n` bytes with codec `id` produces.
function decodedLength(id, n) {
  switch (id) {
    case 3: return n * 2; // hex
    case 4: case 5: return Math.ceil(n / 3) * 4; // base64, base64url
    case 6: return n >>> 1; // utf16le
    default: return n; // utf8, latin1, ascii: at most one unit per byte
  }
}

function swapBytes(buf, width) {
  if (buf.length % width !== 0) {
    throw bufError(RangeError, "ERR_INVALID_BUFFER_SIZE", `Buffer size must be a multiple of ${width * 8}-bits`);
  }
  if (buf.length < 128) {
    for (let i = 0; i < buf.length; i += width) {
      for (let a = i, b = i + width - 1; a < b; a++, b--) {
        const t = buf[a];
        buf[a] = buf[b];
        buf[b] = t;
      }
    }
    return buf;
  }
  __native.swap(buf, width);
  return buf;
}

// Node's bidirectionalIndexOf: `dir` true searches forward.
function bufferIndexOf(buf, value, byteOffset, encoding, dir) {
  if (!ArrayBuffer.isView(buf)) {
    throw new __errors.ERR_INVALID_ARG_TYPE("buffer", ["Buffer", "TypedArray", "DataView"], buf);
  }
  if (typeof byteOffset === "string") {
    encoding = byteOffset;
    byteOffset = undefined;
  } else if (byteOffset > 0x7fffffff) {
    byteOffset = 0x7fffffff;
  } else if (byteOffset < -0x80000000) {
    byteOffset = -0x80000000;
  }
  byteOffset = +byteOffset;
  if (Number.isNaN(byteOffset)) byteOffset = dir ? 0 : buf.length;
  byteOffset = Math.trunc(byteOffset);
  if (typeof value === "number") return __native.indexOfByte(buf, (value >>> 0) & 0xff, byteOffset, dir);
  let id = ENC_UTF8;
  if (encoding !== undefined) id = encodingId(encoding);
  if (typeof value === "string") {
    if (id === undefined) throw unknownEncoding(encoding);
    return __native.indexOfStr(buf, value, id, byteOffset, dir);
  }
  if (value instanceof Uint8Array) return __native.indexOf(buf, value, byteOffset, dir, id === undefined ? ENC_UTF8 : id);
  throw bufInvalidArgType("value", "one of type number or string or an instance of Buffer or Uint8Array", value);
}

// Node's _fill (buf.fill / Buffer.alloc(size, fill)).
function fillBuffer(buf, value, offset, end, encoding) {
  let id;
  if (typeof value === "string") {
    if (offset === undefined || typeof offset === "string") {
      encoding = offset;
      offset = 0;
      end = buf.length;
    } else if (typeof end === "string") {
      encoding = end;
      end = buf.length;
    }
    id = encoding === undefined ? ENC_UTF8 : encodingId(encoding);
    if (id === undefined) {
      if (typeof encoding !== "string") throw bufInvalidArgType("encoding", "of type string", encoding);
      throw unknownEncoding(encoding);
    }
    if (value.length === 0) {
      value = 0;
    } else if (value.length === 1) {
      const code = value.charCodeAt(0);
      if ((id === ENC_UTF8 && code < 128) || id === ENC_LATIN1) value = code;
    }
  }
  if (offset === undefined) {
    offset = 0;
    end = buf.length;
  } else {
    validateOffset(offset, "offset");
    if (end === undefined) end = buf.length;
    else validateOffset(end, "end", 0, buf.length);
    if (offset >= end) return buf;
  }
  if (typeof value === "number") {
    if (offset > end || end > buf.length) throw bufferOutOfBounds();
    Uint8Array.prototype.fill.call(buf, value & 255, offset, end);
    return buf;
  }
  if (typeof value === "string") {
    if (__native.fillStr(buf, value, id, offset, end) < 0) {
      throw bufError(TypeError, "ERR_INVALID_ARG_VALUE", `The argument 'value' is invalid. Received '${value}'`);
    }
    return buf;
  }
  if (ArrayBuffer.isView(value)) {
    let pat = new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
    if (pat.buffer === buf.buffer) pat = pat.slice(); // the op borrows buf mutably: no aliasing
    if (__native.fill(buf, pat, offset, end) < 0) {
      throw bufError(TypeError, "ERR_INVALID_ARG_VALUE", "The argument 'value' is invalid. Received " + bufReceived(value).slice(9));
    }
    return buf;
  }
  // Anything else fills with ToUint32(value) & 255, like Node's binding.
  Uint8Array.prototype.fill.call(buf, (value >>> 0) & 255, offset, end);
  return buf;
}

function fromArrayBuffer(ab, byteOffset, length) {
  if (byteOffset === undefined) {
    byteOffset = 0;
  } else {
    byteOffset = +byteOffset;
    if (Number.isNaN(byteOffset)) byteOffset = 0;
  }
  const maxLength = ab.byteLength - byteOffset;
  if (maxLength < 0) throw bufferOutOfBounds("offset");
  if (length === undefined) {
    length = maxLength;
  } else {
    length = +length;
    if (length > 0) {
      if (length > maxLength) throw bufferOutOfBounds("length");
    } else {
      length = 0;
    }
  }
  return new BufferClass(ab, byteOffset, length);
}

function fromArrayLike(obj) {
  const len = obj.length;
  if (!(len > 0)) return new BufferClass(0);
  const b = new BufferClass(len);
  b.set(obj);
  return b;
}

// Brand check (the byteLength getters throw on a foreign receiver), as V8's IsAnyArrayBuffer.
const abByteLength = Object.getOwnPropertyDescriptor(ArrayBuffer.prototype, "byteLength").get;
const sabByteLength = typeof SharedArrayBuffer === "function"
  ? Object.getOwnPropertyDescriptor(SharedArrayBuffer.prototype, "byteLength").get : null;
function isAnyArrayBuffer(v) {
  if (v === null || typeof v !== "object") return false;
  try { abByteLength.call(v); return true; } catch {}
  if (sabByteLength === null) return false;
  try { sabByteLength.call(v); return true; } catch { return false; }
}

class Buffer extends Uint8Array {
  static from(value, encodingOrOffset, length) {
    if (typeof value === "string") return adopt(bytesFromString(value, encodingOrOffset));
    if (typeof value === "object" && value !== null) {
      if (isAnyArrayBuffer(value)) return fromArrayBuffer(value, encodingOrOffset, length);
      const valueOf = value.valueOf && value.valueOf();
      if (valueOf != null && valueOf !== value && (typeof valueOf === "string" || typeof valueOf === "object")) {
        return Buffer.from(valueOf, encodingOrOffset, length);
      }
      if (value.length !== undefined || isAnyArrayBuffer(value.buffer)) {
        if (typeof value.length !== "number") return new Buffer(0);
        return fromArrayLike(value);
      }
      if (value.type === "Buffer" && Array.isArray(value.data)) return fromArrayLike(value.data);
      if (typeof value[Symbol.toPrimitive] === "function") {
        const primitive = value[Symbol.toPrimitive]("string");
        if (typeof primitive === "string") return Buffer.from(primitive, encodingOrOffset);
      }
    }
    throw bufInvalidArgType("first argument", "of type string or an instance of Buffer, ArrayBuffer, or Array or an Array-like Object", value);
  }
  static alloc(size, fill, encoding) {
    validateSize(size);
    const b = new Buffer(size);
    if (fill !== undefined && fill !== 0 && size > 0) return fillBuffer(b, fill, 0, b.length, encoding);
    return b;
  }
  static allocUnsafe(size) {
    validateSize(size);
    return new Buffer(size);
  }
  // Node distinguishes allocUnsafeSlow (un-pooled) from allocUnsafe; we don't pool, so they are
  // the same. Its presence also matters for feature-detection: safe-buffer only passes the real
  // Buffer through when all four of from/alloc/allocUnsafe/allocUnsafeSlow exist.
  static allocUnsafeSlow(size) {
    validateSize(size);
    return new Buffer(size);
  }
  static isBuffer(x) {
    return x instanceof Buffer;
  }
  static isEncoding(enc) {
    return typeof enc === "string" && enc.length !== 0 && encodingId(enc) !== undefined;
  }
  static compare(a, b) {
    if (!(a instanceof Uint8Array)) throw bufInvalidArgType("buf1", U8_ARG, a);
    if (!(b instanceof Uint8Array)) throw bufInvalidArgType("buf2", U8_ARG, b);
    if (a === b) return 0;
    return __native.compare(a, b);
  }
  static byteLength(string, encoding) {
    if (typeof string !== "string") {
      if (ArrayBuffer.isView(string) || isAnyArrayBuffer(string)) return string.byteLength;
      throw bufInvalidArgType("string", "of type string or an instance of Buffer or ArrayBuffer", string);
    }
    if (string.length === 0) return 0;
    if (!encoding || encoding === "utf8") return __native.byteLength(string, ENC_UTF8);
    if (encoding === "ascii" || encoding === "latin1") return string.length;
    const id = encodingId(encoding);
    return __native.byteLength(string, id === undefined ? ENC_UTF8 : id);
  }
  static concat(list, length) {
    if (!Array.isArray(list)) throw bufInvalidArgType("list", "an instance of Array", list);
    if (list.length === 0) return new Buffer(0);
    if (length === undefined) {
      length = 0;
      for (let i = 0; i < list.length; i++) {
        if (list[i].length) length += list[i].length;
      }
    } else {
      validateOffset(length, "length");
    }
    const out = new Buffer(length);
    let pos = 0;
    for (let i = 0; i < list.length; i++) {
      const b = list[i];
      if (!(b instanceof Uint8Array)) throw bufInvalidArgType(`list[${i}]`, U8_ARG, b);
      if (pos >= length) break;
      const n = Math.min(b.length, length - pos);
      out.set(n === b.length ? b : plainView(b, 0, n), pos);
      pos += n;
    }
    return out;
  }
  toString(encoding, start, end) {
    if (encoding === undefined && start === undefined && end === undefined) {
      return __native.decode(this, ENC_UTF8, 0, this.length);
    }
    return stringFromBytes(this, encoding, start, end);
  }
  toLocaleString(encoding, start, end) {
    return this.toString(encoding, start, end);
  }
  write(string, offset, length, encoding) {
    if (typeof string !== "string") throw bufError(TypeError, "ERR_INVALID_ARG_TYPE", "argument must be a string");
    if (offset === undefined) {
      return __native.write(this, string, ENC_UTF8, 0, this.length);
    }
    if (length === undefined && typeof offset === "string") {
      encoding = offset;
      length = this.length;
      offset = 0;
    } else {
      validateOffset(offset, "offset", 0, this.length);
      const remaining = this.length - offset;
      if (length === undefined) {
        length = remaining;
      } else if (typeof length === "string") {
        encoding = length;
        length = remaining;
      } else {
        validateOffset(length, "length", 0, this.length);
        if (length > remaining) length = remaining;
      }
    }
    let id = ENC_UTF8;
    if (encoding !== undefined && encoding !== null && encoding !== "") {
      id = encodingId(encoding);
      if (id === undefined) throw unknownEncoding(encoding);
    }
    return __native.write(this, string, id, offset, length);
  }
  // Node's slice is subarray: a view over the same memory (not a copy).
  slice(start, end) {
    const len = this.length;
    start = adjustOffset(start, len);
    end = end !== undefined ? adjustOffset(end, len) : len;
    return new Buffer(this.buffer, this.byteOffset + start, end > start ? end - start : 0);
  }
  equals(otherBuffer) {
    if (!(otherBuffer instanceof Uint8Array)) throw bufInvalidArgType("otherBuffer", U8_ARG, otherBuffer);
    if (this === otherBuffer) return true;
    if (this.byteLength !== otherBuffer.byteLength) return false;
    return this.byteLength === 0 || __native.equals(this, otherBuffer);
  }
  compare(target, targetStart, targetEnd, sourceStart, sourceEnd) {
    if (!(target instanceof Uint8Array)) throw bufInvalidArgType("target", U8_ARG, target);
    if (arguments.length === 1) return __native.compare(this, target);
    if (targetStart === undefined) targetStart = 0;
    else validateOffset(targetStart, "targetStart");
    if (targetEnd === undefined) targetEnd = target.length;
    else validateOffset(targetEnd, "targetEnd", 0, target.length);
    if (sourceStart === undefined) sourceStart = 0;
    else validateOffset(sourceStart, "sourceStart");
    if (sourceEnd === undefined) sourceEnd = this.length;
    else validateOffset(sourceEnd, "sourceEnd", 0, this.length);
    if (sourceStart >= sourceEnd) return targetStart >= targetEnd ? 0 : -1;
    if (targetStart >= targetEnd) return 1;
    if (targetStart > target.length) outOfRange("targetStart", `>= 0 && <= ${target.length}`, targetStart);
    if (sourceStart > this.length) outOfRange("sourceStart", `>= 0 && <= ${this.length}`, sourceStart);
    return __native.compare(plainView(this, sourceStart, sourceEnd), plainView(target, targetStart, targetEnd));
  }
  fill(value, offset, end, encoding) {
    return fillBuffer(this, value, offset, end, encoding);
  }
  toJSON() {
    const data = new Array(this.length);
    for (let i = 0; i < this.length; i++) data[i] = this[i];
    return { type: "Buffer", data };
  }
  copy(target, targetStart, sourceStart, sourceEnd) {
    if (!(target instanceof Uint8Array)) throw bufInvalidArgType("target", U8_ARG, target);
    if (targetStart === undefined) {
      targetStart = 0;
    } else {
      targetStart = Number.isInteger(targetStart) ? targetStart : toInteger(targetStart, 0);
      if (targetStart < 0) outOfRange("targetStart", ">= 0", targetStart);
    }
    if (sourceStart === undefined) {
      sourceStart = 0;
    } else {
      sourceStart = Number.isInteger(sourceStart) ? sourceStart : toInteger(sourceStart, 0);
      if (sourceStart < 0 || sourceStart > this.byteLength) {
        outOfRange("sourceStart", `>= 0 && <= ${this.byteLength}`, sourceStart);
      }
    }
    if (sourceEnd === undefined) {
      sourceEnd = this.byteLength;
    } else {
      sourceEnd = Number.isInteger(sourceEnd) ? sourceEnd : toInteger(sourceEnd, 0);
      if (sourceEnd < 0) outOfRange("sourceEnd", ">= 0", sourceEnd);
    }
    if (targetStart >= target.byteLength || sourceStart >= sourceEnd) return 0;
    if (sourceEnd - sourceStart > target.byteLength - targetStart) sourceEnd = sourceStart + target.byteLength - targetStart;
    let nb = sourceEnd - sourceStart;
    const sourceLen = this.byteLength - sourceStart;
    if (nb > sourceLen) nb = sourceLen;
    if (nb <= 0) return 0;
    // TypedArray#set copies as if through a temporary, so overlapping ranges behave like memmove.
    Uint8Array.prototype.set.call(target, new Uint8Array(this.buffer, this.byteOffset + sourceStart, nb), targetStart);
    return nb;
  }
  swap16() { return swapBytes(this, 2); }
  swap32() { return swapBytes(this, 4); }
  swap64() { return swapBytes(this, 8); }
  // Node's `parent`/`offset` legacy aliases for the backing store.
  get parent() { return this instanceof Buffer ? this.buffer : undefined; }
  get offset() { return this instanceof Buffer ? this.byteOffset : undefined; }
}
// The class itself (the public `Buffer` binding is replaced by a legacy-callable wrapper below).
const BufferClass = Buffer;

function validateSize(size) {
  if (typeof size !== "number") throw bufInvalidArgType("size", "of type number", size);
  if (!(size >= 0 && size <= kMaxLength)) outOfRange("size", `>= 0 && <= ${kMaxLength}`, size);
}
function adjustOffset(offset, length) {
  offset = Math.trunc(offset);
  if (offset === 0 || Number.isNaN(offset)) return 0;
  if (offset < 0) {
    offset += length;
    return offset > 0 ? offset : 0;
  }
  return offset < length ? offset : length;
}
function toInteger(n, defaultVal) {
  n = +n;
  if (!Number.isNaN(n) && n >= Number.MIN_SAFE_INTEGER && n <= Number.MAX_SAFE_INTEGER) {
    return n % 1 === 0 ? n : Math.floor(n);
  }
  return defaultVal;
}

// Search methods are plain functions, as in Node (where `new buf.lastIndexOf()` reaches the
// receiver check instead of failing as a non-constructor).
for (const [name, fn] of [
  ["indexOf", function indexOf(value, byteOffset, encoding) {
    return bufferIndexOf(this, value, byteOffset, encoding, true);
  }],
  ["lastIndexOf", function lastIndexOf(value, byteOffset, encoding) {
    return bufferIndexOf(this, value, byteOffset, encoding, false);
  }],
  ["includes", function includes(value, byteOffset, encoding) {
    return this.indexOf(value, byteOffset, encoding) !== -1;
  }],
]) {
  Object.defineProperty(Buffer.prototype, name, { value: fn, writable: true, configurable: true, enumerable: false });
}

// ---- numeric accessors ----------------------------------------------------------------------
// Node's implementation: byte arithmetic on the indexed elements, with the bounds check folded
// into "is the first/last byte undefined" (a typed array reads `undefined` out of range or at a
// fractional index).

function validateNumber(value, name) {
  if (typeof value !== "number") throw bufInvalidArgType(name, "of type number", value);
}
function boundsError(value, length, type) {
  if (Math.floor(value) !== value) {
    validateNumber(value, type || "offset");
    outOfRange(type || "offset", "an integer", value);
  }
  if (length < 0) throw bufferOutOfBounds();
  outOfRange(type || "offset", `>= ${type ? 1 : 0} and <= ${length}`, value);
}
function checkBounds(buf, offset, byteLength) {
  validateNumber(offset, "offset");
  if (buf[offset] === undefined || buf[offset + byteLength] === undefined) {
    boundsError(offset, buf.length - (byteLength + 1));
  }
}
// `byteLength` is the width minus one (Node's convention).
function checkIntBI(value, min, max, buf, offset, byteLength) {
  if (value > max || value < min) {
    const n = typeof min === "bigint" ? "n" : "";
    let range;
    if (byteLength > 3) {
      if (min === 0 || min === 0n) range = `>= 0${n} and < 2${n} ** ${(byteLength + 1) * 8}${n}`;
      else range = `>= -(2${n} ** ${(byteLength + 1) * 8 - 1}${n}) and < 2 ** ${(byteLength + 1) * 8 - 1}${n}`;
    } else {
      range = `>= ${min}${n} and <= ${max}${n}`;
    }
    outOfRange("value", range, value);
  }
  checkBounds(buf, offset, byteLength);
}
// The value check alone (the variable-width writers check bounds via checkOffset).
function checkInt(value, min, max, width) {
  if (value > max || value < min) {
    let range;
    if (width > 4) range = min === 0 ? `>= 0 and < 2 ** ${width * 8}` : `>= -(2 ** ${width * 8 - 1}) and < 2 ** ${width * 8 - 1}`;
    else range = `>= ${min} and <= ${max}`;
    outOfRange("value", range, value);
  }
}
// Node: a non-integer byteLength is a type error first (boundsError -> validateNumber).
function checkByteLength(byteLength) {
  if (Number.isInteger(byteLength) && byteLength >= 1 && byteLength <= 6) return;
  boundsError(byteLength, 6, "byteLength");
}
function checkOffset(buf, offset, width) {
  if (offset === undefined) offset = 0;
  validateNumber(offset, "offset");
  if (buf[offset] === undefined || buf[offset + width - 1] === undefined) boundsError(offset, buf.length - width);
  return offset;
}

function writeBig64(buf, value, offset, min, max, le) {
  // As in Node: a Number value passes the range check and then fails mixing with BigInt masks.
  checkIntBI(value, min, max, buf, offset, 7);
  const lo = Number(value & 0xffffffffn);
  const hi = Number((value >> 32n) & 0xffffffffn);
  if (le) {
    buf[offset] = lo; buf[offset + 1] = lo >>> 8; buf[offset + 2] = lo >>> 16; buf[offset + 3] = lo >>> 24;
    buf[offset + 4] = hi; buf[offset + 5] = hi >>> 8; buf[offset + 6] = hi >>> 16; buf[offset + 7] = hi >>> 24;
  } else {
    buf[offset + 7] = lo; buf[offset + 6] = lo >>> 8; buf[offset + 5] = lo >>> 16; buf[offset + 4] = lo >>> 24;
    buf[offset + 3] = hi; buf[offset + 2] = hi >>> 8; buf[offset + 1] = hi >>> 16; buf[offset] = hi >>> 24;
  }
  return offset + 8;
}

const BP = Buffer.prototype;
// The fixed-width number accessors are one native call each (`readNum` / `writeNum`): cheaper in
// lumen than the byte arithmetic Node does in JS. The ops report an invalid offset as NaN / -1;
// the JS side then builds Node's exact error (and tells a genuine NaN float apart).
const NUM_KINDS = [
  // [name, width, min, max] — the kind id is the index (src/native.rs `read_num`).
  ["UInt8", 1, 0, 0xff], ["Int8", 1, -0x80, 0x7f],
  ["UInt16LE", 2, 0, 0xffff], ["UInt16BE", 2, 0, 0xffff],
  ["Int16LE", 2, -0x8000, 0x7fff], ["Int16BE", 2, -0x8000, 0x7fff],
  ["UInt32LE", 4, 0, 0xffffffff], ["UInt32BE", 4, 0, 0xffffffff],
  ["Int32LE", 4, -0x80000000, 0x7fffffff], ["Int32BE", 4, -0x80000000, 0x7fffffff],
  ["FloatLE", 4], ["FloatBE", 4], ["DoubleLE", 8], ["DoubleBE", 8],
];
function readFail(buf, offset, width) {
  validateNumber(offset, "offset");
  if (buf[offset] === undefined || buf[offset + width - 1] === undefined) boundsError(offset, buf.length - width);
  return NaN; // in bounds: the float really is NaN
}
function writeFail(buf, offset, width) {
  validateNumber(offset, "offset");
  boundsError(offset, buf.length - width);
}
function defineNumberAccessors(kind, name, width, min, max) {
  const read = {
    [`read${name}`](offset = 0) {
      if (typeof offset === "number") {
        const v = __native.readNum(this, offset, kind);
        if (v === v) return v;
      }
      return readFail(this, offset, width);
    },
  }[`read${name}`];
  const write = {
    [`write${name}`](value, offset = 0) {
      value = +value;
      // Node's order: an 8-bit write checks the offset's type before the value's range.
      if (width === 1 && typeof offset !== "number") validateNumber(offset, "offset");
      if (min !== undefined && (value > max || value < min)) outOfRange("value", `>= ${min} and <= ${max}`, value);
      if (typeof offset === "number") {
        const next = __native.writeNum(this, value, offset, kind);
        if (next >= 0) return next;
      }
      return writeFail(this, offset, width);
    },
  }[`write${name}`];
  BP[`read${name}`] = read;
  BP[`write${name}`] = write;
}

NUM_KINDS.forEach(([name, width, min, max], kind) => defineNumberAccessors(kind, name, width, min, max));

BP.readBigUInt64LE = function readBigUInt64LE(offset = 0) {
  validateNumber(offset, "offset");
  const first = this[offset];
  const last = this[offset + 7];
  if (first === undefined || last === undefined) boundsError(offset, this.length - 8);
  const lo = first + this[offset + 1] * 2 ** 8 + this[offset + 2] * 2 ** 16 + this[offset + 3] * 2 ** 24;
  const hi = this[offset + 4] + this[offset + 5] * 2 ** 8 + this[offset + 6] * 2 ** 16 + last * 2 ** 24;
  return BigInt(lo) + (BigInt(hi) << 32n);
};
BP.readBigUInt64BE = function readBigUInt64BE(offset = 0) {
  validateNumber(offset, "offset");
  const first = this[offset];
  const last = this[offset + 7];
  if (first === undefined || last === undefined) boundsError(offset, this.length - 8);
  const hi = first * 2 ** 24 + this[offset + 1] * 2 ** 16 + this[offset + 2] * 2 ** 8 + this[offset + 3];
  const lo = this[offset + 4] * 2 ** 24 + this[offset + 5] * 2 ** 16 + this[offset + 6] * 2 ** 8 + last;
  return (BigInt(hi) << 32n) + BigInt(lo);
};
BP.readBigInt64LE = function readBigInt64LE(offset = 0) {
  validateNumber(offset, "offset");
  const first = this[offset];
  const last = this[offset + 7];
  if (first === undefined || last === undefined) boundsError(offset, this.length - 8);
  const hi = this[offset + 4] + this[offset + 5] * 2 ** 8 + this[offset + 6] * 2 ** 16 + (last << 24);
  const lo = first + this[offset + 1] * 2 ** 8 + this[offset + 2] * 2 ** 16 + this[offset + 3] * 2 ** 24;
  return (BigInt(hi) << 32n) + BigInt(lo);
};
BP.readBigInt64BE = function readBigInt64BE(offset = 0) {
  validateNumber(offset, "offset");
  const first = this[offset];
  const last = this[offset + 7];
  if (first === undefined || last === undefined) boundsError(offset, this.length - 8);
  const hi = (first << 24) + this[offset + 1] * 2 ** 16 + this[offset + 2] * 2 ** 8 + this[offset + 3];
  const lo = this[offset + 4] * 2 ** 24 + this[offset + 5] * 2 ** 16 + this[offset + 6] * 2 ** 8 + last;
  return (BigInt(hi) << 32n) + BigInt(lo);
};

BP.writeBigUInt64LE = function writeBigUInt64LE(value, offset = 0) { return writeBig64(this, value, offset, 0n, 0xffffffffffffffffn, true); };
BP.writeBigUInt64BE = function writeBigUInt64BE(value, offset = 0) { return writeBig64(this, value, offset, 0n, 0xffffffffffffffffn, false); };
BP.writeBigInt64LE = function writeBigInt64LE(value, offset = 0) { return writeBig64(this, value, offset, -(2n ** 63n), 2n ** 63n - 1n, true); };
BP.writeBigInt64BE = function writeBigInt64BE(value, offset = 0) { return writeBig64(this, value, offset, -(2n ** 63n), 2n ** 63n - 1n, false); };

// Node's lowercase aliases (readUint8, writeUint32LE, readBigUint64BE, ...).
for (const name of Object.getOwnPropertyNames(BP)) {
  if (!/^(read|write)(Big)?UInt/.test(name)) continue;
  BP[name.replace("UInt", "Uint")] = BP[name];
}

// Variable-width integers (1..6 bytes), which DataView has no shape for.
Buffer.prototype.readUIntLE = function (offset, byteLength) {
  if (offset === undefined) throw bufInvalidArgType("offset", "of type number", offset);
  checkByteLength(byteLength);
  const o = checkOffset(this, offset, byteLength);
  let val = 0;
  for (let i = byteLength - 1; i >= 0; i--) val = val * 0x100 + this[o + i];
  return val;
};
Buffer.prototype.readUIntBE = function (offset, byteLength) {
  if (offset === undefined) throw bufInvalidArgType("offset", "of type number", offset);
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
  validateNumber(offset, "offset"); // no default offset for the variable-width writers
  const o = checkOffset(this, offset, byteLength);
  let v = value;
  for (let i = 0; i < byteLength; i++) { this[o + i] = v % 0x100; v = Math.floor(v / 0x100); }
  return o + byteLength;
};
Buffer.prototype.writeUIntBE = function (value, offset, byteLength) {
  checkByteLength(byteLength);
  checkInt(value, 0, 2 ** (byteLength * 8) - 1, byteLength);
  validateNumber(offset, "offset"); // no default offset for the variable-width writers
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

// Node's isAscii/isUtf8 accept a TypedArray or (Shared)ArrayBuffer only, and refuse a detached one.
const typedArrayTagGet = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(Uint8Array.prototype), Symbol.toStringTag).get;
function bytesOf(input, name) {
  let bytes;
  if (typedArrayTagGet.call(input) !== undefined) {
    if (input.buffer.detached) throw new __errors.ERR_INVALID_STATE("Cannot validate on a detached buffer");
    bytes = new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
  } else if (isAnyArrayBuffer(input)) {
    if (input.detached) throw new __errors.ERR_INVALID_STATE("Cannot validate on a detached buffer");
    bytes = new Uint8Array(input);
  } else {
    throw new __errors.ERR_INVALID_ARG_TYPE(name, ["ArrayBuffer", "Buffer", "TypedArray"], input);
  }
  return bytes;
}

function isAscii(input) {
  return __native.isAscii(bytesOf(input, "input"));
}

function isUtf8(input) {
  return __native.isUtf8(bytesOf(input, "input"));
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
  return adopt(bytesFromString(stringFromBytes(bytes, fromEnc), toEnc));
}

// Node's resolveObjectURL: "blob:nodedata:<id>" -> a new Blob over the registered one's data, or
// undefined for anything unknown, revoked or malformed.
function resolveObjectURL(url) {
  url = `${url}`;
  try {
    const parsed = new URL(url);
    const split = parsed.pathname.split(":");
    if (split.length !== 2) return;
    const [base, id] = split;
    if (base !== "nodedata") return;
    const blob = __objectURLs.get(id);
    if (blob === undefined) return;
    return blob.slice(0, blob.size, blob.type);
  } catch {
    // Ignored, as in Node.
  }
}

const kMaxLength = 9007199254740991;
// The engine's string limit (lumen's MAX_STR_LEN, 1 << 26), so MAX_STRING_LENGTH is exact: a
// string of this length can be built and one more cannot.
const kStringMaxLength = 1 << 26;

// The public constructor is Node's deprecated `Buffer(arg)` / `new Buffer(arg)`: a number
// allocates (zero-filled), anything else goes through Buffer.from. Subclasses still get the
// Uint8Array constructor, and so do TypedArray methods that build a Buffer through the species
// constructor (`subarray` passes an ArrayBuffer, which Buffer.from turns into a view).
// DEP0005. Node warns for calls outside node_modules, which it tells from the caller's file in the
// stack; lumen's frames carry no file names, so it warns only under --pending-deprecation (where
// Node warns unconditionally) rather than risk flagging every dependency.
let bufferWarned = false;
function showFlaggedDeprecation() {
  if (bufferWarned) return;
  const pending = process.execArgv.includes("--pending-deprecation")
    || (process.env && process.env.NODE_PENDING_DEPRECATION === "1");
  if (!pending) return;
  bufferWarned = true;
  process.emitWarning("Buffer() is deprecated due to security and usability issues. Please use the " +
    "Buffer.alloc(), Buffer.allocUnsafe(), or Buffer.from() methods instead.", "DeprecationWarning", "DEP0005");
}
Buffer = __legacyConstructor(Buffer, undefined, ([arg, encodingOrOffset, length]) => {
  showFlaggedDeprecation();
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

// `<Buffer 61 62 ... N more bytes>` plus any extra own properties, like Node's inspect hook.
let INSPECT_MAX_BYTES = 50;
Object.defineProperty(Buffer.prototype, Symbol.for("nodejs.util.inspect.custom"), {
  value: function inspect(recurseTimes, ctx) {
    const max = INSPECT_MAX_BYTES;
    const actualMax = Math.min(max, this.length);
    const remaining = this.length - max;
    let str = "";
    for (let i = 0; i < actualMax; i++) str += (i ? " " : "") + (this[i] < 16 ? "0" : "") + this[i].toString(16);
    if (remaining > 0) str += ` ... ${remaining} more byte${remaining > 1 ? "s" : ""}`;
    // Inspect special properties as well, if possible.
    if (ctx) {
      let extras = false;
      const obj = { __proto__: null };
      for (const key of Reflect.ownKeys(this)) {
        if (typeof key === "string" && /^(0|[1-9][0-9]*)$/.test(key)) continue;
        if (!ctx.showHidden && !Object.prototype.propertyIsEnumerable.call(this, key)) continue;
        extras = true;
        obj[key] = this[key];
      }
      if (extras) {
        if (this.length !== 0) str += ", ";
        // '[Object: null prototype] {'.length === 26
        str += __builtins.get("util").inspect(obj, { ...ctx, breakLength: Infinity, compact: true }).slice(27, -2);
      }
    }
    return `<${this.constructor.name} ${str}>`;
  },
  writable: true,
  configurable: true,
});
Object.defineProperty(Buffer.prototype, "inspect", {
  value: Buffer.prototype[Symbol.for("nodejs.util.inspect.custom")], writable: true, configurable: true,
});

// TypedArray methods (map, filter, subarray, ...) build their result through the species
// constructor; as in Node (FastBuffer), that is the plain Uint8Array subclass, not the deprecated
// public constructor.
Object.defineProperty(Buffer, Symbol.species, { get() { return BufferClass; }, enumerable: false, configurable: true });

// Node's pool granularity; lumen does not pool, but code sizes allocations off this value.
Buffer.poolSize = 8 * 1024;

// Buffer.copyBytesFrom(view[, offset[, length]]): a copy of a TypedArray's elements' bytes.
Object.defineProperty(Buffer, "copyBytesFrom", {
  value: function copyBytesFrom(view, offset, length) {
    if (typedArrayTagGet.call(view) === undefined) {
      throw new __errors.ERR_INVALID_ARG_TYPE("view", ["TypedArray"], view);
    }
    const viewLength = view.length;
    if (viewLength === 0) return Buffer.alloc(0);
    if (offset !== undefined || length !== undefined) {
      if (offset !== undefined) {
        __validators.validateInteger(offset, "offset", 0);
        if (offset >= viewLength) return Buffer.alloc(0);
      } else {
        offset = 0;
      }
      let end;
      if (length !== undefined) {
        __validators.validateInteger(length, "length", 0);
        end = offset + length;
      } else {
        end = viewLength;
      }
      view = view.slice(offset, end);
    }
    const out = Buffer.allocUnsafe(view.byteLength);
    out.set(new Uint8Array(view.buffer, view.byteOffset, view.byteLength));
    return out;
  },
  writable: true, configurable: true,
});

globalThis.Buffer = Buffer;
__builtins.set("buffer", {
  Buffer,
  SlowBuffer,
  // Node exposes MAX_LENGTH/MAX_STRING_LENGTH here too (mirrors the k* aliases below).
  constants: { MAX_LENGTH: kMaxLength, MAX_STRING_LENGTH: kStringMaxLength },
  kMaxLength,
  kStringMaxLength,
  get INSPECT_MAX_BYTES() { return INSPECT_MAX_BYTES; },
  set INSPECT_MAX_BYTES(val) {
    __validators.validateNumber(val, "INSPECT_MAX_BYTES", 0);
    INSPECT_MAX_BYTES = val;
  },
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

// Installed here, with the Blob registry it feeds, rather than in url.js: node:url loads on
// first use (see build.rs `LAZY`), but the URL global has these methods from the start.
// ---- URL.createObjectURL / revokeObjectURL (internal/url installObjectURLMethods) ------------

{
  const { ERR_INVALID_ARG_TYPE } = __errors;
  function createObjectURL(obj) {
    if (!(obj instanceof Blob)) throw new ERR_INVALID_ARG_TYPE("obj", "Blob", obj);
    const id = crypto.randomUUID();
    __objectURLs.set(id, obj);
    return `blob:nodedata:${id}`;
  }
  // Node's C++ RevokeObjectURL: parse, require blob:nodedata:<id>, forget the id.
  function revokeObjectURL(url) {
    url = `${url}`;
    let parsed;
    try {
      parsed = new URL(url);
    } catch {
      return;
    }
    if (parsed.protocol !== "blob:") return;
    const path = parsed.pathname;
    if (!path.startsWith("nodedata:")) return;
    __objectURLs.delete(path.slice("nodedata:".length));
  }
  Object.defineProperties(URL, {
    createObjectURL: { __proto__: null, configurable: true, writable: true, enumerable: true, value: createObjectURL },
    revokeObjectURL: { __proto__: null, configurable: true, writable: true, enumerable: true, value: revokeObjectURL },
  });
}
