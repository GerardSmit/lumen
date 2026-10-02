
// ---- registration ------------------------------------------------------------------------------

const fs = require("fs");

// Node's synchronous glob subset, over the real fs binding and shared path matcher.
// Extglobs and braces crossing directory separators are explicitly unsupported.
fs.globSync = function globSync(pattern, options = {}) {
  const path = __builtins.get("path");
  const patterns = Array.isArray(pattern) ? pattern : [pattern];
  if (patterns.some((p) => typeof p !== "string")) throw new TypeError("globSync pattern must be a string or string array");
  if (options === null || typeof options !== "object") throw new TypeError("globSync options must be an object");
  for (const key of Object.keys(options)) {
    if (!["cwd", "exclude", "withFileTypes"].includes(key)) throw new Error(`globSync option '${key}' is not supported in lumen`);
  }
  if (options.withFileTypes !== undefined && typeof options.withFileTypes !== "boolean") throw new TypeError("globSync withFileTypes must be a boolean");
  let cwd = options.cwd === undefined ? process.cwd() : options.cwd;
  if (cwd instanceof URL) cwd = __builtins.get("url").fileURLToPath(cwd);
  if (typeof cwd !== "string") throw new TypeError("globSync cwd must be a string or file URL");
  cwd = path.resolve(cwd);
  const exclude = options.exclude;
  if (exclude !== undefined && typeof exclude !== "function" && (!Array.isArray(exclude) || exclude.some((p) => typeof p !== "string"))) {
    throw new TypeError("globSync exclude must be a function or string array");
  }
  const validate = (p) => {
    if (/[?*+!@]\(/.test(p) || /\{[^}]*[\\/][^}]*\}/.test(p)) throw new Error("globSync extglobs and braces crossing directories are not supported in lumen");
    return process.platform === "win32" ? p.replace(/\\/g, "/") : p;
  };
  const exclusions = Array.isArray(exclude) ? exclude.map(validate) : [];
  const results = [];
  const seen = new Set();
  for (const raw of patterns) {
    const glob = validate(raw);
    const absolute = path.isAbsolute(glob);
    const segments = glob.split("/");
    const firstMagic = segments.findIndex((s) => /[*?\[{]/.test(s));
    const base = firstMagic < 0 ? path.dirname(glob) : segments.slice(0, firstMagic).join("/") || (absolute ? "/" : ".");
    const start = path.resolve(cwd, base);
    const stack = [start];
    const hasGlobstar = segments.includes("**");
    if (typeof exclude === "function" && !hasGlobstar) throw new Error("globSync exclude callbacks without a globstar are not supported in lumen");
    const maxDepth = firstMagic < 0 ? 1 : segments.length - firstMagic;
    const hidden = segments.some((s) => s.startsWith(".") && s !== "." && s !== "..");
    const normalized = glob.replace(/^\.\//, "").replace(/\/$/, "");
    // A trailing globstar includes the directory it starts in, unlike matchesGlob's
    // path-only trailing-globstar rule. Obtain the root metadata from the filesystem.
    const directoryPattern = normalized === "**" ? "." : normalized.endsWith("/**") ? normalized.slice(0, -3) : undefined;
    const rootOutput = absolute ? start : path.relative(cwd, start) || ".";
    if (directoryPattern !== undefined && path.matchesGlob(rootOutput, directoryPattern)) {
      let st;
      try { st = fs.lstatSync(start); } catch (error) { if (!["ENOENT", "ENOTDIR"].includes(error.code)) throw error; }
      if (st) {
        const type = st.isSymbolicLink() ? 3 : st.isDirectory() ? 2 : st.isFile() ? 1 : st.isBlockDevice() ? 4 : st.isCharacterDevice() ? 5 : st.isSocket() ? 6 : st.isFIFO() ? 7 : 0;
        const entry = new fs.Dirent(rootOutput === "." ? "." : path.basename(start), type, path.dirname(start));
        entry.parentPath = path.dirname(start);
        const excluded = typeof exclude === "function" ? start !== cwd && exclude(options.withFileTypes ? entry : rootOutput) : exclusions.some((p) => path.matchesGlob(rootOutput, p));
        if (!excluded && !seen.has(start)) { seen.add(start); results.push(options.withFileTypes ? entry : rootOutput); }
        if (excluded) continue;
      }
    }
    while (stack.length) {
      const dir = stack.pop();
      let entries;
      try { entries = fs.readdirSync(dir, { withFileTypes: true }); }
      catch (error) { if (["ENOENT", "ENOTDIR"].includes(error.code)) continue; throw error; }
      for (const entry of entries) {
        entry.parentPath = dir;
        if (!hidden && entry.name.startsWith(".")) continue;
        const full = path.join(dir, entry.name);
        const rel = path.relative(cwd, full);
        const output = absolute ? full : rel;
        const matchPath = process.platform === "win32" ? output.replace(/\\/g, "/") : output;
        const excluded = typeof exclude === "function" ? exclude(options.withFileTypes ? entry : output) : exclusions.some((p) => path.matchesGlob(matchPath, p));
        if (excluded) continue;
        const directoryMatch = directoryPattern !== undefined && (entry.isDirectory() || entry.isSymbolicLink()) && path.matchesGlob(matchPath, directoryPattern);
        if ((path.matchesGlob(matchPath, normalized) || directoryMatch) && !seen.has(full)) {
          seen.add(full);
          results.push(options.withFileTypes ? entry : output);
        }
        // Never descend symlinks: the directory walk cannot follow a cycle or leave its tree
        // through a link. Non-globstar patterns bound descent to their remaining segments.
        const depth = path.relative(start, full).split(path.sep).length;
        if (entry.isDirectory() && (hasGlobstar || depth < maxDepth)) stack.push(full);
      }
    }
  }
  return results;
};

__builtins.set("fs", fs);
__builtins.set("fs/promises", __lazyValue(() => fs.promises));
// The module table, for --expose-internals (internals.js).
__internals.set("fsRequire", require);
