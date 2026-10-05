// The helper side of the suspending host call (see bridge.js). Awaits the browser's Promises and
// hands the answers back through the SharedArrayBuffer. Runs as a module Worker in the browser
// and as a worker_threads Worker under Node.

const STATE = 0;
const LEN = 1;
const MORE = 2;
const STATUS = 3;
const HEADER_BYTES = 16;

const encoder = new TextEncoder();
const decoder = new TextDecoder();

let port;
if (typeof WorkerGlobalScope !== 'undefined') {
  port = {
    post: (m) => self.postMessage(m),
    listen: (f) => (self.onmessage = (e) => f(e.data)),
  };
} else {
  const { parentPort } = await import('node:worker_threads');
  port = { post: (m) => parentPort.postMessage(m), listen: (f) => parentPort.on('message', f) };
}

let ctrl;
let data;
let config = {};

async function waitWhile(index, value) {
  if (typeof Atomics.waitAsync === 'function') {
    const r = Atomics.waitAsync(ctrl, index, value);
    if (r.async) await r.value;
    return;
  }
  while (Atomics.load(ctrl, index) === value) await new Promise((r) => setTimeout(r, 0));
}

async function respond(bytes, failed) {
  const chunk = data.length;
  let at = 0;
  do {
    const n = Math.min(chunk, bytes.length - at);
    data.set(bytes.subarray(at, at + n), 0);
    at += n;
    ctrl[LEN] = n;
    ctrl[MORE] = at < bytes.length ? 1 : 0;
    ctrl[STATUS] = failed ? 1 : 0;
    Atomics.store(ctrl, STATE, 1);
    Atomics.notify(ctrl, STATE);
    if (at < bytes.length) await waitWhile(STATE, 1);
  } while (at < bytes.length);
}

// ---- file trees ----

function normalize(p) {
  const parts = [];
  for (const c of decoder.decode(p).split('/')) {
    if (c === '' || c === '.') continue;
    if (c === '..') parts.pop();
    else parts.push(c);
  }
  return parts;
}

function u64(n) {
  const out = new Uint8Array(8);
  new DataView(out.buffer).setBigUint64(0, BigInt(n), true);
  return out;
}

function concat(...parts) {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}

let manifest;
async function httpManifest(base) {
  if (!manifest) {
    const res = await fetch(new URL('index.json', base));
    if (!res.ok) throw new Error(`cannot load ${new URL('index.json', base)}: ${res.status}`);
    manifest = (await res.json()).files;
  }
  return manifest;
}

const httpFs = {
  async stat(parts, base) {
    const files = await httpManifest(base);
    const key = parts.join('/');
    if (key in files) return concat(Uint8Array.of(1), u64(files[key]));
    const prefix = key ? key + '/' : '';
    if (Object.keys(files).some((f) => f.startsWith(prefix))) return concat(Uint8Array.of(2), u64(0));
    return Uint8Array.of(0);
  },
  async list(parts, base) {
    const files = await httpManifest(base);
    const prefix = parts.length ? parts.join('/') + '/' : '';
    const seen = new Map();
    for (const [name, size] of Object.entries(files)) {
      if (!name.startsWith(prefix)) continue;
      const rest = name.slice(prefix.length);
      const slash = rest.indexOf('/');
      if (slash < 0) seen.set(rest, ['f', size]);
      else seen.set(rest.slice(0, slash), ['d', 0]);
    }
    return encoder.encode([...seen].map(([n, [k, s]]) => `${k}\t${s}\t${n}`).join('\n'));
  },
  async read(parts, base) {
    const res = await fetch(new URL(parts.map(encodeURIComponent).join('/'), base));
    if (!res.ok) return Uint8Array.of(0);
    return concat(Uint8Array.of(1), new Uint8Array(await res.arrayBuffer()));
  },
};

async function opfsDir(parts, create = false) {
  let dir = await navigator.storage.getDirectory();
  for (const p of parts) dir = await dir.getDirectoryHandle(p, { create });
  return dir;
}

