// ---- registration ------------------------------------------------------------------------------

// node:dgram and node:dns are defined in this module table; their glue files pick them up here.
__internals.set("netRequire", require);
// node:tls (tls.js) runs over the same module table and handle bindings.
__internals.set("netBinding", internalBinding);
// child_process: descriptors received over an IPC channel become these handles.
__internals.set("netHandleFromFd", handleFromFd);
__builtins.set("net", require("net"));
{
  const http = require("http");
  // lumen: the WHATWG WebSocket classes ride along on node:http, as in newer Node releases.
  for (const name of ["WebSocket", "MessageEvent", "CloseEvent"]) {
    if (typeof globalThis[name] === "function" && !(name in http)) http[name] = globalThis[name];
  }
  __builtins.set("http", http);
  __builtins.set("_http_agent", require("_http_agent"));
  __builtins.set("_http_client", require("_http_client"));
  __builtins.set("_http_common", require("_http_common"));
  __builtins.set("_http_incoming", require("_http_incoming"));
  __builtins.set("_http_outgoing", require("_http_outgoing"));
  __builtins.set("_http_server", require("_http_server"));
}

// process.binding(): Node 20 still serves an allowlist of internal bindings (DEP0111, a warning
// only under --pending-deprecation). The ones this glue implements are served from here.
{
  const served = ["http_parser", "tcp_wrap", "pipe_wrap", "stream_wrap", "uv"];
  const previous = process.binding;
  process.binding = function binding(name) {
    name = `${name}`;
    if (served.includes(name)) return internalBinding(name);
    return previous.call(this, name);
  };
}
