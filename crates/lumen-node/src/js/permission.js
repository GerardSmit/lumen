// ---- permission model (--experimental-permission) --------------------------------------------
// The CLI parses the flags into process[Symbol.for("lumen.options")]; this file turns them into
// the allow-lists behind `process.permission.has`, and guards the entry points Node's permission
// model guards: the fs path APIs, child processes, workers and process.binding.
{
  const optionsOf = () => (typeof process === "object" && process !== null && process[Symbol.for("lumen.options")]) || {};
  let model;

  // The path functions as they are at startup: a program replacing `path.resolve` must not
  // change how the model resolves what it is asked to check.
  let pathFns;
  const lazyPath = () => {
    if (pathFns === undefined) {
      const { resolve, toNamespacedPath, dirname, isAbsolute } = __builtins.get("path");
      pathFns = { resolve, toNamespacedPath, dirname, isAbsolute };
    }
    return pathFns;
  };

  const newPathSet = (flag, options) => {
    const set = { all: false, exact: [], wildcard: [] };
    const values = options[flag] || [];
    for (const value of values) {
      if (value === "*") {
        set.all = true;
        continue;
      }
      if (value.includes(",")) {
        process.emitWarning(
          `The ${flag} CLI flag has changed. Passing a comma-separated list of paths is no longer valid. ` +
          `Documentation can be found at https://nodejs.org/api/permissions.html#process-based-permissions`,
        );
      }
      if (!lazyPath().isAbsolute(value.endsWith("*") ? value.slice(0, -1) || "/" : value)) continue;
      if (value.endsWith("*")) set.wildcard.push(value.slice(0, -1));
      else set.exact.push(lazyPath().resolve(value));
    }
    return set;
  };

  const getModel = () => {
    if (model !== undefined) return model;
    const options = optionsOf();
    if (!options["--experimental-permission"] && !options["--permission"]) {
      model = null;
      return model;
    }
    model = {
      read: newPathSet("--allow-fs-read", options),
      write: newPathSet("--allow-fs-write", options),
      child: !!options["--allow-child-process"],
      worker: !!options["--allow-worker"],
      inspector: !!options["--allow-inspector"],
      addons: !!options["--allow-addons"],
      wasi: !!options["--allow-wasi"],
    };
    return model;
  };

  const pathAllowed = (set, resolved) => {
    if (set.all) return true;
    for (const p of set.exact) {
      if (resolved === p || resolved.startsWith(p.endsWith("/") ? p : p + "/")) return true;
    }
    for (const prefix of set.wildcard) {
      if (resolved.startsWith(prefix)) return true;
      if (prefix.endsWith("/") && resolved === prefix.slice(0, -1)) return true;
    }
    return false;
  };

  const invalidArg = (name, value) => {
    const util = __builtins.get("util");
    const described = value === null ? "null"
      : value === undefined ? "undefined"
      : typeof value === "object" || typeof value === "function"
        ? (value.constructor && value.constructor.name ? `an instance of ${value.constructor.name}` : util.inspect(value))
        : `type ${typeof value} (${util.inspect(value)})`;
    const message = `The "${name}" argument must be of type string. Received ${described}`;
    const error = new TypeError(message);
    error.code = "ERR_INVALID_ARG_TYPE";
    return error;
  };

  const denied = (permission, resource) => {
    const error = new Error("Access to this API has been restricted");
    error.code = "ERR_ACCESS_DENIED";
    error.permission = permission;
    if (resource !== undefined) error.resource = resource;
    return error;
  };

  const toPath = (value) => {
    if (typeof value === "string") return value;
    if (value instanceof Uint8Array) return Buffer.from(value).toString();
    if (value instanceof URL) return __builtins.get("url").fileURLToPath(value);
    if (value && typeof value === "object" && typeof value.href === "string") return __builtins.get("url").fileURLToPath(value);
    return undefined;
  };

  const check = (scope, target) => {
    const m = getModel();
    if (m === null) return;
    const text = toPath(target);
    if (text === undefined) return;
    const resolved = lazyPath().resolve(text);
    if (!pathAllowed(scope === "FileSystemRead" ? m.read : m.write, resolved)) {
      throw denied(scope, lazyPath().toNamespacedPath(resolved));
    }
  };

  const has = (scope, reference) => {
    const m = getModel();
    if (typeof scope !== "string") throw invalidArg("scope", scope);
    if (reference !== undefined && typeof reference !== "string") throw invalidArg("reference", reference);
    switch (scope) {
      case "fs":
        return m.read.all && m.write.all;
      case "fs.read":
      case "fs.write": {
        const set = scope === "fs.read" ? m.read : m.write;
        if (reference === undefined) return set.all;
        return pathAllowed(set, lazyPath().resolve(reference));
      }
      case "child":
        return m.child;
      case "worker":
        return m.worker;
      case "inspector":
        return m.inspector;
      case "addon":
        return m.addons;
      case "wasi":
        return m.wasi;
      default:
        return false;
    }
  };

  const O_WRONLY = 1, O_RDWR = 2, O_CREAT = 0x200, O_TRUNC = 0x400, O_APPEND = 0x8;
  const openAccess = (flags) => {
    if (typeof flags === "number") {
      const mode = flags & 3;
      const writes = mode === O_WRONLY || mode === O_RDWR || (flags & (O_CREAT | O_TRUNC | O_APPEND)) !== 0
        || (flags & ~(3 | 0x4 | 0x100 | 0x20000 | 0x2000000 | 0x1000000)) !== 0;
      return { read: mode !== O_WRONLY, write: writes };
    }
    const f = typeof flags === "string" ? flags : "r";
    const plus = f.includes("+");
    return { read: f.startsWith("r") || plus, write: !f.startsWith("r") || plus };
  };

  // Which arguments are paths, and how they are used. `r` = read, `w` = write.
  const FS_RULES = {
    access: "r0", exists: "r0", readFile: "r0", readdir: "r0", readlink: "r0", stat: "r0", lstat: "r0",
    statfs: "r0", opendir: "r0", realpath: "r0", watch: "r0", watchFile: "r0", openAsBlob: "r0",
    createReadStream: "r0", writeFile: "w0", appendFile: "w0", mkdir: "w0", mkdtemp: "w0", rm: "w0",
    rmdir: "w0", unlink: "w0", truncate: "w0", utimes: "w0", lutimes: "w0", chmod: "w0", lchmod: "w0",
    chown: "w0", lchown: "w0", createWriteStream: "w0", copyFile: "r0w1", cp: "d1r0w1", rename: "r0w0w1",
    link: "r0w0w1", symlink: "r0w0w1",
  };

  const applyRule = (name, args) => {
    const rule = FS_RULES[name];
    for (const part of rule.match(/[rwd]\d/g)) {
      if (part[0] === "d") {
        const target = toPath(args[+part[1]]);
        if (target !== undefined) check("FileSystemRead", lazyPath().dirname(lazyPath().resolve(target)));
        continue;
      }
      const scope = part[0] === "r" ? "FileSystemRead" : "FileSystemWrite";
      let target = args[+part[1]];
      // mkdtemp is checked on the template it fills in, as Node does.
      if (name === "mkdtemp") {
        const text = toPath(target);
        if (text !== undefined) target = `${text}XXXXXX`;
      }
      check(scope, target);
    }
  };

  const guardFsFunction = (name, fn, promises) => {
    const stripped = name.replace(/Sync$/, "");
    if (name === "open" || name === "openSync" || (promises && name === "open")) {
      return function (path, flags, ...rest) {
        const access = openAccess(promises || typeof flags !== "function" ? flags : undefined);
        const run = () => {
          if (access.read) check("FileSystemRead", path);
          if (access.write) check("FileSystemWrite", path);
        };
        if (promises) {
          try { run(); } catch (e) { return Promise.reject(e); }
        } else {
          run();
        }
        return Reflect.apply(fn, this, [path, flags, ...rest]);
      };
    }
    if (!Object.prototype.hasOwnProperty.call(FS_RULES, stripped)) return fn;
    if (stripped === "exists") {
      return function (...args) {
        try {
          applyRule(stripped, args);
        } catch {
          if (name === "existsSync") return false;
          const callback = args[args.length - 1];
          if (typeof callback === "function") process.nextTick(callback, false);
          return undefined;
        }
        return Reflect.apply(fn, this, args);
      };
    }
    const wrapped = function (...args) {
      if (stripped === "symlink") {
        const target = toPath(args[0]);
        if (target !== undefined && !lazyPath().isAbsolute(target)) {
          const error = denied("FileSystemWrite");
          error.message = `Access to this API has been restricted: relative symbolic link target ${target}`;
          if (promises) return Promise.reject(error);
          const callback = args[args.length - 1];
          if (!name.endsWith("Sync") && typeof callback === "function") {
            process.nextTick(callback, error);
            return undefined;
          }
          throw error;
        }
      }
      if (promises) {
        try { applyRule(stripped, args); } catch (e) { return Promise.reject(e); }
      } else {
        applyRule(stripped, args);
      }
      return Reflect.apply(fn, this, args);
    };
    return wrapped;
  };

  const guardObject = (target, promises) => {
    for (const key of Object.keys(target)) {
      const descriptor = Object.getOwnPropertyDescriptor(target, key);
      if (!descriptor) continue;
      // A lazily materialised member (an accessor) is guarded as the value it yields.
      const accessor = typeof descriptor.get === "function";
      const value = accessor ? target[key] : descriptor.value;
      if (typeof value !== "function" || (!accessor && !descriptor.writable)) continue;
      const guarded = guardFsFunction(key, value, promises);
      if (guarded !== value) {
        Object.defineProperty(guarded, "name", { value: value.name, configurable: true });
        if (value[__builtins.get("util").promisify.custom]) {
          guarded[__builtins.get("util").promisify.custom] = value[__builtins.get("util").promisify.custom];
        }
        Object.defineProperty(target, key, { value: guarded, writable: true, enumerable: descriptor.enumerable, configurable: true });
      }
    }
  };

  const guardAll = () => {
    const fs = __builtins.get("fs");
    guardObject(fs, false);
    for (const name of ["glob", "globSync"]) {
      const original = fs[name];
      if (typeof original !== "function") continue;
      const guarded = function (pattern, options, ...rest) {
        const cwd = options !== null && typeof options === "object" && options.cwd !== undefined ? options.cwd : process.cwd();
        check("FileSystemRead", cwd);
        return Reflect.apply(original, this, [pattern, options, ...rest]);
      };
      Object.defineProperty(guarded, "name", { value: name, configurable: true });
      Object.defineProperty(fs, name, { value: guarded, writable: true, enumerable: false, configurable: true });
    }
    if (fs.promises) guardObject(fs.promises, true);
    const childProcess = __builtins.get("child_process");
    for (const name of ["spawn", "spawnSync", "exec", "execSync", "execFile", "execFileSync", "fork"]) {
      const original = childProcess[name];
      if (typeof original !== "function") continue;
      const guarded = function (...args) {
        if (!getModel().child) throw denied("ChildProcess");
        return Reflect.apply(original, this, args);
      };
      Object.defineProperty(guarded, "name", { value: name, configurable: true });
      for (const symbol of Object.getOwnPropertySymbols(original)) guarded[symbol] = original[symbol];
      childProcess[name] = guarded;
    }
    const workers = __builtins.get("worker_threads");
    const BaseWorker = workers.Worker;
    if (typeof BaseWorker === "function") {
      workers.Worker = class Worker extends BaseWorker {
        constructor(...args) {
          if (!getModel().worker) throw denied("WorkerThreads");
          super(...args);
        }
      };
    }
    for (const name of ["binding", "_linkedBinding"]) {
      if (typeof process[name] !== "function") continue;
      process[name] = function () {
        throw denied("Addon" === name ? "Addon" : "Binding");
      };
    }
  };

  const init = () => {
    const m = getModel();
    if (m === null) return;
    const permission = { has };
    Object.defineProperty(process, "permission", { value: permission, enumerable: true, configurable: true, writable: true });
    process.emitWarning("Permission is an experimental feature and might change at any time", "ExperimentalWarning");
    if (m.child) {
      process.emitWarning("The flag --allow-child-process must be used with extreme caution. It could invalidate the permission model.", "SecurityWarning");
    }
    if (m.worker) {
      process.emitWarning("The flag --allow-worker must be used with extreme caution. It could invalidate the permission model.", "SecurityWarning");
    }
    guardAll();
  };

  __internals.set("permission", { init, check, has: (...args) => (getModel() === null ? true : has(...args)), denied });
}
