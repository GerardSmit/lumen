// ---- digests, HMAC, KDFs, randomness ---------------------------------------------------------

const invalidDigest = (name) =>
  cryptoError(TypeError, "ERR_CRYPTO_INVALID_DIGEST", `Invalid digest: ${name}`);
function cryptoError(Base, code, message) {
  const err = new Base(message);
  err.code = code;
  return err;
}

// Bytes of a secret key argument: a secret KeyObjectHandle or a BufferSource.
function secretBytes(key) {
  if (key instanceof KeyObjectHandle) return key._data;
  if (key instanceof NativeKeyObject) return key[kNativeKeyHandle]._data;
  return bytesOf(key);
}

const hashNames = [
  "RSA-MD5", "RSA-RIPEMD160", "RSA-SHA1", "RSA-SHA1-2", "RSA-SHA224", "RSA-SHA256",
  "RSA-SHA3-224", "RSA-SHA3-256", "RSA-SHA3-384", "RSA-SHA3-512", "RSA-SHA384", "RSA-SHA512",
  "RSA-SHA512/224", "RSA-SHA512/256", "RSA-SM3", "blake2b512", "blake2s256",
  "id-rsassa-pkcs1-v1_5-with-sha3-224", "id-rsassa-pkcs1-v1_5-with-sha3-256",
  "id-rsassa-pkcs1-v1_5-with-sha3-384", "id-rsassa-pkcs1-v1_5-with-sha3-512",
  "md5", "md5-sha1", "md5WithRSAEncryption", "ripemd", "ripemd160", "ripemd160WithRSA", "rmd160",
  "sha1", "sha1WithRSAEncryption", "sha224", "sha224WithRSAEncryption", "sha256",
  "sha256WithRSAEncryption", "sha3-224", "sha3-256", "sha3-384", "sha3-512", "sha384",
  "sha384WithRSAEncryption", "sha512", "sha512-224", "sha512-224WithRSAEncryption", "sha512-256",
  "sha512-256WithRSAEncryption", "sha512WithRSAEncryption", "shake128", "shake256", "sm3",
  "sm3WithRSAEncryption", "ssl3-md5", "ssl3-sha1",
];

class Hash {
  constructor(algorithm, xofLen) {
    if (algorithm instanceof Hash) {
      this._h = algorithm._h.copy(xofLen);
    } else {
      this._h = __rc.hashNew(algorithm, xofLen);
    }
  }

  update(data, encoding) {
    this._h.update(dataBytes(data, encoding));
    return true;
  }

  digest(encoding) {
    return encodeResult(this._h.digest(), encoding);
  }
}

class Hmac {
  init(hash, key) {
    this._h = __rc.hmacNew(hash, secretBytes(key));
  }

  update(data, encoding) {
    this._h.update(dataBytes(data, encoding));
    return true;
  }

  digest(encoding) {
    return encodeResult(this._h.digest(), encoding);
  }
}

class HashJob extends CryptoJob {
  constructor(mode, algorithm, data, length) {
    super(mode);
    if (__rc.hashInfo(algorithm) === null) throw invalidDigest(algorithm);
    this.algorithm = algorithm;
    this.data = new Uint8Array(bytesOf(data));
    this.length = length;
  }

  _run() {
    return toArrayBuffer(__rc.digest(this.algorithm, this.data));
  }
}

class HmacJob extends CryptoJob {
  constructor(mode, signMode, hash, key, data, signature) {
    super(mode);
    const info = __rc.hashInfo(hash);
    if (info === null || info[2]) throw invalidDigest(hash);
    this.signMode = signMode;
    this.hash = hash;
    this.key = secretBytes(key);
    this.data = new Uint8Array(bytesOf(data));
    this.signature = signature === undefined ? undefined : new Uint8Array(bytesOf(signature));
  }

  _run() {
    const mac = __rc.hmac(this.hash, this.key, this.data);
    if (this.signMode === cryptoBinding.kSignJobModeSign) return toArrayBuffer(mac);
    return this.signature.byteLength === mac.byteLength && __rc.timingSafeEqual(mac, this.signature);
  }
}

class PBKDF2Job extends CryptoJob {
  constructor(mode, password, salt, iterations, keylen, digest) {
    super(mode);
    const info = __rc.hashInfo(digest);
    if (info === null || info[2]) throw invalidDigest(digest);
    this.args = [digest, new Uint8Array(bytesOf(password)), new Uint8Array(bytesOf(salt)), iterations, keylen];
  }

