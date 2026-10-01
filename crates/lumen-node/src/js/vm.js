// ---- node:vm ----------------------------------------------------------------------------------
// Contexts are real: `createContext` makes a realm of its own (fresh intrinsics) whose global
// object is fronted by a global proxy intercepting onto the sandbox, like Node's contextify
// (see lumen's `builtins/vm_context.rs`). This file validates arguments the way Node's lib/vm.js
// does and keeps the sandbox -> global proxy map.
{
  const {
    validateString, validateInt32, validateUint32, validateBoolean, validateObject, validateArray,
    validateOneOf, kValidateObjectAllowArray,
  } = __validators;

  // sandbox -> its context's global proxy (an ephemeron: the proxy references the sandbox).
  const contexts = new WeakMap();
  let contextCounter = 0;

  const isArrayBufferView = (v) => ArrayBuffer.isView(v);

  const isContext = (object) => {
    validateObject(object, "object", kValidateObjectAllowArray);
    return contexts.has(object);
  };

  const globalProxyOf = (contextifiedObject) => {
    validateObject(contextifiedObject, "contextifiedObject", kValidateObjectAllowArray);
    const proxy = contexts.get(contextifiedObject);
    if (proxy === undefined) {
      throw new __errors.ERR_INVALID_ARG_TYPE("contextifiedObject", "vm.Context", contextifiedObject);
    }
    return proxy;
  };

  const validateCodeGeneration = (codeGeneration, name) => {
    if (codeGeneration === undefined) return;
    validateObject(codeGeneration, name);
    const { strings, wasm } = codeGeneration;
    if (strings !== undefined) validateBoolean(strings, `${name}.strings`);
    if (wasm !== undefined) validateBoolean(wasm, `${name}.wasm`);
  };

  const createContext = (contextObject = {}, options = {}) => {
    if (isContext(contextObject)) return contextObject;
    validateObject(options, "options");
    const {
      name = `VM Context ${++contextCounter}`,
      origin,
      codeGeneration,
      microtaskMode,
      importModuleDynamically,
    } = options;
    validateString(name, "options.name");
    if (origin !== undefined) validateString(origin, "options.origin");
    validateCodeGeneration(codeGeneration, "options.codeGeneration");
    validateOneOf(microtaskMode, "options.microtaskMode", ["afterEvaluate", undefined]);
    if (importModuleDynamically !== undefined && typeof importModuleDynamically !== "function") {
      throw new __errors.ERR_INVALID_ARG_TYPE("options.importModuleDynamically", "function", importModuleDynamically);
    }
    const proxy = __vmc.createContext(contextObject);
    contexts.set(contextObject, proxy);
    return contextObject;
  };

  // `{ timeout, displayErrors, breakOnSigint }` of the run* methods (Node's getRunInContextArgs).
  const runArgs = (options = {}) => {
    validateObject(options, "options");
    let timeout = options.timeout;
    if (timeout === undefined) {
      timeout = -1;
    } else {
      validateUint32(timeout, "options.timeout", true);
    }
    const { displayErrors = true, breakOnSigint = false } = options;
    validateBoolean(displayErrors, "options.displayErrors");
    validateBoolean(breakOnSigint, "options.breakOnSigint");
    return { timeout, displayErrors, breakOnSigint };
  };

  // `runInNewContext`'s context options (Node's getContextOptions).
  const contextOptions = (options) => {
    if (!options) return {};
    const contextOptions = {
      name: options.contextName,
      origin: options.contextOrigin,
      codeGeneration: undefined,
      microtaskMode: options.microtaskMode,
    };
    if (contextOptions.name !== undefined) validateString(contextOptions.name, "options.contextName");
    if (contextOptions.origin !== undefined) validateString(contextOptions.origin, "options.contextOrigin");
    if (options.contextCodeGeneration !== undefined) {
      validateCodeGeneration(options.contextCodeGeneration, "options.contextCodeGeneration");
      contextOptions.codeGeneration = options.contextCodeGeneration;
    }
    return contextOptions;
  };

  const runBounded = (args, run) =>
    args.timeout === -1 ? run() : __vm.runWithTimeout(args.timeout, run);

  // A code cache stand-in: lumen compiles from source, so the "cache" records only what V8's
  // sanity check does (a magic, the source length and hash) and is accepted iff it matches.
  const cacheMagic = 0x6c6d6331;
  const sourceHash = (source) => {
    let h = 0x811c9dc5;
    for (let i = 0; i < source.length; i++) {
      h ^= source.charCodeAt(i);
      h = Math.imul(h, 0x01000193) >>> 0;
    }
    return h;
  };
  const makeCache = (source) => {
    const buf = Buffer.alloc(12);
    buf.writeUInt32LE(cacheMagic, 0);
    buf.writeUInt32LE(source.length >>> 0, 4);
    buf.writeUInt32LE(sourceHash(source), 8);
    return buf;
  };
  const cacheMatches = (view, source) => {
    if (view.byteLength !== 12) return false;
    const dv = new DataView(view.buffer, view.byteOffset, view.byteLength);
    return dv.getUint32(0, true) === cacheMagic &&
      dv.getUint32(4, true) === (source.length >>> 0) &&
      dv.getUint32(8, true) === sourceHash(source);
  };

  // The last `//# sourceMappingURL=` magic comment, as V8 reports it.
  const sourceMapURLOf = (source) => {
    const re = /\/\/[#@][ \t]+sourceMappingURL=[ \t]*([^\s'"]*)[ \t]*$/gm;
    let url;
    for (let m = re.exec(source); m !== null; m = re.exec(source)) url = m[1];
    return url;
  };

  const kSource = Symbol("kSource");
  const kFilename = Symbol("kFilename");
  const kLineOffset = Symbol("kLineOffset");
  const kColumnOffset = Symbol("kColumnOffset");

  class Script {
    constructor(code, options = {}) {
      code = `${code}`;
      if (typeof options === "string") {
        options = { filename: options };
      } else {
        validateObject(options, "options");
      }
      const {
        filename = "evalmachine.<anonymous>",
        lineOffset = 0,
        columnOffset = 0,
        cachedData,
        produceCachedData = false,
        importModuleDynamically,
      } = options;
      validateString(filename, "options.filename");
      validateInt32(lineOffset, "options.lineOffset");
      validateInt32(columnOffset, "options.columnOffset");
      if (cachedData !== undefined && !isArrayBufferView(cachedData)) {
        throw new __errors.ERR_INVALID_ARG_TYPE("options.cachedData", ["Buffer", "TypedArray", "DataView"], cachedData);
      }
      validateBoolean(produceCachedData, "options.produceCachedData");
      if (importModuleDynamically !== undefined && typeof importModuleDynamically !== "function") {
        throw new __errors.ERR_INVALID_ARG_TYPE("options.importModuleDynamically", "function", importModuleDynamically);
      }
      __vmc.compileScript(code, filename, lineOffset, columnOffset);
      this[kSource] = code;
      this[kFilename] = filename;
      this[kLineOffset] = lineOffset;
      this[kColumnOffset] = columnOffset;
      if (cachedData !== undefined) this.cachedDataRejected = !cacheMatches(cachedData, code);
      if (produceCachedData) {
        this.cachedDataProduced = true;
        this.cachedData = makeCache(code);
      }
      this.sourceMapURL = sourceMapURLOf(code);
    }

    createCachedData() {
      return makeCache(this[kSource]);
    }

    runInThisContext(options) {
      const args = runArgs(options);
      return runScript(this, null, args);
    }

    runInContext(contextifiedObject, options) {
      const proxy = globalProxyOf(contextifiedObject);
      const args = runArgs(options);
      return runScript(this, proxy, args);
    }

    runInNewContext(contextObject, options) {
      const context = createContext(contextObject, contextOptions(options));
      return this.runInContext(context, options);
    }
  }

  const runScript = (script, proxy, args) =>
    runBounded(args, () =>
      __vmc.runScript(proxy, script[kSource], script[kFilename], script[kLineOffset], script[kColumnOffset],
        args.displayErrors));

  const createScript = (code, options) => new Script(code, options);

  const runInContext = (code, contextifiedObject, options) => {
    globalProxyOf(contextifiedObject);
    if (typeof options === "string") options = { filename: options };
    return createScript(code, options).runInContext(contextifiedObject, options);
  };

  const runInNewContext = (code, contextObject, options) => {
    if (typeof options === "string") options = { filename: options };
    contextObject = createContext(contextObject, contextOptions(options));
    return createScript(code, options).runInContext(contextObject, options);
  };

  const runInThisContext = (code, options) => {
    if (typeof options === "string") options = { filename: options };
    return createScript(code, options).runInThisContext(options);
  };

  const compileFunction = (code, params, options = {}) => {
    validateString(code, "code");
    if (params !== undefined) validateArray(params, "params");
    validateObject(options, "options");
    const {
      filename = "",
      columnOffset = 0,
      lineOffset = 0,
      cachedData = undefined,
      produceCachedData = false,
      parsingContext = undefined,
      contextExtensions = [],
      importModuleDynamically,
    } = options;
    validateString(filename, "options.filename");
    validateInt32(columnOffset, "options.columnOffset");
    validateInt32(lineOffset, "options.lineOffset");
    if (cachedData !== undefined && !isArrayBufferView(cachedData)) {
      throw new __errors.ERR_INVALID_ARG_TYPE("options.cachedData", ["Buffer", "TypedArray", "DataView"], cachedData);
    }
    validateBoolean(produceCachedData, "options.produceCachedData");
    let proxy = null;
    if (parsingContext !== undefined) {
      if (typeof parsingContext !== "object" || parsingContext === null || !isContext(parsingContext)) {
        throw new __errors.ERR_INVALID_ARG_TYPE("options.parsingContext", "Context", parsingContext);
      }
      proxy = contexts.get(parsingContext);
    }
    validateArray(contextExtensions, "options.contextExtensions");
    contextExtensions.forEach((extension, i) => {
      const name = `options.contextExtensions[${i}]`;
      validateObject(extension, name, __validators.kValidateObjectAllowNullable);
    });
    if (importModuleDynamically !== undefined && typeof importModuleDynamically !== "function") {
      throw new __errors.ERR_INVALID_ARG_TYPE("options.importModuleDynamically", "function", importModuleDynamically);
    }
    const paramList = params === undefined ? [] : params;
    paramList.forEach((p, i) => validateString(p, `params[${i}]`));
    const fn = __vmc.compileFunction(proxy, code, paramList, contextExtensions, filename, lineOffset, columnOffset);
    const source = `function (${paramList.join(", ")}) {\n${code}\n}`;
    if (produceCachedData) {
      fn.cachedDataProduced = true;
      fn.cachedData = makeCache(source);
    }
    if (cachedData !== undefined) fn.cachedDataRejected = !cacheMatches(cachedData, source);
    return fn;
  };

  const measureMemory = (options = {}) => {
    validateObject(options, "options");
    const { mode = "summary", execution = "default" } = options;
    validateOneOf(mode, "options.mode", ["summary", "detailed"]);
    validateOneOf(execution, "options.execution", ["default", "eager"]);
    const used = process.memoryUsage().heapUsed;
    const estimate = { jsMemoryEstimate: used, jsMemoryRange: [used, used] };
    const result = { total: estimate };
    if (mode === "detailed") {
      result.current = estimate;
      result.other = [];
    }
    return Promise.resolve(result);
  };

  const constants = {
    __proto__: null,
    USE_MAIN_CONTEXT_DEFAULT_LOADER: Symbol("vm_dynamic_import_main_context_default"),
    DONT_CONTEXTIFY: Symbol("vm_context_no_contextify"),
  };

  __builtins.set("vm", {
    Script,
    compileFunction,
    constants,
    createContext,
    createScript,
    isContext,
    measureMemory,
    runInContext,
    runInNewContext,
    runInThisContext,
  });
}
