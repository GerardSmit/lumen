// XMLHttpRequest over the shared fetch transport. Download state and progress
// follow real body reads, and abort/timeout cancel the underlying transport.
class ProgressEvent extends Event {
  constructor(type, init = {}) {
    super(type, init);
    this.lengthComputable = !!init.lengthComputable;
    const count = value => {
      const n = Number(value || 0);
      return Number.isFinite(n) ? Math.max(0, Math.trunc(n)) : 0;
    };
    this.loaded = count(init.loaded);
    this.total = count(init.total);
  }
}

class XMLHttpRequestEventTarget extends EventTarget {}
for (const type of ['loadstart', 'progress', 'abort', 'error', 'load', 'timeout', 'loadend']) {
  globalThis.__eventTargetInternals.defineEventHandler(XMLHttpRequestEventTarget.prototype, type);
}
class XMLHttpRequestUpload extends XMLHttpRequestEventTarget {}

function xhrInvalid(message) { return new DOMException(message, 'InvalidStateError'); }
const xhrForbiddenHeaders = new Set([
  'accept-charset', 'accept-encoding', 'access-control-request-headers',
  'access-control-request-method', 'connection', 'content-length', 'cookie',
  'cookie2', 'date', 'dnt', 'expect', 'host', 'keep-alive', 'origin', 'referer',
  'set-cookie', 'te', 'trailer', 'transfer-encoding', 'upgrade', 'via',
]);

