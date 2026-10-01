// node:console — the Console class and the module object Node exposes, after Node's
// lib/internal/console/constructor.js.
//
// The global `console` is built in Rust with native `log`/`info`/`debug` (stdout) and
// `warn`/`error` (stderr). They format (util.format-compatible for the common cases) and write by
// themselves while nothing JS-visible stands between them and the sinks: `process.stdout` /
// `process.stderr` unmaterialised (or still the standard stream with nothing queued), no group
// open and `console._stdout`/`_stderr` untouched. Otherwise the call goes to the function
// registered with `cops.slow`, which runs Node's write path. Node's `console` module *is* the
// global console, so the rest of the surface is added to that object in place, bound to it the
// way Node binds its global console's methods.

{
  // node:util loads on first use (see build.rs `LAZY`): plain-string logging never needs it.
  let utilMod;
  const util = () => (utilMod ??= __builtins.get("util"));
  const cops = process._console;
  __internals.set("console_native", cops);

  const kCounts = Symbol("counts");
  const kGroupIndent = Symbol("kGroupIndent");
  const kGroupIndentationWidth = Symbol("kGroupIndentWidth");
  const kFormatForStderr = Symbol("kFormatForStderr");
  const kFormatForStdout = Symbol("kFormatForStdout");
  const kGetInspectOptions = Symbol("kGetInspectOptions");
  const kColorMode = Symbol("kColorMode");
  const kIsConsole = Symbol("kIsConsole");
  const kWriteToConsole = Symbol("kWriteToConsole");
  const kBindProperties = Symbol("kBindProperties");
  const kBindStreamsEager = Symbol("kBindStreamsEager");
  const kBindStreamsLazy = Symbol("kBindStreamsLazy");
  const kUseStdout = Symbol("kUseStdout");
  const kUseStderr = Symbol("kUseStderr");
  const kMaxGroupIndentation = 1000;
  const kSecond = 1000;
  const kMinute = 60 * kSecond;
  const kHour = 60 * kMinute;

  const optionsMap = new WeakMap();
  const noop = () => {};

  function shouldColorize(stream) {
    if (process.env.FORCE_COLOR !== undefined) {
      return __builtins.get("tty").WriteStream.prototype.getColorDepth.call(stream, process.env) > 2;
    }
    return !!(stream?.isTTY && (typeof stream.getColorDepth === "function" ? stream.getColorDepth() > 2 : true));
  }

  function Console(options /* or: stdout, stderr, ignoreErrors = true */) {
    // Called without `new`: construct (a custom instanceof covers the global console too).
    if (new.target === undefined) return Reflect.construct(Console, arguments);

    if (!options || typeof options.write === "function") {
      options = { stdout: options, stderr: arguments[1], ignoreErrors: arguments[2] };
    }
    const {
      stdout,
      stderr = stdout,
      ignoreErrors = true,
      colorMode = "auto",
      inspectOptions,
      groupIndentation,
    } = options;

    if (!stdout || typeof stdout.write !== "function") throw new __errors.ERR_CONSOLE_WRITABLE_STREAM("stdout");
    if (!stderr || typeof stderr.write !== "function") throw new __errors.ERR_CONSOLE_WRITABLE_STREAM("stderr");
    if (typeof colorMode !== "boolean" && colorMode !== "auto") {
      throw new __errors.ERR_INVALID_ARG_VALUE("colorMode", colorMode);
    }
    if (groupIndentation !== undefined) {
      __validators.validateInteger(groupIndentation, "groupIndentation", 0, kMaxGroupIndentation);
    }
    if (inspectOptions !== undefined) {
      __validators.validateObject(inspectOptions, "options.inspectOptions");
      if (inspectOptions.colors !== undefined && options.colorMode !== undefined) {
        throw new __errors.ERR_INCOMPATIBLE_OPTION_PAIR("options.inspectOptions.color", "colorMode");
      }
      optionsMap.set(this, inspectOptions);
    }

    // Bind the methods found through the instance (so a subclass's overrides win).
    for (const key of Object.keys(Console.prototype)) {
      this[key] = this[key].bind(this);
      Object.defineProperty(this[key], "name", { value: key });
    }
    this[kBindStreamsEager](stdout, stderr);
    this[kBindProperties](ignoreErrors, colorMode, groupIndentation);
  }

  const consolePropAttributes = { writable: true, enumerable: false, configurable: true };

  // `globalThis.console instanceof Console` holds although the global was not constructed.
  Object.defineProperty(Console, Symbol.hasInstance, {
    value(instance) { return instance != null && instance[kIsConsole] === true; },
  });

  const kColorInspectOptions = { colors: true };
  const kNoColorInspectOptions = {};

  function createWriteErrorHandler(instance, streamSymbol) {
    return (err) => {
      // An error not yet emitted (the write callback ran first) would be emitted on the stream
      // as an 'error' event: a one-off noop listener keeps it from becoming uncaught.
      const stream = streamSymbol === kUseStdout ? instance._stdout : instance._stderr;
      if (err != null && !stream?._writableState?.errorEmitted) {
        if (typeof stream.listenerCount === "function" && stream.listenerCount("error") === 0) {
          stream.once("error", noop);
        }
      }
    };
  }

  Object.defineProperties(Console.prototype, {
    [kBindStreamsEager]: {
      ...consolePropAttributes,
      value: function (stdout, stderr) {
        Object.defineProperties(this, {
          _stdout: { ...consolePropAttributes, value: stdout },
          _stderr: { ...consolePropAttributes, value: stderr },
        });
      },
    },
    [kBindStreamsLazy]: {
      ...consolePropAttributes,
      // Read the streams from `object` only when used, so they are not created needlessly.
      value: function (object) {
        let stdout;
        let stderr;
        Object.defineProperties(this, {
          _stdout: {
            enumerable: false,
            configurable: true,
            get() { return stdout ||= object.stdout; },
            set(value) { stdout = value; cops.setLive(1); },
          },
          _stderr: {
            enumerable: false,
            configurable: true,
            get() { return stderr ||= object.stderr; },
            set(value) { stderr = value; cops.setLive(2); },
          },
        });
      },
    },
    [kBindProperties]: {
      ...consolePropAttributes,
      value: function (ignoreErrors, colorMode, groupIndentation = 2) {
        Object.defineProperties(this, {
          _stdoutErrorHandler: { ...consolePropAttributes, value: createWriteErrorHandler(this, kUseStdout) },
          _stderrErrorHandler: { ...consolePropAttributes, value: createWriteErrorHandler(this, kUseStderr) },
          _ignoreErrors: { ...consolePropAttributes, value: Boolean(ignoreErrors) },
          _times: { ...consolePropAttributes, value: new Map() },
          [kCounts]: { ...consolePropAttributes, value: new Map() },
          [kColorMode]: { ...consolePropAttributes, value: colorMode },
          [kIsConsole]: { ...consolePropAttributes, value: true },
          [kGroupIndent]: { ...consolePropAttributes, value: "" },
          [kGroupIndentationWidth]: { ...consolePropAttributes, value: groupIndentation },
          [Symbol.toStringTag]: { writable: false, enumerable: false, configurable: true, value: "console" },
        });
      },
    },
    [kWriteToConsole]: {
      ...consolePropAttributes,
      value: function (streamSymbol, string) {
        const ignoreErrors = this._ignoreErrors;
        const groupIndent = this[kGroupIndent];
        const useStdout = streamSymbol === kUseStdout;
        const stream = useStdout ? this._stdout : this._stderr;
        const errorHandler = useStdout ? this._stdoutErrorHandler : this._stderrErrorHandler;

        if (groupIndent.length !== 0) {
          if (string.includes("\n")) string = string.replace(/\n/g, `\n${groupIndent}`);
          string = groupIndent + string;
        }
        string += "\n";

        if (ignoreErrors === false) return stream.write(string);

        // Errors may come synchronously (files, TTYs) or asynchronously (pipes): handle both.
        try {
          if (typeof stream.listenerCount === "function" && stream.listenerCount("error") === 0) stream.once("error", noop);
          stream.write(string, errorHandler);
        } catch (e) {
          // Swallowing is wrong for a stack overflow: the caller must see it.
          if (e instanceof RangeError && e.message === "Maximum call stack size exceeded") throw e;
        } finally {
          try { stream.removeListener("error", noop); } catch {}
        }
      },
    },
    [kGetInspectOptions]: {
      ...consolePropAttributes,
      value: function (stream) {
        let color = this[kColorMode];
        if (color === "auto") color = shouldColorize(stream);
        const options = optionsMap.get(this);
        if (options) {
          if (options.colors === undefined) options.colors = color;
          return options;
        }
        return color ? kColorInspectOptions : kNoColorInspectOptions;
      },
    },
    [kFormatForStdout]: {
      ...consolePropAttributes,
      value: function (args) {
        return util().formatWithOptions(this[kGetInspectOptions](this._stdout), ...args);
      },
    },
    [kFormatForStderr]: {
      ...consolePropAttributes,
      value: function (args) {
        return util().formatWithOptions(this[kGetInspectOptions](this._stderr), ...args);
      },
    },
  });

  function pad(value) {
    return `${value}`.padStart(2, "0");
  }

  function formatTime(ms) {
    let hours = 0;
    let minutes = 0;
    let seconds = 0;
    if (ms >= kSecond) {
      if (ms >= kMinute) {
        if (ms >= kHour) {
          hours = Math.floor(ms / kHour);
          ms = ms % kHour;
        }
        minutes = Math.floor(ms / kMinute);
        ms = ms % kMinute;
      }
      seconds = ms / kSecond;
    }
    if (hours !== 0 || minutes !== 0) {
      ({ 0: seconds, 1: ms } = seconds.toFixed(3).split("."));
      const res = hours !== 0 ? `${hours}:${pad(minutes)}` : minutes;
      return `${res}:${pad(seconds)}.${ms} (${hours !== 0 ? "h:m" : ""}m:ss.mmm)`;
    }
    if (seconds !== 0) return `${seconds.toFixed(3)}s`;
    return `${Number(ms.toFixed(3))}ms`;
  }

  function timeLogImpl(self, name, label, data) {
    const time = self._times.get(label);
    if (time === undefined) {
      process.emitWarning(`No such label '${label}' for console.${name}()`);
      return false;
    }
    const duration = process.hrtime(time);
    const ms = duration[0] * 1000 + duration[1] / 1e6;
    const formatted = formatTime(ms);
    if (data === undefined) self.log("%s: %s", label, formatted);
    else self.log("%s: %s", label, formatted, ...data);
    return true;
  }

  const traceConsole = (phase, name, ...rest) => {
    if (__traceEvent !== null) __traceEvent("node,node.console", phase, name, 0, ...rest);
  };

  const keyKey = "Key";
  const valuesKey = "Values";
  const indexKey = "(index)";
  const iterKey = "(iteration index)";

  // lib/internal/cli_table.js
  function cliTable(head, columns) {
    const { getStringWidth } = __internals.get("util_inspect");
    const renderRow = (row, columnWidths) => {
      let out = "│ ";
      for (let i = 0; i < row.length; i++) {
        const cell = row[i];
        const len = getStringWidth(cell);
        const needed = columnWidths[i] - len;
        out += cell + " ".repeat(needed);
        if (i !== row.length - 1) out += " │ ";
      }
      return out + " │";
    };
    const rows = [];
    const columnWidths = head.map((h) => getStringWidth(h));
    const longestColumn = Math.max(...columns.map((a) => a.length));
    for (let i = 0; i < head.length; i++) {
      const column = columns[i];
      for (let j = 0; j < longestColumn; j++) {
        if (rows[j] === undefined) rows[j] = [];
        const value = rows[j][i] = Object.prototype.hasOwnProperty.call(column, j) ? column[j] : "";
        const width = columnWidths[i] || 0;
        const counted = getStringWidth(value);
        columnWidths[i] = Math.max(width, counted);
      }
    }
    const divider = columnWidths.map((i) => "─".repeat(i + 2));
    let result = `┌${divider.join("┬")}┐\n${renderRow(head, columnWidths)}\n├${divider.join("┼")}┤\n`;
    for (const row of rows) result += `${renderRow(row, columnWidths)}\n`;
    result += `└${divider.join("┴")}┘`;
    return result;
  }

  const consoleMethods = {
    log(...args) {
      this[kWriteToConsole](kUseStdout, this[kFormatForStdout](args));
    },

    warn(...args) {
      this[kWriteToConsole](kUseStderr, this[kFormatForStderr](args));
    },

    dir(object, options) {
      this[kWriteToConsole](kUseStdout, util().inspect(object, {
        customInspect: false,
        ...this[kGetInspectOptions](this._stdout),
        ...options,
      }));
    },

    time(label = "default") {
      label = `${label}`;
      if (this._times.has(label)) {
        process.emitWarning(`Label '${label}' already exists for console.time()`);
        return;
      }
      traceConsole("b", `time::${label}`);
      this._times.set(label, process.hrtime());
    },

    timeEnd(label = "default") {
      label = `${label}`;
      const found = timeLogImpl(this, "timeEnd", label);
      traceConsole("e", `time::${label}`);
      if (found) this._times.delete(label);
    },

    timeLog(label = "default", ...data) {
      label = `${label}`;
      timeLogImpl(this, "timeLog", label, data);
      traceConsole("n", `time::${label}`);
    },

    trace(...args) {
      const err = { name: "Trace", message: this[kFormatForStderr](args) };
      Error.captureStackTrace(err, consoleMethods.trace);
      this.error(err.stack);
    },

    assert(expression, ...args) {
      if (!expression) {
        args[0] = `Assertion failed${args.length === 0 ? "" : `: ${args[0]}`}`;
        // The arguments are formatted again by warn().
        Reflect.apply(this.warn, this, args);
      }
    },

    // Clearing only makes sense when _stdout is a TTY.
    clear() {
      if (this._stdout.isTTY && process.env.TERM !== "dumb") {
        const { cursorTo, clearScreenDown } = __builtins.get("readline");
        cursorTo(this._stdout, 0, 0);
        clearScreenDown(this._stdout);
      }
    },

    count(label = "default") {
      // Anything coercible to a string; a Symbol throws.
      label = `${label}`;
      const counts = this[kCounts];
      let count = counts.get(label);
      if (count === undefined) count = 1;
      else count++;
      counts.set(label, count);
      traceConsole("C", `count::${label}`, count);
      this.log(`${label}: ${count}`);
    },

    countReset(label = "default") {
      const counts = this[kCounts];
      if (!counts.has(label)) {
        process.emitWarning(`Count for '${label}' does not exist`);
        return;
      }
      traceConsole("C", `count::${label}`, 0);
      counts.delete(`${label}`);
    },

    group(...data) {
      if (data.length > 0) Reflect.apply(this.log, this, data);
      this[kGroupIndent] += " ".repeat(this[kGroupIndentationWidth]);
    },

    groupEnd() {
      this[kGroupIndent] = this[kGroupIndent].slice(0, this[kGroupIndent].length - this[kGroupIndentationWidth]);
    },

    table(tabularData, properties) {
      if (properties !== undefined) __validators.validateArray(properties, "properties");
      if (tabularData === null || typeof tabularData !== "object") return this.log(tabularData);

      const types = __builtins.get("util/types");
      const final = (k, v) => this.log(cliTable(k, v));
      const isArray = (v) => Array.isArray(v) || types.isTypedArray(v) || __builtins.get("buffer").Buffer.isBuffer(v);
      const _inspect = (v) => {
        const depth = v !== null && typeof v === "object" && !isArray(v) && Object.keys(v).length > 2 ? -1 : 0;
        const opt = {
          depth,
          maxArrayLength: 3,
          breakLength: Infinity,
          ...this[kGetInspectOptions](this._stdout),
        };
        return util().inspect(v, opt);
      };
      const getIndexArray = (length) => Array.from({ length }, (_, i) => _inspect(i));

      const mapIter = types.isMapIterator(tabularData);
      let isKeyValue = false;
      let i = 0;
      if (mapIter) {
        const res = __node.previewEntries(tabularData, true);
        tabularData = res[0];
        isKeyValue = res[1];
      }

      if (isKeyValue || types.isMap(tabularData)) {
        const keys = [];
        const values = [];
        let length = 0;
        if (mapIter) {
          for (; i < tabularData.length / 2; ++i) {
            keys.push(_inspect(tabularData[i * 2]));
            values.push(_inspect(tabularData[i * 2 + 1]));
            length++;
          }
        } else {
          for (const { 0: k, 1: v } of tabularData) {
            keys.push(_inspect(k));
            values.push(_inspect(v));
            length++;
          }
        }
        return final([iterKey, keyKey, valuesKey], [getIndexArray(length), keys, values]);
      }

      const setIter = types.isSetIterator(tabularData);
      if (setIter) tabularData = __node.previewEntries(tabularData);

      const setlike = setIter || mapIter || types.isSet(tabularData);
      if (setlike) {
        const values = [];
        let length = 0;
        for (const v of tabularData) {
          values.push(_inspect(v));
          length++;
        }
        return final([iterKey, valuesKey], [getIndexArray(length), values]);
      }

      const map = { __proto__: null };
      let hasPrimitives = false;
      const valuesKeyArray = [];
      const indexKeyArray = Object.keys(tabularData);
      for (; i < indexKeyArray.length; i++) {
        const item = tabularData[indexKeyArray[i]];
        const primitive = item === null || (typeof item !== "function" && typeof item !== "object");
        if (properties === undefined && primitive) {
          hasPrimitives = true;
          valuesKeyArray[i] = _inspect(item);
        } else {
          const keys = properties || Object.keys(item);
          for (const key of keys) {
            map[key] ??= [];
            if ((primitive && properties) || !Object.prototype.hasOwnProperty.call(item, key)) map[key][i] = "";
            else map[key][i] = _inspect(item[key]);
          }
        }
      }
      const keys = Object.keys(map);
      const values = Object.values(map);
      if (hasPrimitives) {
        keys.push(valuesKey);
        values.push(valuesKeyArray);
      }
      keys.unshift(indexKey);
      values.unshift(indexKeyArray);
      return final(keys, values);
    },
  };

  for (const method of Reflect.ownKeys(consoleMethods)) Console.prototype[method] = consoleMethods[method];
  Console.prototype.debug = Console.prototype.log;
  Console.prototype.info = Console.prototype.log;
  Console.prototype.dirxml = Console.prototype.log;
  Console.prototype.error = Console.prototype.warn;
  Console.prototype.groupCollapsed = Console.prototype.group;

  // --- the global console ----------------------------------------------------------------------
  const g = globalThis.console;
  const NATIVE_KEEP = new Set(["log", "info", "debug", "warn", "error"]);
  for (const prop of Reflect.ownKeys(Console.prototype)) {
    if (prop === "constructor" || NATIVE_KEEP.has(prop)) continue;
    const desc = Reflect.getOwnPropertyDescriptor(Console.prototype, prop);
    if (typeof desc.value === "function") {
      const name = desc.value.name;
      desc.value = desc.value.bind(g);
      Reflect.defineProperty(desc.value, "name", { value: name });
    }
    Reflect.defineProperty(g, prop, desc);
  }
  for (const name of NATIVE_KEEP) {
    if (typeof g[name] === "function") {
      Object.defineProperty(g, name, { value: g[name], writable: true, enumerable: true, configurable: true });
    }
  }
  g[kBindStreamsLazy](process);
  g[kBindProperties](true, "auto");

  // The native fast path does not indent: it only needs to know a group is open.
  for (const name of ["group", "groupEnd"]) {
    const method = g[name];
    const wrapped = {
      [name](...args) {
        const result = Reflect.apply(method, g, args);
        cops.indent(g[kGroupIndent] !== "");
        return result;
      },
    }[name].bind(g);
    Object.defineProperty(wrapped, "name", { value: name });
    Object.defineProperty(g, name, { value: wrapped, writable: true, enumerable: true, configurable: true });
  }
  Object.defineProperty(g, "groupCollapsed", { value: g.group, writable: true, enumerable: true, configurable: true });

  // While a stream is the standard one with nothing queued, the native sink may stand in for it.
  const stdoutWrite = process.stdout && process.stdout.write;
  const stderrWrite = process.stderr && process.stderr.write;
  const isNativeSink = (stream, rawWrite) => {
    const w = stream && stream.write;
    if (w === rawWrite) return true;
    const S = __builtins.get("stream");
    return !!S && w === S.Writable.prototype.write && stream._isStdio === true &&
      !stream.writableCorked && !stream.writableLength;
  };
  const slowWrite = (fd, args) => {
    const useStdout = fd !== 2;
    const kind = useStdout ? kUseStdout : kUseStderr;
    if (cops.live(fd)) {
      const stream = useStdout ? g._stdout : g._stderr;
      if (!isNativeSink(stream, useStdout ? stdoutWrite : stderrWrite)) {
        g[kWriteToConsole](kind, useStdout ? g[kFormatForStdout](args) : g[kFormatForStderr](args));
        return;
      }
    }
    let string = useStdout ? g[kFormatForStdout](args) : g[kFormatForStderr](args);
    const groupIndent = g[kGroupIndent];
    if (groupIndent.length !== 0) {
      if (string.includes("\n")) string = string.replace(/\n/g, `\n${groupIndent}`);
      string = groupIndent + string;
    }
    cops.write(fd, string);
  };
  cops.slow(slowWrite);

  Object.defineProperty(g, "Console", { value: Console, writable: true, enumerable: true, configurable: true });
  // Inspector-timeline hooks: inert without an attached inspector, as in a non-inspected Node.
  const inert = (name) => Object.defineProperty(g, name, {
    value: { [name]() {} }[name], writable: true, enumerable: true, configurable: true,
  });
  inert("timeStamp");
  inert("profile");
  inert("profileEnd");
  Object.defineProperty(g, "context", {
    value: function context(_label) { return new Console(process.stdout, process.stderr); },
    writable: true, enumerable: true, configurable: true,
  });
  // V8's async-stack Task. lumen tracks no async stacks, so run() simply invokes the callback.
  Object.defineProperty(g, "createTask", {
    value: function createTask(name) { return { name: String(name), run(fn, ...args) { return fn(...args); } }; },
    writable: true, enumerable: true, configurable: true,
  });

  __builtins.set("console", g);
}
