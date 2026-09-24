// node:dns — real name resolution over the `__dns` ops (system resolver for lookup/A/AAAA,
// DNS-over-UDP for the record-type queries). Record types the UDP resolver backs (A/AAAA/CNAME/
// NS/MX/TXT/PTR) resolve for real; the ones it can't answer (SRV/SOA/NAPTR/CAA/TLSA/ANY,
// lookupService) fail at *call* time with a Node-shaped `ENOTIMP` error rather than throwing on
// require or inventing records. `Resolver`, `getServers`/`setServers`, and the error-code
// constants mirror Node's shape; `dns.promises` is the same object as `require('dns/promises')`.

{
  // Error-code constants (name → the `E…` string Node uses; identical on `dns` and `dns.promises`).
  const ERROR_CODES = {
    NODATA: "ENODATA", FORMERR: "EFORMERR", SERVFAIL: "ESERVFAIL", NOTFOUND: "ENOTFOUND",
    NOTIMP: "ENOTIMP", REFUSED: "EREFUSED", BADQUERY: "EBADQUERY", BADNAME: "EBADNAME",
    BADFAMILY: "EBADFAMILY", BADRESP: "EBADRESP", CONNREFUSED: "ECONNREFUSED", TIMEOUT: "ETIMEOUT",
    EOF: "EOF", FILE: "EFILE", NOMEM: "ENOMEM", DESTRUCTION: "EDESTRUCTION", BADSTR: "EBADSTR",
    BADFLAGS: "EBADFLAGS", NONAME: "ENONAME", BADHINTS: "EBADHINTS",
    NOTINITIALIZED: "ENOTINITIALIZED", LOADIPHLPAPI: "ELOADIPHLPAPI",
    ADDRGETNETWORKPARAMS: "EADDRGETNETWORKPARAMS", CANCELLED: "ECANCELLED",
  };
  // Numeric `getaddrinfo` hint flags (only on `dns`, not `dns.promises`, matching Node).
  const LOOKUP_FLAGS = { ADDRCONFIG: 1024, ALL: 256, V4MAPPED: 2048 };

  // Record types the DNS-over-UDP op actually answers. Everything else is unbacked → ENOTIMP.
  const BACKED = new Set(["A", "AAAA", "CNAME", "NS", "MX", "TXT", "PTR"]);
  // Record-type resolvers, and the `rrtype` each queries.
  const RESOLVERS = {
    resolve4: "A", resolve6: "AAAA", resolveCname: "CNAME", resolveNs: "NS", resolveMx: "MX",
    resolveTxt: "TXT", resolvePtr: "PTR", resolveSoa: "SOA", resolveSrv: "SRV",
    resolveNaptr: "NAPTR", resolveCaa: "CAA", resolveTlsa: "TLSA", resolveAny: "ANY",
  };

  // A Node-shaped DNS error for a query type we have no backing for. Carries the `code`/`syscall`/
  // `hostname` fields Node code branches on, and only surfaces when the resolver is *called*.
  function notImplError(syscall, hostname) {
    const err = new Error(`${syscall} ENOTIMP${hostname != null ? ` ${hostname}` : ""}`);
    err.code = "ENOTIMP";
    err.syscall = syscall;
    if (hostname != null) err.hostname = String(hostname);
    return err;
  }

  // The c-ares-style syscall name for a record type ("SRV" → "querySrv").
  const syscallFor = (rrtype) => "query" + rrtype[0] + rrtype.slice(1).toLowerCase();

  // Dispatch a record-type query: real for the backed types, deferred ENOTIMP for the rest.
  function rawResolve(hostname, rrtype, callback) {
    const type = String(rrtype).toUpperCase();
    if (!BACKED.has(type)) {
      queueMicrotask(() => callback(notImplError(syscallFor(type), hostname)));
      return;
    }
    __dns.resolve(
      String(hostname),
      type,
      (records) => callback(null, records),
      (err) => callback(err),
    );
  }

  // ---- lookup (system resolver / getaddrinfo) ----

  const validFamilies = [0, 4, 6];
  let invalidHostnameWarningEmitted = false;
  function isIPFamily(s) {
    const net = __builtins.get("net");
    return net ? net.isIP(s) : 0;
  }

  // Node's dns.lookup: the same argument validation, the IP-literal and empty-hostname shortcuts,
  // then getaddrinfo (the system resolver) with the results in resolver order ("verbatim") or
  // IPv4 first.
  function lookup(hostname, options, callback) {
    const V = __validators;
    let hints = 0;
    let family = 0;
    let all = false;
    let verbatim = resultOrder === "verbatim";
    if (hostname) V.validateString(hostname, "hostname");
    if (typeof options === "function") {
      callback = options;
      family = 0;
    } else if (typeof options === "number") {
      V.validateFunction(callback, "callback");
      V.validateOneOf(options, "family", validFamilies);
      family = options;
    } else if (options !== undefined && typeof options !== "object") {
      V.validateFunction(arguments.length === 2 ? options : callback, "callback");
      throw new __errors.ERR_INVALID_ARG_TYPE("options", ["integer", "object"], options);
    } else {
      V.validateFunction(callback, "callback");
      if (options?.hints != null) {
        V.validateNumber(options.hints, "options.hints");
        hints = options.hints >>> 0;
        if ((hints & ~(LOOKUP_FLAGS.ADDRCONFIG | LOOKUP_FLAGS.ALL | LOOKUP_FLAGS.V4MAPPED)) !== 0) {
          throw new __errors.ERR_INVALID_ARG_VALUE("hints", hints);
        }
      }
      if (options?.family != null) {
        switch (options.family) {
          case "IPv4": family = 4; break;
          case "IPv6": family = 6; break;
          default:
            V.validateOneOf(options.family, "options.family", validFamilies);
            family = options.family;
        }
      }
      if (options?.all != null) {
        V.validateBoolean(options.all, "options.all");
        all = options.all;
      }
      if (options?.verbatim != null) {
        V.validateBoolean(options.verbatim, "options.verbatim");
        verbatim = options.verbatim;
      }
    }
    if (!hostname) {
      if (!invalidHostnameWarningEmitted) {
        process.emitWarning(`The provided hostname "${hostname}" is not a valid ` +
          "hostname, and is supported in the dns module solely for compatibility.",
          "DeprecationWarning", "DEP0118");
        invalidHostnameWarningEmitted = true;
      }
      if (all) process.nextTick(callback, null, []);
      else process.nextTick(callback, null, null, family === 6 ? 6 : 4);
      return {};
    }
    const matchedFamily = isIPFamily(hostname);
    if (matchedFamily) {
      if (all) process.nextTick(callback, null, [{ address: hostname, family: matchedFamily }]);
      else process.nextTick(callback, null, hostname, matchedFamily);
      return {};
    }
    __dns.lookup(
      hostname,
      family,
      (list) => {
        if (!verbatim) list = [...list.filter((e) => e.family === 4), ...list.filter((e) => e.family !== 4)];
        if (list.length === 0) {
          const err = new Error(`getaddrinfo ENOTFOUND ${hostname}`);
          Object.assign(err, { errno: __uvCodes.get("EAI_NONAME"), code: "ENOTFOUND", syscall: "getaddrinfo", hostname });
          return callback(err);
        }
        if (all) return callback(null, list);
        callback(null, list[0].address, list[0].family);
      },
      (err) => {
        if (err && typeof err === "object") {
          if (err.hostname === undefined) err.hostname = hostname;
          if (err.syscall === undefined) err.syscall = "getaddrinfo";
          if (err.errno === undefined && err.code === "ENOTFOUND") err.errno = __uvCodes.get("EAI_NONAME");
        }
        callback(err);
      },
    );
    return {};
  }

  // getnameinfo (reverse of a socket address to host + service) has no backing op → ENOTIMP.
  function lookupService(address, port, callback) {
    queueMicrotask(() => callback(notImplError("getnameinfo", address)));
  }

  // ---- reverse (PTR over in-addr.arpa / ip6.arpa) ----

  function reverseImpl(ip, callback) {
    let name;
    if (String(ip).includes(":")) {
      const full = expandIPv6(ip); // IPv6 → nibble-reversed ip6.arpa.
      name = full.split("").reverse().join(".") + ".ip6.arpa";
    } else {
      name = String(ip).split(".").reverse().join(".") + ".in-addr.arpa";
    }
    __dns.resolve(name, "PTR", (records) => callback(null, records), (err) => callback(err));
  }

  // Expand an IPv6 address to its 32 hex nibbles (no colons), for the reverse-PTR name.
  function expandIPv6(ip) {
    const [head, tail = ""] = ip.split("::");
    const h = head ? head.split(":") : [];
    const t = tail ? tail.split(":") : [];
    const missing = 8 - h.length - t.length;
    const groups = [...h, ...Array(missing).fill("0"), ...t];
    return groups.map((g) => g.padStart(4, "0")).join("");
  }

  // ---- default-result-order flag (shared by dns and dns.promises) ----

  let resultOrder = "verbatim";
  const setDefaultResultOrder = (order) => {
    resultOrder = order;
  };
  const getDefaultResultOrder = () => resultOrder;

  // ---- Resolver (an independent server list + the resolve* methods) ----
  // The record-type ops resolve against the system's /etc/resolv.conf, so `setServers` stores the
  // list (and `getServers` reflects it) as a real container even though the native query path is
  // shared; behaviour of the resolve* methods matches the top-level functions exactly.
  class Resolver {
    constructor(options) {
      this._servers = __dns.getServers();
      this._options = options || {};
    }
    getServers() {
      return this._servers.slice();
    }
    setServers(servers) {
      if (!Array.isArray(servers)) {
        throw new TypeError('The "servers" argument must be an instance of Array');
      }
      this._servers = servers.slice();
    }
    setLocalAddress() {}
    cancel() {}
    resolve(hostname, rrtype, callback) {
      if (typeof rrtype === "function") {
        callback = rrtype;
        rrtype = "A";
      }
      rawResolve(hostname, rrtype, callback);
    }
    reverse(ip, callback) {
      reverseImpl(ip, callback);
    }
  }
  for (const [method, rrtype] of Object.entries(RESOLVERS)) {
    Resolver.prototype[method] = function (hostname, options, callback) {
      if (typeof options === "function") callback = options;
      rawResolve(hostname, rrtype, callback);
    };
  }

  // Top-level functions are bound to a shared default Resolver, as in Node.
  const defaultResolver = new Resolver();
  const RESOLVER_METHODS = ["resolve", "reverse", ...Object.keys(RESOLVERS)];
  const bound = {};
  for (const m of RESOLVER_METHODS) bound[m] = defaultResolver[m].bind(defaultResolver);
  const getServers = () => defaultResolver.getServers();
  const setServers = (servers) => defaultResolver.setServers(servers);

  // ---- promises mirror ----

  const promisify = (fn) => (...args) =>
    new Promise((res, rej) =>
      fn(...args, (err, ...vals) => (err ? rej(err) : res(vals.length > 1 ? vals : vals[0]))));

  const lookupPromise = (hostname, options) => {
    const opts = typeof options === "number" ? { family: options } : options === undefined ? {} : options;
    let promise;
    const run = (res, rej) =>
      lookup(hostname, opts, (err, address, family) =>
        err ? rej(err) : res(opts && opts.all ? address : { address, family }));
    // Argument errors throw synchronously, as Node's promises.lookup does.
    let settle;
    promise = new Promise((res, rej) => { settle = [res, rej]; });
    run(settle[0], settle[1]);
    return promise;
  };

  // Promise-returning Resolver: wraps a callback Resolver so its own server list is independent.
  class PromiseResolver {
    constructor(options) {
      this._resolver = new Resolver(options);
    }
    getServers() {
      return this._resolver.getServers();
    }
    setServers(servers) {
      return this._resolver.setServers(servers);
    }
    setLocalAddress(...args) {
      return this._resolver.setLocalAddress(...args);
    }
    cancel() {
      return this._resolver.cancel();
    }
  }
  for (const m of RESOLVER_METHODS) {
    PromiseResolver.prototype[m] = function (...args) {
      return promisify(this._resolver[m].bind(this._resolver))(...args);
    };
  }

  // ---- assemble dns and dns.promises ----

  const dns = {
    lookup,
    lookupService,
    getServers,
    setServers,
    setDefaultResultOrder,
    getDefaultResultOrder,
    Resolver,
    ...bound,
    ...LOOKUP_FLAGS,
    ...ERROR_CODES,
  };

  const promises = {
    lookup: lookupPromise,
    lookupService: promisify(lookupService),
    getServers,
    setServers,
    setDefaultResultOrder,
    getDefaultResultOrder,
    Resolver: PromiseResolver,
    ...ERROR_CODES,
  };
  for (const m of RESOLVER_METHODS) promises[m] = promisify(bound[m]);

  dns.promises = promises;

  __builtins.set("dns", dns);
  __builtins.set("dns/promises", promises);
}
