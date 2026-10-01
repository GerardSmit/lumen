// node:console — the Console class and the module object Node exposes.
//
// The global `console` (native `log`/`info`/`debug` → stdout sink, `warn`/`error` → stderr sink)
// is built in Rust with just those five methods. Node's `console` module is the *same object* as
// the global, carrying the full surface (assert/dir/group/count/time/table/trace/…) plus the
// `Console` constructor. We reproduce that: augment the global console in place (so scripts using
// the global get the full API too) and register it as the module, exactly as Node's identity holds
// (`require('console') === globalThis.console`).
//
// The global `log`/`info`/`debug`/`warn`/`error` stay native: they format (util.format-compatible
// for primitives, plain objects and arrays) and write by themselves while `process.stdout` /
// `process.stderr` have not been materialised and no group is open, so a program that only logs
// never loads util, stream or tty. Anything else (a patched or materialised stream, an open
// group, values the native formatter declines) goes to the function registered with `cops.slow`
// below, which formats through `util` and writes through the stream. Group indentation, counters, and timers are real; `profile`/`profileEnd`/`timeStamp` are inspector-timeline hooks that are inert
// without an attached inspector — the honest behavior for a non-inspected process, matching Node.

{
  // node:util loads on first use (see build.rs `LAZY`): plain-string logging never needs it.
  let utilMod;
  const util = () => (utilMod ??= __builtins.get("util"));
  const cops = process._console;
  __internals.set("console_native", cops);
  const fmt = (args, opts) => {
    if (!opts) {
      const text = cops.format(...args);
      if (text !== undefined) return text;
    }
    return util().formatWithOptions(opts || {}, ...args);
  };
  const nowMs = () => (typeof performance !== "undefined" && performance.now ? performance.now() : Date.now());

  // Internal state lives under symbols so it never shows up in `Object.keys` (Node's console
  // module and Console instances expose only their public method surface).
  const kOut = Symbol("kWriteToStdout");
  const kErr = Symbol("kWriteToStderr");
  const kIndent = Symbol("kGroupIndent");
  const kCounts = Symbol("kCounts");
  const kTimes = Symbol("kTimes");
  const kStdout = Symbol("kStdout");
  const kStderr = Symbol("kStderr");

  function withIndent(self, str) {
    const gi = self[kIndent] || "";
    return gi ? gi + String(str).replace(/\n/g, "\n" + gi) : String(str);
  }

  function formatTime(ms) {
    if (ms >= 60000) {
      const m = Math.floor(ms / 60000);
      return `${m}m${((ms % 60000) / 1000).toFixed(3)}s`;
    }
    if (ms >= 1000) return `${(ms / 1000).toFixed(3)}s`;
    return `${ms.toFixed(3)}ms`;
  }

  // --- console.table ---------------------------------------------------------------------------
  function cellText(v) {
    return util().inspect(v, { depth: 0, colors: false });
  }
  function renderTable(data, properties) {
    if (data === null || typeof data !== "object") return fmt([data]);
    const indexKey = "(index)";
    const valuesKey = "Values";
    const entries = Array.isArray(data)
      ? data.map((v, i) => [String(i), v])
      : data instanceof Map
        ? [...data.entries()].map(([k, v]) => [typeof k === "object" ? cellText(k) : String(k), v])
        : Object.entries(data);
    const colSet = new Map();
    let hasValues = false;
    const rows = [];
    for (const [idx, value] of entries) {
      const cells = {};
      if (value !== null && typeof value === "object") {
        const keys = Array.isArray(value) ? value.map((_, i) => String(i)) : Object.keys(value);
        for (const k of keys) {
          if (properties && !properties.includes(k)) continue;
          colSet.set(k, true);
          cells[k] = cellText(value[k]);
        }
      } else {
        hasValues = true;
        cells[valuesKey] = cellText(value);
      }
      rows.push({ idx, cells });
    }
    const cols = [...colSet.keys()];
    if (hasValues) cols.push(valuesKey);
    const header = [indexKey, ...cols];
    const matrix = rows.map((r) => [r.idx, ...cols.map((c) => (c in r.cells ? r.cells[c] : ""))]);
    const widths = header.map((h, i) => Math.max(h.length, ...matrix.map((row) => String(row[i] ?? "").length), 0));
    const pad = (s, w) => {
      s = String(s);
      const total = w - s.length;
      const left = Math.floor(total / 2);
      return " ".repeat(left + 1) + s + " ".repeat(total - left + 1);
    };
    const rule = (l, m, r) => l + widths.map((w) => "─".repeat(w + 2)).join(m) + r;
    const rowStr = (cells) => "│" + cells.map((c, i) => pad(c, widths[i])).join("│") + "│";
    const out = [rule("┌", "┬", "┐"), rowStr(header), rule("├", "┼", "┤")];
    for (const row of matrix) out.push(rowStr(row));
    out.push(rule("└", "┴", "┘"));
    return out.join("\n");
  }

  // The method surface shared by the global console and every `Console` instance. `this[kOut]` /
  // `this[kErr]` emit one already-formatted line (they own the trailing newline).
  const methods = {
    log(...args) { this[kOut](withIndent(this, fmt(args))); },
    info(...args) { this[kOut](withIndent(this, fmt(args))); },
    debug(...args) { this[kOut](withIndent(this, fmt(args))); },
    dirxml(...args) { this[kOut](withIndent(this, fmt(args))); },
    error(...args) { this[kErr](withIndent(this, fmt(args))); },
    warn(...args) { this[kErr](withIndent(this, fmt(args))); },
    dir(obj, options) { this[kOut](withIndent(this, util().inspect(obj, { colors: false, ...options }))); },
    trace(...args) {
      const label = args.length ? ": " + fmt(args) : "";
      let frames = "";
      try { throw new Error(); } catch (e) {
        if (e.stack) frames = "\n" + e.stack.split("\n").slice(1).join("\n");
      }
      this[kErr](withIndent(this, "Trace" + label + frames));
    },
    assert(value, ...args) {
      if (value) return;
      const msg = args.length ? fmt(args) : "";
      this[kErr](withIndent(this, "Assertion failed" + (msg ? ": " + msg : "")));
    },
    // Behind a pipe (never a TTY) there is nothing to clear — a no-op, as in Node.
    clear() {},
    count(label = "default") {
      label = String(label);
      const c = this[kCounts] || (this[kCounts] = new Map());
      const n = (c.get(label) || 0) + 1;
      c.set(label, n);
      this[kOut](withIndent(this, `${label}: ${n}`));
    },
    countReset(label = "default") {
      if (this[kCounts]) this[kCounts].delete(String(label));
    },
    group(...args) {
      if (args.length) this.log(...args);
      this[kIndent] = (this[kIndent] || "") + "  ";
    },
    groupCollapsed(...args) { this.group(...args); },
    groupEnd() {
      const gi = this[kIndent] || "";
      this[kIndent] = gi.slice(0, Math.max(0, gi.length - 2));
    },
    time(label = "default") {
      label = String(label);
      const t = this[kTimes] || (this[kTimes] = new Map());
      if (t.has(label)) { this[kErr](withIndent(this, `Warning: Label '${label}' already exists for console.time()`)); return; }
      t.set(label, nowMs());
    },
    timeEnd(label = "default") {
      label = String(label);
      const t = this[kTimes];
      if (!t || !t.has(label)) { this[kErr](withIndent(this, `Warning: No such label '${label}' for console.timeEnd()`)); return; }
      const dur = nowMs() - t.get(label);
      t.delete(label);
      this[kOut](withIndent(this, `${label}: ${formatTime(dur)}`));
    },
    timeLog(label = "default", ...args) {
      label = String(label);
      const t = this[kTimes];
      if (!t || !t.has(label)) { this[kErr](withIndent(this, `Warning: No such label '${label}' for console.timeLog()`)); return; }
      const dur = nowMs() - t.get(label);
      this[kOut](withIndent(this, `${label}: ${formatTime(dur)}` + (args.length ? " " + fmt(args) : "")));
    },
    // Inspector-timeline hooks: inert without an attached inspector (as in a non-inspected Node).
    timeStamp() {},
    profile() {},
    profileEnd() {},
    table(tabularData, properties) { this[kOut](withIndent(this, renderTable(tabularData, properties))); },
    // V8's async-stack Task. lumen tracks no async stacks, so run() simply invokes the callback.
    createTask(name) {
      return { name: String(name), run(fn, ...args) { return fn(...args); } };
    },
  };

  // `new Console(stdout[, stderr][, ignoreErrors])` or `new Console({ stdout, stderr, ... })`.
  class Console {
    constructor(options, stderrArg) {
      let stdout, stderr;
      if (options && typeof options === "object" && typeof options.write !== "function") {
        stdout = options.stdout;
        stderr = options.stderr || options.stdout;
      } else {
        stdout = options;
        stderr = stderrArg || options;
      }
      if (!stdout || typeof stdout.write !== "function") {
        throw new TypeError('The "stdout" argument must be an instance of a writable stream');
      }
      if (stderr && typeof stderr.write !== "function") {
        throw new TypeError('The "stderr" argument must be an instance of a writable stream');
      }
      this[kStdout] = stdout;
      this[kStderr] = stderr;
      this[kIndent] = "";
      const self = this;
      Object.defineProperty(this, kOut, { value: (s) => self[kStdout].write(s + "\n") });
      Object.defineProperty(this, kErr, { value: (s) => self[kStderr].write(s + "\n") });
      // Node binds the methods as own properties on each instance.
      for (const name of Object.keys(methods)) {
        Object.defineProperty(this, name, { value: methods[name].bind(this), writable: true, enumerable: true, configurable: true });
      }
    }
  }
  Object.assign(Console.prototype, methods);
  // Callable without `new`, as Node's constructors are (see __legacyConstructor).
  Console = __legacyConstructor(Console);
  Console.prototype.Console = Console;

  // --- augment the global console into the module object ---------------------------------------
  // Node's `console` module *is* the global console (`require('console') === globalThis.console`).
  // The global's log family (`log`/`info`/`debug`/`warn`/`error`) is native and kept as-is (only
  // made enumerable so they show up as module keys). The remaining surface
  // (dir/table/group/count/time/trace/assert/…) is added in JS on top.
  const NATIVE_KEEP = new Set(["log", "info", "debug", "warn", "error"]);
  const g = globalThis.console;
  // Node's global console writes through `process.stdout.write` / `process.stderr.write`, so a
  // program (or test) that replaces those methods captures console output. Until a stream is
  // materialised there is nothing to have replaced and the native sink is written directly.
  const stdoutWrite = process.stdout && process.stdout.write;
  const stderrWrite = process.stderr && process.stderr.write;
  // The native sink may stand in for the stream while the stream is the standard one (its write
  // is the shared Writable write, or the runtime's raw one) and has nothing queued ahead.
  const isNativeSink = (stream, rawWrite) => {
    const w = stream && stream.write;
    if (w === rawWrite) return true;
    const S = __builtins.get("stream");
    return !!S && w === S.Writable.prototype.write && stream._isStdio === true &&
      !stream.writableCorked && !stream.writableLength;
  };
  const emit = (fd, s) => {
    if (cops.live(fd)) {
      const stream = fd === 1 ? process.stdout : process.stderr;
      if (!isNativeSink(stream, fd === 1 ? stdoutWrite : stderrWrite) && stream && typeof stream.write === "function") {
        stream.write(s + "\n");
        return;
      }
    }
    cops.write(fd, s);
  };
  const emitOut = (s) => emit(1, s);
  const emitErr = (s) => emit(2, s);
  Object.defineProperty(g, kOut, { value: emitOut, configurable: true });
  Object.defineProperty(g, kErr, { value: emitErr, configurable: true });
  Object.defineProperty(g, kIndent, { value: "", writable: true, configurable: true });
  const define = (name, value) =>
    Object.defineProperty(g, name, { value, writable: true, enumerable: true, configurable: true });
  for (const name of NATIVE_KEEP) if (typeof g[name] === "function") define(name, g[name]);
  // The native fast path does not indent: it only needs to know a group is open.
  for (const name of ["group", "groupCollapsed", "groupEnd"]) {
    const method = methods[name];
    define(name, {
      [name](...args) {
        const result = method.apply(g, args);
        cops.indent(g[kIndent] !== "");
        return result;
      },
    }[name]);
  }
  cops.slow((fd, args) => (fd === 2 ? g[kErr] : g[kOut])(withIndent(g, fmt(args))));
  for (const name of Object.keys(methods)) {
    if (!NATIVE_KEEP.has(name) && name !== "group" && name !== "groupCollapsed" && name !== "groupEnd") define(name, methods[name]);
  }
  define("Console", Console);
  // `console.context([label])` returns a fresh Console over this process's stdout/stderr.
  define("context", (_label) => new Console(process.stdout, process.stderr));

  __builtins.set("console", g);
}
