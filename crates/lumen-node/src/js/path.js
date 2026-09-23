// node:path — posix and win32 implementations; the default export follows the host platform.

function makePath(sep, isWin) {
  const isAbs = isWin
    ? (p) => /^([a-zA-Z]:[\\/]|[\\/][\\/]|[\\/])/.test(p)
    : (p) => p.startsWith("/");

  function normalizeArray(parts, allowAboveRoot) {
    const res = [];
    for (const p of parts) {
      if (p === "" || p === ".") continue;
      if (p === "..") {
        if (res.length && res[res.length - 1] !== "..") res.pop();
        else if (allowAboveRoot) res.push("..");
      } else {
        res.push(p);
      }
    }
    return res;
  }

  const splitRe = isWin ? /[\\/]+/ : /\/+/;

  // ---- matchesGlob ------------------------------------------------------------------------------
  // Minimatch-compatible subset: *, ?, [...] (with ! / ^ negation), {a,b}, and ** as a whole
  // segment. Like Node (windowsPathsNoEscape), backslashes in the PATTERN are separators, not
  // escapes; wildcards don't match a leading dot; ** never crosses a dot segment and, when
  // trailing, must match at least one segment. Braces spanning a '/' are not supported.
  const GLOBSTAR = Symbol("globstar");
  const escRe = (ch) => (".*+?^${}()|[]\\".includes(ch) ? "\\" + ch : ch);

  function segToRegExp(seg) {
    let re = "";
    let braceDepth = 0;
    let i = 0;
    while (i < seg.length) {
      const c = seg[i];
      if (c === "*") { re += "[^/]*"; i++; continue; }
      if (c === "?") { re += "[^/]"; i++; continue; }
      if (c === "[") {
        const close = seg.indexOf("]", i + 2); // first ']' is literal when the class would be empty
        if (close < 0) { re += "\\["; i++; continue; }
        let cls = seg.slice(i + 1, close);
        let neg = false;
        if (cls[0] === "!" || cls[0] === "^") { neg = true; cls = cls.slice(1); }
        re += "[" + (neg ? "^" : "") + cls.replace(/[\\\]^]/g, "\\$&") + "]";
        i = close + 1;
        continue;
      }
      if (c === "{") { braceDepth++; re += "(?:"; i++; continue; }
      if (c === "}" && braceDepth > 0) { braceDepth--; re += ")"; i++; continue; }
      if (c === "," && braceDepth > 0) { re += "|"; i++; continue; }
      re += escRe(c);
      i++;
    }
    return new RegExp("^" + (seg[0] === "." ? "" : "(?!\\.)") + re + "$");
  }

  function matchSegs(pSegs, pi, gSegs, gi) {
    while (gi < gSegs.length) {
      const g = gSegs[gi];
      if (g === GLOBSTAR) {
        if (gi === gSegs.length - 1) {
          if (pi >= pSegs.length) return false; // trailing ** needs something to match
          for (; pi < pSegs.length; pi++) if (pSegs[pi].startsWith(".")) return false;
          return true;
        }
        for (let k = pi; k <= pSegs.length; k++) {
          if (matchSegs(pSegs, k, gSegs, gi + 1)) return true;
          if (k < pSegs.length && pSegs[k].startsWith(".")) break;
        }
        return false;
      }
      if (pi >= pSegs.length || !g.test(pSegs[pi])) return false;
      pi++;
      gi++;
    }
    return pi === pSegs.length;
  }

  const path = {
    sep,
    delimiter: isWin ? ";" : ":",
    isAbsolute: isAbs,
    normalize(p) {
      p = String(p);
      if (p.length === 0) return ".";
      const absolute = isAbs(p);
      const trailing = /[\\/]$/.test(p);
      let parts = normalizeArray(p.split(splitRe), !absolute).join(sep);
      if (!parts && !absolute) parts = ".";
      if (parts && trailing) parts += sep;
      return (absolute ? sep : "") + parts;
    },
    join(...args) {
      const joined = args.filter((a) => a != null && a !== "").join(sep);
      return joined === "" ? "." : path.normalize(joined);
    },
    resolve(...args) {
      let resolved = "";
      let absolute = false;
      for (let i = args.length - 1; i >= 0 && !absolute; i--) {
        const p = i >= 0 ? String(args[i]) : "";
        if (p === "") continue;
        resolved = p + sep + resolved;
        absolute = isAbs(p);
      }
      if (!absolute) {
        resolved = (isWin ? "" : process.cwd()) + sep + resolved;
        // A relative input resolved against the cwd is absolute from here on: the root must
        // come back with the result and `..` must not climb above it.
        absolute = !isWin;
      }
      const parts = normalizeArray(resolved.split(splitRe), !absolute).join(sep);
      return absolute ? sep + parts : parts || ".";
    },
    dirname(p) {
      p = String(p);
      const parts = p.split(splitRe);
      while (parts.length && parts[parts.length - 1] === "") parts.pop();
      if (parts.length <= 1) return isAbs(p) ? sep : ".";
      parts.pop();
      const d = parts.join(sep);
      return d === "" ? (isAbs(p) ? sep : ".") : d;
    },
    basename(p, ext) {
      p = String(p);
      const parts = p.split(splitRe).filter((x) => x !== "");
      let base = parts.length ? parts[parts.length - 1] : "";
      if (ext && base.endsWith(ext) && base !== ext) base = base.slice(0, -ext.length);
      return base;
    },
    extname(p) {
      const base = path.basename(String(p));
      const i = base.lastIndexOf(".");
      return i <= 0 ? "" : base.slice(i);
    },
    parse(p) {
      const root = isAbs(p) ? sep : "";
      const dir = path.dirname(p);
      const base = path.basename(p);
      const ext = path.extname(base);
      return { root, dir: dir === "." && root ? root : dir, base, ext, name: ext ? base.slice(0, -ext.length) : base };
    },
    format(obj) {
      const dir = obj.dir || obj.root || "";
      const base = obj.base || (obj.name || "") + (obj.ext || "");
      if (!dir) return base;
      return dir === obj.root ? dir + base : dir + sep + base;
    },
    relative(from, to) {
      from = path.resolve(from);
      to = path.resolve(to);
      if (from === to) return "";
      const fromParts = from.split(splitRe).filter(Boolean);
      const toParts = to.split(splitRe).filter(Boolean);
      let i = 0;
      while (i < fromParts.length && i < toParts.length && fromParts[i] === toParts[i]) i++;
      const up = fromParts.slice(i).map(() => "..");
      return [...up, ...toParts.slice(i)].join(sep) || ".";
    },
    matchesGlob(p, pattern) {
      if (typeof p !== "string") throw new TypeError('The "path" argument must be of type string');
      if (typeof pattern !== "string") throw new TypeError('The "pattern" argument must be of type string');
      const gSegs = pattern.replace(/\\/g, "/").split("/").map((s) => (s === "**" ? GLOBSTAR : segToRegExp(s)));
      const pSegs = (isWin ? p.replace(/\\/g, "/") : p).split("/");
      return matchSegs(pSegs, 0, gSegs, 0);
    },
    // Long-path form on win32 (\\?\C:\… / \\?\UNC\…); a pass-through on posix. Unlike Node we
    // prefix without resolving against the (posix) cwd.
    toNamespacedPath(p) {
      if (!isWin || typeof p !== "string" || p.length === 0) return p;
      if (p.startsWith("\\\\?\\") || p.startsWith("\\\\.\\")) return p;
      if (/^[a-zA-Z]:[\\/]/.test(p)) return "\\\\?\\" + p;
      if (/^[\\/][\\/]/.test(p)) return "\\\\?\\UNC\\" + p.slice(2);
      return p;
    },
    matchesGlob(p, pattern) {
      p = String(p);
      pattern = String(pattern);
      if (isWin) { p = p.replace(/\\/g, "/"); pattern = pattern.replace(/\\/g, "/"); }
      let re = "";
      for (let i = 0; i < pattern.length; i++) {
        const c = pattern[i];
        if (c === "*") {
          if (pattern[i + 1] === "*") { re += ".*"; i++; if (pattern[i + 1] === "/") i++; }
          else re += "[^/]*";
        } else if (c === "?") re += "[^/]";
        else if ("\\^$.|+()[]{}".includes(c)) re += "\\" + c;
        else re += c;
      }
      return new RegExp("^" + re + "$").test(p);
    },
  };
  path._makeLong = path.toNamespacedPath; // legacy alias Node still exports
  return path;
}

