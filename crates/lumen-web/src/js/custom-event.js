// DOM CustomEvent shares the Event constructor selected by the active realm. This unit stays
// lazy so lumen-html-js can install its native Event class before this subclass is created.
const eventInternals = globalThis.__eventTargetInternals;
const kEmptyObject = Object.freeze({ __proto__: null });
const kDetail = Symbol("kDetail");

class CustomEvent extends globalThis.Event {
  constructor(type, options = kEmptyObject) {
    if (arguments.length === 0) {
      throw eventInternals.codedError(TypeError, "ERR_MISSING_ARGS", 'The "type" argument must be specified');
    }
    super(type, options);
    this[kDetail] = options?.detail ?? null;
  }
  get detail() {
    if (!(kDetail in Object(this))) throw eventInternals.invalidThis("CustomEvent");
    return this[kDetail];
  }
}
Object.defineProperty(CustomEvent.prototype, Symbol.toStringTag, { value: "CustomEvent", configurable: true });
Object.defineProperty(CustomEvent.prototype, "detail", { enumerable: true });

globalThis.CustomEvent = CustomEvent;
