
// ---- registration ------------------------------------------------------------------------------

const web = require("stream/web");
const { readableStreamTee } = require("internal/webstreams/readablestream");
// Fetch's body clone uses the same stream implementation, with copied chunks on
// the second branch as required by Fetch (ordinary ReadableStream.tee shares them).
Object.defineProperty(web.ReadableStream.prototype, Symbol.for("lumen.cloneBody"), {
  value() { return readableStreamTee(this, true); },
  writable: true,
  enumerable: false,
  configurable: true,
});
__builtins.set("stream/web", web);

// The globals, as Node's lazy interfaces leave them once loaded: plain, non-enumerable data
// properties. A value a program assigned before the first touch stays.
function exposed(name) {
  const now = Object.getOwnPropertyDescriptor(globalThis, name);
  if (now !== undefined && now.get === undefined) return now;
  return { value: web[name], writable: true, enumerable: false, configurable: true };
}
Object.defineProperty(globalThis, "ReadableStream", exposed("ReadableStream"));
Object.defineProperty(globalThis, "ReadableStreamDefaultReader", exposed("ReadableStreamDefaultReader"));
Object.defineProperty(globalThis, "ReadableStreamBYOBReader", exposed("ReadableStreamBYOBReader"));
Object.defineProperty(globalThis, "ReadableStreamBYOBRequest", exposed("ReadableStreamBYOBRequest"));
Object.defineProperty(globalThis, "ReadableByteStreamController", exposed("ReadableByteStreamController"));
Object.defineProperty(globalThis, "ReadableStreamDefaultController", exposed("ReadableStreamDefaultController"));
Object.defineProperty(globalThis, "TransformStream", exposed("TransformStream"));
Object.defineProperty(globalThis, "TransformStreamDefaultController", exposed("TransformStreamDefaultController"));
Object.defineProperty(globalThis, "WritableStream", exposed("WritableStream"));
Object.defineProperty(globalThis, "WritableStreamDefaultWriter", exposed("WritableStreamDefaultWriter"));
Object.defineProperty(globalThis, "WritableStreamDefaultController", exposed("WritableStreamDefaultController"));
Object.defineProperty(globalThis, "ByteLengthQueuingStrategy", exposed("ByteLengthQueuingStrategy"));
Object.defineProperty(globalThis, "CountQueuingStrategy", exposed("CountQueuingStrategy"));
Object.defineProperty(globalThis, "TextEncoderStream", exposed("TextEncoderStream"));
Object.defineProperty(globalThis, "TextDecoderStream", exposed("TextDecoderStream"));
Object.defineProperty(globalThis, "CompressionStream", exposed("CompressionStream"));
Object.defineProperty(globalThis, "DecompressionStream", exposed("DecompressionStream"));
// The module table, for --expose-internals (internals.js).
__internals.set("webstreamsRequire", require);
