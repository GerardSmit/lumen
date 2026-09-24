// Regenerates the golden files for tests/typescript_fixtures.rs from Node's own type stripper
// (`module.stripTypeScriptTypes`, amaro/swc), which lumen's `strip_types` reproduces:
//   node crates/lumen/tests/ts_gen_goldens.mjs
//
// For every fixture `X.ts` / `X.js` in tests/ts_fixtures:
// - `X.strip.golden` : Node's strip-only output, or `ERROR <code> <message>` (the message's
//   first line) when it rejects.
// - `X.offsets.json`: when the stripped text runs as an ES module, the `[start, end)` byte
//   offsets of every reachable function's `Function.prototype.toString()` text in the stripped
//   source (the engine's FnSource::Range). Functions are reached through the module's exports:
//   exported functions and classes (constructor = the class text, static and prototype methods
//   and accessors), and functions inside exported plain objects/arrays (one level deep).
import { readFileSync, writeFileSync, readdirSync, mkdtempSync, rmSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { tmpdir } from "node:os";
import { stripTypeScriptTypes } from "node:module";

const here = dirname(fileURLToPath(import.meta.url));
const fixtures = join(here, "ts_fixtures");
process.removeAllListeners("warning");
const strip = (src) => stripTypeScriptTypes(src, { mode: "strip" });

const tmp = mkdtempSync(join(tmpdir(), "lumen-ts-golden-"));
try {
  for (const name of readdirSync(fixtures).sort()) {
    if (!/\.(ts|js|mts|mjs)$/.test(name) || name.endsWith(".strip.js")) continue;
    const base = name.replace(/\.[^.]+$/, "");
    const src = readFileSync(join(fixtures, name), "utf8");
    let stripped;
    try {
      stripped = strip(src);
    } catch (e) {
      writeFileSync(join(fixtures, `${base}.strip.golden`), `ERROR ${e.code ?? "SyntaxError"} ${e.message.split("\n")[0]}\n`);
      console.log(`${name}: rejected (${e.message.split("\n")[0]})`);
      continue;
    }
    writeFileSync(join(fixtures, `${base}.strip.golden`), stripped);
    const file = join(tmp, `${base}.mjs`);
    writeFileSync(file, stripped);
    let mod;
    try {
      mod = await import(pathToFileURL(file).href);
    } catch (e) {
      console.log(`${name}: stripped text does not run as a module (${e.message.split("\n")[0]}); no offsets`);
      continue;
    }
    const bytes = Buffer.from(stripped, "utf8");
    const out = [];
    const seen = new Set();
    const record = (path, fn) => {
      if (typeof fn !== "function" || seen.has(fn)) return;
      seen.add(fn);
      const text = Function.prototype.toString.call(fn);
      if (text.includes("[native code]")) return;
      const tb = Buffer.from(text, "utf8");
      const start = bytes.indexOf(tb);
      if (start < 0 || bytes.lastIndexOf(tb) !== start) {
        console.log(`${name}: ${path}: text not unique in the stripped source; skipped`);
        return;
      }
      out.push({ path, start, end: start + tb.length });
    };
    const walkClass = (path, cls) => {
      record(path, cls);
      for (const [k, d] of Object.entries(Object.getOwnPropertyDescriptors(cls))) {
        if (["length", "name", "prototype"].includes(k)) continue;
        record(`${path}.${k}`, d.value); record(`${path}.get ${k}`, d.get); record(`${path}.set ${k}`, d.set);
      }
      for (const [k, d] of Object.entries(Object.getOwnPropertyDescriptors(cls.prototype))) {
        if (k === "constructor") continue;
        record(`${path}#${k}`, d.value); record(`${path}#get ${k}`, d.get); record(`${path}#set ${k}`, d.set);
      }
    };
    for (const [k, v] of Object.entries(mod)) {
      if (typeof v === "function") {
        if (/^class\b/.test(Function.prototype.toString.call(v))) walkClass(k, v);
        else record(k, v);
      } else if (v && typeof v === "object") {
        for (const [k2, d] of Object.entries(Object.getOwnPropertyDescriptors(v))) {
          const f = d.value;
          if (typeof f === "function" && /^class\b/.test(Function.prototype.toString.call(f))) walkClass(`${k}.${k2}`, f);
          else record(`${k}.${k2}`, f);
          record(`${k}.get ${k2}`, d.get); record(`${k}.set ${k2}`, d.set);
        }
      }
    }
    out.sort((a, b) => a.start - b.start);
    writeFileSync(join(fixtures, `${base}.offsets.json`), JSON.stringify(out, null, 1) + "\n");
    console.log(`${name}: ${out.length} functions`);
  }
} finally {
  rmSync(tmp, { recursive: true, force: true });
}
