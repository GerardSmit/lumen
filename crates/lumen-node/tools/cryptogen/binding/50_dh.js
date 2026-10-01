// ---- Diffie-Hellman, ECDH and stateless key agreement ----------------------------------------------
// The classes keep their key material as bytes; the arithmetic is `dh*` / `ecdh*` / `statelessDh`
// (src/crypto/dh.rs).

function invalidState(message) {
  return cryptoError(Error, "ERR_CRYPTO_INVALID_STATE", message);
}

function badGenerator() {
  return opensslError("02800080", "Diffie-Hellman routines", "bad generator", "ERR_OSSL_DH_BAD_GENERATOR");
}

function numberBytes(n) {
  const hex = n.toString(16);
  return new Uint8Array(Buffer.from(hex.length % 2 ? "0" + hex : hex, "hex"));
}

class DiffieHellmanBase {
  _init(prime, generator) {
    this._p = prime;
    this._g = generator;
    this._priv = undefined;
    this._pub = undefined;
  }

  get verifyError() {
    return __rc.dhVerify(this._p, this._g);
  }

  generateKeys() {
    try {
      const [x, y] = __rc.dhGenKey(this._p, this._g, this._priv);
      this._priv = new Uint8Array(x);
      this._pub = new Uint8Array(y);
    } catch (err) {
      throw cipherError(err);
    }
    return asBuffer(this._pub);
  }

  computeSecret(key) {
    if (this._priv === undefined) throw cryptoError(Error, "ERR_CRYPTO_OPERATION_FAILED", "Failed to compute DH key");
    return asBuffer(__rc.dhCompute(this._p, this._priv, bytesOf(key)));
  }

  getPrime() {
    return asBuffer(this._p);
  }

  getGenerator() {
    return asBuffer(this._g);
  }

  getPublicKey() {
    if (this._pub === undefined) throw invalidState("No public key - did you forget to generate one?");
    return asBuffer(this._pub);
  }

  getPrivateKey() {
    if (this._priv === undefined) throw invalidState("No private key - did you forget to generate one?");
    return asBuffer(this._priv);
  }
}

class DiffieHellman extends DiffieHellmanBase {
  constructor(sizeOrKey, generator) {
    super();
    let g;
    if (typeof generator === "number") {
      if (generator < 2) throw badGenerator();
      g = numberBytes(generator);
    } else {
      g = new Uint8Array(bytesOf(generator));
      if (g.every((b) => b === 0) || (g.length > 0 && g.every((b, i) => (i === g.length - 1 ? b <= 1 : b === 0)))) {
        throw badGenerator();
      }
    }
    let p;
    if (typeof sizeOrKey === "number") {
      if (sizeOrKey < 2) {
        throw opensslError("01800076", "bignum routines", "bits too small", "ERR_OSSL_BN_BITS_TOO_SMALL");
      }
      const small = g.length === 1 ? g[0] : 0;
      p = new Uint8Array(__rc.dhGenPrime(sizeOrKey, small));
    } else {
      p = new Uint8Array(bytesOf(sizeOrKey));
    }
    this._init(p, g);
  }

  setPublicKey(key) {
    this._pub = new Uint8Array(bytesOf(key));
  }

  setPrivateKey(key) {
    this._priv = new Uint8Array(bytesOf(key));
  }
}

class DiffieHellmanGroup extends DiffieHellmanBase {
  constructor(name) {
    super();
    const params = __rc.dhGroupParams(name);
    if (params === null) throw cryptoError(Error, "ERR_CRYPTO_UNKNOWN_DH_GROUP", "Unknown DH group");
    this._init(new Uint8Array(params[0]), new Uint8Array(params[1]));
  }
}

function trimLeadingZeros(bytes) {
  let i = 0;
  while (i < bytes.byteLength - 1 && bytes[i] === 0) i++;
  return bytes.subarray(i);
}

class ECDH {
  constructor(curve) {
    if (!__rc.ecdhCurveKnown(curve)) {
      throw cryptoError(TypeError, "ERR_CRYPTO_INVALID_CURVE", "Invalid EC curve name");
    }
    this._curve = curve;
    this._priv = undefined;
    this._pub = undefined;
  }

  generateKeys() {
    const [priv, pub] = __rc.ecdhGenerate(this._curve);
    this._priv = trimLeadingZeros(new Uint8Array(priv));
    this._pub = new Uint8Array(pub);
  }

  computeSecret(key) {
    if (this._priv === undefined) throw cryptoError(Error, "ERR_CRYPTO_OPERATION_FAILED", "Failed to compute ECDH key");
    return asBuffer(__rc.ecdhCompute(this._curve, this._priv, bytesOf(key)));
  }

  getPublicKey(format) {
    if (this._pub === undefined) throw cryptoError(Error, "ERR_CRYPTO_OPERATION_FAILED", "Failed to get ECDH public key");
    return asBuffer(__rc.ecdhConvert(this._curve, this._pub, format));
  }

  getPrivateKey() {
    if (this._priv === undefined) throw cryptoError(Error, "ERR_CRYPTO_OPERATION_FAILED", "Failed to get ECDH private key");
    return asBuffer(this._priv);
  }

  setPrivateKey(key) {
    const priv = trimLeadingZeros(new Uint8Array(bytesOf(key)));
    this._pub = new Uint8Array(__rc.ecdhPublic(this._curve, priv));
    this._priv = priv;
  }

  setPublicKey(key) {
    this._pub = new Uint8Array(__rc.ecdhConvert(this._curve, bytesOf(key), 4));
  }
}

function ecdhConvertKey(key, curve, format) {
  return asBuffer(__rc.ecdhConvert(curve, bytesOf(key), format));
}

function statelessSecret(privateHandle, publicHandle) {
  try {
    return __rc.statelessDh(privateHandle._type, privateHandle._data, publicHandle._type, publicHandle._data);
  } catch (err) {
    throw cipherError(err);
  }
}

class ECDHBitsJob extends CryptoJob {
  constructor(mode, name, publicHandle, privateHandle) {
    super(mode);
    this.publicHandle = publicHandle;
    this.privateHandle = privateHandle;
  }

  _run() {
    return toArrayBuffer(statelessSecret(this.privateHandle, this.publicHandle));
  }
}

Object.assign(cryptoBinding, {
  DiffieHellman,
  DiffieHellmanGroup,
  ECDH,
  ECDHBitsJob,
  ECDHConvertKey: ecdhConvertKey,
  statelessDH: (privateHandle, publicHandle) => asBuffer(statelessSecret(privateHandle, publicHandle)),
});
