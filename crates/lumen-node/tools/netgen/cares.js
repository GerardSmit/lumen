// ---- internalBinding('cares_wrap') --------------------------------------------------------------

// What c-ares gives Node: a resolver channel that speaks DNS (RFC 1035) to its servers over UDP,
// falling back to TCP for truncated answers, plus getaddrinfo/getnameinfo over the system
// resolver (`__dns`, dns.rs).

const aiFlags = (() => {
  const platform = __os.info().platform;
  if (platform === "linux" || platform === "android") return { ADDRCONFIG: 0x20, ALL: 0x10, V4MAPPED: 0x8 };
  return { ADDRCONFIG: 0x400, ALL: 0x100, V4MAPPED: 0x800 };
})();

class GetAddrInfoReqWrap {}
class GetNameInfoReqWrap {}
class QueryReqWrap {}

// libuv's EAI_* code (a native op's `code`) as the negative number `dnsException` expects.
function gaiErrno(error) {
  const code = error && error.code;
  const errno = typeof code === "string" ? __uvCodes.get(code) : undefined;
  return errno === undefined ? __uvCodes.get("EAI_FAIL") : errno;
}

function caresGetaddrinfo(req, hostname, family, hints, verbatim) {
  __dns.getaddrinfo(
    hostname,
    family,
    hints | 0,
    (list) => {
      if (verbatim === false) {
        list = [...list.filter((a) => a.family === 4), ...list.filter((a) => a.family !== 4)];
      }
      req.oncomplete(0, list.map((a) => a.address));
    },
    (error) => req.oncomplete(gaiErrno(error)),
  );
  return 0;
}

function caresGetnameinfo(req, address, port) {
  __dns.getnameinfo(
    address,
    port,
    ([hostname, service]) => req.oncomplete(0, hostname, service),
    (error) => req.oncomplete(gaiErrno(error)),
  );
  return 0;
}

// ---- wire format ---------------------------------------------------------------------------

const T = { A: 1, NS: 2, CNAME: 5, SOA: 6, PTR: 12, MX: 15, TXT: 16, AAAA: 28, SRV: 33, NAPTR: 35, CAA: 257, ANY: 255 };

class WireError extends Error {
  constructor(code) {
    super(code);
    this.aresCode = code;
  }
}

function encodeName(name) {
  if (name.endsWith(".")) name = name.slice(0, -1);
  const parts = [];
  if (name !== "") {
    for (const label of name.split(".")) {
      const bytes = Buffer.from(label, "utf8");
      if (bytes.length === 0 || bytes.length > 63) throw new WireError("EBADNAME");
      parts.push(Buffer.from([bytes.length]), bytes);
    }
  }
  parts.push(Buffer.from([0]));
  const out = Buffer.concat(parts);
  if (out.length > 255) throw new WireError("EBADNAME");
  return out;
}

function buildQuery(id, name, type) {
  const header = Buffer.alloc(12);
  header.writeUInt16BE(id, 0);
  header.writeUInt16BE(0x0100, 2);
  header.writeUInt16BE(1, 4);
  const tail = Buffer.alloc(4);
  tail.writeUInt16BE(type, 0);
  tail.writeUInt16BE(1, 2);
  return Buffer.concat([header, encodeName(name), tail]);
}

function readName(buf, offset) {
  const labels = [];
  let pos = offset;
  let next = -1;
  for (let hops = 0; ; hops++) {
    if (pos >= buf.length || hops > 128) throw new WireError("EBADRESP");
    const len = buf[pos];
    if (len === 0) {
      pos += 1;
      break;
    }
    if ((len & 0xc0) === 0xc0) {
      if (pos + 1 >= buf.length) throw new WireError("EBADRESP");
      if (next < 0) next = pos + 2;
      pos = ((len & 0x3f) << 8) | buf[pos + 1];
      continue;
    }
    if ((len & 0xc0) !== 0 || pos + 1 + len > buf.length) throw new WireError("EBADRESP");
    labels.push(buf.toString("latin1", pos + 1, pos + 1 + len));
    pos += 1 + len;
  }
  return { name: labels.join("."), next: next >= 0 ? next : pos };
}

