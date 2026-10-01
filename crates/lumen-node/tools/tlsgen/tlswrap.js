// ---- internalBinding('crypto').SecureContext ----------------------------------------------------
// The handle behind tls.createSecureContext(): an OpenSSL SSL_CTX in the native TLS registry.

function asBytes(value) {
  if (value === undefined || value === null) return undefined;
  if (typeof value === "string") return Buffer.from(value);
  if (value instanceof Uint8Array) return value;
  if (ArrayBuffer.isView(value)) return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
  if (value instanceof ArrayBuffer) return new Uint8Array(value);
  return undefined;
}

class NativeSecureContext {
  constructor() {
    this._id = null;
  }
  init(method, min, max) {
    this._id = tlsOps.ctxNew(method || undefined, min, max);
  }
  _op(name, a, b) {
    if (this._id === null) throw new Error("SecureContext is closed");
    return tlsOps.ctxOp(this._id, name, a, b);
  }
  setKey(key, passphrase) { this._op("setKey", asBytes(key), asBytes(passphrase)); }
  setCert(cert) { this._op("setCert", asBytes(cert)); }
  addCACert(cert) { this._op("addCACert", asBytes(cert)); }
  addCRL(crl) { this._op("addCRL", asBytes(crl)); }
  addRootCerts() { this._op("addRootCerts"); }
  setCiphers(list) { this._op("setCiphers", `${list}`); }
  setCipherSuites(list) { this._op("setCipherSuites", `${list}`); }
  setSigalgs(list) { this._op("setSigalgs", `${list}`); }
  setECDHCurve(curve) { this._op("setECDHCurve", `${curve}`); }
  setDHParam(param) { return this._op("setDHParam", param === true ? undefined : asBytes(param)); }
  setMinProto(version) { this._op("setMinProto", version); }
  setMaxProto(version) { this._op("setMaxProto", version); }
  getMinProto() { return this._op("getMinProto"); }
  getMaxProto() { return this._op("getMaxProto"); }
  setOptions(options) { this._op("setOptions", options); }
  setSessionIdContext(context) { this._op("setSessionIdContext", asBytes(context)); }
  setSessionTimeout(seconds) { this._op("setSessionTimeout", seconds); }
  setTicketKeys(keys) { this._op("setTicketKeys", asBytes(keys)); }
  getTicketKeys() { return Buffer.from(this._op("getTicketKeys")); }
  loadPKCS12(data, passphrase) { this._op("loadPKCS12", asBytes(data), asBytes(passphrase)); }
  getCertificate() { return Buffer.from(this._op("getCertificate")); }
  getIssuer() { return Buffer.from(this._op("getIssuer")); }
  close() {
    if (this._id === null) return;
    const id = this._id;
    this._id = null;
    tlsOps.ctxOp(id, "close");
  }
}

function getSSLCiphers() {
  try {
    return tlsOps.ciphers();
  } catch {
    return [];
  }
}

function getRootCertificates() {
  try {
    return tlsOps.rootCertificates();
  } catch {
    return [];
  }
}

// ---- internalBinding('js_stream') ---------------------------------------------------------------
// A stream handle whose I/O is done by JavaScript callbacks (internal/js_stream_socket sets them).

