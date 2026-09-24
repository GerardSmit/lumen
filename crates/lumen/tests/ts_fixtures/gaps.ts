// Constructs typescript_strip.js mishandles today (the Rust port reproduces them byte for byte;
// see the crate report). None of these is valid JavaScript after stripping.
export class Typed {
  x: number;
  readonly tag: string = "t";
}
export function returnsFn(k: number): () => number {
  return () => k;
}
export function bang(v: string | null): string {
  return v!;
}
export function generic<T>(x: T): T {
  return x;
}
export function dispatch(e: { type: string }): number {
  if (e.type === "x") return 1;
  return 0;
}
import { a as b } from "./other";
