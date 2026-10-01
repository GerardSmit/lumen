// A static server with the headers SharedArrayBuffer needs. Usage: node serve.mjs [port]
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { dirname, extname, join, normalize } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const types = {
  '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript',
  '.json': 'application/json', '.wasm': 'application/wasm', '.txt': 'text/plain',
};
const port = Number(process.argv[2] || 8123);

createServer(async (req, res) => {
  const path = decodeURIComponent(new URL(req.url, 'http://x').pathname);
  const file = normalize(join(root, path === '/' ? '/example/index.html' : path));
  const headers = {
    'Cross-Origin-Opener-Policy': 'same-origin',
    'Cross-Origin-Embedder-Policy': 'require-corp',
    'Cross-Origin-Resource-Policy': 'same-origin',
  };
  if (!file.startsWith(root)) return res.writeHead(403, headers).end();
  try {
    const body = await readFile(file);
    res.writeHead(200, { ...headers, 'Content-Type': types[extname(file)] || 'application/octet-stream' }).end(body);
  } catch {
    res.writeHead(404, headers).end('not found');
  }
}).listen(port, '127.0.0.1', () => console.log(`http://127.0.0.1:${port}/`));
