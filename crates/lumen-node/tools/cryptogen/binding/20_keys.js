// ---- KeyObjectHandle and key generation -----------------------------------------------------------
// A handle holds the key as `_type` (kKeyType*) and `_data`: the raw bytes of a secret key, an SPKI
// DER for a public key or a PKCS#8 DER for a private key. Everything else derives from those.

class KeyObjectHandle {
  constructor() {
    this._type = undefined;
    this._data = undefined;
  }

  init(type, data) {
    this._type = type;
    this._data = new Uint8Array(bytesOf(data));
  }

  getSymmetricKeySize() {
    return this._data.byteLength;
  }

  export() {
    return Buffer.from(this._data);
  }

  equals(other) {
    return this._type === other._type && this._data.byteLength === other._data.byteLength &&
      __rc.timingSafeEqual(this._data, other._data);
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
    const handle = new KeyObjectHandle();
    handle.init(cryptoBinding.kKeyTypeSecret, bytes);
    return handle;
  }
}

function createNativeKeyObjectClass(callback) {
  class NativeKeyObject {
    constructor(handle) {
      this.handle = handle;
    }
  }
  return callback(NativeKeyObject);
}

Object.assign(cryptoBinding, { KeyObjectHandle, SecretKeyGenJob, createNativeKeyObjectClass });
