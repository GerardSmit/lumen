__builtins.set("http2", require("http2"));
// The module table and bindings, for --expose-internals (internals.js).
__internals.set("http2Require", require);
__internals.set("http2Binding", internalBinding);
