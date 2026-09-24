// Assembles crates/lumen-node/src/js/fs.js from head.js, Node 20.10's fs sources, and tail.js.
const fs = require('fs'), path = require('path');
const G = __dirname, N = path.join(G, '..', 'nodefs');
const mods = [
  ['fs', 'fs.js', 'lib/fs.js'],
  ['internal/fs/utils', 'internal_fs_utils.js'],
  ['internal/fs/promises', 'internal_fs_promises.js'],
  ['internal/fs/streams', 'internal_fs_streams.js'],
  ['internal/fs/dir', 'internal_fs_dir.js'],
  ['internal/fs/read/context', 'internal_fs_read_context.js'],
  ['internal/fs/rimraf', 'internal_fs_rimraf.js'],
  ['internal/fs/cp/cp', 'internal_fs_cp_cp.js'],
  ['internal/fs/cp/cp-sync', 'internal_fs_cp_cp-sync.js'],
  ['internal/fs/watchers', 'internal_fs_watchers.js'],
  ['internal/fs/recursive_watch', 'internal_fs_recursive_watch.js'],
];
const patches = {
  'internal/fs/promises': [
    ["const { Interface } = require('internal/readline/interface');",
     "// lumen: node:readline registers after node:fs, so Interface resolves on first use.\nlet Interface;"],
    ["    return new Interface({",
     "    Interface ??= require('internal/readline/interface').Interface; // lumen: see above\n    return new Interface({"],
  ],
};
let out = fs.readFileSync(path.join(G, 'head.js'), 'utf8');
for (const [id, file, label] of mods) {
  let src = fs.readFileSync(path.join(N, file), 'utf8');
  for (const [from, to] of patches[id] || []) {
    const n = src.split(from).length - 1;
    if (n !== 1) throw new Error(`patch for ${id} matched ${n} times: ${from}`);
    src = src.replace(from, to);
  }
  src = src.replace(/\s+$/, '');
  out += `// ---- lib/${label ? label.slice(4) : id + '.js'} (Node v20.10.0) ${'-'.repeat(Math.max(3, 70 - id.length))}\n`;
  out += `defineModule(${JSON.stringify(id)}, function (module, exports, require, internalBinding, primordials) {\n${src}\n});\n\n`;
}
out += fs.readFileSync(path.join(G, 'tail.js'), 'utf8').replace(/^\n/, '');
fs.writeFileSync(process.argv[2], out);
console.log('wrote', out.split('\n').length, 'lines');