const posix = makePath("/", false);
const win32 = makePath("\\", true);
applyWin32Roots(win32);
posix.posix = posix;
posix.win32 = win32;
win32.posix = posix;
win32.win32 = win32;

const isWindows = os_platform_is_win();
__builtins.set("path", isWindows ? win32 : posix);
// `path.posix` / `path.win32` are also require-able as their own submodules — the *same* objects,
// so `require('path/posix') === require('path').posix`.
__builtins.set("path/posix", posix);
__builtins.set("path/win32", win32);

// Windows paths have roots the separator-only algorithm above cannot see: a drive (`C:\`, or
// drive-relative `C:foo`), a UNC share (`\\server\share\`), a device namespace (`\\?\C:\`,
// `\\.\pipe\x`), or a bare `\` (the current drive's root). Every operation that cares about the
// root is replaced here; the rest (extname, format, matchesGlob) is root-agnostic.
function applyWin32Roots(path) {
  const isSep = (c) => c === "\\" || c === "/";
  const splitRe = /[\\/]+/;

  // { device, absolute, rest }: `device` is `C:` or `\\server\share` (or `\\?\…` / `\\.\…`
  // up to its first component), `absolute` whether a separator follows it, `rest` the remainder.
  function splitRoot(p) {
    if (p.length >= 2 && isSep(p[0]) && isSep(p[1])) {
      // `\\?\C:\x` and `\\.\pipe\x`: the namespace prefix plus its first component.
      const ns = /^[\\/][\\/]([?.])[\\/]([^\\/]+)(?:[\\/]|$)(.*)$/s.exec(p);
      if (ns) {
        const device = `\\\\${ns[1]}\\${ns[2]}`;
        return { device, absolute: true, rest: ns[3] };
      }
      const unc = /^[\\/][\\/]([^\\/]+)[\\/]+([^\\/]+)(?:[\\/]|$)(.*)$/s.exec(p);
      if (unc) return { device: `\\\\${unc[1]}\\${unc[2]}`, absolute: true, rest: unc[3] };
      return { device: "", absolute: true, rest: p.replace(/^[\\/]+/, "") };
    }
    if (/^[a-zA-Z]:/.test(p)) {
      const absolute = p.length > 2 && isSep(p[2]);
      return { device: p.slice(0, 2), absolute, rest: p.slice(absolute ? 3 : 2) };
    }
    if (p.length > 0 && isSep(p[0])) return { device: "", absolute: true, rest: p.slice(1) };
    return { device: "", absolute: false, rest: p };
  }

  function normalizeParts(rest, absolute) {
    const res = [];
    for (const part of rest.split(splitRe)) {
      if (part === "" || part === ".") continue;
      if (part === "..") {
        if (res.length && res[res.length - 1] !== "..") res.pop();
        else if (!absolute) res.push("..");
      } else {
        res.push(part);
      }
    }
    return res;
  }

  const rootOf = (r) => r.device + (r.absolute ? "\\" : "");
  const sameDevice = (a, b) => a.toLowerCase() === b.toLowerCase();

  path.isAbsolute = (p) => splitRoot(String(p)).absolute;

  path.normalize = (p) => {
    p = String(p);
    if (p.length === 0) return ".";
    const r = splitRoot(p);
    let tail = normalizeParts(r.rest, r.absolute).join("\\");
    if (!tail && !r.absolute) tail = ".";
    if (tail && isSep(p[p.length - 1])) tail += "\\";
    return rootOf(r) + tail;
  };

  path.join = (...args) => {
    const joined = args.map(String).filter((a) => a !== "").join("\\");
    return joined === "" ? "." : path.normalize(joined);
  };

  path.resolve = (...args) => {
    let device = "";
    let tail = "";
    let absolute = false;
    for (let i = args.length - 1; i >= -1; i--) {
      let p;
      if (i >= 0) {
        p = String(args[i]);
        if (p === "") continue;
      } else {
        // Out of arguments: the cwd, unless it is on another drive than the one already found
        // (Node consults `=C:` there; a realm has no per-drive cwd, so that drive's root).
        p = process.cwd();
        if (device && !sameDevice(splitRoot(p).device, device)) p = device + "\\";
      }
      const r = splitRoot(p);
      if (r.device) {
        if (device && !sameDevice(r.device, device)) continue;
        if (!device) device = r.device;
      }
      if (!absolute) {
        tail = r.rest + "\\" + tail;
        absolute = r.absolute;
      }
      if (absolute && device) break;
    }
    const parts = normalizeParts(tail, absolute).join("\\");
    return device + (absolute ? "\\" : "") + parts || ".";
  };

  path.dirname = (p) => {
    p = String(p);
    const r = splitRoot(p);
    const parts = r.rest.split(splitRe).filter((x) => x !== "");
    if (parts.length <= 1) return rootOf(r) || ".";
    parts.pop();
    return rootOf(r) + parts.join("\\");
  };

  path.basename = (p, ext) => {
    p = String(p);
    const parts = splitRoot(p).rest.split(splitRe).filter((x) => x !== "");
    let base = parts.length ? parts[parts.length - 1] : "";
    if (ext && base.endsWith(ext) && base !== ext) base = base.slice(0, -ext.length);
    return base;
  };

  path.parse = (p) => {
    p = String(p);
    const r = splitRoot(p);
    const root = rootOf(r);
    const base = path.basename(p);
    const ext = path.extname(base);
    const parts = r.rest.split(splitRe).filter((x) => x !== "");
    const dir = parts.length <= 1 ? root : path.dirname(p);
    return { root, dir, base, ext, name: ext ? base.slice(0, -ext.length) : base };
  };

  path.relative = (from, to) => {
    from = path.resolve(from);
    to = path.resolve(to);
    if (from.toLowerCase() === to.toLowerCase()) return "";
    const f = splitRoot(from);
    const t = splitRoot(to);
    if (!sameDevice(f.device, t.device)) return to;
    const fromParts = f.rest.split(splitRe).filter(Boolean);
    const toParts = t.rest.split(splitRe).filter(Boolean);
    let i = 0;
    while (
      i < fromParts.length &&
      i < toParts.length &&
      fromParts[i].toLowerCase() === toParts[i].toLowerCase()
    ) i++;
    const up = fromParts.slice(i).map(() => "..");
    return [...up, ...toParts.slice(i)].join("\\") || ".";
  };

  path.toNamespacedPath = (p) => {
    if (typeof p !== "string" || p.length === 0) return p;
    if (/^[\\/][\\/][?.][\\/]/.test(p)) return p;
    const resolved = path.resolve(p);
    if (/^[a-zA-Z]:\\/.test(resolved)) return "\\\\?\\" + resolved;
    if (resolved.startsWith("\\\\")) return "\\\\?\\UNC\\" + resolved.slice(2);
    return p;
  };
  path._makeLong = path.toNamespacedPath;
}

function os_platform_is_win() {
  // __os.info() is available (os.js runs after this file? no — before). Use the raw op.
  return __os.info().platform === "win32";
}