function formatIPv6(bytes) {
  const groups = [];
  for (let i = 0; i < 16; i += 2) groups.push((bytes[i] << 8) | bytes[i + 1]);
  let bestStart = -1;
  let bestLen = 0;
  for (let i = 0; i < 8;) {
    if (groups[i] !== 0) {
      i++;
      continue;
    }
    let j = i;
    while (j < 8 && groups[j] === 0) j++;
    if (j - i > bestLen) {
      bestStart = i;
      bestLen = j - i;
    }
    i = j;
  }
  if (bestLen < 2) return groups.map((g) => g.toString(16)).join(":");
  const head = groups.slice(0, bestStart).map((g) => g.toString(16)).join(":");
  const tail = groups.slice(bestStart + bestLen).map((g) => g.toString(16)).join(":");
  return `${head}::${tail}`;
}

function parseRecord(buf, type, start, length) {
  const end = start + length;
  if (end > buf.length) throw new WireError("EBADRESP");
  switch (type) {
    case T.A:
      if (length !== 4) throw new WireError("EBADRESP");
      return { type: "A", address: `${buf[start]}.${buf[start + 1]}.${buf[start + 2]}.${buf[start + 3]}` };
    case T.AAAA:
      if (length !== 16) throw new WireError("EBADRESP");
      return { type: "AAAA", address: formatIPv6(buf.subarray(start, end)) };
    case T.NS:
    case T.CNAME:
    case T.PTR:
      return { type: type === T.NS ? "NS" : type === T.CNAME ? "CNAME" : "PTR", value: readName(buf, start).name };
    case T.MX: {
      if (length < 3) throw new WireError("EBADRESP");
      return { type: "MX", priority: buf.readUInt16BE(start), exchange: readName(buf, start + 2).name };
    }
    case T.TXT: {
      const entries = [];
      let pos = start;
      while (pos < end) {
        const len = buf[pos];
        if (pos + 1 + len > end) throw new WireError("EBADRESP");
        entries.push(buf.toString("utf8", pos + 1, pos + 1 + len));
        pos += 1 + len;
      }
      return { type: "TXT", entries };
    }
    case T.SOA: {
      const ns = readName(buf, start);
      const mail = readName(buf, ns.next);
      if (mail.next + 20 > end) throw new WireError("EBADRESP");
      const p = mail.next;
      return {
        type: "SOA",
        nsname: ns.name,
        hostmaster: mail.name,
        serial: buf.readUInt32BE(p),
        refresh: buf.readUInt32BE(p + 4),
        retry: buf.readUInt32BE(p + 8),
        expire: buf.readUInt32BE(p + 12),
        minttl: buf.readUInt32BE(p + 16),
      };
    }
    case T.SRV: {
      if (length < 7) throw new WireError("EBADRESP");
      return {
        type: "SRV",
        priority: buf.readUInt16BE(start),
        weight: buf.readUInt16BE(start + 2),
        port: buf.readUInt16BE(start + 4),
        name: readName(buf, start + 6).name,
      };
    }
    case T.NAPTR: {
      if (length < 4) throw new WireError("EBADRESP");
      const order = buf.readUInt16BE(start);
      const preference = buf.readUInt16BE(start + 2);
      let pos = start + 4;
      const strings = [];
      for (let i = 0; i < 3; i++) {
        const len = buf[pos];
        if (pos + 1 + len > end) throw new WireError("EBADRESP");
        strings.push(buf.toString("latin1", pos + 1, pos + 1 + len));
        pos += 1 + len;
      }
      return {
        type: "NAPTR",
        flags: strings[0],
        service: strings[1],
        regexp: strings[2],
        replacement: readName(buf, pos).name,
        order,
        preference,
      };
    }
    case T.CAA: {
      if (length < 2) throw new WireError("EBADRESP");
      const critical = buf[start];
      const tagLength = buf[start + 1];
      if (start + 2 + tagLength > end) throw new WireError("EBADRESP");
      const tag = buf.toString("latin1", start + 2, start + 2 + tagLength);
      return { type: "CAA", critical, tag, value: buf.toString("latin1", start + 2 + tagLength, end) };
    }
    default:
      return null;
  }
}

