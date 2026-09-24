// @ts-check
/**
 * @typedef {Object} Vec2
 * @property {number} x
 * @property {number} y
 */

/**
 * @param {Vec2} a
 * @param {Vec2} b
 * @returns {number}
 */
export function dot(a, b) {
  return a.x * b.x + a.y * b.y;
}

/** @type {(n: number) => number} */
export const square = (n) => n * n;

export class Particle {
  /** @param {number} mass */
  constructor(mass) {
    /** @type {number} */
    this.mass = mass;
    /** @type {number} */
    this.v = 0;
  }
  /** @param {number} dt @returns {number} */
  step(dt) {
    this.v += dt / this.mass;
    return this.v;
  }
}

/**
 * @template T
 * @param {T[]} xs
 * @returns {T | undefined}
 */
export function head(xs) {
  return xs[0];
}
