#!/usr/bin/env node
// A CommonJS TypeScript entry with a shebang: `#!` is neutralized in place, so offsets hold.
const value: number = require("./lib/value.ts");
const cjs = require("./lib/common.cts") as { twice(n: number): number };
function show(label: string, n: number): string { return `${label}=${n}`; }
console.log(show("value", value), cjs.twice(value));
console.log(JSON.stringify(show.toString()));
console.log(typeof module, typeof exports, __filename.endsWith("shebang.cts"));
