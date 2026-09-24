// node:repl over the stream-based readline implementation. Terminal editing remains readline's
// concern; this module owns evaluation, multiline buffering, commands, prompts, and result output.
{
  const util = __builtins.get("util");
  const readline = __builtins.get("readline");
  const REPL_MODE_SLOPPY = Symbol("repl-sloppy");
  const REPL_MODE_STRICT = Symbol("repl-strict");

  class Recoverable extends Error {
    constructor(error) { super(error && error.message); this.name = "Recoverable"; this.err = error; }
  }

  const writer = value => util.inspect(value, writer.options);
  writer.options = { ...util.inspect.defaultOptions };

  class REPLServer extends readline.Interface {
    constructor(prompt, source, evalFunction, useGlobal, ignoreUndefined, replMode) {
      const options = prompt && typeof prompt === "object" ? { ...prompt } : {
        prompt, stream: source, eval: evalFunction, useGlobal, ignoreUndefined, replMode,
      };
      // Node: `stream` (the legacy second argument, e.g. a socket) is both input and output.
      options.input = options.input || options.stream || process.stdin;
      options.output = options.output || options.stream || process.stdout;
      super({ input: options.input, output: options.output, terminal: options.terminal });
      this.inputStream = options.input;
      this.outputStream = options.output;
      this.useGlobal = options.useGlobal !== false;
      this.ignoreUndefined = !!options.ignoreUndefined;
      this.replMode = options.replMode || REPL_MODE_SLOPPY;
      this._prompt = options.prompt === undefined ? "> " : String(options.prompt);
      this._bufferedCommand = "";
      this.context = globalThis;
      this.eval = typeof options.eval === "function" ? options.eval : this._defaultEval.bind(this);
      this.writer = options.writer || writer;
      this.commands = Object.create(null);
      this._defineDefaults();
      this.on("line", line => this._onLine(line));
      // Node's REPL reports its end (`.exit`, or the input ending) as 'exit'.
      this.once("close", () => this.emit("exit"));
      if (options.breakEvalOnSigint) this.breakEvalOnSigint = true;
      this.displayPrompt();
    }

    _defaultEval(command, _context, filename, callback) {
      try {
        const source = this.replMode === REPL_MODE_STRICT ? `"use strict";\n${command}` : command;
        callback(null, (0, eval)(`${source}\n//# sourceURL=${filename}`));
      } catch (error) {
        if (incomplete(command, error)) callback(new Recoverable(error));
        else callback(error);
      }
    }

    _onLine(line) {
      if (!this._bufferedCommand && line[0] === ".") { this._command(line); return; }
      const command = this._bufferedCommand + line + "\n";
      // Like Node's REPL context console, output written via console during evaluation goes to
      // the REPL's output stream when that is not the process's stdout.
      const done = (error, value) => {
        if (error instanceof Recoverable) { this._bufferedCommand = command; this.displayPrompt(true); return; }
        this._bufferedCommand = "";
        // An error thrown inside `domain.run()` belongs to that domain's 'error' handler.
        if (error && error.domainThrown && error.domain && typeof error.domain.listenerCount === "function"
            && error.domain.listenerCount("error") > 0) {
          error.domain.emit("error", error);
          this.displayPrompt();
          return;
        }
        if (error) this._write(`${error.name || "Error"}: ${error.message}\n`);
        else if (!(this.ignoreUndefined && value === undefined)) this._write(`${this.writer(value)}\n`);
        this.displayPrompt();
      };
      this._withConsole(() => this.eval(command, this.context, "repl", (error, value) => this._withConsole(() => done(error, value))));
    }

    _withConsole(run) {
      const out = this.outputStream;
      if (!out || out === process.stdout || typeof out.write !== "function" || this._consoleSwapped) return run();
      const saved = globalThis.console;
      const Console = __builtins.get("console")?.Console;
      if (typeof Console !== "function") return run();
      this._consoleSwapped = true;
      // The stream's own write() is outside the REPL's code, so it sees the process console.
      let replConsole;
      const sink = { write(chunk) { globalThis.console = saved; try { return out.write(chunk); } finally { globalThis.console = replConsole; } } };
      replConsole = new Console(sink, sink);
      globalThis.console = replConsole;
      try { return run(); } finally { globalThis.console = saved; this._consoleSwapped = false; }
    }

    _command(line) {
      const space = line.indexOf(" ");
      const name = line.slice(1, space < 0 ? undefined : space);
      const argument = space < 0 ? "" : line.slice(space + 1).trim();
      const command = this.commands[name];
      if (!command) { this._write(`Invalid REPL keyword\n`); this.displayPrompt(); return; }
      try { command.action.call(this, argument); }
      catch (error) { this._write(`${error.name || "Error"}: ${error.message}\n`); this.displayPrompt(); }
    }

    _defineDefaults() {
      this.defineCommand("exit", { help: "Exit the REPL", action() { this.close(); } });
      this.defineCommand("clear", { help: "Reset the REPL context", action() { this.resetContext(); this._write("Cleared context\n"); this.displayPrompt(); } });
      this.defineCommand("break", { help: "Cancel a multiline expression", action() { this.clearBufferedCommand(); this.displayPrompt(); } });
      this.defineCommand("help", { help: "Print this help message", action() { for (const name of Object.keys(this.commands).sort()) this._write(`.${name}\t${this.commands[name].help || ""}\n`); this.displayPrompt(); } });
    }

    defineCommand(keyword, command) {
      if (typeof command === "function") command = { action: command };
      if (!command || typeof command.action !== "function") throw new TypeError("REPL command requires an action function");
      this.commands[String(keyword).replace(/^\./, "")] = { help: command.help || "", action: command.action };
    }
    displayPrompt(preserveCursor) { const p = this._bufferedCommand ? "... " : this._prompt; if (!this._closed && p) this._write(p); return this; }
    setPrompt(prompt) { this._prompt = String(prompt); }
    getPrompt() { return this._prompt; }
    clearBufferedCommand() { this._bufferedCommand = ""; }
    resetContext() { this.context = globalThis; this.emit("reset", this.context); return this.context; }
    setupHistory(_path, callback) { this.history = []; queueMicrotask(() => callback && callback(null, this)); }
    _write(value) { if (this.outputStream && typeof this.outputStream.write === "function") this.outputStream.write(value); }
  }

  function incomplete(source, error) {
    if (error && /unexpected end|unterminated|missing/i.test(error.message || "")) return true;
    let braces = 0, quote = null, escaped = false;
    for (const char of source) {
      if (quote) { if (escaped) escaped = false; else if (char === "\\") escaped = true; else if (char === quote) quote = null; continue; }
      if (char === '"' || char === "'" || char === "`") quote = char;
      else if (char === "{" || char === "(" || char === "[") braces++;
      else if (char === "}" || char === ")" || char === "]") braces--;
    }
    return !!quote || braces > 0;
  }

  // `start([options])`, or Node's legacy `start(prompt, stream, eval, useGlobal, ignoreUndefined,
  // replMode)`: all of the arguments reach the server.
  function start(prompt, source, evalFunction, useGlobal, ignoreUndefined, replMode) {
    return new REPLServer(prompt, source, evalFunction, useGlobal, ignoreUndefined, replMode);
  }

  // Callable without `new`, as Node's constructors are (see __legacyConstructor).
  REPLServer = __legacyConstructor(REPLServer);
  const repl = { REPLServer, start, writer, Recoverable, REPL_MODE_SLOPPY, REPL_MODE_STRICT };
  Object.defineProperty(repl, "builtinModules", { enumerable: false, configurable: true, get() { const module = __builtins.get("module"); return module ? module.builtinModules : []; } });
  __builtins.set("repl", repl);
}
