// node:crypto — the subset that has a real, verifiable backing in lumen. Randomness is the native
// CSPRNG; hashing (md5/sha1/sha224/sha256/sha384/sha512/sha512-224/sha512-256), HMAC, PBKDF2 and
// HKDF are native (src/hash.rs, bit-exact with Node; the async forms run on the worker pool). Asymmetric crypto is
// pure-JS over BigInt: Ed25519/X25519 sign/verify + key generation, and ASN.1 DER/PEM/JWK key
// plumbing (createPublicKey/createPrivateKey, KeyObject.export as pkcs1/sec1/pkcs8/spki ×
// pem/der/jwk) for RSA, Ed25519, X25519 and EC P-256 — all cross-verified against Node v22 in
// both directions. Not constant-time (correctness-first, not an HSM). Native code adds the
// SHA-512 family, scrypt ROMix, and symmetric AES ciphers
// aes-{128,192,256}-{ecb,cbc,ctr,gcm} via createCipheriv/createDecipheriv (PKCS#7 padding,
// streaming update/final, GCM AAD + auth tags). Finite-field DH, P-256 ECDH/ECDSA, RSA
// PKCS#1/PSS/OAEP, prime generation, X.509 parsing, and legacy SPKAC certificates are also
// implemented. Non-AES ciphers and legacy OpenSSL engine selection remain honest throwing stubs.

const webCrypto = globalThis.crypto;

function toBytes(data, encoding) {
  if (data instanceof KeyObject) return data._material;
  if (data instanceof Uint8Array) return data;
  if (data instanceof ArrayBuffer) return new Uint8Array(data);
  if (typeof data === "string") return Buffer.from(data, encoding || "utf8");
  if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  throw new TypeError("crypto: data must be a string or BufferSource");
}

function concatBytes(a, b) {
  const out = new Uint8Array(a.length + b.length);
  out.set(a, 0);
  out.set(b, a.length);
  return out;
}

// ---- digests (native: src/hash.rs, via src/native.rs) -------------------------------------------
// md5 / sha1 / sha224 / sha256 / sha384 / sha512 / sha512-224 / sha512-256, streaming or one-shot,
// HMAC, PBKDF2 and HKDF all run in Rust; SHA-1/SHA-256 use the CPU's SHA extensions when present.

function cryptoError(Ctor, code, message) {
  const err = new Ctor(message);
  err.code = code;
  return err;
}

// Node's "Received ..." suffix for ERR_INVALID_ARG_TYPE.
function describeReceived(value) {
  if (value === null) return "Received null";
  if (value === undefined) return "Received undefined";
  switch (typeof value) {
    case "function": return `Received function ${value.name}`;
    case "object":
      if (value.constructor && value.constructor.name) return `Received an instance of ${value.constructor.name}`;
      return "Received an instance of Object";
    case "string": {
      const s = value.length > 28 ? `${value.slice(0, 25)}...` : value;
      return `Received type string (${s.includes("'") ? JSON.stringify(s) : `'${s}'`})`;
    }
    case "bigint": return `Received type bigint (${value}n)`;
    case "number": return `Received type number (${Object.is(value, -0) ? "-0" : value})`;
    default: return `Received type ${typeof value} (${String(value)})`;
  }
}
function invalidArgType(name, expected, value) {
  return cryptoError(TypeError, "ERR_INVALID_ARG_TYPE", `The "${name}" argument must be ${expected}. ${describeReceived(value)}`);
}
function outOfRangeErr(name, range, value) {
  const received = Number.isInteger(value) && Math.abs(value) > 2 ** 32 ? addNumericalSeparator(String(value)) : value;
  return cryptoError(RangeError, "ERR_OUT_OF_RANGE", `The value of "${name}" is out of range. It must be ${range}. Received ${received}`);
}
// OpenSSL 3 refuses to derive zero bytes; Node surfaces that as this error.
const derivingFailed = () => new Error("Deriving bits failed");
function validateString(value, name) {
  if (typeof value !== "string") throw invalidArgType(name, "of type string", value);
}
function validateInt32(value, name, min = -2147483648, max = 2147483647) {
  if (typeof value !== "number") throw invalidArgType(name, "of type number", value);
  if (!Number.isInteger(value)) throw outOfRangeErr(name, "an integer", value);
  if (value < min || value > max) throw outOfRangeErr(name, `>= ${min} && <= ${max}`, value);
}
function validateFunction(value, name) {
  if (typeof value !== "function") throw invalidArgType(name, "of type function", value);
}
const BUFFER_SOURCE = "of type string or an instance of ArrayBuffer, Buffer, TypedArray, or DataView";
// Node's getArrayBufferOrView: strings encode, BufferSources pass through as byte views.
function bufferSource(value, name, encoding) {
  if (typeof value === "string") return Buffer.from(value, encoding || "utf8");
  if (value instanceof Uint8Array) return value;
  if (ArrayBuffer.isView(value)) return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
  if (value instanceof ArrayBuffer || (typeof SharedArrayBuffer === "function" && value instanceof SharedArrayBuffer)) {
    return new Uint8Array(value);
  }
  throw invalidArgType(name, BUFFER_SOURCE, value);
}

// { id, outLen, blockSize, fn } of a digest name; null when unknown.
const digestCache = new Map();
function digestInfo(name) {
  let info = digestCache.get(name);
  if (info === undefined) {
    const id = typeof name === "string" ? __native.hashId(name) : -1;
    if (id < 0) return null;
    const [outLen, blockSize] = __native.hashInfo(id);
    info = { id, outLen, blockSize, fn: (bytes) => __native.digest(id, bytes) };
    digestCache.set(name, info);
  }
  return info;
}
function digestOrThrow(name) {
  const info = digestInfo(name);
  if (!info) throw cryptoError(TypeError, "ERR_CRYPTO_INVALID_DIGEST", `Invalid digest: ${name}`);
  return info;
}

const MD5 = digestInfo("md5");
const SHA1 = digestInfo("sha1");
const SHA256 = digestInfo("sha256");
const SHA384 = digestInfo("sha384");
const SHA512 = digestInfo("sha512");
function md5(bytes) { return __native.digest(MD5.id, bytes); }
function sha1(bytes) { return __native.digest(SHA1.id, bytes); }
function sha256(bytes) { return __native.digest(SHA256.id, bytes); }
function sha384(bytes) { return __native.digest(SHA384.id, bytes); }
function sha512(bytes) { return __native.digest(SHA512.id, bytes); }

// ---- hash registry ----------------------------------------------------------------------------
// `{ id, fn, outLen, blockSize }` per algorithm (digestInfo above; blockSize is the HMAC block
// size), shared by Hash/Hmac, the KDFs and the public-key code.

function resolveHash(algorithm) {
  const reg = digestInfo(String(algorithm));
  if (!reg) throw new Error("Digest method not supported");
  return reg;
}

// Raw HMAC over byte inputs — the shared core for the public-key helpers.
function hmacRaw(reg, keyBytes, dataBytes) {
  return __native.hmac(reg.id, keyBytes, dataBytes);
}

// ---- Hash / Hmac classes ----------------------------------------------------------------------

const HASH_DATA = "of type string or an instance of Buffer, TypedArray, or DataView";
function feed(state, data, encoding) {
  if (typeof data === "string") {
    if (encoding === undefined || encoding === "utf8" || encoding === "utf-8" || !Buffer.isEncoding(encoding)) {
      state.updateStr(data);
    } else {
      state.update(Buffer.from(data, encoding));
    }
  } else if (ArrayBuffer.isView(data)) {
    state.update(data);
  } else {
    throw invalidArgType("data", HASH_DATA, data);
  }
}
// An op's fresh Uint8Array, adopted as a Buffer in place (no copy, no derived-class construct).
const asBuffer = (bytes) => Object.setPrototypeOf(bytes, Buffer.prototype);
function finish(bytes, encoding) {
  const digest = asBuffer(bytes);
  return encoding && encoding !== "buffer" && Buffer.isEncoding(encoding) ? digest.toString(encoding) : digest;
}

class Hash {
  constructor(algorithm, options) {
    if (algorithm instanceof Hash) {
      // copy(): an independent snapshot of the running state.
      this._state = algorithm._state.copy();
      this._reg = algorithm._reg;
      return;
    }
    validateString(algorithm, "algorithm");
    this._reg = resolveHash(algorithm);
    this._state = __native.hashNew(this._reg.id);
    void options;
  }
  update(data, encoding) {
    feed(this._state, data, encoding);
    return this;
  }
  digest(encoding) {
    return finish(this._state.digest(), encoding);
  }
  copy() {
    return new Hash(this);
  }
}

class Hmac {
  constructor(algorithm, key, options) {
    validateString(algorithm, "algorithm");
    const reg = digestInfo(algorithm);
    if (!reg) throw cryptoError(TypeError, "ERR_CRYPTO_INVALID_DIGEST", `Invalid digest: ${algorithm}`);
    let k;
    if (key instanceof KeyObject) k = key._material;
    else if (typeof key === "string") k = Buffer.from(key, (options && options.encoding) || "utf8");
    else if (ArrayBuffer.isView(key) || key instanceof ArrayBuffer) k = bufferSource(key, "key");
    else if (key && typeof key === "object" && key._material instanceof Uint8Array) k = key._material;
    else throw invalidArgType("key", "of type string or an instance of ArrayBuffer, Buffer, TypedArray, DataView, KeyObject, or CryptoKey", key);
    this._reg = reg;
    this._state = __native.hmacNew(reg.id, k);
    this._done = false;
  }
  update(data, encoding) {
    feed(this._state, data, encoding);
    return this;
  }
  digest(encoding) {
    // Node: a second digest() on an Hmac returns an empty result instead of throwing.
    const bytes = this._done ? new Uint8Array(0) : this._state.digest();
    this._done = true;
    return finish(bytes, encoding);
  }
}

// ---- one-shot hash (Node 22 crypto.hash) ------------------------------------------------------

function hash(algorithm, data, outputEncoding) {
  validateString(algorithm, "algorithm");
  if (typeof data !== "string" && !ArrayBuffer.isView(data)) throw invalidArgType("data", HASH_DATA, data);
  const enc = outputEncoding === undefined ? "hex" : outputEncoding;
  const reg = resolveHash(algorithm);
  const bytes = typeof data === "string" ? __native.digestStr(reg.id, data) : __native.digest(reg.id, data);
  return finish(bytes, enc);
}

// ---- KDFs (native) ----------------------------------------------------------------------------

function pbkdf2Check(password, salt, iterations, keylen, digest) {
  validateString(digest, "digest");
  password = bufferSource(password, "password");
  salt = bufferSource(salt, "salt");
  validateInt32(iterations, "iterations", 1);
  validateInt32(keylen, "keylen", 0);
  return { password, salt, reg: digestOrThrow(digest) };
}

function pbkdf2Sync(password, salt, iterations, keylen, digest) {
  const c = pbkdf2Check(password, salt, iterations, keylen, digest);
  if (keylen === 0) throw derivingFailed();
  return asBuffer(__native.pbkdf2(c.reg.id, c.password, c.salt, iterations, keylen));
}

function pbkdf2(password, salt, iterations, keylen, digest, callback) {
  if (typeof digest === "function") { callback = digest; digest = undefined; }
  const c = pbkdf2Check(password, salt, iterations, keylen, digest);
  validateFunction(callback, "callback");
  if (keylen === 0) { process.nextTick(() => callback.call(null, derivingFailed())); return; }
  // Derived on the worker pool; timers and I/O keep running meanwhile.
  __native.pbkdf2Async(c.reg.id, c.password, c.salt, iterations, keylen).then(
    (out) => callback.call(null, null, asBuffer(out)),
    (err) => callback.call(null, err),
  );
}

function hkdfCheck(digest, ikm, salt, info, keylen) {
  validateString(digest, "digest");
  const reg = digestOrThrow(digest);
  const ikmB = ikm instanceof KeyObject ? ikm._material : bufferSource(ikm, "ikm");
  const saltB = bufferSource(salt, "salt");
  const infoB = bufferSource(info, "info");
  validateInt32(keylen, "length", 0);
  if (infoB.length > 1024) {
    throw outOfRangeErr("info", "must not contain more than 1024 bytes", infoB.length);
  }
  if (keylen > 255 * reg.outLen) throw cryptoError(RangeError, "ERR_CRYPTO_INVALID_KEYLEN", "Invalid key length");
  return { reg, ikmB, saltB, infoB };
}

function hkdfSync(digest, ikm, salt, info, keylen) {
  const c = hkdfCheck(digest, ikm, salt, info, keylen);
  if (keylen === 0) throw derivingFailed();
  const okm = __native.hkdf(c.reg.id, c.ikmB, c.saltB, c.infoB, keylen);
  return okm.buffer; // Node returns an ArrayBuffer (the op's Uint8Array owns all of it)
}

function hkdf(digest, ikm, salt, info, keylen, callback) {
  const c = hkdfCheck(digest, ikm, salt, info, keylen);
  validateFunction(callback, "callback");
  if (keylen === 0) { process.nextTick(() => callback.call(null, derivingFailed())); return; }
  __native.hkdfAsync(c.reg.id, c.ikmB, c.saltB, c.infoB, keylen).then(
    (ab) => callback.call(null, null, ab),
    (err) => callback.call(null, err),
  );
}

// ---- WebCrypto subtle.digest --------------------------------------------------------------------
// lumen-web's SubtleCrypto knows SHA-256 only; under node: every WebCrypto digest (SHA-1/256/384/
// 512) is native and computed on the worker pool, like Node's.
const SUBTLE_DIGESTS = { "SHA-1": "sha1", "SHA-256": "sha256", "SHA-384": "sha384", "SHA-512": "sha512" };
if (webCrypto && webCrypto.subtle) {
  const subtleProto = Object.getPrototypeOf(webCrypto.subtle);
  Object.defineProperty(subtleProto, "digest", {
    value: async function digest(algorithm, data) {
      const name = typeof algorithm === "string" ? algorithm : algorithm && algorithm.name;
      const alg = typeof name === "string" ? SUBTLE_DIGESTS[name.toUpperCase()] : undefined;
      if (!alg) throw new DOMException("Unrecognized algorithm name", "NotSupportedError");
      let bytes;
      if (data instanceof ArrayBuffer) bytes = new Uint8Array(data);
      else if (ArrayBuffer.isView(data)) bytes = new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
      else throw new TypeError("Failed to execute 'digest' on 'SubtleCrypto': parameter 2 is not of type 'BufferSource'.");
      return __native.digestAsync(digestInfo(alg).id, bytes);
    },
    writable: true,
    configurable: true,
  });
}

// ---- randomness (native CSPRNG) -----------------------------------------------------------------

function fillRandom(view) {
  __native.randomFill(view);
}

function randomBytes(size, cb) {
  validateInt32(size, "size", 0);
  if (cb !== undefined) validateFunction(cb, "callback");
  const buf = Buffer.allocUnsafe(size);
  fillRandom(buf);
  if (cb) { queueMicrotask(() => cb(null, buf)); return; }
  return buf;
}

function randomFillRange(buf, offset, size) {
  let view;
  if (ArrayBuffer.isView(buf)) view = new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength);
  else if (buf instanceof ArrayBuffer || (typeof SharedArrayBuffer === "function" && buf instanceof SharedArrayBuffer)) view = new Uint8Array(buf);
  else throw invalidArgType("buf", "an instance of ArrayBuffer or ArrayBufferView", buf);
  const elem = ArrayBuffer.isView(buf) && buf.BYTES_PER_ELEMENT ? buf.BYTES_PER_ELEMENT : 1;
  const count = view.length / elem;
  const off = offset === undefined ? 0 : offset;
  if (typeof off !== "number") throw invalidArgType("offset", "of type number", off);
  if (!Number.isInteger(off) || off < 0 || off > count) throw outOfRangeErr("offset", `>= 0 && <= ${count}`, off);
  const sz = size === undefined ? count - off : size;
  if (typeof sz !== "number") throw invalidArgType("size", "of type number", sz);
  if (!Number.isInteger(sz) || sz < 0 || sz + off > count) throw outOfRangeErr("size", `>= 0 && <= ${count - off}`, sz);
  return view.subarray(off * elem, (off + sz) * elem);
}

function randomFillSync(buf, offset, size) {
  fillRandom(randomFillRange(buf, offset, size));
  return buf;
}

function randomFill(buf, offset, size, cb) {
  if (typeof offset === "function") { cb = offset; offset = undefined; size = undefined; }
  else if (typeof size === "function") { cb = size; size = undefined; }
  validateFunction(cb, "callback");
  const range = randomFillRange(buf, offset, size);
  queueMicrotask(() => { fillRandom(range); cb(null, buf); });
}

