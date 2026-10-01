// node:trace_events — Node's trace-category API over a Chrome-trace recorder. Categories come
// from `--trace-events-enabled` / `--trace-event-categories` or from `createTracing().enable()`;
// events of enabled categories are buffered and written as `{"traceEvents": [...]}` to
// `node_trace.<rotation>.log` (`--trace-event-file-pattern`) when the process exits. Event sources
// are the ones lumen has: async resources (through async_hooks), console timers and counters,
// the environment's own lifecycle, and user timing marks.

const { ERR_INVALID_ARG_TYPE, ERR_TRACE_EVENTS_CATEGORY_REQUIRED, ERR_TRACE_EVENTS_UNAVAILABLE } = __errors;

if (!__builtins.get("worker_threads").isMainThread) {
  throw new ERR_TRACE_EVENTS_UNAVAILABLE();
}

const kDefaultCategories = ["v8", "node", "node.async_hooks"];
const kDefaultPattern = "node_trace.${rotation}.log";

// category group -> number of enablers (the command line counts as one, each Tracing as one).
const enabledGroups = new Map();
const events = [];
let started = false;
let startTimestamp = 0;
let pattern = kDefaultPattern;
let asyncHook = null;
let startupTitle = "node";
let fromOptions = false;

const timestamp = () => Number(process.hrtime.bigint() / 1000n);

function retain(groups) {
  for (const group of groups) enabledGroups.set(group, (enabledGroups.get(group) ?? 0) + 1);
  start();
  syncHooks();
}

function release(groups) {
  for (const group of groups) {
    const count = (enabledGroups.get(group) ?? 0) - 1;
    if (count > 0) enabledGroups.set(group, count);
    else enabledGroups.delete(group);
  }
  syncHooks();
}

function matches(group, category) {
  if (group === category || group === "*") return true;
  if (group.endsWith("*")) return category.startsWith(group.slice(0, -1));
  return false;
}

// An event's category string is a comma list ("node,node.async_hooks"); it is recorded when any
// of its members is enabled.
function isTraceCategoryEnabled(category) {
  if (enabledGroups.size === 0) return false;
  for (const member of String(category).split(",")) {
    for (const group of enabledGroups.keys()) {
      if (matches(group, member)) return true;
    }
  }
  return false;
}

function record(ph, category, name, id, args, extra) {
  if (!started || !isTraceCategoryEnabled(category)) return;
  const event = { pid: process.pid, tid: process.pid, ts: timestamp(), ph, cat: category, name };
  if (id !== undefined) event.id = typeof id === "number" ? `0x${id.toString(16)}` : id;
  event.args = args === undefined ? {} : args;
  if (extra !== undefined) Object.assign(event, extra);
  events.push(event);
}

function metadata(name, args) {
  return { pid: process.pid, tid: process.pid, ts: startTimestamp, ph: "M", cat: "__metadata", name, args };
}

function start() {
  if (started) return;
  started = true;
  startTimestamp = timestamp();
  process.once("exit", flush);
}

function flush() {
  if (!started) return;
  started = false;
  const title = process.title;
  if (fromOptions) {
    record("I", "node,node.bootstrap", "loopExit", undefined, {}, { s: "t" });
    for (const name of ["RunCleanup", "AtExit"]) record("X", "node,node.environment", name, undefined, {}, { dur: 0 });
  }
  const all = [
    metadata("process_name", { name: startupTitle }),
    metadata("version", { node: process.versions.node }),
    metadata("node", {
      process: {
        versions: process.versions,
        arch: process.arch,
        platform: process.platform,
        release: process.release,
      },
    }),
    metadata("thread_name", { name: "JavaScriptMainThread" }),
    metadata("thread_name", { name: "PlatformWorkerThread" }),
  ];
  if (typeof title === "string" && title !== startupTitle) all.push(metadata("process_name", { name: title }));
  if (isTraceCategoryEnabled("v8")) {
    all.push({
      pid: process.pid, tid: process.pid, ts: startTimestamp, ph: "X", cat: "v8", name: "V8.Execute",
      dur: Math.max(0, timestamp() - startTimestamp), args: {},
    });
  }
  const fs = __builtins.get("fs");
  const file = pattern.replaceAll("${pid}", String(process.pid)).replaceAll("${rotation}", "1");
  fs.writeFileSync(file, JSON.stringify({ traceEvents: all.concat(events) }));
}