class JSStream {
  constructor() {
    this._asyncId = newAsyncId();
    this.reading = false;
    this.bytesRead = 0;
    this.bytesWritten = 0;
    this.onread = null;
    this.isStreamBase = false;
  }
  getAsyncId() { return this._asyncId; }
  getProviderType() { return 0; }
  get writeQueueSize() { return 0; }
  readStart() { return this.onreadstart(); }
  readStop() { return this.onreadstop(); }
  shutdown(req) { return this.onshutdown(req); }
  useUserBuffer() {}
  _write(req, chunks) {
    let length = 0;
    for (const chunk of chunks) length += chunk.length;
    streamBaseState[kBytesWritten] = length;
    streamBaseState[kLastWriteWasAsync] = 1;
    this.bytesWritten += length;
    return this.onwrite(req, chunks);
  }
  writeBuffer(req, data) {
    return this._write(req, [data instanceof Uint8Array ? data : new Uint8Array(data.buffer, data.byteOffset, data.byteLength)]);
  }
  writev(req, chunks, allBuffers) {
    const parts = [];
    if (allBuffers) {
      for (let i = 0; i < chunks.length; i++) parts.push(chunks[i]);
    } else {
      for (let i = 0; i < chunks.length; i += 2) {
        const chunk = chunks[i];
        parts.push(typeof chunk === "string" ? Buffer.from(chunk, chunks[i + 1]) : chunk);
      }
    }
    return this._write(req, parts);
  }
  writeUtf8String(req, text) { return this._write(req, [Buffer.from(text, "utf8")]); }
  writeLatin1String(req, text) { return this._write(req, [Buffer.from(text, "latin1")]); }
  writeAsciiString(req, text) { return this._write(req, [Buffer.from(text, "latin1")]); }
  writeUcs2String(req, text) { return this._write(req, [Buffer.from(text, "utf16le")]); }
  readBuffer(chunk) {
    this.bytesRead += chunk.length;
    streamBaseState[kReadBytesOrError] = chunk.length;
    streamBaseState[kArrayBufferOffset] = chunk.byteOffset;
    this.onread(chunk.buffer);
  }
  emitEOF() {
    streamBaseState[kReadBytesOrError] = UV_EOF;
    streamBaseState[kArrayBufferOffset] = 0;
    this.onread(undefined);
  }
  finishWrite(req, status) {
    if (typeof req.oncomplete === "function") req.oncomplete(status, this, undefined);
  }
  finishShutdown(req, status) {
    if (typeof req.oncomplete === "function") req.oncomplete(status);
  }
}

// ---- internalBinding('tls_wrap') ----------------------------------------------------------------
// TLSWrap (src/crypto/crypto_tls.cc): a stream handle that encrypts what is written to it and
// decrypts what the parent handle reads, over a native OpenSSL session driven through memory
// buffers. `Cycle()` moves bytes: cleartext in (ClearIn), cleartext out (ClearOut), encrypted
// out to the parent (EncOut); encrypted bytes in arrive from the parent's read callback.

const SSL_ERROR_SSL = 1;
const SSL_ERROR_WANT_READ = 2;
const SSL_ERROR_WANT_WRITE = 3;
const SSL_ERROR_WANT_X509_LOOKUP = 4;
const SSL_ERROR_SYSCALL = 5;
const SSL_ERROR_ZERO_RETURN = 6;
const SSL_ERROR_WANT_CLIENT_HELLO_CB = 11;
const kClearOutChunkSize = 16384;

const x509ErrorCodes = {
  2: "UNABLE_TO_GET_ISSUER_CERT", 3: "UNABLE_TO_GET_CRL", 4: "UNABLE_TO_DECRYPT_CERT_SIGNATURE",
  5: "UNABLE_TO_DECRYPT_CRL_SIGNATURE", 6: "UNABLE_TO_DECODE_ISSUER_PUBLIC_KEY",
  7: "CERT_SIGNATURE_FAILURE", 8: "CRL_SIGNATURE_FAILURE", 9: "CERT_NOT_YET_VALID",
  10: "CERT_HAS_EXPIRED", 11: "CRL_NOT_YET_VALID", 12: "CRL_HAS_EXPIRED",
  13: "ERROR_IN_CERT_NOT_BEFORE_FIELD", 14: "ERROR_IN_CERT_NOT_AFTER_FIELD",
  15: "ERROR_IN_CRL_LAST_UPDATE_FIELD", 16: "ERROR_IN_CRL_NEXT_UPDATE_FIELD", 17: "OUT_OF_MEM",
  18: "DEPTH_ZERO_SELF_SIGNED_CERT", 19: "SELF_SIGNED_CERT_IN_CHAIN",
  20: "UNABLE_TO_GET_ISSUER_CERT_LOCALLY", 21: "UNABLE_TO_VERIFY_LEAF_SIGNATURE",
  22: "CERT_CHAIN_TOO_LONG", 23: "CERT_REVOKED", 24: "INVALID_CA", 25: "PATH_LENGTH_EXCEEDED",
  26: "INVALID_PURPOSE", 27: "CERT_UNTRUSTED", 28: "CERT_REJECTED", 62: "HOSTNAME_MISMATCH",
};

const sigalgSigners = { 6: "RSA", 912: "RSA-PSS", 408: "ECDSA", 1087: "ED25519", 1088: "ED448" };
const sigalgHashes = { 64: "SHA1", 675: "SHA224", 672: "SHA256", 673: "SHA384", 674: "SHA512" };
const ephemeralCurves = { 256: "prime256v1", 384: "secp384r1", 521: "secp521r1" };

function alpnName(bytes) {
  if (bytes === false) return false;
  const text = Buffer.from(bytes).toString("latin1");
  return text;
}

