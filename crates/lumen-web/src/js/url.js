// URL + URLSearchParams: a port of Node's lib/internal/url.js (v20) over the native ada port in
// url.rs. The class shapes, brand checks, error codes/messages and serialization match Node, so
// code that probes URL objects (deep-equal, inspect, Object.keys on searchParams, …) behaves the
// same. Everything lives in this block; the classes are published on globalThis.
{
  const kOmitted = 4294967295;
  const inspectCustom = Symbol.for("nodejs.util.inspect.custom");
  const kEnumerableProperty = { __proto__: null, enumerable: true };

  // ---- Node-style errors (codes and messages as lib/internal/errors.js words them) ----
  function nodeError(Base, code, message) {
    const err = new Base(message);
    Object.defineProperty(err, "toString", {
      __proto__: null,
      value() {
        return `${this.name} [${code}]: ${this.message}`;
      },
      enumerable: false,
      writable: true,
      configurable: true,
    });
    err.code = code;
    return err;
  }
  const errInvalidThis = (type) =>
    nodeError(TypeError, "ERR_INVALID_THIS", `Value of "this" must be of type ${type}`);
  function errMissingArgs(...args) {
    const names = args.map((a) => `"${a}"`);
    let msg = "The ";
    if (names.length === 1) msg += `${names[0]} argument`;
    else if (names.length === 2) msg += `${names[0]} and ${names[1]} arguments`;
    else msg += `${names.slice(0, -1).join(", ")}, and ${names[names.length - 1]} arguments`;
    return nodeError(TypeError, "ERR_MISSING_ARGS", `${msg} must be specified`);
  }
  const errArgNotIterable = (name) =>
    nodeError(TypeError, "ERR_ARG_NOT_ITERABLE", `${name} must be iterable`);
  const errInvalidTuple = (name, type) =>
    nodeError(TypeError, "ERR_INVALID_TUPLE", `${name} must be an iterable ${type} tuple`);
  function describeReceived(value) {
    if (value == null) return ` Received ${value}`;
    if (typeof value === "function") return ` Received function ${value.name}`;
    if (typeof value === "object") {
      if (value.constructor?.name) return ` Received an instance of ${value.constructor.name}`;
      return " Received [object Object]";
    }
    let shown = String(value);
    if (shown.length > 28) shown = `${shown.slice(0, 25)}...`;
    if (typeof value === "string") shown = `'${shown}'`;
    return ` Received type ${typeof value} (${shown})`;
  }
  function validateFunction(value, name) {
    if (typeof value !== "function") {
      throw nodeError(
        TypeError,
        "ERR_INVALID_ARG_TYPE",
        `The "${name}" argument must be of type function.${describeReceived(value)}`,
      );
    }
  }
  function invalidUrl(input, base) {
    // Node raises this from C++: a plain TypeError carrying `code`, `input` (and `base`).
    const err = new TypeError("Invalid URL");
    err.code = "ERR_INVALID_URL";
    err.input = input;
    if (base !== undefined) err.base = base;
    return err;
  }

  function toUSVString(value) {
    const str = `${value}`;
    return typeof str.toWellFormed === "function"
      ? str.toWellFormed()
      : str.replace(/[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/g, "�");
  }

  function getConstructorOf(obj) {
    while (obj) {
      const descriptor = Object.getOwnPropertyDescriptor(obj, "constructor");
      if (descriptor !== undefined && typeof descriptor.value === "function" && descriptor.value.name !== "") {
        return descriptor.value;
      }
      obj = Object.getPrototypeOf(obj);
    }
    return null;
  }

  const removeColors = (s) => String(s).replace(/\u001b\[\d\d?m/g, "");

  // ---- lib/internal/querystring.js ----
  const hexTable = new Array(256);
  for (let i = 0; i < 256; ++i) hexTable[i] = "%" + ((i < 16 ? "0" : "") + i.toString(16)).toUpperCase();
  const isHexTable = new Int8Array(256);
  for (const c of "0123456789ABCDEFabcdef") isHexTable[c.charCodeAt(0)] = 1;

  function encodeStr(str, noEscapeTable, table) {
    const len = str.length;
    if (len === 0) return "";
    let out = "";
    let lastPos = 0;
    let i = 0;
    outer: for (; i < len; i++) {
      let c = str.charCodeAt(i);
      while (c < 0x80) {
        if (noEscapeTable[c] !== 1) {
          if (lastPos < i) out += str.slice(lastPos, i);
          lastPos = i + 1;
          out += table[c];
        }
        if (++i === len) break outer;
        c = str.charCodeAt(i);
      }
      if (lastPos < i) out += str.slice(lastPos, i);
      if (c < 0x800) {
        lastPos = i + 1;
        out += table[0xc0 | (c >> 6)] + table[0x80 | (c & 0x3f)];
        continue;
      }
      if (c < 0xd800 || c >= 0xe000) {
        lastPos = i + 1;
        out += table[0xe0 | (c >> 12)] + table[0x80 | ((c >> 6) & 0x3f)] + table[0x80 | (c & 0x3f)];
        continue;
      }
      ++i;
      if (i >= len) throw nodeError(URIError, "ERR_INVALID_URI", "URI malformed");
      const c2 = str.charCodeAt(i) & 0x3ff;
      lastPos = i + 1;
      c = 0x10000 + (((c & 0x3ff) << 10) | c2);
      out +=
        table[0xf0 | (c >> 18)] +
        table[0x80 | ((c >> 12) & 0x3f)] +
        table[0x80 | ((c >> 6) & 0x3f)] +
        table[0x80 | (c & 0x3f)];
    }
    if (lastPos === 0) return str;
    if (lastPos < len) return out + str.slice(lastPos);
    return out;
  }

  // querystring.unescape: decodeURIComponent, falling back to Node's byte-wise unescapeBuffer
  // (char codes truncated to a byte) decoded as UTF-8 with replacement.
  const utf8Decoder = new TextDecoder();
  function qsUnescape(s) {
    try {
      return decodeURIComponent(s);
    } catch {
      const out = new Uint8Array(s.length);
      let n = 0;
      for (let i = 0; i < s.length; i++) {
        const c = s.charCodeAt(i);
        if (c === 37 && i + 2 < s.length + 0 && isHexTable[s.charCodeAt(i + 1)] && isHexTable[s.charCodeAt(i + 2)]) {
          out[n++] = parseInt(s.slice(i + 1, i + 3), 16);
          i += 2;
        } else {
          out[n++] = c;
        }
      }
      return utf8Decoder.decode(out.subarray(0, n));
    }
  }

  // ---- application/x-www-form-urlencoded (internal/url.js parseParams/serializeParams) ----
  function parseParams(qs) {
    const out = [];
    let seenSep = false;
    let buf = "";
    let encoded = false;
    let encodeCheck = 0;
    let i = qs[0] === "?" ? 1 : 0;
    let pairStart = i;
    let lastPos = i;
    for (; i < qs.length; ++i) {
      const code = qs.charCodeAt(i);
      if (code === 38 /* & */) {
        if (pairStart === i) {
          lastPos = pairStart = i + 1;
          continue;
        }
        if (lastPos < i) buf += qs.slice(lastPos, i);
        if (encoded) buf = qsUnescape(buf);
        out.push(buf);
        if (!seenSep) out.push("");
        seenSep = false;
        buf = "";
        encoded = false;
        encodeCheck = 0;
        lastPos = pairStart = i + 1;
        continue;
      }
      if (!seenSep && code === 61 /* = */) {
        if (lastPos < i) buf += qs.slice(lastPos, i);
        if (encoded) buf = qsUnescape(buf);
        out.push(buf);
        seenSep = true;
        buf = "";
        encoded = false;
        encodeCheck = 0;
        lastPos = i + 1;
        continue;
      }
      if (code === 43 /* + */) {
        if (lastPos < i) buf += qs.slice(lastPos, i);
        buf += " ";
        lastPos = i + 1;
      } else if (!encoded) {
        if (code === 37 /* % */) {
          encodeCheck = 1;
        } else if (encodeCheck > 0) {
          if (isHexTable[code] === 1) {
            if (++encodeCheck === 3) encoded = true;
          } else {
            encodeCheck = 0;
          }
        }
      }
    }
    if (pairStart === i) return out;
    if (lastPos < i) buf += qs.slice(lastPos, i);
    if (encoded) buf = qsUnescape(buf);
    out.push(buf);
    if (!seenSep) out.push("");
    return out;
  }

  const noEscape = new Int8Array(128);
  for (const c of "*-._0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz") {
    noEscape[c.charCodeAt(0)] = 1;
  }
  const paramHexTable = hexTable.slice();
  paramHexTable[0x20] = "+";

  function serializeParams(array) {
    const len = array.length;
    if (len === 0) return "";
    let output = `${encodeStr(array[0], noEscape, paramHexTable)}=${encodeStr(array[1], noEscape, paramHexTable)}`;
    for (let i = 2; i < len; i += 2) {
      output += `&${encodeStr(array[i], noEscape, paramHexTable)}=${encodeStr(array[i + 1], noEscape, paramHexTable)}`;
    }
    return output;
  }

  function merge(out, start, mid, end, lBuffer, rBuffer) {
    const sizeLeft = mid - start;
    const sizeRight = end - mid;
    let l, r, o;
    for (l = 0; l < sizeLeft; l++) lBuffer[l] = out[start + l];
    for (r = 0; r < sizeRight; r++) rBuffer[r] = out[mid + r];
    l = 0;
    r = 0;
    o = start;
    while (l < sizeLeft && r < sizeRight) {
      if (lBuffer[l] <= rBuffer[r]) {
        out[o++] = lBuffer[l++];
        out[o++] = lBuffer[l++];
      } else {
        out[o++] = rBuffer[r++];
        out[o++] = rBuffer[r++];
      }
    }
    while (l < sizeLeft) out[o++] = lBuffer[l++];
    while (r < sizeRight) out[o++] = rBuffer[r++];
  }

  // ---- URLContext: ada's url_components, as Node keeps them ----
  class URLContext {
    href = "";
    protocol_end = 0;
    username_end = 0;
    host_start = 0;
    host_end = 0;
    pathname_start = 0;
    search_start = 0;
    hash_start = 0;
    port = 0;
    scheme_type = 1;

    get hasPort() {
      return this.port !== kOmitted;
    }
    get hasSearch() {
      return this.search_start !== kOmitted;
    }
    get hasHash() {
      return this.hash_start !== kOmitted;
    }
  }

  const contextForInspect = Symbol("context");
  const updateActions = {
    kProtocol: 0,
    kHost: 1,
    kHostname: 2,
    kPort: 3,
    kUsername: 4,
    kPassword: 5,
    kPathname: 6,
    kSearch: 7,
    kHash: 8,
    kHref: 9,
  };

  let setURLSearchParamsContext;
  let getURLSearchParamsList;
  let setURLSearchParams;

  class URLSearchParamsIterator {
    #target;
    #kind;
    #index;

    constructor(target, kind) {
      this.#target = target;
      this.#kind = kind;
      this.#index = 0;
    }

    next() {
      if (typeof this !== "object" || this === null || !(#target in this)) {
        throw errInvalidThis("URLSearchParamsIterator");
      }
      const index = this.#index;
      const values = getURLSearchParamsList(this.#target);
      if (index >= values.length) return { value: undefined, done: true };
      const name = values[index];
      const value = values[index + 1];
      this.#index = index + 2;
      let result;
      if (this.#kind === "key") result = name;
      else if (this.#kind === "value") result = value;
      else result = [name, value];
      return { value: result, done: false };
    }

    [inspectCustom](recurseTimes, ctx, inspect) {
      if (!this || typeof this !== "object" || !(#target in this)) {
        throw errInvalidThis("URLSearchParamsIterator");
      }
      if (typeof recurseTimes === "number" && recurseTimes < 0) return ctx.stylize("[Object]", "special");
      const innerOpts = { ...ctx };
      if (recurseTimes !== null) innerOpts.depth = recurseTimes - 1;
      const index = this.#index;
      const values = getURLSearchParamsList(this.#target);
      const output = values.slice(index).reduce((prev, cur, i) => {
        const key = i % 2 === 0;
        if (this.#kind === "key" && key) prev.push(cur);
        else if (this.#kind === "value" && !key) prev.push(cur);
        else if (this.#kind === "key+value" && !key) prev.push([values[index + i - 1], cur]);
        return prev;
      }, []);
      const breakLn = inspect(output, innerOpts).includes("\n");
      const outputStrs = output.map((p) => inspect(p, innerOpts));
      const outputStr = breakLn ? `\n  ${outputStrs.join(",\n  ")}` : ` ${outputStrs.join(", ")}`;
      return `${this[Symbol.toStringTag]} {${outputStr} }`;
    }
  }
  delete URLSearchParamsIterator.prototype.constructor;
  Object.setPrototypeOf(
    URLSearchParamsIterator.prototype,
    Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]())),
  );
  Object.defineProperties(URLSearchParamsIterator.prototype, {
    [Symbol.toStringTag]: { __proto__: null, configurable: true, value: "URLSearchParams Iterator" },
    next: kEnumerableProperty,
  });

  class URLSearchParams {
    #searchParams = [];
    #context;

    static {
      setURLSearchParamsContext = (obj, ctx) => {
        obj.#context = ctx;
      };
      getURLSearchParamsList = (obj) => obj.#searchParams;
      setURLSearchParams = (obj, query) => {
        obj.#searchParams = query === undefined ? [] : parseParams(query);
      };
    }

    constructor(init = undefined) {
      if (init == null) {
        // Nothing to do.
      } else if (typeof init === "object" || typeof init === "function") {
        const method = init[Symbol.iterator];
        if (method === this[Symbol.iterator] && #searchParams in init) {
          this.#searchParams = init.#searchParams.slice();
        } else if (method != null) {
          if (typeof method !== "function") throw errArgNotIterable("Query pairs");
          for (const pair of init) {
            if (pair == null) {
              throw errInvalidTuple("Each query pair", "[name, value]");
            } else if (Array.isArray(pair)) {
              if (pair.length !== 2) throw errInvalidTuple("Each query pair", "[name, value]");
              this.#searchParams.push(toUSVString(pair[0]), toUSVString(pair[1]));
            } else {
              if ((typeof pair !== "object" && typeof pair !== "function") || typeof pair[Symbol.iterator] !== "function") {
                throw errInvalidTuple("Each query pair", "[name, value]");
              }
              let length = 0;
              for (const element of pair) {
                length++;
                this.#searchParams.push(toUSVString(element));
              }
              if (length !== 2) throw errInvalidTuple("Each query pair", "[name, value]");
            }
          }
        } else {
          const visited = new Map();
          const keys = Reflect.ownKeys(init);
          for (let i = 0; i < keys.length; i++) {
            const key = keys[i];
            const desc = Reflect.getOwnPropertyDescriptor(init, key);
            if (desc !== undefined && desc.enumerable) {
              const typedKey = toUSVString(key);
              const typedValue = toUSVString(init[key]);
              const keyIdx = visited.get(typedKey);
              if (keyIdx !== undefined) {
                this.#searchParams[keyIdx] = typedValue;
              } else {
                visited.set(typedKey, this.#searchParams.push(typedKey, typedValue) - 1);
              }
            }
          }
        }
      } else {
        init = toUSVString(init);
        this.#searchParams = init ? parseParams(init) : [];
      }
    }

    [inspectCustom](recurseTimes, ctx, inspect) {
      if (typeof this !== "object" || this === null || !(#searchParams in this)) {
        throw errInvalidThis("URLSearchParams");
      }
      if (typeof recurseTimes === "number" && recurseTimes < 0) return ctx.stylize("[Object]", "special");
      const separator = ", ";
      const innerOpts = { ...ctx };
      if (recurseTimes !== null) innerOpts.depth = recurseTimes - 1;
      const innerInspect = (v) => inspect(v, innerOpts);
      const list = this.#searchParams;
      const output = [];
      for (let i = 0; i < list.length; i += 2) output.push(`${innerInspect(list[i])} => ${innerInspect(list[i + 1])}`);
      const length = output.reduce((prev, cur) => prev + removeColors(cur).length + separator.length, -separator.length);
      if (length > ctx.breakLength) return `${this.constructor.name} {\n  ${output.join(",\n  ")} }`;
      if (output.length) return `${this.constructor.name} { ${output.join(separator)} }`;
      return `${this.constructor.name} {}`;
    }

    get size() {
      if (typeof this !== "object" || this === null || !(#searchParams in this)) {
        throw errInvalidThis("URLSearchParams");
      }
      return this.#searchParams.length / 2;
    }

    append(name, value) {
      if (typeof this !== "object" || this === null || !(#searchParams in this)) {
        throw errInvalidThis("URLSearchParams");
      }
      if (arguments.length < 2) throw errMissingArgs("name", "value");
      this.#searchParams.push(toUSVString(name), toUSVString(value));
      if (this.#context) this.#context.search = this.toString();
    }

    delete(name, value = undefined) {
      if (typeof this !== "object" || this === null || !(#searchParams in this)) {
        throw errInvalidThis("URLSearchParams");
      }
      if (arguments.length < 1) throw errMissingArgs("name");
      const list = this.#searchParams;
      name = toUSVString(name);
      if (value !== undefined) {
        value = toUSVString(value);
        for (let i = 0; i < list.length; ) {
          if (list[i] === name && list[i + 1] === value) list.splice(i, 2);
          else i += 2;
        }
      } else {
        for (let i = 0; i < list.length; ) {
          if (list[i] === name) list.splice(i, 2);
          else i += 2;
        }
      }
      if (this.#context) this.#context.search = this.toString();
    }

    get(name) {
      if (typeof this !== "object" || this === null || !(#searchParams in this)) {
        throw errInvalidThis("URLSearchParams");
      }
      if (arguments.length < 1) throw errMissingArgs("name");
      const list = this.#searchParams;
      name = toUSVString(name);
      for (let i = 0; i < list.length; i += 2) {
        if (list[i] === name) return list[i + 1];
      }
      return null;
    }

    getAll(name) {
      if (typeof this !== "object" || this === null || !(#searchParams in this)) {
        throw errInvalidThis("URLSearchParams");
      }
      if (arguments.length < 1) throw errMissingArgs("name");
      const list = this.#searchParams;
      const values = [];
      name = toUSVString(name);
      for (let i = 0; i < list.length; i += 2) {
        if (list[i] === name) values.push(list[i + 1]);
      }
      return values;
    }

    has(name, value = undefined) {
      if (typeof this !== "object" || this === null || !(#searchParams in this)) {
        throw errInvalidThis("URLSearchParams");
      }
      if (arguments.length < 1) throw errMissingArgs("name");
      const list = this.#searchParams;
      name = toUSVString(name);
      if (value !== undefined) value = toUSVString(value);
      for (let i = 0; i < list.length; i += 2) {
        if (list[i] === name && (value === undefined || list[i + 1] === value)) return true;
      }
      return false;
    }

    set(name, value) {
      if (typeof this !== "object" || this === null || !(#searchParams in this)) {
        throw errInvalidThis("URLSearchParams");
      }
      if (arguments.length < 2) throw errMissingArgs("name", "value");
      const list = this.#searchParams;
      name = toUSVString(name);
      value = toUSVString(value);
      let found = false;
      for (let i = 0; i < list.length; ) {
        if (list[i] === name) {
          if (!found) {
            list[i + 1] = value;
            found = true;
            i += 2;
          } else {
            list.splice(i, 2);
          }
        } else {
          i += 2;
        }
      }
      if (!found) list.push(name, value);
      if (this.#context) this.#context.search = this.toString();
    }

    sort() {
      if (typeof this !== "object" || this === null || !(#searchParams in this)) {
        throw errInvalidThis("URLSearchParams");
      }
      const a = this.#searchParams;
      const len = a.length;
      if (len <= 2) {
        // Nothing to do.
      } else if (len < 100) {
        for (let i = 2; i < len; i += 2) {
          const curKey = a[i];
          const curVal = a[i + 1];
          let j;
          for (j = i - 2; j >= 0; j -= 2) {
            if (a[j] > curKey) {
              a[j + 2] = a[j];
              a[j + 3] = a[j + 1];
            } else {
              break;
            }
          }
          a[j + 2] = curKey;
          a[j + 3] = curVal;
        }
      } else {
        const lBuffer = new Array(len);
        const rBuffer = new Array(len);
        for (let step = 2; step < len; step *= 2) {
          for (let start = 0; start < len - 2; start += 2 * step) {
            const mid = start + step;
            let end = mid + step;
            end = end < len ? end : len;
            if (mid > end) continue;
            merge(a, start, mid, end, lBuffer, rBuffer);
          }
        }
      }
      if (this.#context) this.#context.search = this.toString();
    }

    entries() {
      if (typeof this !== "object" || this === null || !(#searchParams in this)) {
        throw errInvalidThis("URLSearchParams");
      }
      return new URLSearchParamsIterator(this, "key+value");
    }

    forEach(callback, thisArg = undefined) {
      if (typeof this !== "object" || this === null || !(#searchParams in this)) {
        throw errInvalidThis("URLSearchParams");
      }
      validateFunction(callback, "callback");
      let list = this.#searchParams;
      let i = 0;
      while (i < list.length) {
        const key = list[i];
        const value = list[i + 1];
        callback.call(thisArg, value, key, this);
        list = this.#searchParams;
        i += 2;
      }
    }

    keys() {
      if (typeof this !== "object" || this === null || !(#searchParams in this)) {
        throw errInvalidThis("URLSearchParams");
      }
      return new URLSearchParamsIterator(this, "key");
    }

    values() {
      if (typeof this !== "object" || this === null || !(#searchParams in this)) {
        throw errInvalidThis("URLSearchParams");
      }
      return new URLSearchParamsIterator(this, "value");
    }

    toString() {
      if (typeof this !== "object" || this === null || !(#searchParams in this)) {
        throw errInvalidThis("URLSearchParams");
      }
      return serializeParams(this.#searchParams);
    }
  }

  Object.defineProperties(URLSearchParams.prototype, {
    append: kEnumerableProperty,
    delete: kEnumerableProperty,
    get: kEnumerableProperty,
    getAll: kEnumerableProperty,
    has: kEnumerableProperty,
    set: kEnumerableProperty,
    size: kEnumerableProperty,
    sort: kEnumerableProperty,
    entries: kEnumerableProperty,
    forEach: kEnumerableProperty,
    keys: kEnumerableProperty,
    values: kEnumerableProperty,
    toString: kEnumerableProperty,
    [Symbol.toStringTag]: { __proto__: null, configurable: true, value: "URLSearchParams" },
    [Symbol.iterator]: {
      __proto__: null,
      configurable: true,
      writable: true,
      value: URLSearchParams.prototype.entries,
    },
  });

  function isURL(self) {
    return Boolean(self?.href && self.protocol && self.auth === undefined && self.path === undefined);
  }

  class URL {
    #context = new URLContext();
    #searchParams;

    constructor(input, base = undefined) {
      if (arguments.length === 0) throw errMissingArgs("url");
      input = `${input}`;
      if (base !== undefined) base = `${base}`;
      // Node hands these to C++ as UTF-8, where lone surrogates become U+FFFD.
      const record = __url.parse(toUSVString(input), base === undefined ? undefined : toUSVString(base));
      if (record === null) throw invalidUrl(input, base);
      this.#updateContext(record);
    }

    [inspectCustom](depth, opts, inspect) {
      if (typeof depth === "number" && depth < 0) return this;
      const constructor = getConstructorOf(this) || URL;
      const obj = { __proto__: { constructor } };
      obj.href = this.href;
      obj.origin = this.origin;
      obj.protocol = this.protocol;
      obj.username = this.username;
      obj.password = this.password;
      obj.host = this.host;
      obj.hostname = this.hostname;
      obj.port = this.port;
      obj.pathname = this.pathname;
      obj.search = this.search;
      obj.searchParams = this.searchParams;
      obj.hash = this.hash;
      if (opts.showHidden) obj[contextForInspect] = this.#context;
      if (typeof inspect !== "function") return `${constructor.name} ${JSON.stringify(obj)}`;
      return `${constructor.name} ${inspect(obj, opts)}`;
    }

    #updateContext(record) {
      const ctx = this.#context;
      ctx.href = record[0];
      ctx.protocol_end = record[1];
      ctx.username_end = record[2];
      ctx.host_start = record[3];
      ctx.host_end = record[4];
      ctx.port = record[5];
      ctx.pathname_start = record[6];
      ctx.search_start = record[7];
      ctx.hash_start = record[8];
      ctx.scheme_type = record[9];
      if (this.#searchParams) {
        if (ctx.hasSearch) setURLSearchParams(this.#searchParams, this.search);
        else setURLSearchParams(this.#searchParams, undefined);
      }
    }

    #update(action, value) {
      const record = __url.update(this.#context.href, action, toUSVString(value));
      if (record !== null) this.#updateContext(record);
      return record !== null;
    }

    toString() {
      return this.#context.href;
    }

    get href() {
      return this.#context.href;
    }

    set href(value) {
      value = `${value}`;
      if (!this.#update(updateActions.kHref, value)) throw invalidUrl(value);
    }

    get origin() {
      const protocol = this.#context.href.slice(0, this.#context.protocol_end);
      if (this.#context.scheme_type !== 1) {
        if (this.#context.scheme_type === 6) return "null";
        return `${protocol}//${this.host}`;
      }
      if (protocol === "blob:") {
        const path = this.pathname;
        if (path.length > 0) {
          try {
            const out = new URL(path);
            if (out.#context.scheme_type === 0 || out.#context.scheme_type === 2) {
              return `${out.protocol}//${out.host}`;
            }
          } catch {
            // Opaque origin.
          }
        }
      }
      return "null";
    }

    get protocol() {
      return this.#context.href.slice(0, this.#context.protocol_end);
    }

    set protocol(value) {
      this.#update(updateActions.kProtocol, `${value}`);
    }

    get username() {
      if (this.#context.protocol_end + 2 < this.#context.username_end) {
        return this.#context.href.slice(this.#context.protocol_end + 2, this.#context.username_end);
      }
      return "";
    }

    set username(value) {
      this.#update(updateActions.kUsername, `${value}`);
    }

    get password() {
      if (this.#context.host_start - this.#context.username_end > 0) {
        return this.#context.href.slice(this.#context.username_end + 1, this.#context.host_start);
      }
      return "";
    }

    set password(value) {
      this.#update(updateActions.kPassword, `${value}`);
    }

    get host() {
      let startsAt = this.#context.host_start;
      if (this.#context.href[startsAt] === "@") startsAt++;
      if (startsAt === this.#context.host_end) return "";
      return this.#context.href.slice(startsAt, this.#context.pathname_start);
    }

    set host(value) {
      this.#update(updateActions.kHost, `${value}`);
    }

    get hostname() {
      let startsAt = this.#context.host_start;
      if (this.#context.href[startsAt] === "@") startsAt++;
      return this.#context.href.slice(startsAt, this.#context.host_end);
    }

    set hostname(value) {
      this.#update(updateActions.kHostname, `${value}`);
    }

    get port() {
      return this.#context.hasPort ? `${this.#context.port}` : "";
    }

    set port(value) {
      this.#update(updateActions.kPort, `${value}`);
    }

    get pathname() {
      let endsAt;
      if (this.#context.hasSearch) endsAt = this.#context.search_start;
      else if (this.#context.hasHash) endsAt = this.#context.hash_start;
      return this.#context.href.slice(this.#context.pathname_start, endsAt);
    }

    set pathname(value) {
      this.#update(updateActions.kPathname, `${value}`);
    }

    get search() {
      if (!this.#context.hasSearch) return "";
      let endsAt = this.#context.href.length;
      if (this.#context.hasHash) endsAt = this.#context.hash_start;
      if (endsAt - this.#context.search_start <= 1) return "";
      return this.#context.href.slice(this.#context.search_start, endsAt);
    }

    set search(value) {
      this.#update(updateActions.kSearch, toUSVString(value));
    }

    get searchParams() {
      if (this.#searchParams == null) {
        this.#searchParams = new URLSearchParams(this.search);
        setURLSearchParamsContext(this.#searchParams, this);
      }
      return this.#searchParams;
    }

    get hash() {
      if (!this.#context.hasHash || this.#context.href.length - this.#context.hash_start <= 1) return "";
      return this.#context.href.slice(this.#context.hash_start);
    }

    set hash(value) {
      this.#update(updateActions.kHash, `${value}`);
    }

    toJSON() {
      return this.#context.href;
    }

    static canParse(url, base = undefined) {
      if (arguments.length === 0) throw errMissingArgs("url");
      url = `${url}`;
      if (base !== undefined) return __url.canParse(toUSVString(url), toUSVString(base));
      return __url.canParse(toUSVString(url));
    }
  }

  Object.defineProperties(URL.prototype, {
    [Symbol.toStringTag]: { __proto__: null, configurable: true, value: "URL" },
    toString: kEnumerableProperty,
    href: kEnumerableProperty,
    origin: kEnumerableProperty,
    protocol: kEnumerableProperty,
    username: kEnumerableProperty,
    password: kEnumerableProperty,
    host: kEnumerableProperty,
    hostname: kEnumerableProperty,
    port: kEnumerableProperty,
    pathname: kEnumerableProperty,
    search: kEnumerableProperty,
    searchParams: kEnumerableProperty,
    hash: kEnumerableProperty,
    toJSON: kEnumerableProperty,
  });

  Object.defineProperties(URL, {
    canParse: { __proto__: null, configurable: true, writable: true, enumerable: true },
  });

  // Node's internal/url helpers the node: glue builds `url` on (hidden from user code).
  Object.defineProperty(URL, Symbol.for("lumen.url.internals"), {
    __proto__: null,
    value: Object.freeze({
      domainToASCII: (domain) => __url.domainToASCII(toUSVString(domain)),
      domainToUnicode: (domain) => __url.domainToUnicode(toUSVString(domain)),
      idnaToASCII: (domain) => __url.toASCII(toUSVString(domain)),
      idnaToUnicode: (domain) => __url.toUnicode(toUSVString(domain)),
      format: (href, hash, unicode, search, auth) => __url.format(href, hash, unicode, search, auth),
      isURL,
      toUSVString,
      encodeStr,
      hexTable,
      isHexTable,
      parseParams,
      updateActions,
    }),
  });

  for (const [name, value] of [
    ["URL", URL],
    ["URLSearchParams", URLSearchParams],
  ]) {
    Object.defineProperty(globalThis, name, {
      __proto__: null,
      value,
      writable: true,
      configurable: true,
      enumerable: false,
    });
  }
}
