// Platform-identity globals: `self` (the WindowOrWorkerGlobalScope alias) and the `Performance`
// interface over the native monotonic clock ops.

globalThis.self = globalThis;

const performanceConstructionToken = {};
class Performance extends EventTarget {
  constructor(...args) {
    if (args[0] !== performanceConstructionToken) {
      throw new TypeError("Illegal constructor");
    }
    super();
  }
  now() {
    return __perf.now();
  }
  get timeOrigin() {
    return __perf.timeOrigin();
  }
  toJSON() {
    return { timeOrigin: this.timeOrigin };
  }
}
Object.defineProperty(Performance.prototype, Symbol.toStringTag, {
  value: "Performance", configurable: true,
});
for (const name of ["now", "timeOrigin", "toJSON"]) {
  Object.defineProperty(Performance.prototype, name, { enumerable: true });
}
Object.defineProperty(globalThis, "Performance", {
  value: Performance, writable: true, configurable: true, enumerable: false,
});
globalThis.performance = new Performance(performanceConstructionToken);