function randomInt(...args) {
  let cb;
  if (typeof args[args.length - 1] === "function") cb = args.pop();
  let min, max;
  if (args.length === 1) { min = 0; max = args[0]; }
  else { min = args[0]; max = args[1]; }
  if (!Number.isSafeInteger(min) || !Number.isSafeInteger(max)) {
    throw new RangeError("randomInt: min and max must be safe integers");
  }
  const range = max - min;
  if (range <= 0) {
    throw new RangeError('The value of "max" is out of range. It must be greater than the value of "min".');
  }
  const draw = () => {
    const bits = Math.ceil(Math.log2(range));
    const bytes = Math.max(1, Math.ceil(bits / 8));
    const mod = Math.pow(2, bytes * 8);
    const limit = mod - (mod % range);
    for (;;) {
      const b = randomBytes(bytes);
      let v = 0;
      for (let i = 0; i < bytes; i++) v = v * 256 + b[i];
      if (v < limit) return min + (v % range);
    }
  };
  if (cb) {
    let result, err;
    try { result = draw(); } catch (e) { err = e; }
    queueMicrotask(() => (err ? cb(err) : cb(null, result)));
    return;
  }
  return draw();
}

function timingSafeEqual(a, b) {
  const view = (x, name) => {
    if (ArrayBuffer.isView(x)) return new Uint8Array(x.buffer, x.byteOffset, x.byteLength);
    if (x instanceof ArrayBuffer || (typeof SharedArrayBuffer === "function" && x instanceof SharedArrayBuffer)) return new Uint8Array(x);
    throw cryptoError(TypeError, "ERR_INVALID_ARG_TYPE", `The "${name}" argument must be an instance of ArrayBuffer, Buffer, TypedArray, or DataView.`);
  };
  const ab = view(a, "buf1");
  const bb = view(b, "buf2");
  if (ab.length !== bb.length) {
    throw cryptoError(RangeError, "ERR_CRYPTO_TIMING_SAFE_EQUAL_LENGTH", "Input buffers must have the same byte length");
  }
  return __native.timingSafeEqual(ab, bb);
}

// ---- secret KeyObjects ------------------------------------------------------------------------

class KeyObject {
  constructor(type, material) {
    this._type = type;
    this._material = material;
    this._asym = null; // key struct (see the asymmetric section below) for public/private keys
  }
  get type() { return this._type; }
  get symmetricKeySize() { return this._type === "secret" ? this._material.length : undefined; }
  get asymmetricKeyType() { return this._asym ? this._asym.kind : undefined; }
  get asymmetricKeyDetails() {
    if (!this._asym) return undefined;
    const k = this._asym;
    if (k.kind === "rsa") return { modulusLength: bitLength(k.n), publicExponent: k.e };
    if (k.kind === "ec") return { namedCurve: k.curve };
    return {};
  }
  export(options) {
    if (this._type !== "secret") return exportAsymmetricKey(this, options);
    if (options && options.format === "jwk") {
      const k = Buffer.from(this._material)
        .toString("base64")
        .replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
      return { kty: "oct", k };
    }
    return Buffer.from(this._material);
  }
  equals(other) {
    if (!(other instanceof KeyObject) || other._type !== this._type) return false;
    if (this._type !== "secret") {
      const t = this._type === "private" ? "pkcs8" : "spki";
      const a = exportAsymmetricKey(this, { format: "der", type: t });
      const b = exportAsymmetricKey(other, { format: "der", type: t });
      return a.length === b.length && timingSafeEqual(a, b);
    }
    if (this._material.length !== other._material.length) return false;
    return timingSafeEqual(this._material, other._material);
  }
}

function createSecretKey(key, encoding) {
  const bytes = typeof key === "string" ? Buffer.from(key, encoding || "utf8") : toBytes(key);
  return new KeyObject("secret", Uint8Array.from(bytes));
}

function generateKeySync(type, options) {
  const t = String(type).toLowerCase();
  if (t !== "hmac" && t !== "aes") {
    throw new Error(`Unsupported key type '${type}' in lumen (secret 'hmac'/'aes' only)`);
  }
  const length = options && options.length;
  if (!Number.isInteger(length)) throw new TypeError("options.length must be an integer number of bits");
  if (length % 8 !== 0) throw new RangeError("options.length must be a multiple of 8");
  return new KeyObject("secret", Uint8Array.from(randomBytes(length / 8)));
}

function generateKey(type, options, cb) {
  if (typeof cb !== "function") throw new TypeError("callback must be a function");
  let key, err;
  try { key = generateKeySync(type, options); } catch (e) { err = e; }
  queueMicrotask(() => (err ? cb(err) : cb(null, key)));
}

// ================================================================================================
// ASYMMETRIC CRYPTO (pure JS, BigInt-backed). Implemented tiers are cross-checked bit-for-bit
// against Node v22 / OpenSSL. Not constant-time — a correctness-first implementation for a JS
// runtime, not an HSM. Anything unsupported throws an honest, specific error.
// ================================================================================================

// ---- bignum / byte helpers ----------------------------------------------------------------------

function bytesToBigIntBE(bytes) {
  let n = 0n;
  for (let i = 0; i < bytes.length; i++) n = (n << 8n) | BigInt(bytes[i]);
  return n;
}
function bigIntToBytesBE(n, len) {
  if (n < 0n) throw new RangeError("bigIntToBytesBE: negative");
  const out = [];
  let x = n;
  while (x > 0n) { out.unshift(Number(x & 0xffn)); x >>= 8n; }
  if (out.length === 0) out.push(0);
  let u = Uint8Array.from(out);
  if (len !== undefined) {
    if (u.length > len) throw new RangeError("bigIntToBytesBE: value larger than requested length");
    if (u.length < len) { const o = new Uint8Array(len); o.set(u, len - u.length); u = o; }
  }
  return u;
}
function amod(a, m) { let r = a % m; if (r < 0n) r += m; return r; }
function modPow(base, exp, m) {
  let b = amod(base, m);
  let r = 1n;
  let e = exp;
  while (e > 0n) { if (e & 1n) r = (r * b) % m; b = (b * b) % m; e >>= 1n; }
  return r;
}
function modInv(a, m) {
  let [old_r, r] = [amod(a, m), m];
  let [old_s, s] = [1n, 0n];
  while (r !== 0n) {
    const q = old_r / r;
    [old_r, r] = [r, old_r - q * r];
    [old_s, s] = [s, old_s - q * s];
  }
  if (old_r !== 1n) throw new Error("modInv: value not invertible");
  return amod(old_s, m);
}
function bitLength(n) { let b = 0; let x = n; while (x > 0n) { x >>= 1n; b++; } return b; }
function concatAll(arrs) {
  let total = 0;
  for (const a of arrs) total += a.length;
  const out = new Uint8Array(total);
  let off = 0;
  for (const a of arrs) { out.set(a, off); off += a.length; }
  return out;
}

// Digest-name normalisation for public-key algorithms ("RSA-SHA256", "sha256WithRSAEncryption",
// "ecdsa-with-SHA256" …).
function normalizeDigest(name) {
  let s = String(name).toLowerCase().replace(/^rsa-/, "").replace(/withrsaencryption$/, "");
  s = s.replace(/^ecdsa-with-/, "").replace(/^id-/, "");
  return s;
}

// ---- ASN.1 DER (parse + serialize) + PEM armor --------------------------------------------------

function derRead(buf, off) {
  const tag = buf[off];
  let i = off + 1;
  let len = buf[i++];
  if (len & 0x80) {
    const n = len & 0x7f;
    len = 0;
    for (let j = 0; j < n; j++) len = len * 256 + buf[i++];
  }
  if (i + len > buf.length) throw new Error("ASN.1: truncated DER");
  return { tag, hstart: off, start: i, end: i + len, content: buf.subarray(i, i + len) };
}
function derChildren(buf) {
  const out = [];
  let off = 0;
  while (off < buf.length) {
    const t = derRead(buf, off);
    out.push(t);
    off = t.end;
  }
  return out;
}
function derInt2Big(node) { return bytesToBigIntBE(node.content); }

function derLen(n) {
  if (n < 0x80) return Uint8Array.of(n);
  const b = [];
  let x = n;
  while (x > 0) { b.unshift(x & 0xff); x = Math.floor(x / 256); }
  return Uint8Array.of(0x80 | b.length, ...b);
}
function derTLV(tag, content) { return concatAll([Uint8Array.of(tag), derLen(content.length), content]); }
function derIntFromBig(n) {
  let bytes = bigIntToBytesBE(n);
  if (bytes[0] & 0x80) bytes = concatAll([Uint8Array.of(0), bytes]);
  return derTLV(0x02, bytes);
}
function derSeq(children) { return derTLV(0x30, concatAll(children)); }
function derBitString(bytes) { return derTLV(0x03, concatAll([Uint8Array.of(0), bytes])); }
function derOctet(bytes) { return derTLV(0x04, bytes); }
function derNull() { return Uint8Array.of(0x05, 0x00); }
function encodeOIDBody(str) {
  const parts = str.split(".").map(Number);
  const bytes = [40 * parts[0] + parts[1]];
  for (let i = 2; i < parts.length; i++) {
    let v = parts[i];
    const stack = [v & 0x7f];
    v = Math.floor(v / 128);
    while (v > 0) { stack.unshift((v & 0x7f) | 0x80); v = Math.floor(v / 128); }
    for (const s of stack) bytes.push(s);
  }
  return Uint8Array.from(bytes);
}
function derOID(str) { return derTLV(0x06, encodeOIDBody(str)); }
function decodeOID(bytes) {
  const first = bytes[0];
  const x = first < 80 ? Math.floor(first / 40) : 2;
  const out = [x, first - x * 40];
  let v = 0;
  for (let i = 1; i < bytes.length; i++) {
    v = v * 128 + (bytes[i] & 0x7f);
    if (!(bytes[i] & 0x80)) { out.push(v); v = 0; }
  }
  return out.join(".");
}

function pemEncode(label, der) {
  const b64 = Buffer.from(der).toString("base64");
  let body = "";
  for (let i = 0; i < b64.length; i += 64) body += b64.slice(i, i + 64) + "\n";
  return `-----BEGIN ${label}-----\n${body}-----END ${label}-----\n`;
}
function pemDecode(pem) {
  const s = String(pem);
  const m = s.match(/-----BEGIN ([^-]+)-----\r?\n([\s\S]*?)-----END \1-----/);
  if (!m) throw new Error("crypto: no PEM data found");
  const der = Buffer.from(m[2].replace(/[^A-Za-z0-9+/=]/g, ""), "base64");
  return { label: m[1].trim(), der: Uint8Array.from(der) };
}

const OID = {
  rsa: "1.2.840.113549.1.1.1",
  rsaPss: "1.2.840.113549.1.1.10",
  ed25519: "1.3.101.112",
  x25519: "1.3.101.110",
  ecPublicKey: "1.2.840.10045.2.1",
  p256: "1.2.840.10045.3.1.7",
  sha1WithRSA: "1.2.840.113549.1.1.5",
  sha256WithRSA: "1.2.840.113549.1.1.11",
  sha384WithRSA: "1.2.840.113549.1.1.12",
  sha512WithRSA: "1.2.840.113549.1.1.13",
  ecdsaWithSHA256: "1.2.840.10045.4.3.2",
  ecdsaWithSHA384: "1.2.840.10045.4.3.3",
  ecdsaWithSHA512: "1.2.840.10045.4.3.4",
};
const CURVE_BY_OID = {
  "1.2.840.10045.3.1.7": "prime256v1",
  "1.3.132.0.34": "secp384r1",
  "1.3.132.0.35": "secp521r1",
  "1.3.132.0.10": "secp256k1",
};

// ---- EC P-256 (short Weierstrass, Jacobian coordinates) -----------------------------------------
// Only prime256v1 is implemented; any other named curve throws by name.

const P256 = {
  name: "prime256v1",
  p: 0xffffffff00000001000000000000000000000000ffffffffffffffffffffffffn,
  a: 0xffffffff00000001000000000000000000000000fffffffffffffffffffffffcn,
  b: 0x5ac635d8aa3a93e7b3ebbd55769886bc651d06b0cc53b0f63bce3c3e27d2604bn,
  n: 0xffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551n,
  gx: 0x6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296n,
  gy: 0x4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5n,
  size: 32,
};
function curveByName(name) {
  const n = String(name).toLowerCase();
  if (n === "prime256v1" || n === "p-256" || n === "secp256r1") return P256;
  throw new Error(`Named curve '${name}' is not supported in lumen (only prime256v1/P-256)`);
}
function ecInfinity() { return [1n, 1n, 0n]; }
function ecIsInfinity(P) { return P[2] === 0n; }
function ecDouble(C, P) {
  const p = C.p;
  const [X1, Y1, Z1] = P;
  if (Z1 === 0n || Y1 === 0n) return ecInfinity();
  const S = amod(4n * X1 * Y1 * Y1, p);
  const Z2 = amod(Z1 * Z1, p);
  const M = amod(3n * X1 * X1 + C.a * Z2 * Z2, p);
  const X3 = amod(M * M - 2n * S, p);
  const Y3 = amod(M * (S - X3) - 8n * Y1 * Y1 * Y1 * Y1, p);
  const Z3 = amod(2n * Y1 * Z1, p);
  return [X3, Y3, Z3];
}
function ecAdd(C, P, Q) {
  const p = C.p;
  if (ecIsInfinity(P)) return Q;
  if (ecIsInfinity(Q)) return P;
  const [X1, Y1, Z1] = P;
  const [X2, Y2, Z2] = Q;
  const Z1Z1 = amod(Z1 * Z1, p);
  const Z2Z2 = amod(Z2 * Z2, p);
  const U1 = amod(X1 * Z2Z2, p);
  const U2 = amod(X2 * Z1Z1, p);
  const S1 = amod(Y1 * Z2 * Z2Z2, p);
  const S2 = amod(Y2 * Z1 * Z1Z1, p);
  if (U1 === U2) {
    if (S1 !== S2) return ecInfinity();
    return ecDouble(C, P);
  }
  const H = amod(U2 - U1, p);
  const R = amod(S2 - S1, p);
  const HH = amod(H * H, p);
  const HHH = amod(H * HH, p);
  const U1HH = amod(U1 * HH, p);
  const X3 = amod(R * R - HHH - 2n * U1HH, p);
  const Y3 = amod(R * (U1HH - X3) - S1 * HHH, p);
  const Z3 = amod(H * Z1 * Z2, p);
  return [X3, Y3, Z3];
}
function ecMul(C, k, P) {
  let R = ecInfinity();
  let Q = P;
  let s = k;
  while (s > 0n) {
    if (s & 1n) R = ecAdd(C, R, Q);
    Q = ecDouble(C, Q);
    s >>= 1n;
  }
  return R;
}
function ecToAffine(C, P) {
  if (ecIsInfinity(P)) return null;
  const zi = modInv(P[2], C.p);
  const zi2 = amod(zi * zi, C.p);
  return [amod(P[0] * zi2, C.p), amod(P[1] * zi2 * zi, C.p)];
}
function ecG(C) { return [C.gx, C.gy, 1n]; }
function ecPointEncode(C, aff) {
  return concatAll([Uint8Array.of(4), bigIntToBytesBE(aff[0], C.size), bigIntToBytesBE(aff[1], C.size)]);
}
function ecPointDecode(C, bytes) {
  if ((bytes[0] === 0x04 || bytes[0] === 0x06 || bytes[0] === 0x07) && bytes.length === 1 + 2 * C.size) {
    const x = bytesToBigIntBE(bytes.subarray(1, 1 + C.size));
    const y = bytesToBigIntBE(bytes.subarray(1 + C.size));
    if (x >= C.p || y >= C.p || amod(y * y - x * x * x - C.a * x - C.b, C.p) !== 0n) {
      throw new Error("crypto: EC point is not on the curve");
    }
    if (bytes[0] >= 0x06 && (y & 1n) !== BigInt(bytes[0] & 1)) {
      throw new Error("crypto: invalid hybrid EC point encoding");
    }
    return [x, y, 1n];
  }
  if ((bytes[0] === 0x02 || bytes[0] === 0x03) && bytes.length === 1 + C.size) {
    const x = bytesToBigIntBE(bytes.subarray(1));
    const y2 = amod(x * x * x + C.a * x + C.b, C.p);
    let y = modPow(y2, (C.p + 1n) / 4n, C.p); // p ≡ 3 mod 4 for P-256
    if (amod(y * y, C.p) !== y2) throw new Error("crypto: EC point is not on the curve");
    if ((y & 1n) !== BigInt(bytes[0] & 1)) y = amod(-y, C.p);
    return [x, y, 1n];
  }
  throw new Error("crypto: unsupported EC point encoding");
}
function ecPubFromPriv(C, d) { return ecPointEncode(C, ecToAffine(C, ecMul(C, d, ecG(C)))); }
function ecPointEncodeFormat(C, point, format) {
  const aff = ecToAffine(C, point);
  if (!aff) throw new Error("crypto: EC point at infinity");
  const x = bigIntToBytesBE(aff[0], C.size);
  const y = bigIntToBytesBE(aff[1], C.size);
  const f = format === undefined ? "uncompressed" : String(format).toLowerCase();
  if (f === "compressed") return concatAll([Uint8Array.of(Number(2n | (aff[1] & 1n))), x]);
  if (f === "hybrid") return concatAll([Uint8Array.of(Number(6n | (aff[1] & 1n))), x, y]);
  if (f === "uncompressed") return concatAll([Uint8Array.of(4), x, y]);
  throw new TypeError(`Invalid EC point conversion form '${format}'`);
}

