// Builds crates/lumen-node/src/js/url.js from Node 20.11's lib/querystring.js, lib/url.js,
// lib/internal/querystring.js, lib/internal/constants.js and parts of lib/internal/url.js.
// usage: node gen.js <out>
const fs = require('fs');
const path = require('path');
const here = __dirname;
const nodeurl = path.join(here, '..', 'nodeurl');
const nodefs = path.join(here, '..', 'nodefs');
const read = (p) => fs.readFileSync(p, 'utf8').replace(/\r\n/g, '\n');

function stripLicense(src) {
  // Drop the Joyent header comment block; keep everything from 'use strict' on.
  const i = src.indexOf("'use strict';");
  return i >= 0 ? src.slice(i) : src;
}
function indent(src) {
  return src.split('\n').map((l) => (l.length ? '  ' + l : l)).join('\n');
}
function mod(id, src) {
  return `defineModule(${JSON.stringify(id)}, function (module, exports, require, internalBinding, primordials) {\n` +
    indent(src.trimEnd()) + '\n});\n\n';
}

const internalUrl = read(path.join(nodefs, 'internal_url.js')).split('\n');
const lines = (a, b) => internalUrl.slice(a - 1, b).join('\n');
// Sanity anchors so a changed source fails loudly instead of slicing garbage.
function expectLine(n, text) {
  if (!internalUrl[n - 1].startsWith(text)) throw new Error(`internal_url.js:${n} is not ${text}: ${internalUrl[n - 1]}`);
}
expectLine(99, 'const unsafeProtocol');
expectLine(755, 'function isURL');
expectLine(740, '/**');
expectLine(1288, 'function domainToASCII');
expectLine(1479, 'function toPathIfFileURL');

const internalUrlSrc = `'use strict';

// lumen: the WHATWG URL / URLSearchParams classes are lumen-web's port of this module (the
// globals); the rest below is verbatim from lib/internal/url.js.
const {
  Boolean,
  Number,
  RegExpPrototypeSymbolReplace,
  SafeSet,
  StringPrototypeCharAt,
  StringPrototypeCharCodeAt,
  StringPrototypeCodePointAt,
  StringPrototypeIndexOf,
  StringPrototypeSlice,
  StringPrototypeStartsWith,
  decodeURIComponent,
} = primordials;

const {
  codes: {
    ERR_INVALID_ARG_TYPE,
    ERR_INVALID_ARG_VALUE,
    ERR_INVALID_FILE_URL_HOST,
    ERR_INVALID_FILE_URL_PATH,
    ERR_INVALID_URL_SCHEME,
    ERR_MISSING_ARGS,
  },
} = require('internal/errors');
const {
  CHAR_BACKWARD_SLASH,
  CHAR_FORWARD_SLASH,
  CHAR_LOWERCASE_A,
  CHAR_LOWERCASE_Z,
} = require('internal/constants');
const path = require('path');
const { encodeStr } = require('internal/querystring');
const { toUSVString } = require('internal/util');

const { platform } = process;
const isWindows = platform === 'win32';

const bindingUrl = internalBinding('url');
const { URL, URLSearchParams } = bindingUrl;
const updateActions = bindingUrl.updateActions;

const FORWARD_SLASH = /\\//g;
const SideEffectFreeRegExpPrototypeSymbolReplace = RegExpPrototypeSymbolReplace;

${lines(99, 125)}

${lines(740, 757)}

${lines(1288, 1332)}

${lines(1334, 1490)}

module.exports = {
  toUSVString,
  fileURLToPath,
  pathToFileURL,
  toPathIfFileURL,
  URL,
  URLSearchParams,
  domainToASCII,
  domainToUnicode,
  urlToHttpOptions,
  encodeStr,
  isURL,

  urlUpdateActions: updateActions,
  unsafeProtocol,
  hostlessProtocol,
  slashedProtocol,
};
`;

let out = read(path.join(here, 'head.js'));
out += mod('internal/constants', read(path.join(here, 'constants.js')));
out += mod('internal/querystring', read(path.join(nodeurl, 'lib_internal_querystring.js')));
out += mod('querystring', stripLicense(read(path.join(nodeurl, 'lib_querystring.js'))));
out += mod('internal/url', internalUrlSrc);
out += mod('url', stripLicense(read(path.join(nodeurl, 'lib_url.js'))));
out += read(path.join(here, 'tail.js'));
fs.writeFileSync(process.argv[2], out);
console.log('wrote', out.length);
