// Headless smoke test under Node: the wasm runtime built with
//   wasm-bindgen --target nodejs --out-dir pkg-node ...
// (see ../build.sh). The sync-call helper runs as a worker_threads Worker, so this exercises the
// same Atomics bridge the browser uses. Usage: node smoke-node.mjs [pkg-node-dir]

import { createRequire } from 'node:module';
import { readFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { Worker } from 'node:worker_threads';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { runSmoke } from './smoke-runner.js';

const here = dirname(fileURLToPath(import.meta.url));
const pkg = process.argv[2] || join(here, 'pkg-node');
const { RuntimeSession } = createRequire(import.meta.url)(join(pkg, 'lumen_wasm.js'));

const server = createServer((req, res) => {
  try {
    const body = readFileSync(join(here, 'assets', decodeURIComponent(new URL(req.url, 'http://x').pathname)));
    res.writeHead(200).end(body);
  } catch {
    res.writeHead(404).end();
  }
});
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const assetsUrl = `http://127.0.0.1:${server.address().port}/`;

const result = await runSmoke({
  RuntimeSession,
  createWorker: (url) => new Worker(url),
  assetsUrl,
  script: readFileSync(join(here, 'smoke-script.js'), 'utf8'),
  expected: JSON.parse(readFileSync(join(here, 'expected.json'), 'utf8')),
});
server.close();
for (const l of result.output) console.log(l);
if (result.pass) {
  console.log('smoke: PASS', Object.keys(result.actual).length, 'checks');
} else {
  console.log('smoke: FAIL');
  for (const m of result.mismatches) console.log('  ' + m);
  process.exitCode = 1;
}
