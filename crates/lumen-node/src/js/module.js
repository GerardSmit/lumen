// CommonJS require() with node_modules resolution and the module wrapper.
//
// Resolution follows Node's algorithm (the practical core of it): core modules, then
// relative/absolute via LOAD_AS_FILE + LOAD_AS_DIRECTORY, then the node_modules walk.
// package.json "main" and a subset of "exports" are honored. The module body runs inside a
// `new Function(exports, require, module, __filename, __dirname)` wrapper, exactly as Node's
// does.

// The loader's posix path needs are native, so `node:path` loads only when a program asks for it.
const path = __os.info().platform === "win32" ? __builtins.get("path") : {
  delimiter: ":",
  sep: "/",
  toNamespacedPath: (p) => p,
  isAbsolute: (p) => String(p).startsWith("/"),
  resolve: __node.pathResolve,
  join: __node.pathJoin,
  dirname: __node.pathDirname,
  basename: __node.pathBasename,
  extname: __node.pathExtname,
};
const CORE = new Set([...__builtins.keys()]);
// resolved filename -> module. A plain null-prototype object, exposed live as `require.cache`
// and `Module._cache`, so `delete require.cache[file]` forces the next require to re-run it.
const cache = Object.create(null);

function isCoreSpecifier(spec) {
  if (spec === "bun" || spec.startsWith("bun:")) return CORE.has(spec) ? spec : null;
  const bare = spec.startsWith("node:") ? spec.slice(5) : spec;
  // Internal modules are only reachable under --expose-internals (internals.js provides them).
  if (bare.startsWith("internal/")) return exposedInternal(bare) ? bare : null;
  if (!spec.startsWith("node:") && SCHEME_ONLY.has(bare)) return null;
  return CORE.has(bare) ? bare : null;
}

function exposedInternal(id) {
  return !!process[Symbol.for("lumen.options")]?.["--expose-internals"] && __internals.get("exposedInternals").has(id);
}

function isRelativeSpecifier(specifier) {
  return specifier === "." || specifier === ".." || specifier.startsWith("./") || specifier.startsWith("../");
}

const hasOwn = (object, key) => Object.prototype.hasOwnProperty.call(object, key);

// stat() answers 0 for a file, 1 for a directory and a negative number for anything else, the
// way Node's internalModuleStat does. Results are memoised while one top-level require runs.
let statCache = null;
function stat(filename) {
  filename = path.toNamespacedPath(filename);
  if (statCache !== null) {
    const result = statCache.get(filename);
    if (result !== undefined) return result;
  }
  const result = __node.isDir(filename) ? 1 : __node.isFile(filename) ? 0 : -2;
  if (statCache !== null && result >= 0) statCache.set(filename, result);
  return result;
}

// NODE_PRESERVE_SYMLINKS=1 in the starting environment acts as --preserve-symlinks (read once,
// before any program code runs, as Node reads it at startup).
let envPreserveSymlinks;
const preserveSymlinks = () => {
  const options = process[Symbol.for("lumen.options")];
  if (options && options["--preserve-symlinks"]) return true;
  envPreserveSymlinks ??= process.env.NODE_PRESERVE_SYMLINKS === "1";
  return envPreserveSymlinks;
};
const preserveSymlinksMain = () => {
  const options = process[Symbol.for("lumen.options")];
  return !!(options && options["--preserve-symlinks-main"]);
};
// The stat the resolver uses: `Module._stat`, which a program may replace (Node's experimental
// hook for a virtual file system).
let _stat = stat;
const experimentalWarned = new Set();
function emitExperimentalWarning(feature) {
  if (experimentalWarned.has(feature)) return;
  experimentalWarned.add(feature);
  process.emitWarning(`${feature} is an experimental feature and might change at any time`, "ExperimentalWarning");
}

// Node's loader reads and realpaths through fs, so a program that patched fs (a virtual file
// system) is honoured; until fs has loaded nothing can have patched it, and the native calls
// stand in.
function patchedFs(method) {
  if (__builtins.pending.has("fs")) return undefined;
  const fs = __builtins.get("fs");
  return fs[method] !== __internals.get(`fs_${method}`) ? fs : undefined;
}
function readSource(filename) {
  const fs = patchedFs("readFileSync");
  return fs !== undefined ? fs.readFileSync(filename, "utf8") : __node.readText(filename);
}
function toRealPath(requestPath) {
  const fs = patchedFs("realpathSync");
  return fs !== undefined ? fs.realpathSync(requestPath) : __node.realpath(requestPath);
}

// tryFile(path): `path` when it is a file (symlinks resolved unless preserved), else undefined.
function tryFile(requestPath, isMain) {
  if (_stat(requestPath) !== 0) return undefined;
  if (isMain ? preserveSymlinksMain() : preserveSymlinks()) return path.resolve(requestPath);
  return toRealPath(requestPath);
}

function tryExtensions(basePath, exts, isMain) {
  for (let i = 0; i < exts.length; i++) {
    const filename = tryFile(basePath + exts[i], isMain);
    if (filename) return filename;
  }
  return undefined;
}

// package.json of a directory, with the fields the resolver reads checked as own properties
// (a polluted Object.prototype must not leak a `main`/`exports`/`type` into a package).
const packageJsonCache = new Map();
function readPackage(requestPath) {
  const jsonPath = path.resolve(requestPath, "package.json");
  if (packageJsonCache.has(jsonPath)) return packageJsonCache.get(jsonPath);
  let result = false;
  if (__node.isFile(jsonPath)) {
    let parsed;
    try {
      parsed = JSON.parse(__node.readText(jsonPath).replace(/^\uFEFF/, ""));
    } catch (error) {
      error.path = jsonPath;
      error.message = "Error parsing " + jsonPath + ": " + error.message;
      throw error;
    }
    const field = (name) => (parsed !== null && typeof parsed === "object" && hasOwn(parsed, name) ? parsed[name] : undefined);
    const main = field("main");
    result = {
      jsonPath,
      main: typeof main === "string" ? main : undefined,
      exports: field("exports"),
      name: typeof field("name") === "string" ? field("name") : undefined,
      type: typeof field("type") === "string" ? field("type") : undefined,
    };
  }
  packageJsonCache.set(jsonPath, result);
  return result;
}

// Exact package exports, with declaration-ordered node/require/default conditions.
// undefined means no matching condition; null blocks a branch. Arrays skip both while
// retaining the last blocked/invalid result. Single-star subpaths use Node key precedence.
const INVALID_EXPORT_TARGET = Symbol("invalid package export target");
function relativePackageTarget(target) {
  return target.startsWith("./") && !target.split("/").slice(1).some(part => {
    const lower = part.toLowerCase(), dots = lower.replace(/%2e/g, ".");
    return dots === "." || dots === ".." || lower === "node_modules"
      || lower.includes("%2f") || lower.includes("%5c") || lower.includes("\\");
  });
}
// require()'s conditions: Node's defaults, "node-addons" unless --no-addons, and --conditions.
let cjsConditions;
function cjsConditionMatches(condition) {
  if (cjsConditions === undefined) {
    const options = process[Symbol.for("lumen.options")] || {};
    cjsConditions = new Set(["node", "require", "default"]);
    if (options["--addons"] !== false) cjsConditions.add("node-addons");
    const user = options["--conditions"];
    if (Array.isArray(user)) for (let i = 0; i < user.length; i++) cjsConditions.add(user[i]);
    else if (typeof user === "string") cjsConditions.add(user);
  }
  return cjsConditions.has(condition);
}
function resolveExportTarget(target) {
  if (typeof target === "string") {
    if (!relativePackageTarget(target)) return INVALID_EXPORT_TARGET;
    return target;
  }
  if (target === null) return null;
  if (Array.isArray(target)) {
    if (target.length === 0) return null;
    let last;
    for (const item of target) {
      const resolved = resolveExportTarget(item);
      if (resolved === undefined) continue;
      if (typeof resolved === "string") return resolved;
      last = resolved;
    }
    return last;
  }
  if (target && typeof target === "object") {
    for (const condition of Object.keys(target)) {
      if (cjsConditionMatches(condition)) {
        const resolved = resolveExportTarget(target[condition]);
        if (resolved !== undefined) return resolved;
      }
    }
    return undefined;
  }
  return INVALID_EXPORT_TARGET;
}
function resolveExports(exports, key = ".") {
  let target = exports;
  let capture;
  if (exports && typeof exports === "object" && !Array.isArray(exports)
      && Object.keys(exports).some(key => key.startsWith("."))) {
    if (Object.prototype.hasOwnProperty.call(exports, key)) target = exports[key];
    else {
      let bestPrefix = -1, bestLength = -1;
      target = undefined;
      for (const pattern of Object.keys(exports)) {
        const star = pattern.indexOf("*");
        if (star < 0 || pattern.indexOf("*", star + 1) >= 0) continue;
        const prefix = pattern.slice(0, star), suffix = pattern.slice(star + 1);
        if (key.length < prefix.length + suffix.length
            || !key.startsWith(prefix) || !key.endsWith(suffix)) continue;
        if (prefix.length < bestPrefix
            || (prefix.length === bestPrefix && pattern.length <= bestLength)) continue;
        bestPrefix = prefix.length; bestLength = pattern.length;
        capture = key.slice(prefix.length, key.length - suffix.length);
        target = exports[pattern];
      }
      if (target === undefined) return null;
    }
  } else if (key !== ".") return null;
  const resolved = resolveExportTarget(target);
  if (typeof resolved !== "string") return null;
  const mapped = capture === undefined ? resolved : resolved.split("*").join(capture);
  return relativePackageTarget(mapped) ? mapped : null;
}

