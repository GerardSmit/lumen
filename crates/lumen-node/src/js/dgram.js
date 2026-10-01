// node:dgram — Node's own lib/dgram.js running over the udp_wrap handle (see net.js, which
// carries the sources and the `__udp` bridge); this file only publishes it.

__builtins.set("dgram", __internals.get("netRequire")("dgram"));