// ---- async_hooks events ---------------------------------------------------------------------
function syncHooks() {
  const wanted = isTraceCategoryEnabled("node.async_hooks") || isTraceCategoryEnabled("node.environment");
  if (wanted && asyncHook === null) {
    const types = new Map();
    const environmentPhases = {
      Timeout: ["RunTimers"],
      Immediate: ["RunAndClearNativeImmediates", "CheckImmediate"],
    };
    asyncHook = __builtins.get("async_hooks").createHook({
      init(id, type, trigger) {
        types.set(id, type);
        record("b", "node,node.async_hooks", type, id, {
          data: { executionAsyncId: __builtins.get("async_hooks").executionAsyncId(), triggerAsyncId: trigger },
        });
      },
      before(id) {
        const type = types.get(id);
        record("b", "node,node.async_hooks", `${type}_CALLBACK`, id);
        for (const name of environmentPhases[type] ?? []) record("b", "node,node.environment", name);
      },
      after(id) {
        const type = types.get(id);
        for (const name of environmentPhases[type] ?? []) record("e", "node,node.environment", name);
        record("e", "node,node.async_hooks", `${type}_CALLBACK`, id);
      },
      destroy(id) {
        const type = types.get(id);
        if (type === undefined) return;
        types.delete(id);
        record("e", "node,node.async_hooks", type, id);
      },
    }).enable();
  } else if (!wanted && asyncHook !== null) {
    asyncHook.disable();
    asyncHook = null;
  }
}

// ---- the public API -------------------------------------------------------------------------
class Tracing {
  #categories;
  #groups;
  #enabled = false;

  constructor(categories) {
    if (!Array.isArray(categories)) throw new ERR_INVALID_ARG_TYPE("options.categories", "string[]", categories);
    if (categories.length <= 0) throw new ERR_TRACE_EVENTS_CATEGORY_REQUIRED();
    this.#groups = categories.map(String);
    this.#categories = this.#groups.join(",");
  }

  enable() {
    if (!this.#enabled) {
      this.#enabled = true;
      retain(this.#groups);
    }
  }

  disable() {
    if (this.#enabled) {
      this.#enabled = false;
      release(this.#groups);
    }
  }

  get enabled() {
    return this.#enabled;
  }

  get categories() {
    return this.#categories;
  }

  [Symbol.for("nodejs.util.inspect.custom")](depth, options) {
    if (typeof depth === "number" && depth < 0) return this;
    const opts = { ...options, depth: options.depth == null ? null : options.depth - 1 };
    return `Tracing ${__builtins.get("util").inspect({ enabled: this.enabled, categories: this.categories }, opts)}`;
  }
}

function createTracing(options) {
  if (typeof options !== "object" || options === null) {
    throw new ERR_INVALID_ARG_TYPE("options", "Object", options);
  }
  return new Tracing(options.categories);
}

function getEnabledCategories() {
  if (enabledGroups.size === 0) return undefined;
  return [...enabledGroups.keys()].join(",");
}

// Called once the command line is parsed (module.js `__lumenApplyOptions`).
function startFromOptions(options) {
  const given = options["--trace-event-categories"];
  const categories = typeof given === "string" ? given : options["--trace-events-enabled"] ? kDefaultCategories.join(",") : "";
  const groups = categories.replaceAll('"', "").split(",").map((c) => c.trim()).filter((c) => c !== "");
  const filePattern = options["--trace-event-file-pattern"];
  if (typeof filePattern === "string") pattern = filePattern;
  fromOptions = true;
  startupTitle = typeof options["--title"] === "string" ? options["--title"] : process.title;
  start();
  if (groups.length > 0) retain(groups);
  const boot = (name) => record("I", "node,node.bootstrap", name, undefined, {}, { s: "t" });
  for (const name of ["environment", "nodeStart", "v8Start", "bootstrapComplete"]) boot(name);
  record("X", "node,node.environment", "Environment", undefined, {}, { dur: 0 });
  process.once("beforeExit", () => record("X", "node,node.environment", "BeforeExit", undefined, {}, { dur: 0 }));
  setImmediate(() => boot("loopStart")).unref();
}

// The `trace_events` binding that `internal/test/binding` (--expose-internals) hands out.
const binding = {
  isTraceCategoryEnabled,
  trace(phase, category, name, id, data) {
    record(String.fromCharCode(phase), category, name, id, data === undefined ? undefined : { data });
  },
  getCategoryEnabledBuffer: (category) => new Uint8Array([isTraceCategoryEnabled(category) ? 1 : 0]),
  setTraceCategoryStateUpdateHandler() {},
};

// What console/perf call through `__traceEvent` (preamble.js).
__setTraceEvent((category, phase, name, id, data) => {
  record(phase, category, name, id, data === undefined ? undefined : { data });
});

__builtins.set("trace_events", { createTracing, getEnabledCategories });
__builtins.set("internal/test/binding", {
  internalBinding(name) {
    if (name === "trace_events") return binding;
    return __internals.get("internalBinding")(name);
  },
});
__internals.set("trace_events", { startFromOptions, binding });