class ECDH {
  constructor(curve) {
    this._curve = curveByName(curve);
    this._private = null;
    this._public = null;
  }
  generateKeys(encoding, format) {
    let bytes;
    do {
      bytes = randomBytes(this._curve.size);
      this._private = bytesToBigIntBE(bytes) % this._curve.n;
    } while (this._private === 0n);
    this._public = ecMul(this._curve, this._private, ecG(this._curve));
    return this.getPublicKey(encoding, format);
  }
  computeSecret(otherPublicKey, inputEncoding, outputEncoding) {
    if (this._private === null) throw new Error("Private key is not set");
    const bytes = toBytes(otherPublicKey, inputEncoding);
    const shared = ecToAffine(this._curve, ecMul(this._curve, this._private, ecPointDecode(this._curve, bytes)));
    if (!shared) throw new Error("Failed to compute ECDH key");
    const secret = Buffer.from(bigIntToBytesBE(shared[0], this._curve.size));
    return outputEncoding ? secret.toString(outputEncoding) : secret;
  }
  getPrivateKey(encoding) {
    if (this._private === null) throw new Error("Private key is not set");
    const key = Buffer.from(bigIntToBytesBE(this._private, this._curve.size));
    return encoding ? key.toString(encoding) : key;
  }
  getPublicKey(encoding, format) {
    if (!this._public) throw new Error("Public key is not set");
    const key = Buffer.from(ecPointEncodeFormat(this._curve, this._public, format));
    return encoding ? key.toString(encoding) : key;
  }
  setPrivateKey(privateKey, encoding) {
    const d = bytesToBigIntBE(toBytes(privateKey, encoding));
    if (d <= 0n || d >= this._curve.n) throw new RangeError("Private key is not valid for specified curve");
    this._private = d;
    this._public = ecMul(this._curve, d, ecG(this._curve));
  }
  setPublicKey(publicKey, encoding) {
    this._public = ecPointDecode(this._curve, toBytes(publicKey, encoding));
  }
  static convertKey(key, curve, inputEncoding, outputEncoding, format) {
    const C = curveByName(curve);
    const converted = Buffer.from(ecPointEncodeFormat(C, ecPointDecode(C, toBytes(key, inputEncoding)), format));
    return outputEncoding ? converted.toString(outputEncoding) : converted;
  }
}
function createECDH(curve) { return new ECDH(curve); }

function diffieHellman(options) {
  if (!options || !(options.privateKey instanceof KeyObject) || !(options.publicKey instanceof KeyObject)) {
    throw new TypeError("diffieHellman requires privateKey and publicKey KeyObjects");
  }
  const priv = options.privateKey._asym;
  const pub = options.publicKey._asym;
  if (priv.kind === "ec" && pub.kind === "ec") {
    if (priv.d === undefined) throw new Error("diffieHellman privateKey does not contain a private key");
    const C = curveByName(priv.curve);
    const shared = ecToAffine(C, ecMul(C, priv.d, ecPointDecode(C, pub.point)));
    if (!shared) throw new Error("Failed to compute ECDH key");
    return Buffer.from(bigIntToBytesBE(shared[0], C.size));
  }
  if (priv.kind === "x25519" && pub.kind === "x25519" && priv.priv) {
    return Buffer.from(x25519Scalar(priv.priv, pub.pub));
  }
  throw new Error("diffieHellman keys must use the same supported curve");
}

const MODP14_HEX = "ffffffffffffffffc90fdaa22168c234c4c6628b80dc1cd129024e088a67cc74020bbea63b139b22514a08798e3404ddef9519b3cd3a431b302b0a6df25f14374fe1356d6d51c245e485b576625e7ec6f44c42e9a637ed6b0bff5cb6f406b7edee386bfb5a899fa5ae9f24117c4b1fe649286651ece45b3dc2007cb8a163bf0598da48361c55d39a69163fa8fd24cf5f83655d23dca3ad961c62f356208552bb9ed529077096966d670c354e4abc9804f1746c08ca18217c32905e462e36ce3be39e772c180e86039b2783a2ec07a28fb5c55df06f4c52c9de2bcbf6955817183995497cea956ae515d2261898fa051015728e5a8aacaa68ffffffffffffffff";

function dhBigInt(value, encoding) {
  if (typeof value === "bigint") return value;
  if (typeof value === "number") return BigInt(value);
  return bytesToBigIntBE(toBytes(value, encoding));
}
class DiffieHellman {
  constructor(prime, generator = 2) {
    this._prime = prime;
    this._generator = generator;
    this._private = null;
    this._public = null;
    this.verifyError = 0;
  }
  generateKeys(encoding) {
    if (this._private === null) {
      const size = Math.ceil(bitLength(this._prime) / 8);
      do {
        this._private = 2n + bytesToBigIntBE(randomBytes(size)) % (this._prime - 3n);
      } while (this._private <= 1n);
    }
    this._public = modPow(this._generator, this._private, this._prime);
    return this.getPublicKey(encoding);
  }
  computeSecret(otherPublicKey, inputEncoding, outputEncoding) {
    if (this._private === null) throw new Error("No private key - did you forget to generate one?");
    const other = dhBigInt(otherPublicKey, inputEncoding);
    if (other <= 1n || other >= this._prime - 1n) throw new Error("Supplied key is too small");
    const bytes = Buffer.from(bigIntToBytesBE(modPow(other, this._private, this._prime), Math.ceil(bitLength(this._prime) / 8)));
    return outputEncoding ? bytes.toString(outputEncoding) : bytes;
  }
  getPrime(encoding) {
    const bytes = Buffer.from(bigIntToBytesBE(this._prime, Math.ceil(bitLength(this._prime) / 8)));
    return encoding ? bytes.toString(encoding) : bytes;
  }
  getGenerator(encoding) {
    const bytes = Buffer.from(bigIntToBytesBE(this._generator));
    return encoding ? bytes.toString(encoding) : bytes;
  }
  getPublicKey(encoding) {
    if (this._public === null) throw new Error("No public key - did you forget to generate one?");
    const bytes = Buffer.from(bigIntToBytesBE(this._public, Math.ceil(bitLength(this._prime) / 8)));
    return encoding ? bytes.toString(encoding) : bytes;
  }
  getPrivateKey(encoding) {
    if (this._private === null) throw new Error("No private key - did you forget to generate one?");
    const bytes = Buffer.from(bigIntToBytesBE(this._private));
    return encoding ? bytes.toString(encoding) : bytes;
  }
  setPublicKey(key, encoding) { this._public = dhBigInt(key, encoding); }
  setPrivateKey(key, encoding) {
    const value = dhBigInt(key, encoding);
    if (value <= 1n || value >= this._prime - 1n) throw new RangeError("Private key is not valid for specified group");
    this._private = value;
    this._public = modPow(this._generator, value, this._prime);
  }
}
class DiffieHellmanGroup extends DiffieHellman {
  constructor(name) {
    const normalized = String(name).toLowerCase();
    if (normalized !== "modp14") {
      throw new Error(`Unknown DH group '${name}' (lumen supports modp14)`);
    }
    super(BigInt(`0x${MODP14_HEX}`), 2n);
  }
}
function createDiffieHellman(prime, primeEncoding, generator, generatorEncoding) {
  if (typeof prime === "number") {
    const p = generatePrimeSync(prime, { bigint: true, safe: true });
    return new DiffieHellman(p, typeof primeEncoding === "number" ? BigInt(primeEncoding) : 2n);
  }
  if (typeof primeEncoding === "number" || typeof primeEncoding === "bigint") {
    generator = primeEncoding;
    primeEncoding = undefined;
  }
  const p = dhBigInt(prime, primeEncoding);
  const g = generator === undefined ? 2n : dhBigInt(generator, generatorEncoding);
  if (p <= 4n || g <= 1n || g >= p) throw new RangeError("Invalid Diffie-Hellman parameters");
  return new DiffieHellman(p, g);
}
function createDiffieHellmanGroup(name) { return new DiffieHellmanGroup(name); }
function getDiffieHellman(name) { return new DiffieHellmanGroup(name); }

// ---- Ed25519 (RFC 8032) / X25519 (RFC 7748) -----------------------------------------------------

const ED_P = (1n << 255n) - 19n;
const ED_L = (1n << 252n) + 27742317777372353535851937790883648493n;
const ED_D = amod(-121665n * modInv(121666n, ED_P), ED_P);
const ED_SQRT_M1 = modPow(2n, (ED_P - 1n) / 4n, ED_P);
function edRecoverX(y, sign) {
  const y2 = amod(y * y, ED_P);
  const uv = amod(amod(y2 - 1n, ED_P) * modInv(ED_D * y2 + 1n, ED_P), ED_P);
  let x = modPow(uv, (ED_P + 3n) / 8n, ED_P);
  if (amod(x * x - uv, ED_P) !== 0n) x = amod(x * ED_SQRT_M1, ED_P);
  if (amod(x * x - uv, ED_P) !== 0n) return null;
  if ((x & 1n) !== sign) x = amod(-x, ED_P);
  return x;
}
const ED_BY = amod(4n * modInv(5n, ED_P), ED_P);
const ED_BX = edRecoverX(ED_BY, 0n);
const ED_B = [ED_BX, ED_BY, 1n, amod(ED_BX * ED_BY, ED_P)];
function edAdd(P, Q) {
  const [X1, Y1, Z1, T1] = P;
  const [X2, Y2, Z2, T2] = Q;
  const A = amod((Y1 - X1) * (Y2 - X2), ED_P);
  const B = amod((Y1 + X1) * (Y2 + X2), ED_P);
  const Cc = amod(T1 * 2n * ED_D * T2, ED_P);
  const Dd = amod(Z1 * 2n * Z2, ED_P);
  const E = B - A, F = Dd - Cc, G = Dd + Cc, H = B + A;
  return [amod(E * F, ED_P), amod(G * H, ED_P), amod(F * G, ED_P), amod(E * H, ED_P)];
}
function edMul(s, P) {
  let Q = [0n, 1n, 1n, 0n];
  let base = P;
  let k = s;
  while (k > 0n) { if (k & 1n) Q = edAdd(Q, base); base = edAdd(base, base); k >>= 1n; }
  return Q;
}
function edLe2int(bytes) { let n = 0n; for (let i = bytes.length - 1; i >= 0; i--) n = (n << 8n) | BigInt(bytes[i]); return n; }
function edInt2le(n, len) { const o = new Uint8Array(len); let x = n; for (let i = 0; i < len; i++) { o[i] = Number(x & 0xffn); x >>= 8n; } return o; }
function edEncodePoint(P) {
  const zi = modInv(P[2], ED_P);
  const x = amod(P[0] * zi, ED_P);
  const y = amod(P[1] * zi, ED_P);
  const out = edInt2le(y, 32);
  out[31] |= Number(x & 1n) << 7;
  return out;
}
function edDecodePoint(bytes) {
  if (bytes.length !== 32) return null;
  const y = edLe2int(bytes) & ((1n << 255n) - 1n);
  const sign = BigInt(bytes[31] >> 7);
  if (y >= ED_P) return null;
  const x = edRecoverX(y, sign);
  if (x === null) return null;
  return [x, y, 1n, amod(x * y, ED_P)];
}
function edClamp(h) {
  const a = Uint8Array.from(h.subarray(0, 32));
  a[0] &= 248; a[31] &= 127; a[31] |= 64;
  return edLe2int(a);
}
function ed25519PubFromSeed(seed) {
  return edEncodePoint(edMul(edClamp(sha512(seed)), ED_B));
}
function ed25519Sign(seed, msg) {
  const h = sha512(seed);
  const a = edClamp(h);
  const prefix = h.subarray(32, 64);
  const A = edEncodePoint(edMul(a, ED_B));
  const r = amod(edLe2int(sha512(concatBytes(prefix, msg))), ED_L);
  const R = edEncodePoint(edMul(r, ED_B));
  const k = amod(edLe2int(sha512(concatAll([R, A, msg]))), ED_L);
  const S = amod(r + k * a, ED_L);
  return concatBytes(R, edInt2le(S, 32));
}
function ed25519Verify(pub, msg, sig) {
  if (sig.length !== 64) return false;
  const R = sig.subarray(0, 32);
  const S = edLe2int(sig.subarray(32, 64));
  if (S >= ED_L) return false;
  const A = edDecodePoint(pub);
  if (!A) return false;
  const Rp = edDecodePoint(R);
  if (!Rp) return false;
  const k = amod(edLe2int(sha512(concatAll([R, pub, msg]))), ED_L);
  const left = edEncodePoint(edMul(S, ED_B));
  const right = edEncodePoint(edAdd(Rp, edMul(k, A)));
  for (let i = 0; i < 32; i++) if (left[i] !== right[i]) return false;
  return true;
}
// X25519 Montgomery ladder (RFC 7748).
function x25519Scalar(scalarBytes, uBytes) {
  const kBytes = Uint8Array.from(scalarBytes);
  kBytes[0] &= 248; kBytes[31] &= 127; kBytes[31] |= 64;
  const kk = edLe2int(kBytes);
  const u = edLe2int(uBytes) & ((1n << 255n) - 1n);
  let x1 = u, x2 = 1n, z2 = 0n, x3 = u, z3 = 1n, swap = 0n;
  for (let t = 254; t >= 0; t--) {
    const kt = (kk >> BigInt(t)) & 1n;
    swap ^= kt;
    if (swap) { [x2, x3] = [x3, x2]; [z2, z3] = [z3, z2]; }
    swap = kt;
    const A = amod(x2 + z2, ED_P), AA = amod(A * A, ED_P);
    const B = amod(x2 - z2, ED_P), BB = amod(B * B, ED_P);
    const E = amod(AA - BB, ED_P);
    const Cc = amod(x3 + z3, ED_P), Dd = amod(x3 - z3, ED_P);
    const DA = amod(Dd * A, ED_P), CB = amod(Cc * B, ED_P);
    x3 = amod((DA + CB) * (DA + CB), ED_P);
    z3 = amod(x1 * (DA - CB) * (DA - CB), ED_P);
    x2 = amod(AA * BB, ED_P);
    z2 = amod(E * (AA + amod(121665n * E, ED_P)), ED_P);
  }
  if (swap) { x2 = x3; z2 = z3; }
  return edInt2le(amod(x2 * modInv(z2, ED_P), ED_P), 32);
}
function x25519PubFromPriv(priv) { return x25519Scalar(priv, edInt2le(9n, 32)); }

// ---- key structs: parse (SPKI/PKCS#8/PKCS#1/SEC1/JWK) and serialize -----------------------------
// A key struct is { kind: "rsa"|"ed25519"|"x25519"|"ec", ... } — the internal representation all
// asymmetric operations work on.

function parseSpki(der) {
  const seq = derChildren(derRead(der, 0).content);
  const algSeq = derChildren(seq[0].content);
  const oid = decodeOID(algSeq[0].content);
  const pub = seq[1].content.subarray(1); // BIT STRING, drop unused-bits byte
  if (oid === OID.rsa || oid === OID.rsaPss) {
    const rsaSeq = derChildren(derRead(pub, 0).content);
    return { kind: "rsa", n: derInt2Big(rsaSeq[0]), e: derInt2Big(rsaSeq[1]) };
  }
  if (oid === OID.ed25519) return { kind: "ed25519", pub: Uint8Array.from(pub) };
  if (oid === OID.x25519) return { kind: "x25519", pub: Uint8Array.from(pub) };
  if (oid === OID.ecPublicKey) {
    const curveOid = decodeOID(algSeq[1].content);
    if (curveOid !== OID.p256) throw new Error(`EC curve ${CURVE_BY_OID[curveOid] || curveOid} is not supported in lumen (only prime256v1)`);
    return { kind: "ec", curve: "prime256v1", point: Uint8Array.from(pub) };
  }
  throw new Error(`Public key algorithm ${oid} is not supported in lumen`);
}
function parsePkcs1Public(der) {
  const s = derChildren(derRead(der, 0).content);
  return { kind: "rsa", n: derInt2Big(s[0]), e: derInt2Big(s[1]) };
}
function parsePkcs1Private(der) {
  const s = derChildren(derRead(der, 0).content);
  return {
    kind: "rsa", n: derInt2Big(s[1]), e: derInt2Big(s[2]), d: derInt2Big(s[3]),
    p: derInt2Big(s[4]), q: derInt2Big(s[5]), dp: derInt2Big(s[6]), dq: derInt2Big(s[7]), qi: derInt2Big(s[8]),
  };
}
function parseSec1(der, curveHint) {
  const s = derChildren(derRead(der, 0).content);
  const d = bytesToBigIntBE(s[1].content);
  let curve = curveHint;
  let point = null;
  for (let i = 2; i < s.length; i++) {
    if (s[i].tag === 0xa0) curve = CURVE_BY_OID[decodeOID(derChildren(s[i].content)[0].content)] || curve;
    else if (s[i].tag === 0xa1) point = Uint8Array.from(derChildren(s[i].content)[0].content.subarray(1));
  }
  if (curve && curve !== "prime256v1") throw new Error(`EC curve ${curve} is not supported in lumen (only prime256v1)`);
  if (!point) point = ecPubFromPriv(P256, d);
  return { kind: "ec", curve: "prime256v1", d, point };
}
function parsePkcs8(der) {
  const seq = derChildren(derRead(der, 0).content);
  const algSeq = derChildren(seq[1].content);
  const oid = decodeOID(algSeq[0].content);
  const pk = seq[2].content; // OCTET STRING content
  if (oid === OID.rsa || oid === OID.rsaPss) return parsePkcs1Private(pk);
  if (oid === OID.ed25519) {
    const seed = Uint8Array.from(derRead(pk, 0).content);
    return { kind: "ed25519", seed, pub: ed25519PubFromSeed(seed) };
  }
  if (oid === OID.x25519) {
    const priv = Uint8Array.from(derRead(pk, 0).content);
    return { kind: "x25519", priv, pub: x25519PubFromPriv(priv) };
  }
  if (oid === OID.ecPublicKey) {
    const curveOid = algSeq[1] && algSeq[1].tag === 0x06 ? decodeOID(algSeq[1].content) : OID.p256;
    if (curveOid !== OID.p256) throw new Error(`EC curve ${CURVE_BY_OID[curveOid] || curveOid} is not supported in lumen (only prime256v1)`);
    return parseSec1(pk, "prime256v1");
  }
  throw new Error(`Private key algorithm ${oid} is not supported in lumen`);
}

