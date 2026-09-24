// node:module stripTypeScriptTypes(): Node's strip-only TypeScript erasure, implemented natively
// by the engine parser's TypeScript mode (`__node.stripTypes`). Erased syntax becomes whitespace of the same UTF-8
// and UTF-16 length, so every offset, line and column of the output matches the input. Syntax
// that needs emitted JavaScript (enum, instantiated namespace, parameter properties, ...)
// throws ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX; unparsable input ERR_INVALID_TYPESCRIPT_SYNTAX.
{
  const argError = (code, message) => {
    const e = new TypeError(message);
    e.code = code;
    return e;
  };
  const show = (v) => (typeof v === "string" ? `'${v}'` : String(v));

  function stripTypeScriptTypes(code, options = {}) {
    if (typeof code !== "string") {
      const got = code === null ? "null" : `type ${typeof code} (${String(code)})`;
      throw argError("ERR_INVALID_ARG_TYPE", `The "code" argument must be of type string. Received ${got}`);
    }
    if (options === null || typeof options !== "object") {
      throw argError("ERR_INVALID_ARG_TYPE", `The "options" argument must be of type object. Received ${options === null ? "null" : typeof options}`);
    }
    const { mode = "strip", sourceMap = false, sourceUrl = "" } = options;
    if (mode !== "strip" && mode !== "transform") {
      throw argError("ERR_INVALID_ARG_VALUE", `The property 'options.mode' must be one of: 'strip', 'transform'. Received ${show(mode)}`);
    }
    if (mode === "strip" && sourceMap !== false && sourceMap !== undefined) {
      throw argError("ERR_INVALID_ARG_VALUE", `The property 'options.sourceMap' must be one of: false, undefined. Received ${show(sourceMap)}`);
    }
    if (mode === "transform") {
      // lumen has no TypeScript emitter: erasable syntax is stripped (offsets kept), and
      // syntax that needs transformation is rejected as in strip mode.
      if (sourceMap) throw argError("ERR_INVALID_ARG_VALUE", "The property 'options.sourceMap' is not supported by lumen");
    }
    let result = __node.stripTypes(code);
    if (sourceUrl) result += `\n\n//# sourceURL=${sourceUrl}`;
    return result;
  }

  Object.defineProperty(globalThis, "__lumenStripTypeScriptTypes", {
    value: stripTypeScriptTypes, configurable: true,
  });
}
