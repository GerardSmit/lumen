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
  if (kind === 'sleep') {
    await new Promise((r) => setTimeout(r, Number(decoder.decode(payload))));
    return new Uint8Array(0);
  }
  throw new Error(`unknown host call '${kind}'`);
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