function encodeRsaPublicPkcs1(k) { return derSeq([derIntFromBig(k.n), derIntFromBig(k.e)]); }
function encodeRsaPrivatePkcs1(k) {
  return derSeq([derIntFromBig(0n), derIntFromBig(k.n), derIntFromBig(k.e), derIntFromBig(k.d),
    derIntFromBig(k.p), derIntFromBig(k.q), derIntFromBig(k.dp), derIntFromBig(k.dq), derIntFromBig(k.qi)]);
}
function encodeSec1(key, withParams) {
  const parts = [derIntFromBig(1n), derOctet(bigIntToBytesBE(key.d, 32))];
  if (withParams) parts.push(derTLV(0xa0, derOID(OID.p256)));
  parts.push(derTLV(0xa1, derBitString(key.point)));
  return derSeq(parts);
}
function encodeSpki(key) {
  if (key.kind === "rsa") return derSeq([derSeq([derOID(OID.rsa), derNull()]), derBitString(encodeRsaPublicPkcs1(key))]);
  if (key.kind === "ed25519") return derSeq([derSeq([derOID(OID.ed25519)]), derBitString(key.pub)]);
  if (key.kind === "x25519") return derSeq([derSeq([derOID(OID.x25519)]), derBitString(key.pub)]);
  if (key.kind === "ec") return derSeq([derSeq([derOID(OID.ecPublicKey), derOID(OID.p256)]), derBitString(key.point)]);
  throw new Error("crypto: cannot encode SPKI for this key");
}
function encodePkcs8(key) {
  if (key.kind === "rsa") return derSeq([derIntFromBig(0n), derSeq([derOID(OID.rsa), derNull()]), derOctet(encodeRsaPrivatePkcs1(key))]);
  if (key.kind === "ed25519") return derSeq([derIntFromBig(0n), derSeq([derOID(OID.ed25519)]), derOctet(derOctet(key.seed))]);
  if (key.kind === "x25519") return derSeq([derIntFromBig(0n), derSeq([derOID(OID.x25519)]), derOctet(derOctet(key.priv))]);
  if (key.kind === "ec") return derSeq([derIntFromBig(0n), derSeq([derOID(OID.ecPublicKey), derOID(OID.p256)]), derOctet(encodeSec1(key, false))]);
  throw new Error("crypto: cannot encode PKCS#8 for this key");
}

function b64url(bytes) {
  return Buffer.from(bytes).toString("base64").replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}
function b64urlToBytes(s) {
  return Uint8Array.from(Buffer.from(String(s).replace(/-/g, "+").replace(/_/g, "/"), "base64"));
}
function jwkFromKey(key, isPrivate) {
  if (key.kind === "rsa") {
    const j = { kty: "RSA", n: b64url(bigIntToBytesBE(key.n)), e: b64url(bigIntToBytesBE(key.e)) };
    if (isPrivate) {
      j.d = b64url(bigIntToBytesBE(key.d)); j.p = b64url(bigIntToBytesBE(key.p)); j.q = b64url(bigIntToBytesBE(key.q));
      j.dp = b64url(bigIntToBytesBE(key.dp)); j.dq = b64url(bigIntToBytesBE(key.dq)); j.qi = b64url(bigIntToBytesBE(key.qi));
    }
    return j;
  }
  if (key.kind === "ed25519" || key.kind === "x25519") {
    const j = { kty: "OKP", crv: key.kind === "ed25519" ? "Ed25519" : "X25519", x: b64url(key.pub) };
    if (isPrivate) j.d = b64url(key.kind === "ed25519" ? key.seed : key.priv);
    return j;
  }
  if (key.kind === "ec") {
    const j = {
      kty: "EC", crv: "P-256",
      x: b64url(key.point.subarray(1, 1 + P256.size)), y: b64url(key.point.subarray(1 + P256.size)),
    };
    if (isPrivate) j.d = b64url(bigIntToBytesBE(key.d, 32));
    return j;
  }
  throw new Error("crypto: JWK export is not supported for this key");
}
function keyFromJwk(jwk, needPrivate) {
  if (!jwk || typeof jwk !== "object") throw new TypeError("crypto: JWK must be an object");
  if (needPrivate && jwk.d === undefined) throw new Error("crypto: JWK does not contain a private key");
  if (jwk.kty === "RSA") {
    const n = bytesToBigIntBE(b64urlToBytes(jwk.n)), e = bytesToBigIntBE(b64urlToBytes(jwk.e));
    if (jwk.d !== undefined) {
      return {
        kind: "rsa", n, e, d: bytesToBigIntBE(b64urlToBytes(jwk.d)),
        p: bytesToBigIntBE(b64urlToBytes(jwk.p)), q: bytesToBigIntBE(b64urlToBytes(jwk.q)),
        dp: bytesToBigIntBE(b64urlToBytes(jwk.dp)), dq: bytesToBigIntBE(b64urlToBytes(jwk.dq)),
        qi: bytesToBigIntBE(b64urlToBytes(jwk.qi)),
      };
    }
    return { kind: "rsa", n, e };
  }
  if (jwk.kty === "OKP") {
    const kind = jwk.crv === "Ed25519" ? "ed25519" : jwk.crv === "X25519" ? "x25519" : null;
    if (!kind) throw new Error(`OKP curve '${jwk.crv}' is not supported in lumen`);
    if (jwk.d !== undefined) {
      const secret = b64urlToBytes(jwk.d);
      return kind === "ed25519"
        ? { kind, seed: secret, pub: ed25519PubFromSeed(secret) }
        : { kind, priv: secret, pub: x25519PubFromPriv(secret) };
    }
    return { kind, pub: b64urlToBytes(jwk.x) };
  }
  if (jwk.kty === "EC") {
    if (jwk.crv !== "P-256") throw new Error(`EC curve '${jwk.crv}' is not supported in lumen (only P-256)`);
    const point = concatAll([Uint8Array.of(4), b64urlToBytes(jwk.x), b64urlToBytes(jwk.y)]);
    if (jwk.d !== undefined) return { kind: "ec", curve: "prime256v1", d: bytesToBigIntBE(b64urlToBytes(jwk.d)), point };
    return { kind: "ec", curve: "prime256v1", point };
  }
  throw new Error(`JWK kty '${jwk.kty}' is not supported in lumen`);
}
function structIsPrivate(k) { return k.d !== undefined || k.seed !== undefined || k.priv !== undefined; }
function publicFromPrivateStruct(k) {
  if (k.kind === "rsa") return { kind: "rsa", n: k.n, e: k.e };
  if (k.kind === "ed25519") return { kind: "ed25519", pub: k.pub };
  if (k.kind === "x25519") return { kind: "x25519", pub: k.pub };
  if (k.kind === "ec") return { kind: "ec", curve: k.curve, point: k.point };
  throw new Error("crypto: unknown key kind");
}
function makeKeyObject(type, struct) {
  const ko = new KeyObject(type, null);
  ko._asym = struct;
  return ko;
}

// KeyObject.export for asymmetric keys (the secret-key path stays in the class).
function exportAsymmetricKey(ko, options) {
  if (!options || !options.format) throw new TypeError("KeyObject.export: options.format is required for asymmetric keys");
  const k = ko._asym;
  const isPriv = ko._type === "private";
  if (options.format === "jwk") return jwkFromKey(k, isPriv);
  if (options.cipher || options.passphrase) {
    throw new Error("KeyObject.export: encrypted private-key export is not supported in lumen");
  }
  const type = options.type;
  let der, label;
  if (isPriv) {
    if (type === "pkcs8") { der = encodePkcs8(k); label = "PRIVATE KEY"; }
    else if (type === "pkcs1") {
      if (k.kind !== "rsa") throw new Error("KeyObject.export: 'pkcs1' requires an RSA key");
      der = encodeRsaPrivatePkcs1(k); label = "RSA PRIVATE KEY";
    } else if (type === "sec1") {
      if (k.kind !== "ec") throw new Error("KeyObject.export: 'sec1' requires an EC key");
      der = encodeSec1(k, true); label = "EC PRIVATE KEY";
    } else throw new Error(`KeyObject.export: unsupported private key type '${type}'`);
  } else {
    if (type === "spki") { der = encodeSpki(k); label = "PUBLIC KEY"; }
    else if (type === "pkcs1") {
      if (k.kind !== "rsa") throw new Error("KeyObject.export: 'pkcs1' requires an RSA key");
      der = encodeRsaPublicPkcs1(k); label = "RSA PUBLIC KEY";
    } else throw new Error(`KeyObject.export: unsupported public key type '${type}'`);
  }
  if (options.format === "der") return Buffer.from(der);
  if (options.format === "pem") return pemEncode(label, der);
  throw new Error(`KeyObject.export: unsupported format '${options.format}'`);
}

// Normalize the createPublicKey/createPrivateKey input shapes into { keyObject } or
// { data, format, type }.
function normalizeKeyInput(input) {
  if (input instanceof KeyObject) return { keyObject: input };
  if (typeof input === "string" || input instanceof Uint8Array || input instanceof ArrayBuffer || ArrayBuffer.isView(input)) {
    return { data: input, format: "pem" };
  }
  if (input && typeof input === "object") {
    if (input.key instanceof KeyObject) return { keyObject: input.key };
    if (input.key !== undefined) return { data: input.key, format: input.format || "pem", type: input.type, passphrase: input.passphrase };
  }
  throw new TypeError("crypto: invalid key input");
}
function keyDataToDer(norm) {
  if (norm.passphrase !== undefined) throw new Error("crypto: encrypted private keys are not supported in lumen");
  if (norm.format === "der") return { der: toBytes(norm.data), label: null };
  const text = typeof norm.data === "string" ? norm.data : Buffer.from(toBytes(norm.data)).toString("utf8");
  const p = pemDecode(text);
  return { der: p.der, label: p.label };
}
function createPublicKey(input) {
  const norm = normalizeKeyInput(input);
  if (norm.keyObject) {
    const src = norm.keyObject;
    if (src._type === "public") return src;
    if (src._type !== "private" || !src._asym) throw new TypeError("crypto: cannot derive a public key from this KeyObject");
    return makeKeyObject("public", publicFromPrivateStruct(src._asym));
  }
  let struct;
  if (norm.format === "jwk") {
    struct = keyFromJwk(norm.data, false);
  } else {
    const { der, label } = keyDataToDer(norm);
    const type = norm.type || null;
    if (label === "CERTIFICATE") struct = parseSpki(x509SpkiDer(der));
    else if (label === "RSA PUBLIC KEY" || type === "pkcs1") struct = parsePkcs1Public(der);
    else if (label === "RSA PRIVATE KEY") struct = parsePkcs1Private(der);
    else if (label === "EC PRIVATE KEY") struct = parseSec1(der, "prime256v1");
    else if (label === "PRIVATE KEY") struct = parsePkcs8(der);
    else struct = parseSpki(der); // "PUBLIC KEY" PEM, or DER spki
  }
  if (structIsPrivate(struct)) struct = publicFromPrivateStruct(struct);
  return makeKeyObject("public", struct);
}
function createPrivateKey(input) {
  const norm = normalizeKeyInput(input);
  if (norm.keyObject) {
    if (norm.keyObject._type === "private") return norm.keyObject;
    throw new TypeError("crypto: cannot create a private key from a public key");
  }
  let struct;
  if (norm.format === "jwk") {
    struct = keyFromJwk(norm.data, true);
  } else {
    const { der, label } = keyDataToDer(norm);
    const type = norm.type || null;
    if (label === "RSA PRIVATE KEY" || type === "pkcs1") struct = parsePkcs1Private(der);
    else if (label === "EC PRIVATE KEY" || type === "sec1") struct = parseSec1(der, "prime256v1");
    else struct = parsePkcs8(der); // "PRIVATE KEY" PEM, or DER pkcs8
  }
  if (!structIsPrivate(struct)) throw new Error("crypto: key data does not contain a private key");
  return makeKeyObject("private", struct);
}
// Extract the SubjectPublicKeyInfo DER from a certificate (full parsing lands with
// X509Certificate; this walks straight to the SPKI field).
function x509SpkiDer(certDer) {
  const cert = derRead(certDer, 0);
  const tbs = derRead(cert.content, 0);
  const t = derChildren(tbs.content);
  let idx = 0;
  if (t[0].tag === 0xa0) idx = 1; // explicit version
  idx += 4; // serial, sig alg, issuer, validity
  idx += 1; // subject
  const spki = t[idx];
  return certDer.subarray(cert.start + tbs.start + spki.hstart, cert.start + tbs.start + spki.end);
}

const X509_NAME_OIDS = {
  "2.5.4.3": "CN", "2.5.4.6": "C", "2.5.4.7": "L", "2.5.4.8": "ST",
  "2.5.4.10": "O", "2.5.4.11": "OU", "1.2.840.113549.1.9.1": "emailAddress",
};
const X509_SIGNATURE_OIDS = {
  "1.2.840.113549.1.1.4": "md5",
  "1.2.840.113549.1.1.5": "sha1",
  "1.2.840.113549.1.1.11": "sha256",
  "1.2.840.113549.1.1.12": "sha384",
  "1.2.840.113549.1.1.13": "sha512",
  "1.2.840.10045.4.3.2": "sha256",
  "1.2.840.10045.4.3.3": "sha384",
  "1.2.840.10045.4.3.4": "sha512",
};
function x509String(node) {
  if (node.tag === 0x1e) {
    let out = "";
    for (let i = 0; i + 1 < node.content.length; i += 2) {
      out += String.fromCharCode((node.content[i] << 8) | node.content[i + 1]);
    }
    return out;
  }
  return new TextDecoder().decode(node.content);
}
function x509Name(node) {
  const fields = [];
  for (const set of derChildren(node.content)) {
    for (const sequence of derChildren(set.content)) {
      const pair = derChildren(sequence.content);
      if (pair.length < 2 || pair[0].tag !== 0x06) continue;
      const oid = decodeOID(pair[0].content);
      const name = X509_NAME_OIDS[oid] || oid;
      fields.push(`${name}=${x509String(pair[1]).replace(/\n/g, "\\n")}`);
    }
  }
  return fields.join("\n");
}
function x509Date(node) {
  const text = x509String(node);
  let year, offset;
  if (node.tag === 0x17) {
    const short = Number(text.slice(0, 2));
    year = short >= 50 ? 1900 + short : 2000 + short;
    offset = 2;
  } else if (node.tag === 0x18) {
    year = Number(text.slice(0, 4));
    offset = 4;
  } else {
    throw new Error("X509: unsupported validity time encoding");
  }
  const month = Number(text.slice(offset, offset + 2)) - 1;
  const day = Number(text.slice(offset + 2, offset + 4));
  const hour = Number(text.slice(offset + 4, offset + 6));
  const minute = Number(text.slice(offset + 6, offset + 8));
  const second = Number(text.slice(offset + 8, offset + 10));
  return new Date(Date.UTC(year, month, day, hour, minute, second));
}
function x509DateString(date) {
  const months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
  const two = value => String(value).padStart(2, "0");
  return `${months[date.getUTCMonth()]} ${String(date.getUTCDate()).padStart(2, " ")} ${two(date.getUTCHours())}:${two(date.getUTCMinutes())}:${two(date.getUTCSeconds())} ${date.getUTCFullYear()} GMT`;
}
function x509Fingerprint(bytes, digest) {
  return Buffer.from(resolveHash(digest).fn(bytes)).toString("hex").toUpperCase().match(/../g).join(":");
}
function x509Ip(bytes) {
  if (bytes.length === 4) return Array.from(bytes).join(".");
  if (bytes.length === 16) {
    const groups = [];
    for (let i = 0; i < 16; i += 2) groups.push(((bytes[i] << 8) | bytes[i + 1]).toString(16));
    return groups.join(":");
  }
  return Buffer.from(bytes).toString("hex");
}
function x509Extensions(fields) {
  const result = { ca: false, altNames: [] };
  const wrapper = fields.find(field => field.tag === 0xa3);
  if (!wrapper) return result;
  let extensions;
  try {
    const sequence = derChildren(wrapper.content)[0];
    extensions = derChildren(sequence.content);
  } catch (_error) {
    return result;
  }
  for (const extension of extensions) {
    try {
      const parts = derChildren(extension.content);
      const oid = decodeOID(parts[0].content);
      const value = parts[parts.length - 1];
      if (value.tag !== 0x04) continue;
      const inner = derRead(value.content, 0);
      if (oid === "2.5.29.19" && inner.tag === 0x30) {
        const constraints = derChildren(inner.content);
        result.ca = constraints.some(node => node.tag === 0x01 && node.content[0] !== 0);
      } else if (oid === "2.5.29.17" && inner.tag === 0x30) {
        for (const name of derChildren(inner.content)) {
          if (name.tag === 0x82) result.altNames.push({ type: "DNS", value: x509String(name) });
          else if (name.tag === 0x81) result.altNames.push({ type: "email", value: x509String(name) });
          else if (name.tag === 0x87) result.altNames.push({ type: "IP Address", value: x509Ip(name.content) });
        }
      }
    } catch (_error) {
      // Unknown or malformed non-critical extensions do not invalidate the parsed certificate.
    }
  }
  return result;
}
function x509PublicJwk(key) {
  const jwk = key.export({ format: "jwk" });
  delete jwk.d; delete jwk.p; delete jwk.q; delete jwk.dp; delete jwk.dq; delete jwk.qi;
  return JSON.stringify(jwk);
}

