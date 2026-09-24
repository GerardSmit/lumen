// node:assert and node:assert/strict.
//
// Follows Node's lib/assert.js, internal/assert/assertion_error.js and
// internal/util/comparisons.js: the same deep-equality rules (strict and loose, cycles, Map/Set
// matching of object keys, boxed primitives, typed arrays, errors by name + message), the same
// error-matching rules for throws/rejects (constructor, RegExp, validation function or an object
// of properties whose RegExp values match string properties), the same argument validation, and
// the same generated messages (`Expected values to be strictly equal:` with the +/- line diff),
// since both test suites and tooling compare those. `assert.ok` cannot quote the failing source
// expression (lumen's stack frames carry no source positions), so it reports Node's fallback
// message instead.

const util = __builtins.get("util");
const { inspect } = util;
const { ERR_INVALID_ARG_TYPE, ERR_INVALID_ARG_VALUE, ERR_AMBIGUOUS_ARGUMENT, ERR_INVALID_RETURN_VALUE, ERR_MISSING_ARGS } = __errors;

const objectToString = (v) => Object.prototype.toString.call(v);
const hasOwn = (o, k) => Object.prototype.hasOwnProperty.call(o, k);
const isEnumerable = (o, k) => Object.prototype.propertyIsEnumerable.call(o, k);
const isRegExp = (v) => objectToString(v) === "[object RegExp]";
const isDate = (v) => objectToString(v) === "[object Date]";
const isSet = (v) => {
  try { Reflect.apply(Object.getOwnPropertyDescriptor(Set.prototype, "size").get, v, []); return true; } catch { return false; }
};
const isMap = (v) => {
  try { Reflect.apply(Object.getOwnPropertyDescriptor(Map.prototype, "size").get, v, []); return true; } catch { return false; }
};
const isNativeError = (v) => v instanceof Error || objectToString(v) === "[object Error]";
const isAnyArrayBuffer = (v) => {
  const t = objectToString(v);
  return t === "[object ArrayBuffer]" || t === "[object SharedArrayBuffer]";
};
const typedArrayTagGetter = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(Uint8Array.prototype), Symbol.toStringTag).get;
const typedArrayTag = (v) => typedArrayTagGetter.call(v);
const isBoxedPrimitive = (v) => util.types.isBoxedPrimitive(v);
const isPromise = (v) => util.types.isPromise(v);

// ---- deep equality (internal/util/comparisons) ------------------------------------------------

const kNoIterator = 0;
const kIsArray = 1;
const kIsSet = 2;
const kIsMap = 3;

function getOwnNonIndexProperties(obj, strict) {
  const out = [];
  for (const key of Reflect.ownKeys(obj)) {
    if (typeof key === "string" && /^(0|[1-9][0-9]*)$/.test(key) && Number(key) < 2 ** 32 - 1) continue;
    if (!isEnumerable(obj, key)) continue;
    if (!strict && typeof key === "symbol") continue;
    out.push(key);
  }
  return out;
}

function areSimilarRegExps(a, b) {
  return a.source === b.source && a.flags === b.flags && a.lastIndex === b.lastIndex;
}

function areSimilarFloatArrays(a, b) {
  if (a.byteLength !== b.byteLength) return false;
  for (let offset = 0; offset < a.length; offset++) {
    if (a[offset] !== b[offset]) return false;
  }
  return true;
}