class XMLHttpRequest extends XMLHttpRequestEventTarget {
  constructor() {
    super();
    this._state = 0;
    this._sent = false;
    this._generation = 0;
    this._timeout = 0;
    this._timer = undefined;
    this._credentials = false;
    this._responseType = '';
    this._controller = null;
    this._headers = new Headers();
    this._overrideMime = null;
    this._upload = new XMLHttpRequestUpload();
    this._resetResponse();
  }
  _resetResponse() {
    this._status = 0;
    this._statusText = '';
    this._responseURL = '';
    this._responseHeaders = new Headers();
    this._bytes = new Uint8Array(0);
    this._responseObject = undefined;
  }
  get readyState() { return this._state; }
  get status() { return this._status; }
  get statusText() { return this._statusText; }
  get responseURL() { return this._responseURL; }
  get upload() { return this._upload; }
  get withCredentials() { return this._credentials; }
  set withCredentials(value) {
    if (this._state > 1 || this._sent) throw xhrInvalid('Cannot change credentials after send');
    this._credentials = !!value;
  }
  get timeout() { return this._timeout; }
  set timeout(value) {
    const number = Number(value);
    this._timeout = Number.isFinite(number) ? Math.trunc(number) >>> 0 : 0;
    if (this._sent) this._armTimeout();
  }
  get responseType() { return this._responseType; }
  set responseType(value) {
    if (this._state === 3 || this._state === 4) throw xhrInvalid('Response is already loading');
    value = String(value);
    if (['', 'text', 'json', 'arraybuffer', 'blob', 'document'].includes(value)) this._responseType = value;
  }
  get responseText() {
    if (this._responseType !== '' && this._responseType !== 'text') throw xhrInvalid('Response type is not text');
    if (this._state < 3) return '';
    return this._decodeText();
  }
  _decodeText() {
    // An override without a charset retains the response header's encoding.
    const charsetOf = mime => /charset\s*=\s*["']?([^\s;"']+)/i.exec(mime || '');
    const charset = charsetOf(this._overrideMime) || charsetOf(this._responseHeaders.get('content-type'));
    let decoder;
    try { decoder = new TextDecoder(charset ? charset[1] : 'utf-8'); }
    catch (_) { decoder = new TextDecoder(); }
    return decoder.decode(this._bytes);
  }
  get responseXML() {
    if (this._responseType !== '' && this._responseType !== 'document') throw xhrInvalid('Response type is not a document');
    return this._documentResponse();
  }
  _documentResponse() {
    if (this._state !== 4 || this._status === 0) return null;
    const mime = (this._overrideMime || this._responseHeaders.get('content-type') || 'application/xml').split(';')[0].trim().toLowerCase();
    if (this._responseType === '' && mime === 'text/html') return null;
    const xml = mime === 'text/xml' || mime === 'application/xml' || /^[^\s/]+\/[^\s/]+\+xml$/.test(mime);
    if (mime !== 'text/html' && !xml) return null;
    if (this._responseObject === undefined) {
      if (typeof globalThis.DOMParser !== 'function') throw new DOMException('No document parser installed', 'NotSupportedError');
      const document = new DOMParser().parseFromString(this._decodeText(), xml ? 'application/xml' : 'text/html');
      const root = document.documentElement;
      this._responseObject = xml && root && root.localName === 'parsererror' &&
        root.namespaceURI === 'http://www.mozilla.org/newlayout/xml/parsererror.xml' ? null : document;
    }
    return this._responseObject;
  }
  get response() {
    if (this._responseType === '' || this._responseType === 'text') return this.responseText;
    if (this._state !== 4 || this._status === 0) return null;
    if (this._responseType === 'document') return this._documentResponse();
    if (this._responseObject === undefined) {
      if (this._responseType === 'arraybuffer') this._responseObject = this._bytes.buffer.slice(this._bytes.byteOffset, this._bytes.byteOffset + this._bytes.byteLength);
      else if (this._responseType === 'blob') this._responseObject = new Blob([this._bytes], {type: this._overrideMime || this._responseHeaders.get('content-type') || ''});
      else {
        try { this._responseObject = JSON.parse(new TextDecoder().decode(this._bytes)); }
        catch (_) { this._responseObject = null; }
      }
    }
    return this._responseObject;
  }
  open(method, url, async = true, username = null, password = null) {
    method = String(method);
    if (!method || /[^!#$%&'*+.^_`|~0-9A-Za-z-]/.test(method)) throw new DOMException('Invalid method', 'SyntaxError');
    if (['CONNECT', 'TRACE', 'TRACK'].includes(method.toUpperCase())) throw new DOMException('Forbidden method', 'SecurityError');
    if (['DELETE', 'GET', 'HEAD', 'OPTIONS', 'POST', 'PUT'].includes(method.toUpperCase())) method = method.toUpperCase();
    if (!async) throw new DOMException('Synchronous XMLHttpRequest transport is not implemented', 'NotSupportedError');
    const base = globalThis.document && globalThis.document.baseURI || globalThis.location && globalThis.location.href;
    const target = base ? new URL(String(url), base) : new URL(String(url));
    if (username !== null) target.username = String(username);
    if (password !== null) target.password = String(password);
    ++this._generation;
    if (this._controller) this._controller.abort();
    clearTimeout(this._timer);
    this._controller = null;
    this._sent = false;
    this._method = method;
    this._url = target.href;
    this._headers = new Headers();
    this._overrideMime = null;
    this._resetResponse();
    if (this._state !== 1) {
      this._state = 1;
      this.dispatchEvent(new Event('readystatechange'));
    }
  }
  setRequestHeader(name, value) {
    if (this._state !== 1 || this._sent) throw xhrInvalid('Request is not open');
    name = String(name).toLowerCase();
    value = String(value).trim();
    // Validate before ignoring a forbidden header.
    const checked = new Headers();
    checked.set(name, value);
    if (/[\r\n\0]/.test(value)) throw new DOMException('Invalid header value', 'SyntaxError');
    if (xhrForbiddenHeaders.has(name) || name.startsWith('proxy-') || name.startsWith('sec-')) return;
    this._headers.append(name, value);
  }
  getResponseHeader(name) {
    if (this._state < 2) return null;
    name = String(name).toLowerCase();
    if (name === 'set-cookie' || name === 'set-cookie2') return null;
    return this._responseHeaders.get(name);
  }
  getAllResponseHeaders() {
    if (this._state < 2) return '';
    return [...this._responseHeaders].filter(([name]) => name !== 'set-cookie' && name !== 'set-cookie2').map(([name, value]) => `${name}: ${value}\r\n`).join('');
  }
  overrideMimeType(mime) {
    if (this._state === 3 || this._state === 4) throw xhrInvalid('Response is already loading');
    this._overrideMime = String(mime);
  }
  _progress(type, loaded = 0, total = 0, computable = false) {
    this.dispatchEvent(new ProgressEvent(type, {loaded, total, lengthComputable: computable}));
  }
  _armTimeout() {
    clearTimeout(this._timer);
    if (this._timeout) {
      const elapsed = performance.now() - this._started;
      this._timer = setTimeout(() => this._fail('timeout'), Math.max(0, this._timeout - elapsed));
    }
  }
  _fail(type) {
    if (!this._sent) return;
    ++this._generation;
    if (this._controller) this._controller.abort();
    clearTimeout(this._timer);
    this._sent = false;
    this._resetResponse();
    this._state = 4;
    this.dispatchEvent(new Event('readystatechange'));
    this._progress(type);
    this._progress('loadend');
  }
  abort() {
    if (this._sent) this._fail('abort');
    if (this._state === 4) {
      this._state = 0;
      this._resetResponse();
    }
  }
  send(body = null) {
    if (this._state !== 1 || this._sent) throw xhrInvalid('Request is not open or has already been sent');
    if (this._method === 'GET' || this._method === 'HEAD') body = null;
    const request = new Request(this._url, {method: this._method, headers: this._headers, body});
    this._controller = new AbortController();
    this._sent = true;
    this._started = performance.now();
    const generation = this._generation;
    const current = () => this._sent && this._generation === generation;
    this._progress('loadstart');
    if (!current()) return;
    this._armTimeout();
    fetch(request, {signal: this._controller.signal, credentials: this._credentials ? 'include' : 'same-origin'}).then(async response => {
      if (!current()) return;
      this._status = response.status;
      this._statusText = response.statusText;
      const target = new URL(response.url || this._url);
      target.hash = '';
      this._responseURL = target.href;
      this._responseHeaders = response.headers;
      this._state = 2;
      this.dispatchEvent(new Event('readystatechange'));
      if (!current()) return;
      const header = response.headers.get('content-length');
      const computable = header !== null && /^\d+$/.test(header) && Number.isSafeInteger(Number(header));
      const total = computable ? Number(header) : 0;
      const parts = [];
      let loaded = 0;
      const reader = response.body && response.body.getReader();
      if (reader) {
        try {
          for (;;) {
            const result = await reader.read();
            if (!current()) { await reader.cancel(); return; }
            if (result.done) break;
            const chunk = result.value;
            if (!(chunk instanceof Uint8Array)) throw new TypeError('XHR body chunk is not bytes');
            if (!chunk.byteLength) continue;
            parts.push(chunk);
            loaded += chunk.byteLength;
            // Partial responseText is observable inside every loading/progress callback.
            this._bytes = concatenateBodyChunks(parts, loaded);
            this._state = 3;
            this.dispatchEvent(new Event('readystatechange'));
            if (!current()) { await reader.cancel(); return; }
            this._progress('progress', loaded, total, computable);
            if (!current()) { await reader.cancel(); return; }
          }
        } finally { reader.releaseLock(); }
      }
      if (!loaded) this._progress('progress', 0, total, computable);
      if (!current()) return;
      clearTimeout(this._timer);
      this._sent = false;
      this._state = 4;
      this.dispatchEvent(new Event('readystatechange'));
      if (this._generation !== generation) return;
      this._progress('load', loaded, total, computable);
      if (this._generation !== generation) return;
      this._progress('loadend', loaded, total, computable);
    }).catch(() => { if (current()) this._fail('error'); });
  }
}
globalThis.__eventTargetInternals.defineEventHandler(XMLHttpRequest.prototype, 'readystatechange');
for (const [name, value] of [['UNSENT',0], ['OPENED',1], ['HEADERS_RECEIVED',2], ['LOADING',3], ['DONE',4]]) {
  Object.defineProperty(XMLHttpRequest, name, {value, enumerable: true});
  Object.defineProperty(XMLHttpRequest.prototype, name, {value, enumerable: true});
}
globalThis.ProgressEvent = ProgressEvent;
globalThis.XMLHttpRequestEventTarget = XMLHttpRequestEventTarget;
globalThis.XMLHttpRequestUpload = XMLHttpRequestUpload;
globalThis.XMLHttpRequest = XMLHttpRequest;
