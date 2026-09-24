// Entry of the AOT TypeScript bundle test: a .mts graph with .ts and .cts dependencies.
import { area, type Shape } from "./shapes.ts";
import common from "./common.cts";

const shapes: Shape[] = [{ kind: "sq", size: 3 }];
function total(list: readonly Shape[]): number {
  return list.map(area).reduce((a: number, b: number): number => a + b, 0);
}
console.log(`${total(shapes)}:${common.twice(total(shapes))}:${total.toString().length}`);
