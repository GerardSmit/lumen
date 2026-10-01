(() => {
  const env = globalThis.__env;
  delete globalThis.__env;
  const target = {};
  const requireKey = key => {
    if (typeof key === "symbol") throw new TypeError("Cannot convert a Symbol value to a string");
    return key;
  };
  process.env = new Proxy(target, {
    get: (t, key) => typeof key === "symbol" ? Reflect.get(t,key) : env.get(key) ?? Reflect.get(t,key),
    set: (t, key, value) => { env.set(requireKey(key), `${value}`); return true; },
    has: (t, key) => typeof key === "symbol" ? Reflect.has(t,key) : env.get(key) !== undefined || Reflect.has(t,key),
    deleteProperty: (t, key) => { env.delete(requireKey(key)); return true; },
    ownKeys: () => env.keys(),
    getOwnPropertyDescriptor: (t, key) => {
      if (typeof key === "symbol") return Reflect.getOwnPropertyDescriptor(t,key);
      const value=env.get(key);
      return value === undefined ? undefined : {value,writable:true,configurable:true,enumerable:true};
    },
    defineProperty: (t, key, desc) => {
      if (!("value" in desc) || !desc.writable || !desc.configurable || !desc.enumerable) {
        throw new TypeError("process.env descriptors must contain value, writable, enumerable and configurable");
      }
      env.set(requireKey(key), `${desc.value}`);
      return true;
    },
    preventExtensions: () => false,
  });
  // Worker bootstrap resets only a copied environment; SHARE_ENV binds the backing natively.
  Object.defineProperty(globalThis,"__lumenResetWorkerEnvironment",{value:snapshot=>env.reset(Object.entries(snapshot)),configurable:true});
})();
