// Builds crates/lumen-node/src/js/net.js from Node 20.11's lib/net.js, lib/_http_*.js,
// lib/http.js, lib/https.js and the internal modules they use (netgen/node/), over head.js
// (module table, shims, bindings) and tail.js (registration).
// usage: node gen.js <out>
const fs = require('fs');
const path = require('path');
const here = __dirname;
const node = path.join(here, 'node');
const read = (p) => fs.readFileSync(p, 'utf8').replace(/\r\n/g, '\n');
const src = (name) => read(path.join(node, name));

function stripLicense(text) {
  const i = text.indexOf("'use strict';");
  return i >= 0 ? text.slice(i) : text;
}
function indent(text) {
  return text.split('\n').map((l) => (l.length ? '  ' + l : l)).join('\n');
}
function mod(id, text) {
  return `defineModule(${JSON.stringify(id)}, function (module, exports, require, internalBinding, primordials) {\n` +
    indent(text.trimEnd()) + '\n});\n\n';
}
// A `lumen:` edit: `find` must occur exactly once.
function patch(text, find, replace, file) {
  const first = text.indexOf(find);
  if (first < 0 || text.indexOf(find, first + 1) >= 0) {
    throw new Error(`${file}: patch anchor ${first < 0 ? 'missing' : 'not unique'}: ${find.slice(0, 80)}`);
  }
  return text.slice(0, first) + replace + text.slice(first + find.length);
}

// ---- net.js edits ----
let net = stripLicense(src('net.js'));
net = patch(net, `  stream.Duplex.call(this, options);\n`,
  `  stream.Duplex.call(this, options);\n` +
  `  // lumen: \`_deferRead\` (bun:sql drivers) keeps the socket from reading until a TLS upgrade\n` +
  `  // or explicit \`_readRaw()\` calls take over the connection.\n` +
  `  this._deferRead = !!options._deferRead;\n`, 'net.js');
net = patch(net, `Socket.prototype._read = function(n) {\n`,
  `Socket.prototype._read = function(n) {\n  if (this._deferRead) return; // lumen\n`, 'net.js');
net = patch(net, `module.exports = {\n`,
  `// lumen: raw access used by the bun:sql drivers and tls.connect({ socket }) (tls.js): the native\n` +
  `// socket id, and one read straight off it (for a socket created with \`_deferRead\`).\n` +
  `ObjectDefineProperty(Socket.prototype, '_id', {\n` +
  `  __proto__: null,\n` +
  `  configurable: true,\n` +
  `  get() { return this._handle ? this._handle._id : null; },\n` +
  `});\n` +
  `Socket.prototype._readRaw = function() {\n` +
  `  const id = this._id;\n` +
  `  if (id === null) return PromiseReject(new ERR_SOCKET_CLOSED());\n` +
  `  return new Promise((resolve, reject) =>\n` +
  `    __net.read(id, (value) => resolve(value === null ? null : Buffer.from(value)), reject));\n` +
  `};\n` +
  `// lumen: hand the native socket to another owner (tls.js upgrade); this Socket then closes\n` +
  `// without touching it.\n` +
  `Socket.prototype._detachNative = function() {\n` +
  `  const handle = this._handle;\n` +
  `  const id = handle ? handle._id : null;\n` +
  `  if (handle) handle._id = null;\n` +
  `  this.destroy();\n` +
  `  return id;\n` +
  `};\n\n` +
  `module.exports = {\n`, 'net.js');
// lumen's cluster has no handle sharing (no `_getServer`): a worker listens by itself.
net = patch(net, `  if (cluster.isPrimary || exclusive) {\n`,
  `  if (cluster.isPrimary || exclusive || typeof cluster._getServer !== 'function') { // lumen\n`,
  'net.js');
if (!/\bObjectDefineProperty,/.test(net.slice(0, 2000))) throw new Error('net.js: ObjectDefineProperty not destructured');
net = patch(net, `} = primordials;\n`, `  PromiseReject,\n} = primordials;\n`, 'net.js');

let out = read(path.join(here, 'head.js'));
out += mod('internal/validators', src('internal_validators.js'));
out += mod('internal/net', src('internal_net.js'));
out += mod('internal/stream_base_commons', src('internal_stream_base_commons.js'));
// lumen: SocketAddress.parse (Node 22), which programs written against newer Node use.
out += mod('internal/socketaddress', patch(src('internal_socketaddress.js'),
  `  static isSocketAddress(value) {\n`,
  `  static parse(input) {\n` +
  `    validateString(input, 'input');\n` +
  `    try {\n` +
  `      const { hostname: address, port } = new URL(\`http://\${input}\`);\n` +
  `      if (address.startsWith('[') && address.endsWith(']')) {\n` +
  `        return new SocketAddress({ address: address.slice(1, -1), port: port | 0, family: 'ipv6' });\n` +
  `      }\n` +
  `      return new SocketAddress({ address, port: port | 0 });\n` +
  `    } catch {\n` +
  `      // Not an address: undefined, as in Node.\n` +
  `    }\n` +
  `  }\n\n` +
  `  static isSocketAddress(value) {\n`, 'internal_socketaddress.js'));
out += mod('internal/blocklist', src('internal_blocklist.js'));
out += mod('net', net);

// ---- http ----
out += read(path.join(here, 'http_parser.js'));
out += mod('internal/constants', read(path.join(here, '..', 'urlgen', 'constants.js')));
out += mod('internal/freelist', src('internal_freelist.js'));
out += mod('internal/http', src('internal_http.js'));
// lumen: the glue loads before process.execArgv is set, so --insecure-http-parser is read
// on first use instead of at module load.
let common = stripLicense(src('_http_common.js'));
common = patch(common, "const insecureHTTPParser = getOptionValue('--insecure-http-parser');\n",
  "let insecureHTTPParser; // lumen: read lazily in isLenient()\n", '_http_common.js');
common = patch(common, "function isLenient() {\n",
  "function isLenient() {\n" +
  "  if (insecureHTTPParser === undefined) insecureHTTPParser = getOptionValue('--insecure-http-parser');\n",
  '_http_common.js');
out += mod('_http_common', common);
out += mod('_http_incoming', stripLicense(src('_http_incoming.js')));
out += mod('_http_outgoing', stripLicense(src('_http_outgoing.js')));
out += mod('_http_agent', stripLicense(src('_http_agent.js')));
out += mod('_http_client', stripLicense(src('_http_client.js')));
out += mod('_http_server', stripLicense(src('_http_server.js')));
out += mod('http', stripLicense(src('http.js')));

// https: lumen's tls.Server is a class (tls.js), so https.Server initializes itself through
// its `_init` rather than calling the constructor on `this`.
let https = stripLicense(src('https.js'));
https = patch(https,
  `  FunctionPrototypeCall(tls.Server, this,`,
  `  // lumen: tls.Server is a class; run its initializer on this object.
` +
  `  FunctionPrototypeCall(tls.Server.prototype._init, this,`, 'https.js');
out += mod('https', https);
out += read(path.join(here, 'tail.js'));
fs.writeFileSync(process.argv[2], out);
console.log('wrote', out.length);