class TLSWrap {
  constructor(parent, context, isServer, hasActiveWrite) {
    this._parent = parent;
    this._context = context;
    this._isServer = isServer;
    this._ssl = tlsOps.sessNew(context._id, isServer);
    this._asyncId = newAsyncId();
    this._started = false;
    this._eof = false;
    this._established = false;
    this._shutdown = false;
    this._depth = 0;
    this._pending = null;
    this._current = null;
    this._callbackScheduled = false;
    this._inWrite = false;
    this._writeSize = 0;
    this._pendingShutdown = null;
    this._previousWrite = !!hasActiveWrite;
    this._helloPending = false;
    this._certPending = false;
    this._newSessionPending = false;
    this._sessionCallbacks = false;
    this._certCallback = false;
    this._alpnCallback = false;
    this._userBuf = null;
    this.reading = false;
    this.bytesRead = 0;
    this.bytesWritten = 0;
    this.onread = null;
    this.isStreamBase = false;
    this.sni_context = undefined;
    parent.onread = (buffer) => this._onParentRead(buffer);
  }

  getAsyncId() { return this._asyncId; }
  getProviderType() { return 0; }
  hasRef() { return typeof this._parent.hasRef === "function" ? this._parent.hasRef() : true; }
  get writeQueueSize() {
    return (this._pending === null ? 0 : this._pending.bytes.length) + (this._parent.writeQueueSize || 0);
  }
  useUserBuffer(buf) { this._userBuf = buf; }

  readStart() {
    if (this._parent !== null && !this._eof) return this._parent.readStart();
    return 0;
  }
  readStop() {
    return this._parent !== null ? this._parent.readStop() : 0;
  }

  _op(name, a, b) {
    return tlsOps.sessOp(this._ssl, name, a, b);
  }

  _onParentRead(buffer) {
    const nread = streamBaseState[kReadBytesOrError];
    if (this._eof) return;
    if (nread < 0) {
      this._clearOut();
      if (nread === UV_EOF) this._eof = true;
      streamBaseState[kReadBytesOrError] = nread;
      streamBaseState[kArrayBufferOffset] = 0;
      if (typeof this.onread === "function") this.onread(undefined);
      return;
    }
    if (this._ssl === null) return;
    const offset = streamBaseState[kArrayBufferOffset];
    this._receive(new Uint8Array(buffer, offset, nread));
  }

  receive(data) {
    const bytes = data instanceof Uint8Array ? data : new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    this._receive(bytes);
  }

  _receive(bytes) {
    if (this._eof || this._ssl === null) return;
    tlsOps.feed(this._ssl, bytes);
    this._cycle();
  }

  _cycle() {
    if (++this._depth > 1) return;
    for (; this._depth > 0; this._depth--) {
      this._clearIn();
      this._clearOut();
      this._encOut();
    }
  }

  _emitEvents() {
    const events = this._ssl === null ? undefined : tlsOps.events(this._ssl);
    if (events === undefined) return;
    for (const event of events) {
      switch (event[0]) {
        case "hs-start":
          if (typeof this.onhandshakestart === "function") this.onhandshakestart(Date.now());
          break;
        case "hs-done":
          this._established = true;
          if (typeof this.onhandshakedone === "function") this.onhandshakedone();
          break;
        case "session":
          if (this._isServer) this._newSessionPending = true;
          if (typeof this.onnewsession === "function") this.onnewsession(Buffer.from(event[1]), Buffer.from(event[2]));
          break;
        case "keylog": {
          const line = Buffer.from(event[1]);
          if (typeof this.onkeylog === "function") {
            this.onkeylog(line[line.length - 1] === 10 ? line : Buffer.concat([line, Buffer.from("\n")]));
          }
          break;
        }
        case "ocsp":
          if (typeof this.onocspresponse === "function") {
            this.onocspresponse(event[1] === undefined ? undefined : Buffer.from(event[1]));
          }
          break;
      }
      if (this._ssl === null) return;
    }
  }

  _serviceRetry(code) {
    if (code === SSL_ERROR_WANT_CLIENT_HELLO_CB) {
      const hello = this._op("helloRequest");
      if (hello !== undefined) this._runHello(hello);
    } else if (code === SSL_ERROR_WANT_X509_LOOKUP) {
      const request = this._op("certRequest");
      if (request !== undefined) this._runCertCallback(request);
    }
  }

