export type Shape = { kind: "sq"; size: number };
export interface Unused { x: number }
export function area(s: Shape): number {
  return s.size ** 2;
}
