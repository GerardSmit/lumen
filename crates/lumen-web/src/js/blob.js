// Blob / File / FormData. Byte containers over the buffered-body model: a Blob normalizes its
// parts to one Uint8Array at construction. FormData serializes to multipart/form-data (see
// encodeFormData), consumed by the fetch body path. Loaded before fetch.js so its bodyInit
// handling can reference these.

const kBlobBytes = Symbol("blobBytes");
const kBlobType = Symbol("blobType");
const kBlobFile = Symbol("blobFile");
const kFileChunk = 65536;

// Clamp a Blob.slice() index (negative = from the end), per spec.
function clampSlice(value, size) {
  if (value === undefined) return undefined;
  value = Math.trunc(Number(value)) || 0;
  return value < 0 ? Math.max(size + value, 0) : Math.min(value, size);
}

function partToBytes(part) {
  if (part instanceof Blob) return part[kBlobBytes];
  if (typeof part === "string") return new TextEncoder().encode(part);
  if (part instanceof ArrayBuffer) return new Uint8Array(part);
  if (ArrayBuffer.isView(part)) return new Uint8Array(part.buffer, part.byteOffset, part.byteLength);
  return new TextEncoder().encode(String(part));
}

class Blob {
  constructor(parts = [], options = {}) {
    if (parts != null && (typeof parts === "string" || typeof parts[Symbol.iterator] !== "function")) {
      throw new TypeError("Blob parts must be an iterable (e.g. an array)");
    }
    const chunks = [];
    let size = 0;
    let shareable = false;
    for (const part of parts ?? []) {
      const bytes = partToBytes(part);
      shareable = part instanceof Blob || typeof part === "string";
      chunks.push(bytes);
      size += bytes.length;
    }
    // Blob and string parts are immutable or freshly encoded, so a lone one is shared.
    const sole = chunks.length === 1 && shareable;
    let all;
    if (sole) {
      all = chunks[0];
    } else {
      all = new Uint8Array(size);
      let off = 0;
      for (const c of chunks) {
        all.set(c, off);
        off += c.length;
      }
    }
    this[kBlobBytes] = all;
    const type = options && options.type ? String(options.type) : "";
    // A Blob's type is lowercased; a value with out-of-range chars is dropped to "".
    this[kBlobType] = /[^ -~]/.test(type) ? "" : type.toLowerCase();
  }
  get size() {
    return this[kBlobFile] ? this[kBlobFile].size : this[kBlobBytes].length;
  }
  get type() {
    return this[kBlobType];
  }
  slice(start, end, contentType) {
    const file = this[kBlobFile];
    const size = this.size;
    const s = clampSlice(start, size) ?? 0;
    const e = Math.max(s, clampSlice(end, size) ?? size);
    const b = new Blob([], { type: contentType });
    if (file) {
      makeFileBacked(b, { size: e - s, read: (from, to) => file.read(s + from, s + to) });
    } else {
      b[kBlobBytes] = this[kBlobBytes].subarray(s, e);
    }
    return b;
  }
  async text() {
    return new TextDecoder().decode(this[kBlobBytes]);
  }
  async arrayBuffer() {
    const b = this[kBlobBytes];
    return b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength);
  }
  async bytes() {
    return this[kBlobBytes].slice();
  }
  stream() {
    const file = this[kBlobFile];
    if (file) {
      let offset = 0;
      return new globalThis.ReadableStream({
        pull(controller) {
          try {
            if (offset >= file.size) {
              controller.close();
              return;
            }
            const end = Math.min(offset + kFileChunk, file.size);
            const chunk = file.read(offset, end);
            offset = end;
            controller.enqueue(chunk);
          } catch (e) {
            controller.error(e);
          }
        },
      });
    }
    const bytes = this[kBlobBytes];
    return new globalThis.ReadableStream({
      start(controller) {
        if (bytes.length) controller.enqueue(bytes.slice());
        controller.close();
      },
    });
  }
  get [Symbol.toStringTag]() {
    return "Blob";
  }
}

// Typed browser-host bridge. It is intentionally an internal symbol rather
// than a public Blob method; URL.createObjectURL snapshots these immutable
// bytes into the per-interpreter managed-resource registry.
Object.defineProperty(Blob, Symbol.for("lumen.blob.internals"), {
  value: Object.freeze({
    snapshot(blob) {
      if (!(blob instanceof Blob)) throw new TypeError("expected Blob");
      const bytes = blob[kBlobBytes];
      return {
        bytes: bytes.slice(),
        type: blob[kBlobType],
      };
    },
  }),
});

