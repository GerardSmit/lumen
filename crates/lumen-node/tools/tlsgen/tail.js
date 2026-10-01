
// ---- registration ------------------------------------------------------------------------------

{
  const tls = require("tls");
  // lumen: tls.getCACertificates (Node 22) over the engine's trust store.
  if (typeof tls.getCACertificates !== "function") {
    tls.getCACertificates = function getCACertificates(type = "default") {
      if (type !== "default" && type !== "bundled" && type !== "system" && type !== "extra") {
        throw new codes.ERR_INVALID_ARG_VALUE("type", type);
      }
      return type === "default" || type === "bundled" ? tls.rootCertificates.slice() : [];
    };
  }
  __builtins.set("tls", tls);
  __builtins.set("_tls_common", require("_tls_common"));
  __builtins.set("_tls_wrap", require("_tls_wrap"));
  __builtins.set("https", netRequire("https"));
}
// The module table and bindings, for --expose-internals (internals.js).
__internals.set("tlsRequire", require);
__internals.set("tlsBinding", internalBinding);
