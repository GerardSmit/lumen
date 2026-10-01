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
    set: (t, key, value) => {
      requireKey(key);
      value = `${value}`;
      if (key !== "") env.set(key, value);
      return true;
    },
    has: (t, key) => typeof key === "symbol" ? Reflect.has(t,key) : env.get(key) !== undefined || Reflect.has(t,key),
    deleteProperty: (t, key) => { if (typeof key !== "symbol") env.delete(key); return true; },
    ownKeys: () => env.keys(),
    getOwnPropertyDescriptor: (t, key) => {
      if (typeof key === "symbol") return Reflect.getOwnPropertyDescriptor(t,key);
      const value=env.get(key);
      return value === undefined ? undefined : {value,writable:true,configurable:true,enumerable:true};
    },
    defineProperty: (t, key, desc) => {
      const invalid = (message) => {
        const err = new TypeError(message);
        err.code = "ERR_INVALID_OBJECT_DEFINE_PROPERTY";
        return err;
      };
      if ("value" in desc) {
        if (!desc.writable || !desc.configurable || !desc.enumerable) {
          throw invalid("'process.env' only accepts a configurable, writable, and enumerable data descriptor");
        }
        requireKey(key);
        const value = `${desc.value}`;
        if (key !== "") env.set(key, value);
        return true;
      }
      if ("get" in desc || "set" in desc) {
        throw invalid("'process.env' does not accept an accessor(getter/setter) descriptor");
      }
      throw invalid("'process.env' only accepts a configurable, writable, and enumerable data descriptor");
    },
    preventExtensions: () => false,
  });
  // Worker bootstrap resets only a copied environment; SHARE_ENV binds the backing natively.
  Object.defineProperty(globalThis,"__lumenResetWorkerEnvironment",{value:snapshot=>env.reset(Object.entries(snapshot)),configurable:true});
})();
