// Built-in stand-ins for optional native addons that popular packages try to `require` (ws loads
// `bufferutil` for frame masking and `utf-8-validate` for text frames). Resolution is
// node_modules first: an installed package wins, exactly as in Node, and these are used only
// when the package is absent (see `isFallbackModule` in module.js / native.rs). They are not core
// modules: not in `builtinModules`, and `require('node:bufferutil')` fails as it does in Node.

// A WebSocket mask as one little-endian word: byte i of the key is `key >>> 8 * (i % 4)`.
function maskWord(mask) {
  return (mask[0] | (mask[1] << 8) | (mask[2] << 16) | (mask[3] << 24)) >>> 0;
}

function mask(source, mask, output, offset, length) {
  if (source.buffer === output.buffer) {
    // Same backing store (possibly overlapping): the byte loop has the addon's semantics.
    for (let i = 0; i < length; i++) output[offset + i] = source[i] ^ mask[i & 3];
    return;
  }
  __native.mask(source, maskWord(mask), output, offset, length);
}

function unmask(buffer, mask) {
  __native.unmask(buffer, maskWord(mask));
}

__builtins.set("bufferutil", { mask, unmask });

function isValidUTF8(buf) {
  return __native.isUtf8(buf);
}
isValidUTF8.isValidUTF8 = isValidUTF8;
__builtins.set("utf-8-validate", isValidUTF8);