function tryPackage(requestPath, exts, isMain, originalPath) {
  const pkg = readPackage(requestPath);
  if (!pkg || pkg.main === undefined || pkg.main === "") {
    return tryExtensions(path.resolve(requestPath, "index"), exts, isMain);
  }
  const filename = path.resolve(requestPath, pkg.main);
  let actual = tryFile(filename, isMain)
    || tryExtensions(filename, exts, isMain)
    || tryExtensions(path.resolve(filename, "index"), exts, isMain);
  if (actual === undefined) {
    actual = tryExtensions(path.resolve(requestPath, "index"), exts, isMain);
    if (!actual) {
      const err = new Error(
        `Cannot find module '${filename}'. Please verify that the package.json has a valid "main" entry`,
      );
      err.code = "MODULE_NOT_FOUND";
      err.path = path.resolve(requestPath, "package.json");
      err.requestPath = originalPath;
      throw err;
    }
    process.emitWarning(
      `Invalid 'main' field in '${pkg.jsonPath}' of '${pkg.main}'. Please either fix that or report it to the module author`,
      "DeprecationWarning",
      "DEP0128",
    );
  }
  return actual;
}

// A package naming itself in its own "exports" (`require("pkg/sub")` from inside `pkg`).
function trySelf(parentFilename, request) {
  if (!parentFilename) return false;
  let dir = path.dirname(parentFilename);
  let scope = false;
  for (;;) {
    if (dir.endsWith(path.sep + "node_modules")) return false;
    scope = readPackage(dir);
    if (scope) break;
    const up = path.dirname(dir);
    if (up === dir) return false;
    dir = up;
  }
  if (scope.exports === undefined || scope.exports === null || typeof scope.name !== "string") return false;
  let expansion;
  if (request === scope.name) expansion = ".";
  else if (request.startsWith(scope.name + "/")) expansion = "." + request.slice(scope.name.length);
  else return false;
  const pkgDir = path.dirname(scope.jsonPath);
  const target = resolveExports(scope.exports, expansion);
  const file = target ? tryFile(path.resolve(pkgDir, target), false) : undefined;
  if (file) return file;
  const err = new Error(
    target
      ? `Cannot find module '${path.resolve(pkgDir, target)}'`
      : expansion === "."
        ? `No "exports" main defined in ${scope.jsonPath}`
        : `Package subpath '${expansion}' is not defined by "exports" in ${scope.jsonPath}`,
  );
  err.code = target ? "MODULE_NOT_FOUND" : "ERR_PACKAGE_PATH_NOT_EXPORTED";
  throw err;
}

const EXPORTS_PATTERN = /^((?:@[^/\\%]+\/)?[^./\\%][^/\\%]*)(\/.*)?$/;

// A package's "exports" map as the CommonJS loader applies it to `request` found under the
// node_modules directory `nmPath`; undefined when the package has none.
function resolveExportsFrom(nmPath, request) {
  const match = EXPORTS_PATTERN.exec(request);
  if (!match) return undefined;
  const name = match[1];
  const expansion = match[2] || "";
  const pkgPath = path.resolve(nmPath, name);
  const pkg = readPackage(pkgPath);
  if (!pkg || pkg.exports === undefined || pkg.exports === null) return undefined;
  const target = resolveExports(pkg.exports, "." + expansion);
  const file = target ? tryFile(path.resolve(pkgPath, target), false) : undefined;
  if (file) return file;
  const err = new Error(
    target
      ? `Cannot find module '${path.resolve(pkgPath, target)}'`
      : expansion === ""
        ? `No "exports" main defined in ${pkg.jsonPath}`
        : `Package subpath '.${expansion}' is not defined by "exports" in ${pkg.jsonPath}`,
  );
  err.code = target ? "MODULE_NOT_FOUND" : "ERR_PACKAGE_PATH_NOT_EXPORTED";
  throw err;
}

const pathCacheHit = (key) => Module._pathCache[key];

// Module._findPath(request, paths, isMain): the file `request` names under the first of `paths`
// that has it — exact file, then each registered extension, then a directory's package.json
// "main" or index — or false.
function findPath(request, paths, isMain) {
  const absoluteRequest = path.isAbsolute(request);
  if (absoluteRequest) {
    paths = [""];
  } else if (!paths || paths.length === 0) {
    return false;
  }
  const cacheKey = request + "\x00" + (paths.length === 1 ? paths[0] : paths.join("\x00"));
  const entry = pathCacheHit(cacheKey);
  if (entry) return entry;

  let exts;
  const last = request.charCodeAt(request.length - 1);
  const trailingSlash = request.length > 0 && (last === 47 || (last === 46 && (
    request.length === 1 || request.endsWith("/.") || request.endsWith("/..") || request === ".."
  )));

  for (let i = 0; i < paths.length; i++) {
    const curPath = paths[i];
    if (curPath && !isRelativeSpecifier(request) && _stat(curPath) < 1) continue;
    if (!absoluteRequest) {
      const exportsResolved = resolveExportsFrom(curPath, request);
      if (exportsResolved) return exportsResolved;
    }
    const basePath = path.resolve(curPath, request);
    let filename;
    const rc = _stat(basePath);
    if (!trailingSlash) {
      if (rc === 0) {
        filename = (isMain ? preserveSymlinksMain() : preserveSymlinks()) ? path.resolve(basePath) : toRealPath(basePath);
      }
      if (!filename) {
        if (exts === undefined) exts = resolutionExtensions();
        filename = tryExtensions(basePath, exts, isMain);
      }
    }
    if (!filename && rc === 1) {
      if (exts === undefined) exts = resolutionExtensions();
      filename = tryPackage(basePath, exts, isMain, request);
    }
    if (filename) {
      Module._pathCache[cacheKey] = filename;
      return filename;
    }
  }
  return false;
}

// The extensions a bare `require("./x")` tries, in the order the loaders were registered. ESM-only
// extensions load when named but are not guessed.
function resolutionExtensions() {
  return Object.keys(Module._extensions).filter((ext) => ext !== ".mjs" && ext !== ".mts");
}

// --- ahead-of-time blobs ------------------------------------------------------------------------
// A precompiled blob (lumen_aot::include_js!) with CommonJS units defines `__lumenAot`: its
// units are keyed `aot:/<path>`, and every require() the bundler resolved is recorded in it. A
// require from an `aot:/` module (or of an `aot:/` key) resolves there first; what the blob lacks
// falls back to the filesystem — bare packages from the current directory.
function isAotKey(s) {
  return typeof s === "string" && s.startsWith("aot:/");
}
function aotResolve(specifier, parentFilename) {
  const aot = globalThis.__lumenAot;
  if (aot === undefined) return undefined;
  if (!isAotKey(specifier) && !isAotKey(parentFilename)) return undefined;
  return aot.resolve(specifier, isAotKey(parentFilename) ? parentFilename : "");
}
function aotDirname(key) {
  const i = key.lastIndexOf("/");
  return i >= "aot:/".length ? key.slice(0, i) : "aot:/";
}

const SCHEME_ONLY = new Set(["test", "test/reporters", "sea", "sqlite"]);

