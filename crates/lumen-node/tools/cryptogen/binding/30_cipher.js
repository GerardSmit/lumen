// ---- symmetric ciphers ---------------------------------------------------------------------------
// CipherBase over the native CryptoCipher engine (src/crypto/cipher.rs), getCiphers/getCipherInfo
// and WebCrypto's AESCipherJob.

// Native errors carry OpenSSL's `error:<hex>:<library>::<reason>` message; Node also exposes
// `library` and `reason` on them.
function cipherError(err) {
  const m = err && typeof err.message === "string" && /^error:[0-9A-F]{8}:([^:]*)::(.*)$/.exec(err.message);
  if (m) {
    err.library = m[1];
    err.reason = m[2];
  }
  return err;
}

function cipherInput(data, encoding) {
  if (typeof data === "string") return Buffer.from(data, Buffer.isEncoding(encoding) ? encoding : "utf8");
  return bytesOf(data);
}

function asBuffer(u8) {
  return Buffer.from(u8.buffer, u8.byteOffset, u8.byteLength);
}

function concatBytes(parts) {
  let n = 0;
  for (const p of parts) n += p.byteLength;
  const out = new Uint8Array(n);
  let off = 0;
  for (const p of parts) {
    out.set(p, off);
    off += p.byteLength;
  }
  return out;
}

class CipherBase {
  constructor(isEncrypt) {
    this._enc = isEncrypt === true;
    this._c = undefined;
  }

  init(cipher, password, authTagLength) {
    const mode = __rc.cipherMode(cipher);
    if (mode === null) throw cryptoError(Error, "ERR_CRYPTO_UNKNOWN_CIPHER", "Unknown cipher");
    if (this._enc && (mode === "ctr" || mode === "gcm" || mode === "ccm")) {
      process.emitWarning(`Use Cipheriv for counter mode of ${cipher}`);
    }
    this._c = __rc.cipherInit(cipher, this._enc, bytesOf(password), authTagLength);
  }

  initiv(cipher, key, iv, authTagLength) {
    this._c = __rc.cipherNew(cipher, this._enc, secretBytes(key), iv == null ? null : bytesOf(iv), authTagLength);
  }

  update(data, encoding) {
    try {
      return asBuffer(this._c.update(cipherInput(data, encoding)));
    } catch (err) {
      throw cipherError(err);
    }
  }

  final() {
    try {
      return asBuffer(this._c.final());
    } catch (err) {
      throw cipherError(err);
    }
  }

  setAutoPadding(autoPadding) {
    return this._c.setPadding(autoPadding === undefined || autoPadding === true);
  }

  getAuthTag() {
    const tag = this._c.authTag();
    return tag === null ? undefined : asBuffer(tag);
  }

  setAuthTag(tag) {
    return this._c.setAuthTag(bytesOf(tag));
  }

  setAAD(aad, plaintextLength) {
    return this._c.setAad(bytesOf(aad), plaintextLength);
  }
}

function getCipherInfo(info, nameOrNid, keyLength, ivLength) {
  const r = typeof nameOrNid === "string" ?
    __rc.cipherInfo(nameOrNid, 0, keyLength, ivLength) :
    __rc.cipherInfo(undefined, nameOrNid, keyLength, ivLength);
  if (r === null) return undefined;
  const [name, mode, [nid, blockSize, ivLen, keyLen]] = r;
  info.mode = mode;
  info.name = name;
  info.nid = nid;
  if (blockSize) info.blockSize = blockSize;
  if (ivLen) info.ivLength = ivLen;
  info.keyLength = keyLen;
  return info;
}

const kAesKwDefaultIv = new Uint8Array(8).fill(0xa6);

// WebCrypto AES-CTR/CBC/GCM/KW (crypto_aes.cc): `variant` is one of kKeyVariantAES_*.
class AESCipherJob extends CryptoJob {
  constructor(mode, cipherMode, key, data, variant, iv, extra, additionalData) {
    super(mode);
    this.encrypt = cipherMode === cryptoBinding.kWebCryptoCipherEncrypt;
    this.key = key._data;
    this.data = new Uint8Array(bytesOf(data));
    this.bits = [128, 192, 256][variant % 3];
    this.kind = ["ctr", "cbc", "gcm", "kw"][Math.floor(variant / 3)];
    if (this.kind === "kw") return;
    this.iv = new Uint8Array(bytesOf(iv));
    if (this.kind === "ctr") {
      if (this.iv.byteLength !== 16 || extra === 0 || extra > 128) {
        throw cryptoError(TypeError, "ERR_CRYPTO_INVALID_COUNTER", "Invalid counter");
      }
      this.length = extra;
    } else if (this.kind === "gcm") {
      if (this.encrypt) {
        if (typeof extra !== "number" || extra >>> 0 !== extra || extra > 128) {
          throw cryptoError(TypeError, "ERR_CRYPTO_INVALID_TAG_LENGTH", "Invalid tag length");
        }
        this.tagLength = extra;
      } else {
        if (!(extra instanceof ArrayBuffer || ArrayBuffer.isView(extra))) {
          throw cryptoError(TypeError, "ERR_CRYPTO_INVALID_TAG_LENGTH", "Invalid tag length");
        }
        this.tag = new Uint8Array(bytesOf(extra));
      }
      if (additionalData instanceof ArrayBuffer || ArrayBuffer.isView(additionalData)) {
        this.aad = new Uint8Array(bytesOf(additionalData));
      }
    }
  }

  _run() {
    const { encrypt, key, data, bits } = this;
    switch (this.kind) {
      case "ctr":
        return toArrayBuffer(__rc.aesCtr(key, this.iv, this.length, data));
      case "cbc": {
        const c = __rc.cipherNew(`aes-${bits}-cbc`, encrypt, key, this.iv, -1);
        return toArrayBuffer(concatBytes([c.update(data), c.final()]));
      }
      case "kw": {
        const c = __rc.cipherNew(`id-aes${bits}-wrap`, encrypt, key, kAesKwDefaultIv, -1);
        return toArrayBuffer(concatBytes([c.update(data), c.final()]));
      }
      case "gcm": {
        const c = __rc.cipherNew(`aes-${bits}-gcm`, encrypt, key, this.iv,
                                 encrypt ? this.tagLength : this.tag.byteLength);
        if (!encrypt) c.setAuthTag(this.tag);
        if (this.aad !== undefined && this.aad.byteLength > 0) c.setAad(this.aad, -1);
        const parts = [c.update(data), c.final()];
        if (encrypt) parts.push(c.authTag());
        return toArrayBuffer(concatBytes(parts));
      }
    }
    throw new Error("Cipher job failed");
  }
}

Object.assign(cryptoBinding, {
  CipherBase,
  AESCipherJob,
  getCipherInfo,
  getCiphers: () => __rc.getCiphers(),
});
