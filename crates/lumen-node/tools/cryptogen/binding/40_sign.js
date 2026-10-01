// ---- signatures and RSA encryption ---------------------------------------------------------------
// Sign / Verify / SignJob over `sigSign` / `sigVerify`, and the RSA cipher functions over
// `rsaCipher` (src/crypto/sign.rs).

// `[kind, der]` of a key argument: a KeyObjectHandle, or key material in `format` / `type`.
function resolveKey(kind, data, format, type, passphrase) {
  if (data instanceof KeyObjectHandle) {
    if (data._type === cryptoBinding.kKeyTypeSecret) {
      throw cryptoError(TypeError, "ERR_CRYPTO_INVALID_KEY_OBJECT_TYPE", "Invalid key object type secret, expected private or public.");
    }
    return [data._type, data._data];
  }
  const handle = new KeyObjectHandle();
  handle.init(kind, data, format, type, passphrase);
  return [handle._type, handle._data];
}

function digestOrUndefined(algorithm) {
  if (algorithm === undefined || algorithm === null) return undefined;
  if (__rc.hashInfo(algorithm) === null) throw invalidDigest(algorithm);
  return algorithm;
}

function signBytes(chunks) {
  if (chunks.length === 1) return chunks[0];
  return concatBytes(chunks);
}

function sigSign(key, algorithm, data, padding, salt, dsaEncoding) {
  try {
    return __rc.sigSign(key[0], key[1], algorithm, padding, salt, dsaEncoding === cryptoBinding.kSigEncP1363, data);
  } catch (err) {
    throw cipherError(err);
  }
}

function sigVerify(key, algorithm, data, signature, padding, salt, dsaEncoding) {
  try {
    return __rc.sigVerify(key[0], key[1], algorithm, padding, salt, dsaEncoding === cryptoBinding.kSigEncP1363, data, signature);
  } catch (err) {
    throw cipherError(err);
  }
}

class Sign {
  init(algorithm) {
    if (__rc.hashInfo(algorithm) === null) {
      throw cryptoError(TypeError, "ERR_CRYPTO_INVALID_DIGEST", "Invalid digest");
    }
    this._algorithm = algorithm;
    this._chunks = [];
  }

  update(data, encoding) {
    this._chunks.push(new Uint8Array(dataBytes(data, encoding)));
  }

  sign(data, format, type, passphrase, padding, salt, dsaEncoding) {
    const key = resolveKey(cryptoBinding.kKeyTypePrivate, data, format, type, passphrase);
    const out = sigSign(key, this._algorithm, signBytes(this._chunks.length ? this._chunks : [new Uint8Array(0)]),
                        padding, salt, dsaEncoding);
    return asBuffer(out);
  }
}

class Verify {
  init(algorithm) {
    Sign.prototype.init.call(this, algorithm);
  }

  update(data, encoding) {
    Sign.prototype.update.call(this, data, encoding);
  }

  verify(data, format, type, passphrase, signature, padding, salt, dsaEncoding) {
    const key = resolveKey(cryptoBinding.kKeyTypePublic, data, format, type, passphrase);
    return sigVerify(key, this._algorithm, signBytes(this._chunks.length ? this._chunks : [new Uint8Array(0)]),
                     bytesOf(signature), padding, salt, dsaEncoding);
  }
}

class SignJob extends CryptoJob {
  constructor(mode, signMode, keyData, keyFormat, keyType, keyPassphrase, data, algorithm, saltLength, padding, dsaEncoding, signature) {
    super(mode);
    this.signMode = signMode;
    const sign = signMode === cryptoBinding.kSignJobModeSign;
    this.key = resolveKey(sign ? cryptoBinding.kKeyTypePrivate : cryptoBinding.kKeyTypePublic,
                          keyData, keyFormat, keyType, keyPassphrase);
    this.data = new Uint8Array(bytesOf(data));
    this.algorithm = digestOrUndefined(algorithm);
    this.saltLength = saltLength;
    this.padding = padding;
    this.dsaEncoding = dsaEncoding;
    this.signature = signature === undefined ? undefined : new Uint8Array(bytesOf(signature));
  }

  _run() {
    if (this.signMode === cryptoBinding.kSignJobModeSign) {
      const out = sigSign(this.key, this.algorithm, this.data, this.padding, this.saltLength, this.dsaEncoding);
      return toArrayBuffer(out);
    }
    return sigVerify(this.key, this.algorithm, this.data, this.signature, this.padding, this.saltLength, this.dsaEncoding);
  }
}

function rsaFunction(operation, kind) {
  return (data, format, type, passphrase, buffer, padding, oaepHash, oaepLabel) => {
    const key = resolveKey(kind, data, format, type, passphrase);
    try {
      return asBuffer(__rc.rsaCipher(operation, key[0], key[1], bytesOf(buffer), padding, oaepHash,
                                     oaepLabel === undefined ? undefined : bytesOf(oaepLabel)));
    } catch (err) {
      throw cipherError(err);
    }
  };
}

// WebCrypto RSA-OAEP (crypto_rsa.cc).
class RSACipherJob extends CryptoJob {
  constructor(mode, cipherMode, key, data, variant, hash, label) {
    super(mode);
    this.encrypt = cipherMode === cryptoBinding.kWebCryptoCipherEncrypt;
    this.key = key;
    this.data = new Uint8Array(bytesOf(data));
    this.hash = hash;
    this.label = label === undefined ? undefined : new Uint8Array(bytesOf(label));
  }

  _run() {
    try {
      return toArrayBuffer(__rc.rsaCipher(this.encrypt ? 0 : 1, this.key._type, this.key._data, this.data, 4,
                                          this.hash, this.label));
    } catch (err) {
      throw cipherError(err);
    }
  }
}

Object.assign(cryptoBinding, {
  Sign,
  Verify,
  SignJob,
  RSACipherJob,
  publicEncrypt: rsaFunction(0, cryptoBinding.kKeyTypePublic),
  privateDecrypt: rsaFunction(1, cryptoBinding.kKeyTypePrivate),
  privateEncrypt: rsaFunction(2, cryptoBinding.kKeyTypePrivate),
  publicDecrypt: rsaFunction(3, cryptoBinding.kKeyTypePublic),
});
