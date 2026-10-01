(async () => {
  const out = {};
  out.buffer = Buffer.from('héllo').toString('hex') + ' ' + Buffer.from('aGk=', 'base64').toString();

  const crypto = require('node:crypto');
  out.sha256 = crypto.createHash('sha256').update('abc').digest('hex');
  out.hmac = crypto.createHmac('sha1', 'key').update('data').digest('hex');
  out.random = crypto.randomBytes(16).length;

  const zlib = require('node:zlib');
  const input = Buffer.from('lumen '.repeat(500));
  out.zlib = zlib.inflateSync(zlib.deflateSync(input)).equals(input);
  out.gzip = zlib.gunzipSync(zlib.gzipSync(input)).length;
  out.brotli = zlib.brotliDecompressSync(zlib.brotliCompressSync(input)).length;

  const fs = require('node:fs');
  out.syncRead = fs.readFileSync('/remote/hello.txt', 'utf8').trim();
  out.readdir = fs.readdirSync('/remote').sort().join(',');
  out.nested = fs.readFileSync('/remote/sub/note.txt', 'utf8').trim();
  fs.mkdirSync('/work/dir', { recursive: true });
  fs.writeFileSync('/work/dir/a.txt', 'in memory');
  out.memfs = fs.readFileSync('/work/dir/a.txt', 'utf8');
  out.async = await fs.promises.readFile('/work/dir/a.txt', 'utf8');
  try {
    fs.readFileSync('/remote/missing.txt');
  } catch (e) {
    out.missing = e.code;
  }

  const t0 = Date.now();
  await new Promise((resolve) => setTimeout(resolve, 30));
  out.timer = Date.now() - t0 >= 25;

  function* gen() { yield 1; yield 2; yield 3; }
  async function* agen() { yield 'a'; yield 'b'; }
  out.generator = [...gen()].join('');
  let s = '';
  for await (const x of agen()) s += x;
  out.asyncGenerator = s;

  const res = await fetch('data:text/plain;base64,aGVsbG8gZmV0Y2g=');
  out.fetch = await res.text();

  out.platform = process.platform + '/' + process.arch;
  console.log('RESULT ' + JSON.stringify(out));
})().catch((e) => console.log('FAILED ' + (e && e.stack || e)));