const opfsFs = {
  async stat(parts) {
    if (parts.length === 0) return concat(Uint8Array.of(2), u64(0));
    const parent = await opfsDir(parts.slice(0, -1)).catch(() => null);
    if (!parent) return Uint8Array.of(0);
    const name = parts[parts.length - 1];
    try {
      const f = await (await parent.getFileHandle(name)).getFile();
      return concat(Uint8Array.of(1), u64(f.size));
    } catch {}
    try {
      await parent.getDirectoryHandle(name);
      return concat(Uint8Array.of(2), u64(0));
    } catch {
      return Uint8Array.of(0);
    }
  },
  async list(parts) {
    const dir = await opfsDir(parts);
    const lines = [];
    for await (const [name, handle] of dir.entries()) {
      const size = handle.kind === 'file' ? (await handle.getFile()).size : 0;
      lines.push(`${handle.kind === 'file' ? 'f' : 'd'}\t${size}\t${name}`);
    }
    return encoder.encode(lines.join('\n'));
  },
  async read(parts) {
    try {
      const parent = await opfsDir(parts.slice(0, -1));
      const f = await (await parent.getFileHandle(parts[parts.length - 1])).getFile();
      return concat(Uint8Array.of(1), new Uint8Array(await f.arrayBuffer()));
    } catch {
      return Uint8Array.of(0);
    }
  },
};

async function handle(kind, payload) {
  if (kind.startsWith('fs.')) {
    const cfg = config.fs || {};
    const backend = cfg.type === 'opfs' ? opfsFs : httpFs;
    return backend[kind.slice(3)](normalize(payload), cfg.base);
  }
  if (kind === 'http.get') {
    const res = await fetch(decoder.decode(payload));
    return new Uint8Array(await res.arrayBuffer());
  }
  if (kind === 'http.request') {
    const request = decodeSyncRequest(payload);
    if (request.mode === 'same-origin' && request.origin !== null &&
        new URL(request.url).origin !== request.origin) {
      throw new Error('synchronous request blocked by same-origin mode');
    }
    let response;
    const xhrAvailable = typeof XMLHttpRequest === 'function' && request.mode === 'cors' && request.redirect === 'follow';
    if (request.forcePreflight && !xhrAvailable) {
      throw new Error('NotSupportedError: this host cannot force the required CORS preflight');
    }
    if (xhrAvailable) {
      response = await xhrRequest(request);
    } else {
      const controller = new AbortController();
      let timedOut = false;
      const timer = request.timeoutMs
        ? setTimeout(() => { timedOut = true; controller.abort(); }, request.timeoutMs)
        : null;
      const init = {
        method: request.method,
        headers: request.headers,
        mode: request.mode,
        credentials: request.credentials,
        redirect: request.redirect,
        signal: controller.signal,
      };
      if (request.body !== null && request.method !== 'GET' && request.method !== 'HEAD') {
        init.body = request.body;
      }
      try {
        const result = await fetch(request.url, init);
        response = {
          status: result.status,
          statusText: result.statusText,
          url: result.url || request.url,
          headers: [...result.headers],
          body: new Uint8Array(await result.arrayBuffer()),
        };
      } catch (error) {
        if (timedOut) throw new Error('TimeoutError: synchronous HTTP request timed out');
        throw error;
      } finally {
        if (timer !== null) clearTimeout(timer);
      }
    }
    return encodeSyncResponse({
      status: response.status,
      statusText: response.statusText,
      url: response.url,
      headers: response.headers,
      body: response.body,
    });
  }
  if (kind === 'sleep') {
    await new Promise((r) => setTimeout(r, Number(decoder.decode(payload))));
    return new Uint8Array(0);
  }
  throw new Error(`unknown host call '${kind}'`);
}

const HTTP_FIELD_LIMIT = 1 << 20;
const HTTP_HEADER_LIMIT = 8192;
const HTTP_HEADER_BYTES = 64 << 10;
const HTTP_BODY_LIMIT = 32 << 20;

function decodeSyncRequest(input) {
  let at = 0;
  const take = n => {
    if (!Number.isSafeInteger(n) || n < 0 || at + n > input.length) throw new Error('truncated synchronous HTTP request');
    const value = input.subarray(at, at + n);
    at += n;
    return value;
  };
  const u8 = () => take(1)[0];
  const u16 = () => new DataView(take(2).buffer, input.byteOffset + at - 2, 2).getUint16(0, true);
  const u32 = limit => {
    const value = new DataView(take(4).buffer, input.byteOffset + at - 4, 4).getUint32(0, true);
    if (value > limit) throw new Error('synchronous HTTP field is too large');
    return value;
  };
  const text16 = () => decoder.decode(take(u16()));
  const text32 = () => decoder.decode(take(u32(HTTP_FIELD_LIMIT)));
  const headers = () => {
    const count = u16();
    if (count > HTTP_HEADER_LIMIT) throw new Error('too many synchronous HTTP headers');
    const out = [];
    let size = 0;
    for (let i = 0; i < count; i++) {
      const name = text16(), value = text32();
      size += encoder.encode(name).length + encoder.encode(value).length;
      if (size > HTTP_HEADER_BYTES) throw new Error('synchronous HTTP headers exceed 64 KiB');
      out.push([name, value]);
    }
    return out;
  };
  if (decoder.decode(take(4)) !== 'XHR1') throw new Error('invalid synchronous HTTP request');
  const method = text16();
  const url = text32();
  const requestHeaders = headers();
  const mode = text16();
  const credentials = text16();
  const redirect = text16();
  const forcePreflight = u8();
  if (forcePreflight > 1) throw new Error('invalid synchronous HTTP preflight marker');
  const timeoutMs = u32(0xffffffff);
  const hasOrigin = u8();
  if (hasOrigin > 1) throw new Error('invalid synchronous HTTP origin marker');
  const origin = hasOrigin ? text32() : null;
  const hasBody = u8();
  if (hasBody > 1) throw new Error('invalid synchronous HTTP body marker');
  const body = hasBody ? take(u32(HTTP_BODY_LIMIT)) : null;
  if (at !== input.length) throw new Error('trailing synchronous HTTP request data');
  return {method, url, headers: requestHeaders, mode, credentials, redirect, forcePreflight: !!forcePreflight, timeoutMs, origin, body};
}

