// MARKER_DELTA_MATH_COMMENT_8861 — imports its importer back (a cycle).
import { BASE } from "../main.js";
export * from "./util.js";

export function mul(a, b) {
    return a * b + BASE;
}
