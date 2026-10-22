function assert(x, m) { if (!x) throw new Error('assertion: ' + (m || '')); }
function eq(a, b, m) { if (a !== b) throw new Error((m || '') + ': ' + String(a) + ' !== ' + String(b)); }
function forIn(o) { const r = []; for (const k in o) r.push(k); return r.join(); }

(function identity() {
  const o = { alpha: 1, beta: 2 };
  const k = 'al' + 'pha';
  eq(o[k], 1, 'computed read of static key');
  o[k] = 5;
  eq(o.alpha, 5, 'computed write seen by static read');
  eq(Object.keys(o).join(), 'alpha,beta');
  const j = JSON.parse('{"alpha":7,"beta":8}');
  eq(j[k], 7, 'JSON key read by computed');
  j.alpha = 9;
  eq(j[k], 9);
  eq(forIn(j), 'alpha,beta');
  const fromKeys = Object.keys(j)[0];
  eq(fromKeys, k);
  eq(fromKeys === 'alpha', true);
  eq(o[fromKeys], 5, 'for-in/Object.keys key reads the same slot');
  const names = Object.getOwnPropertyNames(o);
  eq(names.join(), 'alpha,beta');
  eq(Object.entries(o)[1][0], 'beta');
  eq(Reflect.ownKeys(o).join(), 'alpha,beta');
  eq(Reflect.get(o, k), 5);
  Reflect.set(o, k, 6);
  eq(o.alpha, 6);
  eq(Reflect.has(o, k), true);
  eq(k in o, true);
  eq(o.hasOwnProperty(k), true);
  eq(delete o[k], true);
  eq('alpha' in o, false);
  eq(Object.keys(o).join(), 'beta');
})();

(function shapesConverge() {
  const a = {}; a['x' + ''] = 1; a['y'] = 2;
  const b = { x: 1, y: 2 };
  const c = JSON.parse('{"x":1,"y":2}');
  eq(JSON.stringify(a), JSON.stringify(b));
  eq(JSON.stringify(c), JSON.stringify(b));
  const d = Object.fromEntries([['x', 1], ['y', 2]]);
  eq(JSON.stringify(d), JSON.stringify(b));
  for (const o of [a, b, c, d]) { eq(o.x, 1); eq(o['y'], 2); eq(o[['x'].join('')], 1); }
})();

(function symbols() {
  const s = Symbol('alpha');
  const o = { alpha: 1, [s]: 2 };
  eq(o[s], 2);
  eq(o['alpha'], 1);
  eq(Object.keys(o).join(), 'alpha');
  eq(Object.getOwnPropertySymbols(o).length, 1);
  eq(forIn(o), 'alpha');
  eq(JSON.stringify(o), '{"alpha":1}');
  const w = { [Symbol.toPrimitive]() { return s; } };
  eq(o[w], 2);
  const t = Symbol('alpha');
  eq(o[t], undefined);
  eq('\u0000' in o, false);
  eq(o[s], 2);
})();

(function numericLike() {
  const o = {};
  const keys = ['1', '01', '-0', '4294967295', '4294967294', '1e3', '0', ' 1', '1.5', '-1', 'NaN', 'Infinity'];
  for (let i = 0; i < keys.length; i++) o[keys[i]] = i;
  eq(Object.keys(o).join(), '0,1,4294967294,01,-0,4294967295,1e3, 1,1.5,-1,NaN,Infinity');
  eq(forIn(o), '0,1,4294967294,01,-0,4294967295,1e3, 1,1.5,-1,NaN,Infinity');
  for (let i = 0; i < keys.length; i++) eq(o[keys[i]], i, keys[i]);
  eq(o[1], 0);
  eq(o[-0], 6);
  eq(o['-0'], 2);
  eq(o[4294967295], 3);
  eq(o[1e3], undefined);
  eq(JSON.parse('{"1":1,"01":2,"b":3,"0":4}')['01'], 2);
  eq(Object.keys(JSON.parse('{"b":3,"1":1,"01":2,"0":4}')).join(), '0,1,b,01');
  const a = [];
  a['2'] = 'x'; a['01'] = 'y';
  eq(a.length, 3);
  eq(Object.keys(a).join(), '2,01');
})();

(function ordering() {
  const o = {};
  for (const k of ['b', 'a', 'c']) o[k] = k;
  eq(Object.keys(o).join(), 'b,a,c');
  delete o.a;
  o['a'] = 'again';
  eq(Object.keys(o).join(), 'b,c,a');
  eq(forIn(o), 'b,c,a');
  eq(JSON.stringify(o), '{"b":"b","c":"c","a":"again"}');
  const p = Object.create({ inherited: 1, b: 2 });
  p.own = 1; p.b = 3;
  eq(forIn(p), 'own,b,inherited');
  const keysAfterDelete = [];
  const q = { x: 1, y: 2, z: 3 };
  for (const k in q) { keysAfterDelete.push(k); delete q.y; }
  eq(keysAfterDelete.join(), 'x,z');
})();