function xhrRequest(request) {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open(request.method, request.url, true);
    xhr.withCredentials = request.credentials === 'include';
    xhr.timeout = request.timeoutMs;
    xhr.responseType = 'arraybuffer';
    for (const [name, value] of request.headers) xhr.setRequestHeader(name, value);
    if (request.forcePreflight) xhr.upload.addEventListener('progress', () => {});
    xhr.onload = () => {
      const headers = [];
      for (const line of xhr.getAllResponseHeaders().split('\r\n')) {
        const colon = line.indexOf(':');
        if (colon > 0) headers.push([line.slice(0, colon).trim(), line.slice(colon + 1).trim()]);
      }
      resolve({
        status: xhr.status,
        statusText: xhr.statusText,
        url: xhr.responseURL || request.url,
        headers,
        body: new Uint8Array(xhr.response || new ArrayBuffer(0)),
      });
    };
    xhr.onerror = () => reject(new Error('synchronous host HTTP request failed'));
    xhr.ontimeout = () => reject(new Error('TimeoutError: synchronous HTTP request timed out'));
    xhr.onabort = () => reject(new Error('synchronous host HTTP request was aborted'));
    try { xhr.send(request.body); } catch (error) { reject(error); }
  });
}

function encodeSyncResponse(response) {
  const parts = [];
  const u16 = n => parts.push(Uint8Array.of(n & 255, (n >>> 8) & 255));
  const u32 = n => parts.push(Uint8Array.of(n & 255, (n >>> 8) & 255, (n >>> 16) & 255, (n >>> 24) & 255));
  const text16 = value => {
    const bytes = encoder.encode(value);
    if (bytes.length > 65535) throw new Error('synchronous HTTP field is too large');
    u16(bytes.length); parts.push(bytes);
  };
  const text32 = value => {
    const bytes = encoder.encode(value);
    if (bytes.length > HTTP_FIELD_LIMIT) throw new Error('synchronous HTTP field is too large');
    u32(bytes.length); parts.push(bytes);
  };
  const body = response.body;
  if (body.length > HTTP_BODY_LIMIT) throw new Error('synchronous HTTP response body is too large');
  parts.push(encoder.encode('XHS1'));
  u16(response.status);
  text16(response.statusText);
  text32(response.url);
  if (response.headers.length > HTTP_HEADER_LIMIT) throw new Error('too many synchronous HTTP headers');
  u16(response.headers.length);
  let headerBytes = 0;
  for (const [name, value] of response.headers) {
    headerBytes += encoder.encode(name).length + encoder.encode(value).length;
    if (headerBytes > HTTP_HEADER_BYTES) throw new Error('synchronous HTTP headers exceed 64 KiB');
    text16(name); text32(value);
  }
  u32(body.length); parts.push(body);
  const length = parts.reduce((n, part) => n + part.length, 0);
  const output = new Uint8Array(length);
  let offset = 0;
  for (const part of parts) { output.set(part, offset); offset += part.length; }
  return output;
}

port.listen(async (msg) => {
  if (msg.type === 'init') {
    ctrl = new Int32Array(msg.sab, 0, 4);
    data = new Uint8Array(msg.sab, HEADER_BYTES);
    config = msg.config || {};
    port.post({ type: 'ready' });
  } else if (msg.type === 'call') {
    try {
      await respond(await handle(msg.kind, msg.payload), false);
    } catch (e) {
      await respond(encoder.encode(String((e && e.message) || e)), true);
    }
  }
});
