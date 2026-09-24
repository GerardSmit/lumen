// Records Node's behaviour for the TypeScript program corpus (tests/typescript.rs):
//   node crates/lumen-cli/tests/ts/gen.mjs
// - every entry `X.ts` / `X.mts` / `X.cts` here: its stdout, in `X.out`;
// - every entry in `reject/`: Node's error code and message, in `X.err` (`CODE message`).
// Node >= 22.18 / 23.6 runs TypeScript with type stripping on by default.
import { readdirSync, writeFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const here = dirname(fileURLToPath(import.meta.url));
const entry = (name) => /\.(ts|mts|cts)$/.test(name);
const run = (file) =>
  spawnSync(process.execPath, ["--no-warnings", file], { cwd: dirname(file), encoding: "utf8" });

for (const name of readdirSync(here).filter(entry).sort()) {
  const r = run(join(here, name));
  if (r.status !== 0) throw new Error(`${name} failed under node:\n${r.stderr}`);
  writeFileSync(join(here, name.replace(/\.[^.]+$/, ".out")), r.stdout);
  console.log(`${name}: ok`);
}
const rejects = join(here, "reject");
for (const name of readdirSync(rejects).filter(entry).sort()) {
  const r = run(join(rejects, name));
  const m = /SyntaxError \[(ERR_[A-Z_]+)\]: (.*)/.exec(r.stderr);
  if (r.status === 0 || !m) throw new Error(`${name} did not fail with a TypeScript error:\n${r.stderr}`);
  // Parse errors are reported with the parser's own wording; only the code is compared.
  const message = m[1] === "ERR_INVALID_TYPESCRIPT_SYNTAX" ? "" : m[2];
  writeFileSync(join(rejects, name.replace(/\.[^.]+$/, ".err")), `${m[1]} ${message}`.trim() + "\n");
  console.log(`reject/${name}: ${m[1]}`);
}
