// Expressions: generics, assertions, non-null, and the `<`/`>` ambiguities.
type Pair<A, B = A> = [first: A, second?: B];
type Fn = <T>(x: T) => T;

function id<T>(x: T): T {
  return x;
}
const ident: Fn = <T,>(x: T): T => x;
const asyncId = async <T,>(x: T): Promise<T> => x;
const make = <T extends unknown[]>(...xs: T): T => xs;

let a = 1, b = 2, c = 3;
const cmp1 = a < b > c; // (a < b) > c: `c` cannot follow type arguments
const ctor = (x: number) => x * 2;
const cmp2 = ctor < number > (c); // an instantiation call: ctor(c)
const call = id<number>(4);
const inst = id<string>;
const m = new Map<string, number[]>([["k", [1, 2]]]);
const nn = m.get("k")!.length!;
const sat = { x: 1 } satisfies { x: number };
const pair: Pair<number> = [1];
const opt = (m as Map<string, number[]> | undefined)?.get("k")?.[0]!;
const tagged = String.raw<unknown>`a${1}b` as string;
function f(x: number, y: number): boolean { return x < y; }
const g = (u: boolean, v: boolean) => [u, v];

console.log(cmp1, cmp2, call, inst("s"), nn, sat.x, pair.length, opt, tagged);
console.log(ident("i"), make(1, 2).length, g(a < b, c > a), f(1, 2));
asyncId(5).then((v: number) => console.log("async", v));

// Conditional expressions with arrow-looking branches (plain JavaScript).
const t = a ? (b) : (c);
const u = a ? (x: number): number => x + 1 : (x: number) => x - 1;
const v = a ? (b): number => b : c;
console.log(t, u(10), typeof v);

// Non-null and definite assignment.
let later!: string;
later = "set";
const el: { value?: { deep: number } } = { value: { deep: 3 } };
console.log(later, el.value!.deep, el!.value!["deep"]);

// Type-only syntax in odd places.
for (let i: number = 0, j: string = ""; i < 2; i++) j += i;
try { throw new Error("e"); } catch (e: unknown) { console.log((e as Error).message); }
const { p, q }: { p: number; q: string } = { p: 1, q: "q" };
console.log(p, q, ((x?: number, ...rest: number[]): number => (x ?? 0) + rest.length)(1, 2, 3));
function assertIsString(val: any): asserts val is string {}
function isNum(val: unknown): val is number { return typeof val === "number"; }
console.log(isNum(1), assertIsString("x"));
function thisParam(this: { k: number }, add: number) { return this.k + add; }
console.log(thisParam.call({ k: 1 }, 2));