class X509Certificate {
  constructor(buffer) {
    let der;
    if (typeof buffer === "string" || (buffer instanceof Uint8Array && Buffer.from(buffer).toString("utf8").includes("BEGIN CERTIFICATE"))) {
      const decoded = pemDecode(typeof buffer === "string" ? buffer : Buffer.from(buffer).toString("utf8"));
      if (decoded.label !== "CERTIFICATE") throw new Error("X509: expected a CERTIFICATE PEM block");
      der = decoded.der;
    } else {
      der = toBytes(buffer);
    }
    this.raw = Buffer.from(der);
    const cert = derRead(der, 0);
    if (cert.tag !== 0x30 || cert.end !== der.length) throw new Error("X509: invalid certificate DER");
    const outer = derChildren(cert.content);
    if (outer.length !== 3) throw new Error("X509: invalid certificate structure");
    const tbs = outer[0];
    const fields = derChildren(tbs.content);
    let i = fields[0].tag === 0xa0 ? 1 : 0;
    const serial = fields[i++];
    i++; // TBS signature algorithm
    const issuer = fields[i++];
    const validity = derChildren(fields[i++].content);
    const subject = fields[i++];
    const spki = fields[i++];
    this.serialNumber = Buffer.from(serial.content).toString("hex").replace(/^00/, "").toUpperCase() || "00";
    this.issuer = x509Name(issuer);
    this.subject = x509Name(subject);
    this.validFromDate = x509Date(validity[0]);
    this.validToDate = x509Date(validity[1]);
    this.validFrom = x509DateString(this.validFromDate);
    this.validTo = x509DateString(this.validToDate);
    this.fingerprint = x509Fingerprint(der, "sha1");
    this.fingerprint256 = x509Fingerprint(der, "sha256");
    this.fingerprint512 = x509Fingerprint(der, "sha512");
    const spkiDer = tbs.content.subarray(spki.hstart, spki.end);
    this.publicKey = createPublicKey({ key: spkiDer, format: "der", type: "spki" });
    const extensions = x509Extensions(fields.slice(i));
    this.ca = extensions.ca;
    this.keyUsage = undefined;
    this._altNames = extensions.altNames;
    this.subjectAltName = extensions.altNames.length
      ? extensions.altNames.map(name => `${name.type}:${name.value}`).join(", ")
      : undefined;
    this.infoAccess = undefined;
    this.issuerCertificate = undefined;
    this._tbs = tbs.content.length === 0 ? new Uint8Array(0) : cert.content.subarray(tbs.hstart, tbs.end);
    const algorithm = derChildren(outer[1].content);
    this._signatureAlgorithm = algorithm[0] && algorithm[0].tag === 0x06
      ? X509_SIGNATURE_OIDS[decodeOID(algorithm[0].content)]
      : undefined;
    this._signature = outer[2].tag === 0x03 ? outer[2].content.subarray(1) : new Uint8Array(0);
  }
  checkIssued(otherCert) {
    if (!(otherCert instanceof X509Certificate)) throw new TypeError("checkIssued expects an X509Certificate");
    return this.issuer === otherCert.subject;
  }
  checkPrivateKey(privateKey) {
    try { return x509PublicJwk(createPublicKey(privateKey)) === x509PublicJwk(this.publicKey); }
    catch (_error) { return false; }
  }
  verify(publicKey) {
    if (!this._signatureAlgorithm) return false;
    return cryptoVerify(this._signatureAlgorithm, this._tbs, publicKey, this._signature);
  }
  checkHost(name) {
    const host = String(name).toLowerCase();
    let candidates = this._altNames.filter(item => item.type === "DNS").map(item => item.value);
    if (candidates.length === 0) {
      const cn = this.subject.split("\n").find(field => field.startsWith("CN="));
      if (cn) candidates = [cn.slice(3)];
    }
    for (const candidate of candidates) {
      const pattern = candidate.toLowerCase();
      if (pattern === host) return name;
      if (pattern.startsWith("*.") && host.endsWith(pattern.slice(1)) && host.split(".").length === pattern.split(".").length) {
        return name;
      }
    }
    return undefined;
  }
  checkEmail(email) {
    const target = String(email).toLowerCase();
    const match = this._altNames.find(item => item.type === "email" && item.value.toLowerCase() === target);
    return match ? email : undefined;
  }
  checkIP(ip) {
    const target = String(ip).toLowerCase();
    const match = this._altNames.find(item => item.type === "IP Address" && item.value.toLowerCase() === target);
    return match ? ip : undefined;
  }
  toString() { return pemEncode("CERTIFICATE", this.raw); }
  toJSON() { return this.toString(); }
}

// SPKAC is a base64-encoded SignedPublicKeyAndChallenge sequence. Accepting DER as well is useful
// for Buffer callers and mirrors OpenSSL's decoder once the outer base64 armor has been removed.
function parseSpkac(input, encoding) {
  let der = toBytes(input, encoding);
  if (der[0] !== 0x30) {
    let text = Buffer.from(der).toString("ascii").trim();
    if (text.startsWith("SPKAC=")) text = text.slice(6).trim();
    der = Buffer.from(text, "base64");
  }
  const outerNode = derRead(der, 0);
  if (outerNode.tag !== 0x30 || outerNode.end !== der.length) throw new Error("SPKAC: invalid DER");
  const outer = derChildren(outerNode.content);
  if (outer.length !== 3 || outer[0].tag !== 0x30 || outer[1].tag !== 0x30 || outer[2].tag !== 0x03) {
    throw new Error("SPKAC: invalid structure");
  }
  const publicKeyAndChallenge = outer[0];
  const fields = derChildren(publicKeyAndChallenge.content);
  if (fields.length !== 2 || fields[0].tag !== 0x30 || fields[1].tag !== 0x16) {
    throw new Error("SPKAC: invalid public key and challenge");
  }
  const algorithm = derChildren(outer[1].content);
  if (!algorithm[0] || algorithm[0].tag !== 0x06 || outer[2].content[0] !== 0) {
    throw new Error("SPKAC: invalid signature");
  }
  return {
    challenge: fields[1].content,
    publicKeyDer: publicKeyAndChallenge.content.subarray(fields[0].hstart, fields[0].end),
    signed: outerNode.content.subarray(publicKeyAndChallenge.hstart, publicKeyAndChallenge.end),
    signatureAlgorithm: X509_SIGNATURE_OIDS[decodeOID(algorithm[0].content)],
    signature: outer[2].content.subarray(1),
  };
}
function certificateExportChallenge(spkac, encoding) {
  try { return Buffer.from(parseSpkac(spkac, encoding).challenge); }
  catch (_error) { return Buffer.alloc(0); }
}
function certificateExportPublicKey(spkac, encoding) {
  try { return Buffer.from(pemEncode("PUBLIC KEY", parseSpkac(spkac, encoding).publicKeyDer)); }
  catch (_error) { return Buffer.alloc(0); }
}
function certificateVerifySpkac(spkac, encoding) {
  try {
    const parsed = parseSpkac(spkac, encoding);
    if (!parsed.signatureAlgorithm) return false;
    const key = createPublicKey({ key: parsed.publicKeyDer, format: "der", type: "spki" });
    return cryptoVerify(parsed.signatureAlgorithm, parsed.signed, key, parsed.signature);
  } catch (_error) {
    return false;
  }
}
function Certificate() {
  if (!(this instanceof Certificate)) return new Certificate();
}
Certificate.exportChallenge = certificateExportChallenge;
Certificate.exportPublicKey = certificateExportPublicKey;
Certificate.verifySpkac = certificateVerifySpkac;
Certificate.prototype.exportChallenge = certificateExportChallenge;
Certificate.prototype.exportPublicKey = certificateExportPublicKey;
Certificate.prototype.verifySpkac = certificateVerifySpkac;

// ---- MGF1 + RSA (PKCS#1 v1.5, PSS, OAEP) --------------------------------------------------------

function mgf1(seed, len, reg) {
  const out = new Uint8Array(len);
  let off = 0;
  let counter = 0;
  while (off < len) {
    const block = reg.fn(concatBytes(seed, Uint8Array.of(
      (counter >>> 24) & 0xff, (counter >>> 16) & 0xff, (counter >>> 8) & 0xff, counter & 0xff)));
    const n = Math.min(block.length, len - off);
    out.set(block.subarray(0, n), off);
    off += n;
    counter++;
  }
  return out;
}
function xorBytes(a, b) {
  const out = new Uint8Array(a.length);
  for (let i = 0; i < a.length; i++) out[i] = a[i] ^ b[i];
  return out;
}

// DigestInfo prefixes for EMSA-PKCS1-v1_5 (DER of AlgorithmIdentifier + OCTET STRING header).
const DIGEST_INFO_PREFIX = {
  md5: Uint8Array.from([0x30, 0x20, 0x30, 0x0c, 0x06, 0x08, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x05, 0x05, 0x00, 0x04, 0x10]),
  sha1: Uint8Array.from([0x30, 0x21, 0x30, 0x09, 0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00, 0x04, 0x14]),
  sha256: Uint8Array.from([0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05, 0x00, 0x04, 0x20]),
  sha384: Uint8Array.from([0x30, 0x41, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02, 0x05, 0x00, 0x04, 0x30]),
  sha512: Uint8Array.from([0x30, 0x51, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03, 0x05, 0x00, 0x04, 0x40]),
};
function rsaModLen(key) { return Math.ceil(bitLength(key.n) / 8); }
// Public op (encrypt / verify): m^e mod n.
function rsaEP(key, m) {
  if (m >= key.n) throw new Error("RSA: data too large for the modulus");
  return modPow(m, key.e, key.n);
}
// Private op (decrypt / sign) using the CRT parameters when the key carries them.
function rsaDP(key, c) {
  if (c >= key.n) throw new Error("RSA: data too large for the modulus");
  if (key.p && key.q && key.dp && key.dq && key.qi) {
    const m1 = modPow(c, key.dp, key.p);
    const m2 = modPow(c, key.dq, key.q);
    const h = amod(key.qi * (m1 - m2), key.p);
    return amod(m2 + h * key.q, key.n);
  }
  return modPow(c, key.d, key.n);
}
function rsaPkcs1v15Pad(digestName, hashBytes, emLen) {
  const prefix = DIGEST_INFO_PREFIX[digestName];
  if (!prefix) throw new Error(`RSA PKCS#1 v1.5 digest '${digestName}' is not supported in lumen`);
  const T = concatBytes(prefix, hashBytes);
  if (emLen < T.length + 11) throw new Error("RSA: modulus too short for this digest");
  const ps = new Uint8Array(emLen - T.length - 3).fill(0xff);
  return concatAll([Uint8Array.of(0x00, 0x01), ps, Uint8Array.of(0x00), T]);
}
function emsaPssEncode(mHash, emBits, reg, sLen) {
  const hLen = reg.outLen;
  const emLen = Math.ceil(emBits / 8);
  if (emLen < hLen + sLen + 2) throw new Error("RSA-PSS: salt too long for the modulus");
  const salt = sLen > 0 ? Uint8Array.from(randomBytes(sLen)) : new Uint8Array(0);
  const H = reg.fn(concatAll([new Uint8Array(8), mHash, salt]));
  const DB = concatAll([new Uint8Array(emLen - sLen - hLen - 2), Uint8Array.of(0x01), salt]);
  const maskedDB = xorBytes(DB, mgf1(H, emLen - hLen - 1, reg));
  const bits = 8 * emLen - emBits;
  if (bits > 0) maskedDB[0] &= 0xff >> bits;
  return concatAll([maskedDB, H, Uint8Array.of(0xbc)]);
}
// sLen === -1 means auto-detect (Node's RSA_PSS_SALTLEN_AUTO verify default).
function emsaPssVerify(mHash, em, emBits, reg, sLen) {
  const hLen = reg.outLen;
  const emLen = em.length;
  if (emLen < hLen + 2) return false;
  if (em[emLen - 1] !== 0xbc) return false;
  const maskedDB = em.subarray(0, emLen - hLen - 1);
  const H = em.subarray(emLen - hLen - 1, emLen - 1);
  const bits = 8 * emLen - emBits;
  if (bits > 0 && (maskedDB[0] & (0xff << (8 - bits)) & 0xff) !== 0) return false;
  const DB = xorBytes(maskedDB, mgf1(H, emLen - hLen - 1, reg));
  if (bits > 0) DB[0] &= 0xff >> bits;
  let saltStart;
  if (sLen === -1) {
    let i = 0;
    while (i < DB.length && DB[i] === 0) i++;
    if (i >= DB.length || DB[i] !== 0x01) return false;
    saltStart = i + 1;
  } else {
    saltStart = DB.length - sLen;
    for (let i = 0; i < saltStart - 1; i++) if (DB[i] !== 0) return false;
    if (DB[saltStart - 1] !== 0x01) return false;
  }
  const salt = DB.subarray(saltStart);
  const H2 = reg.fn(concatAll([new Uint8Array(8), mHash, salt]));
  for (let i = 0; i < hLen; i++) if (H[i] !== H2[i]) return false;
  return true;
}
function pssSaltLenForSign(opts, reg, key) {
  const sl = opts.saltLength;
  if (sl === undefined || sl === constants.RSA_PSS_SALTLEN_DIGEST) return reg.outLen;
  if (sl === constants.RSA_PSS_SALTLEN_MAX_SIGN) return Math.ceil((bitLength(key.n) - 1) / 8) - reg.outLen - 2;
  return sl;
}
function rsaOaepEncrypt(key, msg, reg, label) {
  const k = rsaModLen(key);
  const hLen = reg.outLen;
  if (msg.length > k - 2 * hLen - 2) throw new Error("RSA-OAEP: message too long");
  const lHash = reg.fn(label || new Uint8Array(0));
  const DB = concatAll([lHash, new Uint8Array(k - msg.length - 2 * hLen - 2), Uint8Array.of(0x01), msg]);
  const seed = Uint8Array.from(randomBytes(hLen));
  const maskedDB = xorBytes(DB, mgf1(seed, k - hLen - 1, reg));
  const maskedSeed = xorBytes(seed, mgf1(maskedDB, hLen, reg));
  const em = concatAll([Uint8Array.of(0x00), maskedSeed, maskedDB]);
  return bigIntToBytesBE(rsaEP(key, bytesToBigIntBE(em)), k);
}
function rsaOaepDecrypt(key, ct, reg, label) {
  const k = rsaModLen(key);
  const hLen = reg.outLen;
  const em = bigIntToBytesBE(rsaDP(key, bytesToBigIntBE(ct)), k);
  const lHash = reg.fn(label || new Uint8Array(0));
  if (em[0] !== 0x00) throw new Error("RSA-OAEP: decryption error");
  const maskedSeed = em.subarray(1, 1 + hLen);
  const maskedDB = em.subarray(1 + hLen);
  const seed = xorBytes(maskedSeed, mgf1(maskedDB, hLen, reg));
  const DB = xorBytes(maskedDB, mgf1(seed, k - hLen - 1, reg));
  for (let i = 0; i < hLen; i++) if (DB[i] !== lHash[i]) throw new Error("RSA-OAEP: decryption error");
  let i = hLen;
  while (i < DB.length && DB[i] === 0x00) i++;
  if (i >= DB.length || DB[i] !== 0x01) throw new Error("RSA-OAEP: decryption error");
  return Uint8Array.from(DB.subarray(i + 1));
}
function rsaPkcs1Encrypt(key, msg) {
  const k = rsaModLen(key);
  if (msg.length > k - 11) throw new Error("RSA-PKCS1: message too long");
  const ps = new Uint8Array(k - msg.length - 3);
  for (let i = 0; i < ps.length; i++) { let b = 0; while (b === 0) b = randomBytes(1)[0]; ps[i] = b; }
  const em = concatAll([Uint8Array.of(0x00, 0x02), ps, Uint8Array.of(0x00), msg]);
  return bigIntToBytesBE(rsaEP(key, bytesToBigIntBE(em)), k);
}
// (No rsaPkcs1Decrypt: Node v22 removed PKCS#1 v1.5 private decryption — Marvin attack — and
// privateDecrypt matches that refusal exactly.)

