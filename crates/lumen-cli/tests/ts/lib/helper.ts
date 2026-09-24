// A module imported by the other fixtures: value and type exports side by side.
export interface Helper { name: string }
export type Shape = { kind: "sq"; size: number } | { kind: "circle"; r: number };
export const type = "type-export";
export const as = "as-export";
export const VERSION: string = "1.0";
export function area(s: Shape): number {
  switch (s.kind) {
    case "sq": return s.size ** 2;
    case "circle": return Math.round(Math.PI * s.r ** 2);
  }
}
