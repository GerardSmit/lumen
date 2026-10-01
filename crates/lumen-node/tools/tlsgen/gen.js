// Builds crates/lumen-node/src/js/tls.js from Node 20.11's lib/tls.js, lib/_tls_wrap.js,
// lib/_tls_common.js and the internal modules they use (tlsgen/node/), over head.js (module
// table, shims), x509.js and tlswrap.js (the bindings), bindings.js and tail.js (registration).
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

let wrap = stripLicense(src('_tls_wrap.js'));
wrap = patch(wrap, `  assert(handle.isStreamBase, 'handle must be a StreamBase');\n`,
  `  // lumen: net's handles are not flagged StreamBase (that flag gates native fast paths).\n`,
  '_tls_wrap.js');

let out = read(path.join(here, 'head.js'));
out += read(path.join(here, 'x509.js')) + '\n';
out += read(path.join(here, 'tlswrap.js'));
out += read(path.join(here, 'bindings.js'));
out += mod('internal/js_stream_socket', stripLicense(src('internal_js_stream_socket.js')));
out += mod('internal/tls/secure-context', stripLicense(src('internal_tls_secure_context.js')));
out += mod('internal/tls/secure-pair', stripLicense(src('internal_tls_secure_pair.js')));
out += mod('_tls_common', stripLicense(src('_tls_common.js')));
out += mod('_tls_wrap', wrap);
out += mod('tls', stripLicense(src('tls.js')));
out += read(path.join(here, 'tail.js'));
fs.writeFileSync(process.argv[2], out);
