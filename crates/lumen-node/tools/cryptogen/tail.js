// ---- registration ------------------------------------------------------------------------------

__builtins.set("crypto", require("crypto"));
__internals.set("cryptoRequire", (id) => require(id));
__internals.set("cryptoBinding", cryptoBinding);
__internals.set("cloneModule:internal/crypto",
                (id, name) => (name === "keyObjectFromClone" ? keyObjectFromClone : require(id)[name]));

// The WebCrypto globals, as accessors like Node's own that an assignment replaces.
function lazyGlobal(name, get) {
  return {
    __proto__: null,
    get,
    set(value) {
      Object.defineProperty(globalThis, name, { __proto__: null, value, writable: true, enumerable: false, configurable: true });
    },
    enumerable: false,
    configurable: true,
  };
}
Object.defineProperty(globalThis, "crypto", lazyGlobal("crypto", () => require("internal/crypto/webcrypto").crypto));
Object.defineProperty(globalThis, "Crypto", lazyGlobal("Crypto", () => require("internal/crypto/webcrypto").Crypto));
Object.defineProperty(globalThis, "CryptoKey", lazyGlobal("CryptoKey", () => require("internal/crypto/webcrypto").CryptoKey));
Object.defineProperty(globalThis, "SubtleCrypto", lazyGlobal("SubtleCrypto", () => require("internal/crypto/webcrypto").SubtleCrypto));
