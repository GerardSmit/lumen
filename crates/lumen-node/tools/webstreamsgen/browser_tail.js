// Browser subset installation: algorithms and constructors come from the same Node source
// modules; Node-only CompressionStream adapters are intentionally not published without a real
// host zlib backend.
const web = require('stream/web');
const { readableStreamTee } = require('internal/webstreams/readablestream');
Object.defineProperty(web.ReadableStream.prototype, Symbol.for('lumen.cloneBody'), {
  value() { return readableStreamTee(this, true); },
  writable: true,
  enumerable: false,
  configurable: true,
});
__builtins.set('stream/web', web);

function exposed(name) {
  const now = Object.getOwnPropertyDescriptor(globalThis, name);
  if (now !== undefined && now.get === undefined) return now;
  return { value: web[name], writable: true, enumerable: false, configurable: true };
}
for (const name of [
  'ReadableStream', 'ReadableStreamDefaultReader', 'ReadableStreamBYOBReader',
  'ReadableStreamBYOBRequest', 'ReadableByteStreamController',
  'ReadableStreamDefaultController', 'WritableStream', 'WritableStreamDefaultWriter',
  'WritableStreamDefaultController', 'TransformStream', 'TransformStreamDefaultController',
  'ByteLengthQueuingStrategy', 'CountQueuingStrategy', 'TextEncoderStream', 'TextDecoderStream',
]) Object.defineProperty(globalThis, name, exposed(name));
