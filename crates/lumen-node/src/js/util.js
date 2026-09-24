// node:util — the slice real-world code uses: inherits, format/formatWithOptions, a practical
// inspect, deprecate, promisify/callbackify, types.*, and the legacy is* predicates. inspect is
// not byte-identical to Node's (that formatter is enormous) but covers the shapes debug output
// and error messages need.

function inherits(ctor, superCtor) {
  if (ctor === undefined || ctor === null) throw new TypeError('The "ctor" argument must be a function');
  if (superCtor === undefined || superCtor === null) throw new TypeError('The "superCtor" argument must be a function');
  if (superCtor.prototype === undefined) throw new TypeError('The "superCtor.prototype" property must not be undefined');
  Object.defineProperty(ctor, "super_", { value: superCtor, writable: true, configurable: true });
  Object.setPrototypeOf(ctor.prototype, superCtor.prototype);
}

// ---- inspect ----------------------------------------------------------------------------------
// Node's util.inspect algorithm: the same option set and defaults, the same line-breaking rules
// (`compact: 3`, `breakLength: 128`, array grouping into columns), quoting, key rendering,
// circular-reference markers, class/function/error/boxed-primitive bases and prefixes such as
// `Map(1) {`, `[Object: null prototype] {` and `Foo [bar] {`. Tests and tooling compare its output
// byte for byte, so the rules follow Node's rather than being approximated.

const inspectDefaultOptions = Object.seal({
  showHidden: false,
  depth: 2,
  colors: false,
  customInspect: true,
  showProxy: false,
  maxArrayLength: 100,
  maxStringLength: 10000,
  breakLength: 80,
  compact: 3,
  sorted: false,
  getters: false,
  numericSeparator: false,
});

const kObjectType = 0;
const kArrayType = 1;
const kArrayExtrasType = 2;

const builtInObjects = new Set(
  Object.getOwnPropertyNames(globalThis).filter((e) => /^[A-Z][a-zA-Z0-9]+$/.test(e)),
);

