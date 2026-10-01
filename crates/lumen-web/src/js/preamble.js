// Capture the raw op namespaces and remove them from the global scope; everything below
// closes over these consts. (This whole file set runs inside one IIFE — see lib.rs.)
"use strict";
const __encoding = globalThis.__encoding;
const __url = globalThis.__url;
const __http = globalThis.__http;
const __crypto = globalThis.__crypto;
const __perf = globalThis.__perf;
const __compress = globalThis.__compress;
const __wasm = globalThis.__wasm;
const __ws = globalThis.__ws;
const __sse = globalThis.__sse;
delete globalThis.__encoding;
delete globalThis.__url;
delete globalThis.__http;
delete globalThis.__crypto;
delete globalThis.__perf;
delete globalThis.__compress;
delete globalThis.__wasm;
delete globalThis.__ws;
delete globalThis.__sse;

// A unit of web glue that runs on first use. `names` are the globals it publishes (a trailing
// `!` marks a non-enumerable one): each is an accessor in the slot the unit's own assignment
// would fill, so the key order is unchanged, and the first read (or write) runs `init`, whose
// assignments replace the accessors with plain data properties.
function __lazyWeb(names, init) {
  let state = 0;
  const run = () => {
    if (state !== 0) return;
    state = 1;
    const body = init;
    init = null;
    try {
      body();
    } finally {
      state = 2;
    }
  };
  for (let name of names.split(" ")) {
    const enumerable = !name.endsWith("!");
    if (!enumerable) name = name.slice(0, -1);
    const get = () => {
      run();
      const now = Object.getOwnPropertyDescriptor(globalThis, name);
      if (now !== undefined && now.get === get) {
        if (state !== 2) return undefined;
        delete globalThis[name];
        return undefined;
      }
      return globalThis[name];
    };
    const set = (value) => {
      Object.defineProperty(globalThis, name, { value, writable: true, enumerable, configurable: true });
    };
    Object.defineProperty(globalThis, name, { get, set, enumerable, configurable: true });
  }
}