// ---- probable primes (Miller-Rabin over BigInt) -------------------------------------------------

const SMALL_PRIMES = (() => {
  const out = [];
  const sieve = new Uint8Array(4096);
  for (let i = 2; i < 4096; i++) {
    if (!sieve[i]) { out.push(BigInt(i)); for (let j = i * i; j < 4096; j += i) sieve[j] = 1; }
  }
  return out;
})();
function randomBigIntBits(bits) {
  const bytes = Math.ceil(bits / 8);
  const b = Uint8Array.from(randomBytes(bytes));
  b[0] &= 0xff >> (bytes * 8 - bits);
  return bytesToBigIntBE(b);
}
function millerRabinRound(n, d, r, a) {
  let x = modPow(a, d, n);
  if (x === 1n || x === n - 1n) return true;
  for (let j = 1n; j < r; j++) {
    x = (x * x) % n;
    if (x === n - 1n) return true;
  }
  return false;
}
function isProbablePrime(n, rounds) {
  if (n < 2n) return false;
  for (const sp of SMALL_PRIMES) {
    if (n === sp) return true;
    if (n % sp === 0n) return false;
  }
  let d = n - 1n;
  let r = 0n;
  while ((d & 1n) === 0n) { d >>= 1n; r++; }
  // A fixed base-2 round first: it rejects almost all composites before the random rounds.
  if (!millerRabinRound(n, d, r, 2n)) return false;
  const nBits = bitLength(n);
  for (let i = 0; i < rounds; i++) {
    const a = 2n + randomBigIntBits(nBits) % (n - 3n);
    if (!millerRabinRound(n, d, r, a)) return false;
  }
  return true;
}
function randomPrime(bits, safe, add, rem) {
  if (!Number.isInteger(bits) || bits < 2) throw new RangeError("prime size must be an integer >= 2 bits");
  for (;;) {
    let cand = randomBigIntBits(bits);
    cand |= 1n;
    cand |= 1n << BigInt(bits - 1);
    if (bits >= 3) cand |= 1n << BigInt(bits - 2); // full-size products for RSA
    if (add !== undefined) {
      cand += (rem - cand % add + add) % add;
      if ((cand & 1n) === 0n && (add & 1n) === 1n) cand += add;
      if (bitLength(cand) !== bits) continue;
    }
    if (safe) {
      if ((cand & 3n) !== 3n) continue; // safe primes are ≡ 3 (mod 4)
      if (isProbablePrime((cand - 1n) >> 1n, 8) && isProbablePrime(cand, 20)) return cand;
    } else if (isProbablePrime(cand, 20)) {
      return cand;
    }
  }
}

function checkPrimeSync(candidate, options) {
  let n;
  if (typeof candidate === "bigint") n = candidate;
  else n = bytesToBigIntBE(toBytes(candidate));
  const checks = options && options.checks ? options.checks : 40;
  return isProbablePrime(n, checks);
}
function checkPrime(candidate, options, cb) {
  if (typeof options === "function") { cb = options; options = undefined; }
  if (typeof cb !== "function") throw new TypeError("callback must be a function");
  let res, err;
  try { res = checkPrimeSync(candidate, options); } catch (e) { err = e; }
  queueMicrotask(() => (err ? cb(err) : cb(null, res)));
}
function generatePrimeSync(size, options) {
  options = options || {};
  let add, rem;
  if (options.add !== undefined) {
    add = typeof options.add === "bigint" ? options.add : bytesToBigIntBE(toBytes(options.add));
    rem = options.rem === undefined ? (options.safe ? 3n : 1n)
      : typeof options.rem === "bigint" ? options.rem : bytesToBigIntBE(toBytes(options.rem));
    if (add <= 0n) throw new RangeError("generatePrime option 'add' must be greater than zero");
    if (rem < 0n || rem >= add) throw new RangeError("generatePrime option 'rem' must be non-negative and less than 'add'");
    if (size > 2 && (add & 1n) === 0n && (rem & 1n) === 0n) throw new RangeError("generatePrime add/rem constraints cannot produce an odd prime");
  }
  const p = randomPrime(size, !!options.safe, add, rem);
  if (options.bigint) return p;
  return bigIntToBytesBE(p, Math.ceil(size / 8)).buffer; // Node returns an ArrayBuffer
}
function generatePrime(size, options, cb) {
  if (typeof options === "function") { cb = options; options = undefined; }
  if (typeof cb !== "function") throw new TypeError("callback must be a function");
  let res, err;
  try { res = generatePrimeSync(size, options); } catch (e) { err = e; }
  queueMicrotask(() => (err ? cb(err) : cb(null, res)));
}

// ---- RSA key generation -------------------------------------------------------------------------
// Miller-Rabin probable primes over BigInt. Correct but slow for large moduli (pure JS bignum, no
// Montgomery ladder in the engine yet): 2048-bit generation can take tens of seconds — documented,
// accepted; sign/verify on imported 2048-bit keys is fast enough for real use.

function generateRsa(bits, publicExponent) {
  if (!Number.isInteger(bits) || bits < 512 || bits > 8192) {
    throw new RangeError("RSA modulusLength must be an integer in [512, 8192]");
  }
  const e = BigInt(publicExponent === undefined ? 65537 : publicExponent);
  const pbits = bits >> 1;
  const qbits = bits - pbits;
  for (;;) {
    const p = randomPrime(pbits, false);
    const q = randomPrime(qbits, false);
    if (p === q) continue;
    const n = p * q;
    if (bitLength(n) !== bits) continue;
    const phi = (p - 1n) * (q - 1n);
    let d;
    try { d = modInv(e, phi); } catch (_e) { continue; }
    const [hi, lo] = p > q ? [p, q] : [q, p];
    return {
      kind: "rsa", n, e, d,
      p: hi, q: lo,
      dp: amod(d, hi - 1n), dq: amod(d, lo - 1n), qi: modInv(lo, hi),
    };
  }
}

// ---- public-key encryption ------------------------------------------------------------------------

function oaepReg(opts) { return resolveHash(normalizeDigest(opts.oaepHash || "sha1")); }
function oaepLabel(opts) { return opts.oaepLabel !== undefined ? toBytes(opts.oaepLabel) : undefined; }
function rsaNoPadding(key, data, privateOperation) {
  const k = rsaModLen(key);
  if (data.length !== k) throw new Error("RSA_NO_PADDING requires data with the same size as the modulus");
  const value = bytesToBigIntBE(data);
  const transformed = privateOperation ? rsaDP(key, value) : rsaEP(key, value);
  return bigIntToBytesBE(transformed, k);
}
function publicEncrypt(keyLike, buffer) {
  const opts = keyOptionsFrom(keyLike);
  const struct = createPublicKey(keyLike)._asym;
  if (struct.kind !== "rsa") throw new Error("publicEncrypt: only RSA keys are supported in lumen");
  const data = toBytes(buffer);
  const padding = opts.padding === undefined ? constants.RSA_PKCS1_OAEP_PADDING : opts.padding;
  if (padding === constants.RSA_PKCS1_OAEP_PADDING) return Buffer.from(rsaOaepEncrypt(struct, data, oaepReg(opts), oaepLabel(opts)));
  if (padding === constants.RSA_PKCS1_PADDING) return Buffer.from(rsaPkcs1Encrypt(struct, data));
  if (padding === constants.RSA_NO_PADDING) return Buffer.from(rsaNoPadding(struct, data, false));
  throw new Error(`publicEncrypt: padding ${padding} is not supported in lumen`);
}
function privateDecrypt(keyLike, buffer) {
  const opts = keyOptionsFrom(keyLike);
  const struct = createPrivateKey(keyLike)._asym;
  if (struct.kind !== "rsa") throw new Error("privateDecrypt: only RSA keys are supported in lumen");
  const data = toBytes(buffer);
  const padding = opts.padding === undefined ? constants.RSA_PKCS1_OAEP_PADDING : opts.padding;
  if (padding === constants.RSA_PKCS1_OAEP_PADDING) return Buffer.from(rsaOaepDecrypt(struct, data, oaepReg(opts), oaepLabel(opts)));
  if (padding === constants.RSA_PKCS1_PADDING) {
    // Match Node v22 exactly: PKCS#1 v1.5 private decryption was removed (Marvin attack).
    throw new TypeError("RSA_PKCS1_PADDING is no longer supported for private decryption");
  }
  if (padding === constants.RSA_NO_PADDING) return Buffer.from(rsaNoPadding(struct, data, true));
  throw new Error(`privateDecrypt: padding ${padding} is not supported in lumen`);
}
function privateEncrypt(keyLike, buffer) {
  const opts = keyOptionsFrom(keyLike);
  const struct = createPrivateKey(keyLike)._asym;
  if (struct.kind !== "rsa") throw new Error("privateEncrypt: only RSA keys are supported in lumen");
  const padding = opts.padding === undefined ? constants.RSA_PKCS1_PADDING : opts.padding;
  const data = toBytes(buffer);
  if (padding === constants.RSA_NO_PADDING) return Buffer.from(rsaNoPadding(struct, data, true));
  if (padding !== constants.RSA_PKCS1_PADDING) throw new Error(`privateEncrypt: padding ${padding} is not supported in lumen`);
  const k = rsaModLen(struct);
  const msg = data;
  if (msg.length > k - 11) throw new Error("privateEncrypt: message too long");
  const ps = new Uint8Array(k - msg.length - 3).fill(0xff);
  const em = concatAll([Uint8Array.of(0x00, 0x01), ps, Uint8Array.of(0x00), msg]);
  return Buffer.from(bigIntToBytesBE(rsaDP(struct, bytesToBigIntBE(em)), k));
}
function publicDecrypt(keyLike, buffer) {
  const opts = keyOptionsFrom(keyLike);
  const struct = createPublicKey(keyLike)._asym;
  if (struct.kind !== "rsa") throw new Error("publicDecrypt: only RSA keys are supported in lumen");
  const padding = opts.padding === undefined ? constants.RSA_PKCS1_PADDING : opts.padding;
  const data = toBytes(buffer);
  if (padding === constants.RSA_NO_PADDING) return Buffer.from(rsaNoPadding(struct, data, false));
  if (padding !== constants.RSA_PKCS1_PADDING) throw new Error(`publicDecrypt: padding ${padding} is not supported in lumen`);
  const k = rsaModLen(struct);
  const em = bigIntToBytesBE(rsaEP(struct, bytesToBigIntBE(data)), k);
  if (em[0] !== 0x00 || em[1] !== 0x01) throw new Error("publicDecrypt: decryption error");
  let i = 2;
  while (i < em.length && em[i] === 0xff) i++;
  if (i >= em.length || em[i] !== 0x00) throw new Error("publicDecrypt: decryption error");
  return Buffer.from(em.subarray(i + 1));
}

// ---- streaming Sign / Verify classes --------------------------------------------------------------

class Sign {
  constructor(algorithm) {
    this._algo = normalizeDigest(algorithm);
    resolveHash(this._algo); // validate eagerly, like Node
    this._chunks = [];
  }
  update(data, encoding) { this._chunks.push(toBytes(data, encoding)); return this; }
  sign(keyLike, encoding) {
    let total = 0;
    for (const c of this._chunks) total += c.length;
    const all = new Uint8Array(total);
    let off = 0;
    for (const c of this._chunks) { all.set(c, off); off += c.length; }
    const out = signDigest(this._algo, all, keyLike);
    return encoding && encoding !== "buffer" ? out.toString(encoding) : out;
  }
}
class Verify {
  constructor(algorithm) {
    this._algo = normalizeDigest(algorithm);
    resolveHash(this._algo);
    this._chunks = [];
  }
  update(data, encoding) { this._chunks.push(toBytes(data, encoding)); return this; }
  verify(keyLike, signature, encoding) {
    let total = 0;
    for (const c of this._chunks) total += c.length;
    const all = new Uint8Array(total);
    let off = 0;
    for (const c of this._chunks) { all.set(c, off); off += c.length; }
    const sig = typeof signature === "string" ? Buffer.from(signature, encoding || "hex") : toBytes(signature);
    return verifyDigest(this._algo, all, keyLike, sig);
  }
}

// ---- sign / verify ------------------------------------------------------------------------------

// Extra options (padding, saltLength, dsaEncoding …) ride along on the key argument, as in Node.
function keyOptionsFrom(keyLike) {
  if (keyLike instanceof KeyObject) return {};
  if (typeof keyLike === "string" || keyLike instanceof Uint8Array || ArrayBuffer.isView(keyLike)) return {};
  return keyLike && typeof keyLike === "object" ? keyLike : {};
}
function signDigest(algorithm, data, keyLike) {
  const struct = createPrivateKey(keyLike)._asym;
  const msg = toBytes(data);
  if (struct.kind === "ed25519") {
    if (algorithm != null) throw new Error("crypto.sign: algorithm must be null/undefined for Ed25519 keys");
    return Buffer.from(ed25519Sign(struct.seed, msg));
  }
  if (struct.kind === "x25519") throw new Error("crypto.sign: X25519 keys cannot sign");
  if (algorithm == null) throw new Error(`crypto.sign: a digest algorithm is required for ${struct.kind} keys`);
  const digestName = normalizeDigest(algorithm);
  const reg = resolveHash(digestName);
  const mHash = reg.fn(msg);
  const opts = keyOptionsFrom(keyLike);
  if (struct.kind === "rsa") return Buffer.from(rsaSignHash(struct, digestName, reg, mHash, opts));
  if (struct.kind === "ec") return Buffer.from(ecdsaSignHash(struct, mHash, opts));
  throw new Error(`crypto.sign: unsupported key type '${struct.kind}'`);
}
function verifyDigest(algorithm, data, keyLike, signature) {
  const struct = createPublicKey(keyLike)._asym;
  const msg = toBytes(data);
  const sig = toBytes(signature);
  if (struct.kind === "ed25519") {
    if (algorithm != null) throw new Error("crypto.verify: algorithm must be null/undefined for Ed25519 keys");
    return ed25519Verify(struct.pub, msg, sig);
  }
  if (struct.kind === "x25519") throw new Error("crypto.verify: X25519 keys cannot verify");
  if (algorithm == null) throw new Error(`crypto.verify: a digest algorithm is required for ${struct.kind} keys`);
  const digestName = normalizeDigest(algorithm);
  const reg = resolveHash(digestName);
  const mHash = reg.fn(msg);
  const opts = keyOptionsFrom(keyLike);
  if (struct.kind === "rsa") return rsaVerifyHash(struct, digestName, reg, mHash, sig, opts);
  if (struct.kind === "ec") return ecdsaVerifyHash(struct, mHash, sig, opts);
  throw new Error(`crypto.verify: unsupported key type '${struct.kind}'`);
}
function rsaSignHash(struct, digestName, reg, mHash, opts) {
  if (struct.d === undefined) throw new Error("crypto.sign: an RSA private key is required");
  const padding = opts.padding === undefined ? constants.RSA_PKCS1_PADDING : opts.padding;
  if (padding === constants.RSA_PKCS1_PSS_PADDING) {
    const emBits = bitLength(struct.n) - 1;
    const em = emsaPssEncode(mHash, emBits, reg, pssSaltLenForSign(opts, reg, struct));
    return bigIntToBytesBE(rsaDP(struct, bytesToBigIntBE(em)), rsaModLen(struct));
  }
  if (padding !== constants.RSA_PKCS1_PADDING) throw new Error(`crypto.sign: RSA padding ${padding} is not supported in lumen`);
  const k = rsaModLen(struct);
  const em = rsaPkcs1v15Pad(digestName, mHash, k);
  return bigIntToBytesBE(rsaDP(struct, bytesToBigIntBE(em)), k);
}
function rsaVerifyHash(struct, digestName, reg, mHash, sig, opts) {
  const k = rsaModLen(struct);
  if (sig.length !== k) return false;
  const padding = opts.padding === undefined ? constants.RSA_PKCS1_PADDING : opts.padding;
  let m;
  try { m = rsaEP(struct, bytesToBigIntBE(sig)); } catch (_e) { return false; }
  if (padding === constants.RSA_PKCS1_PSS_PADDING) {
    const emBits = bitLength(struct.n) - 1;
    const em = bigIntToBytesBE(m, Math.ceil(emBits / 8));
    let sLen = opts.saltLength;
    if (sLen === undefined || sLen === constants.RSA_PSS_SALTLEN_AUTO) sLen = -1; // auto-detect
    else if (sLen === constants.RSA_PSS_SALTLEN_DIGEST) sLen = reg.outLen;
    return emsaPssVerify(mHash, em, emBits, reg, sLen);
  }
  if (padding !== constants.RSA_PKCS1_PADDING) throw new Error(`crypto.verify: RSA padding ${padding} is not supported in lumen`);
  const em = bigIntToBytesBE(m, k);
  const expect = rsaPkcs1v15Pad(digestName, mHash, k);
  let diff = 0;
  for (let i = 0; i < k; i++) diff |= em[i] ^ expect[i];
  return diff === 0;
}
function ecdsaHashInt(C, hash) {
  return bytesToBigIntBE(hash.length > C.size ? hash.subarray(0, C.size) : hash);
}
function ecdsaEncoding(opts) {
  const encoding = opts.dsaEncoding === undefined ? "der" : String(opts.dsaEncoding);
  if (encoding !== "der" && encoding !== "ieee-p1363") {
    throw new TypeError(`Invalid ECDSA signature encoding '${encoding}'`);
  }
  return encoding;
}
function ecdsaSignatureEncode(C, r, s, encoding) {
  if (encoding === "ieee-p1363") {
    return concatAll([bigIntToBytesBE(r, C.size), bigIntToBytesBE(s, C.size)]);
  }
  return derSeq([derIntFromBig(r), derIntFromBig(s)]);
}
function ecdsaSignatureDecode(C, signature, encoding) {
  if (encoding === "ieee-p1363") {
    if (signature.length !== C.size * 2) return null;
    return [bytesToBigIntBE(signature.subarray(0, C.size)), bytesToBigIntBE(signature.subarray(C.size))];
  }
  try {
    const seq = derRead(signature, 0);
    if (seq.tag !== 0x30 || seq.end !== signature.length) return null;
    const parts = derChildren(seq.content);
    if (parts.length !== 2 || parts[0].tag !== 0x02 || parts[1].tag !== 0x02) return null;
    return [bytesToBigIntBE(parts[0].content), bytesToBigIntBE(parts[1].content)];
  } catch (_error) {
    return null;
  }
}
function ecdsaSignHash(struct, mHash, opts) {
  if (struct.d === undefined) throw new Error("crypto.sign: an EC private key is required");
  const C = curveByName(struct.curve);
  const z = ecdsaHashInt(C, mHash);
  for (;;) {
    const k = bytesToBigIntBE(randomBytes(C.size)) % C.n;
    if (k === 0n) continue;
    const point = ecToAffine(C, ecMul(C, k, ecG(C)));
    const r = point[0] % C.n;
    if (r === 0n) continue;
    const s = amod(modInv(k, C.n) * (z + r * struct.d), C.n);
    if (s === 0n) continue;
    return ecdsaSignatureEncode(C, r, s, ecdsaEncoding(opts));
  }
}
function ecdsaVerifyHash(struct, mHash, signature, opts) {
  const C = curveByName(struct.curve);
  const decoded = ecdsaSignatureDecode(C, signature, ecdsaEncoding(opts));
  if (!decoded) return false;
  const [r, s] = decoded;
  if (r <= 0n || r >= C.n || s <= 0n || s >= C.n) return false;
  let Q;
  try { Q = ecPointDecode(C, struct.point); } catch (_error) { return false; }
  const w = modInv(s, C.n);
  const u1 = amod(ecdsaHashInt(C, mHash) * w, C.n);
  const u2 = amod(r * w, C.n);
  const point = ecToAffine(C, ecAdd(C, ecMul(C, u1, ecG(C)), ecMul(C, u2, Q)));
  return point !== null && amod(point[0], C.n) === r;
}

