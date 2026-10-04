// Assembles crates/lumen-node/src/js/webstreams.js from head.js, Node 20.11's web streams sources,
// and tail.js. With a second output argument, also assembles webstreams_browser.js from the exact
// same upstream module sources, with a host-independent shim and without Node-zlib compression.
// usage: node gen.js <out> [<browser-out>]
const fs = require('fs'), path = require('path');
const G = __dirname, N = path.join(G, 'node');
const mods = [
  ['internal/webstreams/util', 'internal_webstreams_util.js'],
  ['internal/webstreams/queuingstrategies', 'internal_webstreams_queuingstrategies.js'],
  ['internal/webstreams/readablestream', 'internal_webstreams_readablestream.js'],
  ['internal/webstreams/writablestream', 'internal_webstreams_writablestream.js'],
  ['internal/webstreams/transformstream', 'internal_webstreams_transformstream.js'],
  ['internal/webstreams/encoding', 'internal_webstreams_encoding.js'],
  ['internal/webstreams/compression', 'internal_webstreams_compression.js'],
  ['stream/web', 'stream_web.js'],
];
const patches = {
  'internal/webstreams/readablestream': [
    [`  const startResult = startAlgorithm();

  PromisePrototypeThen(
    PromiseResolve(startResult),
    () => {
      controller[kState].started = true;
      assert(!controller[kState].pulling);
      assert(!controller[kState].pullAgain);
      readableStreamDefaultControllerCallPullIfNeeded(controller);`,
     `  const startResult = startAlgorithm();
  // lumen: a synchronous start is complete for readableStreamDefaultReaderReadSync.
  controller[kState].syncStart = typeof startResult?.then !== 'function';

  PromisePrototypeThen(
    PromiseResolve(startResult),
    () => {
      if (controller[kState].started) return; // lumen: started early by a synchronous read
      controller[kState].started = true;
      assert(!controller[kState].pulling);
      assert(!controller[kState].pullAgain);
      readableStreamDefaultControllerCallPullIfNeeded(controller);`],
    [`function setupReadableStreamBYOBReader(reader, stream) {`,
     `// lumen: one read that completes synchronously or not at all, for lumen-web's buffered
// Request/Response bodies (fetch.js). Returns \`{ pending: true }\` when the source cannot produce
// a chunk without awaiting; the read request is then withdrawn, leaving the stream as it was.
function readableStreamDefaultReaderReadSync(reader) {
  if (!isReadableStreamDefaultReader(reader))
    throw new ERR_INVALID_THIS('ReadableStreamDefaultReader');
  const { stream } = reader[kState];
  if (stream === undefined)
    throw new ERR_INVALID_STATE.TypeError('The reader is not attached to a stream');
  const { controller } = stream[kState];
  if (!isReadableByteStreamController(controller) &&
      !controller[kState].started && controller[kState].syncStart) {
    controller[kState].started = true;
  }
  let result;
  const readRequest = {
    [kChunk](value) { result = { value, done: false }; },
    [kClose]() { result = { value: undefined, done: true }; },
    [kError](error) { result = { error }; },
  };
  readableStreamDefaultReaderRead(reader, readRequest);
  if (result === undefined) {
    const { readRequests } = reader[kState];
    const at = readRequests.indexOf(readRequest);
    if (at !== -1) readRequests.splice(at, 1);
    return { pending: true };
  }
  if ('error' in result) throw result.error;
  return result;
}

function setupReadableStreamBYOBReader(reader, stream) {`],
    [`  readableStreamDefaultReaderRead,\n  setupReadableStreamBYOBReader,`,
     `  readableStreamDefaultReaderRead,\n  readableStreamDefaultReaderReadSync, // lumen\n  setupReadableStreamBYOBReader,`],
  ],
};
const browserPatches = {
  'stream/web': [
    [`const {
  CompressionStream,
  DecompressionStream,
} = require('internal/webstreams/compression');
`, ``],
    [`  CompressionStream,
  DecompressionStream,
`, ``],
  ],
};
let out = fs.readFileSync(path.join(G, 'head.js'), 'utf8');
for (const [id, file] of mods) {
  let src = fs.readFileSync(path.join(N, file), 'utf8').replace(/\r\n/g, '\n');
  for (const [from, to] of patches[id] || []) {
    const n = src.split(from).length - 1;
    if (n !== 1) throw new Error(`patch for ${id} matched ${n} times: ${from}`);
    src = src.replace(from, to);
  }
  src = src.replace(/\s+$/, '');
  out += `// ---- lib/${id}.js (Node v20.11.0) ${'-'.repeat(Math.max(3, 70 - id.length))}\n`;
  out += `defineModule(${JSON.stringify(id)}, function (module, exports, require, internalBinding, primordials) {\n${src}\n});\n\n`;
}
out += fs.readFileSync(path.join(G, 'tail.js'), 'utf8').replace(/^\n/, '');
fs.writeFileSync(process.argv[2], out);
console.log('wrote', out.split('\n').length, 'lines');

if (process.argv[3]) {
  const browserHead = fs.readFileSync(path.join(G, 'browser_head.js'), 'utf8');
  const browserTail = fs.readFileSync(path.join(G, 'browser_tail.js'), 'utf8');
  // Keep the engine primordial capture in one canonical source: the browser artifact receives
  // the exact resolver used by Node, extracted from preamble.js rather than forked here.
  const preamble = fs.readFileSync(path.join(G, '../../src/js/preamble.js'), 'utf8');
  const primordialStart = preamble.indexOf('// Node\'s `primordials` for glue ported verbatim');
  const primordialEnd = preamble.indexOf('// Node defines most of its public classes', primordialStart);
  if (primordialStart < 0 || primordialEnd < 0)
    throw new Error('could not locate canonical primordial resolver in preamble.js');
  const primordials = preamble.slice(primordialStart, primordialEnd);
  const nodeHead = fs.readFileSync(path.join(G, 'head.js'), 'utf8');
  const loaderStart = nodeHead.indexOf('// ---- the module table');
  const loaderEnd = nodeHead.indexOf('// Node\'s internal AbortError', loaderStart);
  if (loaderStart < 0 || loaderEnd < 0)
    throw new Error('could not locate canonical webstreams module loader in head.js');
  let browser = browserHead
    .replace('/* INSERT_CANONICAL_PRIMORDIALS */', primordials)
    .replace('/* INSERT_CANONICAL_MODULE_LOADER */', nodeHead.slice(loaderStart, loaderEnd));
  for (const [id, file] of mods) {
    // Compression is the sole Node-only stream module: it requires zlib and a Node Duplex adapter.
    // Do not register it at all in the host-independent extension.
    if (id === 'internal/webstreams/compression') continue;
    let src = fs.readFileSync(path.join(N, file), 'utf8').replace(/\r\n/g, '\n');
    const modePatches = [...(patches[id] || []), ...(browserPatches[id] || [])];
    for (const [from, to] of modePatches) {
      const n = src.split(from).length - 1;
      if (n !== 1) throw new Error(`browser patch for ${id} matched ${n} times: ${from}`);
      src = src.replace(from, to);
    }
    src = src.replace(/\s+$/, '');
    browser += `// ---- lib/${id}.js (Node v20.11.0) ${'-'.repeat(Math.max(3, 70 - id.length))}\n`;
    browser += `defineModule(${JSON.stringify(id)}, function (module, exports, require, internalBinding, primordials) {\n${src}\n});\n\n`;
  }
  browser += browserTail.replace(/^\n/, '');
  fs.writeFileSync(process.argv[3], browser);
  console.log('wrote browser subset', browser.split('\n').length, 'lines');
}
