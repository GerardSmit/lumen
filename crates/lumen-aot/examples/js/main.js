// entry module. MARKER_GAMMA_MAIN_COMMENT_5520
import { mul, twice } from "./lib/math.js";
import { greet } from "host:greeting"; // bare specifier: the host loader supplies it

export const BASE = 10;

export function run() {
    const a = mul(6, 7);          // 42 + BASE (math imports BASE back from here)
    const b = twice(prelude.scale(2)); // 14
    return a + b;
}

globalThis.out = {
    sum: run(),
    greeting: greet("aot"),
    mulSrc: mul.toString(),
    classSrc: (class Widget { go() {} }).toString(),
    metaUrl: import.meta.url,
};
globalThis.dyn = import("./lib/extra.js").then((m) => m.extra(5));
