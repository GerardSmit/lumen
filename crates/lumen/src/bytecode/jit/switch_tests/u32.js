var out = [];
function crcTable() {
  var c, t = [];
  for (var n = 0; n < 256; n++) {
    c = n;
    for (var k = 0; k < 8; k++) c = (c & 1) ? (0xEDB88320 ^ (c >>> 1)) : (c >>> 1);
    t[n] = c;
  }
  return t;
}
var table = crcTable();
function crc32(crc, buf) {
  var t = table;
  crc ^= -1;
  for (var i = 0; i < buf.length; i++) crc = (crc >>> 8) ^ t[(crc ^ buf[i]) & 0xFF];
  return crc ^ -1;
}
var data = new Uint8Array(300);
for (var i = 0; i < data.length; i++) data[i] = (i * 31 + 7) & 255;
out.push(crc32(0, data), crc32(0, data.subarray(0, 17)), crc32(123456, data));

function toU(x) { return x >>> 0; }
var probes = [0, 1, -1, -2, 2147483647, -2147483648, 2147483648, 4294967295, 4294967296, 4294967297,
  -4294967297, 1.5, -1.5, 3000000000.7, -3000000000.7, 0.5, -0.5, NaN, Infinity, -Infinity, 9007199254740993,
  1e21, -1e21, 123456789012345];
var r = [];
for (var p = 0; p < probes.length; p++) r.push(toU(probes[p]));
out.push(r.join());

function ushrVar(x, n) { return x >>> n; }
r = [];
for (var p = 0; p < probes.length; p++)
  for (var n = 0; n < 34; n += 5) r.push(ushrVar(probes[p], n));
out.push(r.join());

function cmp(x, y) {
  var a = x >>> 0, b = y >>> 0;
  return [a < b, a > b, a === b, a >= 2147483648, a > 2147483647, a == b, a !== b, a <= b].join('');
}
r = [];
for (var p = 0; p < probes.length; p++) r.push(cmp(probes[p], probes[(p + 3) % probes.length]));
out.push(r.join());

function arith(x, y) {
  var a = x >>> 0, b = y >>> 0;
  return [a + b, a - b, a * 2, a / 2, a % 7, (a + b) | 0, (a - b) | 0, (a + b) >>> 0, (a - b) >>> 0,
    a + 0.5, -a, ~a, a ^ b, a & b, a | b, a << 1, a >> 1, a >>> 1, a >> 31, a >>> 31, (a + b) ^ 5,
    (a + 1) & 0xFF, (a - 1) | 0, a + b + b, (a + b) - a].join();
}
r = [];
for (var p = 0; p < probes.length; p++) r.push(arith(probes[p], probes[(p + 5) % probes.length]));
out.push(r.join('|'));

function store(x) {
  var a = x >>> 0;
  var arr = [1, 2, 3];
  arr[1] = a;
  var u = new Uint32Array(2);
  u[0] = a;
  var i = new Int32Array(1);
  i[0] = a;
  var f = new Float64Array(1);
  f[0] = a;
  var o = { v: a };
  return [arr[1], u[0], i[0], f[0], o.v, String(a), a + '', [a].join(), JSON.stringify([a])].join();
}
r = [];
for (var p = 0; p < probes.length; p++) r.push(store(probes[p]));
out.push(r.join('|'));

function idx(x) {
  var a = x >>> 0;
  var arr = [];
  arr[a & 15] = a;
  var m = {};
  m[a] = 1;
  return [arr[a & 15], arr[(a >>> 28)], Object.keys(m)[0], data[a & 255], data[(a >>> 24) & 255], table[a >>> 24]].join();
}
r = [];
for (var p = 0; p < probes.length; p++) r.push(idx(probes[p]));
out.push(r.join('|'));

function loop(seed) {
  var h = seed >>> 0;
  for (var i = 0; i < 200; i++) {
    h = (h ^ (h << 13)) >>> 0;
    h = (h ^ (h >>> 17)) >>> 0;
    h = (h ^ (h << 5)) >>> 0;
  }
  return h;
}
out.push(loop(2463534242), loop(1), loop(-1), loop(4294967295));

function mixed(x, d) {
  var a = x >>> 0;
  var s = 0;
  for (var i = 0; i < 20; i++) {
    s += a;
    s -= d;
    a = (a + d) >>> 0;
  }
  return [s, a, a + d, (a * d) | 0, Math.imul(a, 31), Math.max(a, d), Math.min(a, -d), Math.floor(a / d)].join();
}
r = [];
for (var p = 0; p < probes.length; p++) r.push(mixed(probes[p], 3.25));
out.push(r.join('|'));

function ret(x) { return x >>> 0; }
function retXor(x, y) { return (x >>> 0) ^ y; }
function retAdd(x, y) { return (x >>> 0) + (y >>> 0); }
function retTernary(x) { return (x >>> 0) > 100 ? x >>> 0 : -(x >>> 0); }
r = [];
for (var p = 0; p < probes.length; p++) {
  r.push(ret(probes[p]), retXor(probes[p], 255), retAdd(probes[p], probes[(p + 1) % probes.length]), retTernary(probes[p]));
}
out.push(r.join());

function hold(bits) {
  var h = 0, n = 0, total = 0;
  for (var i = 0; i < 40; i++) {
    h += (data[i] << n) >>> 0;
    n += 8;
    if (n >= 24) {
      total += h & 0xFFFF;
      h >>>= 16;
      n -= 16;
    }
  }
  return total + ',' + h + ',' + n;
}
out.push(hold(1));

function consts(x) {
  return [(x >>> 0) & 0xFFFFFFFF, (x >>> 0) | 0, 3988292384 ^ x, (x ^ 3988292384) >>> 0, (x & 4294967295) >>> 0,
    x | 4294967296, x ^ 9007199254740992, (x >>> 0) + 4294967295 | 0].join();
}
r = [];
for (var p = 0; p < probes.length; p++) r.push(consts(probes[p]));
out.push(r.join('|'));
out.join('\n');
