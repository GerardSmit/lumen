export class Vec {
  x: number;
  y: number;
  readonly tag: string = "v";
  constructor(x: number, y: number) {
    this.x = x;
    this.y = y;
  }
  dot(o: Vec): number {
    return this.x * o.x + this.y * o.y;
  }
  scale(k: number): Vec {
    return new Vec(this.x * k, this.y * k);
  }
}

export function norm(v: Vec): number {
  return Math.sqrt(v.dot(v));
}

export function sum(a: number[]): number {
  let s = 0;
  for (let i = 0; i < a.length; i++) {
    s += a[i];
  }
  return s;
}

function bad(x: any): number {
  return x.y;
}

const twice = (n: number): number => n * 2;

export function first(xs: string[] | null): string {
  if (xs === null) return "";
  const v = xs[0];
  return v ?? "";
}

async function load(path: string): Promise<string> {
  return path;
}

function describe(v: number | string): string {
  if (typeof v === "number") {
    return v.toFixed(2);
  }
  return v;
}
