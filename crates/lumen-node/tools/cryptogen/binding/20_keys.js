// ---- KeyObjectHandle, key generation and WebCrypto key export --------------------------------------
// A handle holds the key as `_type` (kKeyType*) and `_data`: the raw bytes of a secret key, an SPKI
// DER for a public key or a PKCS#8 DER for a private key. Everything else derives from those through
// the native key model (src/crypto/keys).

const kKeyFormatPEMValue = 1;

function hexToArrayBuffer(hex) {
  const bytes = Buffer.from(hex.length % 2 ? "0" + hex : hex, "hex");
  return toArrayBuffer(new Uint8Array(bytes));
}

function asBytes(u8) {
  return Buffer.from(u8.buffer, u8.byteOffset, u8.byteLength);
}

function base64url(bytes) {
  return Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength).toString("base64url");
}

class KeyObjectHandle {
  constructor() {
    this._type = undefined;
    this._data = undefined;
  }

  init(type, data, format, encoding, passphrase) {
    if (type === cryptoBinding.kKeyTypeSecret) {
      this._type = type;
      this._data = new Uint8Array(bytesOf(data));
      return;
    }
    if (data instanceof KeyObjectHandle) {
      if (data._type === cryptoBinding.kKeyTypeSecret) {
        throw cryptoError(TypeError, "ERR_CRYPTO_INVALID_KEY_OBJECT_TYPE", "Invalid key object type secret, expected private or public.");
      }
      if (type === cryptoBinding.kKeyTypePrivate) {
        this._data = data._data;
        this._type = data._type;
      } else {
        this._data = data._type === cryptoBinding.kKeyTypePrivate ? __rc.keyToPublic(data._data) : data._data;
        this._type = cryptoBinding.kKeyTypePublic;
      }
      return;
    }
    try {
      this._data = __rc.keyImport(type, bytesOf(data), format, encoding,
                                  passphrase === undefined ? undefined : bytesOf(passphrase));
    } catch (err) {
      throw cipherError(err);
    }
    this._type = type;
  }

  initJwk(jwk, namedCurve) {
    if (jwk.kty === "oct") {
      if (typeof jwk.k !== "string") {
        throw cryptoError(TypeError, "ERR_CRYPTO_INVALID_JWK", "Invalid JWK secret key format");
      }
      this._type = cryptoBinding.kKeyTypeSecret;
      this._data = new Uint8Array(Buffer.from(jwk.k, "base64"));
      return this._type;
    }
    const fields = [];
    for (const key of Object.keys(jwk)) {
      if (typeof jwk[key] === "string") fields.push(key, jwk[key]);
    }
    let result;
    try {
      result = __rc.keyImportJwk(fields, namedCurve);
    } catch (err) {
      throw cipherError(err);
    }
    this._type = result[0];
    this._data = new Uint8Array(result[1]);
    return this._type;
  }

  initEDRaw(name, data, type) {
    const der = __rc.keyImportOkpRaw(name, bytesOf(data), type === cryptoBinding.kKeyTypePrivate);
    if (der === null) return false;
    this._type = type;
    this._data = new Uint8Array(der);
    return true;
  }

  initECRaw(curve, data) {
    const der = __rc.keyImportEcRaw(curve, bytesOf(data));
    if (der === null) return false;
    this._type = cryptoBinding.kKeyTypePublic;
    this._data = new Uint8Array(der);
    return true;
  }

  getSymmetricKeySize() {
    return this._data.byteLength;
  }

  getAsymmetricKeyType() {
    return __rc.keyType(this._type, this._data);
  }

  keyDetail(target) {
    if (this._type === cryptoBinding.kKeyTypeSecret) {
      target.length = this._data.byteLength * 8;
      return target;
    }
    const flat = __rc.keyDetail(this._type, this._data);
    for (let i = 0; i < flat.length; i += 2) {
      const name = flat[i];
      const value = flat[i + 1];
      if (name === "publicExponent") target[name] = hexToArrayBuffer(value);
      else if (name === "modulusLength" || name === "divisorLength" || name === "saltLength") target[name] = Number(value);
      else target[name] = value;
    }
    return target;
  }

  export(format, type, cipher, passphrase) {
    if (this._type === cryptoBinding.kKeyTypeSecret) return Buffer.from(this._data.slice().buffer);
    const encoding = type === undefined ? (this._type === cryptoBinding.kKeyTypePrivate ? 1 : 2) : type;
    let out;
    try {
      out = __rc.keyExport(this._type, this._data, format, encoding, cipher,
                           passphrase === undefined ? undefined : bytesOf(passphrase));
    } catch (err) {
      throw cipherError(err);
    }
    const buf = asBytes(out);
    return format === kKeyFormatPEMValue ? buf.toString("latin1") : buf;
  }