  _run() {
    return toArrayBuffer(__rc.pbkdf2(...this.args));
  }

  _runAsync() {
    return __rc.pbkdf2Async(...this.args).then(toArrayBuffer);
  }
}

class ScryptJob extends CryptoJob {
  constructor(mode, password, salt, N, r, p, maxmem, keylen) {
    super(mode);
    if (!__rc.scryptCheck(N, r, p, maxmem)) {
      throw cryptoError(RangeError, "ERR_CRYPTO_INVALID_SCRYPT_PARAMS", "Invalid scrypt params: memory limit exceeded");
    }
    this.args = [new Uint8Array(bytesOf(password)), new Uint8Array(bytesOf(salt)), N, r, p, keylen];
  }

  _run() {
    return toArrayBuffer(__rc.scrypt(...this.args));
  }

  _runAsync() {
    return __rc.scryptAsync(...this.args).then(toArrayBuffer);
  }
}

class HKDFJob extends CryptoJob {
  constructor(mode, hash, key, salt, info, length) {
    super(mode);
    const hi = __rc.hashInfo(hash);
    if (hi === null || hi[2]) throw invalidDigest(hash);
    if (length > 255 * hi[0]) {
      throw cryptoError(RangeError, "ERR_CRYPTO_INVALID_KEYLEN", "Invalid key length");
    }
    this.args = [hash, new Uint8Array(secretBytes(key)), new Uint8Array(bytesOf(salt)), new Uint8Array(bytesOf(info)), length];
  }

  _run() {
    return toArrayBuffer(__rc.hkdf(...this.args));
  }

  _runAsync() {
    return __rc.hkdfAsync(...this.args).then(toArrayBuffer);
  }
}

class Argon2Job extends CryptoJob {
  constructor(mode, message, nonce, parallelism, tagLength, memory, passes, secret, associatedData, type) {
    super(mode);
    const empty = new Uint8Array(0);
    this.args = [type, new Uint8Array(bytesOf(message)), new Uint8Array(bytesOf(nonce)), parallelism, tagLength,
                 memory, passes, secret === undefined ? empty : new Uint8Array(bytesOf(secret)),
                 associatedData === undefined ? empty : new Uint8Array(bytesOf(associatedData))];
  }

  _run() {
    return toArrayBuffer(__rc.argon2(...this.args));
  }

  _runAsync() {
    return __rc.argon2Async(...this.args).then(toArrayBuffer);
  }
}

class RandomBytesJob extends CryptoJob {
  constructor(mode, buffer, offset, size) {
    super(mode);
    this.target = bytesOf(buffer).subarray(offset, offset + size);
  }

  _run() {
    __rc.randomFill(this.target);
    return undefined;
  }
}

function timingSafeEqual(a, b) {
  const isBuf = (v) => v instanceof ArrayBuffer || ArrayBuffer.isView(v);
  if (!isBuf(a)) throw new codes.ERR_INVALID_ARG_TYPE("buf1", ["ArrayBuffer", "Buffer", "TypedArray", "DataView"], a);
  if (!isBuf(b)) throw new codes.ERR_INVALID_ARG_TYPE("buf2", ["ArrayBuffer", "Buffer", "TypedArray", "DataView"], b);
  const x = bytesOf(a);
  const y = bytesOf(b);
  if (x.byteLength !== y.byteLength) {
    throw cryptoError(RangeError, "ERR_CRYPTO_TIMING_SAFE_EQUAL_LENGTH", "Input buffers must have the same byte length");
  }
  return __rc.timingSafeEqual(x, y);
}

Object.assign(cryptoBinding, {
  Hash,
  Hmac,
  HashJob,
  HmacJob,
  PBKDF2Job,
  ScryptJob,
  HKDFJob,
  Argon2Job,
  RandomBytesJob,
  timingSafeEqual,
  getHashes: () => hashNames.slice(),
  secureBuffer: (length) => new Uint8Array(length),
  secureHeapUsed: () => undefined,
  setEngine: undefined,
  getFipsCrypto: () => 0,
  setFipsCrypto(enable) {
    if (enable) {
      throw opensslError("12800067", "DSO support routines", "could not load the shared library", "ERR_OSSL_DSO_COULD_NOT_LOAD_THE_SHARED_LIBRARY");
    }
  },
  testFipsCrypto: () => false,
});
