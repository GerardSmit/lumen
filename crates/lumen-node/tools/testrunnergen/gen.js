// Assembles crates/lumen-node/src/js/test_runner.js from head.js (module table, bindings, adapters),
// Node 20.11's test runner sources (node/), and tail.js.
// usage: node gen.js <out>
const fs = require('fs'), path = require('path');
const G = __dirname, N = path.join(G, 'node');
const mods = [
  'internal/error_serdes',
  'internal/watch_mode/files_watcher',
  'internal/test_runner/tests_stream',
  'internal/test_runner/utils',
  'internal/test_runner/test',
  'internal/test_runner/harness',
  'internal/test_runner/runner',
  'internal/test_runner/coverage',
  'internal/test_runner/mock/mock',
  'internal/test_runner/mock/mock_timers',
  'internal/test_runner/reporter/dot',
  'internal/test_runner/reporter/junit',
  'internal/test_runner/reporter/lcov',
  'internal/test_runner/reporter/spec',
  'internal/test_runner/reporter/tap',
  'internal/test_runner/reporter/v8-serializer',
  'internal/main/test_runner',
  'test',
  'test/reporters',
];
// `lumen:` edits, [find, replace]; each `find` must occur exactly once.
const patches = {
  'internal/test_runner/runner': [
    [`          // The stack will not be useful since the failures came from tests
          // in a child process.
          stack: undefined,
        });`,
     `        });
        // lumen: V8 errors carry an own \`stack\`, which the assignment overwrote; lumen's errors
        // inherit Error.prototype.stack, whose setter only takes strings.
        // The stack will not be useful since the failures came from tests
        // in a child process.
        ObjectDefineProperty(err, 'stack', { __proto__: null, value: undefined, writable: true, configurable: true });`],
    [`  ObjectAssign,
`, `  ObjectAssign,
  ObjectDefineProperty,
`],
  ],
};
let out = fs.readFileSync(path.join(G, 'head.js'), 'utf8');
for (const id of mods) {
  let src = fs.readFileSync(path.join(N, id.replace(/\//g, '_') + '.js'), 'utf8').replace(/\r\n/g, '\n');
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
