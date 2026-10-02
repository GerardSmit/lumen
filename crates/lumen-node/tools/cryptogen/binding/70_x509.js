// ---- X.509 certificates, SPKAC and curve lists -------------------------------------------------------
// `parseX509` returns a handle over the certificate DER and the `x509*` ops (src/crypto/x509.rs).

function orUndefined(value) {
  return value === null ? undefined : value;
}

function nameObject(entries) {
  if (entries === null) return undefined;
  const out = { __proto__: null };
  for (let i = 0; i < entries.length; i += 2) {
    const key = entries[i];
    const value = entries[i + 1];
    if (key in out) out[key] = [].concat(out[key], value);
    else out[key] = value;
  }
  return out;
}

class X509Handle {
  constructor(der) {
    this._der = der;
  }

  subject() {
    return orUndefined(__rc.x509Subject(this._der));
  }

  issuer() {
    return orUndefined(__rc.x509Issuer(this._der));
  }

  subjectAltName() {
    const [present, text] = __rc.x509SubjectAltName(this._der);
    return present ? orUndefined(text) : undefined;
  }

  infoAccess() {
    const [present, text] = __rc.x509InfoAccess(this._der);
    return present ? orUndefined(text) : undefined;
  }

  getIssuerCert() {
    return undefined;
  }

  validFrom() {
    return __rc.x509Validity(this._der)[0];
  }

  validTo() {
    return __rc.x509Validity(this._der)[1];
  }

  fingerprint() {
    return __rc.x509Fingerprint(this._der, "sha1");
  }

  fingerprint256() {
    return __rc.x509Fingerprint(this._der, "sha256");
  }

  fingerprint512() {
    return __rc.x509Fingerprint(this._der, "sha512");
  }

  keyUsage() {
    return orUndefined(__rc.x509KeyUsage(this._der));
  }

  serialNumber() {
    return __rc.x509SerialNumber(this._der);
  }

  raw() {
    return Buffer.from(this._der);
  }

  publicKey() {
    return handleOf(cryptoBinding.kKeyTypePublic, __rc.x509PublicKey(this._der));
  }

  pem() {
    return __rc.x509Pem(this._der);
  }

  checkCA() {
    return __rc.x509CheckCA(this._der);
  }

  checkHost(name, flags) {
    return orUndefined(__rc.x509CheckHost(this._der, name, flags));
  }

  checkEmail(email, flags) {
    return __rc.x509CheckEmail(this._der, email, flags) ? email : undefined;
  }

  checkIP(ip, flags) {
    return __rc.x509CheckIP(this._der, ip, flags) ? ip : undefined;
  }

  checkIssued(other) {
    return __rc.x509CheckIssued(this._der, other._der);
  }

  checkPrivateKey(handle) {
    return __rc.x509CheckPrivateKey(this._der, handle._data);
  }

  verify(handle) {
    return __rc.x509Verify(this._der, keyDer(handle, true));
  }

  toLegacy() {
    const der = this._der;
    const out = {
      subject: nameObject(__rc.x509NameEntries(der, false)),
      issuer: nameObject(__rc.x509NameEntries(der, true)),
    };
    const san = this.subjectAltName();
    if (san !== undefined) out.subjectaltname = san;
    const access = this.infoAccess();
    if (access !== undefined) out.infoAccess = access;
    out.ca = this.checkCA();
    const [type, names, bits, pubkey] = __rc.x509KeyDetails(der);
    if (type === "rsa") {
      out.modulus = names[0];
      out.bits = bits;
      out.exponent = names[1];
      out.pubkey = Buffer.from(pubkey);
    } else if (type === "ec") {
      out.bits = bits;
      out.pubkey = Buffer.from(pubkey);
      out.asn1Curve = names[0];
      if (names[1] !== "") out.nistCurve = names[1];
    }
    out.valid_from = this.validFrom();
    out.valid_to = this.validTo();
    out.fingerprint = this.fingerprint();
    out.fingerprint256 = this.fingerprint256();
    out.fingerprint512 = this.fingerprint512();
    const usage = this.keyUsage();
    if (usage !== undefined) out.ext_key_usage = usage;
    out.serialNumber = this.serialNumber();
    out.raw = this.raw();
    return out;
  }
}

function parseX509(buffer) {
  try {
    return new X509Handle(new Uint8Array(__rc.x509Parse(bytesOf(buffer))));
  } catch (err) {
    throw cipherError(err);
  }
}

function spkacResult(fn, buffer) {
  const out = fn(bytesOf(buffer));
  return out === null ? "" : asBuffer(out);
}

Object.assign(cryptoBinding, {
  parseX509,
  certVerifySpkac: (buffer) => __rc.certVerifySpkac(bytesOf(buffer)),
  certExportPublicKey: (buffer) => spkacResult(__rc.certExportPublicKey, buffer),
  certExportChallenge: (buffer) => spkacResult(__rc.certExportChallenge, buffer),
  getCurves: () => __rc.keyCurves(),
});
