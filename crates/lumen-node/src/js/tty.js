// node:tty — isatty over the runtime's isatty op, and the tty stream classes. WriteStream is a
// Writable over the standard stream's synchronous sink (Node makes TTY writes blocking too), with
// the terminal size, Node's color-depth detection (lib/internal/tty.js) and the cursor-control
// escape sequences readline writes. ReadStream is process.stdin's data under the TTY shape.
{
  const { Readable, Writable } = __builtins.get("stream");
  const { ERR_INVALID_FD } = __errors;
  const { validateInteger } = __validators;

  function isatty(fd) {
    return Number.isInteger(fd) && fd >= 0 && fd <= 2147483647 &&
      typeof process._isatty === "function" && process._isatty(fd);
  }

  // ---- color depth (Node's lib/internal/tty.js) ----
  const COLORS_2 = 1, COLORS_16 = 4, COLORS_256 = 8, COLORS_16m = 24;
  const TERM_ENVS = {
    "eterm": COLORS_16, "cons25": COLORS_16, "console": COLORS_16, "cygwin": COLORS_16,
    "dtterm": COLORS_16, "gnome": COLORS_16, "hurd": COLORS_16, "jfbterm": COLORS_16,
    "konsole": COLORS_16, "kterm": COLORS_16, "mlterm": COLORS_16, "mosh": COLORS_16m,
    "putty": COLORS_16, "st": COLORS_16, "rxvt-unicode-24bit": COLORS_16m, "terminator": COLORS_16m,
  };
  const TERM_ENVS_REG_EXP = [/ansi/, /color/, /linux/, /^con[0-9]*x[0-9]/, /^rxvt/, /^screen/, /^xterm/, /^vt100/];
  let warned = false;
  function warnOnDeactivatedColors(env) {
    if (warned) return;
    let name = "";
    if (env.NODE_DISABLE_COLORS !== undefined) name = "NODE_DISABLE_COLORS";
    if (env.NO_COLOR !== undefined) {
      if (name !== "") name += "' and '";
      name += "NO_COLOR";
    }
    if (name !== "") {
      process.emitWarning(`The '${name}' env is ignored due to the 'FORCE_COLOR' env being set.`, "Warning");
      warned = true;
    }
  }
  function getColorDepth(env = process.env) {
    if (env.FORCE_COLOR !== undefined) {
      switch (env.FORCE_COLOR) {
        case "": case "1": case "true": warnOnDeactivatedColors(env); return COLORS_16;
        case "2": warnOnDeactivatedColors(env); return COLORS_256;
        case "3": warnOnDeactivatedColors(env); return COLORS_16m;
        default: return COLORS_2;
      }
    }
    if (env.NODE_DISABLE_COLORS !== undefined || env.NO_COLOR !== undefined || env.TERM === "dumb") return COLORS_2;
    if (process.platform === "win32") {
      const release = __builtins.get("os").release().split(".");
      if (+release[0] >= 10) {
        const build = +release[2];
        if (build >= 14931) return COLORS_16m;
        if (build >= 10586) return COLORS_256;
      }
      return COLORS_16;
    }
    if (env.TMUX) return COLORS_256;
    if (env.CI) {
      if (["APPVEYOR", "BUILDKITE", "CIRCLECI", "DRONE", "GITHUB_ACTIONS", "GITLAB_CI", "TRAVIS"].some((sign) => sign in env) ||
          env.CI_NAME === "codeship") {
        return COLORS_256;
      }
      return COLORS_2;
    }
    if ("TEAMCITY_VERSION" in env) {
      return /^(9\.(0*[1-9]\d*)\.|\d{2,}\.)/.exec(env.TEAMCITY_VERSION) !== null ? COLORS_16 : COLORS_2;
    }
    switch (env.TERM_PROGRAM) {
      case "iTerm.app":
        if (!env.TERM_PROGRAM_VERSION || /^[0-2]\./.exec(env.TERM_PROGRAM_VERSION) !== null) return COLORS_256;
        return COLORS_16m;
      case "HyperTerm": case "MacTerm": return COLORS_16m;
      case "Apple_Terminal": return COLORS_256;
    }
    if (env.COLORTERM === "truecolor" || env.COLORTERM === "24bit") return COLORS_16m;
    if (env.TERM) {
      if (/^xterm-256/.exec(env.TERM) !== null) return COLORS_256;
      const termEnv = env.TERM.toLowerCase();
      if (TERM_ENVS[termEnv]) return TERM_ENVS[termEnv];
      if (TERM_ENVS_REG_EXP.some((term) => term.exec(termEnv) !== null)) return COLORS_16;
    }
    if (env.COLORTERM) return COLORS_16;
    return COLORS_2;
  }
  function hasColors(count, env) {
    if (env === undefined && (count === undefined || (typeof count === "object" && count !== null))) {
      env = count;
      count = 16;
    } else {
      validateInteger(count, "count", 2);
    }
    return count <= 2 ** getColorDepth(env);
  }

  // ---- cursor control (readline's escape sequences) ----
  const CSI = (s) => `\x1b[${s}`;
  function writeCtl(stream, data, callback) {
    if (typeof callback === "function") return stream.write(data, callback);
    return stream.write(data);
  }

  class ReadStream extends Readable {
    constructor(fd, options) {
      if (fd >> 0 !== fd || fd < 0) throw new ERR_INVALID_FD(fd);
      super({ highWaterMark: 0, ...options });
      this.fd = fd;
      this.isRaw = false;
      this.isTTY = true;
      this._input = fd === 0 ? process.stdin : null;
      this._wired = false;
    }
    _read() {
      if (this._wired) return;
      this._wired = true;
      const input = this._input;
      if (!input) { this.push(null); return; }
      input.on("data", (chunk) => this.push(chunk));
      input.on("end", () => this.push(null));
      input.on("error", (error) => this.destroy(error));
    }
    setRawMode(flag) {
      this.isRaw = !!flag;
      return this;
    }
  }

  class WriteStream extends Writable {
    constructor(fd) {
      if (fd >> 0 !== fd || fd < 0) throw new ERR_INVALID_FD(fd);
      super({ decodeStrings: false });
      this.fd = fd;
      this._raw = __internals.get("stdio_raw")?.[fd] ?? null;
      const size = typeof process._ttySize === "function" ? process._ttySize(fd) : undefined;
      if (size) {
        this.columns = size[0];
        this.rows = size[1];
      }
    }
    _write(chunk, encoding, cb) {
      try {
        const data = typeof chunk === "string" && encoding !== "utf8" && encoding !== "utf-8" ? Buffer.from(chunk, encoding) : chunk;
        if (this._raw) this._raw.write(data);
        else __builtins.get("fs").writeSync(this.fd, data);
      } catch (e) {
        cb(e);
        return;
      }
      cb();
    }
    _refreshSize() {
      const size = typeof process._ttySize === "function" ? process._ttySize(this.fd) : undefined;
      if (!size) return;
      if (size[0] !== this.columns || size[1] !== this.rows) {
        this.columns = size[0];
        this.rows = size[1];
        this.emit("resize");
      }
    }
    cursorTo(x, y, callback) {
      if (typeof y === "function") { callback = y; y = undefined; }
      if (Number.isNaN(x) || Number.isNaN(y)) throw new __errors.ERR_INVALID_CURSOR_POS();
      if (typeof x !== "number") { if (typeof callback === "function") process.nextTick(callback, null); return true; }
      return writeCtl(this, typeof y !== "number" ? CSI(`${x + 1}G`) : CSI(`${y + 1};${x + 1}H`), callback);
    }
    moveCursor(dx, dy, callback) {
      let data = "";
      if (dx < 0) data += CSI(`${-dx}D`);
      else if (dx > 0) data += CSI(`${dx}C`);
      if (dy < 0) data += CSI(`${-dy}A`);
      else if (dy > 0) data += CSI(`${dy}B`);
      if (data === "") { if (typeof callback === "function") process.nextTick(callback, null); return true; }
      return writeCtl(this, data, callback);
    }
    clearLine(dir, callback) {
      const type = dir < 0 ? CSI("1K") : dir > 0 ? CSI("0K") : CSI("2K");
      return writeCtl(this, type, callback);
    }
    clearScreenDown(callback) {
      return writeCtl(this, CSI("0J"), callback);
    }
    getWindowSize() {
      return [this.columns, this.rows];
    }
  }
  WriteStream.prototype.isTTY = true;
  WriteStream.prototype.getColorDepth = getColorDepth;
  WriteStream.prototype.hasColors = hasColors;

  // Callable without `new`, as Node's constructors are (see __legacyConstructor).
  const ReadStreamLegacy = __legacyConstructor(ReadStream);
  const WriteStreamLegacy = __legacyConstructor(WriteStream);

  __builtins.set("tty", { isatty, ReadStream: ReadStreamLegacy, WriteStream: WriteStreamLegacy });
}
