// An ES module entry (.mts) importing TypeScript modules of every flavour.
import { area, type Shape, VERSION } from "./lib/helper.ts";
import value from "./lib/value.ts";
import common from "./lib/common.cts";
import { createRequire } from "node:module";

const shapes: Shape[] = [{ kind: "sq", size: 2 }, { kind: "circle", r: 1 }];
const total: number = shapes.map(area).reduce((a: number, b: number): number => a + b, 0);
const later = await Promise.resolve<number>(total);
const require = createRequire(import.meta.url);
const again: number = require("./lib/value.ts");
console.log(VERSION, total, later, value, common.twice(2), again, typeof import.meta.url);
