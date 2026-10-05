// Regenerates src/entities/table.rs from entities.json (the WHATWG named character references)
// and xhtml.txt (the XHTML 1.0 subset that JSX accepts). `node generate-entities.mjs --verify`
// checks that the checked-in table is current.
import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const root = dirname(fileURLToPath(import.meta.url));
const source = JSON.parse(readFileSync(join(root, 'entities.json'), 'utf8'));
const xhtml = new Set(readFileSync(join(root, 'xhtml.txt'), 'utf8').split(/\s+/).filter(Boolean));
const entries = Object.entries(source).map(([name, value]) => [name.slice(1), value.characters]);
entries.sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0);
for (const name of xhtml) {
  if (!entries.some(([entry]) => entry === `${name};`)) throw Error(`xhtml.txt: unknown entity ${name}`);
}
const rust = value => `"${[...value].map(char => {
  const code = char.codePointAt(0);
  if (char === '"' || char === '\\') return `\\${char}`;
  return code >= 32 && code < 127 ? char : `\\u{${code.toString(16)}}`;
}).join('')}"`;
let names = '', values = '';
const rows = entries.map(([name, value]) => {
  const flags = name.endsWith(';') && xhtml.has(name.slice(0, -1)) ? 1 : 0;
  const row = [Buffer.byteLength(names), name.length, Buffer.byteLength(values), Buffer.byteLength(value), flags];
  names += name;
  values += value;
  return row;
});
if (Buffer.byteLength(names) > 65535 || Buffer.byteLength(values) > 65535) {
  throw Error('entity blob exceeds u16 offsets');
}
const output = `// Generated from https://html.spec.whatwg.org/entities.json. Run node generate-entities.mjs --verify.\n\
// Entry: (name offset, name length, value offset, value length, flags); flag 1 marks the XHTML subset.\n\
pub const NAMES: &str = ${rust(names)};\n\
pub const VALUES: &str = ${rust(values)};\n\
pub const ENTRIES: &[(u16, u8, u16, u8, u8)] = &[\n${rows.map(row => `    (${row.join(', ')}),`).join('\n')}\n];\n`;
const target = join(root, '..', 'src', 'entities', 'table.rs');
if (process.argv.includes('--verify')) {
  if (readFileSync(target, 'utf8') !== output) {
    throw Error('entities/table.rs is out of date');
  }
} else {
  writeFileSync(target, output);
}