  exportJwk(target, handleRsaPss) {
    if (this._type === cryptoBinding.kKeyTypeSecret) {
      target.kty = "oct";
      target.k = base64url(this._data);
      return target;
    }
    const flat = __rc.keyExportJwk(this._type, this._data, handleRsaPss === true);
    for (let i = 0; i < flat.length; i += 2) target[flat[i]] = flat[i + 1];
    return target;
  }

  checkEcKeyData() {
    return __rc.keyCheckEc(this._type, this._data);
  }

  equals(other) {
    if (this._type !== other._type) return false;
    if (this._type === cryptoBinding.kKeyTypeSecret) {
      return this._data.byteLength === other._data.byteLength && __rc.timingSafeEqual(this._data, other._data);
    }
    return __rc.keyEquals(this._type, this._data, other._data);
  }
}

function handleOf(type, data) {
  const handle = new KeyObjectHandle();
  handle._type = type;
  handle._data = new Uint8Array(data);
  return handle;
}

// Key pair results: a handle per key when no encoding was requested, else the exported key.
function exportPairKey(handle, format, type, cipher, passphrase) {
  if (format === undefined) return handle;
  if (format === cryptoBinding.kKeyFormatJWK) return handle.exportJwk({}, false);
  return handle.export(format, type, cipher, passphrase);
}

class KeyPairGenJob extends CryptoJob {
  constructor(mode, encoding) {
    super(mode);
    this.encoding = encoding;
    if (typeof this._generateAsync === "function") {
      this._runAsync = () => this._generateAsync().then((pair) => this._finish(pair), (err) => {
        throw cipherError(err);
      });
    }
  }

  _finish(pair) {
    const { 0: publicFormat, 1: publicType, 2: privateFormat, 3: privateType, 4: cipher, 5: passphrase } = this.encoding;
    return [
      exportPairKey(handleOf(cryptoBinding.kKeyTypePublic, pair[0]), publicFormat, publicType),
      exportPairKey(handleOf(cryptoBinding.kKeyTypePrivate, pair[1]), privateFormat, privateType, cipher, passphrase),
    ];
  }

  _run() {
    try {
      return this._finish(this._generate());
    } catch (err) {
      throw cipherError(err);
    }
  }
}

class RsaKeyPairGenJob extends KeyPairGenJob {
  constructor(mode, variant, modulusLength, publicExponent, ...rest) {
    const pss = variant === cryptoBinding.kKeyVariantRSA_PSS;
    const options = pss ? rest.slice(0, 3) : [];
    super(mode, pss ? rest.slice(3) : rest);
    this.args = [modulusLength, publicExponent, pss, ...(pss ? options : [undefined, undefined, undefined])];
  }

  _generate() {
    return __rc.keygenRsa(...this.args);
  }

  _generateAsync() {
    return __rc.keygenRsaAsync(...this.args);
  }
}

class DsaKeyPairGenJob extends KeyPairGenJob {
  constructor(mode, modulusLength, divisorLength, ...encoding) {
    super(mode, encoding);
    this.args = [modulusLength, divisorLength];
  }

  _generate() {
    return __rc.keygenDsa(...this.args);
  }

  _generateAsync() {
    return __rc.keygenDsaAsync(...this.args);
  }
}

class EcKeyPairGenJob extends KeyPairGenJob {
  constructor(mode, namedCurve, paramEncoding, ...encoding) {
    super(mode, encoding);
    this.args = [namedCurve, paramEncoding === cryptoBinding.OPENSSL_EC_EXPLICIT_CURVE];
  }

  _generate() {
    return __rc.keygenEc(...this.args);
  }
}

const nidTypes = {
  [cryptoBinding.EVP_PKEY_ED25519]: "ed25519",
  [cryptoBinding.EVP_PKEY_ED448]: "ed448",
  [cryptoBinding.EVP_PKEY_X25519]: "x25519",
  [cryptoBinding.EVP_PKEY_X448]: "x448",
};

class NidKeyPairGenJob extends KeyPairGenJob {
  constructor(mode, id, ...encoding) {
    super(mode, encoding);
    this.kind = nidTypes[id];
  }

  _generate() {
    return __rc.keygenOkp(this.kind);
  }
}

