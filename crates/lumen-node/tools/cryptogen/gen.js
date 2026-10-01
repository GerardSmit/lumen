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

function patchCrypto(text) {
  const file = 'crypto.js';
  text = patch(text, "const {\n  Cipher,\n  Cipheriv,\n  Decipher,\n  Decipheriv,\n  privateDecrypt,",
    "const {\n  Cipheriv,\n  Decipheriv,\n  privateDecrypt,", file);
  text = patch(text, "const {\n  Hash,\n  Hmac,\n} = require('internal/crypto/hash');",
    "const {\n  Hash,\n  Hmac,\n} = require('internal/crypto/hash');\n" +
    "const { argon2, argon2Sync } = require('internal/crypto/argon2');\n" +
    "const {\n  validateObject,\n  validateString,\n  validateUint32,\n} = require('internal/validators');\n" +
    "const { isArrayBufferView } = require('internal/util/types');\n" +
    "const { normalizeEncoding } = require('internal/util');\n" +
    "const { ERR_INVALID_ARG_TYPE, ERR_INVALID_ARG_VALUE } = require('internal/errors').codes;", file);
  text = patch(text, "function createCipher(cipher, password, options) {\n  return new Cipher(cipher, password, options);\n}\n\n", '', file);
  text = patch(text, "function createDecipher(cipher, password, options) {\n  return new Decipher(cipher, password, options);\n}\n\n", '', file);
  text = patch(text, "  createCipher: {\n    __proto__: null,\n    enumerable: false,\n    value: deprecate(createCipher,\n" +
    "                     'crypto.createCipher is deprecated.', 'DEP0106'),\n  },\n  createDecipher: {\n    __proto__: null,\n" +
    "    enumerable: false,\n    value: deprecate(createDecipher,\n                     'crypto.createDecipher is deprecated.', 'DEP0106'),\n  },\n", '', file);
  text = patch(text, "  Certificate,\n  Cipher,\n  Cipheriv,\n  Decipher,\n  Decipheriv,", "  Certificate,\n  Cipheriv,\n  Decipheriv,", file);
  text = patch(text, "  checkPrime,\n  checkPrimeSync,\n  createCipheriv,", "  argon2,\n  argon2Sync,\n  hash,\n  checkPrime,\n  checkPrimeSync,\n  createCipheriv,", file);
  text = patch(text, "function getFips() {", `// lumen: crypto.hash(), the one-shot digest of Node 26's lib/crypto.js.
function hash(algorithm, input, options) {
  validateString(algorithm, 'algorithm');
  if (typeof input !== 'string' && !isArrayBufferView(input)) {
    throw new ERR_INVALID_ARG_TYPE('input', ['Buffer', 'TypedArray', 'DataView', 'string'], input);
  }
  let outputEncoding;
  let outputLength;
  if (typeof options === 'string') {
    outputEncoding = options;
  } else if (options !== undefined) {
    validateObject(options, 'options');
    outputLength = options.outputLength;
    outputEncoding = options.outputEncoding;
  }
  outputEncoding ??= 'hex';
  let normalized = outputEncoding;
  if (normalized !== 'hex') {
    validateString(outputEncoding, 'outputEncoding');
    normalized = normalizeEncoding(outputEncoding);
    if (normalized === undefined) {
      if (outputEncoding.toLowerCase() === 'buffer') {
        normalized = 'buffer';
      } else {
        throw new ERR_INVALID_ARG_VALUE('outputEncoding', outputEncoding);
      }
    }
  }
  if (outputLength !== undefined) {
    validateUint32(outputLength, 'outputLength');
    outputLength += 0;
  }
  const hasher = new Hash(algorithm, outputLength === undefined ? undefined : { outputLength });
  hasher.update(input);
  return normalized === 'buffer' ? hasher.digest() : hasher.digest(normalized);
}

function getFips() {`, file);
  return text;
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
out += mod('crypto', patchCrypto(stripLicense(src('crypto.js'))));
out += read(path.join(here, 'tail.js'));
fs.writeFileSync(process.argv[2], out);
