// Plain JavaScript that a text-based stripper gets wrong; none of it may change.
var type = 1, as = 2, satisfies = 3, declare = 4, abstract = 5, namespace = 6, module_ = 7;
var interface_ = { type: "t", enum: "e", namespace: "n", as: "a", readonly: "r" };
type
= 10;
declare
; // an expression statement `declare;`
const ratio = type / as / satisfies;
const re = /[/]<T>(x: number)/g.source;
const re2 = "a:b:c".replace(/:/g, "/");
const cond = type ? (as) : (satisfies);
const arrowInTernary = type ? (x) => x + 1 : (x) => x - 1;
const nested = type ? as ? "a" : "b" : "c";
const str = "let x: number = <T>y as any; enum E {}";
const tpl = `type ${type} = ${as}; interface ${"I"} {}`;
const obj = { type, as, satisfies, "key: value": 1, get enum() { return "getter"; } };
label: for (const k of [1, 2]) { if (k) continue label; }
const lt = type < as, gt = satisfies > declare, both = type < as > false;
function generic(a, b) { return a < b; }
const call = generic(type < as, satisfies > declare);
const cls = class { static type = "static-type"; declare = "field-declare"; abstract() { return "m"; } };
console.log(type, as, satisfies, declare, ratio.toFixed(3), re, re2, cond, arrowInTernary(1), nested);
console.log(str, tpl, JSON.stringify(obj), lt, gt, both, call, cls.type, new cls().declare, new cls().abstract());
console.log(interface_.enum, abstract + namespace + module_);