  _runHello(hello) {
    const [sessionId, servername, hasTicket, , alpn] = hello;
    if (this._alpnCallback && alpn.length > 0 && typeof this.ALPNCallback === "function") {
      let offset;
      try {
        offset = this.ALPNCallback(Buffer.from(alpn));
      } catch (error) {
        this._op("setAlpnChoice", -1);
        this._op("helloDone");
        process.nextTick(() => { throw error; });
        return;
      }
      this._op("setAlpnChoice", typeof offset === "number" ? offset : -1);
    }
    if (this._sessionCallbacks && typeof this.onclienthello === "function") {
      this._helloPending = true;
      this.onclienthello({ sessionId: Buffer.from(sessionId), servername, tlsTicket: hasTicket });
    } else {
      this._op("helloDone");
    }
  }

  _runCertCallback(request) {
    this._certPending = true;
    if (typeof this.oncertcb === "function") {
      this.oncertcb({ servername: request[0], OCSPRequest: request[1] ? Buffer.alloc(0) : undefined });
    } else {
      this.certCbDone();
    }
  }

  endParser() {
    if (this._ssl === null) return;
    this._op("helloDone");
    this._helloPending = false;
    this._cycle();
  }

  certCbDone() {
    if (this._ssl === null) return;
    const context = this.sni_context;
    if (context !== undefined && context !== null) {
      if (context instanceof NativeSecureContext) {
        this._op("setSniContext", context._id);
      } else if (typeof context === "object") {
        const error = new TypeError("Invalid SNI context");
        if (typeof this.onerror === "function") this.onerror(error);
        return;
      }
    }
    this._certPending = false;
    this._op("certDone");
    this._cycle();
  }

  _clearIn() {
    if (this._helloPending || this._certPending || this._ssl === null || this._pending === null) return;
    const { bytes } = this._pending;
    this._pending = null;
    const written = tlsOps.write(this._ssl, bytes);
    this._emitEvents();
    if (this._ssl === null) return;
    if (written === bytes.length) return;
    const code = -written;
    if (code === SSL_ERROR_SSL || code === SSL_ERROR_SYSCALL) {
      this._callbackScheduled = true;
      this._invokeQueued(UV_EPROTO, this._failure().message);
      return;
    }
    this._pending = { bytes };
    this._serviceRetry(code);
  }

  _failure() {
    return tlsOps.lastError(this._ssl);
  }

  _clearOut() {
    if (this._helloPending || this._certPending || this._eof || this._ssl === null) return;
    let result;
    for (;;) {
      result = tlsOps.read(this._ssl, kClearOutChunkSize);
      this._emitEvents();
      if (this._ssl === null) return;
      if (typeof result === "number") break;
      this._emitData(result);
      if (this._ssl === null) return;
    }
    switch (result) {
      case SSL_ERROR_ZERO_RETURN:
        if (!this._eof) {
          this._eof = true;
          streamBaseState[kReadBytesOrError] = UV_EOF;
          streamBaseState[kArrayBufferOffset] = 0;
          if (typeof this.onread === "function") this.onread(undefined);
        }
        return;
      case SSL_ERROR_SSL:
      case SSL_ERROR_SYSCALL: {
        const error = this._failure();
        this._encOut();
        if (typeof this.onerror === "function") this.onerror(error);
        return;
      }
      case SSL_ERROR_WANT_CLIENT_HELLO_CB:
      case SSL_ERROR_WANT_X509_LOOKUP:
        this._serviceRetry(result);
        return;
    }
  }

  _emitData(chunk) {
    if (typeof this.onread !== "function") return;
    if (this._userBuf !== null) {
      while (chunk.length > 0 && this._ssl !== null) {
        const n = Math.min(chunk.length, this._userBuf.length);
        this._userBuf.set(chunk.subarray(0, n));
        streamBaseState[kReadBytesOrError] = n;
        streamBaseState[kArrayBufferOffset] = 0;
        const next = this.onread(undefined);
        if (next instanceof Uint8Array) this._userBuf = next;
        chunk = chunk.subarray(n);
      }
      return;
    }
    streamBaseState[kReadBytesOrError] = chunk.length;
    streamBaseState[kArrayBufferOffset] = chunk.byteOffset;
    this.onread(chunk.buffer);
  }