function cryptoSign(algorithm, data, key, cb) {
  if (typeof cb === "function") {
    let r, e;
    try { r = signDigest(algorithm, data, key); } catch (x) { e = x; }
    queueMicrotask(() => (e ? cb(e) : cb(null, r)));
    return;
  }
  return signDigest(algorithm, data, key);
}
function cryptoVerify(algorithm, data, key, signature, cb) {
  if (typeof cb === "function") {
    let r, e;
    try { r = verifyDigest(algorithm, data, key, signature); } catch (x) { e = x; }
    queueMicrotask(() => (e ? cb(e) : cb(null, r)));
    return;
  }
  return verifyDigest(algorithm, data, key, signature);
}

// ---- asymmetric key generation ------------------------------------------------------------------

function makeKeyPair(privStruct) {
  return {
    publicKey: makeKeyObject("public", publicFromPrivateStruct(privStruct)),
    privateKey: makeKeyObject("private", privStruct),
  };
}
function generateEd25519() {
  const seed = Uint8Array.from(randomBytes(32));
  return { kind: "ed25519", seed, pub: ed25519PubFromSeed(seed) };
}
function generateX25519() {
  const priv = Uint8Array.from(randomBytes(32));
  return { kind: "x25519", priv, pub: x25519PubFromPriv(priv) };
}
function generateEc(options) {
  const C = curveByName(options && options.namedCurve);
  let d;
  do { d = bytesToBigIntBE(randomBytes(C.size)) % C.n; } while (d === 0n);
  return { kind: "ec", curve: C.name, d, point: ecPubFromPriv(C, d) };
}
function generateKeyPairStruct(type, options) {
  const t = String(type).toLowerCase();
  if (t === "ed25519") return generateEd25519();
  if (t === "x25519") return generateX25519();
  if (t === "rsa") {
    if (!options || !Number.isInteger(options.modulusLength)) {
      throw new TypeError("options.modulusLength is required for RSA key generation");
    }
    return generateRsa(options.modulusLength, options.publicExponent);
  }
  if (t === "ec") return generateEc(options);
  throw new Error(`generateKeyPair type '${type}' is not supported in lumen (ed25519, x25519, rsa, ec)`);
}
function generateKeyPairSync(type, options) {
  const pair = makeKeyPair(generateKeyPairStruct(type, options));
  const pubEnc = options && options.publicKeyEncoding;
  const privEnc = options && options.privateKeyEncoding;
  return {
    publicKey: pubEnc ? pair.publicKey.export(pubEnc) : pair.publicKey,
    privateKey: privEnc ? pair.privateKey.export(privEnc) : pair.privateKey,
  };
}
function generateKeyPair(type, options, cb) {
  if (typeof options === "function") { cb = options; options = undefined; }
  if (typeof cb !== "function") throw new TypeError("callback must be a function");
  let res, err;
  try { res = generateKeyPairSync(type, options); } catch (e) { err = e; }
  queueMicrotask(() => (err ? cb(err) : cb(null, res.publicKey, res.privateKey)));
}
// ---- AES ciphers (native __crypto ops; see lumen-node/src/crypto.rs) ---------------------------
// Real: aes-{128,192,256}-{ecb,cbc,ctr,gcm}, streaming update/final with Node's buffer-holdback,
// PKCS#7 auto-padding, GCM AAD/auth-tag (variable tag lengths) — all bit-exact with Node v22.
// Everything else (chacha20, des, cfb/ofb/ccm/ocb/xts, …) throws honestly, naming the algorithm.

const CIPHERS = {
  "aes-128-ecb": { mode: "ecb", name: "aes-128-ecb", nid: 418, blockSize: 16, keyLength: 16 },
  "aes-128-cbc": { mode: "cbc", name: "aes-128-cbc", nid: 419, blockSize: 16, ivLength: 16, keyLength: 16 },
  "aes-128-ctr": { mode: "ctr", name: "aes-128-ctr", nid: 904, blockSize: 1, ivLength: 16, keyLength: 16 },
  "aes-128-gcm": { mode: "gcm", name: "id-aes128-gcm", nid: 895, blockSize: 1, ivLength: 12, keyLength: 16 },
  "aes-192-ecb": { mode: "ecb", name: "aes-192-ecb", nid: 422, blockSize: 16, keyLength: 24 },
  "aes-192-cbc": { mode: "cbc", name: "aes-192-cbc", nid: 423, blockSize: 16, ivLength: 16, keyLength: 24 },
  "aes-192-ctr": { mode: "ctr", name: "aes-192-ctr", nid: 905, blockSize: 1, ivLength: 16, keyLength: 24 },
  "aes-192-gcm": { mode: "gcm", name: "id-aes192-gcm", nid: 898, blockSize: 1, ivLength: 12, keyLength: 24 },
  "aes-256-ecb": { mode: "ecb", name: "aes-256-ecb", nid: 426, blockSize: 16, keyLength: 32 },
  "aes-256-cbc": { mode: "cbc", name: "aes-256-cbc", nid: 427, blockSize: 16, ivLength: 16, keyLength: 32 },
  "aes-256-ctr": { mode: "ctr", name: "aes-256-ctr", nid: 906, blockSize: 1, ivLength: 16, keyLength: 32 },
  "aes-256-gcm": { mode: "gcm", name: "id-aes256-gcm", nid: 901, blockSize: 1, ivLength: 12, keyLength: 32 },
};
const CIPHER_ALIASES = { aes128: "aes-128-cbc", aes192: "aes-192-cbc", aes256: "aes-256-cbc" };
const GCM_TAG_LENS = [4, 8, 12, 13, 14, 15, 16];

function codedError(Ctor, message, code) {
  const e = new Ctor(message);
  if (code) e.code = code;
  return e;
}

function resolveCipherName(algorithm) {
  let name = String(algorithm).toLowerCase();
  if (CIPHER_ALIASES[name]) name = CIPHER_ALIASES[name];
  return CIPHERS[name];
}

function initCipher(self, algorithm, key, iv, options, isDecipher) {
  const info = resolveCipherName(algorithm);
  if (!info) {
    throw new Error(
      `node:crypto cipher '${algorithm}' is not supported in lumen (aes-{128,192,256}-{ecb,cbc,ctr,gcm} only)`,
    );
  }
  const keyBytes = toBytes(key);
  if (keyBytes.length !== info.keyLength) {
    throw codedError(RangeError, "Invalid key length", "ERR_CRYPTO_INVALID_KEYLEN");
  }
  let ivBytes = iv == null ? null : toBytes(iv);
  if (info.mode === "ecb") {
    if (ivBytes && ivBytes.length !== 0) {
      throw codedError(TypeError, "Invalid initialization vector", "ERR_CRYPTO_INVALID_IV");
    }
    ivBytes = null;
  } else if (info.mode === "gcm") {
    if (!ivBytes || ivBytes.length === 0) {
      throw codedError(TypeError, "Invalid initialization vector", "ERR_CRYPTO_INVALID_IV");
    }
  } else if (!ivBytes || ivBytes.length !== 16) {
    throw codedError(TypeError, "Invalid initialization vector", "ERR_CRYPTO_INVALID_IV");
  }
  self._info = info;
  self._key = Uint8Array.from(keyBytes);
  self._iv = ivBytes ? Uint8Array.from(ivBytes) : null;
  self._decipher = isDecipher;
  self._autoPadding = true;
  self._state = 0; // 0 = init, 1 = updating, 2 = finalized
  self._buf = new Uint8Array(0);
  if (info.mode === "gcm") {
    self._tagLenExplicit = !!(options && options.authTagLength !== undefined);
    if (self._tagLenExplicit) {
      const n = options.authTagLength;
      if (!GCM_TAG_LENS.includes(n)) {
        throw codedError(TypeError, `Invalid authentication tag length: ${n}`, "ERR_CRYPTO_INVALID_AUTH_TAG");
      }
      self._tagLen = n;
    } else {
      self._tagLen = 16;
    }
    self._aad = new Uint8Array(0);
    self._ct = new Uint8Array(0); // accumulated ciphertext, for the tag at final()
    self._counter = Uint8Array.from(__crypto.gcmInit(self._key, self._iv)); // inc32(J0)
    self._ks = new Uint8Array(0);
    self._authTag = null; // decipher: set via setAuthTag
    self._tag = null; // cipher: computed at final()
  } else if (info.mode === "ctr") {
    self._counter = Uint8Array.from(self._iv);
    self._ks = new Uint8Array(0);
  }
}

// Bump the counter block: CTR increments all 128 bits, GCM only the low 32 (SP 800-38D inc32).
function incCounter(block, low32Only) {
  for (let i = 15; i >= (low32Only ? 12 : 0); i--) {
    block[i] = (block[i] + 1) & 0xff;
    if (block[i] !== 0) break;
  }
}

// XOR `input` against the CTR/GCM keystream: use the leftover partial keystream block first, then
// generate the rest in one batched native ECB call over consecutive counter blocks.
function keystreamXor(self, input) {
  const out = new Uint8Array(input.length);
  const left = Math.min(self._ks.length, input.length);
  for (let i = 0; i < left; i++) out[i] = input[i] ^ self._ks[i];
  self._ks = self._ks.subarray(left);
  const remaining = input.length - left;
  if (remaining > 0) {
    const nblocks = Math.ceil(remaining / 16);
    const counters = new Uint8Array(nblocks * 16);
    const low32 = self._info.mode === "gcm";
    for (let b = 0; b < nblocks; b++) {
      counters.set(self._counter, b * 16);
      incCounter(self._counter, low32);
    }
    const stream = __crypto.aesEcb(true, self._key, counters);
    for (let i = 0; i < remaining; i++) out[left + i] = input[left + i] ^ stream[i];
    self._ks = stream.subarray(remaining);
  }
  return out;
}

// ECB/CBC streaming: emit complete blocks, buffering the remainder. A decipher with auto-padding
// additionally holds the last full block back so final() can strip the PKCS#7 padding.
function blockUpdate(self, bytes) {
  const all = concatBytes(self._buf, bytes);
  let n = all.length - (all.length % 16);
  if (self._decipher && self._autoPadding && n > 0 && n === all.length) n -= 16;
  if (n <= 0) {
    self._buf = all;
    return new Uint8Array(0);
  }
  const chunk = all.subarray(0, n);
  self._buf = Uint8Array.from(all.subarray(n));
  let out;
  if (self._info.mode === "ecb") {
    out = __crypto.aesEcb(!self._decipher, self._key, chunk);
  } else {
    out = __crypto.aesCbc(!self._decipher, self._key, self._iv, chunk);
    // CBC chains through the last ciphertext block (output when encrypting, input when decrypting).
    const src = self._decipher ? chunk : out;
    self._iv = Uint8Array.from(src.subarray(src.length - 16));
  }
  return out;
}

function cipherUpdate(self, data, inputEncoding, outputEncoding) {
  if (self._state === 2) throw new Error("Trying to add data in unsupported state");
  self._state = 1;
  const bytes = toBytes(data, inputEncoding || "utf8");
  let out;
  const mode = self._info.mode;
  if (mode === "ecb" || mode === "cbc") {
    out = blockUpdate(self, bytes);
  } else {
    out = keystreamXor(self, bytes);
    if (mode === "gcm") self._ct = concatBytes(self._ct, self._decipher ? bytes : out);
  }
  const buf = Buffer.from(out);
  return outputEncoding && outputEncoding !== "buffer" ? buf.toString(outputEncoding) : buf;
}

const wrongFinalBlock = () =>
  codedError(Error, "error:1C80006B:Provider routines::wrong final block length", "ERR_OSSL_WRONG_FINAL_BLOCK_LENGTH");
const badDecrypt = () =>
  codedError(Error, "error:1C800064:Provider routines::bad decrypt", "ERR_OSSL_BAD_DECRYPT");

function cipherFinal(self, outputEncoding) {
  if (self._state === 2) throw codedError(Error, "Invalid state", "ERR_CRYPTO_INVALID_STATE");
  self._state = 2;
  const mode = self._info.mode;
  let out = new Uint8Array(0);
  if (mode === "ecb" || mode === "cbc") {
    if (!self._decipher) {
      if (self._autoPadding) {
        const padLen = 16 - self._buf.length;
        const block = new Uint8Array(16);
        block.set(self._buf);
        block.fill(padLen, self._buf.length);
        out = mode === "ecb"
          ? __crypto.aesEcb(true, self._key, block)
          : __crypto.aesCbc(true, self._key, self._iv, block);
      } else if (self._buf.length !== 0) {
        throw wrongFinalBlock();
      }
    } else if (self._autoPadding) {
      // The held-back block must be exactly one block; anything else means misaligned input.
      if (self._buf.length !== 16) throw wrongFinalBlock();
      const block = mode === "ecb"
        ? __crypto.aesEcb(false, self._key, self._buf)
        : __crypto.aesCbc(false, self._key, self._iv, self._buf);
      const padLen = block[15];
      let ok = padLen >= 1 && padLen <= 16;
      for (let i = 16 - padLen; ok && i < 16; i++) if (block[i] !== padLen) ok = false;
      if (!ok) throw badDecrypt();
      out = block.subarray(0, 16 - padLen);
    } else if (self._buf.length !== 0) {
      throw wrongFinalBlock();
    }
  } else if (mode === "gcm") {
    const full = __crypto.gcmTag(self._key, self._iv, self._aad, self._ct);
    if (self._decipher) {
      const tag = self._authTag;
      let ok = tag !== null;
      if (ok) {
        let diff = 0;
        for (let i = 0; i < tag.length; i++) diff |= tag[i] ^ full[i];
        ok = diff === 0;
      }
      if (!ok) throw new Error("Unsupported state or unable to authenticate data");
    } else {
      self._tag = Uint8Array.from(full.subarray(0, self._tagLen));
    }
  }
  const buf = Buffer.from(out);
  return outputEncoding && outputEncoding !== "buffer" ? buf.toString(outputEncoding) : buf;
}

function cipherSetAAD(self, buffer, options) {
  if (self._info.mode !== "gcm" || self._state !== 0) {
    throw codedError(Error, "Invalid state for operation setAAD", "ERR_CRYPTO_INVALID_STATE");
  }
  self._aad = concatBytes(self._aad, toBytes(buffer, options && options.encoding));
  return self;
}

