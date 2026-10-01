// Builds crates/lumen-node/src/js/http2.js from Node 20.11's lib/http2.js and
// lib/internal/http2/{core,compat,util}.js (http2gen/node/), over head.js (module table, shims),
// binding.js (the nghttp2 binding, written in JS over lumen's sockets) and tail.js (registration).
// usage: node gen.js <out>
const fs = require('fs');
const path = require('path');
const here = __dirname;
const node = path.join(here, 'node');
const netNode = path.join(here, '..', 'netgen', 'node');
const read = (p) => fs.readFileSync(p, 'utf8').replace(/\r\n/g, '\n');

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
function patch(text, find, replace, file) {
  const first = text.indexOf(find);
  if (first < 0 || text.indexOf(find, first + 1) >= 0) {
    throw new Error(`${file}: patch anchor ${first < 0 ? 'missing' : 'not unique'}: ${find.slice(0, 80)}`);
  }
  return text.slice(0, first) + replace + text.slice(first + find.length);
}

let core = stripLicense(read(path.join(node, 'core.js')));
core = patch(core, `const JSStreamSocket = require('internal/js_stream_socket');\n`, '', 'core.js');
// lumen: the session reads and writes through the socket's own stream interface, so any
// Duplex (not only a StreamBase handle) can carry it.
core = patch(core,
  `  assert(socket._handle !== undefined,\n` +
  `         'Internal HTTP/2 Failure. The socket is not connected. Please ' +\n` +
  `         'report this as a bug in Node.js');\n\n`, '', 'core.js');
core = patch(core, `  handle.consume(socket._handle);\n`, `  handle.consume(socket); // lumen\n`, 'core.js');
core = patch(core,
  `    if (!socket._handle || !socket._handle.isStreamBase) {\n` +
  `      socket = new JSStreamSocket(socket);\n` +
  `    }\n`, '', 'core.js');

// lumen: a plain Duplex has no ref()/unref() (Node wraps it in a JSStreamSocket, which does).
core = patch(core, `    if (this[kSocket]) {\n      this[kSocket].ref();\n    }`,
  `    if (this[kSocket] && typeof this[kSocket].ref === 'function') {\n      this[kSocket].ref();\n    }`, 'core.js');
core = patch(core, `    if (this[kSocket]) {\n      this[kSocket].unref();\n    }`,
  `    if (this[kSocket] && typeof this[kSocket].unref === 'function') {\n      this[kSocket].unref();\n    }`, 'core.js');

let out = read(path.join(here, 'head.js'));
out += read(path.join(here, 'constants.js'));
out += read(path.join(here, 'binding.js'));
out += mod('internal/validators', stripLicense(read(path.join(netNode, 'internal_validators.js'))));
out += mod('internal/stream_base_commons', stripLicense(read(path.join(netNode, 'internal_stream_base_commons.js'))));
out += mod('internal/http2/util', stripLicense(read(path.join(node, 'util.js'))));
out += mod('internal/http2/compat', stripLicense(read(path.join(node, 'compat.js'))));
out += mod('internal/http2/core', core);
out += mod('http2', stripLicense(read(path.join(node, 'http2.js'))));
out += read(path.join(here, 'tail.js'));
fs.writeFileSync(process.argv[2], out);
