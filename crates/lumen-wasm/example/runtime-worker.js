// The Worker that hosts the runtime in the browser page: loads the wasm module, runs the smoke
// script and posts the verdict back. The sync-call helper is a nested Worker.

import init, { RuntimeSession } from './pkg/lumen_wasm.js';
import { runSmoke } from './smoke-runner.js';

self.onmessage = async () => {
  try {
    await init();
    const [script, expected] = await Promise.all([
      fetch('./smoke-script.js').then((r) => r.text()),
      fetch('./expected.json').then((r) => r.json()),
    ]);
    const result = await runSmoke({
      RuntimeSession,
      createWorker: (url) => new Worker(url, { type: 'module' }),
      assetsUrl: new URL('./assets/', self.location.href).href,
      script,
      expected,
    });
    self.postMessage({ type: 'result', isolated: self.crossOriginIsolated, ...result });
  } catch (e) {
    self.postMessage({ type: 'result', pass: false, mismatches: [String((e && e.stack) || e)], output: [] });
  }
};
