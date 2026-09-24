// ---- URL.createObjectURL / revokeObjectURL (internal/url installObjectURLMethods) ------------

{
  const { ERR_INVALID_ARG_TYPE } = __errors;
  function createObjectURL(obj) {
    if (!(obj instanceof Blob)) throw new ERR_INVALID_ARG_TYPE("obj", "Blob", obj);
    const id = crypto.randomUUID();
    __objectURLs.set(id, obj);
    return `blob:nodedata:${id}`;
  }
  // Node's C++ RevokeObjectURL: parse, require blob:nodedata:<id>, forget the id.
  function revokeObjectURL(url) {
    url = `${url}`;
    let parsed;
    try {
      parsed = new URL(url);
    } catch {
      return;
    }
    if (parsed.protocol !== "blob:") return;
    const path = parsed.pathname;
    if (!path.startsWith("nodedata:")) return;
    __objectURLs.delete(path.slice("nodedata:".length));
  }
  Object.defineProperties(URL, {
    createObjectURL: { __proto__: null, configurable: true, writable: true, enumerable: true, value: createObjectURL },
    revokeObjectURL: { __proto__: null, configurable: true, writable: true, enumerable: true, value: revokeObjectURL },
  });
}

// ---- registration ------------------------------------------------------------------------------

__builtins.set("querystring", require("querystring"));
__builtins.set("url", require("url"));
