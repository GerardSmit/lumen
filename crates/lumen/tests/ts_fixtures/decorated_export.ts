// Decorators before `export` (puppeteer's JSHandle.ts: `@moveable export abstract class`).
// Node (swc) blanks the `export` keyword of a decorated abstract class and keeps `abstract`;
// the engine's own blanked source keeps the valid form (`export`, `abstract` erased).
declare function moveable(c: any, ctx: any): void;
declare function sealed(c: any, ctx: any): void;

@moveable
export abstract class Handle<T = unknown> {
  declare move: () => this;
  declare _?: T;
  #n: number;
  constructor(n: number) {
    this.#n = n;
  }
  abstract dispose(): void;
}

@sealed @moveable
export class Plain<K extends string> implements Iterable<K> {
  *[Symbol.iterator](): Iterator<K> {}
}

@moveable export default abstract class Base {}

@sealed
export declare class Ambient {}
