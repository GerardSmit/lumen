// ---- node:vm ----------------------------------------------------------------------------------
// The engine can eval (globalThis.eval and `new Function` both work), so the pieces of `vm` that
// only need "run this string" are real: `runInThisContext`, `Script`, `compileFunction`.
//
// Contexts are best-effort. Node's `createContext` builds a brand-new global object with fresh
// copies of every intrinsic (Object, Array, ...) that the sandboxed code sees instead of the
// host's. lumen cannot mint a second set of intrinsics from JS, so `runInContext` isolates the
// *variable environment* only: a `with`-over-Proxy whose `has` trap claims every name, so free
// identifiers in the code resolve against the sandbox object rather than escaping to the caller's
// scope. Names the sandbox does not define fall through to the host's globals (same Object/Array
// identity). This matches Node's loose contract for the common "eval a config/expression against a
// supplied object" case, but it is NOT a security boundary and shares host intrinsics — do not use
// it to run untrusted code.
{
  // Indirect eval always evaluates in the global lexical scope, which is exactly the semantics of
  // `runInThisContext`. Capturing it via a comma-expression call keeps it indirect.
  const indirectEval = eval;
  const runInGlobal = (code) => (0, indirectEval)(code);

  const contexts = new WeakSet();

  // Build the `with`-scope proxy over a contextified sandbox (see file header).
  const proxyFor = (sandbox) =>
    new Proxy(sandbox, {
      has() {
        // Claim every name so all free identifiers resolve through this proxy.
        return true;
      },
      get(target, key) {
        if (key === Symbol.unscopables) return undefined;
        if (key in target) return target[key];
        return globalThis[key];
      },
      set(target, key, value) {
        target[key] = value;
        return true;
      },
      deleteProperty(target, key) {
        delete target[key];
        return true;
      },
    });

  // `with (proxy) { return eval("<code>") }` — a *direct* eval whose argument is a baked-in string
  // literal (the proxy's `has` claims every name, so a `code` *parameter* would itself be shadowed
  // to undefined; a literal sidesteps that). The direct eval runs in the with-scope, so `code`'s
  // free identifiers bind through the proxy. JSON.stringify escapes the source so it cannot break
  // out of the literal.
  const runWithSandbox = (code, sandbox) => {
    const runner = new Function("_", "with (_) { return eval(" + JSON.stringify(code) + "); }");
    return runner(proxyFor(sandbox));
  };

  // `options.timeout`: run under a deadline (see vm_timeout.rs); past it the run throws
  // ERR_SCRIPT_EXECUTION_TIMEOUT instead of spinning forever.
  const bounded = (options, run) => {
    const timeout = options !== null && typeof options === "object" ? options.timeout : undefined;
    if (timeout === undefined) return run();
    if (typeof timeout !== "number" || !Number.isInteger(timeout) || timeout <= 0 || timeout > 4294967295) {
      const error = new RangeError(`The value of "options.timeout" is out of range. It must be >= 1 && <= 4294967295. Received ${String(timeout)}`);
      error.code = "ERR_OUT_OF_RANGE";
      throw error;
    }
    return __vm.runWithTimeout(timeout, run);
  };

  const contextify = (sandbox) => {
    if (sandbox === undefined) sandbox = {};
    if (typeof sandbox !== "object" || sandbox === null) {
      throw new TypeError("The \"contextObject\" argument must be an object.");
    }
    contexts.add(sandbox);
    return sandbox;
  };

  class Script {
    constructor(code, options = {}) {
      this.code = code === undefined ? "undefined" : String(code);
      this.filename = (options && options.filename) || "evalmachine.<anonymous>";
      // Compile eagerly so a SyntaxError surfaces at construction, like Node. `new Function`
      // parses the body without running it.
      try {
        new Function(this.code);
      } catch (e) {
        if (e instanceof SyntaxError) throw e;
      }
    }
    runInThisContext(options) {
      return bounded(options, () => runInGlobal(this.code));
    }
    runInContext(contextifiedObject, options) {
      if (!contexts.has(contextifiedObject)) {
        throw new TypeError("The \"contextifiedObject\" argument must be a vm.Context.");
      }
      return bounded(options, () => runWithSandbox(this.code, contextifiedObject));
    }
    runInNewContext(sandbox, options) {
      const context = contextify(sandbox);
      return bounded(options, () => runWithSandbox(this.code, context));
    }
    createCachedData() {
      throw new Error("vm.Script.createCachedData is not supported in lumen");
    }
  }

  const runInThisContext = (code, options) => {
    code = String(code);
    return bounded(options, () => runInGlobal(code));
  };
  const createContext = (sandbox) => contextify(sandbox);
  const isContext = (sandbox) => contexts.has(sandbox);
  const runInContext = (code, contextifiedObject, options) => {
    if (!contexts.has(contextifiedObject)) {
      throw new TypeError("The \"contextifiedObject\" argument must be a vm.Context.");
    }
    code = String(code);
    return bounded(options, () => runWithSandbox(code, contextifiedObject));
  };
  const runInNewContext = (code, sandbox, options) => {
    code = String(code);
    const context = contextify(sandbox);
    return bounded(options, () => runWithSandbox(code, context));
  };
  const createScript = (code, options) => new Script(code, options);

  // `compileFunction(code, params, options)` — a real function compiled from the body + named
  // params via `new Function`. The `parsingContext`/`contextExtensions` options are not honored.
  const compileFunction = (code, params = [], options = {}) => {
    const args = Array.isArray(params) ? params.slice() : [];
    args.push(code === undefined ? "" : String(code));
    return new Function(...args);
  };

  // Node returns a Promise of a memory estimate; lumen has no per-context measurement, so hand back
  // a correctly shaped resolved estimate of zero.
  const measureMemory = () =>
    Promise.resolve({ total: { jsMemoryEstimate: 0, jsMemoryRange: [0, 0] } });

  const constants = {
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
