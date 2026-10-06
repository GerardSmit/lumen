// Capture the raw op namespaces and remove them from the global scope; everything below
// closes over these consts. (This whole file set runs inside one IIFE — see lib.rs.)
"use strict";
const __url = globalThis.__url;
const __http = globalThis.__http;
const __wasm = globalThis.__wasm;
const __ws = globalThis.__ws;
const __sse = globalThis.__sse;
delete globalThis.__url;
delete globalThis.__http;
delete globalThis.__wasm;
delete globalThis.__ws;
delete globalThis.__sse;

// Capture descriptor intrinsics before author code can replace them. Lazy unit
// publication inspects properties without invoking a host/author getter or setter.
const __lazyGlobal = globalThis;
const __lazyGetDescriptor = Object.getOwnPropertyDescriptor;
const __lazyDefineProperty = Object.defineProperty;
const __lazyCreate = Object.create;
const __lazyOwnKeys = Reflect.ownKeys;

// Each unit owns only the lazy accessor slots it installed. The build redirects
// export writes to a private receiver; reads and exported closures keep the real
// globalThis. A host replacement, deletion, or reentrant override is never restored
// or overwritten when another export causes the rest of the unit to initialize.
function __lazyWeb(names, init) {
  let state = 0;
  const slots = __lazyCreate(null);
  const exports = __lazyCreate(null);
  const owns = (name) => {
    const slot = slots[name];
    const now = __lazyGetDescriptor(__lazyGlobal, name);
    return slot !== undefined && now !== undefined && now.get === slot.get && now.set === slot.set &&
      now.enumerable === slot.enumerable && now.configurable === slot.configurable;
  };
  const define = (name, descriptor) => {
    // Object.defineProperty performs ToPropertyKey exactly once. Most bootstrap
    // exports already use strings; symbols and non-string keys retain semantics.
    let key = name;
    if (typeof key !== "string" && typeof key !== "symbol") {
      const holder = __lazyCreate(null);
      __lazyDefineProperty(holder, key, { value: true });
      key = __lazyOwnKeys(holder)[0];
    }
    if (slots[key] === undefined || owns(key)) {
      __lazyDefineProperty(__lazyGlobal, key, descriptor);
    }
    return __lazyGlobal;
  };
  const run = () => {
    if (state !== 0) return;
    state = 1;
    const body = init;
    init = null;
    try {
      body(exports, define);
    } finally {
      state = 2;
    }
  };
  for (let name of names.split(" ")) {
    const enumerable = !name.endsWith("!");
    if (!enumerable) name = name.slice(0, -1);
    const get = () => {
      run();
      const now = __lazyGetDescriptor(__lazyGlobal, name);
      if (now !== undefined && now.get === get) {
        if (state !== 2) return undefined;
        if (owns(name)) delete __lazyGlobal[name];
        return undefined;
      }
      return __lazyGlobal[name];
    };
    // An explicit author assignment retains its existing lazy-setter behavior.
    const set = (value) => {
      const now = __lazyGetDescriptor(__lazyGlobal, name);
      const e = now !== undefined && now.get === get ? now.enumerable : enumerable;
      __lazyDefineProperty(__lazyGlobal, name, { value, writable: true, enumerable: e, configurable: true });
    };
    slots[name] = { get, set, enumerable, configurable: true };
    __lazyDefineProperty(exports, name, {
      set(value) { if (owns(name)) set(value); }, configurable: true
    });
    // Providers installed before this lazy family are canonical too.
    if (__lazyGetDescriptor(__lazyGlobal, name) === undefined) {
      __lazyDefineProperty(__lazyGlobal, name, { get, set, enumerable, configurable: true });
    }
  }
}
