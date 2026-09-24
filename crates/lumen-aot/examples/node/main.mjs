// aot_node fixture: bare packages (CommonJS and ES), a subpath export, JSON through require,
// a literal dynamic import, and kept function text. Throws on any mismatch.
import cjs, { add } from 'cjs-pkg';
import esm, { twice } from 'esm-pkg';
import sub from 'esm-pkg/sub';
import { createRequire } from 'node:module';

const check = (what, got, want) => {
  if (JSON.stringify(got) !== JSON.stringify(want)) {
    throw new Error(`${what}: got ${JSON.stringify(got)}, want ${JSON.stringify(want)}`);
  }
  console.log('ok ', what, JSON.stringify(got));
};

check('cjs default', cjs.add(2, 3), 5);
check('cjs named', add(4, 5), 9);
check('cjs json via require', cjs.data, { answer: 42 });
check('cjs relative require', cjs.util, 'util');
check('cjs optional dependency', cjs.optional, 'missing');
check('cjs __dirname', cjs.dirname.startsWith('aot:/'), true);
check('esm default', esm, 'esm-pkg');
check('esm named', twice(21), 42);
check('esm subpath export', sub, 'sub');
const lazy = await import('./lib/lazy.js');
check('literal dynamic import', lazy.value, 'lazy');
check('kept toString', String((a, b) => a + b), '(a, b) => a + b');
check('stripped toString', String(add).includes('[native code]'), true);
const require = createRequire(import.meta.url);
check('require of a bundled package by name', require('cjs-pkg').add(1, 1), 2);
console.log('aot_node: all ok');
