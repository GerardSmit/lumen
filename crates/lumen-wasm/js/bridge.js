// The suspending synchronous host call. Runs in the Worker that hosts the wasm module.
//
// A runtime op that must look synchronous to the script (`fs.readFileSync` of a file that lives
// on a server or in OPFS) calls the host's `syncCall(kind, payload)`. This module implements it:
// the request goes to a helper Worker (bridge-worker.js), which awaits the browser's Promise and
// writes the answer into a SharedArrayBuffer; this thread blocks in `Atomics.wait` until it is
// there. Nothing else of the realm runs meanwhile. The page must be cross-origin isolated
// (COOP: same-origin, COEP: require-corp) for SharedArrayBuffer to exist.
//
// JSPI (`WebAssembly.Suspending`) could suspend the wasm stack instead and needs neither the
// helper nor isolation, but it needs the host import wrapped at instantiation, which the
// wasm-bindgen glue does not expose; it is not implemented here.

const STATE = 0;
const LEN = 1;
const MORE = 2;
const STATUS = 3;
const HEADER_BYTES = 16;
const CHUNK_BYTES = 1 << 20;

const encoder = new TextEncoder();
const decoder = new TextDecoder();

/**
 * @param {object} options
 * @param {(url: URL) => any} options.createWorker  starts bridge-worker.js; returns the Worker
 *   (browser) or worker_threads Worker (Node).
 * @param {object} [options.config]  `{ fs: { type: 'http', base } | { type: 'opfs' } }`
 * @param {number} [options.timeoutMs]
 * @returns {Promise<{ call(kind: string, payload?: Uint8Array | string): Uint8Array, close(): void }>}
 */
export async function createSyncBridge({ createWorker, config = {}, timeoutMs = 60000 }) {
  if (typeof SharedArrayBuffer === 'undefined') {
    throw new Error(
      'SharedArrayBuffer is unavailable: serve the page with Cross-Origin-Opener-Policy: same-origin ' +
        'and Cross-Origin-Embedder-Policy: require-corp',
    );
  }
  const sab = new SharedArrayBuffer(HEADER_BYTES + CHUNK_BYTES);
  const ctrl = new Int32Array(sab, 0, 4);
  const data = new Uint8Array(sab, HEADER_BYTES);
  const worker = createWorker(new URL('./bridge-worker.js', import.meta.url));

  await new Promise((resolve, reject) => {
    const onMessage = (m) => {
      const msg = m && m.data !== undefined ? m.data : m;
      if (msg && msg.type === 'ready') resolve();
      else if (msg && msg.type === 'error') reject(new Error(msg.message));
    };
    if (worker.addEventListener) {
      worker.addEventListener('message', onMessage);
      worker.addEventListener('error', (e) => reject(new Error(e.message || 'bridge worker failed')));
    } else {
      worker.on('message', onMessage);
      worker.on('error', reject);
    }
    worker.postMessage({ type: 'init', sab, config });
  });

  function call(kind, payload = new Uint8Array(0), options = {}) {
    const bytes = typeof payload === 'string' ? encoder.encode(payload) : payload;
    Atomics.store(ctrl, STATE, 0);
    worker.postMessage({ type: 'call', kind, payload: bytes });
    const parts = [];
    let failed = false;
    for (;;) {
      const waited = Atomics.wait(ctrl, STATE, 0, options.timeoutMs ?? timeoutMs);
      if (waited === 'timed-out') throw new Error(`host call '${kind}' timed out`);
      const len = ctrl[LEN];
      failed = ctrl[STATUS] === 1;
      parts.push(data.slice(0, len));
      const more = ctrl[MORE] === 1;
      Atomics.store(ctrl, STATE, 0);
      Atomics.notify(ctrl, STATE);
      if (!more) break;
    }
    const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
    let at = 0;
    for (const p of parts) {
      out.set(p, at);
      at += p.length;
    }
    if (failed) throw new Error(decoder.decode(out));
    return out;
  }

  return {
    call,
    close() {
      if (worker.terminate) worker.terminate();
    },
  };
}