function parseResponse(buf) {
  if (buf.length < 12) throw new WireError("EBADRESP");
  const flags = buf.readUInt16BE(2);
  const counts = [buf.readUInt16BE(4), buf.readUInt16BE(6)];
  let pos = 12;
  for (let i = 0; i < counts[0]; i++) {
    pos = readName(buf, pos).next + 4;
    if (pos > buf.length) throw new WireError("EBADRESP");
  }
  const answers = [];
  for (let i = 0; i < counts[1]; i++) {
    const owner = readName(buf, pos);
    pos = owner.next;
    if (pos + 10 > buf.length) throw new WireError("EBADRESP");
    const type = buf.readUInt16BE(pos);
    const ttl = buf.readUInt32BE(pos + 4);
    const length = buf.readUInt16BE(pos + 8);
    pos += 10;
    const record = parseRecord(buf, type, pos, length);
    if (record !== null) {
      record.ttl = ttl;
      answers.push(record);
    }
    pos += length;
  }
  return { id: buf.readUInt16BE(0), truncated: (flags & 0x0200) !== 0, rcode: flags & 0xf, response: (flags & 0x8000) !== 0, answers };
}

const RCODE_ERRORS = { 1: "EFORMERR", 2: "ESERVFAIL", 3: "ENOTFOUND", 4: "ENOTIMP", 5: "EREFUSED" };

// ---- the channel ---------------------------------------------------------------------------

const DNS_ESETSRVPENDING = -1000;
const ARES_MESSAGES = {
  ENODATA: "DNS server returned answer with no data",
  EFORMERR: "DNS server claims query was misformatted",
  ESERVFAIL: "DNS server returned general failure",
  ENOTFOUND: "Domain name not found",
  ENOTIMP: "DNS server does not implement requested operation",
  EREFUSED: "DNS server refused query",
  EBADQUERY: "Misformatted DNS query",
  EBADNAME: "Misformatted domain name",
  EBADFAMILY: "Unsupported address family",
  EBADRESP: "Misformatted DNS reply",
  ECONNREFUSED: "Could not contact DNS servers",
  ETIMEOUT: "Timeout while contacting DNS servers",
  EOF: "End of file",
  EFILE: "Error reading file",
  ENOMEM: "Out of memory",
  EDESTRUCTION: "Channel is being destroyed",
  EBADSTR: "Misformatted string",
  EBADFLAGS: "Illegal flags specified",
  ENONAME: "Given hostname is not numeric",
  EBADHINTS: "Illegal hints flags specified",
  ENOTINITIALIZED: "c-ares library initialization not yet performed",
  ECANCELLED: "DNS query cancelled",
};
function strerror(code) {
  if (code === DNS_ESETSRVPENDING) return "There are pending queries.";
  return ARES_MESSAGES[code] || "unknown";
}

function systemServers() {
  const servers = __dns.getServers().map((ip) => ({ ip, port: 53, family: ip.includes(":") ? 6 : 4 }));
  return servers.length === 0 ? [{ ip: "127.0.0.1", port: 53, family: 4 }] : servers;
}

class Query {
  constructor(channel, name, type, finish) {
    this.channel = channel;
    this.name = name;
    this.type = type;
    this.finish = finish;
    this.id = Math.floor(Math.random() * 0x10000);
    this.attempt = 0;
    this.error = "ETIMEOUT";
    this.timer = null;
    this.sockets = new Map();
    this.done = false;
    this.packet = null;
  }
}

class ChannelWrap {
  constructor(timeout, tries) {
    this._timeout = timeout < 0 ? 2000 : timeout;
    this._tries = tries > 0 ? tries : 4;
    this._servers = null;
    this._active = new Set();
    this._local4 = "";
    this._local6 = "";
  }

  getServers() {
    const servers = this._servers || systemServers();
    return servers.map((s) => [s.ip, s.port]);
  }

