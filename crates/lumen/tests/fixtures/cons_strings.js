function assert(x, m) { if (!x) throw new Error('assertion: ' + (m || '')); }
function eq(a, b, m) { if (a !== b) throw new Error((m || '') + ': ' + String(a) + ' !== ' + String(b)); }
// Defeats the compiler's constant folding / fused append paths: operands come from an array.
const hold = [];
function cat(a, b) { return a + b; }

(function deepLeftChain() {
  let s = 'x'.repeat(300);
  const piece = 'ab';
  for (let i = 0; i < 1000000; i++) { s = cat(s, piece); hold[0] = s; }
  hold.length = 0;
  eq(s.length, 300 + 2000000, 'length');
  eq(s.charAt(0), 'x');
  eq(s.charAt(300), 'a');
  eq(s.charCodeAt(301), 98);
  eq(s.charAt(s.length - 1), 'b');
  eq(s.indexOf('abab'), 300);
  eq(s.lastIndexOf('xa'), 299);
  eq(s.slice(298, 304), 'xxabab');
  eq(s.endsWith('abab'), true);
  eq(s.startsWith('xxx'), true);
  eq(JSON.stringify(s).length, s.length + 2);
  const o = {};
  o[s] = 1;
  eq(o[s], 1);
  eq(Object.keys(o)[0].length, s.length);
  eq(/x{300}(ab)+$/.test(s), true);
  eq(s.replace(/ab/g, '').length, 300);
  eq(s.split('x').length, 301);
})();

(function deepRightChain() {
  let s = 'y'.repeat(300);
  for (let i = 0; i < 300000; i++) { s = cat('c', s); hold[0] = s; }
  hold.length = 0;
  eq(s.length, 300300);
  eq(s.charAt(0), 'c');
  eq(s.charAt(300000), 'y');
  eq(s.indexOf('y'), 300000);
})();

(function treeShapes() {
  const a = 'a'.repeat(200), b = 'b'.repeat(200);
  const ab = cat(a, b);
  const abab = cat(ab, ab);
  const t = cat(abab, abab);
  eq(t.length, 1600);
  eq(t.indexOf('ba'), 399);
  eq(t.lastIndexOf('ab'), 1399);
  eq(t.charAt(1599), 'b');
  eq(abab.length, 800);
  eq(abab.slice(398, 402), 'bbaa');
  eq(t === cat(cat(a, b), cat(a, b)) === true, false);
  eq(t === cat(abab, abab), true);
  eq(t < cat(t, 'c'), true);
  eq(cat(t, 'a') > t, true);
  const m = new Map();
  m.set(t, 1);
  eq(m.get(cat(abab, abab)), 1);
  eq(new Set([t, cat(abab, abab)]).size, 1);
  eq(parseInt(cat('12'.repeat(200), '')), parseInt('12'.repeat(200)));
  eq(encodeURIComponent(cat(a, ' ')).length, 203);
  eq(cat(a, b).toUpperCase().charAt(0), 'A');
  eq(String(t).length, 1600);
  eq((t + 1).length, 1601);
  eq(`${t}!`.length, 1601);
  eq([t, t].join('|').length, 3201);
  eq(t.concat('z').length, 1601);
  eq(t.padEnd(1700, '-').length, 1700);
  eq(t.at(-1), 'b');
  eq(t[0], 'a');
  eq(Array.from(cat(a, b)).length, 400);
  eq([...cat(a, b)].length, 400);
})();

(function sharedNodes() {
  const base = cat('p'.repeat(200), 'q'.repeat(200));
  const x = cat(base, 'X');
  const y = cat(base, 'Y');
  eq(x.charAt(400), 'X');
  eq(y.charAt(400), 'Y');
  eq(base.length, 400);
  eq(x.slice(0, 400) === base, true);
  const z = cat(x, y);
  eq(z.length, 802);
  eq(z.indexOf('XP'), -1);
  eq(z.indexOf('Xp'), 400);
  eq(z.lastIndexOf('Y'), 801);
})();

