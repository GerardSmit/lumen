const p = require('path');
const cases = [
  ['resolve', ['C:\\y', 'sub'], 'C:\\y\\sub'],
  ['resolve', ['C:\\a\\b', '..\\c'], 'C:\\a\\c'],
  ['resolve', ['C:\\a', 'D:\\b', 'c'], 'D:\\b\\c'],
  ['resolve', ['\\\\srv\\share\\x', '..\\..\\y'], '\\\\srv\\share\\y'],
  ['resolve', ['C:\\a', '/b'], 'C:\\b'],
  ['normalize', ['C:/a//b/../c/'], 'C:\\a\\c\\'],
  ['normalize', ['C:'], 'C:.'],
  ['normalize', ['a/../../b'], '..\\b'],
  ['dirname', ['C:\\foo'], 'C:\\'],
  ['dirname', ['C:\\foo\\bar.txt'], 'C:\\foo'],
  ['dirname', ['C:\\'], 'C:\\'],
  ['dirname', ['foo'], '.'],
  ['dirname', ['\\\\srv\\share\\x'], '\\\\srv\\share\\'],
  ['basename', ['C:\\foo\\bar.txt', '.txt'], 'bar'],
  ['basename', ['C:\\foo\\'], 'foo'],
  ['isAbsolute', ['C:\\x'], true],
  ['isAbsolute', ['C:x'], false],
  ['isAbsolute', ['\\x'], true],
  ['isAbsolute', ['x'], false],
  ['relative', ['C:\\a\\b', 'C:\\a\\c\\d'], '..\\c\\d'],
  ['relative', ['C:\\a', 'D:\\b'], 'D:\\b'],
  ['relative', ['C:\\A\\b', 'c:\\a\\b\\c'], 'c'],
  ['join', ['C:\\a', '..', 'b'], 'C:\\b'],
  ['join', ['a', 'b'], 'a\\b'],
  ['toNamespacedPath', ['C:\\a'], '\\\\?\\C:\\a'],
  ['toNamespacedPath', ['\\\\srv\\share\\x'], '\\\\?\\UNC\\srv\\share\\x'],
];
let bad = 0;
for (const [f, a, want] of cases) {
  const got = p.win32[f](...a);
  if (got !== want) {
    bad++;
    console.log('FAIL', f, JSON.stringify(a), '->', JSON.stringify(got), 'want', JSON.stringify(want));
  }
}
const parsed = p.win32.parse('C:\\dir\\file.txt');
const want = { root: 'C:\\', dir: 'C:\\dir', base: 'file.txt', ext: '.txt', name: 'file' };
if (JSON.stringify(parsed) !== JSON.stringify(want)) { bad++; console.log('FAIL parse', JSON.stringify(parsed)); }
const parsedRoot = p.win32.parse('C:\\file.txt');
if (parsedRoot.dir !== 'C:\\') { bad++; console.log('FAIL parse root', JSON.stringify(parsedRoot)); }
console.log(process.platform, bad ? bad + ' failed' : 'all ok');