class DhKeyPairGenJob extends KeyPairGenJob {
  constructor(mode, first, ...rest) {
    if (typeof first === "string") {
      super(mode, rest);
      this.args = [first, undefined, undefined, 2];
      return;
    }
    const generator = rest[0];
    super(mode, rest.slice(1));
    this.args = typeof first === "number" ?
      [undefined, undefined, first, generator] :
      [undefined, bytesOf(first), undefined, generator];
  }

  _generate() {
    return __rc.keygenDh(...this.args);
  }

  _generateAsync() {
    return __rc.keygenDhAsync(...this.args);
  }
}

class SecretKeyGenJob extends CryptoJob {
  constructor(mode, bits) {
    super(mode);
    this.bits = bits;
  }

  _run() {
    const bytes = new Uint8Array(this.bits >> 3);
    __rc.randomFill(bytes);
    return handleOf(cryptoBinding.kKeyTypeSecret, bytes);
  }
}

function keyDer(handle, wantPublic) {
  if (wantPublic && handle._type === cryptoBinding.kKeyTypePrivate) return __rc.keyToPublic(handle._data);
  return handle._data;
}

// WebCrypto `exportKey` for EC / CFRG keys (crypto_ec.cc): raw point, PKCS#8 or SPKI.
class ECKeyExportJob extends CryptoJob {
  constructor(mode, format, handle) {
    super(mode);
    this.format = format;
    this.handle = handle;
  }

  _run() {
    const { format, handle } = this;
    switch (format) {
      case cryptoBinding.kWebCryptoKeyFormatRaw:
        return toArrayBuffer(__rc.keyRawPublic(keyDer(handle, true)));
      case cryptoBinding.kWebCryptoKeyFormatPKCS8:
        if (handle._type !== cryptoBinding.kKeyTypePrivate) throw cryptoError(Error, "ERR_CRYPTO_INVALID_KEYTYPE", "Invalid key type");
        return toArrayBuffer(handle._data);
      case cryptoBinding.kWebCryptoKeyFormatSPKI:
        return toArrayBuffer(keyDer(handle, true));
    }
    throw cryptoError(Error, "ERR_CRYPTO_INVALID_KEYTYPE", "Unsupported key export format");
  }
}

// WebCrypto `exportKey` for RSA keys (crypto_rsa.cc): PKCS#8 or SPKI.
class RSAKeyExportJob extends CryptoJob {
  constructor(mode, format, handle, variant) {
    super(mode);
    this.format = format;
    this.handle = handle;
    this.variant = variant;
  }

  _run() {
    const { format, handle } = this;
    switch (format) {
      case cryptoBinding.kWebCryptoKeyFormatPKCS8:
        if (handle._type !== cryptoBinding.kKeyTypePrivate) throw cryptoError(Error, "ERR_CRYPTO_INVALID_KEYTYPE", "Invalid key type");
        return toArrayBuffer(handle._data);
      case cryptoBinding.kWebCryptoKeyFormatSPKI:
        return toArrayBuffer(keyDer(handle, true));
    }
    throw cryptoError(Error, "ERR_CRYPTO_INVALID_KEYTYPE", "Unsupported key export format");
  }
}

// The base of KeyObject. Like Node's C++ NativeKeyObject it holds the handle out of sight and
// clones across threads (structured clone rebuilds it with `keyObjectFromClone`).
const kNativeKeyHandle = Symbol("kNativeKeyHandle");
let keyObjectClasses;
class NativeKeyObject {
  constructor(handle) {
    Object.defineProperty(this, kNativeKeyHandle, { value: handle });
  }

  [kClone]() {
    const handle = this[kNativeKeyHandle];
    return {
      data: { kind: handle._type, bytes: handle._data },
      deserializeInfo: "internal/crypto/keys:keyObjectFromClone",
    };
  }
}

function createNativeKeyObjectClass(callback) {
  keyObjectClasses = callback(NativeKeyObject);
  return keyObjectClasses;
}

function keyObjectFromClone({ kind, bytes }) {
  const handle = handleOf(kind, bytes);
  if (kind === cryptoBinding.kKeyTypeSecret) return new keyObjectClasses[1](handle);
  if (kind === cryptoBinding.kKeyTypePublic) return new keyObjectClasses[2](handle);
  return new keyObjectClasses[3](handle);
}

Object.assign(cryptoBinding, {
  KeyObjectHandle,
  SecretKeyGenJob,
  RsaKeyPairGenJob,
  DsaKeyPairGenJob,
  EcKeyPairGenJob,
  NidKeyPairGenJob,
  DhKeyPairGenJob,
  ECKeyExportJob,
  RSAKeyExportJob,
  createNativeKeyObjectClass,
});