// A Blob whose bytes stay on disk: `read(from, to)` fetches a range and throws
// NotReadableError once the file no longer matches what was opened.
function makeFileBacked(blob, source) {
  blob[kBlobFile] = source;
  Object.defineProperty(blob, kBlobBytes, { get: () => source.read(0, source.size), configurable: true });
  return blob;
}

Object.defineProperty(Blob, Symbol.for("lumen.fileBlob"), {
  value(size, type, read) {
    return makeFileBacked(new Blob([], { type }), { size, read });
  },
});

const kFileName = Symbol("fileName");
const kFileLastMod = Symbol("fileLastModified");

class File extends Blob {
  constructor(parts, name, options = {}) {
    if (arguments.length < 2) throw new TypeError("File requires fileBits and fileName");
    super(parts, options);
    this[kFileName] = String(name);
    this[kFileLastMod] =
      options && options.lastModified !== undefined ? Number(options.lastModified) : Date.now();
  }
  get name() {
    return this[kFileName];
  }
  get lastModified() {
    return this[kFileLastMod];
  }
  get [Symbol.toStringTag]() {
    return "File";
  }
}

const kEntries = Symbol("formEntries");

// A FormData entry value is either a string or a File (a Blob value is wrapped in a File).
function toEntryValue(value, filename) {
  if (value instanceof Blob) {
    if (filename !== undefined) return new File([value], String(filename), { type: value.type });
    if (value instanceof File) return value;
    return new File([value], "blob", { type: value.type });
  }
  if (filename !== undefined) {
    throw new TypeError("FormData: a filename is only valid with a Blob/File value");
  }
  return String(value);
}

class FormData {
  #formDataBrand;
  static {
    const hasFormDataBrand = value => {
      if ((typeof value !== 'object' || value === null) && typeof value !== 'function') return false;
      return #formDataBrand in value;
    };
    Object.defineProperty(globalThis, "__lumenIsFormData", {
      configurable: false,
      enumerable: false,
      writable: false,
      value: hasFormDataBrand,
    });
  }
  constructor(...args) {
    this[kEntries] = [];
    const [form, submitter] = args;
    if (form !== undefined) {
      if (typeof globalThis.__lumenPopulateFormData !== 'function') {
        throw new TypeError('FormData(form) requires a DOM form');
      }
      globalThis.__lumenPopulateFormData(this, form, submitter);
    }
  }
  append(name, value, filename) {
    this[kEntries].push([String(name), toEntryValue(value, filename)]);
  }
  set(name, value, filename) {
    name = String(name);
    const v = toEntryValue(value, filename);
    const out = [];
    let done = false;
    for (const [n, val] of this[kEntries]) {
      if (n === name) {
        if (!done) {
          out.push([name, v]);
          done = true;
        }
      } else {
        out.push([n, val]);
      }
    }
    if (!done) out.push([name, v]);
    this[kEntries] = out;
  }
  get(name) {
    name = String(name);
    const e = this[kEntries].find(([n]) => n === name);
    return e ? e[1] : null;
  }
  getAll(name) {
    name = String(name);
    return this[kEntries].filter(([n]) => n === name).map(([, v]) => v);
  }
  has(name) {
    name = String(name);
    return this[kEntries].some(([n]) => n === name);
  }
  delete(name) {
    name = String(name);
    this[kEntries] = this[kEntries].filter(([n]) => n !== name);
  }
  *entries() {
    for (const [n, v] of this[kEntries]) yield [n, v];
  }
  *keys() {
    for (const [n] of this[kEntries]) yield n;
  }
  *values() {
    for (const [, v] of this[kEntries]) yield v;
  }
  forEach(callback, thisArg) {
    for (const [n, v] of this[kEntries]) callback.call(thisArg, v, n, this);
  }
  [Symbol.iterator]() {
    return this.entries();
  }
  get [Symbol.toStringTag]() {
    return "FormData";
  }
}