  _encOut() {
    if (this._helloPending || this._certPending) return;
    if (this._writeSize !== 0) return;
    if (this._newSessionPending) return;
    if (this._previousWrite) return;
    if (this._established && this._current !== null) this._callbackScheduled = true;
    if (this._ssl === null) return;
    const output = tlsOps.output(this._ssl);
    if (output === undefined) {
      if (this._pending === null) {
        if (!this._inWrite) this._invokeQueued(0);
        else setImmediate(() => this._invokeQueued(0));
      }
      this._flushShutdown();
      return;
    }
    this._writeSize = output.length;
    const req = new WriteWrap();
    req.oncomplete = (status) => this._onParentWrite(status);
    const parent = this._parent;
    const error = parent.writeBuffer(req, output);
    if (error !== 0) {
      this._invokeQueued(error);
      return;
    }
    if (streamBaseState[kLastWriteWasAsync] === 0) setImmediate(() => this._onParentWrite(0));
  }

  _onParentWrite(status) {
    if (this._ssl === null) status = UV_ECANCELED;
    if (status) {
      if (this._shutdown) return;
      this._invokeQueued(status);
      return;
    }
    this._clearIn();
    this._writeSize = 0;
    this._encOut();
  }

  _flushShutdown() {
    if (this._pendingShutdown === null || this._writeSize !== 0) return;
    const req = this._pendingShutdown;
    this._pendingShutdown = null;
    const status = this._parent.shutdown(req);
    if (status !== 0 && typeof req.oncomplete === "function") process.nextTick(() => req.oncomplete(status));
  }

  _invokeQueued(status, error) {
    if (!this._callbackScheduled) return;
    const req = this._current;
    if (req === null) return;
    this._current = null;
    req.error = error;
    if (typeof req.oncomplete === "function") req.oncomplete(status, this, error);
  }

  _write(req, bytes) {
    if (this._ssl === null) {
      req.error = "Write after DestroySSL";
      return UV_EPROTO;
    }
    streamBaseState[kBytesWritten] = bytes.length;
    streamBaseState[kLastWriteWasAsync] = 1;
    this.bytesWritten += bytes.length;
    if (bytes.length === 0) {
      this._clearOut();
      setImmediate(() => {
        if (typeof req.oncomplete === "function") req.oncomplete(0, this, undefined);
      });
      return 0;
    }
    this._current = req;
    const written = tlsOps.write(this._ssl, bytes);
    this._emitEvents();
    if (this._ssl === null) return 0;
    if (written !== bytes.length) {
      const code = -written;
      if (code === SSL_ERROR_SSL || code === SSL_ERROR_SYSCALL) {
        this._current = null;
        req.error = this._failure().message;
        return UV_EPROTO;
      }
      this._pending = { bytes };
      this._serviceRetry(code);
    }
    this._inWrite = true;
    try {
      this._encOut();
    } finally {
      this._inWrite = false;
    }
    // The parent write above may have reset the flag; this write always completes later.
    streamBaseState[kLastWriteWasAsync] = 1;
    return 0;
  }

  writeBuffer(req, data) {
    return this._write(req, data instanceof Uint8Array ? data : new Uint8Array(data.buffer, data.byteOffset, data.byteLength));
  }
  writev(req, chunks, allBuffers) {
    const parts = [];
    if (allBuffers) {
      for (let i = 0; i < chunks.length; i++) parts.push(chunks[i]);
    } else {
      for (let i = 0; i < chunks.length; i += 2) {
        const chunk = chunks[i];
        parts.push(typeof chunk === "string" ? Buffer.from(chunk, chunks[i + 1]) : chunk);
      }
    }
    return this._write(req, parts.length === 1 ? parts[0] : Buffer.concat(parts));
  }
  writeUtf8String(req, text) { return this._write(req, Buffer.from(text, "utf8")); }
  writeLatin1String(req, text) { return this._write(req, Buffer.from(text, "latin1")); }
  writeAsciiString(req, text) { return this._write(req, Buffer.from(text, "latin1")); }
  writeUcs2String(req, text) { return this._write(req, Buffer.from(text, "utf16le")); }

  shutdown(req) {
    if (this._ssl !== null) this._op("shutdown");
    this._shutdown = true;
    this._encOut();
    if (this._writeSize !== 0) {
      this._pendingShutdown = req;
      return 0;
    }
    return this._parent.shutdown(req);
  }

  start() {
    this._started = true;
    this._clearOut();
    this._encOut();
  }

  destroySSL() {
    if (this._ssl === null) return;
    this._callbackScheduled = true;
    this._invokeQueued(UV_ECANCELED, "Canceled because of SSL destruction");
    const id = this._ssl;
    this._ssl = null;
    this._pending = null;
    tlsOps.sessFree(id);
  }

