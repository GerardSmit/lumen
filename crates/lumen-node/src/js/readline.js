// node:readline and node:readline/promises — Node v20.11's own lib/readline.js,
// lib/readline/promises.js and lib/internal/readline/*.js (MIT, Copyright Joyent, Inc. and other
// Node contributors), each as written there inside a small CommonJS loader. `require` resolves
// the ported modules first, then the shims below for Node internals, then `__builtins`.
{
  const primordials = __primordials;
  const util = __builtins.get("util");
  const { kFirstEventParam } = __internals.get("events_symbols");
  const { getStringWidth } = __internals.get("util_inspect");

  class AbortError extends Error {
    constructor(message = "The operation was aborted", options = undefined) {
      super(message, options);
      this.code = "ABORT_ERR";
      this.name = "AbortError";
    }
  }

  const shims = {
    __proto__: null,
    "internal/errors": { codes: __errors, AbortError },
    "internal/validators": __validators,
    "internal/util": { kEmptyObject: Object.freeze({ __proto__: null }), promisify: util.promisify },
    "internal/util/inspect": {
      inspect: util.inspect,
      getStringWidth,
      stripVTControlCharacters: util.stripVTControlCharacters,
    },
    "internal/events/symbols": { kFirstEventParam },
    "internal/streams/utils": {
      // lumen: stream.js keeps its isWritable private; this is its Writable-stream case.
      isWritable(stream) {
        if (typeof stream?.writable !== "boolean") return null;
        if (stream.destroyed) return false;
        return typeof stream.write === "function" && stream.writable && !stream.writableEnded;
      },
    },
  };

  const defs = { __proto__: null };
  const cache = { __proto__: null };
  const require = (id) => {
    if (id in cache) return cache[id].exports;
    if (id in defs) {
      const module = { exports: {} };
      cache[id] = module;
      defs[id](module, module.exports);
      return module.exports;
    }
    if (id in shims) return shims[id];
    return __builtins.get(id);
  };

  defs["internal/readline/utils"] = function (module, exports) {
    const {
      ArrayPrototypeSlice,
      ArrayPrototypeSort,
      RegExpPrototypeExec,
      StringFromCharCode,
      StringPrototypeCharCodeAt,
      StringPrototypeCodePointAt,
      StringPrototypeSlice,
      StringPrototypeToLowerCase,
      Symbol,
    } = primordials;

    const kUTF16SurrogateThreshold = 0x10000; // 2 ** 16
    const kEscape = '\x1b';
    const kSubstringSearch = Symbol('kSubstringSearch');

    function CSI(strings, ...args) {
      let ret = `${kEscape}[`;
      for (let n = 0; n < strings.length; n++) {
        ret += strings[n];
        if (n < args.length)
          ret += args[n];
      }
      return ret;
    }

    CSI.kEscape = kEscape;
    CSI.kClearToLineBeginning = CSI`1K`;
    CSI.kClearToLineEnd = CSI`0K`;
    CSI.kClearLine = CSI`2K`;
    CSI.kClearScreenDown = CSI`0J`;

    // TODO(BridgeAR): Treat combined characters as single character, i.e,
    // 'a\u0301' and '\u0301a' (both have the same visual output).
    // Check Canonical_Combining_Class in
    // http://userguide.icu-project.org/strings/properties
    function charLengthLeft(str, i) {
      if (i <= 0)
        return 0;
      if ((i > 1 &&
          StringPrototypeCodePointAt(str, i - 2) >= kUTF16SurrogateThreshold) ||
          StringPrototypeCodePointAt(str, i - 1) >= kUTF16SurrogateThreshold) {
        return 2;
      }
      return 1;
    }

    function charLengthAt(str, i) {
      if (str.length <= i) {
        // Pretend to move to the right. This is necessary to autocomplete while
        // moving to the right.
        return 1;
      }
      return StringPrototypeCodePointAt(str, i) >= kUTF16SurrogateThreshold ? 2 : 1;
    }

    /*
      Some patterns seen in terminal key escape codes, derived from combos seen
      at http://www.midnight-commander.org/browser/lib/tty/key.c

      ESC letter
      ESC [ letter
      ESC [ modifier letter
      ESC [ 1 ; modifier letter
      ESC [ num char
      ESC [ num ; modifier char
      ESC O letter
      ESC O modifier letter
      ESC O 1 ; modifier letter
      ESC N letter
      ESC [ [ num ; modifier char
      ESC [ [ 1 ; modifier letter
      ESC ESC [ num char
      ESC ESC O letter

      - char is usually ~ but $ and ^ also happen with rxvt
      - modifier is 1 +
                    (shift     * 1) +
                    (left_alt  * 2) +
                    (ctrl      * 4) +
                    (right_alt * 8)
      - two leading ESCs apparently mean the same as one leading ESC
    */
    function* emitKeys(stream) {
      while (true) {
        let ch = yield;
        let s = ch;
        let escaped = false;
        const key = {
          sequence: null,
          name: undefined,
          ctrl: false,
          meta: false,
          shift: false,
        };

        if (ch === kEscape) {
          escaped = true;
          s += (ch = yield);

          if (ch === kEscape) {
            s += (ch = yield);
          }
        }

        if (escaped && (ch === 'O' || ch === '[')) {
          // ANSI escape sequence
          let code = ch;
          let modifier = 0;

          if (ch === 'O') {
            // ESC O letter
            // ESC O modifier letter
            s += (ch = yield);

            if (ch >= '0' && ch <= '9') {
              modifier = (ch >> 0) - 1;
              s += (ch = yield);
            }

            code += ch;
          } else if (ch === '[') {
            // ESC [ letter
            // ESC [ modifier letter
            // ESC [ [ modifier letter
            // ESC [ [ num char
            s += (ch = yield);

            if (ch === '[') {
              // \x1b[[A
              //      ^--- escape codes might have a second bracket
              code += ch;
              s += (ch = yield);
            }

            /*
             * Here and later we try to buffer just enough data to get
             * a complete ascii sequence.
             *
             * We have basically two classes of ascii characters to process:
             *
             *
             * 1. `\x1b[24;5~` should be parsed as { code: '[24~', modifier: 5 }
             *
             * This particular example is featuring Ctrl+F12 in xterm.
             *
             *  - `;5` part is optional, e.g. it could be `\x1b[24~`
             *  - first part can contain one or two digits
             *  - there is also special case when there can be 3 digits
             *    but without modifier. They are the case of paste bracket mode
             *
             * So the generic regexp is like /^(?:\d\d?(;\d)?[~^$]|\d{3}~)$/
             *
             *
             * 2. `\x1b[1;5H` should be parsed as { code: '[H', modifier: 5 }
             *
             * This particular example is featuring Ctrl+Home in xterm.
             *
             *  - `1;5` part is optional, e.g. it could be `\x1b[H`
             *  - `1;` part is optional, e.g. it could be `\x1b[5H`
             *
             * So the generic regexp is like /^((\d;)?\d)?[A-Za-z]$/
             *
             */
            const cmdStart = s.length - 1;

            // Skip one or two leading digits
            if (ch >= '0' && ch <= '9') {
              s += (ch = yield);

              if (ch >= '0' && ch <= '9') {
                s += (ch = yield);

                if (ch >= '0' && ch <= '9') {
                  s += (ch = yield);
                }
              }
            }

            // skip modifier
            if (ch === ';') {
              s += (ch = yield);

              if (ch >= '0' && ch <= '9') {
                s += yield;
              }
            }

            /*
             * We buffered enough data, now trying to extract code
             * and modifier from it
             */
            const cmd = StringPrototypeSlice(s, cmdStart);
            let match;

            if ((match = RegExpPrototypeExec(/^(?:(\d\d?)(?:;(\d))?([~^$])|(\d{3}~))$/, cmd))) {
              if (match[4]) {
                code += match[4];
              } else {
                code += match[1] + match[3];
                modifier = (match[2] || 1) - 1;
              }
            } else if (
              (match = RegExpPrototypeExec(/^((\d;)?(\d))?([A-Za-z])$/, cmd))
            ) {
              code += match[4];
              modifier = (match[3] || 1) - 1;
            } else {
              code += cmd;
            }
          }

          // Parse the key modifier
          key.ctrl = !!(modifier & 4);
          key.meta = !!(modifier & 10);
          key.shift = !!(modifier & 1);
          key.code = code;

          // Parse the key itself
          switch (code) {
            /* xterm/gnome ESC [ letter (with modifier) */
            case '[P': key.name = 'f1'; break;
            case '[Q': key.name = 'f2'; break;
            case '[R': key.name = 'f3'; break;
            case '[S': key.name = 'f4'; break;

            /* xterm/gnome ESC O letter (without modifier) */
            case 'OP': key.name = 'f1'; break;
            case 'OQ': key.name = 'f2'; break;
            case 'OR': key.name = 'f3'; break;
            case 'OS': key.name = 'f4'; break;

            /* xterm/rxvt ESC [ number ~ */
            case '[11~': key.name = 'f1'; break;
            case '[12~': key.name = 'f2'; break;
            case '[13~': key.name = 'f3'; break;
            case '[14~': key.name = 'f4'; break;

            /* paste bracket mode */
            case '[200~': key.name = 'paste-start'; break;
            case '[201~': key.name = 'paste-end'; break;

            /* from Cygwin and used in libuv */
            case '[[A': key.name = 'f1'; break;
            case '[[B': key.name = 'f2'; break;
            case '[[C': key.name = 'f3'; break;
            case '[[D': key.name = 'f4'; break;
            case '[[E': key.name = 'f5'; break;

            /* common */
            case '[15~': key.name = 'f5'; break;
            case '[17~': key.name = 'f6'; break;
            case '[18~': key.name = 'f7'; break;
            case '[19~': key.name = 'f8'; break;
            case '[20~': key.name = 'f9'; break;
            case '[21~': key.name = 'f10'; break;
            case '[23~': key.name = 'f11'; break;
            case '[24~': key.name = 'f12'; break;

            /* xterm ESC [ letter */
            case '[A': key.name = 'up'; break;
            case '[B': key.name = 'down'; break;
            case '[C': key.name = 'right'; break;
            case '[D': key.name = 'left'; break;
            case '[E': key.name = 'clear'; break;
            case '[F': key.name = 'end'; break;
            case '[H': key.name = 'home'; break;

            /* xterm/gnome ESC O letter */
            case 'OA': key.name = 'up'; break;
            case 'OB': key.name = 'down'; break;
            case 'OC': key.name = 'right'; break;
            case 'OD': key.name = 'left'; break;
            case 'OE': key.name = 'clear'; break;
            case 'OF': key.name = 'end'; break;
            case 'OH': key.name = 'home'; break;

            /* xterm/rxvt ESC [ number ~ */
            case '[1~': key.name = 'home'; break;
            case '[2~': key.name = 'insert'; break;
            case '[3~': key.name = 'delete'; break;
            case '[4~': key.name = 'end'; break;
            case '[5~': key.name = 'pageup'; break;
            case '[6~': key.name = 'pagedown'; break;

            /* putty */
            case '[[5~': key.name = 'pageup'; break;
            case '[[6~': key.name = 'pagedown'; break;

            /* rxvt */
            case '[7~': key.name = 'home'; break;
            case '[8~': key.name = 'end'; break;

            /* rxvt keys with modifiers */
            case '[a': key.name = 'up'; key.shift = true; break;
            case '[b': key.name = 'down'; key.shift = true; break;
            case '[c': key.name = 'right'; key.shift = true; break;
            case '[d': key.name = 'left'; key.shift = true; break;
            case '[e': key.name = 'clear'; key.shift = true; break;

            case '[2$': key.name = 'insert'; key.shift = true; break;
            case '[3$': key.name = 'delete'; key.shift = true; break;
            case '[5$': key.name = 'pageup'; key.shift = true; break;
            case '[6$': key.name = 'pagedown'; key.shift = true; break;
            case '[7$': key.name = 'home'; key.shift = true; break;
            case '[8$': key.name = 'end'; key.shift = true; break;

            case 'Oa': key.name = 'up'; key.ctrl = true; break;
            case 'Ob': key.name = 'down'; key.ctrl = true; break;
            case 'Oc': key.name = 'right'; key.ctrl = true; break;
            case 'Od': key.name = 'left'; key.ctrl = true; break;
            case 'Oe': key.name = 'clear'; key.ctrl = true; break;

            case '[2^': key.name = 'insert'; key.ctrl = true; break;
            case '[3^': key.name = 'delete'; key.ctrl = true; break;
            case '[5^': key.name = 'pageup'; key.ctrl = true; break;
            case '[6^': key.name = 'pagedown'; key.ctrl = true; break;
            case '[7^': key.name = 'home'; key.ctrl = true; break;
            case '[8^': key.name = 'end'; key.ctrl = true; break;

            /* misc. */
            case '[Z': key.name = 'tab'; key.shift = true; break;
            default: key.name = 'undefined'; break;
          }
        } else if (ch === '\r') {
          // carriage return
          key.name = 'return';
          key.meta = escaped;
        } else if (ch === '\n') {
          // Enter, should have been called linefeed
          key.name = 'enter';
          key.meta = escaped;
        } else if (ch === '\t') {
          // tab
          key.name = 'tab';
          key.meta = escaped;
        } else if (ch === '\b' || ch === '\x7f') {
          // backspace or ctrl+h
          key.name = 'backspace';
          key.meta = escaped;
        } else if (ch === kEscape) {
          // escape key
          key.name = 'escape';
          key.meta = escaped;
        } else if (ch === ' ') {
          key.name = 'space';
          key.meta = escaped;
        } else if (!escaped && ch <= '\x1a') {
          // ctrl+letter
          key.name = StringFromCharCode(
            StringPrototypeCharCodeAt(ch) + StringPrototypeCharCodeAt('a') - 1,
          );
          key.ctrl = true;
        } else if (RegExpPrototypeExec(/^[0-9A-Za-z]$/, ch) !== null) {
          // Letter, number, shift+letter
          key.name = StringPrototypeToLowerCase(ch);
          key.shift = RegExpPrototypeExec(/^[A-Z]$/, ch) !== null;
          key.meta = escaped;
        } else if (escaped) {
          // Escape sequence timeout
          key.name = ch.length ? undefined : 'escape';
          key.meta = true;
        }

        key.sequence = s;

        if (s.length !== 0 && (key.name !== undefined || escaped)) {
          /* Named character or sequence */
          stream.emit('keypress', escaped ? undefined : s, key);
        } else if (charLengthAt(s, 0) === s.length) {
          /* Single unnamed character, e.g. "." */
          stream.emit('keypress', s, key);
        }
        /* Unrecognized or broken escape sequence, don't emit anything */
      }
    }

    // This runs in O(n log n).
    function commonPrefix(strings) {
      if (strings.length === 0) {
        return '';
      }
      if (strings.length === 1) {
        return strings[0];
      }
      const sorted = ArrayPrototypeSort(ArrayPrototypeSlice(strings));
      const min = sorted[0];
      const max = sorted[sorted.length - 1];
      for (let i = 0; i < min.length; i++) {
        if (min[i] !== max[i]) {
          return StringPrototypeSlice(min, 0, i);
        }
      }
      return min;
    }

    module.exports = {
      charLengthAt,
      charLengthLeft,
      commonPrefix,
      emitKeys,
      kSubstringSearch,
      CSI,
    };
  };

  defs["internal/readline/callbacks"] = function (module, exports) {
    const {
      NumberIsNaN,
    } = primordials;

    const {
      codes: {
        ERR_INVALID_ARG_VALUE,
        ERR_INVALID_CURSOR_POS,
      },
    } = require('internal/errors');

    const {
      validateFunction,
    } = require('internal/validators');
    const {
      CSI,
    } = require('internal/readline/utils');

    const {
      kClearLine,
      kClearScreenDown,
      kClearToLineBeginning,
      kClearToLineEnd,
    } = CSI;


    /**
     * moves the cursor to the x and y coordinate on the given stream
     */

    function cursorTo(stream, x, y, callback) {
      if (callback !== undefined) {
        validateFunction(callback, 'callback');
      }

      if (typeof y === 'function') {
        callback = y;
        y = undefined;
      }

      if (NumberIsNaN(x)) throw new ERR_INVALID_ARG_VALUE('x', x);
      if (NumberIsNaN(y)) throw new ERR_INVALID_ARG_VALUE('y', y);

      if (stream == null || (typeof x !== 'number' && typeof y !== 'number')) {
        if (typeof callback === 'function') process.nextTick(callback, null);
        return true;
      }

      if (typeof x !== 'number') throw new ERR_INVALID_CURSOR_POS();

      const data = typeof y !== 'number' ? CSI`${x + 1}G` : CSI`${y + 1};${x + 1}H`;
      return stream.write(data, callback);
    }

    /**
     * moves the cursor relative to its current location
     */

    function moveCursor(stream, dx, dy, callback) {
      if (callback !== undefined) {
        validateFunction(callback, 'callback');
      }

      if (stream == null || !(dx || dy)) {
        if (typeof callback === 'function') process.nextTick(callback, null);
        return true;
      }

      let data = '';

      if (dx < 0) {
        data += CSI`${-dx}D`;
      } else if (dx > 0) {
        data += CSI`${dx}C`;
      }

      if (dy < 0) {
        data += CSI`${-dy}A`;
      } else if (dy > 0) {
        data += CSI`${dy}B`;
      }

      return stream.write(data, callback);
    }

    /**
     * clears the current line the cursor is on:
     *   -1 for left of the cursor
     *   +1 for right of the cursor
     *    0 for the entire line
     */

    function clearLine(stream, dir, callback) {
      if (callback !== undefined) {
        validateFunction(callback, 'callback');
      }

      if (stream === null || stream === undefined) {
        if (typeof callback === 'function') process.nextTick(callback, null);
        return true;
      }

      const type =
        dir < 0 ? kClearToLineBeginning : dir > 0 ? kClearToLineEnd : kClearLine;
      return stream.write(type, callback);
    }

    /**
     * clears the screen from the current position of the cursor down
     */

    function clearScreenDown(stream, callback) {
      if (callback !== undefined) {
        validateFunction(callback, 'callback');
      }

      if (stream === null || stream === undefined) {
        if (typeof callback === 'function') process.nextTick(callback, null);
        return true;
      }

      return stream.write(kClearScreenDown, callback);
    }

    module.exports = {
      clearLine,
      clearScreenDown,
      cursorTo,
      moveCursor,
    };
  };

  defs["internal/readline/interface"] = function (module, exports) {
    const {
      ArrayFrom,
      ArrayPrototypeFilter,
      ArrayPrototypeIndexOf,
      ArrayPrototypeJoin,
      ArrayPrototypeMap,
      ArrayPrototypePop,
      ArrayPrototypePush,
      ArrayPrototypeReverse,
      ArrayPrototypeSplice,
      ArrayPrototypeShift,
      ArrayPrototypeUnshift,
      DateNow,
      FunctionPrototypeCall,
      MathCeil,
      MathFloor,
      MathMax,
      MathMaxApply,
      NumberIsFinite,
      ObjectSetPrototypeOf,
      RegExpPrototypeExec,
      StringPrototypeCodePointAt,
      StringPrototypeEndsWith,
      StringPrototypeRepeat,
      StringPrototypeSlice,
      StringPrototypeStartsWith,
      StringPrototypeTrim,
      Symbol,
      SymbolDispose,
      SymbolAsyncIterator,
      SafeStringIterator,
    } = primordials;

    const { codes } = require('internal/errors');

    const {
      ERR_INVALID_ARG_VALUE,
      ERR_USE_AFTER_CLOSE,
    } = codes;
    const {
      validateAbortSignal,
      validateArray,
      validateNumber,
      validateString,
      validateUint32,
    } = require('internal/validators');
    const { kEmptyObject } = require('internal/util');
    const {
      inspect,
      getStringWidth,
      stripVTControlCharacters,
    } = require('internal/util/inspect');
    const EventEmitter = require('events');
    const {
      charLengthAt,
      charLengthLeft,
      commonPrefix,
      kSubstringSearch,
    } = require('internal/readline/utils');
    let emitKeypressEvents;
    let kFirstEventParam;
    const {
      clearScreenDown,
      cursorTo,
      moveCursor,
    } = require('internal/readline/callbacks');

    const { StringDecoder } = require('string_decoder');

    const kHistorySize = 30;
    const kMaxUndoRedoStackSize = 2048;
    const kMincrlfDelay = 100;
    // \r\n, \n, or \r followed by something other than \n
    const lineEnding = /\r?\n|\r(?!\n)/g;

    const kLineObjectStream = Symbol('line object stream');
    const kQuestionCancel = Symbol('kQuestionCancel');
    const kQuestion = Symbol('kQuestion');

    // GNU readline library - keyseq-timeout is 500ms (default)
    const ESCAPE_CODE_TIMEOUT = 500;

    // Max length of the kill ring
    const kMaxLengthOfKillRing = 32;

    const kAddHistory = Symbol('_addHistory');
    const kBeforeEdit = Symbol('_beforeEdit');
    const kDecoder = Symbol('_decoder');
    const kDeleteLeft = Symbol('_deleteLeft');
    const kDeleteLineLeft = Symbol('_deleteLineLeft');
    const kDeleteLineRight = Symbol('_deleteLineRight');
    const kDeleteRight = Symbol('_deleteRight');
    const kDeleteWordLeft = Symbol('_deleteWordLeft');
    const kDeleteWordRight = Symbol('_deleteWordRight');
    const kGetDisplayPos = Symbol('_getDisplayPos');
    const kHistoryNext = Symbol('_historyNext');
    const kHistoryPrev = Symbol('_historyPrev');
    const kInsertString = Symbol('_insertString');
    const kLine = Symbol('_line');
    const kLine_buffer = Symbol('_line_buffer');
    const kKillRing = Symbol('_killRing');
    const kKillRingCursor = Symbol('_killRingCursor');
    const kMoveCursor = Symbol('_moveCursor');
    const kNormalWrite = Symbol('_normalWrite');
    const kOldPrompt = Symbol('_oldPrompt');
    const kOnLine = Symbol('_onLine');
    const kPreviousKey = Symbol('_previousKey');
    const kPrompt = Symbol('_prompt');
    const kPushToKillRing = Symbol('_pushToKillRing');
    const kPushToUndoStack = Symbol('_pushToUndoStack');
    const kQuestionCallback = Symbol('_questionCallback');
    const kRedo = Symbol('_redo');
    const kRedoStack = Symbol('_redoStack');
    const kRefreshLine = Symbol('_refreshLine');
    const kSawKeyPress = Symbol('_sawKeyPress');
    const kSawReturnAt = Symbol('_sawReturnAt');
    const kSetRawMode = Symbol('_setRawMode');
    const kTabComplete = Symbol('_tabComplete');
    const kTabCompleter = Symbol('_tabCompleter');
    const kTtyWrite = Symbol('_ttyWrite');
    const kUndo = Symbol('_undo');
    const kUndoStack = Symbol('_undoStack');
    const kWordLeft = Symbol('_wordLeft');
    const kWordRight = Symbol('_wordRight');
    const kWriteToOutput = Symbol('_writeToOutput');
    const kYank = Symbol('_yank');
    const kYanking = Symbol('_yanking');
    const kYankPop = Symbol('_yankPop');

    function InterfaceConstructor(input, output, completer, terminal) {
      this[kSawReturnAt] = 0;
      // TODO(BridgeAR): Document this property. The name is not ideal, so we
      // might want to expose an alias and document that instead.
      this.isCompletionEnabled = true;
      this[kSawKeyPress] = false;
      this[kPreviousKey] = null;
      this.escapeCodeTimeout = ESCAPE_CODE_TIMEOUT;
      this.tabSize = 8;

      FunctionPrototypeCall(EventEmitter, this);

      let history;
      let historySize;
      let removeHistoryDuplicates = false;
      let crlfDelay;
      let prompt = '> ';
      let signal;

      if (input?.input) {
        // An options object was given
        output = input.output;
        completer = input.completer;
        terminal = input.terminal;
        history = input.history;
        historySize = input.historySize;
        signal = input.signal;
        if (input.tabSize !== undefined) {
          validateUint32(input.tabSize, 'tabSize', true);
          this.tabSize = input.tabSize;
        }
        removeHistoryDuplicates = input.removeHistoryDuplicates;
        if (input.prompt !== undefined) {
          prompt = input.prompt;
        }
        if (input.escapeCodeTimeout !== undefined) {
          if (NumberIsFinite(input.escapeCodeTimeout)) {
            this.escapeCodeTimeout = input.escapeCodeTimeout;
          } else {
            throw new ERR_INVALID_ARG_VALUE(
              'input.escapeCodeTimeout',
              this.escapeCodeTimeout,
            );
          }
        }

        if (signal) {
          validateAbortSignal(signal, 'options.signal');
        }

        crlfDelay = input.crlfDelay;
        input = input.input;
      }

      if (completer !== undefined && typeof completer !== 'function') {
        throw new ERR_INVALID_ARG_VALUE('completer', completer);
      }

      if (history === undefined) {
        history = [];
      } else {
        validateArray(history, 'history');
      }

      if (historySize === undefined) {
        historySize = kHistorySize;
      }

      validateNumber(historySize, 'historySize', 0);

      // Backwards compat; check the isTTY prop of the output stream
      //  when `terminal` was not specified
      if (terminal === undefined && !(output === null || output === undefined)) {
        terminal = !!output.isTTY;
      }

      const self = this;

      this.line = '';
      this[kSubstringSearch] = null;
      this.output = output;
      this.input = input;
      this[kUndoStack] = [];
      this[kRedoStack] = [];
      this.history = history;
      this.historySize = historySize;

      // The kill ring is a global list of blocks of text that were previously
      // killed (deleted). If its size exceeds kMaxLengthOfKillRing, the oldest
      // element will be removed to make room for the latest deletion. With kill
      // ring, users are able to recall (yank) or cycle (yank pop) among previously
      // killed texts, quite similar to the behavior of Emacs.
      this[kKillRing] = [];
      this[kKillRingCursor] = 0;

      this.removeHistoryDuplicates = !!removeHistoryDuplicates;
      this.crlfDelay = crlfDelay ?
        MathMax(kMincrlfDelay, crlfDelay) :
        kMincrlfDelay;
      this.completer = completer;

      this.setPrompt(prompt);

      this.terminal = !!terminal;


      function onerror(err) {
        self.emit('error', err);
      }

      function ondata(data) {
        self[kNormalWrite](data);
      }

      function onend() {
        if (
          typeof self[kLine_buffer] === 'string' &&
            self[kLine_buffer].length > 0
        ) {
          self.emit('line', self[kLine_buffer]);
        }
        self.close();
      }

      function ontermend() {
        if (typeof self.line === 'string' && self.line.length > 0) {
          self.emit('line', self.line);
        }
        self.close();
      }

      function onkeypress(s, key) {
        self[kTtyWrite](s, key);
        if (key && key.sequence) {
          // If the key.sequence is half of a surrogate pair
          // (>= 0xd800 and <= 0xdfff), refresh the line so
          // the character is displayed appropriately.
          const ch = StringPrototypeCodePointAt(key.sequence, 0);
          if (ch >= 0xd800 && ch <= 0xdfff) self[kRefreshLine]();
        }
      }

      function onresize() {
        self[kRefreshLine]();
      }

      this[kLineObjectStream] = undefined;

      input.on('error', onerror);

      if (!this.terminal) {
        function onSelfCloseWithoutTerminal() {
          input.removeListener('data', ondata);
          input.removeListener('error', onerror);
          input.removeListener('end', onend);
        }

        input.on('data', ondata);
        input.on('end', onend);
        self.once('close', onSelfCloseWithoutTerminal);
        this[kDecoder] = new StringDecoder('utf8');
      } else {
        function onSelfCloseWithTerminal() {
          input.removeListener('keypress', onkeypress);
          input.removeListener('error', onerror);
          input.removeListener('end', ontermend);
          if (output !== null && output !== undefined) {
            output.removeListener('resize', onresize);
          }
        }

        emitKeypressEvents ??= require('internal/readline/emitKeypressEvents');
        emitKeypressEvents(input, this);

        // `input` usually refers to stdin
        input.on('keypress', onkeypress);
        input.on('end', ontermend);

        this[kSetRawMode](true);
        this.terminal = true;

        // Cursor position on the line.
        this.cursor = 0;

        this.historyIndex = -1;

        if (output !== null && output !== undefined)
          output.on('resize', onresize);

        self.once('close', onSelfCloseWithTerminal);
      }

      if (signal) {
        const onAborted = () => self.close();
        if (signal.aborted) {
          process.nextTick(onAborted);
        } else {
          const disposable = EventEmitter.addAbortListener(signal, onAborted);
          self.once('close', disposable[SymbolDispose]);
        }
      }

      // Current line
      this.line = '';

      input.resume();
    }

    ObjectSetPrototypeOf(InterfaceConstructor.prototype, EventEmitter.prototype);
    ObjectSetPrototypeOf(InterfaceConstructor, EventEmitter);

    class Interface extends InterfaceConstructor {
      // eslint-disable-next-line no-useless-constructor
      constructor(input, output, completer, terminal) {
        super(input, output, completer, terminal);
      }
      get columns() {
        if (this.output && this.output.columns) return this.output.columns;
        return Infinity;
      }

      /**
       * Sets the prompt written to the output.
       * @param {string} prompt
       * @returns {void}
       */
      setPrompt(prompt) {
        this[kPrompt] = prompt;
      }

      /**
       * Returns the current prompt used by `rl.prompt()`.
       * @returns {string}
       */
      getPrompt() {
        return this[kPrompt];
      }

      [kSetRawMode](mode) {
        const wasInRawMode = this.input.isRaw;

        if (typeof this.input.setRawMode === 'function') {
          this.input.setRawMode(mode);
        }

        return wasInRawMode;
      }

      /**
       * Writes the configured `prompt` to a new line in `output`.
       * @param {boolean} [preserveCursor]
       * @returns {void}
       */
      prompt(preserveCursor) {
        if (this.paused) this.resume();
        if (this.terminal && process.env.TERM !== 'dumb') {
          if (!preserveCursor) this.cursor = 0;
          this[kRefreshLine]();
        } else {
          this[kWriteToOutput](this[kPrompt]);
        }
      }

      [kQuestion](query, cb) {
        if (this.closed) {
          throw new ERR_USE_AFTER_CLOSE('readline');
        }
        if (this[kQuestionCallback]) {
          this.prompt();
        } else {
          this[kOldPrompt] = this[kPrompt];
          this.setPrompt(query);
          this[kQuestionCallback] = cb;
          this.prompt();
        }
      }

      [kOnLine](line) {
        if (this[kQuestionCallback]) {
          const cb = this[kQuestionCallback];
          this[kQuestionCallback] = null;
          this.setPrompt(this[kOldPrompt]);
          cb(line);
        } else {
          this.emit('line', line);
        }
      }

      [kBeforeEdit](oldText, oldCursor) {
        this[kPushToUndoStack](oldText, oldCursor);
      }

      [kQuestionCancel]() {
        if (this[kQuestionCallback]) {
          this[kQuestionCallback] = null;
          this.setPrompt(this[kOldPrompt]);
          this.clearLine();
        }
      }

      [kWriteToOutput](stringToWrite) {
        validateString(stringToWrite, 'stringToWrite');

        if (this.output !== null && this.output !== undefined) {
          this.output.write(stringToWrite);
        }
      }

      [kAddHistory]() {
        if (this.line.length === 0) return '';

        // If the history is disabled then return the line
        if (this.historySize === 0) return this.line;

        // If the trimmed line is empty then return the line
        if (StringPrototypeTrim(this.line).length === 0) return this.line;

        if (this.history.length === 0 || this.history[0] !== this.line) {
          if (this.removeHistoryDuplicates) {
            // Remove older history line if identical to new one
            const dupIndex = ArrayPrototypeIndexOf(this.history, this.line);
            if (dupIndex !== -1) ArrayPrototypeSplice(this.history, dupIndex, 1);
          }

          ArrayPrototypeUnshift(this.history, this.line);

          // Only store so many
          if (this.history.length > this.historySize)
            ArrayPrototypePop(this.history);
        }

        this.historyIndex = -1;

        // The listener could change the history object, possibly
        // to remove the last added entry if it is sensitive and should
        // not be persisted in the history, like a password
        const line = this.history[0];

        // Emit history event to notify listeners of update
        this.emit('history', this.history);

        return line;
      }

      [kRefreshLine]() {
        // line length
        const line = this[kPrompt] + this.line;
        const dispPos = this[kGetDisplayPos](line);
        const lineCols = dispPos.cols;
        const lineRows = dispPos.rows;

        // cursor position
        const cursorPos = this.getCursorPos();

        // First move to the bottom of the current line, based on cursor pos
        const prevRows = this.prevRows || 0;
        if (prevRows > 0) {
          moveCursor(this.output, 0, -prevRows);
        }

        // Cursor to left edge.
        cursorTo(this.output, 0);
        // erase data
        clearScreenDown(this.output);

        // Write the prompt and the current buffer content.
        this[kWriteToOutput](line);

        // Force terminal to allocate a new line
        if (lineCols === 0) {
          this[kWriteToOutput](' ');
        }

        // Move cursor to original position.
        cursorTo(this.output, cursorPos.cols);

        const diff = lineRows - cursorPos.rows;
        if (diff > 0) {
          moveCursor(this.output, 0, -diff);
        }

        this.prevRows = cursorPos.rows;
      }

      /**
       * Closes the `readline.Interface` instance.
       * @returns {void}
       */
      close() {
        if (this.closed) return;
        this.pause();
        if (this.terminal) {
          this[kSetRawMode](false);
        }
        this.closed = true;
        this.emit('close');
      }

      /**
       * Pauses the `input` stream.
       * @returns {void | Interface}
       */
      pause() {
        if (this.paused) return;
        this.input.pause();
        this.paused = true;
        this.emit('pause');
        return this;
      }

      /**
       * Resumes the `input` stream if paused.
       * @returns {void | Interface}
       */
      resume() {
        if (!this.paused) return;
        this.input.resume();
        this.paused = false;
        this.emit('resume');
        return this;
      }

      /**
       * Writes either `data` or a `key` sequence identified by
       * `key` to the `output`.
       * @param {string} d
       * @param {{
       *   ctrl?: boolean;
       *   meta?: boolean;
       *   shift?: boolean;
       *   name?: string;
       *   }} [key]
       * @returns {void}
       */
      write(d, key) {
        if (this.paused) this.resume();
        if (this.terminal) {
          this[kTtyWrite](d, key);
        } else {
          this[kNormalWrite](d);
        }
      }

      [kNormalWrite](b) {
        if (b === undefined) {
          return;
        }
        let string = this[kDecoder].write(b);
        if (
          this[kSawReturnAt] &&
          DateNow() - this[kSawReturnAt] <= this.crlfDelay
        ) {
          if (StringPrototypeCodePointAt(string) === 10) string = StringPrototypeSlice(string, 1);
          this[kSawReturnAt] = 0;
        }

        // Run test() on the new string chunk, not on the entire line buffer.
        let newPartContainsEnding = RegExpPrototypeExec(lineEnding, string);
        if (newPartContainsEnding !== null) {
          if (this[kLine_buffer]) {
            string = this[kLine_buffer] + string;
            this[kLine_buffer] = null;
            lineEnding.lastIndex = 0; // Start the search from the beginning of the string.
            newPartContainsEnding = RegExpPrototypeExec(lineEnding, string);
          }
          this[kSawReturnAt] = StringPrototypeEndsWith(string, '\r') ?
            DateNow() :
            0;

          const indexes = [0, newPartContainsEnding.index, lineEnding.lastIndex];
          let nextMatch;
          while ((nextMatch = RegExpPrototypeExec(lineEnding, string)) !== null) {
            ArrayPrototypePush(indexes, nextMatch.index, lineEnding.lastIndex);
          }
          const lastIndex = indexes.length - 1;
          // Either '' or (conceivably) the unfinished portion of the next line
          this[kLine_buffer] = StringPrototypeSlice(string, indexes[lastIndex]);
          for (let i = 1; i < lastIndex; i += 2) {
            this[kOnLine](StringPrototypeSlice(string, indexes[i - 1], indexes[i]));
          }
        } else if (string) {
          // No newlines this time, save what we have for next time
          if (this[kLine_buffer]) {
            this[kLine_buffer] += string;
          } else {
            this[kLine_buffer] = string;
          }
        }
      }

      [kInsertString](c) {
        this[kBeforeEdit](this.line, this.cursor);
        if (this.cursor < this.line.length) {
          const beg = StringPrototypeSlice(this.line, 0, this.cursor);
          const end = StringPrototypeSlice(
            this.line,
            this.cursor,
            this.line.length,
          );
          this.line = beg + c + end;
          this.cursor += c.length;
          this[kRefreshLine]();
        } else {
          const oldPos = this.getCursorPos();
          this.line += c;
          this.cursor += c.length;
          const newPos = this.getCursorPos();

          if (oldPos.rows < newPos.rows) {
            this[kRefreshLine]();
          } else {
            this[kWriteToOutput](c);
          }
        }
      }

      async [kTabComplete](lastKeypressWasTab) {
        this.pause();
        const string = StringPrototypeSlice(this.line, 0, this.cursor);
        let value;
        try {
          value = await this.completer(string);
        } catch (err) {
          this[kWriteToOutput](`Tab completion error: ${inspect(err)}`);
          return;
        } finally {
          this.resume();
        }
        this[kTabCompleter](lastKeypressWasTab, value);
      }

      [kTabCompleter](lastKeypressWasTab, { 0: completions, 1: completeOn }) {
        // Result and the text that was completed.

        if (!completions || completions.length === 0) {
          return;
        }

        // If there is a common prefix to all matches, then apply that portion.
        const prefix = commonPrefix(
          ArrayPrototypeFilter(completions, (e) => e !== ''),
        );
        if (StringPrototypeStartsWith(prefix, completeOn) &&
            prefix.length > completeOn.length) {
          this[kInsertString](StringPrototypeSlice(prefix, completeOn.length));
          return;
        } else if (!StringPrototypeStartsWith(completeOn, prefix)) {
          this.line = StringPrototypeSlice(this.line,
                                           0,
                                           this.cursor - completeOn.length) +
                      prefix +
                      StringPrototypeSlice(this.line,
                                           this.cursor,
                                           this.line.length);
          this.cursor = this.cursor - completeOn.length + prefix.length;
          this._refreshLine();
          return;
        }

        if (!lastKeypressWasTab) {
          return;
        }

        this[kBeforeEdit](this.line, this.cursor);

        // Apply/show completions.
        const completionsWidth = ArrayPrototypeMap(completions, (e) =>
          getStringWidth(e),
        );
        const width = MathMaxApply(completionsWidth) + 2; // 2 space padding
        let maxColumns = MathFloor(this.columns / width) || 1;
        if (maxColumns === Infinity) {
          maxColumns = 1;
        }
        let output = '\r\n';
        let lineIndex = 0;
        let whitespace = 0;
        for (let i = 0; i < completions.length; i++) {
          const completion = completions[i];
          if (completion === '' || lineIndex === maxColumns) {
            output += '\r\n';
            lineIndex = 0;
            whitespace = 0;
          } else {
            output += StringPrototypeRepeat(' ', whitespace);
          }
          if (completion !== '') {
            output += completion;
            whitespace = width - completionsWidth[i];
            lineIndex++;
          } else {
            output += '\r\n';
          }
        }
        if (lineIndex !== 0) {
          output += '\r\n\r\n';
        }
        this[kWriteToOutput](output);
        this[kRefreshLine]();
      }

      [kWordLeft]() {
        if (this.cursor > 0) {
          // Reverse the string and match a word near beginning
          // to avoid quadratic time complexity
          const leading = StringPrototypeSlice(this.line, 0, this.cursor);
          const reversed = ArrayPrototypeJoin(
            ArrayPrototypeReverse(ArrayFrom(leading)),
            '',
          );
          const match = RegExpPrototypeExec(/^\s*(?:[^\w\s]+|\w+)?/, reversed);
          this[kMoveCursor](-match[0].length);
        }
      }

      [kWordRight]() {
        if (this.cursor < this.line.length) {
          const trailing = StringPrototypeSlice(this.line, this.cursor);
          const match = RegExpPrototypeExec(/^(?:\s+|[^\w\s]+|\w+)\s*/, trailing);
          this[kMoveCursor](match[0].length);
        }
      }

      [kDeleteLeft]() {
        if (this.cursor > 0 && this.line.length > 0) {
          this[kBeforeEdit](this.line, this.cursor);
          // The number of UTF-16 units comprising the character to the left
          const charSize = charLengthLeft(this.line, this.cursor);
          this.line =
            StringPrototypeSlice(this.line, 0, this.cursor - charSize) +
            StringPrototypeSlice(this.line, this.cursor, this.line.length);

          this.cursor -= charSize;
          this[kRefreshLine]();
        }
      }

      [kDeleteRight]() {
        if (this.cursor < this.line.length) {
          this[kBeforeEdit](this.line, this.cursor);
          // The number of UTF-16 units comprising the character to the left
          const charSize = charLengthAt(this.line, this.cursor);
          this.line =
            StringPrototypeSlice(this.line, 0, this.cursor) +
            StringPrototypeSlice(
              this.line,
              this.cursor + charSize,
              this.line.length,
            );
          this[kRefreshLine]();
        }
      }

      [kDeleteWordLeft]() {
        if (this.cursor > 0) {
          this[kBeforeEdit](this.line, this.cursor);
          // Reverse the string and match a word near beginning
          // to avoid quadratic time complexity
          let leading = StringPrototypeSlice(this.line, 0, this.cursor);
          const reversed = ArrayPrototypeJoin(
            ArrayPrototypeReverse(ArrayFrom(leading)),
            '',
          );
          const match = RegExpPrototypeExec(/^\s*(?:[^\w\s]+|\w+)?/, reversed);
          leading = StringPrototypeSlice(
            leading,
            0,
            leading.length - match[0].length,
          );
          this.line =
            leading +
            StringPrototypeSlice(this.line, this.cursor, this.line.length);
          this.cursor = leading.length;
          this[kRefreshLine]();
        }
      }

      [kDeleteWordRight]() {
        if (this.cursor < this.line.length) {
          this[kBeforeEdit](this.line, this.cursor);
          const trailing = StringPrototypeSlice(this.line, this.cursor);
          const match = RegExpPrototypeExec(/^(?:\s+|\W+|\w+)\s*/, trailing);
          this.line =
            StringPrototypeSlice(this.line, 0, this.cursor) +
            StringPrototypeSlice(trailing, match[0].length);
          this[kRefreshLine]();
        }
      }

      [kDeleteLineLeft]() {
        this[kBeforeEdit](this.line, this.cursor);
        const del = StringPrototypeSlice(this.line, 0, this.cursor);
        this.line = StringPrototypeSlice(this.line, this.cursor);
        this.cursor = 0;
        this[kPushToKillRing](del);
        this[kRefreshLine]();
      }

      [kDeleteLineRight]() {
        this[kBeforeEdit](this.line, this.cursor);
        const del = StringPrototypeSlice(this.line, this.cursor);
        this.line = StringPrototypeSlice(this.line, 0, this.cursor);
        this[kPushToKillRing](del);
        this[kRefreshLine]();
      }

      [kPushToKillRing](del) {
        if (!del || del === this[kKillRing][0]) return;
        ArrayPrototypeUnshift(this[kKillRing], del);
        this[kKillRingCursor] = 0;
        while (this[kKillRing].length > kMaxLengthOfKillRing)
          ArrayPrototypePop(this[kKillRing]);
      }

      [kYank]() {
        if (this[kKillRing].length > 0) {
          this[kYanking] = true;
          this[kInsertString](this[kKillRing][this[kKillRingCursor]]);
        }
      }

      [kYankPop]() {
        if (!this[kYanking]) {
          return;
        }
        if (this[kKillRing].length > 1) {
          const lastYank = this[kKillRing][this[kKillRingCursor]];
          this[kKillRingCursor]++;
          if (this[kKillRingCursor] >= this[kKillRing].length) {
            this[kKillRingCursor] = 0;
          }
          const currentYank = this[kKillRing][this[kKillRingCursor]];
          const head =
                StringPrototypeSlice(this.line, 0, this.cursor - lastYank.length);
          const tail =
                StringPrototypeSlice(this.line, this.cursor);
          this.line = head + currentYank + tail;
          this.cursor = head.length + currentYank.length;
          this[kRefreshLine]();
        }
      }

      clearLine() {
        this[kMoveCursor](+Infinity);
        this[kWriteToOutput]('\r\n');
        this.line = '';
        this.cursor = 0;
        this.prevRows = 0;
      }

      [kLine]() {
        const line = this[kAddHistory]();
        this[kUndoStack] = [];
        this[kRedoStack] = [];
        this.clearLine();
        this[kOnLine](line);
      }

      [kPushToUndoStack](text, cursor) {
        if (ArrayPrototypePush(this[kUndoStack], { text, cursor }) >
            kMaxUndoRedoStackSize) {
          ArrayPrototypeShift(this[kUndoStack]);
        }
      }

      [kUndo]() {
        if (this[kUndoStack].length <= 0) return;

        ArrayPrototypePush(
          this[kRedoStack],
          { text: this.line, cursor: this.cursor },
        );

        const entry = ArrayPrototypePop(this[kUndoStack]);
        this.line = entry.text;
        this.cursor = entry.cursor;

        this[kRefreshLine]();
      }

      [kRedo]() {
        if (this[kRedoStack].length <= 0) return;

        ArrayPrototypePush(
          this[kUndoStack],
          { text: this.line, cursor: this.cursor },
        );

        const entry = ArrayPrototypePop(this[kRedoStack]);
        this.line = entry.text;
        this.cursor = entry.cursor;

        this[kRefreshLine]();
      }

      // TODO(BridgeAR): Add underscores to the search part and a red background in
      // case no match is found. This should only be the visual part and not the
      // actual line content!
      // TODO(BridgeAR): In case the substring based search is active and the end is
      // reached, show a comment how to search the history as before. E.g., using
      // <ctrl> + N. Only show this after two/three UPs or DOWNs, not on the first
      // one.
      [kHistoryNext]() {
        if (this.historyIndex >= 0) {
          this[kBeforeEdit](this.line, this.cursor);
          const search = this[kSubstringSearch] || '';
          let index = this.historyIndex - 1;
          while (
            index >= 0 &&
            (!StringPrototypeStartsWith(this.history[index], search) ||
              this.line === this.history[index])
          ) {
            index--;
          }
          if (index === -1) {
            this.line = search;
          } else {
            this.line = this.history[index];
          }
          this.historyIndex = index;
          this.cursor = this.line.length; // Set cursor to end of line.
          this[kRefreshLine]();
        }
      }

      [kHistoryPrev]() {
        if (this.historyIndex < this.history.length && this.history.length) {
          this[kBeforeEdit](this.line, this.cursor);
          const search = this[kSubstringSearch] || '';
          let index = this.historyIndex + 1;
          while (
            index < this.history.length &&
            (!StringPrototypeStartsWith(this.history[index], search) ||
              this.line === this.history[index])
          ) {
            index++;
          }
          if (index === this.history.length) {
            this.line = search;
          } else {
            this.line = this.history[index];
          }
          this.historyIndex = index;
          this.cursor = this.line.length; // Set cursor to end of line.
          this[kRefreshLine]();
        }
      }

      // Returns the last character's display position of the given string
      [kGetDisplayPos](str) {
        let offset = 0;
        const col = this.columns;
        let rows = 0;
        str = stripVTControlCharacters(str);
        for (const char of new SafeStringIterator(str)) {
          if (char === '\n') {
            // Rows must be incremented by 1 even if offset = 0 or col = +Infinity.
            rows += MathCeil(offset / col) || 1;
            offset = 0;
            continue;
          }
          // Tabs must be aligned by an offset of the tab size.
          if (char === '\t') {
            offset += this.tabSize - (offset % this.tabSize);
            continue;
          }
          const width = getStringWidth(char, false /* stripVTControlCharacters */);
          if (width === 0 || width === 1) {
            offset += width;
          } else {
            // width === 2
            if ((offset + 1) % col === 0) {
              offset++;
            }
            offset += 2;
          }
        }
        const cols = offset % col;
        rows += (offset - cols) / col;
        return { cols, rows };
      }

      /**
       * Returns the real position of the cursor in relation
       * to the input prompt + string.
       * @returns {{
       *   rows: number;
       *   cols: number;
       *   }}
       */
      getCursorPos() {
        const strBeforeCursor =
          this[kPrompt] + StringPrototypeSlice(this.line, 0, this.cursor);
        return this[kGetDisplayPos](strBeforeCursor);
      }

      // This function moves cursor dx places to the right
      // (-dx for left) and refreshes the line if it is needed.
      [kMoveCursor](dx) {
        if (dx === 0) {
          return;
        }
        const oldPos = this.getCursorPos();
        this.cursor += dx;

        // Bounds check
        if (this.cursor < 0) {
          this.cursor = 0;
        } else if (this.cursor > this.line.length) {
          this.cursor = this.line.length;
        }

        const newPos = this.getCursorPos();

        // Check if cursor stayed on the line.
        if (oldPos.rows === newPos.rows) {
          const diffWidth = newPos.cols - oldPos.cols;
          moveCursor(this.output, diffWidth, 0);
        } else {
          this[kRefreshLine]();
        }
      }

      // Handle a write from the tty
      [kTtyWrite](s, key) {
        const previousKey = this[kPreviousKey];
        key = key || kEmptyObject;
        this[kPreviousKey] = key;

        if (!key.meta || key.name !== 'y') {
          // Reset yanking state unless we are doing yank pop.
          this[kYanking] = false;
        }

        // Activate or deactivate substring search.
        if (
          (key.name === 'up' || key.name === 'down') &&
          !key.ctrl &&
          !key.meta &&
          !key.shift
        ) {
          if (this[kSubstringSearch] === null) {
            this[kSubstringSearch] = StringPrototypeSlice(
              this.line,
              0,
              this.cursor,
            );
          }
        } else if (this[kSubstringSearch] !== null) {
          this[kSubstringSearch] = null;
          // Reset the index in case there's no match.
          if (this.history.length === this.historyIndex) {
            this.historyIndex = -1;
          }
        }

        // Undo & Redo
        if (typeof key.sequence === 'string') {
          switch (StringPrototypeCodePointAt(key.sequence, 0)) {
            case 0x1f:
              this[kUndo]();
              return;
            case 0x1e:
              this[kRedo]();
              return;
            default:
              break;
          }
        }

        // Ignore escape key, fixes
        // https://github.com/nodejs/node-v0.x-archive/issues/2876.
        if (key.name === 'escape') return;

        if (key.ctrl && key.shift) {
          /* Control and shift pressed */
          switch (key.name) {
            // TODO(BridgeAR): The transmitted escape sequence is `\b` and that is
            // identical to <ctrl>-h. It should have a unique escape sequence.
            case 'backspace':
              this[kDeleteLineLeft]();
              break;

            case 'delete':
              this[kDeleteLineRight]();
              break;
          }
        } else if (key.ctrl) {
          /* Control key pressed */

          switch (key.name) {
            case 'c':
              if (this.listenerCount('SIGINT') > 0) {
                this.emit('SIGINT');
              } else {
                // This readline instance is finished
                this.close();
              }
              break;

            case 'h': // delete left
              this[kDeleteLeft]();
              break;

            case 'd': // delete right or EOF
              if (this.cursor === 0 && this.line.length === 0) {
                // This readline instance is finished
                this.close();
              } else if (this.cursor < this.line.length) {
                this[kDeleteRight]();
              }
              break;

            case 'u': // Delete from current to start of line
              this[kDeleteLineLeft]();
              break;

            case 'k': // Delete from current to end of line
              this[kDeleteLineRight]();
              break;

            case 'a': // Go to the start of the line
              this[kMoveCursor](-Infinity);
              break;

            case 'e': // Go to the end of the line
              this[kMoveCursor](+Infinity);
              break;

            case 'b': // back one character
              this[kMoveCursor](-charLengthLeft(this.line, this.cursor));
              break;

            case 'f': // Forward one character
              this[kMoveCursor](+charLengthAt(this.line, this.cursor));
              break;

            case 'l': // Clear the whole screen
              cursorTo(this.output, 0, 0);
              clearScreenDown(this.output);
              this[kRefreshLine]();
              break;

            case 'n': // next history item
              this[kHistoryNext]();
              break;

            case 'p': // Previous history item
              this[kHistoryPrev]();
              break;

            case 'y': // Yank killed string
              this[kYank]();
              break;

            case 'z':
              if (process.platform === 'win32') break;
              if (this.listenerCount('SIGTSTP') > 0) {
                this.emit('SIGTSTP');
              } else {
                process.once('SIGCONT', () => {
                  // Don't raise events if stream has already been abandoned.
                  if (!this.paused) {
                    // Stream must be paused and resumed after SIGCONT to catch
                    // SIGINT, SIGTSTP, and EOF.
                    this.pause();
                    this.emit('SIGCONT');
                  }
                  // Explicitly re-enable "raw mode" and move the cursor to
                  // the correct position.
                  // See https://github.com/joyent/node/issues/3295.
                  this[kSetRawMode](true);
                  this[kRefreshLine]();
                });
                this[kSetRawMode](false);
                process.kill(process.pid, 'SIGTSTP');
              }
              break;

            case 'w': // Delete backwards to a word boundary
            // TODO(BridgeAR): The transmitted escape sequence is `\b` and that is
            // identical to <ctrl>-h. It should have a unique escape sequence.
            // Falls through
            case 'backspace':
              this[kDeleteWordLeft]();
              break;

            case 'delete': // Delete forward to a word boundary
              this[kDeleteWordRight]();
              break;

            case 'left':
              this[kWordLeft]();
              break;

            case 'right':
              this[kWordRight]();
              break;
          }
        } else if (key.meta) {
          /* Meta key pressed */

          switch (key.name) {
            case 'b': // backward word
              this[kWordLeft]();
              break;

            case 'f': // forward word
              this[kWordRight]();
              break;

            case 'd': // delete forward word
            case 'delete':
              this[kDeleteWordRight]();
              break;

            case 'backspace': // Delete backwards to a word boundary
              this[kDeleteWordLeft]();
              break;

            case 'y': // Doing yank pop
              this[kYankPop]();
              break;
          }
        } else {
          /* No modifier keys used */

          // \r bookkeeping is only relevant if a \n comes right after.
          if (this[kSawReturnAt] && key.name !== 'enter') this[kSawReturnAt] = 0;

          switch (key.name) {
            case 'return': // Carriage return, i.e. \r
              this[kSawReturnAt] = DateNow();
              this[kLine]();
              break;

            case 'enter':
              // When key interval > crlfDelay
              if (
                this[kSawReturnAt] === 0 ||
                DateNow() - this[kSawReturnAt] > this.crlfDelay
              ) {
                this[kLine]();
              }
              this[kSawReturnAt] = 0;
              break;

            case 'backspace':
              this[kDeleteLeft]();
              break;

            case 'delete':
              this[kDeleteRight]();
              break;

            case 'left':
              // Obtain the code point to the left
              this[kMoveCursor](-charLengthLeft(this.line, this.cursor));
              break;

            case 'right':
              this[kMoveCursor](+charLengthAt(this.line, this.cursor));
              break;

            case 'home':
              this[kMoveCursor](-Infinity);
              break;

            case 'end':
              this[kMoveCursor](+Infinity);
              break;

            case 'up':
              this[kHistoryPrev]();
              break;

            case 'down':
              this[kHistoryNext]();
              break;

            case 'tab':
              // If tab completion enabled, do that...
              if (
                typeof this.completer === 'function' &&
                this.isCompletionEnabled
              ) {
                const lastKeypressWasTab =
                  previousKey && previousKey.name === 'tab';
                this[kTabComplete](lastKeypressWasTab);
                break;
              }
            // falls through
            default:
              if (typeof s === 'string' && s) {
                // Erase state of previous searches.
                lineEnding.lastIndex = 0;
                let nextMatch;
                // Keep track of the end of the last match.
                let lastIndex = 0;
                while ((nextMatch = RegExpPrototypeExec(lineEnding, s)) !== null) {
                  this[kInsertString](StringPrototypeSlice(s, lastIndex, nextMatch.index));
                  ({ lastIndex } = lineEnding);
                  this[kLine]();
                  // Restore lastIndex as the call to kLine could have mutated it.
                  lineEnding.lastIndex = lastIndex;
                }
                // This ensures that the last line is written if it doesn't end in a newline.
                // Note that the last line may be the first line, in which case this still works.
                this[kInsertString](StringPrototypeSlice(s, lastIndex));
              }
          }
        }
      }

      /**
       * Creates an `AsyncIterator` object that iterates through
       * each line in the input stream as a string.
       * @typedef {{
       *   [Symbol.asyncIterator]: () => InterfaceAsyncIterator,
       *   next: () => Promise<string>
       * }} InterfaceAsyncIterator
       * @returns {InterfaceAsyncIterator}
       */
      [SymbolAsyncIterator]() {
        if (this[kLineObjectStream] === undefined) {
          kFirstEventParam ??= require('internal/events/symbols').kFirstEventParam;
          this[kLineObjectStream] = EventEmitter.on(
            this, 'line', {
              close: ['close'],
              highWatermark: 1024,
              [kFirstEventParam]: true,
            });
        }
        return this[kLineObjectStream];
      }
    }

    module.exports = {
      Interface,
      InterfaceConstructor,
      kAddHistory,
      kDecoder,
      kDeleteLeft,
      kDeleteLineLeft,
      kDeleteLineRight,
      kDeleteRight,
      kDeleteWordLeft,
      kDeleteWordRight,
      kGetDisplayPos,
      kHistoryNext,
      kHistoryPrev,
      kInsertString,
      kLine,
      kLine_buffer,
      kMoveCursor,
      kNormalWrite,
      kOldPrompt,
      kOnLine,
      kPreviousKey,
      kPrompt,
      kQuestion,
      kQuestionCallback,
      kQuestionCancel,
      kRefreshLine,
      kSawKeyPress,
      kSawReturnAt,
      kSetRawMode,
      kTabComplete,
      kTabCompleter,
      kTtyWrite,
      kWordLeft,
      kWordRight,
      kWriteToOutput,
    };
  };

  defs["internal/readline/emitKeypressEvents"] = function (module, exports) {
    const {
      SafeStringIterator,
      Symbol,
    } = primordials;

    const {
      charLengthAt,
      CSI,
      emitKeys,
    } = require('internal/readline/utils');
    const {
      kSawKeyPress,
    } = require('internal/readline/interface');

    const { clearTimeout, setTimeout } = require('timers');
    const {
      kEscape,
    } = CSI;

    const { StringDecoder } = require('string_decoder');

    const KEYPRESS_DECODER = Symbol('keypress-decoder');
    const ESCAPE_DECODER = Symbol('escape-decoder');

    // GNU readline library - keyseq-timeout is 500ms (default)
    const ESCAPE_CODE_TIMEOUT = 500;

    /**
     * accepts a readable Stream instance and makes it emit "keypress" events
     */

    function emitKeypressEvents(stream, iface = {}) {
      if (stream[KEYPRESS_DECODER]) return;

      stream[KEYPRESS_DECODER] = new StringDecoder('utf8');

      stream[ESCAPE_DECODER] = emitKeys(stream);
      stream[ESCAPE_DECODER].next();

      const triggerEscape = () => stream[ESCAPE_DECODER].next('');
      const { escapeCodeTimeout = ESCAPE_CODE_TIMEOUT } = iface;
      let timeoutId;

      function onData(input) {
        if (stream.listenerCount('keypress') > 0) {
          const string = stream[KEYPRESS_DECODER].write(input);
          if (string) {
            clearTimeout(timeoutId);

            // This supports characters of length 2.
            iface[kSawKeyPress] = charLengthAt(string, 0) === string.length;
            iface.isCompletionEnabled = false;

            let length = 0;
            for (const character of new SafeStringIterator(string)) {
              length += character.length;
              if (length === string.length) {
                iface.isCompletionEnabled = true;
              }

              try {
                stream[ESCAPE_DECODER].next(character);
                // Escape letter at the tail position
                if (length === string.length && character === kEscape) {
                  timeoutId = setTimeout(triggerEscape, escapeCodeTimeout);
                }
              } catch (err) {
                // If the generator throws (it could happen in the `keypress`
                // event), we need to restart it.
                stream[ESCAPE_DECODER] = emitKeys(stream);
                stream[ESCAPE_DECODER].next();
                throw err;
              }
            }
          }
        } else {
          // Nobody's watching anyway
          stream.removeListener('data', onData);
          stream.on('newListener', onNewListener);
        }
      }

      function onNewListener(event) {
        if (event === 'keypress') {
          stream.on('data', onData);
          stream.removeListener('newListener', onNewListener);
        }
      }

      if (stream.listenerCount('keypress') > 0) {
        stream.on('data', onData);
      } else {
        stream.on('newListener', onNewListener);
      }
    }

    module.exports = emitKeypressEvents;
  };

  defs["internal/readline/promises"] = function (module, exports) {
    const {
      ArrayPrototypeJoin,
      ArrayPrototypePush,
      Promise,
    } = primordials;

    const { CSI } = require('internal/readline/utils');
    const { validateBoolean, validateInteger } = require('internal/validators');
    const { isWritable } = require('internal/streams/utils');
    const { codes: { ERR_INVALID_ARG_TYPE } } = require('internal/errors');

    const {
      kClearToLineBeginning,
      kClearToLineEnd,
      kClearLine,
      kClearScreenDown,
    } = CSI;

    class Readline {
      #autoCommit = false;
      #stream;
      #todo = [];

      constructor(stream, options = undefined) {
        if (!isWritable(stream))
          throw new ERR_INVALID_ARG_TYPE('stream', 'Writable', stream);
        this.#stream = stream;
        if (options?.autoCommit != null) {
          validateBoolean(options.autoCommit, 'options.autoCommit');
          this.#autoCommit = options.autoCommit;
        }
      }

      /**
       * Moves the cursor to the x and y coordinate on the given stream.
       * @param {integer} x
       * @param {integer} [y]
       * @returns {Readline} this
       */
      cursorTo(x, y = undefined) {
        validateInteger(x, 'x');
        if (y != null) validateInteger(y, 'y');

        const data = y == null ? CSI`${x + 1}G` : CSI`${y + 1};${x + 1}H`;
        if (this.#autoCommit) process.nextTick(() => this.#stream.write(data));
        else ArrayPrototypePush(this.#todo, data);

        return this;
      }

      /**
       * Moves the cursor relative to its current location.
       * @param {integer} dx
       * @param {integer} dy
       * @returns {Readline} this
       */
      moveCursor(dx, dy) {
        if (dx || dy) {
          validateInteger(dx, 'dx');
          validateInteger(dy, 'dy');

          let data = '';

          if (dx < 0) {
            data += CSI`${-dx}D`;
          } else if (dx > 0) {
            data += CSI`${dx}C`;
          }

          if (dy < 0) {
            data += CSI`${-dy}A`;
          } else if (dy > 0) {
            data += CSI`${dy}B`;
          }
          if (this.#autoCommit) process.nextTick(() => this.#stream.write(data));
          else ArrayPrototypePush(this.#todo, data);
        }
        return this;
      }

      /**
       * Clears the current line the cursor is on.
       * @param {-1|0|1} dir Direction to clear:
       *   -1 for left of the cursor
       *   +1 for right of the cursor
       *    0 for the entire line
       * @returns {Readline} this
       */
      clearLine(dir) {
        validateInteger(dir, 'dir', -1, 1);

        const data =
          dir < 0 ? kClearToLineBeginning :
            dir > 0 ? kClearToLineEnd :
              kClearLine;
        if (this.#autoCommit) process.nextTick(() => this.#stream.write(data));
        else ArrayPrototypePush(this.#todo, data);
        return this;
      }

      /**
       * Clears the screen from the current position of the cursor down.
       * @returns {Readline} this
       */
      clearScreenDown() {
        if (this.#autoCommit) {
          process.nextTick(() => this.#stream.write(kClearScreenDown));
        } else {
          ArrayPrototypePush(this.#todo, kClearScreenDown);
        }
        return this;
      }

      /**
       * Sends all the pending actions to the associated `stream` and clears the
       * internal list of pending actions.
       * @returns {Promise<void>} Resolves when all pending actions have been
       * flushed to the associated `stream`.
       */
      commit() {
        return new Promise((resolve) => {
          this.#stream.write(ArrayPrototypeJoin(this.#todo, ''), resolve);
          this.#todo = [];
        });
      }

      /**
       * Clears the internal list of pending actions without sending it to the
       * associated `stream`.
       * @returns {Readline} this
       */
      rollback() {
        this.#todo = [];
        return this;
      }
    }

    module.exports = {
      Readline,
    };
  };

  defs["readline/promises"] = function (module, exports) {
    const {
      Promise,
      SymbolDispose,
    } = primordials;

    const {
      Readline,
    } = require('internal/readline/promises');

    const {
      Interface: _Interface,
      kQuestion,
      kQuestionCancel,
    } = require('internal/readline/interface');

    const {
      AbortError,
    } = require('internal/errors');
    const { validateAbortSignal } = require('internal/validators');

    const {
      kEmptyObject,
    } = require('internal/util');
    let addAbortListener;

    class Interface extends _Interface {
      // eslint-disable-next-line no-useless-constructor
      constructor(input, output, completer, terminal) {
        super(input, output, completer, terminal);
      }
      question(query, options = kEmptyObject) {
        return new Promise((resolve, reject) => {
          let cb = resolve;

          if (options?.signal) {
            validateAbortSignal(options.signal, 'options.signal');
            if (options.signal.aborted) {
              return reject(
                new AbortError(undefined, { cause: options.signal.reason }));
            }

            const onAbort = () => {
              this[kQuestionCancel]();
              reject(new AbortError(undefined, { cause: options.signal.reason }));
            };
            addAbortListener ??= require('events').addAbortListener;
            const disposable = addAbortListener(options.signal, onAbort);

            cb = (answer) => {
              disposable[SymbolDispose]();
              resolve(answer);
            };
          }

          this[kQuestion](query, cb);
        });
      }
    }

    function createInterface(input, output, completer, terminal) {
      return new Interface(input, output, completer, terminal);
    }

    module.exports = {
      Interface,
      Readline,
      createInterface,
    };
  };

  defs["readline"] = function (module, exports) {
    const {
      DateNow,
      FunctionPrototypeBind,
      FunctionPrototypeCall,
      ObjectDefineProperties,
      ObjectSetPrototypeOf,
      Promise,
      PromiseReject,
      StringPrototypeSlice,
      SymbolDispose,
    } = primordials;

    const {
      clearLine,
      clearScreenDown,
      cursorTo,
      moveCursor,
    } = require('internal/readline/callbacks');
    const emitKeypressEvents = require('internal/readline/emitKeypressEvents');
    const promises = require('readline/promises');

    const {
      AbortError,
    } = require('internal/errors');
    const {
      inspect,
    } = require('internal/util/inspect');
    const {
      kEmptyObject,
      promisify,
    } = require('internal/util');
    const { validateAbortSignal } = require('internal/validators');

    /**
     * @typedef {import('./stream.js').Readable} Readable
     * @typedef {import('./stream.js').Writable} Writable
     */

    const {
      Interface: _Interface,
      InterfaceConstructor,
      kAddHistory,
      kDecoder,
      kDeleteLeft,
      kDeleteLineLeft,
      kDeleteLineRight,
      kDeleteRight,
      kDeleteWordLeft,
      kDeleteWordRight,
      kGetDisplayPos,
      kHistoryNext,
      kHistoryPrev,
      kInsertString,
      kLine,
      kLine_buffer,
      kMoveCursor,
      kNormalWrite,
      kOldPrompt,
      kOnLine,
      kPreviousKey,
      kPrompt,
      kQuestion,
      kQuestionCallback,
      kQuestionCancel,
      kRefreshLine,
      kSawKeyPress,
      kSawReturnAt,
      kSetRawMode,
      kTabComplete,
      kTabCompleter,
      kTtyWrite,
      kWordLeft,
      kWordRight,
      kWriteToOutput,
    } = require('internal/readline/interface');
    let addAbortListener;

    function Interface(input, output, completer, terminal) {
      if (!(this instanceof Interface)) {
        return new Interface(input, output, completer, terminal);
      }

      if (input?.input &&
          typeof input.completer === 'function' && input.completer.length !== 2) {
        const { completer } = input;
        input.completer = (v, cb) => cb(null, completer(v));
      } else if (typeof completer === 'function' && completer.length !== 2) {
        const realCompleter = completer;
        completer = (v, cb) => cb(null, realCompleter(v));
      }

      FunctionPrototypeCall(InterfaceConstructor, this,
                            input, output, completer, terminal);

      if (process.env.TERM === 'dumb') {
        this._ttyWrite = FunctionPrototypeBind(_ttyWriteDumb, this);
      }
    }

    ObjectSetPrototypeOf(Interface.prototype, _Interface.prototype);
    ObjectSetPrototypeOf(Interface, _Interface);

    /**
     * Displays `query` by writing it to the `output`.
     * @param {string} query
     * @param {{ signal?: AbortSignal; }} [options]
     * @param {Function} cb
     * @returns {void}
     */
    Interface.prototype.question = function question(query, options, cb) {
      cb = typeof options === 'function' ? options : cb;
      if (options === null || typeof options !== 'object') {
        options = kEmptyObject;
      }

      if (options.signal) {
        validateAbortSignal(options.signal, 'options.signal');
        if (options.signal.aborted) {
          return;
        }

        const onAbort = () => {
          this[kQuestionCancel]();
        };
        addAbortListener ??= require('events').addAbortListener;
        const disposable = addAbortListener(options.signal, onAbort);
        const originalCb = cb;
        cb = typeof cb === 'function' ? (answer) => {
          disposable[SymbolDispose]();
          return originalCb(answer);
        } : disposable[SymbolDispose];
      }

      if (typeof cb === 'function') {
        this[kQuestion](query, cb);
      }
    };
    Interface.prototype.question[promisify.custom] = function question(query, options) {
      if (options === null || typeof options !== 'object') {
        options = kEmptyObject;
      }

      if (options.signal && options.signal.aborted) {
        return PromiseReject(
          new AbortError(undefined, { cause: options.signal.reason }));
      }

      return new Promise((resolve, reject) => {
        let cb = resolve;

        if (options.signal) {
          const onAbort = () => {
            reject(new AbortError(undefined, { cause: options.signal.reason }));
          };
          addAbortListener ??= require('events').addAbortListener;
          const disposable = addAbortListener(options.signal, onAbort);
          cb = (answer) => {
            disposable[SymbolDispose]();
            resolve(answer);
          };
        }

        this.question(query, options, cb);
      });
    };

    /**
     * Creates a new `readline.Interface` instance.
     * @param {Readable | {
     *   input: Readable;
     *   output: Writable;
     *   completer?: Function;
     *   terminal?: boolean;
     *   history?: string[];
     *   historySize?: number;
     *   removeHistoryDuplicates?: boolean;
     *   prompt?: string;
     *   crlfDelay?: number;
     *   escapeCodeTimeout?: number;
     *   tabSize?: number;
     *   signal?: AbortSignal;
     *   }} input
     * @param {Writable} [output]
     * @param {Function} [completer]
     * @param {boolean} [terminal]
     * @returns {Interface}
     */
    function createInterface(input, output, completer, terminal) {
      return new Interface(input, output, completer, terminal);
    }

    ObjectDefineProperties(Interface.prototype, {
      // Redirect internal prototype methods to the underscore notation for backward
      // compatibility.
      [kSetRawMode]: {
        __proto__: null,
        get() {
          return this._setRawMode;
        },
      },
      [kOnLine]: {
        __proto__: null,
        get() {
          return this._onLine;
        },
      },
      [kWriteToOutput]: {
        __proto__: null,
        get() {
          return this._writeToOutput;
        },
      },
      [kAddHistory]: {
        __proto__: null,
        get() {
          return this._addHistory;
        },
      },
      [kRefreshLine]: {
        __proto__: null,
        get() {
          return this._refreshLine;
        },
      },
      [kNormalWrite]: {
        __proto__: null,
        get() {
          return this._normalWrite;
        },
      },
      [kInsertString]: {
        __proto__: null,
        get() {
          return this._insertString;
        },
      },
      [kTabComplete]: {
        __proto__: null,
        get() {
          return this._tabComplete;
        },
      },
      [kWordLeft]: {
        __proto__: null,
        get() {
          return this._wordLeft;
        },
      },
      [kWordRight]: {
        __proto__: null,
        get() {
          return this._wordRight;
        },
      },
      [kDeleteLeft]: {
        __proto__: null,
        get() {
          return this._deleteLeft;
        },
      },
      [kDeleteRight]: {
        __proto__: null,
        get() {
          return this._deleteRight;
        },
      },
      [kDeleteWordLeft]: {
        __proto__: null,
        get() {
          return this._deleteWordLeft;
        },
      },
      [kDeleteWordRight]: {
        __proto__: null,
        get() {
          return this._deleteWordRight;
        },
      },
      [kDeleteLineLeft]: {
        __proto__: null,
        get() {
          return this._deleteLineLeft;
        },
      },
      [kDeleteLineRight]: {
        __proto__: null,
        get() {
          return this._deleteLineRight;
        },
      },
      [kLine]: {
        __proto__: null,
        get() {
          return this._line;
        },
      },
      [kHistoryNext]: {
        __proto__: null,
        get() {
          return this._historyNext;
        },
      },
      [kHistoryPrev]: {
        __proto__: null,
        get() {
          return this._historyPrev;
        },
      },
      [kGetDisplayPos]: {
        __proto__: null,
        get() {
          return this._getDisplayPos;
        },
      },
      [kMoveCursor]: {
        __proto__: null,
        get() {
          return this._moveCursor;
        },
      },
      [kTtyWrite]: {
        __proto__: null,
        get() {
          return this._ttyWrite;
        },
      },

      // Defining proxies for the internal instance properties for backward
      // compatibility.
      _decoder: {
        __proto__: null,
        get() {
          return this[kDecoder];
        },
        set(value) {
          this[kDecoder] = value;
        },
      },
      _line_buffer: {
        __proto__: null,
        get() {
          return this[kLine_buffer];
        },
        set(value) {
          this[kLine_buffer] = value;
        },
      },
      _oldPrompt: {
        __proto__: null,
        get() {
          return this[kOldPrompt];
        },
        set(value) {
          this[kOldPrompt] = value;
        },
      },
      _previousKey: {
        __proto__: null,
        get() {
          return this[kPreviousKey];
        },
        set(value) {
          this[kPreviousKey] = value;
        },
      },
      _prompt: {
        __proto__: null,
        get() {
          return this[kPrompt];
        },
        set(value) {
          this[kPrompt] = value;
        },
      },
      _questionCallback: {
        __proto__: null,
        get() {
          return this[kQuestionCallback];
        },
        set(value) {
          this[kQuestionCallback] = value;
        },
      },
      _sawKeyPress: {
        __proto__: null,
        get() {
          return this[kSawKeyPress];
        },
        set(value) {
          this[kSawKeyPress] = value;
        },
      },
      _sawReturnAt: {
        __proto__: null,
        get() {
          return this[kSawReturnAt];
        },
        set(value) {
          this[kSawReturnAt] = value;
        },
      },
    });

    // Make internal methods public for backward compatibility.
    Interface.prototype._setRawMode = _Interface.prototype[kSetRawMode];
    Interface.prototype._onLine = _Interface.prototype[kOnLine];
    Interface.prototype._writeToOutput = _Interface.prototype[kWriteToOutput];
    Interface.prototype._addHistory = _Interface.prototype[kAddHistory];
    Interface.prototype._refreshLine = _Interface.prototype[kRefreshLine];
    Interface.prototype._normalWrite = _Interface.prototype[kNormalWrite];
    Interface.prototype._insertString = _Interface.prototype[kInsertString];
    Interface.prototype._tabComplete = function(lastKeypressWasTab) {
      // Overriding parent method because `this.completer` in the legacy
      // implementation takes a callback instead of being an async function.
      this.pause();
      const string = StringPrototypeSlice(this.line, 0, this.cursor);
      this.completer(string, (err, value) => {
        this.resume();

        if (err) {
          this._writeToOutput(`Tab completion error: ${inspect(err)}`);
          return;
        }

        this[kTabCompleter](lastKeypressWasTab, value);
      });
    };
    Interface.prototype._wordLeft = _Interface.prototype[kWordLeft];
    Interface.prototype._wordRight = _Interface.prototype[kWordRight];
    Interface.prototype._deleteLeft = _Interface.prototype[kDeleteLeft];
    Interface.prototype._deleteRight = _Interface.prototype[kDeleteRight];
    Interface.prototype._deleteWordLeft = _Interface.prototype[kDeleteWordLeft];
    Interface.prototype._deleteWordRight = _Interface.prototype[kDeleteWordRight];
    Interface.prototype._deleteLineLeft = _Interface.prototype[kDeleteLineLeft];
    Interface.prototype._deleteLineRight = _Interface.prototype[kDeleteLineRight];
    Interface.prototype._line = _Interface.prototype[kLine];
    Interface.prototype._historyNext = _Interface.prototype[kHistoryNext];
    Interface.prototype._historyPrev = _Interface.prototype[kHistoryPrev];
    Interface.prototype._getDisplayPos = _Interface.prototype[kGetDisplayPos];
    Interface.prototype._getCursorPos = _Interface.prototype.getCursorPos;
    Interface.prototype._moveCursor = _Interface.prototype[kMoveCursor];
    Interface.prototype._ttyWrite = _Interface.prototype[kTtyWrite];

    function _ttyWriteDumb(s, key) {
      key = key || kEmptyObject;

      if (key.name === 'escape') return;

      if (this[kSawReturnAt] && key.name !== 'enter')
        this[kSawReturnAt] = 0;

      if (key.ctrl) {
        if (key.name === 'c') {
          if (this.listenerCount('SIGINT') > 0) {
            this.emit('SIGINT');
          } else {
            // This readline instance is finished
            this.close();
          }

          return;
        } else if (key.name === 'd') {
          this.close();
          return;
        }
      }

      switch (key.name) {
        case 'return':  // Carriage return, i.e. \r
          this[kSawReturnAt] = DateNow();
          this._line();
          break;

        case 'enter':
          // When key interval > crlfDelay
          if (this[kSawReturnAt] === 0 ||
              DateNow() - this[kSawReturnAt] > this.crlfDelay) {
            this._line();
          }
          this[kSawReturnAt] = 0;
          break;

        default:
          if (typeof s === 'string' && s) {
            this.line += s;
            this.cursor += s.length;
            this._writeToOutput(s);
          }
      }
    }

    module.exports = {
      Interface,
      clearLine,
      clearScreenDown,
      createInterface,
      cursorTo,
      emitKeypressEvents,
      moveCursor,
      promises,
    };
  };

  __internals.set("readlineRequire", require);
  __builtins.set("readline", require("readline"));
  __builtins.set("readline/promises", require("readline/promises"));
}