function pseudoParent(fromDir, parentFilename) {
  const filename = parentFilename || path.join(fromDir, "[internal]");
  return { id: filename, filename, path: fromDir, paths: nodeModulePaths(fromDir) };
}

// Node's Module._resolveFilename. Builtins come back exactly as requested.
function resolveRequest(request, parent, isMain, options) {
  if (isCoreSpecifier(request)) return request;
  const parentFilename = parent && typeof parent.filename === "string" ? parent.filename : undefined;
  const aotKey = aotResolve(request, parentFilename);
  if (aotKey !== undefined) return aotKey;
  if (isAotKey(parentFilename) || isAotKey(request)) {
    if (isAotKey(request) || isRelativeSpecifier(request)) {
      const e = new Error(`Cannot find module '${request}' from '${parentFilename || request}'`);
      e.code = "MODULE_NOT_FOUND";
      throw e;
    }
    parent = pseudoParent(process.cwd());
  }

  let paths;
  if (typeof options === "object" && options !== null) {
    if (Array.isArray(options.paths)) {
      if (isRelativeSpecifier(request)) {
        paths = options.paths;
      } else {
        const fakeParent = new Module("", null);
        paths = [];
        for (const dir of options.paths) {
          fakeParent.paths = Module._nodeModulePaths(dir);
          const lookupPaths = Module._resolveLookupPaths(request, fakeParent);
          for (const candidate of lookupPaths) {
            if (!paths.includes(candidate)) paths.push(candidate);
          }
        }
      }
    } else if (options.paths === undefined) {
      paths = Module._resolveLookupPaths(request, parent);
    } else {
      throw new __errors.ERR_INVALID_ARG_VALUE("options.paths", options.paths);
    }
  } else {
    paths = Module._resolveLookupPaths(request, parent);
  }

  const selfResolved = trySelf(parentFilename, request);
  if (selfResolved) return selfResolved;

  const filename = Module._findPath(request, paths, isMain);
  if (filename) return filename;
  // Optional native addons lumen implements itself (bufferutil, ...): used only when not installed.
  if (__native.isFallbackModule(request)) return "node:" + request;
  const requireStack = [];
  for (let cursor = parent; cursor; cursor = moduleParentCache.get(cursor)) {
    requireStack.push(cursor.filename || cursor.id);
  }
  let message = `Cannot find module '${request}'`;
  if (requireStack.length > 0) message += "\nRequire stack:\n- " + requireStack.join("\n- ");
  const err = new Error(message);
  err.code = "MODULE_NOT_FOUND";
  err.requireStack = requireStack;
  throw err;
}

// The internal resolve step: builtins come back as "node:<name>".
function defaultResolveFilename(specifier, fromDir, parentFilename) {
  const parent = pseudoParent(fromDir, parentFilename);
  const resolved = resolveRequest(specifier, parent, false, undefined);
  const core = isCoreSpecifier(resolved);
  return core ? "node:" + core : resolved;
}

// --- registerHooks (Node's sync module customization hooks) -------------------------------------
// module.registerHooks({ resolve, load }) chains user hooks onto require()'s resolve and load
// steps, LIFO like Node's (the most recently registered hook runs first; nextResolve/nextLoad
// walks toward the default). Hooks speak URLs — file:// for files, node: for builtins — so the
// default steps translate to/from the path-based machinery above. Scope, honestly: this covers
// the CommonJS require() path (resolve + load, including require.resolve and Module._load) and,
// through `esmHook` below, ESM `import` — the native loader consults the chains first.
const __registeredHooks = []; // registration order; chains are built newest-outermost

