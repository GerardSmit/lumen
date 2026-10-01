// HPACK (RFC 7541) header compression for node:http2: an encoder and a decoder with their own
// dynamic tables. Header lists are arrays of [name, value, neverIndex] triples, in wire order;
// strings are byte strings (latin1) except that a value holding code points above 0xff is sent
// as UTF-8.
{
  const huffman = globalThis.__lumenHpackHuffman;
  const STATIC = [null,
    [":authority", ""], [":method", "GET"], [":method", "POST"], [":path", "/"], [":path", "/index.html"],
    [":scheme", "http"], [":scheme", "https"], [":status", "200"], [":status", "204"], [":status", "206"],
    [":status", "304"], [":status", "400"], [":status", "404"], [":status", "500"], ["accept-charset", ""],
    ["accept-encoding", "gzip, deflate"], ["accept-language", ""], ["accept-ranges", ""], ["accept", ""],
    ["access-control-allow-origin", ""], ["age", ""], ["allow", ""], ["authorization", ""], ["cache-control", ""],
    ["content-disposition", ""], ["content-encoding", ""], ["content-language", ""], ["content-length", ""],
    ["content-location", ""], ["content-range", ""], ["content-type", ""], ["cookie", ""], ["date", ""], ["etag", ""],
    ["expect", ""], ["expires", ""], ["from", ""], ["host", ""], ["if-match", ""], ["if-modified-since", ""],
    ["if-none-match", ""], ["if-range", ""], ["if-unmodified-since", ""], ["last-modified", ""], ["link", ""],
    ["location", ""], ["max-forwards", ""], ["proxy-authenticate", ""], ["proxy-authorization", ""], ["range", ""],
    ["referer", ""], ["refresh", ""], ["retry-after", ""], ["server", ""], ["set-cookie", ""],
    ["strict-transport-security", ""], ["transfer-encoding", ""], ["user-agent", ""], ["vary", ""],
    ["via", ""], ["www-authenticate", ""],
  ];
  const STATIC_EXACT = new Map();
  const STATIC_NAME = new Map();
  for (let i = 1; i < STATIC.length; i++) {
    STATIC_EXACT.set(`${STATIC[i][0]}\0${STATIC[i][1]}`, i);
    if (!STATIC_NAME.has(STATIC[i][0])) STATIC_NAME.set(STATIC[i][0], i);
  }

  function compressionError(message) {
    const error = new Error(message);
    error.code = "ERR_HTTP2_COMPRESSION_ERROR";
    return error;
  }

  // Header strings go out one byte per character, as nghttp2 receives them from Node.
  const bytesOf = (str) => Buffer.from(str, "latin1");

  function encodeInteger(out, value, prefixBits, first) {
    const maximum = (1 << prefixBits) - 1;
    if (value < maximum) { out.push(first | value); return; }
    out.push(first | maximum);
    value -= maximum;
    while (value >= 128) { out.push((value % 128) | 128); value = Math.floor(value / 128); }
    out.push(value);
  }

  function encodeString(out, str) {
    const bytes = bytesOf(str);
    encodeInteger(out, bytes.length, 7, 0);
    for (let i = 0; i < bytes.length; i++) out.push(bytes[i]);
  }

  const entrySize = (name, value) => name.length + value.length + 32;

  class DynamicTable {
    constructor(maxSize) {
      this.entries = [];
      this.size = 0;
      this.maxSize = maxSize;
    }
    add(name, value) {
      const size = entrySize(name, value);
      if (size > this.maxSize) { this.entries.length = 0; this.size = 0; return; }
      this.entries.unshift([name, value]);
      this.size += size;
      this.evict();
    }
    evict() {
      while (this.size > this.maxSize) {
        const [name, value] = this.entries.pop();
        this.size -= entrySize(name, value);
      }
    }
    resize(maxSize) {
      this.maxSize = maxSize;
      this.evict();
    }
  }

  class Encoder {
    constructor(maxTableSize = 4096) {
      this.table = new DynamicTable(maxTableSize);
      this.limit = maxTableSize;
      this.pendingResize = null;
    }
    // The peer's SETTINGS_HEADER_TABLE_SIZE: the next block starts with a size update.
    setLimit(size) {
      this.limit = size;
      if (size < this.table.maxSize || this.pendingResize !== null) this.pendingResize = Math.min(size, this.pendingResize ?? size);
      if (this.table.maxSize > size) this.table.resize(size);
    }
    encode(headers) {
      const out = [];
      if (this.pendingResize !== null) {
        encodeInteger(out, this.pendingResize, 5, 0x20);
        this.table.resize(this.pendingResize);
        this.pendingResize = null;
      }
      for (const header of headers) {
        const name = header[0], value = header[1];
        if (header[2] || name === "authorization" || (name === "cookie" && value.length < 20)) {
          this.literal(out, name, value, 0x10, 4);
          continue;
        }
        const key = `${name}\0${value}`;
        const exact = STATIC_EXACT.get(key);
        if (exact !== undefined) { encodeInteger(out, exact, 7, 0x80); continue; }
        let dynamicIndex = 0;
        const entries = this.table.entries;
        for (let i = 0; i < entries.length; i++) {
          if (entries[i][0] === name && entries[i][1] === value) { dynamicIndex = i + 1; break; }
        }
        if (dynamicIndex !== 0) { encodeInteger(out, STATIC.length - 1 + dynamicIndex, 7, 0x80); continue; }
        if (value.length > 0xff || name === "content-length" || name === ":path" && value.length > 32) {
          this.literal(out, name, value, 0x00, 4);
        } else {
          this.literal(out, name, value, 0x40, 6);
          this.table.add(name, value);
        }
      }
      return Buffer.from(out);
    }
    literal(out, name, value, first, prefix) {
      let index = STATIC_NAME.get(name) ?? 0;
      if (index === 0) {
        const entries = this.table.entries;
        for (let i = 0; i < entries.length; i++) if (entries[i][0] === name) { index = STATIC.length + i; break; }
      }
      encodeInteger(out, index, prefix, first);
      if (index === 0) encodeString(out, name);
      encodeString(out, value);
    }
  }

  function decodeInteger(bytes, offset, prefixBits) {
    const maximum = (1 << prefixBits) - 1;
    let value = bytes[offset] & maximum;
    offset++;
    if (value < maximum) return [value, offset];
    let shift = 0;
    for (; offset < bytes.length; offset++) {
      const byte = bytes[offset];
      value += (byte & 127) * Math.pow(2, shift);
      if (!(byte & 128)) return [value, offset + 1];
      shift += 7;
      if (shift > 35) throw compressionError("HPACK integer overflow");
    }
    throw compressionError("truncated HPACK integer");
  }

  function decodeString(bytes, offset) {
    if (offset >= bytes.length) throw compressionError("truncated HPACK string");
    const compressed = (bytes[offset] & 0x80) !== 0;
    const [length, start] = decodeInteger(bytes, offset, 7);
    if (start + length > bytes.length) throw compressionError("truncated HPACK string");
    const raw = bytes.subarray(start, start + length);
    const value = compressed ? huffman.decode(raw) : raw;
    return [Buffer.from(value.buffer, value.byteOffset, value.length).toString("latin1"), start + length];
  }

  class Decoder {
    constructor(maxTableSize = 4096) {
      this.table = new DynamicTable(maxTableSize);
      this.limit = maxTableSize;
    }
    setLimit(size) { this.limit = size; }
    entry(index) {
      if (index === 0) throw compressionError("invalid HPACK index 0");
      if (index < STATIC.length) return STATIC[index];
      const entry = this.table.entries[index - STATIC.length];
      if (entry === undefined) throw compressionError("invalid HPACK index");
      return entry;
    }
    decode(input) {
      const bytes = input instanceof Uint8Array ? input : Buffer.from(input);
      const headers = [];
      let offset = 0;
      let sawField = false;
      while (offset < bytes.length) {
        const first = bytes[offset];
        let decoded;
        if (first & 0x80) {
          decoded = decodeInteger(bytes, offset, 7);
          offset = decoded[1];
          const entry = this.entry(decoded[0]);
          headers.push([entry[0], entry[1], false]);
          sawField = true;
        } else if ((first & 0xe0) === 0x20) {
          if (sawField) throw compressionError("HPACK table size update after a header field");
          decoded = decodeInteger(bytes, offset, 5);
          offset = decoded[1];
          if (decoded[0] > this.limit) throw compressionError("HPACK table size update exceeds the limit");
          this.table.resize(decoded[0]);
        } else {
          const indexed = (first & 0x40) !== 0;
          const never = (first & 0xf0) === 0x10;
          decoded = decodeInteger(bytes, offset, indexed ? 6 : 4);
          offset = decoded[1];
          let name;
          if (decoded[0] !== 0) name = this.entry(decoded[0])[0];
          else { decoded = decodeString(bytes, offset); name = decoded[0]; offset = decoded[1]; }
          decoded = decodeString(bytes, offset);
          const value = decoded[0];
          offset = decoded[1];
          headers.push([name, value, never]);
          if (indexed) this.table.add(name, value);
          sawField = true;
        }
      }
      return headers;
    }
  }

  Object.defineProperty(globalThis, "__lumenHttp2Codec", { value: { Encoder, Decoder }, configurable: true });
}
