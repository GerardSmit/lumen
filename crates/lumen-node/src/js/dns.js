// node:dns — Node's own lib/dns.js over the cares_wrap binding (see net.js, which carries the
// sources, the DNS client and the `__dns` bridge); this file only publishes it.

__builtins.set("dns", __internals.get("netRequire")("dns"));
__builtins.set("dns/promises", __internals.get("netRequire")("dns/promises"));