  writesIssuedByPrevListenerDone() {
    this._previousWrite = false;
    this._encOut();
  }

  newSessionDone() {
    this._newSessionPending = false;
    this._cycle();
  }

  enableTrace() {}
  enableSessionCallbacks() {
    this._sessionCallbacks = true;
    this._op("enableSessionCallbacks");
    if (this._isServer) this._op("enableHelloCb");
  }
  enableKeylogCallback() { this._op("enableKeylog"); }
  enableCertCb() {
    this._certCallback = true;
    this._op("enableCertCb");
  }
  enableALPNCb() {
    this._alpnCallback = true;
    this._op("enableAlpnCb");
  }
  setVerifyMode(requestCert, rejectUnauthorized) { this._op("setVerifyMode", requestCert, rejectUnauthorized); }
  setALPNProtocols(protocols) {
    this._op("setAlpn", asBytes(protocols));
  }
  loadSession(session) { this._op("loadSession", asBytes(session)); }
  setOCSPResponse(response) { this._op("setOCSPResponse", asBytes(response)); }
  requestOCSP() { this._op("requestOCSP"); }
  setSession(session) {
    if (!this._op("setSession", asBytes(session))) throw new Error("SSL_set_session error");
  }
  setServername(name) { this._op("setServername", name); }
  setMaxSendFragment(size) { return this._op("setMaxSendFragment", size) ? 1 : 0; }
  renegotiate() { this._op("renegotiate"); }
  exportKeyingMaterial(length, label, context) {
    return Buffer.from(tlsOps.sessOp(this._ssl, "exportKeyingMaterial", length, label, asBytes(context)));
  }

  getServername() { return this._op("servername"); }
  getProtocol() { return this._op("protocol"); }
  getALPNNegotiatedProtocol() { return alpnName(this._op("alpnSelected")); }
  isSessionReused() { return this._op("isSessionReused"); }
  getSession() {
    const session = this._op("getSession");
    return session === undefined ? undefined : Buffer.from(session);
  }
  getTLSTicket() { return undefined; }
  getFinished() {
    const data = this._op("finished", false);
    return data === undefined ? undefined : Buffer.from(data);
  }
  getPeerFinished() {
    const data = this._op("finished", true);
    return data === undefined ? undefined : Buffer.from(data);
  }
  getCipher() {
    const cipher = this._op("cipher");
    if (cipher === null) return undefined;
    return { name: cipher[0], standardName: cipher[1], version: cipher[2] };
  }
  getSharedSigalgs() {
    return this._op("sharedSigalgs").map((entry) => {
      const [sign, hash] = entry.split("+").map(Number);
      const signer = sigalgSigners[sign] || `${sign}`;
      const digest = sigalgHashes[hash];
      return digest === undefined ? signer : `${signer}+${digest}`;
    });
  }
  getEphemeralKeyInfo() {
    if (this._isServer) return null;
    const key = this._op("ephemeralKey");
    if (key === undefined) return {};
    const [type, bits] = key;
    if (type === 28) return { type: "DH", size: bits };
    if (type === 408) return { type: "ECDH", name: ephemeralCurves[bits] || `${bits}`, size: bits };
    if (type === 1034) return { type: "ECDH", name: "X25519", size: 253 };
    if (type === 1035) return { type: "ECDH", name: "X448", size: 448 };
    return {};
  }
  verifyError() {
    const failure = this._op("verifyError");
    if (failure === undefined) return null;
    const error = new Error(failure[1]);
    error.code = x509ErrorCodes[failure[0]] || "UNKNOWN_CERTIFICATE_VERIFICATION_ERROR";
    return error;
  }

  _peerChain() {
    return this._op("peerCertificates");
  }
  getPeerCertificate(detailed) {
    return x509Chain(this._peerChain(), detailed === true);
  }
  getCertificate() {
    const der = this._op("ownCertificate");
    return der === undefined ? undefined : x509Object(der);
  }
  getPeerX509Certificate() {
    const chain = this._peerChain();
    return chain.length === 0 ? undefined : Buffer.from(chain[0]);
  }
  getX509Certificate() {
    const der = this._op("ownCertificate");
    return der === undefined ? undefined : Buffer.from(der);
  }
}

function wrapTls(parent, context, isServer, hasActiveWrite) {
  return new TLSWrap(parent, context, isServer, hasActiveWrite);
}
