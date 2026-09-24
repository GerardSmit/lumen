// Functions of every kind, with erasable annotations the location-preserving strip blanks.
interface Point { x: number; y: number }
type Id = number | string;
type Thunk = () => number;

export function add(a: number, b: number): number {
  return a + b;
}

export async function later(ms: number): Promise<number> {
  return ms;
}

export function* count(n: number): Generator<number> {
  for (let i: number = 0; i < n; i++) yield i;
}

export const double = (n: number): number => n * 2;
export const asyncArrow = async (s: string): Promise<string> => s;
export const single = x => x;
export const expr = function named(p: Point): number { return p.x + p.y; };

export class Shape {
  constructor(name: string) {
    this.name = name;
  }
  area(): number {
    return 0;
  }
  get label(): string {
    return this.name;
  }
  set label(v: string) {
    this.name = v;
  }
  static make(name: string): Shape {
    return new Shape(name);
  }
  async load(id: Id): Promise<Id> {
    return id;
  }
  *items(): Generator<number> {
    yield 1;
  }
  static async *stream(): AsyncGenerator<number> {
    yield 2;
  }
}

export class Circle extends Shape {
  constructor(r: number) {
    super("circle");
    this.r = r;
  }
  area(): number {
    return Math.PI * this.r * this.r;
  }
}

export class Empty {}

export const api = {
  ping(): string { return "pong"; },
  get version(): number { return 1; },
  handler: (e: Point): number => e.x,
  async fetch(u: string): Promise<string> { return u; },
};

export function outer(k: number): Thunk {
  const inner = (): number => k;
  return inner;
}

export function withDefault(a: number, b: number = 2, c?: number): number {
  return a + b + (c ?? 0);
}

export function rest(...xs: number[]): number {
  return xs.length;
}

export function cast(v: unknown): number {
  return (v as number) + 1;
}

export function nonNull(v: string | null): string {
  return v as string;
}

export function /* comment */ spaced  (a: number)  : number { return a; }