(function surrogateSeams() {
  const pad = 'p'.repeat(300);
  const hi = '\uD83D', lo = '\uDE00';
  const left = cat(pad, hi);
  const right = cat(lo, pad);
  const joined = cat(left, right);
  eq(joined.length, 602);
  eq(joined.codePointAt(300), 0x1F600);
  eq(joined.charCodeAt(300), 0xD83D);
  eq(joined.charCodeAt(301), 0xDE00);
  eq([...joined].length, 601);
  eq(joined, pad + '\u{1F600}' + pad);
  eq(joined === pad + '\u{1F600}' + pad, true);
  eq(JSON.stringify(joined), JSON.stringify(pad + '\u{1F600}' + pad));
  const lone = cat(left, 'x'.repeat(10));
  eq(lone.charCodeAt(300), 0xD83D);
  eq(lone.length, 311);
  eq(lone.isWellFormed(), false);
  const nested = cat(cat(pad, hi), cat(lo, cat(pad, hi)));
  eq(nested.length, 603);
  eq(nested.codePointAt(300), 0x1F600);
  eq(nested.charCodeAt(602), 0xD83D);
  const reversedPair = cat(cat(pad, lo), cat(hi, pad));
  eq(reversedPair.charCodeAt(300), 0xDE00);
  eq(reversedPair.charCodeAt(301), 0xD83D);
  eq(reversedPair.isWellFormed(), false);
  eq(cat(pad, hi + lo).length, 302);
  eq(cat(pad, hi + lo).codePointAt(300), 0x1F600);
  eq(`${pad}${hi}${lo}${pad}`.length, 602);
  eq(`${pad}${hi}${lo}${pad}`.codePointAt(300), 0x1F600);
  const t = pad + hi;
  const u = t + lo + pad;
  eq(u.codePointAt(300), 0x1F600);
})();

(function withViews() {
  const big = Array.from({ length: 400 }, function (_, i) { return 'line number ' + String(i).padStart(4, '0') + ' of the long list'; }).join('\n');
  const lines = big.split('\n');
  const v = big.slice(100, 400);
  const joined = cat(v, lines[7]);
  eq(joined.length, 300 + lines[7].length);
  eq(joined.slice(300), lines[7]);
  eq(joined.slice(0, 300), v);
  const w = cat(lines[3], big.slice(50, 500));
  eq(w.length, lines[3].length + 450);
  eq(w.indexOf('line number 0003'), 0);
  const back = cat(w, lines[9]);
  eq(back.slice(-lines[9].length), lines[9]);
  const sub = back.slice(5, 400);
  eq(sub.length, 395);
  eq(sub, big.slice(0, 0) + back.substring(5, 400));
  eq(cat(big, big).length, 2 * big.length);
  eq(cat(big, big).slice(big.length - 5, big.length + 5), big.slice(-5) + big.slice(0, 5));
})();

(function inLoopsAndKeys() {
  let total = 0;
  for (let i = 0; i < 2000; i++) {
    const s = cat('k'.repeat(256), String(i));
    total += s.length + s.charCodeAt(256);
    const o = {};
    o[s] = i;
    if (o['k'.repeat(256) + i] !== i) throw new Error('key mismatch ' + i);
  }
  eq(total, 620885);
})();

(function hotLoops() {
  const a = 'a'.repeat(300), b = 'b'.repeat(300);
  const s = cat(a, b);
  let h = 0;
  for (let i = 0; i < 600; i++) h = (h * 31 + s.charCodeAt(i)) | 0;
  for (let r = 0; r < 5000; r++) {
    const t = cat(a, b);
    h = (h * 31 + t.charCodeAt(r % 600) + t.length + (t === s ? 1 : 0) + (t[r % 600] === 'a' ? 2 : 0)) | 0;
  }
  eq(h, -720689088);
})();