  setServers(list) {
    if (this._active.size !== 0) return DNS_ESETSRVPENDING;
    this._servers = list.map(([family, ip, port]) => ({ ip, port, family }));
    return 0;
  }

  setLocalAddress(ipv4, ipv6) {
    const v4 = net_isIPv4(ipv4);
    if (!v4) throw new codes.ERR_INVALID_IP_ADDRESS(ipv4);
    this._local4 = ipv4;
    if (ipv6 !== undefined) {
      if (!isIPv6(ipv6)) throw new codes.ERR_INVALID_IP_ADDRESS(ipv6);
      this._local6 = ipv6;
    }
  }

  cancel() {
    for (const query of [...this._active]) this._end(query, "ECANCELLED");
  }

  _serverList() {
    return this._servers || systemServers();
  }

  _start(req, name, type, parse) {
    const query = new Query(this, name, type, (code, answers) => {
      let result;
      let ttls;
      if (code === 0) {
        try {
          [result, ttls] = parse(answers);
        } catch {
          code = "EBADRESP";
        }
        if (code === 0 && result === null) code = "ENODATA";
      }
      if (code !== 0) req.oncomplete(code);
      else req.oncomplete(0, result, ttls);
    });
    try {
      query.packet = buildQuery(query.id, name, type);
    } catch (error) {
      this._active.add(query);
      process.nextTick(() => this._end(query, error.aresCode || "EBADNAME"));
      return 0;
    }
    this._active.add(query);
    process.nextTick(() => this._send(query));
    return 0;
  }

  _end(query, code, answers) {
    if (query.done) return;
    query.done = true;
    this._active.delete(query);
    if (query.timer !== null) clearTimeout(query.timer);
    for (const handle of query.sockets.values()) handle.close();
    query.sockets.clear();
    query.finish(code, answers);
  }

  _send(query) {
    if (query.done) return;
    const servers = this._serverList();
    const limit = this._tries * servers.length;
    if (query.attempt >= limit) {
      this._end(query, query.error);
      return;
    }
    const k = query.attempt++;
    const server = servers[k % servers.length];
    const wait = this._timeout * 2 ** Math.floor(k / servers.length);
    const handle = this._socketFor(query, server);
    if (handle === null) {
      query.error = "ECONNREFUSED";
      process.nextTick(() => this._send(query));
      return;
    }
    const err = handle.send({}, [query.packet], 1, server.port, server.ip);
    if (err < 0) {
      query.error = "ECONNREFUSED";
      process.nextTick(() => this._send(query));
      return;
    }
    query.timer = setTimeout(() => {
      query.timer = null;
      query.error = "ETIMEOUT";
      this._send(query);
    }, wait);
  }

  _socketFor(query, server) {
    let handle = query.sockets.get(server.family);
    if (handle !== undefined) return handle;
    handle = new UDP();
    const local = server.family === 6 ? this._local6 : this._local4;
    const err = server.family === 6
      ? handle.bind6(local || "::", 0, 0)
      : handle.bind(local || "0.0.0.0", 0, 0);
    if (err !== 0) {
      handle.close();
      return null;
    }
    handle.onmessage = (nread, _h, buf) => this._onPacket(query, server, nread, buf);
    handle.recvStart();
    query.sockets.set(server.family, handle);
    return handle;
  }

  _onPacket(query, server, nread, buf) {
    if (query.done) return;
    if (nread < 0) {
      query.error = "ECONNREFUSED";
      this._retryNow(query);
      return;
    }
    let parsed;
    try {
      parsed = parseResponse(buf);
    } catch {
      return;
    }
    if (!parsed.response || parsed.id !== query.id) return;
    if (parsed.truncated) {
      this._viaTcp(query, server);
      return;
    }
    this._complete(query, parsed);
  }

  _retryNow(query) {
    if (query.timer !== null) clearTimeout(query.timer);
    query.timer = null;
    process.nextTick(() => this._send(query));
  }

