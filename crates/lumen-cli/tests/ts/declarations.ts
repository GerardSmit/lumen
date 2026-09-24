// Type-only declarations, overloads and imports/exports that erase completely.
import type { Helper } from "./lib/helper.ts";
import { type Shape, area, type as as, VERSION } from "./lib/helper.ts";
import * as lib from "./lib/helper.ts";

declare global {
  interface Window { lumen: boolean }
}
declare module "virtual" {
  export const v: number;
}
declare namespace Ambient {
  const x: number;
  function f(): void;
}
namespace Types {
  export type Id = string;
  export interface Named { name: Id }
  namespace Inner { type Deep = 1 }
}
declare const env: { mode: string };
declare function external(x: number): string;
declare class Outside { m(): void }
declare enum Colors { Red }
declare let later: number;
declare var old: string;

type Point = { x: number; y: number };
type Fn<T> = (arg: T) => T;
type Cond<T> = T extends string ? "s" : T extends (infer U)[] ? U : never;
type Mapped = { readonly [K in keyof Point]?: Point[K] };
type Tpl = `p-${string}`;
interface Box<T> extends Types.Named { value: T; map<U>(f: (v: T) => U): Box<U> }

function over(x: number): number;
function over(x: string): string;
function over(x: any): any {
  return typeof x === "number" ? x * 2 : x + x;
}

export function useHelper(h: Helper, s: Shape): string {
  return `${h.name}:${area(s)}`;
}
export type { Point, Fn };
export { type Mapped, over };

const named: Types.Named = { name: "n" };
const box: Box<number> | null = null;
const t: Tpl = "p-1";
const c: Cond<string[]> = "x" as never;
console.log(over(2), over("ab"), named.name, box, t, typeof c);
console.log(useHelper({ name: "h" }, { kind: "sq", size: 3 }), as, VERSION, Object.keys(lib).sort().join());
