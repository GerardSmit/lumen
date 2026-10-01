// ---- internalBinding('crypto'): shared plumbing ---------------------------------------------------
// `__rc` is the native op table (src/crypto/*.rs). The binding files below add Node's C++ classes
// and jobs to `cryptoBinding`; keys are JS `KeyObjectHandle`s holding raw bytes / DER (20_keys.js).

const __rc = __cryptoBinding();
const cryptoBinding = {};
const kCryptoJobAsync = 0;
const kCryptoJobSync = 1;
Object.assign(cryptoBinding, {
  kCryptoJobAsync,
  kCryptoJobSync,
  kKeyTypeSecret: 0,
  kKeyTypePublic: 1,
  kKeyTypePrivate: 2,
  kKeyFormatDER: 0,
  kKeyFormatPEM: 1,
  kKeyFormatJWK: 2,
  kKeyEncodingPKCS1: 0,
  kKeyEncodingPKCS8: 1,
  kKeyEncodingSPKI: 2,
  kKeyEncodingSEC1: 3,
  kSigEncDER: 0,
  kSigEncP1363: 1,
  kSignJobModeSign: 0,
  kSignJobModeVerify: 1,
  kWebCryptoKeyFormatRaw: 0,
  kWebCryptoKeyFormatPKCS8: 1,
  kWebCryptoKeyFormatSPKI: 2,
  kWebCryptoKeyFormatJWK: 3,
  kWebCryptoCipherEncrypt: 0,
  kWebCryptoCipherDecrypt: 1,
  kKeyVariantRSA_SSA_PKCS1_v1_5: 0,
  kKeyVariantRSA_PSS: 1,
  kKeyVariantRSA_OAEP: 2,
  kKeyVariantAES_CTR_128: 0,
  kKeyVariantAES_CTR_192: 1,
  kKeyVariantAES_CTR_256: 2,
  kKeyVariantAES_CBC_128: 3,
  kKeyVariantAES_CBC_192: 4,
  kKeyVariantAES_CBC_256: 5,
  kKeyVariantAES_GCM_128: 6,
  kKeyVariantAES_GCM_192: 7,
  kKeyVariantAES_GCM_256: 8,
  kKeyVariantAES_KW_128: 9,
  kKeyVariantAES_KW_192: 10,
  kKeyVariantAES_KW_256: 11,
  EVP_PKEY_ED25519: 1087,
  EVP_PKEY_ED448: 1088,
  EVP_PKEY_X25519: 1034,
  EVP_PKEY_X448: 1035,
  OPENSSL_EC_NAMED_CURVE: 1,
  OPENSSL_EC_EXPLICIT_CURVE: 0,
  RSA_PKCS1_PSS_PADDING: 6,
  X509_CHECK_FLAG_ALWAYS_CHECK_SUBJECT: 1,
  X509_CHECK_FLAG_NO_WILDCARDS: 2,
  X509_CHECK_FLAG_NO_PARTIAL_WILDCARDS: 4,
  X509_CHECK_FLAG_MULTI_LABEL_WILDCARDS: 8,
  X509_CHECK_FLAG_SINGLE_LABEL_SUBDOMAINS: 16,
  X509_CHECK_FLAG_NEVER_CHECK_SUBJECT: 32,
});

// `internalBinding('constants').crypto`: the crypto half of node:constants.
const cryptoConstants = (() => {
  const all = __builtins.get("constants");
  const out = {};
  for (const key of Object.keys(all)) {
    if (/^(OPENSSL_|SSL_OP_|ENGINE_METHOD_|DH_|RSA_|TLS\d|POINT_CONVERSION_|defaultCoreCipherList)/.test(key)) {
      out[key] = all[key];
    }
  }
  return out;
})();

function internalBinding(name) {
  switch (name) {
    case "crypto": return cryptoBinding;
    case "constants": return { crypto: cryptoConstants, os: { signals: __builtins.get("os").constants.signals } };
  }
  throw new Error(`lumen crypto: internalBinding('${name}') is not available`);
}

// Bytes of an ArrayBuffer / view / string-free input as a Uint8Array view (no copy).
function bytesOf(data) {
  if (data instanceof Uint8Array) return data;
  if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  if (data instanceof ArrayBuffer || (typeof SharedArrayBuffer === "function" && data instanceof SharedArrayBuffer)) {
    return new Uint8Array(data);
  }
  return data;
}
// An ArrayBuffer holding exactly the bytes of `u8`.
function toArrayBuffer(u8) {
  if (u8.byteOffset === 0 && u8.byteLength === u8.buffer.byteLength) return u8.buffer;
  return u8.buffer.slice(u8.byteOffset, u8.byteOffset + u8.byteLength);
}
// Input of the `update(data, encoding)` style: a string in `encoding`, or a view.
function dataBytes(data, encoding) {
  if (typeof data === "string") {
    if (encoding === "buffer" || encoding === undefined) encoding = "utf8";
    return Buffer.from(data, encoding);
  }
  return bytesOf(data);
}
// A byte result as Node returns it from `digest(enc)`/`final(enc)`: a Buffer, or an encoded string.
function encodeResult(u8, encoding) {
  const buf = Buffer.from(u8.buffer, u8.byteOffset, u8.byteLength);
  if (encoding === undefined || encoding === "buffer" || !Buffer.isEncoding(encoding)) return buf;
  return buf.toString(encoding);
}

// OpenSSL-flavoured errors: `error:<hex>:<lib>::<reason>` messages with the `library`, `reason` and
// `code: ERR_OSSL_*` fields Node attaches.
function opensslError(hex, library, reason, code, Base = Error) {
  const err = new Base(`error:${hex}:${library}::${reason}`);
  err.library = library;
  err.reason = reason;
  err.code = code || `ERR_OSSL_${reason.toUpperCase().replace(/[^A-Z0-9]+/g, "_")}`;
  return err;
}

// Base of every CryptoJob: `run()` is synchronous (returns [err, result]) or schedules `ondone`.
// Subclasses implement `_run()` (a result or a throw) and may implement `_runAsync()` returning a
// Promise when the work belongs on the native worker pool.
class CryptoJob {
  constructor(mode) {
    this.mode = mode;
    this.ondone = undefined;
  }

  run() {
    if (this.mode === kCryptoJobSync) {
      try {
        return [undefined, this._run()];
      } catch (err) {
        return [err, undefined];
      }
    }
    const pending = typeof this._runAsync === "function" ?
      this._runAsync() :
      new Promise((resolve, reject) => {
        setImmediate(() => {
          try {
            resolve(this._run());
          } catch (err) {
            reject(err);
          }
        });
      });
    pending.then(
      (result) => process.nextTick(() => this.ondone(undefined, result)),
      (err) => process.nextTick(() => this.ondone(err, undefined)),
    );
  }
}
