// Assembles crates/lumen-node/src/js/internals.js from head.js (module table, bindings, adapters),
// Node 20.11's sources of the self-contained internal modules (node/), and tail.js.
// usage: node gen.js <out>
const fs = require('fs'), path = require('path');
const G = __dirname, N = path.join(G, 'node');
const mods = [
  ['internal/assert', 'internal_assert.js'],
  ['internal/errors', 'internal_errors.js'],
  ['internal/util', 'internal_util.js'],
  ['internal/util/colors', 'internal_util_colors.js'],
  ['internal/validators', 'internal_validators.js'],
  ['internal/linkedlist', 'internal_linkedlist.js'],
  ['internal/priority_queue', 'internal_priority_queue.js'],
  ['internal/fixed_queue', 'internal_fixed_queue.js'],
  ['internal/util/iterable_weak_map', 'internal_util_iterable_weak_map.js'],
];
// `lumen:` edits, [find, replace]; each `find` must occur exactly once.
const patches = {
  'internal/util': [
    [`const kCustomPromisifyArgsSymbol = Symbol('customPromisifyArgs');`,
     `// lumen: the symbol lumen's util.promisify reads.
const kCustomPromisifyArgsSymbol = __internals.get('customPromisifyArgs');`],
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