  _complete(query, parsed) {
    if (parsed.rcode === 0) {
      this._end(query, 0, parsed.answers);
    } else if (parsed.rcode === 1 || parsed.rcode === 3) {
      this._end(query, RCODE_ERRORS[parsed.rcode]);
    } else {
      query.error = RCODE_ERRORS[parsed.rcode] || "ESERVFAIL";
      this._retryNow(query);
    }
  }

  _viaTcp(query, server) {
    if (query.timer !== null) clearTimeout(query.timer);
    query.timer = null;
    const socket = require("net").connect({ host: server.ip, port: server.port });
    const chunks = [];
    let settled = false;
    const fail = (code) => {
      if (settled) return;
      settled = true;
      socket.destroy();
      query.error = code;
      if (!query.done) this._send(query);
    };
    query.timer = setTimeout(() => fail("ETIMEOUT"), Math.max(this._timeout, 1) * 2);
    socket.on("connect", () => {
      const frame = Buffer.alloc(2 + query.packet.length);
      frame.writeUInt16BE(query.packet.length, 0);
      query.packet.copy(frame, 2);
      socket.write(frame);
    });
    socket.on("data", (chunk) => {
      chunks.push(chunk);
      const all = Buffer.concat(chunks);
      if (all.length < 2 || all.length < 2 + all.readUInt16BE(0) || settled) return;
      settled = true;
      socket.destroy();
      if (query.timer !== null) clearTimeout(query.timer);
      query.timer = null;
      let parsed;
      try {
        parsed = parseResponse(all.subarray(2, 2 + all.readUInt16BE(0)));
      } catch {
        this._end(query, "EBADRESP");
        return;
      }
      if (parsed.id !== query.id) this._end(query, "EBADRESP");
      else this._complete(query, parsed);
    });
    socket.on("error", () => fail("ECONNREFUSED"));
    socket.on("close", () => fail("ECONNREFUSED"));
  }
}

function net_isIPv4(text) {
  return /^(25[0-5]|2[0-4]\d|1?\d?\d)(\.(25[0-5]|2[0-4]\d|1?\d?\d)){3}$/.test(text);
}
function isIPv6(text) {
  return require("net").isIPv6(text);
}

// The records of `type` among the answers (CNAME chains leave A/AAAA answers under other names).
const ofType = (answers, type) => answers.filter((a) => a.type === type);

