// Runs smoke-script.js inside the runtime and compares the outcome with expected.json. Shared by
// the Node smoke test (smoke-node.mjs) and the browser page (index.html / runtime-worker.js).

import { createSyncBridge } from '../js/bridge.js';
import { createRuntime } from '../js/host.js';

export async function runSmoke({ RuntimeSession, createWorker, assetsUrl, script, expected, timeoutMs = 20000 }) {
  const bridge = await createSyncBridge({
    createWorker,
    config: { fs: { type: 'http', base: assetsUrl } },
  });
  const lines = [];
  const rt = await createRuntime({
    RuntimeSession,
    bridge,
    options: { cwd: '/work', argv: ['lumen', 'smoke'] },
    onOutput: (stream, text) => lines.push(...text.split('\n').filter(Boolean).map((l) => `[${stream}] ${l}`)),
  });
  rt.mountRemote('/remote');
  const first = rt.eval(script);
  await Promise.race([
    rt.whenIdle(),
    new Promise((_, reject) => setTimeout(() => reject(new Error('smoke script did not finish')), timeoutMs)),
  ]);
  bridge.close();

  const line = lines.find((l) => l.includes('RESULT '));
  const failure = lines.find((l) => l.includes('FAILED '));
  const mismatches = [];
  let actual = null;
  if (!line) {
    mismatches.push(failure || first.error || 'no RESULT line was printed');
  } else {
    actual = JSON.parse(line.slice(line.indexOf('RESULT ') + 7));
    for (const key of Object.keys(expected)) {
      if (JSON.stringify(actual[key]) !== JSON.stringify(expected[key])) {
        mismatches.push(`${key}: expected ${JSON.stringify(expected[key])}, got ${JSON.stringify(actual[key])}`);
      }
    }
  }
  return { pass: mismatches.length === 0, actual, mismatches, output: lines };
}
