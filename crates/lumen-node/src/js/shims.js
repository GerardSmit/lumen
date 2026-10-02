// Small node: builtins the Express stack pulls in. Each is the practical subset its consumers
// use, not a full implementation; gaps throw clearly rather than silently misbehaving. Each one is
// built on its first `__builtins.get` (see `__lazyValue`), so touching one does not build the rest.

// node:querystring and node:url live in url.js (Node's lib sources over lumen-web's URL).

// node:net now lives in its own glue file (net.js) — its surface grew past the "small shim" bar
// (BlockList, SocketAddress, auto-select-family flags).

// node:assert lives in assert.js.

// ---- node:string_decoder ----------------------------------------------------------------------
// Streaming decode that never splits a character across chunks: UTF-8 through TextDecoder's
// streaming mode (WHATWG replacement semantics, as Node's decoder), UTF-16LE holding back an odd
// byte or a lone high surrogate, base64/base64url holding back a partial 3-byte group (so each
// chunk encodes on its own, as Node emits it), and the single-byte encodings chunk by chunk.
__builtins.set("string_decoder", __lazyValue(() => {
"lumen:run-once";
  const { ERR_INVALID_ARG_TYPE, ERR_UNKNOWN_ENCODING } = __errors;
  function normalizeEncoding(enc) {
    const raw = enc === undefined || enc === null ? "utf8" : `${enc}`;
    switch (raw.toLowerCase()) {
      case "": case "utf8": case "utf-8": return "utf8";
      case "ucs2": case "ucs-2": case "utf16le": case "utf-16le": return "utf16le";
      case "latin1": case "binary": return "latin1";
      case "base64": return "base64";
      case "base64url": return "base64url";
      case "hex": return "hex";
      case "ascii": return "ascii";
    }
    throw new ERR_UNKNOWN_ENCODING(enc);
  }
  function toBytes(buf) {
    if (buf instanceof Uint8Array) return buf;
    if (ArrayBuffer.isView(buf)) return new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength);
    throw new ERR_INVALID_ARG_TYPE("buf", ["Buffer", "TypedArray", "DataView"], buf);
  }
  // A function constructor, NOT a class: iconv-lite inherits via `StringDecoder.call(this, enc)`
  // + `Child.prototype = StringDecoder.prototype`, which a class constructor rejects ("cannot be
  // invoked without new").
  const kNative = Symbol("kNativeDecoder");
  // state layout of Node's native decoder: 4 bytes of an incomplete character, missing bytes,
  // buffered bytes
  const kMissing = 4;
  const kBuffered = 5;
  function makeString(bytes, enc) {
    return Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength).toString(enc);
  }
  function StringDecoder(encoding) {
    this.encoding = normalizeEncoding(encoding);
    this[kNative] = new Uint8Array(6);
  }
  StringDecoder.prototype.write = function (buf) {
    if (typeof buf === "string") return buf;
    let data = toBytes(buf);
    const st = this[kNative];
    const enc = this.encoding;
    if (enc !== "utf8" && enc !== "utf16le" && enc !== "base64" && enc !== "base64url") {
      return makeString(data, enc);
    }
    let prepend = "";
    let body = "";
    if (st[kMissing] > 0) {
      if (enc === "utf8") {
        for (let i = 0; i < data.length && i < st[kMissing]; ++i) {
          if ((data[i] & 0xc0) !== 0x80) {
            st[kMissing] = 0;
            st.set(data.subarray(0, i), st[kBuffered]);
            st[kBuffered] += i;
            data = data.subarray(i);
            break;
          }
        }
      }
      const found = Math.min(data.length, st[kMissing]);
      st.set(data.subarray(0, found), st[kBuffered]);
      data = data.subarray(found);
      st[kMissing] -= found;
      st[kBuffered] += found;
      if (st[kMissing] === 0) {
        prepend = makeString(st.subarray(0, st[kBuffered]), enc);
        st[kBuffered] = 0;
      }
    }
    if (data.length === 0) return prepend;
    const n = data.length;
    if (enc === "utf8" && (data[n - 1] & 0x80)) {
      for (let i = n - 1; ; --i) {
        st[kBuffered]++;
        if ((data[i] & 0xc0) === 0x80) {
          if (st[kBuffered] >= 4 || i === 0) {
            st[kBuffered] = 0;
            break;
          }
        } else {
          if ((data[i] & 0xe0) === 0xc0) st[kMissing] = 2;
          else if ((data[i] & 0xf0) === 0xe0) st[kMissing] = 3;
          else if ((data[i] & 0xf8) === 0xf0) st[kMissing] = 4;
          else {
            st[kBuffered] = 0;
            break;
          }
          if (st[kBuffered] >= st[kMissing]) {
            st[kMissing] = 0;
            st[kBuffered] = 0;
          }
          st[kMissing] -= st[kBuffered];
          break;
        }
      }
    } else if (enc === "utf16le") {
      if (n % 2 === 1) {
        st[kBuffered] = 1;
        st[kMissing] = 1;
      } else if ((data[n - 1] & 0xfc) === 0xd8) {
        st[kBuffered] = 2;
        st[kMissing] = 2;
      }
    } else if (enc === "base64" || enc === "base64url") {
      st[kBuffered] = n % 3;
      if (st[kBuffered] > 0) st[kMissing] = 3 - st[kBuffered];
    }
    let len = n;
    if (st[kBuffered] > 0) {
      len -= st[kBuffered];
      st.set(data.subarray(len), 0);
    }
    if (len > 0) body = makeString(data.subarray(0, len), enc);
    return prepend + body;
  };
  StringDecoder.prototype.end = function (buf) {
    let out = buf === undefined ? "" : this.write(buf);
    const st = this[kNative];
    if (this.encoding === "utf16le" && st[kBuffered] % 2 === 1) {
      st[kMissing]--;
      st[kBuffered]--;
    }
    if (st[kBuffered] > 0) {
      out += makeString(st.subarray(0, st[kBuffered]), this.encoding);
      st[kMissing] = 0;
      st[kBuffered] = 0;
    }
    return out;
  };
  StringDecoder.prototype.text = function (buf, offset) {
    this[kNative][kMissing] = 0;
    this[kNative][kBuffered] = 0;
    return this.write(toBytes(buf).subarray(offset));
  };
  Object.defineProperties(StringDecoder.prototype, {
    lastNeed: { get() { return this[kNative][kMissing]; }, configurable: true, enumerable: true },
    lastTotal: { get() { return this[kNative][kBuffered] + this[kNative][kMissing]; }, configurable: true, enumerable: true },
    lastChar: { get() { return Buffer.from(this[kNative].buffer, 0, 4); }, configurable: true, enumerable: true },
  });
  return { StringDecoder };
}));