// Serialize FormData to multipart/form-data bytes + the matching Content-Type (with boundary).
function encodeFormData(form) {
  const token = typeof __multipartBoundary === "function"
    ? __multipartBoundary() : crypto.randomUUID().replace(/-/g, "");
  const boundary = "----lumenFormBoundary" + token;
  const enc = new TextEncoder();
  const chunks = [];
  const push = (s) => chunks.push(typeof s === "string" ? enc.encode(s) : s);
  for (const [name, value] of form[kEntries]) {
    push(`--${boundary}\r\n`);
    const safeName = name.replace(/"/g, "%22").replace(/\r?\n/g, "%0A");
    if (value instanceof Blob) {
      const filename = (value instanceof File ? value.name : "blob").replace(/"/g, "%22");
      push(`Content-Disposition: form-data; name="${safeName}"; filename="${filename}"\r\n`);
      push(`Content-Type: ${value.type || "application/octet-stream"}\r\n\r\n`);
      push(value[kBlobBytes]);
      push("\r\n");
    } else {
      push(`Content-Disposition: form-data; name="${safeName}"\r\n\r\n`);
      push(`${value}\r\n`);
    }
  }
  push(`--${boundary}--\r\n`);
  let size = 0;
  for (const c of chunks) size += c.length;
  const out = new Uint8Array(size);
  let off = 0;
  for (const c of chunks) {
    out.set(c, off);
    off += c.length;
  }
  return { bytes: out, contentType: `multipart/form-data; boundary=${boundary}` };
}

// Byte-level substring search (multipart bodies are binary, so we can't decode-then-split).
function indexOfBytes(haystack, needle, from) {
  outer: for (let i = from; i <= haystack.length - needle.length; i++) {
    for (let j = 0; j < needle.length; j++) {
      if (haystack[i + j] !== needle[j]) continue outer;
    }
    return i;
  }
  return -1;
}

// Parse multipart/form-data bytes back into a FormData (the inverse of encodeFormData; also parses
// bodies produced by other clients). Used by Request/Response.formData().
function decodeMultipart(bytes, boundary) {
  const fd = new FormData();
  const enc = new TextEncoder();
  const dec = new TextDecoder();
  const marker = enc.encode(`--${boundary}`);
  const headerSep = enc.encode("\r\n\r\n");
  let pos = indexOfBytes(bytes, marker, 0);
  while (pos !== -1) {
    let start = pos + marker.length;
    if (bytes[start] === 0x2d && bytes[start + 1] === 0x2d) break; // closing "--boundary--"
    if (bytes[start] === 0x0d && bytes[start + 1] === 0x0a) start += 2; // skip CRLF after boundary
    const headerEnd = indexOfBytes(bytes, headerSep, start);
    if (headerEnd === -1) break;
    const headerText = dec.decode(bytes.subarray(start, headerEnd));
    const bodyStart = headerEnd + 4;
    const next = indexOfBytes(bytes, marker, bodyStart);
    if (next === -1) break;
    const body = bytes.subarray(bodyStart, next - 2); // drop the CRLF before the next boundary

    let name = null;
    let filename = null;
    let ctype = "";
    for (const line of headerText.split("\r\n")) {
      if (/^content-disposition:/i.test(line)) {
        const nm = /name="([^"]*)"/i.exec(line);
        if (nm) name = nm[1].replace(/%22/g, '"').replace(/%0A/g, "\n");
        const fn = /filename="([^"]*)"/i.exec(line);
        if (fn) filename = fn[1].replace(/%22/g, '"');
      } else if (/^content-type:/i.test(line)) {
        ctype = line.slice(line.indexOf(":") + 1).trim();
      }
    }
    if (name !== null) {
      if (filename !== null) fd.append(name, new File([body.slice()], filename, { type: ctype }));
      else fd.append(name, dec.decode(body));
    }
    pos = next;
  }
  return fd;
}

globalThis.Blob = Blob;
globalThis.File = File;
globalThis.FormData = FormData;

// Private native bridge used by the browser's HTML form-navigation service.
// It snapshots the actual post-`formdata` entry list, including File bytes,
// without exposing or replacing FormData's private entry storage.
Object.defineProperty(globalThis, "__lumenSnapshotFormData", {
  configurable: false,
  enumerable: false,
  writable: false,
  value(form) {
    if (!globalThis.__lumenIsFormData(form)) throw new TypeError("expected FormData");
    return form[kEntries].map(([name, value]) => {
      if (value instanceof Blob) {
        const file = value instanceof File ? value : new File([value], "blob", { type: value.type });
        return {
          name,
          kind: "file",
          fileName: file.name,
          type: file.type,
          lastModified: file.lastModified,
          bytes: Array.from(file[kBlobBytes]),
        };
      }
      return { name, kind: "text", value: String(value) };
    });
  },
});