class Cipheriv {
  constructor(algorithm, key, iv, options) {
    initCipher(this, algorithm, key, iv, options, false);
  }
  update(data, inputEncoding, outputEncoding) {
    return cipherUpdate(this, data, inputEncoding, outputEncoding);
  }
  final(outputEncoding) {
    return cipherFinal(this, outputEncoding);
  }
  setAutoPadding(autoPadding) {
    this._autoPadding = autoPadding === undefined ? true : !!autoPadding;
    return this;
  }
  setAAD(buffer, options) {
    return cipherSetAAD(this, buffer, options);
  }
  getAuthTag() {
    if (this._info.mode !== "gcm" || this._state !== 2 || this._tag === null) {
      throw codedError(Error, "Invalid state for operation getAuthTag", "ERR_CRYPTO_INVALID_STATE");
    }
    return Buffer.from(this._tag);
  }
}

class Decipheriv {
  constructor(algorithm, key, iv, options) {
    initCipher(this, algorithm, key, iv, options, true);
  }
  update(data, inputEncoding, outputEncoding) {
    return cipherUpdate(this, data, inputEncoding, outputEncoding);
  }
  final(outputEncoding) {
    return cipherFinal(this, outputEncoding);
  }
  setAutoPadding(autoPadding) {
    this._autoPadding = autoPadding === undefined ? true : !!autoPadding;
    return this;
  }
  setAAD(buffer, options) {
    return cipherSetAAD(this, buffer, options);
  }
  setAuthTag(tag, encoding) {
    if (this._info.mode !== "gcm" || this._state === 2) {
      throw codedError(Error, "Invalid state for operation setAuthTag", "ERR_CRYPTO_INVALID_STATE");
    }
    const t = toBytes(tag, encoding);
    const valid = this._tagLenExplicit ? t.length === this._tagLen : GCM_TAG_LENS.includes(t.length);
    if (!valid) {
      throw codedError(TypeError, `Invalid authentication tag length: ${t.length}`, "ERR_CRYPTO_INVALID_AUTH_TAG");
    }
    this._authTag = Uint8Array.from(t);
    return this;
  }
}

// getCipherInfo mirrors Node: name (with the id-aes*-gcm OpenSSL names) or nid lookup; the
// keyLength/ivLength options act as a "does the cipher support this?" probe.
function getCipherInfo(nameOrNid, options) {
  let info;
  if (typeof nameOrNid === "number") {
    info = Object.values(CIPHERS).find((c) => c.nid === nameOrNid);
  } else {
    info = resolveCipherName(nameOrNid);
  }
  if (!info) return undefined;
  const result = { mode: info.mode, name: info.name, nid: info.nid, blockSize: info.blockSize };
  if (info.ivLength !== undefined) result.ivLength = info.ivLength;
  result.keyLength = info.keyLength;
  if (options && options.keyLength !== undefined && options.keyLength !== info.keyLength) return undefined;
  if (options && options.ivLength !== undefined) {
    if (info.mode === "ecb") return undefined;
    if (info.mode === "gcm") {
      if (options.ivLength < 1) return undefined;
      result.ivLength = options.ivLength; // GCM accepts variable IV sizes; Node echoes the query
    } else if (options.ivLength !== 16) {
      return undefined;
    }
  }
  return result;
}

// ---- scrypt (RFC 7914; ROMix is native, PBKDF2-HMAC-SHA256 wrapping is the JS one above) -------

function validateScryptNum(name, v, max) {
  if (typeof v !== "number") {
    const rendered = typeof v === "string" ? `'${v}'` : String(v);
    throw codedError(
      TypeError,
      `The "${name}" argument must be of type number. Received type ${typeof v} (${rendered})`,
      "ERR_INVALID_ARG_TYPE",
    );
  }
  if (!Number.isInteger(v)) {
    throw codedError(RangeError, `The value of "${name}" is out of range. It must be an integer. Received ${v}`, "ERR_OUT_OF_RANGE");
  }
  if (v < 0 || v > max) {
    throw codedError(RangeError, `The value of "${name}" is out of range. It must be >= 0 && <= ${max}. Received ${v}`, "ERR_OUT_OF_RANGE");
  }
  return v;
}

function scryptSync(password, salt, keylen, options) {
  validateScryptNum("keylen", keylen, 2147483647);
  const opts = options || {};
  const pickOpt = (primary, alias) => {
    if (opts[primary] !== undefined && opts[alias] !== undefined) {
      throw codedError(Error, "Invalid scrypt parameter", "ERR_CRYPTO_SCRYPT_INVALID_PARAMETER");
    }
    const name = opts[primary] !== undefined ? primary : alias;
    if (opts[name] === undefined) return undefined;
    return validateScryptNum(name, opts[name], 4294967295);
  };
  // Falsy (0/undefined) falls back to the default, matching Node's `|| default` behavior.
  const N = pickOpt("N", "cost") || 16384;
  const r = pickOpt("r", "blockSize") || 8;
  const p = pickOpt("p", "parallelization") || 1;
  let maxmem = 32 * 1024 * 1024;
  if (opts.maxmem !== undefined) maxmem = validateScryptNum("maxmem", opts.maxmem, Number.MAX_SAFE_INTEGER) || maxmem;
  if (N < 2 || (N & (N - 1)) !== 0) {
    throw codedError(RangeError, "Invalid scrypt params", "ERR_CRYPTO_INVALID_SCRYPT_PARAMS");
  }
  // OpenSSL's memory accounting: B (128*r*p) plus V (128*r*(N+2)) must fit in maxmem.
  if (128 * r * p + 128 * r * (N + 2) > maxmem) {
    throw codedError(
      RangeError,
      "Invalid scrypt params: error:030000AC:digital envelope routines::memory limit exceeded",
      "ERR_CRYPTO_INVALID_SCRYPT_PARAMS",
    );
  }
  const B = pbkdf2Sync(password, salt, 1, 128 * r * p, "sha256");
  const mixed = __crypto.scryptRomix(B, N, r);
  return Buffer.from(pbkdf2Sync(password, mixed, 1, keylen, "sha256"));
}

function scrypt(password, salt, keylen, options, callback) {
  if (typeof options === "function") {
    callback = options;
    options = undefined;
  }
  if (typeof callback !== "function") throw new TypeError("callback must be a function");
  let result, err;
  try {
    result = scryptSync(password, salt, keylen, options);
  } catch (e) {
    err = e;
  }
  queueMicrotask(() => (err ? callback(err) : callback(null, result)));
}

// ---- Argon2 (native RFC 9106 implementation shared with Bun.password) -------------------------

function argon2Parameters(algorithm, parameters, who) {
  if (typeof algorithm !== "string" || !["argon2d", "argon2i", "argon2id"].includes(algorithm)) {
    throw new TypeError(`${who}: algorithm must be 'argon2d', 'argon2i', or 'argon2id'`);
  }
  if (parameters === null || typeof parameters !== "object") {
    throw new TypeError(`${who}: parameters must be an object`);
  }
  const required = ["message", "nonce", "parallelism", "tagLength", "memory", "passes"];
  for (const name of required) {
    if (parameters[name] === undefined) throw new TypeError(`${who}: parameters.${name} is required`);
  }
  const message = Buffer.from(toBytes(parameters.message));
  const nonce = Buffer.from(toBytes(parameters.nonce));
  const integer = (name, min, max) => {
    const value = parameters[name];
    if (!Number.isInteger(value) || value < min || value > max) {
      throw new RangeError(`${who}: parameters.${name} must be an integer between ${min} and ${max}`);
    }
    return value;
  };
  const parallelism = integer("parallelism", 1, 0xffffff);
  const tagLength = integer("tagLength", 4, 0xffffffff);
  const memory = integer("memory", 8 * parallelism, 0xffffffff);
  const passes = integer("passes", 1, 0xffffffff);
  if (nonce.length < 8) throw new RangeError(`${who}: parameters.nonce must be at least 8 bytes`);
  const secret = parameters.secret === undefined ? Buffer.alloc(0) : Buffer.from(toBytes(parameters.secret));
  const associatedData = parameters.associatedData === undefined
    ? Buffer.alloc(0)
    : Buffer.from(toBytes(parameters.associatedData));
  return [algorithm, message, nonce, parallelism, tagLength, memory, passes, secret, associatedData];
}

function argon2Sync(algorithm, parameters) {
  return Buffer.from(__password.argon2Sync(...argon2Parameters(algorithm, parameters, "crypto.argon2Sync")));
}

function argon2(algorithm, parameters, callback) {
  if (typeof callback !== "function") throw new TypeError("crypto.argon2: callback must be a function");
  const args = argon2Parameters(algorithm, parameters, "crypto.argon2");
  __password.argon2(...args,
    (bytes) => callback(null, Buffer.from(bytes)),
    (error) => callback(error));
}

// ---- honest stubs (no native primitive backs these) -------------------------------------------

function notImpl(name) {
  return () => {
    throw new Error(`node:crypto ${name} is not supported in lumen (no native primitive available)`);
  };
}

// ---- constants (real OpenSSL values, captured from Node v22) -----------------------------------

const constants = {
  OPENSSL_VERSION_NUMBER: 805306624,
  SSL_OP_ALL: 2147485776, SSL_OP_ALLOW_NO_DHE_KEX: 1024,
  SSL_OP_ALLOW_UNSAFE_LEGACY_RENEGOTIATION: 262144, SSL_OP_CIPHER_SERVER_PREFERENCE: 4194304,
  SSL_OP_CISCO_ANYCONNECT: 32768, SSL_OP_COOKIE_EXCHANGE: 8192,
  SSL_OP_CRYPTOPRO_TLSEXT_BUG: 2147483648, SSL_OP_DONT_INSERT_EMPTY_FRAGMENTS: 2048,
  SSL_OP_LEGACY_SERVER_CONNECT: 4, SSL_OP_NO_COMPRESSION: 131072,
  SSL_OP_NO_ENCRYPT_THEN_MAC: 524288, SSL_OP_NO_QUERY_MTU: 4096,
  SSL_OP_NO_RENEGOTIATION: 1073741824, SSL_OP_NO_SESSION_RESUMPTION_ON_RENEGOTIATION: 65536,
  SSL_OP_NO_SSLv2: 0, SSL_OP_NO_SSLv3: 33554432, SSL_OP_NO_TICKET: 16384,
  SSL_OP_NO_TLSv1: 67108864, SSL_OP_NO_TLSv1_1: 268435456, SSL_OP_NO_TLSv1_2: 134217728,
  SSL_OP_NO_TLSv1_3: 536870912, SSL_OP_PRIORITIZE_CHACHA: 2097152, SSL_OP_TLS_ROLLBACK_BUG: 8388608,
  ENGINE_METHOD_RSA: 1, ENGINE_METHOD_DSA: 2, ENGINE_METHOD_DH: 4, ENGINE_METHOD_RAND: 8,
  ENGINE_METHOD_EC: 2048, ENGINE_METHOD_CIPHERS: 64, ENGINE_METHOD_DIGESTS: 128,
  ENGINE_METHOD_PKEY_METHS: 512, ENGINE_METHOD_PKEY_ASN1_METHS: 1024, ENGINE_METHOD_ALL: 65535,
  ENGINE_METHOD_NONE: 0,
  DH_CHECK_P_NOT_SAFE_PRIME: 2, DH_CHECK_P_NOT_PRIME: 1, DH_UNABLE_TO_CHECK_GENERATOR: 4,
  DH_NOT_SUITABLE_GENERATOR: 8,
  RSA_PKCS1_PADDING: 1, RSA_NO_PADDING: 3, RSA_PKCS1_OAEP_PADDING: 4, RSA_X931_PADDING: 5,
  RSA_PKCS1_PSS_PADDING: 6, RSA_PSS_SALTLEN_DIGEST: -1, RSA_PSS_SALTLEN_MAX_SIGN: -2,
  RSA_PSS_SALTLEN_AUTO: -2,
  defaultCoreCipherList: "TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256:TLS_AES_128_GCM_SHA256:ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-AES256-GCM-SHA384:DHE-RSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-SHA256:DHE-RSA-AES128-SHA256:ECDHE-RSA-AES256-SHA384:DHE-RSA-AES256-SHA384:ECDHE-RSA-AES256-SHA256:DHE-RSA-AES256-SHA256:HIGH:!aNULL:!eNULL:!EXPORT:!DES:!RC4:!MD5:!PSK:!SRP:!CAMELLIA",
  TLS1_VERSION: 769, TLS1_1_VERSION: 770, TLS1_2_VERSION: 771, TLS1_3_VERSION: 772,
  POINT_CONVERSION_COMPRESSED: 2, POINT_CONVERSION_UNCOMPRESSED: 4, POINT_CONVERSION_HYBRID: 6,
  defaultCipherList: "TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256:TLS_AES_128_GCM_SHA256:ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-AES256-GCM-SHA384:DHE-RSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-SHA256:DHE-RSA-AES128-SHA256:ECDHE-RSA-AES256-SHA384:DHE-RSA-AES256-SHA384:ECDHE-RSA-AES256-SHA256:DHE-RSA-AES256-SHA256:HIGH:!aNULL:!eNULL:!EXPORT:!DES:!RC4:!MD5:!PSK:!SRP:!CAMELLIA",
};

// ---- module surface ---------------------------------------------------------------------------

// Callable without `new`, as Node's constructors are (see __legacyConstructor).
Hash = __legacyConstructor(Hash);
Hmac = __legacyConstructor(Hmac);
ECDH = __legacyConstructor(ECDH);
DiffieHellman = __legacyConstructor(DiffieHellman);
DiffieHellmanGroup = __legacyConstructor(DiffieHellmanGroup);
Sign = __legacyConstructor(Sign);
Verify = __legacyConstructor(Verify);
Cipheriv = __legacyConstructor(Cipheriv);
Decipheriv = __legacyConstructor(Decipheriv);

const crypto = {
  argon2,
  argon2Sync,
  // -- real: hashing / MAC / KDF --
  createHash: (algorithm) => new Hash(algorithm),
  createHmac: (algorithm, key) => new Hmac(algorithm, key),
  Hash,
  Hmac,
  hash,
  getHashes: () => ["md5", "sha1", "sha224", "sha256", "sha384", "sha512", "sha512-224", "sha512-256"],
  pbkdf2,
  pbkdf2Sync,
  hkdf,
  hkdfSync,
  scrypt,
  scryptSync,

  // -- real: randomness --
  randomBytes,
  randomFill,
  randomFillSync,
  randomInt,
  randomUUID: () => webCrypto.randomUUID(),
  getRandomValues: (arr) => webCrypto.getRandomValues(arr),
  timingSafeEqual,

  // -- real: secret keys --
  KeyObject,
  createSecretKey,
  generateKey,
  generateKeySync,

  // -- real: symmetric ciphers (native AES; see lumen-node/src/crypto.rs) --
  createCipheriv: (algorithm, key, iv, options) => new Cipheriv(algorithm, key, iv, options),
  createDecipheriv: (algorithm, key, iv, options) => new Decipheriv(algorithm, key, iv, options),
  Cipheriv,
  Decipheriv,

  // -- real: introspection / config --
  // Introspection lists exactly the cipher and named-curve sets lumen implements.
  getCiphers: () => Object.keys(CIPHERS).slice().sort(),
  getCurves: () => ["P-256", "prime256v1", "secp256r1"],
  getCipherInfo,
  getFips: () => 0,
  setFips: (v) => { if (v) throw new Error("FIPS mode is not supported in lumen"); },
  secureHeapUsed: () => ({ total: 0, min: 0, used: 0, utilization: 0 }),
  constants,

  // -- real: WebCrypto bridge (subtle backs SHA-256 digest + getRandomValues/randomUUID) --
  webcrypto: webCrypto,
  subtle: webCrypto.subtle,

  // -- real: sign/verify (Ed25519 + RSA PKCS#1 v1.5/PSS; ECDSA lands with the EC tier) --
  sign: cryptoSign,
  verify: cryptoVerify,
  createSign: (algorithm) => new Sign(algorithm),
  createVerify: (algorithm) => new Verify(algorithm),
  Sign,
  Verify,
  // -- real: RSA encryption (OAEP + PKCS#1 v1.5) --
  privateEncrypt,
  privateDecrypt,
  publicEncrypt,
  publicDecrypt,

  // -- real: asymmetric key management (ASN.1 DER/PEM/JWK, pure JS) --
  createPublicKey,
  createPrivateKey,
  generateKeyPair,
  generateKeyPairSync,
  // -- real: finite-field DH, P-256 ECDH, and KeyObject diffieHellman (P-256/X25519) --
  DiffieHellman,
  DiffieHellmanGroup,
  ECDH,
  createDiffieHellman,
  createDiffieHellmanGroup,
  createECDH,
  getDiffieHellman,
  diffieHellman,

  // -- real: probable primes (Miller-Rabin over BigInt) --
  checkPrime,
  checkPrimeSync,
  generatePrime,
  generatePrimeSync,
  // -- real: X.509 certificate parsing and signature verification --
  X509Certificate,
  // -- real: legacy SPKAC parsing and verification --
  Certificate,

  // -- stubs: engines --
  setEngine: notImpl("setEngine"),
};

__builtins.set("crypto", crypto);