const PARSERS = {
  queryA: [T.A, (answers) => {
    const found = ofType(answers, "A");
    return found.length === 0 ? [null] : [found.map((a) => a.address), found.map((a) => a.ttl)];
  }],
  queryAaaa: [T.AAAA, (answers) => {
    const found = ofType(answers, "AAAA");
    return found.length === 0 ? [null] : [found.map((a) => a.address), found.map((a) => a.ttl)];
  }],
  queryCname: [T.CNAME, (answers) => {
    const found = ofType(answers, "CNAME");
    return found.length === 0 ? [null] : [found.map((a) => a.value)];
  }],
  queryNs: [T.NS, (answers) => {
    const found = ofType(answers, "NS");
    return found.length === 0 ? [null] : [found.map((a) => a.value)];
  }],
  queryPtr: [T.PTR, (answers) => {
    const found = ofType(answers, "PTR");
    return found.length === 0 ? [null] : [found.map((a) => a.value)];
  }],
  queryMx: [T.MX, (answers) => {
    const found = ofType(answers, "MX");
    return found.length === 0 ? [null] : [found.map((a) => ({ exchange: a.exchange, priority: a.priority }))];
  }],
  queryTxt: [T.TXT, (answers) => {
    const found = ofType(answers, "TXT");
    return found.length === 0 ? [null] : [found.map((a) => a.entries)];
  }],
  querySrv: [T.SRV, (answers) => {
    const found = ofType(answers, "SRV");
    return found.length === 0 ? [null] : [found.map((a) => ({ name: a.name, port: a.port, priority: a.priority, weight: a.weight }))];
  }],
  queryNaptr: [T.NAPTR, (answers) => {
    const found = ofType(answers, "NAPTR");
    return found.length === 0 ? [null] : [found.map((a) => ({
      flags: a.flags, service: a.service, regexp: a.regexp, replacement: a.replacement,
      order: a.order, preference: a.preference,
    }))];
  }],
  querySoa: [T.SOA, (answers) => {
    const found = ofType(answers, "SOA");
    if (found.length === 0) return [null];
    const a = found[0];
    return [{
      nsname: a.nsname, hostmaster: a.hostmaster, serial: a.serial, refresh: a.refresh,
      retry: a.retry, expire: a.expire, minttl: a.minttl,
    }];
  }],
  queryCaa: [T.CAA, (answers) => {
    const found = ofType(answers, "CAA");
    return found.length === 0 ? [null] : [found.map((a) => ({ critical: a.critical, [a.tag]: a.value }))];
  }],
  queryAny: [T.ANY, (answers) => {
    const out = [];
    for (const type of ["A", "AAAA", "CNAME", "MX", "NS", "TXT", "SRV", "PTR", "NAPTR", "SOA", "CAA"]) {
      for (const a of ofType(answers, type)) {
        switch (type) {
          case "A":
          case "AAAA": out.push({ type, address: a.address, ttl: a.ttl }); break;
          case "CNAME":
          case "NS":
          case "PTR": out.push({ type, value: a.value }); break;
          case "MX": out.push({ type, priority: a.priority, exchange: a.exchange }); break;
          case "TXT": out.push({ type, entries: a.entries }); break;
          case "SRV": out.push({ type, name: a.name, port: a.port, priority: a.priority, weight: a.weight }); break;
          case "NAPTR":
            out.push({ type, flags: a.flags, service: a.service, regexp: a.regexp, replacement: a.replacement, order: a.order, preference: a.preference });
            break;
          case "SOA":
            out.push({ type, nsname: a.nsname, hostmaster: a.hostmaster, serial: a.serial, refresh: a.refresh, retry: a.retry, expire: a.expire, minttl: a.minttl });
            break;
          case "CAA": out.push({ type, critical: a.critical, [a.tag]: a.value }); break;
        }
      }
    }
    return out.length === 0 ? [null] : [out];
  }],
};
for (const [method, [type, parse]] of Object.entries(PARSERS)) {
  ChannelWrap.prototype[method] = function (req, name) {
    return this._start(req, String(name), type, parse);
  };
}

function reverseName(ip) {
  if (ip.includes(":")) {
    const [head, tail = ""] = ip.split("::");
    const h = head ? head.split(":") : [];
    const t = tail ? tail.split(":") : [];
    const groups = [...h, ...Array(8 - h.length - t.length).fill("0"), ...t];
    const nibbles = groups.map((g) => g.padStart(4, "0")).join("");
    return `${nibbles.split("").reverse().join(".")}.ip6.arpa`;
  }
  return `${ip.split(".").reverse().join(".")}.in-addr.arpa`;
}

ChannelWrap.prototype.getHostByAddr = function (req, ip) {
  const family = require("net").isIP(ip);
  if (family === 0) return UV_EINVAL;
  return this._start(req, reverseName(ip), T.PTR, (answers) => {
    const found = ofType(answers, "PTR");
    return found.length === 0 ? [null] : [found.map((a) => a.value)];
  });
};

bindings.cares_wrap = {
  GetAddrInfoReqWrap,
  GetNameInfoReqWrap,
  QueryReqWrap,
  ChannelWrap,
  getaddrinfo: caresGetaddrinfo,
  getnameinfo: caresGetnameinfo,
  canonicalizeIP(ip) {
    const family = require("net").isIP(ip);
    if (family === 0) return undefined;
    if (family === 4) return ip.split(".").map(Number).join(".");
    return new (require("net").SocketAddress)({ address: ip, family: "ipv6" }).address;
  },
  strerror,
  AI_ADDRCONFIG: aiFlags.ADDRCONFIG,
  AI_ALL: aiFlags.ALL,
  AI_V4MAPPED: aiFlags.V4MAPPED,
  DNS_ORDER_VERBATIM: 0,
  DNS_ORDER_IPV4_FIRST: 1,
  DNS_ORDER_IPV6_FIRST: 2,
};