(function deleteReadd() {
  for (let round = 0; round < 3; round++) {
    const o = { p: 1, q: 2, r: 3 };
    delete o.q;
    eq('q' in o, false);
    o['q'] = 4;
    eq(Object.keys(o).join(), 'p,r,q');
    eq(o.q, 4);
    delete o['p']; delete o['r']; delete o.q;
    eq(Object.keys(o).length, 0);
    o.p = 9;
    eq(o['p'], 9);
  }
})();

(function proxies() {
  const log = [];
  const target = { a: 1, b: 2 };
  const p = new Proxy(target, {
    ownKeys(t) { log.push('ownKeys'); return ['b', 'a', 'extra']; },
    getOwnPropertyDescriptor(t, k) { log.push('gopd:' + k); return k === 'extra' ? { value: 0, enumerable: true, configurable: true } : Reflect.getOwnPropertyDescriptor(t, k); },
    get(t, k) { return k === 'extra' ? 0 : t[k]; },
  });
  eq(Reflect.ownKeys(p).join(), 'b,a,extra');
  eq(Object.keys(p).join(), 'b,a,extra');
  eq(forIn(new Proxy({ a: 1, b: 2 }, { ownKeys() { return ['b', 'a']; } })), 'b,a');
  const key = 'e' + 'xtra';
  eq(p[key], 0);
  eq(Object.entries(p).length, 3);
  eq(Object.getOwnPropertyNames(p).join(), 'b,a,extra');
})();

(function longKeys() {
  const long = 'k'.repeat(100000);
  const o = {};
  o[long] = 1;
  o['k'.repeat(99999) + 'k'] = 2;
  eq(Object.keys(o).length, 1);
  eq(o[long], 2);
  eq(Object.keys(o)[0].length, 100000);
  const j = JSON.parse('{"' + long + '":3}');
  eq(j[long], 3);
  eq(forIn(j).length, 100000);
  const mid = 'm'.repeat(300);
  const m = {};
  m[mid + '1'] = 1; m[mid + '2'] = 2;
  eq(Object.keys(m).map(function (x) { return x.slice(-1); }).join(), '1,2');
})();

(function churnAndDictionary() {
  const keep = {};
  for (let i = 0; i < 40; i++) keep['keep' + i] = i;
  for (let round = 0; round < 3; round++) {
    for (let i = 0; i < 60000; i++) {
      const t = {}; t['tmp' + round + '_' + i] = i;
      eq(t['tmp' + round + '_' + i], i);
    }
  }
  const dict = {};
  for (let i = 0; i < 5000; i++) dict['d' + i] = i;
  for (let i = 0; i < 5000; i += 7) delete dict['d' + i];
  for (let i = 0; i < 5000; i++) eq(dict['d' + i], i % 7 === 0 ? undefined : i);
  for (let i = 0; i < 40; i++) eq(keep['keep' + i], i);
  const arr = JSON.parse('[' + Array.from({ length: 2000 }, function (_, i) { return '{"id":' + i + ',"name":"n' + i + '","w":' + i * 2 + '}'; }).join() + ']');
  let sum = 0;
  for (const o of arr) for (const k in o) sum += k.length;
  eq(sum, 2000 * (2 + 4 + 1));
  eq(arr[1999]['na' + 'me'], 'n1999');
  const keys = Object.keys(arr[5]);
  eq(arr[5][keys[1]], 'n5');
  eq(Object.keys(keep).length, 40);
})();

(function keysAreIndependentStrings() {
  const o = { alpha: 1 };
  let k = Object.keys(o)[0];
  k += 'x';
  eq(k, 'alphax');
  eq(Object.keys(o)[0], 'alpha');
  for (const key in o) { let s = key; s += '!'; eq(s, 'alpha!'); }
  eq(Object.keys(o)[0], 'alpha');
  const first = Object.keys(o)[0];
  const parts = [];
  for (let i = 0; i < 3; i++) parts.push(first + i);
  eq(parts.join(), 'alpha0,alpha1,alpha2');
  eq(Object.keys(o).join(), 'alpha');
})();

(function reflectField() {
  function field(o, k) { return o[k]; }
  function setField(o, k, v) { o[k] = v; }
  const o = { name: 'a', hp: 3 };
  let n = 0;
  for (let i = 0; i < 2000; i++) { setField(o, 'hp', i); n += field(o, 'hp'); setField(o, 'mp' + (i % 3), i); }
  eq(n, 1999000);
  eq(Object.keys(o).join(), 'name,hp,mp0,mp1,mp2');
  eq(o.mp1, 1999);
})();
