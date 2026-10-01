// Builds crates/lumen-node/src/js/crypto.js from Node 20.11's lib/crypto.js and
// lib/internal/crypto/*.js (node/), over head.js (module table, shims), the binding files
// (binding/*.js: internalBinding('crypto') over the RustCrypto ops) and tail.js (registration).
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

let out = read(path.join(here, 'head.js'));
for (const f of fs.readdirSync(path.join(here, 'binding')).filter((n) => n.endsWith('.js')).sort()) {
  out += read(path.join(here, 'binding', f)) + '\n';
}
out += mod('internal/validators', stripLicense(read(path.join(here, '..', 'netgen', 'node', 'internal_validators.js'))));
out += mod('internal/streams/lazy_transform', stripLicense(src('internal_streams_lazy_transform.js')));
for (const f of fs.readdirSync(node).filter((n) => n.startsWith('internal_crypto_')).sort()) {
  const id = 'internal/crypto/' + f.slice('internal_crypto_'.length, -3);
  out += mod(id, stripLicense(src(f)));
}
out += mod('crypto', stripLicense(src('crypto.js')));
out += read(path.join(here, 'tail.js'));
fs.writeFileSync(process.argv[2], out);