// Minimal path <-> file URL translation (symmetric; percent-encodes only what breaks a URL).
function pathToFileUrl(p) {
  let s = String(p).replace(/\\/g, "/");
  if (!s.startsWith("/")) s = "/" + s; // win32 drive form C:/x -> /C:/x
  return "file://" + s.replace(/%/g, "%25").replace(/#/g, "%23").replace(/\?/g, "%3F").replace(/ /g, "%20");
}
function fileUrlToPath(u) {
  let p = String(u).slice("file://".length);
  const q = p.indexOf("?");
  if (q >= 0) p = p.slice(0, q); // a search suffix is legal in a loader URL; the file is the path
  p = p.replace(/^\/([A-Za-z]:)/, "$1");
  return decodeURIComponent(p);
}
function hookConditions() {
  return ["require", "node", "default"];
}

// Compose the registered hooks of one kind around `defaultStep`, newest hook outermost. Each
// hook must either call next() or return { shortCircuit: true }, exactly as Node enforces.
function chainHooks(kind, defaultStep) {
  let next = defaultStep;
  for (const reg of __registeredHooks) { // oldest first, so the newest ends up outermost
    const fn = reg[kind];
    if (!fn) continue;
    const inner = next;
    next = (arg0, context) => {
      let calledNext = false;
      const nextHook = (a, c) => {
        calledNext = true;
        return inner(a === undefined ? arg0 : a, c === undefined ? context : c);
      };
      const result = fn(arg0, context, nextHook);
      if (!result || typeof result !== "object") {
        const e = new TypeError(`The "${kind}" hook must return an object`);
        e.code = "ERR_INVALID_RETURN_VALUE";
        throw e;
      }
      if (!calledNext && !result.shortCircuit) {
        const e = new TypeError(
          `Expected true to be returned for the "shortCircuit" from the "${kind}" hook but got ${result.shortCircuit}.`,
        );
        e.code = "ERR_INVALID_RETURN_PROPERTY_VALUE";
        throw e;
      }
      return result;
    };
  }
  return next;
}

function hasHook(kind) {
  // Indexed, not for-of: module loading must survive a program deleting the array iterator.
  for (let i = 0; i < __registeredHooks.length; i++) if (__registeredHooks[i][kind]) return true;
  return false;
}

// resolveFilename: the single resolve funnel (require, require.resolve, Module._load /
// _resolveFilename all land here). With hooks registered, run the chain over URLs and map the
// winning url back into the internal filename form ("node:x" or an absolute path).
function resolveFilename(specifier, fromDir, parentFilename) {
  if (!hasHook("resolve")) return defaultResolveFilename(specifier, fromDir, parentFilename);
  const defaultStep = (spec, _context) => {
    const filename = defaultResolveFilename(String(spec), fromDir, parentFilename);
    if (isAotKey(filename)) return { url: filename, shortCircuit: true };
    return {
      url: filename.startsWith("node:") ? filename : pathToFileUrl(filename),
      shortCircuit: true,
    };
  };
  const run = chainHooks("resolve", defaultStep);
  const result = run(String(specifier), {
    conditions: hookConditions(),
    importAttributes: {},
    parentURL: parentFilename ? pathToFileUrl(parentFilename) : pathToFileUrl(fromDir + "/"),
  });
  const url = String(result.url);
  if (isAotKey(url)) return url;
  if (url.startsWith("node:") || isBuiltin(url)) return url.startsWith("node:") ? url : "node:" + url;
  if (url.startsWith("file://")) return __node.realpath(fileUrlToPath(url));
  throw new Error(
    `registerHooks: resolve returned unsupported URL scheme '${url}' (file:// and node: are supported in lumen)`,
  );
}

// ESM imports: the engine asks here before its native resolver whenever hooks are registered
// (`__lumenEsmHook`, see `Interp::fetch_module`). The chains run exactly as for require(), with
// a default step that hands the decision back to the native loader: a hook that defers on both
// resolve and load answers `undefined`, one that redirects to a file answers `{ specifier }`,
// and one that short-circuits both with a module answers `{ key, source }`.
// require(esm) must execute the exact source returned by its synchronous load hook.
// This temporary handoff lasts only through loadESM's synchronous import/microtask drain;
// dependencies still resolve through their ordinary hooks and package scopes.
const requireEsmSources = new Map();
function esmHook(specifier, referrer, attrType) {
  const supplied = requireEsmSources.get(specifier);
  if (supplied !== undefined) return { key: pathToFileUrl(specifier), source: supplied };
  if (!hasHook("resolve") && !hasHook("load")) return undefined;
  const parentURL = /^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(referrer) ? referrer : pathToFileUrl(referrer);
  const importAttributes = attrType ? { type: attrType } : {};
  const conditions = ["import", "node", "default"];
  let nativeResolve = false;
  const resolved = chainHooks("resolve", (spec) => {
    nativeResolve = true;
    return { url: String(spec), shortCircuit: true };
  })(String(specifier), { conditions, importAttributes, parentURL });
  const url = String(resolved.url);
  let nativeLoad = false;
  const loaded = chainHooks("load", (u) => {
    nativeLoad = true;
    return { format: resolved.format, source: null, shortCircuit: true };
  })(url, { conditions, format: resolved.format, importAttributes });
  if (nativeLoad) {
    if (nativeResolve) return undefined;
    if (url.startsWith("node:") || isBuiltin(url)) return { specifier: url };
    if (url.startsWith("file://")) return { specifier: fileUrlToPath(url) };
    throw new Error(`registerHooks: resolve returned '${url}', which no load hook answered and the runtime cannot load`);
  }
  if (loaded.format !== "module") {
    throw new Error(`registerHooks: load format '${loaded.format}' is not supported for import in lumen (module is)`);
  }
  const source = loaded.source;
  return {
    key: nativeResolve ? pathToFileUrl(defaultResolveFilename(url, path.dirname(fileUrlToPath(parentURL)))) : url,
    source: typeof source === "string" ? source : Buffer.from(source).toString("utf8"),
  };
}
Object.defineProperty(globalThis, "__lumenEsmHook", { value: esmHook, writable: true, configurable: true, enumerable: false });

// Compile CommonJS source into `module` via the wrapper — the shared back half of the `.js`
// extension handler, split out so a registerHooks load hook can feed transformed source in.
// With `detectEsm`, a source that does not parse as CommonJS returns false (nothing ran) so the
// caller can load it as ESM instead; otherwise the parse error propagates. With `ts` the source
// is TypeScript: the engine compiles it itself (strip-only semantics) with no synthesized
// wrapper header, so every position is the file's.
function compileCommonJS(module, filename, source, detectEsm = false, ts = false) {
  const dirname = path.dirname(filename);
  // A leading #! shebang line is neutralized, as Node does, before wrapping: `#!` becomes `//`
  // so every offset in the file (and in its type table) stays put.
  source = __node.stripShebang(String(source));
  const require = makeRequireFunction(module);
  let compiled;
  try {
    if (ts || /\.[jt]sx$/.test(filename)) {
      compiled = __node.compileCommonJS(source, filename, true);
    } else if (wrapperPatched) {
      compiled = (0, eval)(Module.wrap(source));
      __node.nameSource(compiled, filename);
    } else {
      compiled = new Function("exports", "require", "module", "__filename", "__dirname", source);
      // Stack traces name the module's frames after its file, positions relative to `source`.
      __node.nameSource(compiled, filename);
    }
  } catch (e) {
    // The textual ESM probe only runs once the CommonJS parse has already failed.
    if (detectEsm && e instanceof SyntaxError && ESM_SYNTAX.test(source)) return false;
    throw e;
  }
  Reflect.apply(compiled, module.exports, [module.exports, require, module, filename, dirname]);
  return true;
}

// A load-hook `source` may be a string or a TypedArray/ArrayBuffer (Node accepts both).
function hookSourceToString(source) {
  if (typeof source === "string") return source;
  if (source instanceof ArrayBuffer) return Buffer.from(new Uint8Array(source)).toString("utf8");
  if (ArrayBuffer.isView(source)) {
    return Buffer.from(new Uint8Array(source.buffer, source.byteOffset, source.byteLength)).toString("utf8");
  }
  return String(source);
}

// With load hooks registered, run the chain for `filename` and materialize the result. Returns
// true when the hooks produced the module; false to fall through to the extension dispatch
// (builtin/addon results, i.e. source === null on a non-transformable format).
function loadViaHooks(module, filename) {
  const defaultStep = (u, _context) => {
    const url = String(u);
    if (url.startsWith("node:")) return { format: "builtin", source: null, shortCircuit: true };
    const p = url.startsWith("file://") ? fileUrlToPath(url) : url;
    const ext = path.extname(p);
    if (ext === ".json") return { format: "json", source: __node.readText(p), shortCircuit: true };
    if (ext === ".node") return { format: "addon", source: null, shortCircuit: true };
    // Node v22's default nextLoad reports `format: undefined` for a required .js file (the CJS
    // loader decides by extension after the chain); mirror that so format-switching hooks match.
    return { format: undefined, source: __node.readText(p), shortCircuit: true };
  };
  const run = chainHooks("load", defaultStep);
  const result = run(pathToFileUrl(filename), {
    format: undefined,
    conditions: hookConditions(),
    importAttributes: {},
  });
  if (result.source == null) return false; // addon or builtin: extension dispatch handles it
  const source = hookSourceToString(result.source);
  if (result.format === "json") {
    module.exports = JSON.parse(source);
    return true;
  }
  if (result.format === "module") {
    loadHookESM(module, filename, source);
    return true;
  }
  if (result.format === undefined) {
    const ext = path.extname(filename);
    const scoped = ext === ".js" || ext === ".ts";
    const type = scoped ? packageScopeType(filename) : undefined;
    if (ext === ".mjs" || ext === ".mts" || (scoped && type === "module")) {
      loadHookESM(module, filename, source);
    } else {
      const detect = scoped && type === undefined;
      const ts = ext === ".ts" || ext === ".cts";
      if (module._compile(source, filename, detect, ts) === false) loadHookESM(module, filename, source);
    }
    return true;
  }
  if (result.format === "commonjs") {
    module._compile(source, filename);
    return true;
  }
  throw new Error(
    `registerHooks: load format '${result.format}' is not supported in lumen (module, commonjs and json are)`,
  );
}

const moduleParentCache = new WeakMap();

function updateChildren(parent, child, scan) {
  const children = parent && parent.children;
  if (children && !(scan && children.includes(child))) children.push(child);
}

// Loads a module whose resolved `filename` is not a builtin into `module` (see Module.prototype.load).
function loadFile(module, filename) {
  // A unit of a precompiled blob: CommonJS runs its precompiled module wrapper; an ES module
  // unit loads through import() (require(esm)).
  if (isAotKey(filename)) {
    const aot = globalThis.__lumenAot;
    const kind = aot === undefined ? undefined : aot.kind(filename);
    const dirname = aotDirname(filename);
    module.path = dirname;
    if (kind === "commonjs") {
      const wrapper = aot.load(filename);
      wrapper.call(module.exports, module.exports, makeRequireFunction(module), module, filename, dirname);
    } else if (kind === "module") {
      loadESM(module, filename);
    } else {
      const e = new Error(`Cannot find module '${filename}'`);
      e.code = "MODULE_NOT_FOUND";
      throw e;
    }
    return;
  }
  // registerHooks load chain first (it can rewrite the source); otherwise — and for results the
  // hooks leave alone (addons) — dispatch on file extension through Module._extensions, exactly
  // as Node does: `.js`/`.cjs` run the module wrapper, `.json` is parsed, `.node` is dlopen'd for
  // its N-API registration. An unknown extension falls back to the `.js` loader, like Node.
  if (!(hasHook("load") && loadViaHooks(module, filename))) {
    Module._extensions[findLongestRegisteredExtension(filename)](module, filename);
  }
}

function findLongestRegisteredExtension(filename) {
  const name = path.basename(filename);
  let startIndex = 0;
  let index;
  while ((index = name.indexOf(".", startIndex)) !== -1) {
    startIndex = index + 1;
    if (index === 0) continue;
    const currentExtension = name.slice(index);
    if (Module._extensions[currentExtension]) return currentExtension;
  }
  return ".js";
}

// Node's makeRequireFunction: a `require` bound to `mod`, always going through the current
// `mod.require` so a host that replaces `Module.prototype.require` / `Module._load` (the VS Code
// extension host does, to serve `require("vscode")`) intercepts loads from modules it did not create.
function makeRequireFunction(mod) {
  const require = function require(path) {
    return mod.require(path);
  };
  function resolve(request, options) {
    if (typeof request !== "string") throw new __errors.ERR_INVALID_ARG_TYPE("request", "string", request);
    return Module._resolveFilename(request, mod, false, options);
  }
  require.resolve = resolve;
  function paths(request) {
    if (typeof request !== "string") throw new __errors.ERR_INVALID_ARG_TYPE("request", "string", request);
    return Module._resolveLookupPaths(request, mod);
  }
  resolve.paths = paths;
  Object.defineProperty(require, "main", { value: process.mainModule, writable: true, enumerable: true, configurable: true });
  require.extensions = Module._extensions;
  require.cache = Module._cache;
  return require;
}

// Run `filename` as the program entry (require.main === module), returning its exports.
function runMain(filename) {
  return Module._load(path.resolve(String(filename)), null, true);
}

// The main module from source the embedder holds, under a `filename` that need not exist.
function runMainSource(filename, source) {
  const module = new Module(filename, null);
  module.id = ".";
  module.filename = filename;
  module.paths = nodeModulePaths(path.dirname(filename));
  process.mainModule = module;
  Module._cache[filename] = module;
  compileCommonJS(module, filename, source);
  module.loaded = true;
  return module.exports;
}

// A cwd-bound require for -e / the REPL, plus createRequire(fromPath) like node:module's.
const cwdRequire = makeRequireFunction(Object.assign(new Module(path.join(process.cwd(), "[cwd-require]")), {
  filename: path.join(process.cwd(), "[cwd-require]"),
  paths: nodeModulePaths(process.cwd()),
}));
globalThis.require = cwdRequire;

const createRequireError = "must be a file URL object, file URL string, or absolute path string";
function createRequire(filename) {
  let filepath;
  const isURL = filename !== null && typeof filename === "object" && typeof filename.href === "string" && typeof filename.protocol === "string";
  if (isURL || (typeof filename === "string" && !path.isAbsolute(filename))) {
    try {
      filepath = __builtins.get("url").fileURLToPath(filename);
    } catch {
      throw new __errors.ERR_INVALID_ARG_VALUE("filename", filename, createRequireError);
    }
  } else if (typeof filename !== "string") {
    throw new __errors.ERR_INVALID_ARG_VALUE("filename", filename, createRequireError);
  } else {
    filepath = filename;
  }
  if (isAotKey(filepath)) {
    // createRequire(import.meta.url) in a module of a precompiled blob: resolve like a unit there.
    const dir = aotDirname(filepath);
    const m = new Module(filepath);
    m.filename = filepath;
    m.path = dir;
    m.paths = [];
    return makeRequireFunction(m);
  }
  const trailingSlash = filepath.endsWith("/");
  const proxyPath = trailingSlash ? path.join(filepath, "noop.js") : filepath;
  const m = new Module(proxyPath);
  m.filename = proxyPath;
  m.paths = nodeModulePaths(m.path);
  return makeRequireFunction(m);
}

// --- node:module surface -----------------------------------------------------------------
// `module`/`node:module` were not in `__builtins` when CORE was snapshotted at the top of this
// file (module.js is last and registers them right here), so add the bare name to the core set
// now — otherwise require('module') and require('node:module') would throw MODULE_NOT_FOUND, and
// isBuiltin('module') would be wrong. Only the bare name is a core specifier; "node:module" stays
// a __builtins alias key, not a member of CORE.
CORE.add("module");

// The frozen list of core module names, mirroring node:module's `builtinModules` (bare names,
// no "node:" prefix), which now includes "module" itself.
const builtinModules = Object.freeze([...CORE].filter((name) => !name.startsWith("internal/") && !SCHEME_ONLY.has(name)));

// isBuiltin(spec): true when `spec` — with an optional "node:" prefix — names a core module.
function isBuiltin(spec) {
  if (typeof spec !== "string") return false;
  return isCoreSpecifier(spec) !== null;
}

// Normalize a path or a file: URL (string or URL object) to a filesystem path — the same shape
// createRequire accepts above.
function toFsPath(input) {
  let p = typeof input === "object" && input ? input.href || String(input) : String(input);
  if (p.startsWith("file://")) p = p.slice(7).replace(/^\/([A-Za-z]:)/, "$1");
  return p;
}

// The pieces of Node's constants surface we can represent. Only the compile-cache status enum is
// public today; lumen never enables the cache, so DISABLED is the only value it reports back.
const constants = {
  compileCacheStatus: { FAILED: 0, ENABLED: 1, ALREADY_ENABLED: 2, DISABLED: 3 },
};

// The module wrapper strings Node exposes so tools can reconstruct the `(function (exports, ...))`
// preamble; kept identical to Node's so byte-offset math in coverage/source tooling lines up.
let wrapperPatched = false;
let wrapper = new Proxy(["(function (exports, require, module, __filename, __dirname) { ", "\n});"], {
  set(target, property, value, receiver) {
    wrapperPatched = true;
    return Reflect.set(target, property, value, receiver);
  },
  defineProperty(target, property, descriptor) {
    wrapperPatched = true;
    return Reflect.defineProperty(target, property, descriptor);
  },
});
let wrap = function (script) {
  return wrapper[0] + script + wrapper[1];
};

// The nearest enclosing package.json's "type" for `filename` ("module" | "commonjs" | undefined),
// Node's package scope lookup. Cached per directory: a package tree asks once per folder.
const packageTypeCache = new Map();
function packageScopeType(filename) {
  const seen = [];
  let dir = path.dirname(filename);
  let type;
  for (;;) {
    if (packageTypeCache.has(dir)) {
      type = packageTypeCache.get(dir);
      break;
    }
    seen.push(dir);
    const pkgPath = path.join(dir, "package.json");
    if (__node.isFile(pkgPath)) {
      try {
        const t = JSON.parse(__node.readText(pkgPath)).type;
        type = typeof t === "string" ? t : undefined;
      } catch {
        type = undefined;
      }
      break;
    }
    // A node_modules folder bounds the scope lookup the way a package root does.
    if (path.basename(dir) === "node_modules") break;
    const parent = path.dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  for (let i = 0; i < seen.length; i++) packageTypeCache.set(seen[i], type);
  return type;
}

// require(esm) — Node 22.12+/20.19+: a synchronous ES module graph loads through require() and
// yields its namespace object (or the value exported under the string name "module.exports").
// lumen's `import()` fetches, links and evaluates a graph without top-level await synchronously,
// only settling the returned promise in a reaction job, so draining the microtask queue here
// observes the settled namespace. A graph still pending after the drain is suspended at a
// top-level await, which require() cannot wait for: ERR_REQUIRE_ASYNC_MODULE, as in Node.
function loadHookESM(module, filename, source) {
  requireEsmSources.set(filename, source);
  try {
    loadESM(module, filename);
  } finally {
    requireEsmSources.delete(filename);
  }
}
function loadESM(module, filename) {
  let state = 0;
  let value;
  import(filename).then(
    (ns) => {
      state = 1;
      value = ns;
    },
    (e) => {
      state = 2;
      value = e;
    },
  );
  __node.drainMicrotasks();
  if (state === 2) throw value;
  if (state === 0) {
    const e = new Error(
      `require() cannot be used on an ESM graph with top-level await. Use import() instead. To see where the top-level await comes from, use --experimental-print-required-tla.\n  From ${filename}`,
    );
    e.code = "ERR_REQUIRE_ASYNC_MODULE";
    throw e;
  }
  module.exports = esmExportsForRequire(value);
}

// What require(esm) returns for namespace `ns` (Node's rule): the "module.exports" string export
// when there is one; otherwise the namespace — except that one with a default export (and no
// __esModule of its own) comes back as a namespace-like facade carrying `__esModule: true`, so
// transpiled-CJS consumers (`_interopRequireDefault`) pick up `default` instead of wrapping.
function esmExportsForRequire(ns) {
  if ("module.exports" in ns) return ns["module.exports"];
  if (!("default" in ns) || "__esModule" in ns) return ns;
  const facade = Object.create(null);
  Object.defineProperty(facade, "__esModule", { value: true, enumerable: true });
  for (const name of Object.keys(ns)) {
    // Live bindings, like the namespace's own.
    Object.defineProperty(facade, name, { get: () => ns[name], enumerable: true });
  }
  Object.defineProperty(facade, Symbol.toStringTag, { value: "Module" });
  return Object.preventExtensions(facade);
}

// Node's syntax detection for a `.js` file outside any "type" scope: source that fails to
// compile as CommonJS but uses ESM syntax is re-run as a module. The probe is a cheap textual
// check; the CJS wrapper failing to parse decides that it was not CommonJS in the first place.
const ESM_SYNTAX = /(^|[\s;})])(import\s*[{*\w"']|export\s*[{*\w]|import\.meta\b)/m;

// Node's per-extension loaders. loadModule() above dispatches through these, so replacing or
// wrapping an entry (as ts-node / pirates do) actually takes effect — real behavior, not a stub.
const _extensions = {
  // An explicit .cjs file stays CommonJS even inside a type:module package.
  ".cjs": function (module, filename) {
    module._compile(readSource(filename), filename, false);
  },
  ".js": function (module, filename) {
    const type = packageScopeType(filename);
    if (type === "module") return loadESM(module, filename);
    const source = readSource(filename);
    const detect = type === undefined;
    if (module._compile(source, filename, detect) === false) loadESM(module, filename);
  },
  ".mjs": function (module, filename) {
    loadESM(module, filename);
  },
  // TypeScript: the engine parses it (Node's strip-only semantics, every offset kept). `.ts`
  // follows the package "type" (with syntax detection when there is none), like `.js`; `.mts`
  // is always ESM, `.cts` always CommonJS.
  ".ts": function (module, filename) {
    const type = packageScopeType(filename);
    if (type === "module") return loadESM(module, filename);
    const source = readSource(filename);
    const detect = type === undefined;
    if (module._compile(source, filename, detect, true) === false) loadESM(module, filename);
  },
  ".mts": function (module, filename) {
    loadESM(module, filename);
  },
  ".jsx": function (module, filename) {
    const type = packageScopeType(filename);
    if (type === "module") return loadESM(module, filename);
    if (module._compile(readSource(filename), filename, type === undefined, false) === false) loadESM(module, filename);
  },
  ".tsx": function (module, filename) {
    const type = packageScopeType(filename);
    if (type === "module") return loadESM(module, filename);
    if (module._compile(readSource(filename), filename, type === undefined, true) === false) loadESM(module, filename);
  },
  ".cts": function (module, filename) {
    module._compile(readSource(filename), filename, false, true);
  },
  ".json": function (module, filename) {
    try {
      module.exports = JSON.parse(readSource(filename).replace(/^\uFEFF/, ""));
    } catch (err) {
      err.message = filename + ": " + err.message;
      throw err;
    }
  },
  ".node": function (module, filename) {
    module.exports = __node.loadNativeAddon(filename);
  },
};

const pendingDeprecation = () =>
  !!process[Symbol.for("lumen.options")]?.["--pending-deprecation"] || process.env.NODE_PENDING_DEPRECATION === "1";

function Module(id = "", parent) {
  this.id = id;
  this.path = path.dirname(id);
  Object.defineProperty(this, "exports", { value: {}, writable: true, enumerable: true, configurable: true });
  moduleParentCache.set(this, parent);
  updateChildren(parent, this, false);
  this.filename = null;
  this.loaded = false;
  this.children = [];
}

const parentDeprecation = () => {
  if (pendingDeprecation()) {
    process.emitWarning(
      "module.parent is deprecated due to accuracy issues. Please use require.main to find program entry point instead.",
      "DeprecationWarning",
      "DEP0144",
    );
  }
};
Object.defineProperty(Module.prototype, "parent", {
  get() {
    parentDeprecation();
    return moduleParentCache.get(this);
  },
  set(value) {
    parentDeprecation();
    moduleParentCache.set(this, value);
  },
  configurable: true,
});

let isPreloading = false;
Object.defineProperty(Module.prototype, "isPreloading", { get: () => isPreloading, configurable: true });

let requireDepth = 0;
let modulePaths = null;
let globalPathsValue = [];

function initPaths() {
  const home = process.platform === "win32" ? process.env.USERPROFILE : process.env.HOME;
  const nodePath = process.env.NODE_PATH;
  let paths = [path.resolve(process.execPath, "..", "..", "lib", "node")];
  if (home) {
    paths.unshift(path.resolve(home, ".node_libraries"));
    paths.unshift(path.resolve(home, ".node_modules"));
  }
  if (nodePath) paths = nodePath.split(path.delimiter).filter(Boolean).concat(paths);
  modulePaths = paths;
  globalPathsValue = paths.slice();
}

Module.prototype.require = function (id) {
  if (typeof id !== "string") throw new __errors.ERR_INVALID_ARG_TYPE("id", "string", id);
  if (id === "") throw new __errors.ERR_INVALID_ARG_VALUE("id", id, "must be a non-empty string");
  requireDepth++;
  try {
    return Module._load(id, this, false);
  } finally {
    requireDepth--;
  }
};

Module.prototype.load = function (filename) {
  this.filename = filename;
  if (!isAotKey(filename)) this.paths = Module._nodeModulePaths(path.dirname(filename));
  loadFile(this, filename);
  this.loaded = true;
};

Module.prototype._compile = function (content, filename, detectEsm, ts) {
  const initial = requireDepth === 0;
  if (initial) statCache = new Map();
  try {
    return compileCommonJS(this, filename, content, detectEsm === true, ts === true);
  } finally {
    if (initial) statCache = null;
  }
};

// node_modules lookup paths for `from`, walking to the filesystem root (Node's _nodeModulePaths).
function nodeModulePaths(from) {
  const paths = [];
  let dir = path.resolve(String(from));
  while (true) {
    if (path.basename(dir) !== "node_modules") paths.push(path.join(dir, "node_modules"));
    const parent = path.dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  return paths;
}

// The dir a module-ish `parent` resolves specifiers from (its `path`, else its `filename`'s dir).
function parentDir(parent) {
  if (parent && typeof parent.path === "string") return parent.path;
  if (parent && typeof parent.filename === "string") return path.dirname(parent.filename);
  return process.cwd();
}

function _resolveFilename(request, parent, isMain, options) {
  return resolveRequest(request, parent, isMain, options);
}

const relativeResolveCache = Object.create(null);

const CircularRequirePrototypeWarningProxy = new Proxy(
  {},
  {
    get(target, prop) {
      if (prop in target || prop === "__esModule") return target[prop];
      emitCircularRequireWarning(prop);
      return undefined;
    },
    getOwnPropertyDescriptor(target, prop) {
      if (hasOwn(target, prop) || prop === "__esModule") return Object.getOwnPropertyDescriptor(target, prop);
      emitCircularRequireWarning(prop);
      return undefined;
    },
  },
);

function emitCircularRequireWarning(prop) {
  process.emitWarning(`Accessing non-existent property '${String(prop)}' of module exports inside circular dependency`);
}

const isProxy = (value) => __node.isProxy(value);

function getExportsForCircularRequire(module) {
  if (
    module.exports &&
    !isProxy(module.exports) &&
    Object.getPrototypeOf(module.exports) === Object.prototype &&
    !module.exports.__esModule
  ) {
    Object.setPrototypeOf(module.exports, CircularRequirePrototypeWarningProxy);
  }
  return module.exports;
}

function builtinFor(filename) {
  const core = filename.startsWith("node:") ? filename.slice(5) : filename;
  if (core.startsWith("internal/")) return exposedInternal(core) ? core : null;
  return CORE.has(core) &&(filename.startsWith("node:") || isCoreSpecifier(filename) !== null) ? core : null;
}

// Module._load(request, parent, isMain): resolve then load, returning the module's exports.
function _load(request, parent, isMain) {
  let relResolveCacheIdentifier;
  if (parent) {
    if (!request.startsWith("node:")) {
      relResolveCacheIdentifier = `${parent.path}\x00${request}`;
      const cachedFilename = relativeResolveCache[relResolveCacheIdentifier];
      if (cachedFilename !== undefined) {
        const cachedModule = Module._cache[cachedFilename];
        if (cachedModule !== undefined) {
          updateChildren(parent, cachedModule, true);
          if (!cachedModule.loaded) return getExportsForCircularRequire(cachedModule);
          return cachedModule.exports;
        }
        delete relativeResolveCache[relResolveCacheIdentifier];
      }
    }
  }
  if (request.startsWith("node:")) {
    if (!isCoreSpecifier(request)) throw new __errors.ERR_UNKNOWN_BUILTIN_MODULE(request);
    return loadBuiltin(request.slice(5));
  }
  let filename;
  if (hasHook("resolve")) {
    filename = resolveFilename(request, parentDir(parent), parent && typeof parent.filename === "string" ? parent.filename : undefined);
  } else {
    filename = Module._resolveFilename(request, parent, isMain);
  }
  // The cache comes first, so `require.cache.fs = …` stands in for the builtin (`node:fs` does not).
  const cachedModule = Module._cache[filename];
  if (cachedModule !== undefined) {
    updateChildren(parent, cachedModule, true);
    if (!cachedModule.loaded) return getExportsForCircularRequire(cachedModule);
    return cachedModule.exports;
  }
  const builtin = builtinFor(filename);
  if (builtin !== null) return loadBuiltin(builtin);
  const module = new Module(filename, parent);
  if (isMain) {
    process.mainModule = module;
    module.id = ".";
  }
  Module._cache[filename] = module;
  if (parent !== undefined && relResolveCacheIdentifier !== undefined) relativeResolveCache[relResolveCacheIdentifier] = filename;
  let threw = true;
  try {
    module.load(filename);
    threw = false;
  } finally {
    if (threw) {
      delete Module._cache[filename];
      if (relResolveCacheIdentifier !== undefined) delete relativeResolveCache[relResolveCacheIdentifier];
      const children = parent && parent.children;
      if (Array.isArray(children)) {
        const index = children.indexOf(module);
        if (index !== -1) children.splice(index, 1);
      }
    } else if (
      module.exports &&
      !isProxy(module.exports) &&
      Object.getPrototypeOf(module.exports) === CircularRequirePrototypeWarningProxy
    ) {
      Object.setPrototypeOf(module.exports, Object.prototype);
    }
  }
  return module.exports;
}

function _findPath(request, paths, isMain) {
  return findPath(request, paths, isMain);
}

// Module._resolveLookupPaths(request, parent): the search paths for `request` (null for core
// modules); a relative request resolves against its parent's directory.
function _resolveLookupPaths(request, parent) {
  if (isCoreSpecifier(request)) return null;
  const second = request.charAt(1);
  if (request.charAt(0) !== "." || (request.length > 1 && second !== "." && second !== "/")) {
    if (modulePaths === null) initPaths();
    const paths = parent && parent.paths && parent.paths.length ? parent.paths.concat(modulePaths) : modulePaths;
    return paths.length > 0 ? paths : null;
  }
  if (!parent || !parent.id || !parent.filename) return ["."];
  return [path.dirname(parent.filename)];
}

// Module.runMain(): run the process entry point (process.argv[1]) as the main module.
function moduleRunMain(main) {
  const entry = main != null ? String(main) : process.argv && process.argv[1];
  if (!entry) throw new Error("Module.runMain: no entry point (process.argv[1] is empty)");
  return runMain(entry);
}

// findPackageJSON(specifier, base): the nearest package.json for a resolved specifier, walking up
// from its directory. Experimental in Node; a real filesystem walk here, undefined if none found.
function findPackageJSON(specifier, base) {
  let dir;
  try {
    const fromInput = base != null ? toFsPath(base) : process.cwd();
    const fromDir = __node.isDir(fromInput) ? fromInput : path.dirname(fromInput);
    const resolved = resolveFilename(String(specifier), fromDir);
    if (resolved.startsWith("node:")) return undefined;
    dir = path.dirname(resolved);
  } catch (e) {
    return undefined;
  }
  while (true) {
    const pkg = path.join(dir, "package.json");
    if (__node.isFile(pkg)) return pkg;
    const parent = path.dirname(dir);
    if (parent === dir) return undefined;
    dir = parent;
  }
}

// syncBuiltinESMExports(): a no-op. Node uses it to push CJS monkeypatches of a builtin onto that
// builtin's ESM named exports. lumen's ESM builtins re-read the live module object at import time
// (see __esmBuiltin below), so there is nothing to re-sync; returns undefined like Node.
function syncBuiltinESMExports() {}

// Source-maps: lumen carries no per-frame source information (see the placeholder CallSites in
// preamble.js), so there are no maps to hand back. The support flags are honest state — settable
// and observable — but toggling them cannot make maps materialize.
let __sourceMapsSupport = { enabled: false, nodeModules: false, generatedCode: false };
function getSourceMapsSupport() {
  return { ...__sourceMapsSupport };
}
function setSourceMapsSupport(enabled, options) {
  __sourceMapsSupport = {
    enabled: !!enabled,
    nodeModules: !!(options && options.nodeModules),
    generatedCode: !!(options && options.generatedCode),
  };
}
// A minimal SourceMap: it stores the payload it is given (Node exposes `payload`/`lineLengths`),
// but lumen decodes no mappings, so findEntry/findOrigin resolve to empty results.
function SourceMap(payload, opts) {
  const lineLengths = (opts && opts.lineLengths) || [];
  Object.defineProperty(this, "payload", { enumerable: true, get: () => payload });
  Object.defineProperty(this, "lineLengths", { enumerable: true, get: () => lineLengths });
}
SourceMap.prototype.findEntry = function () {
  return {};
};
SourceMap.prototype.findOrigin = function () {
  return {};
};
// findSourceMap(path): undefined — lumen registers no maps, which is a valid Node result.
function findSourceMap() {
  return undefined;
}

// The compile cache is a V8 code-cache-on-disk optimization lumen does not implement. Report it as
// permanently DISABLED (honest, non-throwing) rather than pretending to enable it.
function enableCompileCache() {
  return { status: constants.compileCacheStatus.DISABLED, message: "compile cache is not supported in lumen" };
}
function getCompileCacheDir() {
  return undefined;
}
function flushCompileCache() {}

// registerHooks({ resolve, load }) — Node's SYNC module customization hooks, wired into the
// resolve/load funnels above (see the "__registeredHooks" section). Returns { deregister }.
function registerHooks(hooks) {
  if (hooks == null || typeof hooks !== "object") {
    throw new TypeError(`Cannot destructure property 'resolve' of 'hooks' as it is ${hooks === null ? "null" : typeof hooks}.`);
  }
  const entry = { resolve: hooks.resolve, load: hooks.load };
  for (const name of ["resolve", "load"]) {
    if (entry[name] !== undefined && typeof entry[name] !== "function") {
      const e = new TypeError(
        `The "hooks.${name}" property must be of type function. Received type ${typeof entry[name]}`,
      );
      e.code = "ERR_INVALID_ARG_TYPE";
      throw e;
    }
  }
  __registeredHooks.push(entry);
  return {
    deregister() {
      const i = __registeredHooks.indexOf(entry);
      if (i >= 0) __registeredHooks.splice(i, 1);
    },
  };
}

// The async loader-thread hook machinery remains unavailable. For sync hooks, use registerHooks.
function register() {
  throw new Error("node:module register() (async ESM loader hooks) is not supported in lumen; module.registerHooks (the sync API) is");
}
function stripTypeScriptTypes(code, options = {}) {
  return globalThis.__lumenStripTypeScriptTypes(code, options);
}

// Node's `require('module')` is the Module constructor itself, with every named export hung off it
// as a static (so require('module') === require('module').Module). Mirror that exactly.
Module.Module = Module;
Module.SourceMap = SourceMap;
Module.builtinModules = builtinModules;
Module.constants = constants;
Module.createRequire = createRequire;
Module.isBuiltin = isBuiltin;
Module.syncBuiltinESMExports = syncBuiltinESMExports;
Module.findSourceMap = findSourceMap;
Module.getSourceMapsSupport = getSourceMapsSupport;
Module.setSourceMapsSupport = setSourceMapsSupport;
Module.findPackageJSON = findPackageJSON;
Module.register = register;
Module.registerHooks = registerHooks;
Module.stripTypeScriptTypes = stripTypeScriptTypes;
Module.enableCompileCache = enableCompileCache;
Module.getCompileCacheDir = getCompileCacheDir;
Module.flushCompileCache = flushCompileCache;
Module.runMain = moduleRunMain;
// wrap/wrapper are non-enumerable in Node (they stay off Object.keys(module)), so define them so.
Object.defineProperty(Module, "wrap", {
  get: () => wrap,
  set(value) {
    wrapperPatched = true;
    wrap = value;
  },
  configurable: true,
});
Object.defineProperty(Module, "wrapper", {
  get: () => wrapper,
  set(value) {
    wrapperPatched = true;
    wrapper = value;
  },
  configurable: true,
});
Module._extensions = _extensions;
Module._pathCache = Object.create(null);
Module._debug = function () {};
Module._findPath = _findPath;
Object.defineProperty(Module, "_stat", {
  get() { return _stat; },
  set(fn) {
    emitExperimentalWarning("Module._stat");
    _stat = fn;
    return true;
  },
  configurable: true,
});
Module._nodeModulePaths = nodeModulePaths;
Module._resolveFilename = _resolveFilename;
Module._resolveLookupPaths = _resolveLookupPaths;
Module._load = _load;
Module._initPaths = initPaths;
Module._preloadModules = function (requests) {
  if (!Array.isArray(requests)) return;
  isPreloading = true;
  const parent = new Module("internal/preload", null);
  try {
    parent.paths = Module._nodeModulePaths(process.cwd());
  } catch (e) {
    if (e.code !== "ENOENT") throw e;
  }
  try {
    for (const request of requests) parent.require(request);
  } finally {
    isPreloading = false;
  }
};
// _cache is the live require cache; globalPaths is computed on access because process.env is
// populated by the runtime *after* this glue runs (so reading HOME eagerly here would miss it).
Module._cache = cache;
Object.defineProperty(Module, "globalPaths", {
  enumerable: true,
  configurable: true,
  get() {
    if (modulePaths === null) initPaths();
    return globalPathsValue;
  },
  set(value) {
    globalPathsValue = value;
  },
});

__builtins.set("module", Module);
__builtins.set("node:module", Module);

// The CLI calls this once the parsed command-line options are in place (see Runtime::set_cli_options).
// --trace-exit: process.exit() reports where it was called from, like Node's Environment::Exit.
__internals.set("traceExit", function traceExit(code, threadId) {
  const who = threadId === undefined || threadId === 0 ? `node:${process.pid}` : `node:${process.pid}, thread:${threadId}`;
  const stack = String(new Error().stack).split("\n").slice(1).join("\n");
  process.stderr.write(`(${who}) WARNING: Exited the environment with code ${code}\n${stack}\n`);
});

globalThis.__lumenApplyOptions = function () {
  const options = process[Symbol.for("lumen.options")];
  if (options && options["--trace-exit"]) {
    const reallyExit = process.reallyExit;
    process.reallyExit = function reallyExit_(code) {
      __internals.get("traceExit")(code);
      return Reflect.apply(reallyExit, process, [code]);
    };
  }
  if (options && typeof options["--title"] === "string") process.title = options["--title"];
  if (options && (options["--experimental-permission"] || options["--permission"])) {
    __internals.get("permission").init();
  }
  if (options && (options["--trace-events-enabled"] || options["--trace-event-categories"] !== undefined)) {
    __internals.get("trace_events").startFromOptions(options);
  }
  if (options && options["--allow-natives-syntax"]) __node.v8SetFlags("--allow-natives-syntax");
  if (options && options["--heapsnapshot-signal"]) {
    const signal = options["--heapsnapshot-signal"];
    process.on(signal, function doWriteHeapSnapshot() {
      __builtins.get("v8").writeHeapSnapshot();
    });
  }
  if (options && Number(options["--heapsnapshot-near-heap-limit"]) > 0) {
    __builtins.get("v8").setHeapSnapshotNearHeapLimit(Number(options["--heapsnapshot-near-heap-limit"]));
  }
  if (process.env.NODE_V8_COVERAGE && !process.features.inspector) {
    process.emitWarning("The inspector is disabled, coverage could not be collected", "Warning");
  }
  if (options && options["--experimental-global-customevent"] === false) {
    // Materialise the lazy events unit first, or its first use would publish the name again.
    void globalThis.Event;
    delete globalThis.CustomEvent;
  }
  if (options && options["--experimental-global-webcrypto"] === false) {
    void globalThis.crypto;
    for (const name of ["crypto", "Crypto", "CryptoKey", "SubtleCrypto"]) delete globalThis[name];
  }
  // Node's global object: of its own properties only these are enumerable (V8's builtins and
  // the web interfaces Node adds are not), and `process`/`Buffer` are replaceable accessors.
  const enumerable = new Set([
    "global", "queueMicrotask", "clearImmediate", "clearInterval", "clearTimeout", "atob", "btoa",
    "performance", "setImmediate", "setInterval", "setTimeout", "structuredClone", "fetch", "crypto",
  ]);
  for (const key of Reflect.ownKeys(globalThis)) {
    const desc = Reflect.getOwnPropertyDescriptor(globalThis, key);
    const want = enumerable.has(key);
    if (desc.configurable && desc.enumerable !== want) Object.defineProperty(globalThis, key, { enumerable: want });
  }
  for (const name of ["process", "Buffer"]) {
    let value = globalThis[name];
    Object.defineProperty(globalThis, name, {
      get() { return value; },
      set(v) { value = v; },
      enumerable: false,
      configurable: true,
    });
  }
};

// `lumen-cli --test`: Node's test runner main (test_runner.js).
globalThis.__lumenRunTestMain = () => __internals.get("testRunnerMain")();

// Exposed to the CLI (via a tiny bootstrap) to run a file as the main module.
// The text an uncaught Error prints as: its inspection, which carries the own properties
// (`code`, `requireStack`, ...) after the stack. Plain errors keep the short report.
globalThis.__lumenDescribeError = (error) => {
  try {
    if (error instanceof Error && Object.keys(error).length > 0) return __builtins.get("util").inspect(error);
  } catch {}
  return undefined;
};

// Node's ReportFatalException: an object prints as its inspection (customInspect off), anything
// else as its string with the --trace-uncaught hint; then the Node.js version line.
globalThis.__lumenFatalReport = (error) => {
  let text;
  if ((typeof error === "object" && error !== null) || typeof error === "function") {
    try {
      const { inspect } = __builtins.get("util");
      text = inspect(error, { colors: false, customInspect: false, depth: Math.max(inspect.defaultOptions.depth, 5) });
    } catch {
      try { text = error.stack; } catch {}
    }
  }
  if (typeof text !== "string" || text === "") {
    try {
      text = typeof error === "symbol" ? error.toString() : String(error);
    } catch {
      text = "<toString() threw exception>";
    }
    const argv0 = String(process.argv0 || "node").replace(/^.*[\\/]/, "").replace(/\.exe$/, "");
    text += `\n(Use \`${argv0} --trace-uncaught ...\` to show where the exception was thrown)`;
  }
  return `${text}\n\nNode.js ${process.version}`;
};

globalThis.__runMain = (filename) => Module.runMain(filename);
globalThis.__runMainSource = runMainSource;

// --- ESM interop: synthetic re-export modules for the builtins ---
// The runtime's module loader (Rust) builds each builtin's ESM source from the export lists
// below; the source reads the module object through `__esmBuiltin`.
let punycodeWarned = false;
function loadBuiltin(name) {
  if (name === "punycode" && !punycodeWarned) {
    punycodeWarned = true;
    if (process.execArgv.includes("--pending-deprecation") || /(^|\s)--pending-deprecation(\s|$)/.test(process.env.NODE_OPTIONS || "")) {
      process.emitWarning("The `punycode` module is deprecated. Please use a userland alternative instead.", "DeprecationWarning", "DEP0040");
    }
  }
  if (name.startsWith("internal/")) return __internals.get("exposedInternals").require(name);
  return __builtins.get(name);
}
globalThis.__esmBuiltin = (name) => __builtins.get(name);

// The export names of each builtin, for the runtime's module loader: it builds a builtin's
// synthetic ESM source (`export default …; export const readFile = …`) on first import
// (lumen-runtime `esm::builtin_source`). The names come from esm_exports.js, not the modules'
// keys: reading those would load every builtin at startup (they load on first use; see build.rs
// `LAZY`). The table itself loads on first read of the global.
{
  const publish = (value) => Object.defineProperty(globalThis, "__esmExportLists", {
    value, writable: true, enumerable: true, configurable: true,
  });
  Object.defineProperty(globalThis, "__esmExportLists", {
    get() { const lists = __internals.get("esmExportLists"); publish(lists); return lists; },
    set: publish,
    enumerable: true, configurable: true,
  });
}

// The clean builtin base names (skip the "node:module" alias key). Order is cosmetic here.
const __BUILTIN_NAMES = [
  "buffer", "path", "os", "fs", "module",
  "events", "sqlite", "util", "util/types", "sys", "console", "timers", "timers/promises",
  "crypto", "querystring", "url", "net", "assert", "assert/strict",
  "string_decoder", "tty", "async_hooks", "zlib", "stream", "stream/web",
  "stream/promises", "stream/consumers", "http", "https", "http2",
  "perf_hooks", "fs/promises", "path/posix", "path/win32", "child_process",
  "dns", "dns/promises",
  "v8", "inspector", "inspector/promises", "worker_threads", "readline",
  "readline/promises", "test", "test/reporters", "tls", "process",
  "diagnostics_channel", "domain", "trace_events",
  "vm", "repl", "cluster", "dgram", "wasi",
];
globalThis.__builtinNames = __BUILTIN_NAMES.join(",");