const strEscapeSequencesRegExp = /[\x00-\x1f\x27\x5c\x7f-\x9f]|[\ud800-\udbff](?![\udc00-\udfff])|(?<![\ud800-\udbff])[\udc00-\udfff]/;
const strEscapeSequencesReplacer = /[\x00-\x1f\x27\x5c\x7f-\x9f]|[\ud800-\udbff](?![\udc00-\udfff])|(?<![\ud800-\udbff])[\udc00-\udfff]/g;
const strEscapeSequencesRegExpSingle = /[\x00-\x1f\x5c\x7f-\x9f]|[\ud800-\udbff](?![\udc00-\udfff])|(?<![\ud800-\udbff])[\udc00-\udfff]/;
const strEscapeSequencesReplacerSingle = /[\x00-\x1f\x5c\x7f-\x9f]|[\ud800-\udbff](?![\udc00-\udfff])|(?<![\ud800-\udbff])[\udc00-\udfff]/g;
const keyStrRegExp = /^[a-zA-Z_][a-zA-Z_0-9]*$/;
const numberRegExp = /^(0|[1-9][0-9]*)$/;
const classRegExp = /^(\s+[^(]*?)\s*{/;
const stripCommentsRegExp = /(\/\/.*?\n)|(\/\*(.|\n)*?\*\/)/g;
const colorRegExp = /\u001b\[\d\d?m/g;

// Escaped control characters (plus ' and \): the table Node's strEscape uses.
const meta = [
  "\\x00", "\\x01", "\\x02", "\\x03", "\\x04", "\\x05", "\\x06", "\\x07",
  "\\b", "\\t", "\\n", "\\x0B", "\\f", "\\r", "\\x0E", "\\x0F",
  "\\x10", "\\x11", "\\x12", "\\x13", "\\x14", "\\x15", "\\x16", "\\x17",
  "\\x18", "\\x19", "\\x1A", "\\x1B", "\\x1C", "\\x1D", "\\x1E", "\\x1F",
  "", "", "", "", "", "", "", "\\'", "", "", "", "", "", "", "", "",
  "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
  "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
  "", "", "", "", "", "", "", "", "", "", "", "", "\\\\", "", "", "",
  "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
  "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "\\x7F",
  "\\x80", "\\x81", "\\x82", "\\x83", "\\x84", "\\x85", "\\x86", "\\x87",
  "\\x88", "\\x89", "\\x8A", "\\x8B", "\\x8C", "\\x8D", "\\x8E", "\\x8F",
  "\\x90", "\\x91", "\\x92", "\\x93", "\\x94", "\\x95", "\\x96", "\\x97",
  "\\x98", "\\x99", "\\x9A", "\\x9B", "\\x9C", "\\x9D", "\\x9E", "\\x9F",
];

const escapeFn = (str) => meta[str.charCodeAt(0)] || `\\u${str.charCodeAt(0).toString(16)}`;

function addQuotes(str, quotes) {
  if (quotes === -1) return `"${str}"`;
  if (quotes === -2) return `\`${str}\``;
  return `'${str}'`;
}

function strEscape(str) {
  let escapeTest = strEscapeSequencesRegExp;
  let escapeReplace = strEscapeSequencesReplacer;
  let singleQuote = 39;
  // Prefer double quotes, then backticks, when the string contains single quotes; fall back to
  // escaping the single quotes.
  if (str.includes("'")) {
    if (!str.includes('"')) singleQuote = -1;
    else if (!str.includes("`") && !str.includes("${")) singleQuote = -2;
    if (singleQuote !== 39) {
      escapeTest = strEscapeSequencesRegExpSingle;
      escapeReplace = strEscapeSequencesReplacerSingle;
    }
  }
  if (str.length < 5000 && escapeTest.exec(str) === null) return addQuotes(str, singleQuote);
  if (str.length > 100) return addQuotes(str.replace(escapeReplace, escapeFn), singleQuote);
  let result = "";
  let last = 0;
  for (let i = 0; i < str.length; i++) {
    const point = str.charCodeAt(i);
    if (point === singleQuote || point === 92 || point < 32 || (point > 126 && point < 160)) {
      result += last === i ? meta[point] : `${str.slice(last, i)}${meta[point]}`;
      last = i + 1;
    } else if (point >= 0xd800 && point <= 0xdfff) {
      if (point <= 0xdbff && i + 1 < str.length) {
        const next = str.charCodeAt(i + 1);
        if (next >= 0xdc00 && next <= 0xdfff) {
          i++;
          continue;
        }
      }
      result += `${str.slice(last, i)}\\u${point.toString(16)}`;
      last = i + 1;
    }
  }
  if (last !== str.length) result += str.slice(last);
  return addQuotes(result, singleQuote);
}

const inspectColors = Object.assign(Object.create(null), {
  reset: [0, 0], bold: [1, 22], dim: [2, 22], italic: [3, 23], underline: [4, 24], blink: [5, 25],
  inverse: [7, 27], hidden: [8, 28], strikethrough: [9, 29], doubleunderline: [21, 24],
  black: [30, 39], red: [31, 39], green: [32, 39], yellow: [33, 39], blue: [34, 39],
  magenta: [35, 39], cyan: [36, 39], white: [37, 39], bgBlack: [40, 49], bgRed: [41, 49],
  bgGreen: [42, 49], bgYellow: [43, 49], bgBlue: [44, 49], bgMagenta: [45, 49], bgCyan: [46, 49],
  bgWhite: [47, 49], framed: [51, 54], overlined: [53, 55], gray: [90, 39], redBright: [91, 39],
  greenBright: [92, 39], yellowBright: [93, 39], blueBright: [94, 39], magentaBright: [95, 39],
  cyanBright: [96, 39], whiteBright: [97, 39], bgGray: [100, 49], bgRedBright: [101, 49],
  bgGreenBright: [102, 49], bgYellowBright: [103, 49], bgBlueBright: [104, 49],
  bgMagentaBright: [105, 49], bgCyanBright: [106, 49], bgWhiteBright: [107, 49],
});
for (const [alias, target] of [
  ["grey", "gray"], ["blackBright", "gray"], ["bgGrey", "bgGray"], ["bgBlackBright", "bgGray"],
  ["dim", "faint"], ["strikethrough", "crossedout"], ["strikethrough", "strikeThrough"],
  ["strikethrough", "crossedOut"], ["hidden", "conceal"], ["inverse", "swapColors"],
  ["inverse", "swapcolors"], ["doubleunderline", "doubleUnderline"],
]) {
  const [from, to] = inspectColors[alias] ? [alias, target] : [target, alias];
  Object.defineProperty(inspectColors, to, {
    get() { return this[from]; },
    set(value) { this[from] = value; },
    configurable: true,
    enumerable: false,
  });
}
const inspectStyles = Object.assign(Object.create(null), {
  special: "cyan", number: "yellow", bigint: "yellow", boolean: "yellow", undefined: "grey",
  null: "bold", string: "green", symbol: "green", date: "magenta", regexp: "red", module: "underline",
});

function stylizeWithColor(str, styleType) {
  const style = inspectStyles[styleType];
  if (style !== undefined) {
    const color = inspectColors[style];
    if (color !== undefined) return `\u001b[${color[0]}m${str}\u001b[${color[1]}m`;
  }
  return str;
}
function stylizeNoColor(str) {
  return str;
}

const removeColors = (str) => String(str).replace(colorRegExp, "");

function isFullWidthCodePoint(code) {
  return code >= 0x1100 && (
    code <= 0x115f || code === 0x2329 || code === 0x232a ||
    (code >= 0x2e80 && code <= 0x3247 && code !== 0x303f) ||
    (code >= 0x3250 && code <= 0x4dbf) || (code >= 0x4e00 && code <= 0xa4c6) ||
    (code >= 0xa960 && code <= 0xa97c) || (code >= 0xac00 && code <= 0xd7a3) ||
    (code >= 0xf900 && code <= 0xfaff) || (code >= 0xfe10 && code <= 0xfe19) ||
    (code >= 0xfe30 && code <= 0xfe6b) || (code >= 0xff01 && code <= 0xff60) ||
    (code >= 0xffe0 && code <= 0xffe6) || (code >= 0x1b000 && code <= 0x1b001) ||
    (code >= 0x1f200 && code <= 0x1f251) || (code >= 0x1f300 && code <= 0x1f64f) ||
    (code >= 0x20000 && code <= 0x3fffd));
}
function isZeroWidthCodePoint(code) {
  return code <= 0x1f || (code >= 0x7f && code <= 0x9f) || (code >= 0x300 && code <= 0x36f) ||
    (code >= 0x200b && code <= 0x200f) || (code >= 0x20d0 && code <= 0x20ff) ||
    (code >= 0xfe00 && code <= 0xfe0f) || (code >= 0xfe20 && code <= 0xfe2f) ||
    (code >= 0xe0100 && code <= 0xe01ef);
}
function getStringWidth(str, removeControlChars = true) {
  let width = 0;
  if (removeControlChars) str = stripVTControlCharacters(str);
  str = str.normalize("NFC");
  for (const char of str) {
    const code = char.codePointAt(0);
    if (isFullWidthCodePoint(code)) width += 2;
    else if (!isZeroWidthCodePoint(code)) width++;
  }
  return width;
}

function inspect(value, opts) {
  const ctx = {
    budget: {},
    indentationLvl: 0,
    seen: [],
    currentDepth: 0,
    stylize: stylizeNoColor,
    showHidden: inspectDefaultOptions.showHidden,
    depth: inspectDefaultOptions.depth,
    colors: inspectDefaultOptions.colors,
    customInspect: inspectDefaultOptions.customInspect,
    showProxy: inspectDefaultOptions.showProxy,
    maxArrayLength: inspectDefaultOptions.maxArrayLength,
    maxStringLength: inspectDefaultOptions.maxStringLength,
    breakLength: inspectDefaultOptions.breakLength,
    compact: inspectDefaultOptions.compact,
    sorted: inspectDefaultOptions.sorted,
    getters: inspectDefaultOptions.getters,
    numericSeparator: inspectDefaultOptions.numericSeparator,
  };
  if (arguments.length > 1) {
    // Legacy signature: inspect(value, showHidden, depth, colors).
    if (arguments.length > 2) {
      if (arguments[2] !== undefined) ctx.depth = arguments[2];
      if (arguments.length > 3 && arguments[3] !== undefined) ctx.colors = arguments[3];
    }
    if (typeof opts === "boolean") {
      ctx.showHidden = opts;
    } else if (opts) {
      for (const key of Object.keys(opts)) {
        if (Object.prototype.hasOwnProperty.call(inspectDefaultOptions, key) || key === "stylize") {
          ctx[key] = opts[key];
        } else if (ctx.userOptions === undefined) {
          // User-defined options are passed through to custom inspect functions.
          ctx.userOptions = opts;
        }
      }
    }
  }
  if (ctx.colors) ctx.stylize = stylizeWithColor;
  if (ctx.maxArrayLength === null) ctx.maxArrayLength = Infinity;
  if (ctx.maxStringLength === null) ctx.maxStringLength = Infinity;
  return formatValue(ctx, value, 0);
}
inspect.custom = Symbol.for("nodejs.util.inspect.custom");
Object.defineProperty(inspect, "defaultOptions", {
  get() { return inspectDefaultOptions; },
  set(options) {
    if (options === null || typeof options !== "object") {
      throw new __errors.ERR_INVALID_ARG_TYPE("options", "Object", options);
    }
    return Object.assign(inspectDefaultOptions, options);
  },
  enumerable: true,
  configurable: true,
});
inspect.colors = inspectColors;
inspect.styles = inspectStyles;

function getUserOptions(ctx, isCrossContext) {
  const ret = {
    stylize: ctx.stylize,
    showHidden: ctx.showHidden,
    depth: ctx.depth,
    colors: ctx.colors,
    customInspect: ctx.customInspect,
    showProxy: ctx.showProxy,
    maxArrayLength: ctx.maxArrayLength,
    maxStringLength: ctx.maxStringLength,
    breakLength: ctx.breakLength,
    compact: ctx.compact,
    sorted: ctx.sorted,
    getters: ctx.getters,
    numericSeparator: ctx.numericSeparator,
    ...ctx.userOptions,
  };
  if (isCrossContext) {
    Object.setPrototypeOf(ret, null);
    for (const key of Object.keys(ret)) {
      if ((typeof ret[key] === "object" || typeof ret[key] === "function") && ret[key] !== null) delete ret[key];
    }
    ret.stylize = Object.setPrototypeOf((value, flavour) => {
      let stylized;
      try { stylized = `${ctx.stylize(value, flavour)}`; } catch { /* ignore */ }
      if (typeof stylized !== "string") return value;
      return stylized;
    }, null);
  }
  return ret;
}

function formatValue(ctx, value, recurseTimes, typedArray) {
  if (typeof value !== "object" && typeof value !== "function") {
    return formatPrimitive(ctx.stylize, value, ctx);
  }
  if (value === null) return ctx.stylize("null", "null");

  const context = value;
  const proxy = __node.proxyParts(value);
  if (proxy !== undefined) {
    if (proxy[0] === null || proxy[0] === undefined) return ctx.stylize("<Revoked Proxy>", "special");
    if (ctx.showProxy) return formatProxy(ctx, proxy, recurseTimes);
    // Inspect the target so no traps run.
    value = proxy[0];
    let nested = __node.proxyParts(value);
    while (nested !== undefined) {
      if (nested[0] === null || nested[0] === undefined) return ctx.stylize("<Revoked Proxy>", "special");
      value = nested[0];
      nested = __node.proxyParts(value);
    }
  }

  if (ctx.customInspect) {
    const maybeCustom = value[inspect.custom];
    if (typeof maybeCustom === "function" && maybeCustom !== inspect &&
        !(value.constructor && value.constructor.prototype === value)) {
      const depth = ctx.depth === null ? null : ctx.depth - recurseTimes;
      const isCrossContext = proxy !== undefined;
      const ret = maybeCustom.call(context, depth, getUserOptions(ctx, isCrossContext), inspect);
      if (ret !== context) {
        if (typeof ret !== "string") return formatValue(ctx, ret, recurseTimes);
        return ret.replaceAll("\n", `\n${" ".repeat(ctx.indentationLvl)}`);
      }
    }
  }

  if (ctx.seen.includes(value)) {
    let index = 1;
    if (ctx.circular === undefined) {
      ctx.circular = new Map();
      ctx.circular.set(value, index);
    } else {
      index = ctx.circular.get(value);
      if (index === undefined) {
        index = ctx.circular.size + 1;
        ctx.circular.set(value, index);
      }
    }
    return ctx.stylize(`[Circular *${index}]`, "special");
  }
  return formatRaw(ctx, value, recurseTimes, typedArray);
}

function formatProxy(ctx, proxy, recurseTimes) {
  if (recurseTimes > ctx.depth && ctx.depth !== null) return ctx.stylize("Proxy [Array]", "special");
  recurseTimes += 1;
  ctx.indentationLvl += 2;
  const res = [formatValue(ctx, proxy[0], recurseTimes), formatValue(ctx, proxy[1], recurseTimes)];
  ctx.indentationLvl -= 2;
  return reduceToSingleString(ctx, res, "", ["Proxy [", "]"], kArrayExtrasType, recurseTimes);
}

function isInstanceof(object, proto) {
  try { return object instanceof proto; } catch { return false; }
}

// The V8 class name of an object whose prototype chain has no named constructor.
function internalConstructorName(obj) {
  const tag = tagOf(obj);
  return tag === "Object" || tag === "" ? "Object" : tag;
}

function getConstructorName(obj, ctx, recurseTimes) {
  let firstProto;
  const tmp = obj;
  while (obj) {
    const descriptor = Object.getOwnPropertyDescriptor(obj, "constructor");
    if (descriptor !== undefined && typeof descriptor.value === "function" &&
        descriptor.value.name !== "" && isInstanceof(tmp, descriptor.value)) {
      return String(descriptor.value.name);
    }
    obj = Object.getPrototypeOf(obj);
    if (firstProto === undefined) firstProto = obj;
  }
  if (firstProto === null) return null;
  const res = internalConstructorName(tmp);
  if (recurseTimes > ctx.depth && ctx.depth !== null) return `${res} <Complex prototype>`;
  const protoConstr = getConstructorName(firstProto, ctx, recurseTimes + 1);
  if (protoConstr === null) {
    return `${res} <${inspect(firstProto, { ...ctx, customInspect: false, depth: -1 })}>`;
  }
  return `${res} <${protoConstr}>`;
}

function getPrefix(constructor, tag, fallback, size = "") {
  if (constructor === null) {
    if (tag !== "" && fallback !== tag) return `[${fallback}${size}: null prototype] [${tag}] `;
    return `[${fallback}${size}: null prototype] `;
  }
  if (tag !== "" && constructor !== tag) return `${constructor}${size} [${tag}] `;
  return `${constructor}${size} `;
}

function getKeys(value, showHidden) {
  let keys;
  const symbols = Object.getOwnPropertySymbols(value);
  if (showHidden) {
    keys = Object.getOwnPropertyNames(value);
    if (symbols.length !== 0) keys.push(...symbols);
  } else {
    try {
      keys = Object.keys(value);
    } catch {
      keys = Object.getOwnPropertyNames(value);
    }
    if (symbols.length !== 0) keys.push(...symbols.filter((key) => Object.prototype.propertyIsEnumerable.call(value, key)));
  }
  return keys;
}

const isArrayIndexKey = (key) => typeof key === "string" && numberRegExp.test(key) && Number(key) < 2 ** 32 - 1;

function getOwnNonIndexProperties(value, showHidden) {
  const out = [];
  for (const key of Reflect.ownKeys(value)) {
    if (isArrayIndexKey(key)) continue;
    if (!showHidden && !Object.prototype.propertyIsEnumerable.call(value, key)) continue;
    out.push(key);
  }
  return out;
}

function getFunctionBase(value, constructor, tag) {
  let stringified;
  try { stringified = Function.prototype.toString.call(value); } catch { stringified = ""; }
  if (stringified.startsWith("class") && stringified.endsWith("}")) {
    const slice = stringified.slice(5, -1);
    const bracketIndex = slice.indexOf("{");
    if (bracketIndex !== -1 &&
        (!slice.slice(0, bracketIndex).includes("(") ||
          classRegExp.exec(slice.replace(stripCommentsRegExp, "")) !== null)) {
      return getClassBase(value, constructor, tag);
    }
  }
  let type = "Function";
  const fnTag = tagOf(value);
  if (fnTag === "GeneratorFunction") type = `Generator${type}`;
  else if (fnTag === "AsyncGeneratorFunction") type = `AsyncGenerator${type}`;
  else if (fnTag === "AsyncFunction") type = `Async${type}`;
  let base = `[${type}`;
  if (constructor === null) base += " (null prototype)";
  if (value.name === "") base += " (anonymous)";
  else base += `: ${value.name}`;
  base += "]";
  if (constructor !== type && constructor !== null) base += ` ${constructor}`;
  if (tag !== "" && constructor !== tag) base += ` [${tag}]`;
  return base;
}

function getClassBase(value, constructor, tag) {
  const hasName = Object.prototype.hasOwnProperty.call(value, "name");
  const name = (hasName && value.name) || "(anonymous)";
  let base = `class ${name}`;
  if (constructor !== "Function" && constructor !== null) base += ` [${constructor}]`;
  if (tag !== "" && constructor !== tag) base += ` [${tag}]`;
  if (constructor !== null) {
    const superName = Object.getPrototypeOf(value).name;
    if (superName) base += ` extends ${superName}`;
  } else {
    base += " extends [null prototype]";
  }
  return `[${base}]`;
}

function getStackString(error) {
  return error.stack ? String(error.stack) : Error.prototype.toString.call(error);
}

function removeDuplicateErrorKeys(ctx, keys, err, stack) {
  if (!ctx.showHidden && keys.length !== 0) {
    for (const name of ["name", "message", "stack"]) {
      const index = keys.indexOf(name);
      if (index !== -1 && stack.includes(err[name])) keys.splice(index, 1);
    }
  }
}

function improveStack(stack, constructor, name, tag) {
  let len = name.length;
  if (constructor === null ||
      (name.endsWith("Error") && stack.startsWith(name) &&
        (stack.length === len || stack[len] === ":" || stack[len] === "\n"))) {
    let fallback = "Error";
    if (constructor === null) {
      const start = /^([A-Z][a-z_ A-Z0-9[\]()-]+)(?::|\n\s+at)/.exec(stack) || /^([a-z_A-Z0-9-]*Error)$/.exec(stack);
      fallback = (start && start[1]) || "";
      len = fallback.length;
      fallback = fallback || "Error";
    }
    const prefix = getPrefix(constructor, tag, fallback).slice(0, -1);
    if (name !== prefix) {
      if (prefix.includes(name)) {
        stack = len === 0 ? `${prefix}: ${stack}` : `${prefix}${stack.slice(len)}`;
      } else {
        stack = `${prefix} [${name}]${stack.slice(len)}`;
      }
    }
  }
  return stack;
}

function formatError(err, constructor, tag, ctx, keys) {
  const name = err.name != null ? String(err.name) : "Error";
  let stack = getStackString(err);
  removeDuplicateErrorKeys(ctx, keys, err, stack);
  if ("cause" in err && (keys.length === 0 || !keys.includes("cause"))) keys.push("cause");
  if (Array.isArray(err.errors) && (keys.length === 0 || !keys.includes("errors"))) keys.push("errors");
  stack = improveStack(stack, constructor, name, tag);

  // Ignore the error message if it's contained in the stack.
  let pos = (err.message && stack.indexOf(err.message)) || -1;
  if (pos !== -1) pos += err.message.length;
  const stackStart = stack.indexOf("\n    at", pos);
  if (stackStart === -1) {
    stack = `[${stack}]`;
  } else if (ctx.colors) {
    let newStack = stack.slice(0, stackStart);
    for (const line of stack.slice(stackStart + 1).split("\n")) {
      newStack += `\n${/\(node:|\(internal\/|^\s+at node:/.test(line) ? ctx.stylize(line, "undefined") : line}`;
    }
    stack = newStack;
  }
  if (ctx.indentationLvl !== 0) {
    stack = stack.replace(/\n/g, `\n${" ".repeat(ctx.indentationLvl)}`);
  }
  return stack;
}

function getBoxedBase(value, ctx, keys, constructor, tag) {
  let fn;
  let type;
  const boxedTag = tagOf(value);
  if (boxedTag === "Number") { fn = Number.prototype.valueOf; type = "Number"; }
  else if (boxedTag === "String") {
    fn = String.prototype.valueOf;
    type = "String";
    // Drop the index keys of the string's characters.
    keys.splice(0, value.length);
  } else if (boxedTag === "Boolean") { fn = Boolean.prototype.valueOf; type = "Boolean"; }
  else if (boxedTag === "BigInt") { fn = BigInt.prototype.valueOf; type = "BigInt"; }
  else { fn = Symbol.prototype.valueOf; type = "Symbol"; }
  let base = `[${type}`;
  if (type !== constructor) {
    base += constructor === null ? " (null prototype)" : ` (${constructor})`;
  }
  base += `: ${formatPrimitive(stylizeNoColor, fn.call(value), ctx)}]`;
  if (tag !== "" && tag !== constructor) base += ` [${tag}]`;
  if (keys.length !== 0 || ctx.stylize === stylizeNoColor) return base;
  return ctx.stylize(base, type.toLowerCase());
}

const TYPED_ARRAY_TAGS = new Set([
  "Int8Array", "Uint8Array", "Uint8ClampedArray", "Int16Array", "Uint16Array", "Int32Array",
  "Uint32Array", "Float16Array", "Float32Array", "Float64Array", "BigInt64Array", "BigUint64Array",
]);
const typedArrayTag = (v) => {
  if (!ArrayBuffer.isView(v)) return undefined;
  const t = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(Uint8Array.prototype), Symbol.toStringTag).get.call(v);
  return TYPED_ARRAY_TAGS.has(t) ? t : undefined;
};

function isErrorValue(value) {
  return value instanceof Error || tagOf(value) === "Error";
}

function formatRaw(ctx, value, recurseTimes, typedArray) {
  let keys;
  const constructor = getConstructorName(value, ctx, recurseTimes);
  let tag = value[Symbol.toStringTag];
  // Only list the tag in case it's non-enumerable / not an own property, otherwise it would be
  // printed twice.
  if (typeof tag !== "string" ||
      (tag !== "" &&
        (ctx.showHidden ? Object.prototype.hasOwnProperty : Object.prototype.propertyIsEnumerable).call(value, Symbol.toStringTag))) {
    tag = "";
  }
  let base = "";
  let formatter = () => [];
  let braces;
  let noIterator = true;
  let extrasType = kObjectType;
  const valueTag = tagOf(value);

  if (Symbol.iterator in value || constructor === null) {
    noIterator = false;
    const taTag = typedArrayTag(value);
    if (Array.isArray(value)) {
      const prefix = constructor !== "Array" || tag !== "" ? getPrefix(constructor, tag, "Array", `(${value.length})`) : "";
      keys = getOwnNonIndexProperties(value, ctx.showHidden);
      braces = [`${prefix}[`, "]"];
      if (value.length === 0 && keys.length === 0) return `${braces[0]}]`;
      extrasType = kArrayExtrasType;
      formatter = formatArray;
    } else if (valueTag === "Set" && isSetValue(value)) {
      const size = Set.prototype.has && Reflect.apply(Object.getOwnPropertyDescriptor(Set.prototype, "size").get, value, []);
      const prefix = getPrefix(constructor, tag, "Set", `(${size})`);
      keys = getKeys(value, ctx.showHidden);
      formatter = (c, v, r) => formatSet(v, c, r);
      if (size === 0 && keys.length === 0) return `${prefix}{}`;
      braces = [`${prefix}{`, "}"];
    } else if (valueTag === "Map" && isMapValue(value)) {
      const size = Reflect.apply(Object.getOwnPropertyDescriptor(Map.prototype, "size").get, value, []);
      const prefix = getPrefix(constructor, tag, "Map", `(${size})`);
      keys = getKeys(value, ctx.showHidden);
      formatter = (c, v, r) => formatMap(v, c, r);
      if (size === 0 && keys.length === 0) return `${prefix}{}`;
      braces = [`${prefix}{`, "}"];
    } else if (taTag !== undefined) {
      keys = getOwnNonIndexProperties(value, ctx.showHidden);
      let bound = value;
      let fallback = "";
      if (constructor === null) {
        fallback = taTag;
        bound = new globalThis[taTag](value);
      }
      const size = value.length;
      const prefix = getPrefix(constructor, tag, fallback, `(${size})`);
      braces = [`${prefix}[`, "]"];
      if (value.length === 0 && keys.length === 0 && !ctx.showHidden) return `${braces[0]}]`;
      formatter = (c, v, r) => formatTypedArray(bound, size, c, r);
      extrasType = kArrayExtrasType;
    } else if (valueTag === "Map Iterator") {
      keys = getKeys(value, ctx.showHidden);
      braces = [`[${tag || "Map Iterator"}] {`, "}"];
      formatter = () => [];
    } else if (valueTag === "Set Iterator") {
      keys = getKeys(value, ctx.showHidden);
      braces = [`[${tag || "Set Iterator"}] {`, "}"];
      formatter = () => [];
    } else {
      noIterator = true;
    }
  }
  if (noIterator) {
    keys = getKeys(value, ctx.showHidden);
    braces = ["{", "}"];
    if (constructor === "Object") {
      if (valueTag === "Arguments") {
        braces[0] = "[Arguments] {";
      } else if (tag !== "") {
        braces[0] = `${getPrefix(constructor, tag, "Object")}{`;
      }
      if (keys.length === 0) return `${braces[0]}}`;
    } else if (typeof value === "function") {
      base = getFunctionBase(value, constructor, tag);
      if (keys.length === 0) return ctx.stylize(base, "special");
    } else if (valueTag === "RegExp") {
      base = RegExp.prototype.toString.call(constructor !== null ? value : new RegExp(value));
      const prefix = getPrefix(constructor, tag, "RegExp");
      if (prefix !== "RegExp ") base = `${prefix}${base}`;
      if (keys.length === 0 || (recurseTimes > ctx.depth && ctx.depth !== null)) return ctx.stylize(base, "regexp");
    } else if (valueTag === "Date") {
      const time = Date.prototype.getTime.call(value);
      base = Number.isNaN(time) ? Date.prototype.toString.call(value) : Date.prototype.toISOString.call(value);
      const prefix = getPrefix(constructor, tag, "Date");
      if (prefix !== "Date ") base = `${prefix}${base}`;
      if (keys.length === 0) return ctx.stylize(base, "date");
    } else if (isErrorValue(value)) {
      base = formatError(value, constructor, tag, ctx, keys);
      if (keys.length === 0) return base;
    } else if (valueTag === "ArrayBuffer" || valueTag === "SharedArrayBuffer") {
      const prefix = getPrefix(constructor, tag, valueTag);
      if (typedArray === undefined) {
        formatter = formatArrayBuffer;
      } else if (keys.length === 0) {
        return prefix + `{ byteLength: ${formatNumber(ctx.stylize, value.byteLength, false)} }`;
      }
      braces[0] = `${prefix}{`;
      keys.unshift("byteLength");
    } else if (valueTag === "DataView") {
      braces[0] = `${getPrefix(constructor, tag, "DataView")}{`;
      keys.unshift("byteLength", "byteOffset", "buffer");
    } else if (valueTag === "Promise" && __node.promiseState(value) !== undefined) {
      braces[0] = `${getPrefix(constructor, tag, "Promise")}{`;
      formatter = formatPromise;
    } else if (valueTag === "WeakSet") {
      braces[0] = `${getPrefix(constructor, tag, "WeakSet")}{`;
      formatter = formatWeakCollection;
    } else if (valueTag === "WeakMap") {
      braces[0] = `${getPrefix(constructor, tag, "WeakMap")}{`;
      formatter = formatWeakCollection;
    } else if (valueTag === "Module") {
      braces[0] = `${getPrefix(constructor, tag, "Module")}{`;
      formatter = (c, v, r) => formatNamespaceObject(keys, c, v, r);
    } else if (BOXED_TAGS.has(valueTag) && typeof value === "object" && isBoxedPrimitiveValue(value, valueTag)) {
      base = getBoxedBase(value, ctx, keys, constructor, tag);
      if (keys.length === 0) return base;
    } else {
      if (keys.length === 0) return `${getPrefix(constructor, tag, "Object")}{}`;
      braces[0] = `${getPrefix(constructor, tag, "Object")}{`;
    }
  }

  if (recurseTimes > ctx.depth && ctx.depth !== null) {
    let constructorName = getPrefix(constructor, tag, "Object").slice(0, -1);
    if (constructor !== null) constructorName = `[${constructorName}]`;
    return ctx.stylize(constructorName, "special");
  }
  recurseTimes += 1;
  ctx.seen.push(value);
  ctx.currentDepth = recurseTimes;
  let output;
  const indentationLvl = ctx.indentationLvl;
  try {
    output = formatter(ctx, value, recurseTimes);
    for (let i = 0; i < keys.length; i++) {
      output.push(formatProperty(ctx, value, recurseTimes, keys[i], extrasType));
    }
  } catch (err) {
    if (err instanceof RangeError && /call stack/i.test(err.message)) {
      ctx.seen.pop();
      ctx.indentationLvl = indentationLvl;
      const constructorName = getPrefix(constructor, tag, "Object").slice(0, -1);
      return ctx.stylize(`[${constructorName}: Inspection interrupted prematurely. Maximum call stack size exceeded.]`, "special");
    }
    throw err;
  }
  if (ctx.circular !== undefined) {
    const index = ctx.circular.get(value);
    if (index !== undefined) {
      const reference = ctx.stylize(`<ref *${index}>`, "special");
      if (ctx.compact !== true) base = base === "" ? reference : `${reference} ${base}`;
      else braces[0] = `${reference} ${braces[0]}`;
    }
  }
  ctx.seen.pop();

  if (ctx.sorted) {
    const comparator = ctx.sorted === true ? undefined : ctx.sorted;
    if (extrasType === kObjectType) {
      output.sort(comparator);
    } else if (keys.length > 1) {
      const sorted = output.slice(output.length - keys.length).sort(comparator);
      output.splice(output.length - keys.length, keys.length, ...sorted);
    }
  }

  const res = reduceToSingleString(ctx, output, base, braces, extrasType, recurseTimes, value);
  const budget = ctx.budget[ctx.indentationLvl] || 0;
  const newLength = budget + res.length;
  ctx.budget[ctx.indentationLvl] = newLength;
  // Stop descending once the output is enormous (> 128 MiB), as Node does.
  if (newLength > 2 ** 27) ctx.depth = -1;
  return res;
}

function isSetValue(v) {
  try { Reflect.apply(Object.getOwnPropertyDescriptor(Set.prototype, "size").get, v, []); return true; } catch { return false; }
}
function isMapValue(v) {
  try { Reflect.apply(Object.getOwnPropertyDescriptor(Map.prototype, "size").get, v, []); return true; } catch { return false; }
}
function isBoxedPrimitiveValue(v, tag) {
  const proto = { Number: Number.prototype, String: String.prototype, Boolean: Boolean.prototype, BigInt: BigInt.prototype, Symbol: Symbol.prototype }[tag];
  try { proto.valueOf.call(v); return true; } catch { return false; }
}

function formatNumber(fn, number, numericSeparator) {
  if (!numericSeparator) {
    if (Object.is(number, -0)) return fn("-0", "number");
    return fn(`${number}`, "number");
  }
  const integer = Math.trunc(number);
  const string = String(integer);
  if (integer === number) {
    if (!Number.isFinite(number) || string.includes("e")) return fn(string, "number");
    return fn(`${addNumericSeparator(string)}`, "number");
  }
  if (Number.isNaN(number)) return fn(string, "number");
  return fn(`${addNumericSeparator(string)}.${addNumericSeparatorEnd(String(number).slice(string.length + 1))}`, "number");
}

function addNumericSeparator(integerString) {
  let result = "";
  let i = integerString.length;
  const start = integerString.startsWith("-") ? 1 : 0;
  for (; i >= start + 4; i -= 3) result = `_${integerString.slice(i - 3, i)}${result}`;
  return i === integerString.length ? integerString : `${integerString.slice(0, i)}${result}`;
}

function addNumericSeparatorEnd(integerString) {
  let result = "";
  let i = 0;
  for (; i < integerString.length - 3; i += 3) result += `${integerString.slice(i, i + 3)}_`;
  return i === 0 ? integerString : `${result}${integerString.slice(i)}`;
}

function formatBigInt(fn, bigint, numericSeparator) {
  const string = String(bigint);
  if (!numericSeparator) return fn(`${string}n`, "bigint");
  return fn(`${addNumericSeparator(string)}n`, "bigint");
}

function formatPrimitive(fn, value, ctx) {
  if (typeof value === "string") {
    let trailer = "";
    if (value.length > ctx.maxStringLength) {
      const remaining = value.length - ctx.maxStringLength;
      value = value.slice(0, ctx.maxStringLength);
      trailer = `... ${remaining} more character${remaining > 1 ? "s" : ""}`;
    }
    if (ctx.compact !== true &&
        // Strings longer than 16 characters that do not fit the line are split on newlines.
        value.length > 16 &&
        value.length > ctx.breakLength - ctx.indentationLvl - 4) {
      return value
        .split(/(?<=\n)/)
        .map((line) => fn(strEscape(line), "string"))
        .join(` +\n${" ".repeat(ctx.indentationLvl + 2)}`) + trailer;
    }
    return fn(strEscape(value), "string") + trailer;
  }
  if (typeof value === "number") return formatNumber(fn, value, ctx.numericSeparator);
  if (typeof value === "bigint") return formatBigInt(fn, value, ctx.numericSeparator);
  if (typeof value === "boolean") return fn(`${value}`, "boolean");
  if (typeof value === "undefined") return fn("undefined", "undefined");
  // es6 symbol primitive
  return fn(Symbol.prototype.toString.call(value), "symbol");
}

function formatNamespaceObject(keys, ctx, value, recurseTimes) {
  const output = new Array(keys.length);
  for (let i = 0; i < keys.length; i++) {
    try {
      output[i] = formatProperty(ctx, value, recurseTimes, keys[i], kObjectType);
    } catch {
      // An uninitialized binding (TDZ) of a module namespace.
      const tmp = { [keys[i]]: "" };
      output[i] = formatProperty(ctx, tmp, recurseTimes, keys[i], kObjectType);
      const pos = output[i].lastIndexOf(" ");
      output[i] = output[i].slice(0, pos + 1) + ctx.stylize("<uninitialized>", "special");
    }
  }
  // Reset the keys to an empty array. This prevents duplicated inspection.
  keys.length = 0;
  return output;
}

function formatSpecialArray(ctx, value, recurseTimes, maxLength, output, i) {
  const keys = Object.keys(value);
  let index = i;
  for (; i < keys.length && output.length < maxLength; i++) {
    const key = keys[i];
    const tmp = +key;
    // Arrays can only have up to 2^32 - 1 entries.
    if (tmp > 2 ** 32 - 2) break;
    if (`${index}` !== key) {
      if (numberRegExp.exec(key) === null) break;
      const emptyItems = tmp - index;
      const ending = emptyItems > 1 ? "s" : "";
      output.push(ctx.stylize(`<${emptyItems} empty item${ending}>`, "undefined"));
      index = tmp;
      if (output.length === maxLength) break;
    }
    output.push(formatProperty(ctx, value, recurseTimes, key, kArrayType));
    index++;
  }
  const remaining = value.length - index;
  if (output.length !== maxLength) {
    if (remaining > 0) {
      const ending = remaining > 1 ? "s" : "";
      output.push(ctx.stylize(`<${remaining} empty item${ending}>`, "undefined"));
    }
  } else if (remaining > 0) {
    output.push(remainingText(remaining));
  }
  return output;
}

const remainingText = (remaining) => `... ${remaining} more item${remaining > 1 ? "s" : ""}`;

function formatArrayBuffer(ctx, value) {
  let buffer;
  try {
    buffer = new Uint8Array(value);
  } catch {
    return [ctx.stylize("(detached)", "special")];
  }
  let hex = "";
  const n = Math.min(ctx.maxArrayLength, buffer.length);
  for (let i = 0; i < n; i++) hex += (i === 0 ? "" : " ") + (buffer[i] < 16 ? "0" : "") + buffer[i].toString(16);
  const remaining = buffer.length - ctx.maxArrayLength;
  if (remaining > 0) hex += ` ... ${remaining} more byte${remaining > 1 ? "s" : ""}`;
  return [`${ctx.stylize("[Uint8Contents]", "special")}: <${hex}>`];
}

function formatArray(ctx, value, recurseTimes) {
  const valLen = value.length;
  const len = Math.min(Math.max(0, ctx.maxArrayLength), valLen);
  const remaining = valLen - len;
  const output = [];
  for (let i = 0; i < len; i++) {
    // Special handle sparse arrays.
    if (!Object.prototype.hasOwnProperty.call(value, i)) {
      return formatSpecialArray(ctx, value, recurseTimes, len, output, i);
    }
    output.push(formatProperty(ctx, value, recurseTimes, i, kArrayType));
  }
  if (remaining > 0) output.push(remainingText(remaining));
  return output;
}

function formatTypedArray(value, length, ctx, recurseTimes) {
  const maxLength = Math.min(Math.max(0, ctx.maxArrayLength), length);
  const remaining = value.length - maxLength;
  const output = new Array(maxLength);
  const elementFormatter = value.length > 0 && typeof value[0] === "number" ? formatNumber : formatBigInt;
  for (let i = 0; i < maxLength; ++i) output[i] = elementFormatter(ctx.stylize, value[i], ctx.numericSeparator);
  if (remaining > 0) output[maxLength] = remainingText(remaining);
  if (ctx.showHidden) {
    // .buffer goes last, it's not a primitive like the others.
    ctx.indentationLvl += 2;
    for (const key of ["BYTES_PER_ELEMENT", "length", "byteLength", "byteOffset", "buffer"]) {
      const str = formatValue(ctx, value[key], recurseTimes, true);
      output.push(`[${key}]: ${str}`);
    }
    ctx.indentationLvl -= 2;
  }
  return output;
}

function formatSet(value, ctx, recurseTimes) {
  const length = value.size;
  const maxLength = Math.min(Math.max(0, ctx.maxArrayLength), length);
  const remaining = length - maxLength;
  const output = [];
  ctx.indentationLvl += 2;
  let i = 0;
  for (const v of Set.prototype.values.call(value)) {
    if (i >= maxLength) break;
    output.push(formatValue(ctx, v, recurseTimes));
    i++;
  }
  if (remaining > 0) output.push(remainingText(remaining));
  ctx.indentationLvl -= 2;
  return output;
}

function formatMap(value, ctx, recurseTimes) {
  const length = value.size;
  const maxLength = Math.min(Math.max(0, ctx.maxArrayLength), length);
  const remaining = length - maxLength;
  const output = [];
  ctx.indentationLvl += 2;
  let i = 0;
  for (const [k, v] of Map.prototype.entries.call(value)) {
    if (i >= maxLength) break;
    output.push(`${formatValue(ctx, k, recurseTimes)} => ${formatValue(ctx, v, recurseTimes)}`);
    i++;
  }
  if (remaining > 0) output.push(remainingText(remaining));
  ctx.indentationLvl -= 2;
  return output;
}

function formatWeakCollection(ctx) {
  return [ctx.stylize("<items unknown>", "special")];
}

function formatPromise(ctx, value, recurseTimes) {
  const [state, result] = __node.promiseState(value);
  if (state === 0) return [ctx.stylize("<pending>", "special")];
  ctx.indentationLvl += 2;
  const str = formatValue(ctx, result, recurseTimes);
  ctx.indentationLvl -= 2;
  return [state === 2 ? `${ctx.stylize("<rejected>", "special")} ${str}` : str];
}

function formatProperty(ctx, value, recurseTimes, key, type, desc, original = value) {
  let name;
  let str;
  let extra = " ";
  desc = desc || Object.getOwnPropertyDescriptor(value, key) || { value: value[key], enumerable: true };
  if (desc.value !== undefined) {
    const diff = ctx.compact !== true || type !== kObjectType ? 2 : 3;
    ctx.indentationLvl += diff;
    str = formatValue(ctx, desc.value, recurseTimes);
    if (diff === 3 && ctx.breakLength < getStringWidth(str, ctx.colors)) {
      extra = `\n${" ".repeat(ctx.indentationLvl)}`;
    }
    ctx.indentationLvl -= diff;
  } else if (desc.get !== undefined) {
    const label = desc.set !== undefined ? "Getter/Setter" : "Getter";
    const s = ctx.stylize;
    const sp = "special";
    if (ctx.getters && (ctx.getters === true ||
          (ctx.getters === "get" && desc.set === undefined) ||
          (ctx.getters === "set" && desc.set !== undefined))) {
      try {
        const tmp = desc.get.call(original);
        ctx.indentationLvl += 2;
        if (tmp === null) {
          str = `${s(`[${label}:`, sp)} ${s("null", "null")}${s("]", sp)}`;
        } else if (typeof tmp === "object") {
          str = `${s(`[${label}]`, sp)} ${formatValue(ctx, tmp, recurseTimes)}`;
        } else {
          const primitive = formatPrimitive(s, tmp, ctx);
          str = `${s(`[${label}:`, sp)} ${primitive}${s("]", sp)}`;
        }
        ctx.indentationLvl -= 2;
      } catch (err) {
        const message = `<Inspection threw (${err.message})>`;
        str = `${s(`[${label}:`, sp)} ${message}${s("]", sp)}`;
      }
    } else {
      str = ctx.stylize(`[${label}]`, sp);
    }
  } else if (desc.set !== undefined) {
    str = ctx.stylize("[Setter]", "special");
  } else {
    str = ctx.stylize("undefined", "undefined");
  }
  if (type === kArrayType) return str;
  if (typeof key === "symbol") {
    const tmp = Symbol.prototype.toString.call(key).replace(strEscapeSequencesReplacer, escapeFn);
    name = `[${ctx.stylize(tmp, "symbol")}]`;
  } else if (key === "__proto__") {
    name = "['__proto__']";
  } else if (desc.enumerable === false) {
    const tmp = String(key).replace(strEscapeSequencesReplacer, escapeFn);
    name = `[${tmp}]`;
  } else if (keyStrRegExp.exec(key) !== null) {
    name = ctx.stylize(key, "name");
  } else {
    name = ctx.stylize(strEscape(String(key)), "string");
  }
  return `${name}:${extra}${str}`;
}

function isBelowBreakLength(ctx, output, start, base) {
  let totalLength = output.length + start;
  if (totalLength + output.length > ctx.breakLength) return false;
  for (let i = 0; i < output.length; i++) {
    totalLength += ctx.colors ? removeColors(output[i]).length : output[i].length;
    if (totalLength > ctx.breakLength) return false;
  }
  // Do not line up properties on the same line if `base` contains line breaks.
  return base === "" || !base.includes("\n");
}

function groupArrayElements(ctx, output, value) {
  let totalLength = 0;
  let maxLength = 0;
  let i = 0;
  let outputLength = output.length;
  if (ctx.maxArrayLength < output.length) {
    // This makes sure the "... n more items" part is not taken into account.
    outputLength--;
  }
  const separatorSpace = 2; // Add 1 for the space and 1 for the separator.
  const dataLen = new Array(outputLength);
  for (; i < outputLength; i++) {
    const len = getStringWidth(output[i], ctx.colors);
    dataLen[i] = len;
    totalLength += len + separatorSpace;
    if (maxLength < len) maxLength = len;
  }
  const actualMax = maxLength + separatorSpace;
  if (actualMax * 3 + ctx.indentationLvl < ctx.breakLength &&
      (totalLength / actualMax > 5 || maxLength <= 6)) {
    const approxCharHeights = 2.5;
    const averageBias = Math.sqrt(actualMax - totalLength / output.length);
    const biasedMax = Math.max(actualMax - 3 - averageBias, 1);
    const columns = Math.min(
      Math.round(Math.sqrt(approxCharHeights * biasedMax * outputLength) / biasedMax),
      Math.floor((ctx.breakLength - ctx.indentationLvl) / actualMax),
      ctx.compact * 4,
      15,
    );
    if (columns <= 1) return output;
    const tmp = [];
    const maxLineLength = [];
    for (let i = 0; i < columns; i++) {
      let lineLength = 0;
      for (let j = i; j < output.length; j += columns) {
        if (dataLen[j] > lineLength) lineLength = dataLen[j];
      }
      maxLineLength.push(lineLength + separatorSpace);
    }
    let order = String.prototype.padStart;
    if (value !== undefined) {
      for (let i = 0; i < output.length; i++) {
        if (typeof value[i] !== "number" && typeof value[i] !== "bigint") {
          order = String.prototype.padEnd;
          break;
        }
      }
    }
    for (let i = 0; i < outputLength; i += columns) {
      const max = Math.min(i + columns, outputLength);
      let str = "";
      let j = i;
      for (; j < max - 1; j++) {
        const padding = maxLineLength[j - i] + output[j].length - dataLen[j];
        str += order.call(`${output[j]}, `, padding, " ");
      }
      if (order === String.prototype.padStart) {
        const padding = maxLineLength[j - i] + output[j].length - dataLen[j] - separatorSpace;
        str += output[j].padStart(padding, " ");
      } else {
        str += output[j];
      }
      tmp.push(str);
    }
    if (ctx.maxArrayLength < output.length) tmp.push(output[outputLength]);
    output = tmp;
  }
  return output;
}

function reduceToSingleString(ctx, output, base, braces, extrasType, recurseTimes, value) {
  if (ctx.compact !== true) {
    if (typeof ctx.compact === "number" && ctx.compact >= 1) {
      const entries = output.length;
      if (extrasType === kArrayExtrasType && entries > 6) {
        output = groupArrayElements(ctx, output, value);
      }
      if (ctx.currentDepth - recurseTimes < ctx.compact && entries === output.length) {
        // Fits on a single line when the combined length stays below `breakLength`.
        const start = output.length + ctx.indentationLvl + braces[0].length + base.length + 10;
        if (isBelowBreakLength(ctx, output, start, base)) {
          const joinedOutput = output.join(", ");
          if (!joinedOutput.includes("\n")) {
            return `${base ? `${base} ` : ""}${braces[0]} ${joinedOutput}` + ` ${braces[1]}`;
          }
        }
      }
    }
    const indentation = `\n${" ".repeat(ctx.indentationLvl)}`;
    return `${base ? `${base} ` : ""}${braces[0]}${indentation}  ${output.join(`,${indentation}  `)}${indentation}${braces[1]}`;
  }
  if (isBelowBreakLength(ctx, output, 0, base)) {
    return `${braces[0]}${base ? ` ${base}` : ""} ${output.join(", ")} ` + braces[1];
  }
  const indentation = " ".repeat(ctx.indentationLvl);
  const ln = base === "" && braces[0].length === 1 ? " " : `${base ? ` ${base}` : ""}\n${indentation}  `;
  return `${braces[0]}${ln}${output.join(`,\n${indentation}  `)} ${braces[1]}`;
}

// ---- format -----------------------------------------------------------------------------------

function hasBuiltInToString(value) {
  const proxy = __node.proxyParts(value);
  if (proxy !== undefined) {
    if (proxy[0] === null || proxy[0] === undefined) return true;
    value = proxy[0];
  }
  // Count objects that have no `toString` function as built-in.
  if (typeof value.toString !== "function") return true;
  // An own `toString` property is not a built-in one.
  if (Object.prototype.hasOwnProperty.call(value, "toString")) return false;
  let pointer = value;
  do {
    pointer = Object.getPrototypeOf(pointer);
  } while (!Object.prototype.hasOwnProperty.call(pointer, "toString"));
  const descriptor = Object.getOwnPropertyDescriptor(pointer, "constructor");
  return descriptor !== undefined && typeof descriptor.value === "function" && builtInObjects.has(descriptor.value.name);
}

function tryStringify(arg) {
  try {
    return JSON.stringify(arg);
  } catch (err) {
    if (err && /circular/i.test(err.message)) return "[Circular]";
    throw err;
  }
}

function formatWithOptions(inspectOptions, ...args) {
  if (inspectOptions === null || typeof inspectOptions !== "object") {
    throw new __errors.ERR_INVALID_ARG_TYPE("inspectOptions", "Object", inspectOptions);
  }
  return formatWithOptionsInternal(inspectOptions, args);
}

function formatWithOptionsInternal(inspectOptions, args) {
  const first = args[0];
  let a = 0;
  let str = "";
  let join = "";
  if (typeof first === "string") {
    if (args.length === 1) return first;
    let tempStr;
    let lastPos = 0;
    for (let i = 0; i < first.length - 1; i++) {
      if (first.charCodeAt(i) === 37) { // '%'
        const nextChar = first.charCodeAt(++i);
        if (a + 1 !== args.length) {
          switch (nextChar) {
            case 115: { // 's'
              const tempArg = args[++a];
              if (typeof tempArg === "number") tempStr = formatNumber(stylizeNoColor, tempArg, false);
              else if (typeof tempArg === "bigint") tempStr = formatBigInt(stylizeNoColor, tempArg, false);
              else if (typeof tempArg !== "object" || tempArg === null || !hasBuiltInToString(tempArg)) tempStr = String(tempArg);
              else tempStr = inspect(tempArg, { ...inspectOptions, depth: 0, colors: false, compact: 3 });
              break;
            }
            case 106: // 'j'
              tempStr = tryStringify(args[++a]);
              break;
            case 100: { // 'd'
              const tempNum = args[++a];
              if (typeof tempNum === "bigint") tempStr = formatBigInt(stylizeNoColor, tempNum, false);
              else if (typeof tempNum === "symbol") tempStr = "NaN";
              else tempStr = formatNumber(stylizeNoColor, Number(tempNum), false);
              break;
            }
            case 79: // 'O'
              tempStr = inspect(args[++a], inspectOptions);
              break;
            case 111: // 'o'
              tempStr = inspect(args[++a], { ...inspectOptions, showHidden: true, showProxy: true, depth: 4 });
              break;
            case 105: { // 'i'
              const tempInteger = args[++a];
              if (typeof tempInteger === "bigint") tempStr = formatBigInt(stylizeNoColor, tempInteger, false);
              else if (typeof tempInteger === "symbol") tempStr = "NaN";
              else tempStr = formatNumber(stylizeNoColor, Number.parseInt(tempInteger), false);
              break;
            }
            case 102: { // 'f'
              const tempFloat = args[++a];
              if (typeof tempFloat === "symbol") tempStr = "NaN";
              else tempStr = formatNumber(stylizeNoColor, Number.parseFloat(tempFloat), false);
              break;
            }
            case 99: // 'c'
              a += 1;
              tempStr = "";
              break;
            case 37: // '%'
              str += first.slice(lastPos, i);
              lastPos = i + 1;
              continue;
            default: // Any other character is not a correct placeholder.
              continue;
          }
          if (lastPos !== i - 1) str += first.slice(lastPos, i - 1);
          str += tempStr;
          lastPos = i + 1;
        } else if (nextChar === 37) {
          str += first.slice(lastPos, i);
          lastPos = i + 1;
        }
      }
    }
    if (lastPos !== 0) {
      a++;
      join = " ";
      if (lastPos < first.length) str += first.slice(lastPos);
    }
  }
  while (a < args.length) {
    const value = args[a];
    str += join;
    str += typeof value !== "string" ? inspect(value, inspectOptions) : value;
    join = " ";
    a++;
  }
  return str;
}

function format(...args) {
  return formatWithOptionsInternal(undefined, args);
}


// ---- deprecate / promisify / callbackify ------------------------------------------------------

function deprecate(fn, msg, code) {
  let warned = false;
  function deprecated(...args) {
    if (!warned) {
      warned = true;
      if (typeof console !== "undefined" && console.error) {
        console.error(`DeprecationWarning:${code ? ` [${code}]` : ""} ${msg}`);
      }
    }
    return Reflect.apply(fn, this, args);
  }
  return deprecated;
}

const kCustomPromisify = Symbol.for("nodejs.util.promisify.custom");

// fs.read/fs.write/dns.lookup resolve several callback values as a named object.
const kCustomPromisifyArgs = Symbol("customPromisifyArgs");
__internals.set("customPromisifyArgs", kCustomPromisifyArgs);

function promisify(original) {
  __validators.validateFunction(original, "original");
  if (original[kCustomPromisify]) {
    const fn = original[kCustomPromisify];
    __validators.validateFunction(fn, "util.promisify.custom");
    return Object.defineProperty(fn, kCustomPromisify, {
      __proto__: null, value: fn, enumerable: false, writable: false, configurable: true,
    });
  }
  const argumentNames = original[kCustomPromisifyArgs];
  function fn(...args) {
    return new Promise((resolve, reject) => {
      args.push((err, ...values) => {
        if (err) return reject(err);
        if (argumentNames !== undefined && values.length > 1) {
          const obj = {};
          for (let i = 0; i < argumentNames.length; i++) obj[argumentNames[i]] = values[i];
          resolve(obj);
        } else {
          resolve(values[0]);
        }
      });
      Reflect.apply(original, this, args);
    });
  }
  Object.setPrototypeOf(fn, Object.getPrototypeOf(original));
  Object.defineProperty(fn, kCustomPromisify, {
    __proto__: null, value: fn, enumerable: false, writable: false, configurable: true,
  });
  return Object.defineProperties(fn, Object.getOwnPropertyDescriptors(original));
}
promisify.custom = kCustomPromisify;

function callbackify(original) {
  if (typeof original !== "function") throw new TypeError('The "original" argument must be of type function');
  return function (...args) {
    const cb = args.pop();
    original.apply(this, args).then(
      (value) => queueMicrotask(() => cb(null, value)),
      (err) => queueMicrotask(() => cb(err || new Error("Promise was rejected with a falsy value"))),
    );
  };
}

// ---- util/types -------------------------------------------------------------------------------
// The engine gives every builtin an accurate `Object.prototype.toString` tag (Map, Promise,
// GeneratorFunction, Map Iterator, boxed Number/String/…, ArrayBuffer vs SharedArrayBuffer, …),
// so most of these predicates are exact. The few the engine cannot observe are noted inline and
// return false honestly rather than guessing.

const objToString = Object.prototype.toString;
const tagOf = (v) => {
  const s = objToString.call(v);
  return s.slice(8, s.length - 1); // "[object X]" -> "X"
};
const isObjectValue = (v) => v !== null && typeof v === "object";
const BOXED_TAGS = new Set(["Number", "String", "Boolean", "Symbol", "BigInt"]);

// Brand checks, as V8's: an internal-slot probe (a prototype getter or method that throws on a
// foreign receiver), so neither Symbol.toStringTag nor a borrowed prototype can spoof them.
const isObjectLike = (v) => v !== null && (typeof v === "object" || typeof v === "function");
const brandGetter = (C, prop) => {
  const get = Object.getOwnPropertyDescriptor(C.prototype, prop).get;
  return (v) => {
    if (!isObjectLike(v)) return false;
    try { get.call(v); return true; } catch { return false; }
  };
};
const brandMethod = (fn, ...args) => (v) => {
  if (!isObjectLike(v)) return false;
  try { fn.call(v, ...args); return true; } catch { return false; }
};
// %TypedArray%.prototype[@@toStringTag] is the spec's typed-array brand probe.
const typedArrayName = (() => {
  const get = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(Uint8Array.prototype), Symbol.toStringTag).get;
  return (v) => get.call(v);
})();
const isTypedArrayOf = (name) => (v) => typedArrayName(v) === name;
const isArrayBufferBrand = brandGetter(ArrayBuffer, "byteLength");
const isSharedArrayBufferBrand = typeof SharedArrayBuffer === "function"
  ? brandGetter(SharedArrayBuffer, "byteLength") : () => false;
const isRegExpBrand = brandGetter(RegExp, "source");

const types = {
  // C++ external pointers have no JS representation in lumen, so nothing is ever an External.
  isExternal: () => false,
  isProxy: (v) => __node.isProxy(v),
  // crypto loads after util, so resolve the constructor lazily when the predicate is called.
  isKeyObject: (v) => {
    const crypto = __builtins.get("crypto");
    return !!crypto && typeof crypto.KeyObject === "function" && v instanceof crypto.KeyObject;
  },

  isDate: brandMethod(Date.prototype.getTime),
  isRegExp: (v) => v !== RegExp.prototype && isRegExpBrand(v),
  isArgumentsObject: (v) => tagOf(v) === "Arguments",
  isNativeError: (v) => v instanceof Error,
  isMap: brandGetter(Map, "size"),
  isSet: brandGetter(Set, "size"),
  isMapIterator: (v) => tagOf(v) === "Map Iterator",
  isSetIterator: (v) => tagOf(v) === "Set Iterator",
  isWeakMap: brandMethod(WeakMap.prototype.has, {}),
  isWeakSet: brandMethod(WeakSet.prototype.has, {}),
  isPromise: (v) => isObjectLike(v) && __node.promiseState(v) !== undefined,
  isGeneratorFunction: (v) => tagOf(v) === "GeneratorFunction" || tagOf(v) === "AsyncGeneratorFunction",
  isAsyncFunction: (v) => tagOf(v) === "AsyncFunction" || tagOf(v) === "AsyncGeneratorFunction",
  isGeneratorObject: (v) => tagOf(v) === "Generator",
  isModuleNamespaceObject: (v) => tagOf(v) === "Module",

  isNumberObject: brandMethod(Number.prototype.valueOf),
  isStringObject: brandMethod(String.prototype.valueOf),
  isBooleanObject: brandMethod(Boolean.prototype.valueOf),
  isSymbolObject: brandMethod(Symbol.prototype.valueOf),
  isBigIntObject: brandMethod(BigInt.prototype.valueOf),
  isBoxedPrimitive: (v) => types.isNumberObject(v) || types.isStringObject(v) || types.isBooleanObject(v)
    || types.isSymbolObject(v) || types.isBigIntObject(v),

  isArrayBuffer: isArrayBufferBrand,
  isSharedArrayBuffer: isSharedArrayBufferBrand,
  isAnyArrayBuffer: (v) => isArrayBufferBrand(v) || isSharedArrayBufferBrand(v),
  isDataView: brandGetter(DataView, "byteLength"),
  isArrayBufferView: (v) => ArrayBuffer.isView(v),
  isTypedArray: (v) => typedArrayName(v) !== undefined,
  isUint8Array: isTypedArrayOf("Uint8Array"),
  isUint8ClampedArray: isTypedArrayOf("Uint8ClampedArray"),
  isUint16Array: isTypedArrayOf("Uint16Array"),
  isUint32Array: isTypedArrayOf("Uint32Array"),
  isInt8Array: isTypedArrayOf("Int8Array"),
  isInt16Array: isTypedArrayOf("Int16Array"),
  isInt32Array: isTypedArrayOf("Int32Array"),
  isFloat16Array: isTypedArrayOf("Float16Array"),
  isFloat32Array: isTypedArrayOf("Float32Array"),
  isFloat64Array: isTypedArrayOf("Float64Array"),
  isBigInt64Array: isTypedArrayOf("BigInt64Array"),
  isBigUint64Array: isTypedArrayOf("BigUint64Array"),

  // CryptoKey is a WebCrypto global in lumen, so this one is observable.
  isCryptoKey: (v) => typeof CryptoKey !== "undefined" && v instanceof CryptoKey,
};

__builtins.set("util/types", types);

// ---- coded errors -----------------------------------------------------------------------------
// Node attaches a stable `.code` to these errors; downstream code (and our own tests) branch on it.

function codedError(Ctor, code, message) {
  const err = new Ctor(message);
  err.code = code;
  return err;
}

// ---- system error names -----------------------------------------------------------------------
// libuv's negative errno table for this platform (preamble.js `__uvErrmap`).

const sysErrorMap = __uvErrmap;

function validateErrno(err) {
  if (typeof err !== "number") throw new TypeError('The "err" argument must be of type number.');
  if (err >= 0 || !Number.isInteger(err)) {
    throw new RangeError(`The value of "err" is out of range. It must be a negative integer. Received ${err}`);
  }
}
function getSystemErrorName(err) {
  validateErrno(err);
  const entry = sysErrorMap.get(err);
  return entry ? entry[0] : `Unknown system error ${err}`;
}
function getSystemErrorMessage(err) {
  validateErrno(err);
  const entry = sysErrorMap.get(err);
  return entry ? entry[1] : `Unknown system error ${err}`;
}
function getSystemErrorMap() {
  return new Map(sysErrorMap);
}

function _errnoException(err, syscall, original) {
  const name = getSystemErrorName(err);
  let message = `${syscall} ${name}`;
  if (original) message += ` ${original}`;
  const e = new Error(message);
  e.errno = err;
  e.code = name;
  e.syscall = syscall;
  return e;
}
function _exceptionWithHostPort(err, syscall, address, port, additional) {
  const name = getSystemErrorName(err);
  let details = "";
  if (port && port > 0) details = ` ${address}:${port}`;
  else if (address) details = ` ${address}`;
  if (additional) details += ` - Local (${additional})`;
  const e = new Error(`${syscall} ${name}${details}`);
  e.errno = err;
  e.code = name;
  e.syscall = syscall;
  if (address) e.address = address;
  if (port) e.port = port;
  return e;
}

// ---- log / debuglog ---------------------------------------------------------------------------

const LOG_MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const pad2 = (n) => String(n).padStart(2, "0");
function log(...args) {
  const d = new Date();
  const stamp = `${pad2(d.getDate())} ${LOG_MONTHS[d.getMonth()]} ${pad2(d.getHours())}:${pad2(d.getMinutes())}:${pad2(d.getSeconds())}`;
  if (typeof console !== "undefined" && console.log) console.log("%s - %s", stamp, format(...args));
}

function debuglog(section, cb) {
  section = String(section).toUpperCase();
  let enabledState = null;
  const isEnabled = () => {
    if (enabledState === null) {
      const env = (typeof process !== "undefined" && process.env && process.env.NODE_DEBUG) || "";
      enabledState = env
        .split(/[\s,]+/)
        .filter(Boolean)
        .some((token) => new RegExp(`^${token.toUpperCase().replace(/[*]/g, ".*")}$`).test(section));
    }
    return enabledState;
  };
  let notified = false;
  const logger = function (...args) {
    if (!isEnabled()) return;
    if (!notified && typeof cb === "function") {
      notified = true;
      cb(logger);
    }
    const pid = (typeof process !== "undefined" && process.pid) || 0;
    if (typeof console !== "undefined" && console.error) {
      console.error("%s %d: %s", section, pid, formatWithOptions({}, ...args));
    }
  };
  Object.defineProperty(logger, "enabled", { get: isEnabled, enumerable: true, configurable: true });
  return logger;
}

// ---- ANSI text helpers ------------------------------------------------------------------------

const ANSI_CODES = {
  reset: [0, 0], bold: [1, 22], dim: [2, 22], italic: [3, 23], underline: [4, 24],
  blink: [5, 25], inverse: [7, 27], hidden: [8, 28], strikethrough: [9, 29],
  doubleunderline: [21, 24], black: [30, 39], red: [31, 39], green: [32, 39],
  yellow: [33, 39], blue: [34, 39], magenta: [35, 39], cyan: [36, 39], white: [37, 39],
  bgBlack: [40, 49], bgRed: [41, 49], bgGreen: [42, 49], bgYellow: [43, 49], bgBlue: [44, 49],
  bgMagenta: [45, 49], bgCyan: [46, 49], bgWhite: [47, 49], framed: [51, 54], overlined: [53, 55],
  gray: [90, 39], redBright: [91, 39], greenBright: [92, 39], yellowBright: [93, 39],
  blueBright: [94, 39], magentaBright: [95, 39], cyanBright: [96, 39], whiteBright: [97, 39],
  bgGray: [100, 49], bgRedBright: [101, 49], bgGreenBright: [102, 49], bgYellowBright: [103, 49],
  bgBlueBright: [104, 49], bgMagentaBright: [105, 49], bgCyanBright: [106, 49], bgWhiteBright: [107, 49],
};

// Matches ANSI/VT escape sequences (CSI colour/style codes and OSC strings).
const ANSI_PATTERN = /[][[\]()#;?]*(?:(?:(?:\d{1,4}(?:;\d{0,4})*)?[0-9A-ORZcf-nqry=><~])|(?:[a-zA-Z\d]+(?:;[-a-zA-Z\d/#&.:=?%@~_]*)*)?)/g;

function stripVTControlCharacters(str) {
  if (typeof str !== "string") throw new TypeError('The "str" argument must be of type string.');
  return str.replace(ANSI_PATTERN, "");
}

function styleText(fmt, text, options) {
  if (typeof text !== "string") throw new TypeError('The "text" argument must be of type string.');
  const opts = options || {};
  // Honour an explicitly non-TTY stream by returning the text unstyled, like Node. With no stream
  // hint we apply the codes (lumen has no reliable isTTY on its stdout wrapper).
  if (opts.stream && opts.stream.isTTY === false) return text;
  const formats = Array.isArray(fmt) ? fmt : [fmt];
  let open = "";
  let close = "";
  for (const name of formats) {
    const code = ANSI_CODES[name];
    if (code === undefined) {
      throw codedError(TypeError, "ERR_INVALID_ARG_VALUE", `The argument 'format' must be a valid color/style. Received ${JSON.stringify(name)}`);
    }
    open += `[${code[0]}m`;
    close = `[${code[1]}m${close}`;
  }
  return `${open}${text}${close}`;
}

// ---- toUSVString ------------------------------------------------------------------------------
// Replace lone/unpaired surrogate code units with U+FFFD, yielding a well-formed USV string.

function toUSVString(str) {
  str = `${str}`;
  return str.replace(/[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/g, "�");
}

// ---- isDeepStrictEqual ------------------------------------------------------------------------

function ownEnumerableKeys(obj) {
  const keys = [];
  for (const key of Reflect.ownKeys(obj)) {
    const desc = Object.getOwnPropertyDescriptor(obj, key);
    if (desc && desc.enumerable) keys.push(key);
  }
  return keys;
}

function bytesEqual(a, b) {
  if (a.byteLength !== b.byteLength) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

function deepStrictEqual(a, b, seen) {
  if (Object.is(a, b)) return true;
  if (typeof a !== "object" || typeof b !== "object" || a === null || b === null) return false;
  if (Object.getPrototypeOf(a) !== Object.getPrototypeOf(b)) return false;

  const tag = tagOf(a);
  if (tag !== tagOf(b)) return false;

  const prior = seen.get(a);
  if (prior !== undefined) return prior === b;
  seen.set(a, b);

  if (tag === "Date") {
    const ta = a.getTime();
    const tb = b.getTime();
    return ta === tb || (Number.isNaN(ta) && Number.isNaN(tb));
  }
  if (tag === "RegExp") {
    if (a.source !== b.source || a.flags !== b.flags) return false;
  }
  if (BOXED_TAGS.has(tag)) {
    if (!Object.is(a.valueOf(), b.valueOf())) return false;
  }
  if (tag === "ArrayBuffer" || tag === "SharedArrayBuffer") {
    if (!bytesEqual(new Uint8Array(a), new Uint8Array(b))) return false;
  }
  if (ArrayBuffer.isView(a) && tag !== "DataView") {
    if (a.length !== b.length) return false;
    for (let i = 0; i < a.length; i++) {
      if (!deepStrictEqual(a[i], b[i], seen)) return false;
    }
  } else if (tag === "DataView") {
    if (!bytesEqual(new Uint8Array(a.buffer, a.byteOffset, a.byteLength), new Uint8Array(b.buffer, b.byteOffset, b.byteLength))) {
      return false;
    }
  }
  if (tag === "Map") {
    if (a.size !== b.size) return false;
    const bRemaining = new Set(b.keys());
    for (const [k, v] of a) {
      if (b.has(k)) {
        if (!deepStrictEqual(v, b.get(k), seen)) return false;
        bRemaining.delete(k);
      } else {
        // Object key: find a structurally-equal, not-yet-matched key in b.
        let matched = false;
        for (const bk of bRemaining) {
          if (deepStrictEqual(k, bk, seen) && deepStrictEqual(v, b.get(bk), seen)) {
            bRemaining.delete(bk);
            matched = true;
            break;
          }
        }
        if (!matched) return false;
      }
    }
  }
  if (tag === "Set") {
    if (a.size !== b.size) return false;
    const bRemaining = new Set(b);
    for (const v of a) {
      if (bRemaining.has(v)) {
        bRemaining.delete(v);
      } else {
        let matched = false;
        for (const bv of bRemaining) {
          if (deepStrictEqual(v, bv, seen)) {
            bRemaining.delete(bv);
            matched = true;
            break;
          }
        }
        if (!matched) return false;
      }
    }
  }

  const keysA = ownEnumerableKeys(a);
  const keysB = ownEnumerableKeys(b);
  if (keysA.length !== keysB.length) return false;
  for (const key of keysA) {
    if (!Object.prototype.propertyIsEnumerable.call(b, key)) return false;
    if (!deepStrictEqual(a[key], b[key], seen)) return false;
  }
  return true;
}

function isDeepStrictEqual(a, b) {
  return deepStrictEqual(a, b, new Map());
}

// ---- AbortSignal helpers ----------------------------------------------------------------------

function aborted(signal, resource) {
  if (signal == null || typeof signal.addEventListener !== "function") {
    throw new TypeError('The "signal" argument must be an instance of AbortSignal.');
  }
  if (signal.aborted) return Promise.resolve();
  return new Promise((resolve) => {
    signal.addEventListener("abort", () => resolve(), { once: true });
  });
}

// lumen has no MessagePort transfer, so the "transferable" marker is a no-op; these return real,
// fully-functional controllers/signals so the common non-transfer usage works.
function transferableAbortController() {
  return new AbortController();
}
function transferableAbortSignal(signal) {
  return signal;
}

// ---- getCallSites -----------------------------------------------------------------------------
// lumen carries no per-frame source information (see preamble.js), so there are no observable call
// sites to report; return an empty list rather than fabricated frames.

function getCallSites() {
  return [];
}
const getCallSite = getCallSites;

// ---- parseEnv ---------------------------------------------------------------------------------

function parseEnv(content) {
  content = `${content}`;
  const result = Object.create(null);
  const n = content.length;
  let i = 0;
  const isSpace = (c) => c === " " || c === "\t" || c === "\r" || c === "\n";
  while (i < n) {
    while (i < n && isSpace(content[i])) i++;
    if (i >= n) break;
    if (content[i] === "#") {
      while (i < n && content[i] !== "\n") i++;
      continue;
    }
    if (content.startsWith("export", i) && isSpace(content[i + 6] || " ")) {
      i += 6;
      while (i < n && (content[i] === " " || content[i] === "\t")) i++;
    }
    let key = "";
    while (i < n && content[i] !== "=" && content[i] !== "\n") key += content[i++];
    key = key.trim();
    if (content[i] !== "=") {
      while (i < n && content[i] !== "\n") i++;
      continue;
    }
    i++; // skip '='
    while (i < n && (content[i] === " " || content[i] === "\t")) i++;
    let value = "";
    const quote = content[i];
    if (quote === '"' || quote === "'" || quote === "`") {
      i++;
      while (i < n && content[i] !== quote) {
        if (quote === '"' && content[i] === "\\" && i + 1 < n) {
          const next = content[i + 1];
          value += next === "n" ? "\n" : next === "t" ? "\t" : next === "r" ? "\r" : next;
          i += 2;
          continue;
        }
        value += content[i++];
      }
      i++; // closing quote
    } else {
      while (i < n && content[i] !== "\n") value += content[i++];
      const comment = value.indexOf(" #");
      if (comment !== -1) value = value.slice(0, comment);
      value = value.trim();
    }
    if (key && key !== "__proto__") result[key] = value;
  }
  return result;
}

// ---- parseArgs --------------------------------------------------------------------------------
// Faithful port of Node's tokenizer + option store, covering strict/non-strict, short groups,
// inline values, `multiple`, defaults, positionals, the `--` terminator, and `tokens`.

function findLongFromShort(short, options) {
  for (const name of Object.keys(options)) {
    if (options[name] && options[name].short === short) return name;
  }
  return undefined;
}
function optionType(long, options) {
  return options[long] ? options[long].type : undefined;
}
function isOptionLikeValue(value) {
  return value != null && value.length > 1 && value[0] === "-";
}

function tokenizeArgs(args, options) {
  const tokens = [];
  const remaining = args.slice();
  let groupCount = 0;
  while (remaining.length > 0) {
    const arg = remaining.shift();
    const nextArg = remaining[0];
    let index = args.length - remaining.length - 1 - groupCount;

    if (arg === "--") {
      tokens.push({ kind: "option-terminator", index });
      for (const rest of remaining) tokens.push({ kind: "positional", index: ++index, value: rest });
      break;
    }

    const isShort = arg.length >= 2 && arg[0] === "-" && arg[1] !== "-";
    const isLong = arg.length > 2 && arg[0] === "-" && arg[1] === "-";

    if (isShort && arg.length === 2) {
      // lone short option: -f
      const short = arg[1];
      const long = findLongFromShort(short, options) ?? short;
      let value;
      let inlineValue;
      if (optionType(long, options) === "string" && nextArg !== undefined) {
        value = remaining.shift();
        inlineValue = false;
      }
      tokens.push({ kind: "option", name: long, rawName: arg, index, value, inlineValue });
      continue;
    }

    if (isShort && arg.length > 2) {
      const firstShort = arg[1];
      const firstLong = findLongFromShort(firstShort, options);
      if (optionType(firstLong, options) === "string") {
        // -xVALUE : first short is a string option, remainder is its inline value
        tokens.push({ kind: "option", name: firstLong, rawName: `-${firstShort}`, index, value: arg.slice(2), inlineValue: true });
        continue;
      }
      // short option group: expand and reprocess
      const expanded = [];
      for (let c = 1; c < arg.length; c++) {
        const short = arg[c];
        const long = findLongFromShort(short, options);
        if (optionType(long, options) !== "string" || c === arg.length - 1) {
          expanded.push(`-${short}`);
        } else {
          expanded.push(`-${arg.slice(c)}`);
          break;
        }
      }
      remaining.unshift(...expanded);
      groupCount += expanded.length - 1;
      continue;
    }

    if (isLong) {
      const eq = arg.indexOf("=");
      if (eq === -1) {
        const long = arg.slice(2);
        let value;
        let inlineValue;
        if (optionType(long, options) === "string" && nextArg !== undefined) {
          value = remaining.shift();
          inlineValue = false;
        }
        tokens.push({ kind: "option", name: long, rawName: arg, index, value, inlineValue });
      } else {
        const long = arg.slice(2, eq);
        tokens.push({ kind: "option", name: long, rawName: `--${long}`, index, value: arg.slice(eq + 1), inlineValue: true });
      }
      continue;
    }

    tokens.push({ kind: "positional", index, value: arg });
  }
  return tokens;
}

function storeOption(long, value, options, values) {
  if (long === "__proto__") return;
  const newValue = value === undefined ? true : value;
  if (options[long] && options[long].multiple) {
    if (Object.prototype.hasOwnProperty.call(values, long)) values[long].push(newValue);
    else values[long] = [newValue];
  } else {
    values[long] = newValue;
  }
}

function parseArgs(config) {
  config = config || {};
  const args = config.args ?? (typeof process !== "undefined" && process.argv ? process.argv.slice(2) : []);
  const strict = config.strict ?? true;
  const allowPositionals = config.allowPositionals ?? !strict;
  const returnTokens = config.tokens ?? false;
  const options = config.options ?? {};

  if (typeof options !== "object" || options === null) {
    throw codedError(TypeError, "ERR_INVALID_ARG_TYPE", 'The "options" argument must be of type object.');
  }
  for (const name of Object.keys(options)) {
    const opt = options[name];
    if (typeof opt !== "object" || opt === null || (opt.type !== "string" && opt.type !== "boolean")) {
      throw codedError(TypeError, "ERR_INVALID_ARG_TYPE", `options.${name}.type must be "string" or "boolean".`);
    }
  }

  const tokens = tokenizeArgs(args, options);
  const result = { values: Object.create(null), positionals: [] };
  if (returnTokens) result.tokens = tokens;

  for (const token of tokens) {
    if (token.kind === "option") {
      if (strict) {
        if (!Object.prototype.hasOwnProperty.call(options, token.name)) {
          throw codedError(TypeError, "ERR_PARSE_ARGS_UNKNOWN_OPTION", `Unknown option '${token.rawName}'`);
        }
        if (!token.inlineValue && isOptionLikeValue(token.value)) {
          throw codedError(TypeError, "ERR_PARSE_ARGS_INVALID_OPTION_VALUE", `Option '${token.rawName}' argument is ambiguous. Received '${token.value}'`);
        }
        const type = optionType(token.name, options);
        if (type === "string" && typeof token.value !== "string") {
          throw codedError(TypeError, "ERR_PARSE_ARGS_INVALID_OPTION_VALUE", `Option '${token.rawName} <value>' argument missing`);
        }
        if (type === "boolean" && token.value != null) {
          throw codedError(TypeError, "ERR_PARSE_ARGS_INVALID_OPTION_VALUE", `Option '${token.rawName}' does not take an argument`);
        }
      }
      storeOption(token.name, token.value, options, result.values);
    } else if (token.kind === "positional") {
      if (!allowPositionals) {
        throw codedError(TypeError, "ERR_PARSE_ARGS_UNEXPECTED_POSITIONAL", `Unexpected argument '${token.value}'. This command does not take positional arguments`);
      }
      result.positionals.push(token.value);
    }
  }

  for (const name of Object.keys(options)) {
    if (options[name].default !== undefined && !Object.prototype.hasOwnProperty.call(result.values, name)) {
      result.values[name] = options[name].default;
    }
  }
  return result;
}

// ---- diff (LCS) -------------------------------------------------------------------------------
// Returns an array of [operation, value] triples: 0 = unchanged, 1 = only in `actual`,
// -1 = only in `expected`. Strings are compared code point by code point.

function diff(actual, expected) {
  const isStr = typeof actual === "string";
  if (isStr) {
    if (typeof expected !== "string") {
      throw codedError(TypeError, "ERR_INVALID_ARG_TYPE", 'The "expected" argument must be of type string.');
    }
    if (actual === expected) return [];
  } else if (!Array.isArray(actual) || !Array.isArray(expected)) {
    throw codedError(TypeError, "ERR_INVALID_ARG_TYPE", 'The "actual" and "expected" arguments must both be strings or both be arrays.');
  }

  const a = isStr ? [...actual] : actual;
  const b = isStr ? [...expected] : expected;
  for (let i = 0; i < a.length; i++) {
    if (typeof a[i] !== "string") throw codedError(TypeError, "ERR_INVALID_ARG_TYPE", `The "actual[${i}]" argument must be of type string.`);
  }
  for (let i = 0; i < b.length; i++) {
    if (typeof b[i] !== "string") throw codedError(TypeError, "ERR_INVALID_ARG_TYPE", `The "expected[${i}]" argument must be of type string.`);
  }

  const m = a.length;
  const k = b.length;
  const lcs = Array.from({ length: m + 1 }, () => new Array(k + 1).fill(0));
  for (let i = 1; i <= m; i++) {
    for (let j = 1; j <= k; j++) {
      lcs[i][j] = a[i - 1] === b[j - 1] ? lcs[i - 1][j - 1] + 1 : Math.max(lcs[i - 1][j], lcs[i][j - 1]);
    }
  }
  const out = [];
  let i = m;
  let j = k;
  while (i > 0 || j > 0) {
    if (i > 0 && j > 0 && a[i - 1] === b[j - 1]) {
      out.push([0, a[i - 1]]);
      i--;
      j--;
    } else if (j > 0 && (i === 0 || lcs[i][j - 1] >= lcs[i - 1][j])) {
      out.push([-1, b[j - 1]]);
      j--;
    } else {
      out.push([1, a[i - 1]]);
      i--;
    }
  }
  out.reverse();
  return out;
}

// ---- MIMEType / MIMEParams --------------------------------------------------------------------

const HTTP_TOKEN = /^[!#$%&'*+\-.^_`|~A-Za-z0-9]+$/;
const NEEDS_QUOTE = /[^!#$%&'*+\-.^_`|~A-Za-z0-9]/;

function serializeParamValue(value) {
  if (value.length === 0 || NEEDS_QUOTE.test(value)) {
    return `"${value.replace(/["\\]/g, "\\$&")}"`;
  }
  return value;
}

class MIMEParams {
  #data = new Map();

  get(name) {
    name = `${name}`;
    return this.#data.has(name) ? this.#data.get(name) : null;
  }
  has(name) {
    return this.#data.has(`${name}`);
  }
  set(name, value) {
    name = `${name}`;
    value = `${value}`;
    if (!HTTP_TOKEN.test(name)) {
      throw codedError(TypeError, "ERR_INVALID_MIME_SYNTAX", `The MIME parameter name "${name}" is invalid`);
    }
    this.#data.set(name, value);
  }
  delete(name) {
    this.#data.delete(`${name}`);
  }
  entries() {
    return this.#data.entries();
  }
  keys() {
    return this.#data.keys();
  }
  values() {
    return this.#data.values();
  }
  [Symbol.iterator]() {
    return this.#data.entries();
  }
  // Internal helpers used by the parser/serializer.
  _setRaw(name, value) {
    this.#data.set(name, value);
  }
  _serialize() {
    let out = "";
    for (const [name, value] of this.#data) out += `;${name}=${serializeParamValue(value)}`;
    return out;
  }
}

class MIMEType {
  #type;
  #subtype;
  #params = new MIMEParams();

  constructor(input) {
    input = `${input}`.trim();
    const slash = input.indexOf("/");
    if (slash === -1) {
      throw codedError(TypeError, "ERR_INVALID_MIME_SYNTAX", `The MIME syntax for "${input}" is invalid: missing "/"`);
    }
    const type = input.slice(0, slash).toLowerCase();
    let rest = input.slice(slash + 1);
    let subtype = rest;
    const semi = rest.indexOf(";");
    if (semi !== -1) {
      subtype = rest.slice(0, semi);
      rest = rest.slice(semi + 1);
    } else {
      rest = "";
    }
    subtype = subtype.trim().toLowerCase();
    if (!HTTP_TOKEN.test(type) || !HTTP_TOKEN.test(subtype)) {
      throw codedError(TypeError, "ERR_INVALID_MIME_SYNTAX", `The MIME syntax for "${input}" is invalid`);
    }
    this.#type = type;
    this.#subtype = subtype;
    this.#parseParams(rest);
  }

  #parseParams(str) {
    let i = 0;
    const n = str.length;
    while (i < n) {
      while (i < n && (str[i] === ";" || str[i] === " " || str[i] === "\t")) i++;
      if (i >= n) break;
      let name = "";
      while (i < n && str[i] !== "=" && str[i] !== ";") name += str[i++];
      name = name.trim().toLowerCase();
      if (str[i] !== "=") {
        while (i < n && str[i] !== ";") i++;
        continue;
      }
      i++; // skip '='
      let value = "";
      if (str[i] === '"') {
        i++;
        while (i < n && str[i] !== '"') {
          if (str[i] === "\\" && i + 1 < n) {
            value += str[i + 1];
            i += 2;
            continue;
          }
          value += str[i++];
        }
        i++; // closing quote
        while (i < n && str[i] !== ";") i++;
      } else {
        while (i < n && str[i] !== ";") value += str[i++];
        value = value.trim();
      }
      if (name && HTTP_TOKEN.test(name) && !this.#params.has(name)) this.#params._setRaw(name, value);
    }
  }

  get type() {
    return this.#type;
  }
  set type(value) {
    value = `${value}`.toLowerCase();
    if (!HTTP_TOKEN.test(value)) {
      throw codedError(TypeError, "ERR_INVALID_MIME_SYNTAX", `The MIME type "${value}" is invalid`);
    }
    this.#type = value;
  }
  get subtype() {
    return this.#subtype;
  }
  set subtype(value) {
    value = `${value}`.toLowerCase();
    if (!HTTP_TOKEN.test(value)) {
      throw codedError(TypeError, "ERR_INVALID_MIME_SYNTAX", `The MIME subtype "${value}" is invalid`);
    }
    this.#subtype = value;
  }
  get essence() {
    return `${this.#type}/${this.#subtype}`;
  }
  get params() {
    return this.#params;
  }
  toString() {
    return `${this.#type}/${this.#subtype}${this.#params._serialize()}`;
  }
  toJSON() {
    return this.toString();
  }
}

// ---- assembled export -------------------------------------------------------------------------

const util = {
  inherits,
  inspect,
  format,
  formatWithOptions,
  deprecate,
  promisify,
  callbackify,
  types,
  isArray: Array.isArray,
  isDate: types.isDate,
  isRegExp: types.isRegExp,
  isError: types.isNativeError,
  isFunction: (v) => typeof v === "function",
  isString: (v) => typeof v === "string",
  isNumber: (v) => typeof v === "number",
  isBoolean: (v) => typeof v === "boolean",
  isNull: (v) => v === null,
  isNullOrUndefined: (v) => v == null,
  isUndefined: (v) => v === undefined,
  isObject: (v) => v !== null && typeof v === "object",
  isPrimitive: (v) => v === null || (typeof v !== "object" && typeof v !== "function"),
  isBuffer: (v) => (typeof Buffer !== "undefined" && Buffer.isBuffer ? Buffer.isBuffer(v) : false),
  isSymbol: (v) => typeof v === "symbol",
  _extend: (target, source) => Object.assign(target, source),
  isDeepStrictEqual,
  log,
  debuglog,
  debug: debuglog,
  parseArgs,
  parseEnv,
  styleText,
  stripVTControlCharacters,
  toUSVString,
  diff,
  getSystemErrorName,
  getSystemErrorMessage,
  getSystemErrorMap,
  aborted,
  transferableAbortController,
  transferableAbortSignal,
  getCallSite,
  getCallSites,
  _errnoException,
  _exceptionWithHostPort,
  MIMEType,
  MIMEParams,
  TextEncoder: globalThis.TextEncoder,
  TextDecoder: globalThis.TextDecoder,
};

__builtins.set("util", util);