function bytesEqual(a, b) {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

function areSimilarTypedArrays(a, b) {
  if (a.byteLength !== b.byteLength) return false;
  return bytesEqual(new Uint8Array(a.buffer, a.byteOffset, a.byteLength), new Uint8Array(b.buffer, b.byteOffset, b.byteLength));
}

function areEqualArrayBuffers(buf1, buf2) {
  return buf1.byteLength === buf2.byteLength && bytesEqual(new Uint8Array(buf1), new Uint8Array(buf2));
}

function isEqualBoxedPrimitive(val1, val2) {
  const t = util.types;
  if (t.isNumberObject(val1)) return t.isNumberObject(val2) && Object.is(Number.prototype.valueOf.call(val1), Number.prototype.valueOf.call(val2));
  if (t.isStringObject(val1)) return t.isStringObject(val2) && String.prototype.valueOf.call(val1) === String.prototype.valueOf.call(val2);
  if (t.isBooleanObject(val1)) return t.isBooleanObject(val2) && Boolean.prototype.valueOf.call(val1) === Boolean.prototype.valueOf.call(val2);
  if (t.isBigIntObject(val1)) return t.isBigIntObject(val2) && BigInt.prototype.valueOf.call(val1) === BigInt.prototype.valueOf.call(val2);
  if (t.isSymbolObject(val1)) return t.isSymbolObject(val2) && Symbol.prototype.valueOf.call(val1) === Symbol.prototype.valueOf.call(val2);
  return false;
}

function innerDeepEqual(val1, val2, strict, memos) {
  // All identical values are equivalent, as determined by ===.
  if (val1 === val2) {
    if (val1 !== 0) return true;
    return strict ? Object.is(val1, val2) : true;
  }
  if (strict) {
    if (typeof val1 !== "object") return typeof val1 === "number" && Number.isNaN(val1) && Number.isNaN(val2);
    if (typeof val2 !== "object" || val1 === null || val2 === null) return false;
    if (Object.getPrototypeOf(val1) !== Object.getPrototypeOf(val2)) return false;
  } else {
    if (val1 === null || typeof val1 !== "object") {
      if (val2 === null || typeof val2 !== "object") {
        // eslint-disable-next-line eqeqeq
        return val1 == val2 || (Number.isNaN(val1) && Number.isNaN(val2));
      }
      return false;
    }
    if (val2 === null || typeof val2 !== "object") return false;
  }
  const val1Tag = objectToString(val1);
  const val2Tag = objectToString(val2);
  if (val1Tag !== val2Tag) return false;

  if (Array.isArray(val1)) {
    if (!Array.isArray(val2) || val1.length !== val2.length) return false;
    const keys1 = getOwnNonIndexProperties(val1, strict);
    const keys2 = getOwnNonIndexProperties(val2, strict);
    if (keys1.length !== keys2.length) return false;
    return keyCheck(val1, val2, strict, memos, kIsArray, keys1);
  } else if (val1Tag === "[object Object]" && !isNativeError(val1)) {
    return keyCheck(val1, val2, strict, memos, kNoIterator);
  } else if (isDate(val1)) {
    if (!isDate(val2) || Date.prototype.getTime.call(val1) !== Date.prototype.getTime.call(val2)) return false;
  } else if (isRegExp(val1)) {
    if (!isRegExp(val2) || !areSimilarRegExps(val1, val2)) return false;
  } else if (isNativeError(val1)) {
    // Do not compare the stack as it might differ even though the error itself is otherwise
    // identical.
    if (!isNativeError(val2) || val1.message !== val2.message || val1.name !== val2.name) return false;
  } else if (ArrayBuffer.isView(val1)) {
    if (!ArrayBuffer.isView(val2)) return false;
    if (typedArrayTag(val1) !== typedArrayTag(val2)) return false;
    if (typedArrayTag(val1) === undefined) {
      // DataView
      if (!areEqualArrayBuffers(val1.buffer.slice(val1.byteOffset, val1.byteOffset + val1.byteLength), val2.buffer.slice(val2.byteOffset, val2.byteOffset + val2.byteLength))) return false;
    } else if (!strict && (val1 instanceof Float32Array || val1 instanceof Float64Array)) {
      if (!areSimilarFloatArrays(val1, val2)) return false;
    } else if (!areSimilarTypedArrays(val1, val2)) {
      return false;
    }
    const keys1 = getOwnNonIndexProperties(val1, strict);
    const keys2 = getOwnNonIndexProperties(val2, strict);
    if (keys1.length !== keys2.length) return false;
    return keyCheck(val1, val2, strict, memos, kNoIterator, keys1);
  } else if (isSet(val1)) {
    if (!isSet(val2) || val1.size !== val2.size) return false;
    return keyCheck(val1, val2, strict, memos, kIsSet);
  } else if (isMap(val1)) {
    if (!isMap(val2) || val1.size !== val2.size) return false;
    return keyCheck(val1, val2, strict, memos, kIsMap);
  } else if (isAnyArrayBuffer(val1)) {
    if (!isAnyArrayBuffer(val2) || !areEqualArrayBuffers(val1, val2)) return false;
  } else if (isBoxedPrimitive(val1)) {
    if (!isEqualBoxedPrimitive(val1, val2)) return false;
  } else if (Array.isArray(val2) || ArrayBuffer.isView(val2) || isSet(val2) || isMap(val2) ||
             isDate(val2) || isRegExp(val2) || isAnyArrayBuffer(val2) || isBoxedPrimitive(val2) ||
             isNativeError(val2)) {
    return false;
  }
  return keyCheck(val1, val2, strict, memos, kNoIterator);
}

function getEnumerables(val, keys) {
  return keys.filter((k) => isEnumerable(val, k));
}

function keyCheck(val1, val2, strict, memos, iterationType, aKeys) {
  const explicitKeys = aKeys !== undefined;
  if (!explicitKeys) {
    aKeys = Object.keys(val1);
    const bKeys = Object.keys(val2);
    // The pair must have the same number of owned properties.
    if (aKeys.length !== bKeys.length) return false;
  }
  // Cheap key test
  let i = 0;
  for (; i < aKeys.length; i++) {
    if (!hasOwn(val2, aKeys[i])) return false;
  }
  if (strict && !explicitKeys) {
    // Validate that the amount of symbols is identical.
    const symbolKeysA = Object.getOwnPropertySymbols(val1);
    if (symbolKeysA.length !== 0) {
      let count = 0;
      for (i = 0; i < symbolKeysA.length; i++) {
        const key = symbolKeysA[i];
        if (isEnumerable(val1, key)) {
          if (!isEnumerable(val2, key)) return false;
          aKeys.push(key);
          count++;
        } else if (isEnumerable(val2, key)) {
          return false;
        }
      }
      const symbolKeysB = Object.getOwnPropertySymbols(val2);
      if (symbolKeysA.length !== symbolKeysB.length && getEnumerables(val2, symbolKeysB).length !== count) return false;
    } else {
      const symbolKeysB = Object.getOwnPropertySymbols(val2);
      if (symbolKeysB.length !== 0 && getEnumerables(val2, symbolKeysB).length !== 0) return false;
    }
  }
  if (aKeys.length === 0 &&
      (iterationType === kNoIterator || (iterationType === kIsArray && val1.length === 0) || val1.size === 0)) {
    return true;
  }
  // Use memos to handle cycles.
  if (memos === undefined) {
    memos = { val1: new Map(), val2: new Map(), position: 0 };
  } else {
    const val2MemoA = memos.val1.get(val1);
    if (val2MemoA !== undefined) {
      const val2MemoB = memos.val2.get(val2);
      if (val2MemoB !== undefined) return val2MemoA === val2MemoB;
    }
    memos.position++;
  }
  memos.val1.set(val1, memos.position);
  memos.val2.set(val2, memos.position);
  const areEq = objEquiv(val1, val2, strict, aKeys, memos, iterationType);
  memos.val1.delete(val1);
  memos.val2.delete(val2);
  return areEq;
}

function setHasEqualElement(set, val1, strict, memo) {
  for (const val2 of set) {
    if (innerDeepEqual(val1, val2, strict, memo)) {
      // Remove the matching element to make sure we do not check that again.
      set.delete(val2);
      return true;
    }
  }
  return false;
}

// Loose equality can match primitives of different types (1 == '1'); these narrow which values
// could still have a loose partner in the other collection.
function findLooseMatchingPrimitives(prim) {
  switch (typeof prim) {
    case "undefined":
      return null;
    case "object": // Only pass in null as object!
      return undefined;
    case "symbol":
      return false;
    case "string":
      prim = +prim;
    // Loose equal entries exist only if the string is possible to convert to a regular number
    // and not NaN.
    // falls through
    case "number":
      if (Number.isNaN(prim)) return false;
  }
  return true;
}

function setMightHaveLoosePrim(a, b, prim) {
  const altValue = findLooseMatchingPrimitives(prim);
  if (altValue != null) return altValue;
  return b.has(altValue) && !a.has(altValue);
}

function mapMightHaveLoosePrim(a, b, prim, item, memo) {
  const altValue = findLooseMatchingPrimitives(prim);
  if (altValue != null) return altValue;
  const curB = b.get(altValue);
  if ((curB === undefined && !b.has(altValue)) || !innerDeepEqual(item, curB, false, memo)) return false;
  return !a.has(altValue) && innerDeepEqual(item, curB, false, memo);
}

function setEquiv(a, b, strict, memo) {
  let set = null;
  for (const val of a) {
    if (typeof val === "object" && val !== null) {
      if (set === null) set = new Set();
      set.add(val);
    } else if (!b.has(val)) {
      if (strict) return false;
      if (!setMightHaveLoosePrim(a, b, val)) return false;
      if (set === null) set = new Set();
      set.add(val);
    }
  }
  if (set !== null) {
    for (const val of b) {
      if (typeof val === "object" && val !== null) {
        if (!setHasEqualElement(set, val, strict, memo)) return false;
      } else if (!strict && !a.has(val) && !setHasEqualElement(set, val, strict, memo)) {
        return false;
      }
    }
    return set.size === 0;
  }
  return true;
}

function mapHasEqualEntry(set, map, key1, item1, strict, memo) {
  for (const key2 of set) {
    if (innerDeepEqual(key1, key2, strict, memo) && innerDeepEqual(item1, map.get(key2), strict, memo)) {
      set.delete(key2);
      return true;
    }
  }
  return false;
}

function mapEquiv(a, b, strict, memo) {
  let set = null;
  for (const [key, item1] of a) {
    if (typeof key === "object" && key !== null) {
      if (set === null) set = new Set();
      set.add(key);
    } else {
      const item2 = b.get(key);
      if ((item2 === undefined && !b.has(key)) || !innerDeepEqual(item1, item2, strict, memo)) {
        if (strict) return false;
        if (!mapMightHaveLoosePrim(a, b, key, item1, memo)) return false;
        if (set === null) set = new Set();
        set.add(key);
      }
    }
  }
  if (set !== null) {
    for (const [key, item] of b) {
      if (typeof key === "object" && key !== null) {
        if (!mapHasEqualEntry(set, a, key, item, strict, memo)) return false;
      } else if (!strict && (!a.has(key) || !innerDeepEqual(a.get(key), item, false, memo)) &&
                 !mapHasEqualEntry(set, a, key, item, false, memo)) {
        return false;
      }
    }
    return set.size === 0;
  }
  return true;
}

function objEquiv(a, b, strict, keys, memos, iterationType) {
  let i = 0;
  if (iterationType === kIsSet) {
    if (!setEquiv(a, b, strict, memos)) return false;
  } else if (iterationType === kIsMap) {
    if (!mapEquiv(a, b, strict, memos)) return false;
  } else if (iterationType === kIsArray) {
    for (; i < a.length; i++) {
      if (hasOwn(a, i)) {
        if (!hasOwn(b, i) || !innerDeepEqual(a[i], b[i], strict, memos)) return false;
      } else if (hasOwn(b, i)) {
        return false;
      } else {
        // Array is sparse.
        const keysA = Object.keys(a);
        for (; i < keysA.length; i++) {
          const key = keysA[i];
          if (!hasOwn(b, key) || !innerDeepEqual(a[key], b[key], strict, memos)) return false;
        }
        if (keysA.length !== Object.keys(b).length) return false;
        return true;
      }
    }
  }
  // The pair must have equivalent values for every corresponding key.
  for (i = 0; i < keys.length; i++) {
    const key = keys[i];
    if (!innerDeepEqual(a[key], b[key], strict, memos)) return false;
  }
  return true;
}

const isDeepEqual = (val1, val2) => innerDeepEqual(val1, val2, false);
const isDeepStrictEqual = (val1, val2) => innerDeepEqual(val1, val2, true);

// util.isDeepStrictEqual shares these rules.
util.isDeepStrictEqual = isDeepStrictEqual;

// ---- AssertionError ---------------------------------------------------------------------------

const kReadableOperator = {
  deepStrictEqual: "Expected values to be strictly deep-equal:",
  strictEqual: "Expected values to be strictly equal:",
  strictEqualObject: 'Expected "actual" to be reference-equal to "expected":',
  deepEqual: "Expected values to be loosely deep-equal:",
  notDeepStrictEqual: 'Expected "actual" not to be strictly deep-equal to:',
  notStrictEqual: 'Expected "actual" to be strictly unequal to:',
  notStrictEqualObject: 'Expected "actual" not to be reference-equal to "expected":',
  notDeepEqual: 'Expected "actual" not to be loosely deep-equal to:',
  notIdentical: "Values identical but not reference-equal:",
  notDeepEqualUnequal: "Expected values not to be loosely deep-equal:",
};

// Comparing short primitives should just show === / !== instead of using the diff.
const kMaxShortLength = 12;

function copyError(source) {
  const target = Object.assign({ __proto__: Object.getPrototypeOf(source) }, source);
  Object.defineProperty(target, "message", { value: source.message });
  return target;
}

function inspectValue(val) {
  return inspect(val, {
    compact: false,
    customInspect: false,
    depth: 1000,
    maxArrayLength: Infinity,
    showHidden: false,
    showProxy: false,
    sorted: true,
    getters: true,
  });
}

function createErrDiff(actual, expected, operator) {
  let other = "";
  let res = "";
  let end = "";
  let skipped = false;
  const actualInspected = inspectValue(actual);
  const actualLines = actualInspected.split("\n");
  const expectedLines = inspectValue(expected).split("\n");

  let i = 0;
  let indicator = "";

  // In case both values are objects or functions explicitly mark them as not reference equal for
  // the `strictEqual` operator.
  if (operator === "strictEqual" &&
      ((typeof actual === "object" && actual !== null && typeof expected === "object" && expected !== null) ||
        (typeof actual === "function" && typeof expected === "function"))) {
    operator = "strictEqualObject";
  }

  // If "actual" and "expected" fit on a single line and they are not strictly equal, print them
  // next to each other.
  if (actualLines.length === 1 && expectedLines.length === 1 && actualLines[0] !== expectedLines[0]) {
    const actualRaw = actualLines[0];
    const expectedRaw = expectedLines[0];
    const inputLength = actualRaw.length + expectedRaw.length;
    if (inputLength <= kMaxShortLength) {
      if ((typeof actual !== "object" || actual === null) &&
          (typeof expected !== "object" || expected === null) &&
          (actual !== 0 || expected !== 0)) { // -0 === +0
        return `${kReadableOperator[operator]}\n\n${actualLines[0]} !== ${expectedLines[0]}\n`;
      }
    } else if (operator !== "strictEqualObject") {
      // Behind a pipe the terminal width is unknown; Node uses 80 columns then.
      const maxLength = 80;
      if (inputLength < maxLength) {
        while (actualRaw[i] === expectedRaw[i]) i++;
        // Ignore the first characters.
        if (i > 2) {
          // Add position indicator for the first mismatch.
          indicator = `\n  ${" ".repeat(i)}^`;
          i = 0;
        }
      }
    }
  }

  // Remove all ending lines that match (this optimizes the output for readability by reducing
  // the number of total changed lines).
  let a = actualLines[actualLines.length - 1];
  let b = expectedLines[expectedLines.length - 1];
  while (a === b) {
    if (i++ < 3) {
      end = `\n  ${a}${end}`;
    } else {
      other = a;
    }
    actualLines.pop();
    expectedLines.pop();
    if (actualLines.length === 0 || expectedLines.length === 0) break;
    a = actualLines[actualLines.length - 1];
    b = expectedLines[expectedLines.length - 1];
  }

  const maxLines = Math.max(actualLines.length, expectedLines.length);
  // Strict equal with identical objects that are not identical by reference.
  if (maxLines === 0) {
    const lines = actualInspected.split("\n");
    if (lines.length > 50) {
      lines[46] = "...";
      while (lines.length > 47) lines.pop();
    }
    return `${kReadableOperator.notIdentical}\n\n${lines.join("\n")}\n`;
  }

  // There were at least five identical lines at the end. Mark a couple of skipped.
  if (i >= 5) {
    end = `\n...${end}`;
    skipped = true;
  }
  if (other !== "") {
    end = `\n  ${other}${end}`;
    other = "";
  }

  let printedLines = 0;
  let identical = 0;
  const msg = kReadableOperator[operator] + "\n+ actual - expected";
  const skippedMsg = " ... Lines skipped";

  let lines = actualLines;
  let plusMinus = "+";
  let maxLength = expectedLines.length;
  if (actualLines.length < maxLines) {
    lines = expectedLines;
    plusMinus = "-";
    maxLength = actualLines.length;
  }

  for (i = 0; i < maxLines; i++) {
    if (maxLength < i + 1) {
      // If more than two former lines are identical, print them. Collapse them in case more
      // than five lines were identical.
      if (identical > 2) {
        if (identical > 3) {
          if (identical > 4) {
            if (identical === 5) {
              res += `\n  ${lines[i - 3]}`;
              printedLines++;
            } else {
              res += "\n...";
              skipped = true;
            }
          }
          res += `\n  ${lines[i - 2]}`;
          printedLines++;
        }
        res += `\n  ${lines[i - 1]}`;
        printedLines++;
      }
      identical = 0;
      if (lines === actualLines) {
        res += `\n${plusMinus} ${lines[i]}`;
      } else {
        other += `\n${plusMinus} ${lines[i]}`;
      }
      printedLines++;
    } else {
      const expectedLine = expectedLines[i];
      let actualLine = actualLines[i];
      let divergingLines = actualLine !== expectedLine &&
        (!actualLine.endsWith(",") || actualLine.slice(0, -1) !== expectedLine);
      // If the expected line has a trailing comma but is otherwise identical, add a comma at the
      // end of the actual line.
      if (divergingLines && expectedLine.endsWith(",") && expectedLine.slice(0, -1) === actualLine) {
        divergingLines = false;
        actualLine += ",";
      }
      if (divergingLines) {
        if (identical > 2) {
          if (identical > 3) {
            if (identical > 4) {
              if (identical === 5) {
                res += `\n  ${actualLines[i - 3]}`;
                printedLines++;
              } else {
                res += "\n...";
                skipped = true;
              }
            }
            res += `\n  ${actualLines[i - 2]}`;
            printedLines++;
          }
          res += `\n  ${actualLines[i - 1]}`;
          printedLines++;
        }
        identical = 0;
        // Add the actual line to the result and cache the expected diverging line so
        // consecutive diverging lines show up as +++--- and not +-+-+-.
        res += `\n+ ${actualLine}`;
        other += `\n- ${expectedLine}`;
        printedLines += 2;
      } else {
        // Lines are identical: flush the cached expected lines first.
        res += other;
        other = "";
        identical++;
        if (identical <= 2) {
          res += `\n  ${actualLine}`;
          printedLines++;
        }
      }
    }
    // Inspected object too big (show ~50 rows max).
    if (printedLines > 50 && i < maxLines - 2) {
      return `${msg}${skippedMsg}\n${res}\n...${other}\n...`;
    }
  }

  return `${msg}${skipped ? skippedMsg : ""}\n${res}${other}${end}${indicator}`;
}

class AssertionError extends Error {
  constructor(options) {
    if (options === null || typeof options !== "object") {
      throw new ERR_INVALID_ARG_TYPE("options", "Object", options);
    }
    const { message, operator, stackStartFn, details } = options;
    let { actual, expected } = options;

    if (message != null) {
      super(String(message));
    } else {
      if (typeof actual === "object" && actual !== null && typeof expected === "object" && expected !== null &&
          "stack" in actual && actual instanceof Error && "stack" in expected && expected instanceof Error) {
        actual = copyError(actual);
        expected = copyError(expected);
      }

      if (operator === "deepStrictEqual" || operator === "strictEqual") {
        super(createErrDiff(actual, expected, operator));
      } else if (operator === "notDeepStrictEqual" || operator === "notStrictEqual") {
        // In case the objects are equal but the operator requires unequal, show the first object
        // and say A equals B.
        let base = kReadableOperator[operator];
        const res = inspectValue(actual).split("\n");
        if (operator === "notStrictEqual" &&
            ((typeof actual === "object" && actual !== null) || typeof actual === "function")) {
          base = kReadableOperator.notStrictEqualObject;
        }
        // Only remove lines in case it makes sense to collapse those.
        if (res.length > 50) {
          res[46] = "...";
          while (res.length > 47) res.pop();
        }
        if (res.length === 1) {
          super(`${base}${res[0].length > 5 ? "\n\n" : " "}${res[0]}`);
        } else {
          super(`${base}\n\n${res.join("\n")}\n`);
        }
      } else {
        let res = inspectValue(actual);
        let other = inspectValue(expected);
        const knownOperator = kReadableOperator[operator];
        if (operator === "notDeepEqual" && res === other) {
          res = `${knownOperator}\n\n${res}`;
          if (res.length > 1024) res = `${res.slice(0, 1021)}...`;
          super(res);
        } else {
          if (res.length > 512) res = `${res.slice(0, 509)}...`;
          if (other.length > 512) other = `${other.slice(0, 509)}...`;
          if (operator === "deepEqual") {
            res = `${knownOperator}\n\n${res}\n\nshould loosely deep-equal\n\n`;
          } else {
            const newOp = kReadableOperator[`${operator}Unequal`];
            if (newOp) {
              res = `${newOp}\n\n${res}\n\nshould not loosely deep-equal\n\n`;
            } else {
              other = ` ${operator} ${other}`;
            }
          }
          super(`${res}${other}`);
        }
      }
    }

    this.generatedMessage = !message;
    Object.defineProperty(this, "name", {
      value: "AssertionError [ERR_ASSERTION]",
      enumerable: false,
      writable: true,
      configurable: true,
    });
    this.code = "ERR_ASSERTION";
    if (details) {
      this.actual = undefined;
      this.expected = undefined;
      this.operator = undefined;
      for (let i = 0; i < details.length; i++) {
        this["message " + i] = details[i].message;
        this["actual " + i] = details[i].actual;
        this["expected " + i] = details[i].expected;
        this["operator " + i] = details[i].operator;
        this["stack trace " + i] = details[i].stack;
      }
    } else {
      this.actual = actual;
      this.expected = expected;
      this.operator = operator;
    }
    if (typeof Error.captureStackTrace === "function") {
      try { Error.captureStackTrace(this, stackStartFn); } catch { /* keep the constructor's stack */ }
    }
    // The stack header carries the code, like Node's.
    if (typeof this.stack === "string") {
      const nl = this.stack.indexOf("\n    at");
      const frames = nl === -1 ? "" : this.stack.slice(nl);
      this.stack = `AssertionError [ERR_ASSERTION]: ${this.message}${frames}`;
    }
    this.name = "AssertionError";
  }

  toString() {
    return `${this.name} [${this.code}]: ${this.message}`;
  }

  [inspect.custom](recurseTimes, ctx) {
    // Long strings should not be fully inspected.
    const tmpActual = this.actual;
    const tmpExpected = this.expected;
    const truncate = (s) => (typeof s === "string" && s.length > 512 ? `${s.slice(0, 512)}...` : s);
    if (typeof this.actual === "string") this.actual = truncate(this.actual);
    if (typeof this.expected === "string") this.expected = truncate(this.expected);
    // This limits the `actual` and `expected` property default inspection to the minimum depth.
    // Otherwise those values would be too verbose compared to the actual error message which
    // contains a combined view of these two input values.
    const result = inspect(this, { ...ctx, customInspect: false, depth: 0 });
    this.actual = tmpActual;
    this.expected = tmpExpected;
    return result;
  }
}

// ---- assert -----------------------------------------------------------------------------------

const NO_EXCEPTION_SENTINEL = {};

function innerFail(obj) {
  if (obj.message instanceof Error) throw obj.message;
  throw new AssertionError(obj);
}

let warnedFail = false;
function fail(actual, expected, message, operator, stackStartFn) {
  const argsLen = arguments.length;
  let internalMessage = false;
  if (actual == null && argsLen <= 1) {
    internalMessage = true;
    message = "Failed";
  } else if (argsLen === 1) {
    message = actual;
    actual = undefined;
  } else {
    if (!warnedFail) {
      warnedFail = true;
      process.emitWarning(
        "assert.fail() with more than one argument is deprecated. " +
          "Please use assert.strictEqual() instead or only pass a message.",
        "DeprecationWarning",
        "DEP0094",
      );
    }
    if (argsLen === 2) operator = "!=";
  }
  if (message instanceof Error) throw message;
  const err = new AssertionError({
    actual,
    expected,
    operator: operator === undefined ? "fail" : operator,
    stackStartFn: stackStartFn || fail,
    message,
  });
  if (internalMessage) err.generatedMessage = true;
  throw err;
}

function innerOk(fn, argLen, value, message) {
  if (!value) {
    let generatedMessage = false;
    if (argLen === 0) {
      generatedMessage = true;
      message = "No value argument passed to `assert.ok()`";
    } else if (message == null) {
      generatedMessage = true;
      message = undefined;
    } else if (message instanceof Error) {
      throw message;
    }
    const err = new AssertionError({ actual: value, expected: true, message, operator: "==", stackStartFn: fn });
    err.generatedMessage = generatedMessage;
    throw err;
  }
}

function ok(...args) {
  innerOk(ok, args.length, ...args);
}

const assert = ok;

assert.fail = fail;
assert.AssertionError = AssertionError;
assert.ok = ok;

assert.equal = function equal(actual, expected, message) {
  if (arguments.length < 2) throw new ERR_MISSING_ARGS("actual", "expected");
  // eslint-disable-next-line eqeqeq
  if (actual != expected && (!Number.isNaN(actual) || !Number.isNaN(expected))) {
    innerFail({ actual, expected, message, operator: "==", stackStartFn: equal });
  }
};

assert.notEqual = function notEqual(actual, expected, message) {
  if (arguments.length < 2) throw new ERR_MISSING_ARGS("actual", "expected");
  // eslint-disable-next-line eqeqeq
  if (actual == expected || (Number.isNaN(actual) && Number.isNaN(expected))) {
    innerFail({ actual, expected, message, operator: "!=", stackStartFn: notEqual });
  }
};

assert.deepEqual = function deepEqual(actual, expected, message) {
  if (arguments.length < 2) throw new ERR_MISSING_ARGS("actual", "expected");
  if (!isDeepEqual(actual, expected)) {
    innerFail({ actual, expected, message, operator: "deepEqual", stackStartFn: deepEqual });
  }
};

assert.notDeepEqual = function notDeepEqual(actual, expected, message) {
  if (arguments.length < 2) throw new ERR_MISSING_ARGS("actual", "expected");
  if (isDeepEqual(actual, expected)) {
    innerFail({ actual, expected, message, operator: "notDeepEqual", stackStartFn: notDeepEqual });
  }
};

assert.deepStrictEqual = function deepStrictEqual(actual, expected, message) {
  if (arguments.length < 2) throw new ERR_MISSING_ARGS("actual", "expected");
  if (!isDeepStrictEqual(actual, expected)) {
    innerFail({ actual, expected, message, operator: "deepStrictEqual", stackStartFn: deepStrictEqual });
  }
};

assert.notDeepStrictEqual = notDeepStrictEqual;
function notDeepStrictEqual(actual, expected, message) {
  if (arguments.length < 2) throw new ERR_MISSING_ARGS("actual", "expected");
  if (isDeepStrictEqual(actual, expected)) {
    innerFail({ actual, expected, message, operator: "notDeepStrictEqual", stackStartFn: notDeepStrictEqual });
  }
}

assert.strictEqual = function strictEqual(actual, expected, message) {
  if (arguments.length < 2) throw new ERR_MISSING_ARGS("actual", "expected");
  if (!Object.is(actual, expected)) {
    innerFail({ actual, expected, message, operator: "strictEqual", stackStartFn: strictEqual });
  }
};

assert.notStrictEqual = function notStrictEqual(actual, expected, message) {
  if (arguments.length < 2) throw new ERR_MISSING_ARGS("actual", "expected");
  if (Object.is(actual, expected)) {
    innerFail({ actual, expected, message, operator: "notStrictEqual", stackStartFn: notStrictEqual });
  }
};

class Comparison {
  constructor(obj, keys, actual) {
    for (const key of keys) {
      if (key in obj) {
        if (actual !== undefined && typeof actual[key] === "string" && isRegExp(obj[key]) &&
            RegExp.prototype.exec.call(obj[key], actual[key]) !== null) {
          this[key] = actual[key];
        } else {
          this[key] = obj[key];
        }
      }
    }
  }
}

function compareExceptionKey(actual, expected, key, message, keys, fn) {
  if (!(key in actual) || !isDeepStrictEqual(actual[key], expected[key])) {
    if (!message) {
      // Create placeholder objects to create a nice output.
      const a = new Comparison(actual, keys);
      const b = new Comparison(expected, keys, actual);
      const err = new AssertionError({ actual: a, expected: b, operator: "deepStrictEqual", stackStartFn: fn });
      err.actual = actual;
      err.expected = expected;
      err.operator = fn.name;
      throw err;
    }
    innerFail({ actual, expected, message, operator: fn.name, stackStartFn: fn });
  }
}

function expectedException(actual, expected, message, fn) {
  let generatedMessage = false;
  let throwError = false;

  if (typeof expected !== "function") {
    // Handle regular expressions.
    if (isRegExp(expected)) {
      const str = String(actual);
      if (RegExp.prototype.exec.call(expected, str) !== null) return;
      if (!message) {
        generatedMessage = true;
        message = "The input did not match the regular expression " +
          `${inspect(expected)}. Input:\n\n${inspect(str)}\n`;
      }
      throwError = true;
      // Handle primitives properly.
    } else if (typeof actual !== "object" || actual === null) {
      const err = new AssertionError({ actual, expected, message, operator: "deepStrictEqual", stackStartFn: fn });
      err.operator = fn.name;
      throw err;
    } else {
      // Handle validation objects.
      const keys = Object.keys(expected);
      // Special handle errors to make sure the name and the message are compared as well.
      if (expected instanceof Error) {
        keys.push("name", "message");
      } else if (keys.length === 0) {
        throw new ERR_INVALID_ARG_VALUE("error", expected, "may not be an empty object");
      }
      for (const key of keys) {
        if (typeof actual[key] === "string" && isRegExp(expected[key]) &&
            RegExp.prototype.exec.call(expected[key], actual[key]) !== null) {
          continue;
        }
        compareExceptionKey(actual, expected, key, message, keys, fn);
      }
      return;
    }
    // Guard instanceof against arrow functions as they don't have a prototype.
    // Check for matching Error classes.
  } else if (expected.prototype !== undefined && actual instanceof expected) {
    return;
  } else if (Error.isPrototypeOf(expected)) {
    if (!message) {
      generatedMessage = true;
      message = "The error is expected to be an instance of " + `"${expected.name}". Received `;
      if (isNativeError(actual)) {
        const name = (actual.constructor && actual.constructor.name) || actual.name;
        if (expected.name === name) {
          message += "an error with identical name but a different prototype.";
        } else {
          message += `"${name}"`;
        }
        if (actual.message) message += `\n\nError message:\n\n${actual.message}`;
      } else {
        message += `"${inspect(actual, { depth: -1 })}"`;
      }
    }
    throwError = true;
  } else {
    // Check validation functions return value next.
    const res = Reflect.apply(expected, {}, [actual]);
    if (res !== true) {
      if (!message) {
        generatedMessage = true;
        const name = expected.name ? `"${expected.name}" ` : "";
        message = `The ${name}validation function is expected to return` + ` "true". Received ${inspect(res)}`;
        if (isNativeError(actual)) message += `\n\nCaught error:\n\n${actual}`;
      }
      throwError = true;
    }
  }

  if (throwError) {
    const err = new AssertionError({ actual, expected, message, operator: fn.name, stackStartFn: fn });
    err.generatedMessage = generatedMessage;
    throw err;
  }
}

function getActual(fn) {
  __validators.validateFunction(fn, "fn");
  try {
    fn();
  } catch (e) {
    return e;
  }
  return NO_EXCEPTION_SENTINEL;
}

function checkIsPromise(obj) {
  // Accept native ES6 promises and promises that are implemented in a similar way. Do not accept
  // thenables that use a function as `obj` and that have no `catch` handler.
  return isPromise(obj) ||
    (obj !== null && typeof obj === "object" && typeof obj.then === "function" && typeof obj.catch === "function");
}

async function waitForActual(promiseFn) {
  let resultPromise;
  if (typeof promiseFn === "function") {
    // Return a rejected promise if `promiseFn` throws synchronously.
    resultPromise = promiseFn();
    // Fail in case no promise is returned.
    if (!checkIsPromise(resultPromise)) {
      throw new ERR_INVALID_RETURN_VALUE("instance of Promise", "promiseFn", resultPromise);
    }
  } else if (checkIsPromise(promiseFn)) {
    resultPromise = promiseFn;
  } else {
    throw new ERR_INVALID_ARG_TYPE("promiseFn", ["Function", "Promise"], promiseFn);
  }
  try {
    await resultPromise;
  } catch (e) {
    return e;
  }
  return NO_EXCEPTION_SENTINEL;
}

function expectsError(stackStartFn, actual, error, message) {
  if (typeof error === "string") {
    if (arguments.length === 4) {
      throw new ERR_INVALID_ARG_TYPE("error", ["Object", "Error", "Function", "RegExp"], error);
    }
    if (typeof actual === "object" && actual !== null) {
      if (actual.message === error) {
        throw new ERR_AMBIGUOUS_ARGUMENT("error/message", `The error message "${actual.message}" is identical to the message.`);
      }
    } else if (actual === error) {
      throw new ERR_AMBIGUOUS_ARGUMENT("error/message", `The error "${actual}" is identical to the message.`);
    }
    message = error;
    error = undefined;
  } else if (error != null && typeof error !== "object" && typeof error !== "function") {
    throw new ERR_INVALID_ARG_TYPE("error", ["Object", "Error", "Function", "RegExp"], error);
  }

  if (actual === NO_EXCEPTION_SENTINEL) {
    let details = "";
    if (error && error.name) details += ` (${error.name})`;
    details += message ? `: ${message}` : ".";
    const fnType = stackStartFn === assert.rejects ? "rejection" : "exception";
    innerFail({
      actual: undefined,
      expected: error,
      operator: stackStartFn.name,
      message: `Missing expected ${fnType}${details}`,
      stackStartFn,
    });
  }

  if (!error) return;
  expectedException(actual, error, message, stackStartFn);
}

function hasMatchingError(actual, expected) {
  if (typeof expected !== "function") {
    if (isRegExp(expected)) {
      const str = String(actual);
      return RegExp.prototype.exec.call(expected, str) !== null;
    }
    throw new ERR_INVALID_ARG_TYPE("expected", ["Function", "RegExp"], expected);
  }
  // Guard instanceof against arrow functions as they don't have a prototype.
  if (expected.prototype !== undefined && actual instanceof expected) return true;
  if (Error.isPrototypeOf(expected)) return false;
  return Reflect.apply(expected, {}, [actual]) === true;
}

function expectsNoError(stackStartFn, actual, error, message) {
  if (actual === NO_EXCEPTION_SENTINEL) return;
  if (typeof error === "string") {
    message = error;
    error = undefined;
  }
  if (!error || hasMatchingError(actual, error)) {
    const details = message ? `: ${message}` : ".";
    const fnType = stackStartFn === assert.doesNotReject ? "rejection" : "exception";
    innerFail({
      actual,
      expected: error,
      operator: stackStartFn.name,
      message: `Got unwanted ${fnType}${details}\n` + `Actual message: "${actual && actual.message}"`,
      stackStartFn,
    });
  }
  throw actual;
}

assert.throws = function throws(promiseFn, ...args) {
  expectsError(throws, getActual(promiseFn), ...args);
};

assert.rejects = async function rejects(promiseFn, ...args) {
  expectsError(rejects, await waitForActual(promiseFn), ...args);
};

assert.doesNotThrow = function doesNotThrow(fn, ...args) {
  expectsNoError(doesNotThrow, getActual(fn), ...args);
};

assert.doesNotReject = async function doesNotReject(fn, ...args) {
  expectsNoError(doesNotReject, await waitForActual(fn), ...args);
};

assert.ifError = function ifError(err) {
  if (err !== null && err !== undefined) {
    let message = "ifError got unwanted exception: ";
    if (typeof err === "object" && typeof err.message === "string") {
      if (err.message.length === 0 && err.constructor) {
        message += err.constructor.name;
      } else {
        message += err.message;
      }
    } else {
      message += inspect(err);
    }
    const newErr = new AssertionError({ actual: err, expected: null, operator: "ifError", message, stackStartFn: ifError });
    // Make sure the original error's stack is kept.
    const origStack = err.stack;
    if (typeof origStack === "string") {
      const origStackStart = origStack.indexOf("\n    at");
      if (origStackStart !== -1) {
        const originalFrames = origStack.slice(origStackStart + 1).split("\n");
        const newStack = newErr.stack.split("\n");
        const tmp2 = newStack.shift();
        newErr.stack = `${tmp2}\n${originalFrames.join("\n")}\n${newStack.join("\n")}`;
      }
    }
    throw newErr;
  }
};

function internalMatch(string, regexp, message, fn) {
  if (!isRegExp(regexp)) {
    throw new ERR_INVALID_ARG_TYPE("regexp", "RegExp", regexp);
  }
  const match = fn === assert.match;
  if (typeof string !== "string" || (RegExp.prototype.exec.call(regexp, string) !== null) !== match) {
    if (message instanceof Error) throw message;
    const generatedMessage = !message;
    message = message || (typeof string !== "string"
      ? 'The "string" argument must be of type string. Received type ' + `${typeof string} (${inspect(string)})`
      : (match
        ? "The input did not match the regular expression "
        : "The input was expected to not match the regular expression ") +
        `${inspect(regexp)}. Input:\n\n${inspect(string)}\n`);
    const err = new AssertionError({ actual: string, expected: regexp, message, operator: fn.name, stackStartFn: fn });
    err.generatedMessage = generatedMessage;
    throw err;
  }
}

assert.match = function match(string, regexp, message) {
  internalMatch(string, regexp, message, match);
};

assert.doesNotMatch = function doesNotMatch(string, regexp, message) {
  internalMatch(string, regexp, message, doesNotMatch);
};

// Deprecated in Node, still exported: verifies functions are called an exact number of times.
class CallTracker {
  #callChecks = new Set();
  calls(fn, expected = 1) {
    if (typeof fn === "number") {
      expected = fn;
      fn = () => {};
    } else if (fn === undefined) {
      fn = () => {};
    }
    __validators.validateUint32(expected, "expected", true);
    const context = { expected, actual: 0, name: fn.name || "calls", stackTrace: new Error(), calls: [] };
    this.#callChecks.add(context);
    const tracked = new Proxy(fn, {
      apply(target, thisArg, argList) {
        context.actual++;
        context.calls.push({ thisArg, arguments: argList });
        return Reflect.apply(target, thisArg, argList);
      },
    });
    this.#tracked.set(tracked, context);
    return tracked;
  }
  #tracked = new WeakMap();
  getCalls(fn) {
    const context = this.#tracked.get(fn);
    if (!context) throw new ERR_INVALID_ARG_VALUE("fn", fn, "is not a tracked function");
    return context.calls.map((c) => ({ ...c }));
  }
  reset(fn) {
    if (fn === undefined) {
      for (const context of this.#callChecks) {
        context.actual = 0;
        context.calls = [];
      }
      return;
    }
    const context = this.#tracked.get(fn);
    if (!context) throw new ERR_INVALID_ARG_VALUE("fn", fn, "is not a tracked function");
    context.actual = 0;
    context.calls = [];
  }
  report() {
    const errors = [];
    for (const context of this.#callChecks) {
      if (context.actual !== context.expected) {
        const message = `Expected the ${context.name} function to be executed ${context.expected} time(s) but was executed ${context.actual} time(s).`;
        errors.push({
          message,
          actual: context.actual,
          expected: context.expected,
          operator: context.name,
          stack: context.stackTrace,
        });
      }
    }
    return errors;
  }
  verify() {
    const errors = this.report();
    if (errors.length === 0) return;
    const message = errors.length === 1
      ? errors[0].message
      : "Functions were not called the expected number of times";
    throw new AssertionError({ message, details: errors });
  }
}
assert.CallTracker = CallTracker;

// The strict view: the loose comparators become their strict counterparts.
function strict(...args) {
  innerOk(strict, args.length, ...args);
}
assert.strict = Object.assign(strict, assert, {
  equal: assert.strictEqual,
  deepEqual: assert.deepStrictEqual,
  notEqual: assert.notStrictEqual,
  notDeepEqual: assert.notDeepStrictEqual,
});
assert.strict.strict = assert.strict;

__builtins.set("assert", assert);
__builtins.set("assert/strict", assert.strict);
