// entry module. MARKER_GAMMA_MAIN_COMMENT_5520
import { mul, twice } from "./lib/math.js";
import { greet } from "host:greeting"; // bare specifier: the host loader supplies it

export const BASE = 10;

export function run() {
    const a = mul(6, 7);          // 42 + BASE (math imports BASE back from here)
    const b = twice(prelude.scale(2)); // 14
    return a + b;
}

// Bytecode-shaped work: a loop (compiles on its first call when loaded from source), closures
// over a captured local, constants of every kind, a class method and a refused generator.
export function sumTo(n) {
    let s = 0;
    for (let i = 1; i <= n; i++) s += i % 7 === 0 ? i * 2 : i;
    return s;
}
function counters() {
    let c = 0;
    const inc = () => ++c;
    function peek() { return c; }
    inc(); inc();
    return [inc(), peek(), typeof inc, 10n ** 3n, -0 === 0, null ?? "dflt", 1.5e-7].join(",");
}
class Acc { constructor() { this.v = 1; } add(x) { this.v += x; return this; } }
function* gen() { yield 1; yield 2; }

globalThis.out = {
    sum: run(),
    greeting: greet("aot"),
    mulSrc: mul.toString(),
    classSrc: (class Widget { go() {} }).toString(),
    metaUrl: import.meta.url,
    loop: sumTo(1000),
    closures: counters(),
    acc: new Acc().add(2).add(3).v,
    gen: [...gen()].join("+"),
};
globalThis.dyn = import("./lib/extra.js").then((m) => m.extra(5));
