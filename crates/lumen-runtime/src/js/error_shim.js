globalThis.onerror = null;
globalThis.onunhandledrejection = null;
// Node's process-level hooks come first: a registered 'uncaughtException' /
// 'unhandledRejection' listener owns the error, exactly as it does in Node.
const processHas = (event) => {
    const p = globalThis.process;
    return !!(p && typeof p.listenerCount === 'function' && p.listenerCount(event) > 0);
};
const callOnerror = (error) => {
    const h = globalThis.onerror;
    if (typeof h !== 'function') return false;
    let message = '';
    try {
        message =
            error instanceof Error
                ? `Uncaught ${error.name}: ${error.message}`
                : `Uncaught ${String(error)}`;
    } catch {}
    try {
        return h.call(globalThis, message, '', 0, 0, error) === true;
    } catch {
        return false;
    }
};
const callOnunhandledrejection = (promise, reason) => {
    const h = globalThis.onunhandledrejection;
    if (typeof h !== 'function') return false;
    let prevented = false;
    const event = {
        type: 'unhandledrejection',
        promise,
        reason,
        cancelable: true,
        preventDefault() { prevented = true; },
        get defaultPrevented() { return prevented; },
    };
    try {
        h.call(globalThis, event);
    } catch {}
    return prevented;
};
const fireError = globalThis.__lumen_fire_error = function (error, origin = 'uncaughtException') {
    // Node's process owns fatal errors through `process._fatalException` (exit code 6 when a
    // program replaced it with a non-function; a throw out of it propagates, which is fatal).
    const p = globalThis.process;
    if (p && p[Symbol.for('lumen.nodeFatalException')] === true) {
        const fatal = p._fatalException;
        if (typeof fatal !== 'function') return 6;
        if (!processHas('uncaughtException') && !p.hasUncaughtExceptionCaptureCallback() && callOnerror(error)) return true;
        if (fatal.call(p, error, origin === 'unhandledRejection')) return true;
        const execArgv = p.execArgv;
        if (Array.isArray(execArgv) && execArgv.includes('--abort-on-uncaught-exception')) {
            try {
                console.error(error instanceof Error ? error.stack : error);
            } catch {}
            p.abort();
        }
        return false;
    }
    // node:domain: an error thrown inside a domain goes to its 'error' handler.
    const toDomain = globalThis.__lumen_domain_uncaught;
    if (typeof toDomain === 'function') {
        try {
            if (toDomain(error)) return true;
        } catch {
            return false;
        }
    }
    if (processHas('uncaughtException')) {
        try {
            globalThis.process.emit('uncaughtException', error, origin);
            return true;
        } catch {
            return false;
        }
    }
    const execArgv = globalThis.process && globalThis.process.execArgv;
    if (Array.isArray(execArgv) && execArgv.includes('--abort-on-uncaught-exception')) {
        try {
            console.error(error instanceof Error ? error.stack : error);
        } catch {}
        globalThis.process.abort();
    }
    return callOnerror(error);
};
// Node's --unhandled-rejections modes (lib/internal/process/promises.js). The fire helper
// returns true when the rejection is dealt with, or `[error]` to raise `error` as an uncaught
// exception (origin 'unhandledRejection').
const nodeRejections = {
    mode: undefined,
    uid: 0,
    ids: new WeakMap(),
};
const rejectionMode = () => {
    if (nodeRejections.mode === undefined) {
        const options = globalThis.process && globalThis.process[Symbol.for('lumen.options')];
        nodeRejections.mode = (options && options['--unhandled-rejections']) || 'throw';
    }
    return nodeRejections.mode;
};
// V8 errors carry an own `stack`; lumen's is lazy, so a native error counts too.
const isErrorLike = (o) =>
    typeof o === 'object' &&
    o !== null &&
    (Object.prototype.hasOwnProperty.call(o, 'stack') || Object.prototype.toString.call(o) === '[object Error]');
const noSideEffectsToString = (v) => {
    if (typeof v === 'symbol') return v.description === undefined ? 'Symbol()' : `Symbol(${v.description})`;
    if ((typeof v !== 'object' && typeof v !== 'function') || v === null) return String(v);
    try {
        const ctor = Object.getPrototypeOf(v)?.constructor;
        return `#<${(typeof ctor === 'function' && ctor.name) || 'Object'}>`;
    } catch {
        return '#<Object>';
    }
};
const errorWithoutStack = (name, message) => {
    const limit = Error.stackTraceLimit;
    Error.stackTraceLimit = 0;
    const err = new Error(message);
    Error.stackTraceLimit = limit;
    Object.defineProperty(err, 'name', { value: name, enumerable: false, writable: true, configurable: true });
    return err;
};
const unhandledRejectionError = (reason) => {
    if (isErrorLike(reason)) return reason;
    const err = errorWithoutStack(
        'UnhandledPromiseRejection',
        'This error originated either by throwing inside of an async function without a catch ' +
            'block, or by rejecting a promise which was not handled with .catch(). The promise ' +
            `rejected with the reason "${noSideEffectsToString(reason)}".`,
    );
    err.code = 'ERR_UNHANDLED_REJECTION';
    return err;
};
const warnUnhandledRejection = (uid, reason) => {
    const type = 'UnhandledPromiseRejectionWarning';
    const warning = errorWithoutStack(
        type,
        'Unhandled promise rejection. This error originated either by throwing inside of an ' +
            'async function without a catch block, or by rejecting a promise which was not ' +
            'handled with .catch(). To terminate the node process on unhandled promise ' +
            'rejection, use the CLI flag `--unhandled-rejections=strict` (see ' +
            'https://nodejs.org/api/cli.html#cli_unhandled_rejections_mode). ' +
            `(rejection id: ${uid})`,
    );
    const p = globalThis.process;
    try {
        if (isErrorLike(reason)) {
            warning.stack = reason.stack;
            p.emitWarning(reason.stack, type);
        } else {
            p.emitWarning(noSideEffectsToString(reason), type);
        }
    } catch {
        try {
            p.emitWarning(noSideEffectsToString(reason), type);
        } catch {}
    }
    p.emitWarning(warning);
};
const nodeUnhandledRejection = (promise, reason) => {
    const p = globalThis.process;
    const uid = ++nodeRejections.uid;
    nodeRejections.ids.set(promise, uid);
    const emit = () => p.emit('unhandledRejection', reason, promise);
    switch (rejectionMode()) {
        case 'strict': {
            const err = unhandledRejectionError(reason);
            if (!fireError(err, 'unhandledRejection')) return [err];
            if (!emit()) warnUnhandledRejection(uid, reason);
            return true;
        }
        case 'none':
            emit();
            return true;
        case 'warn':
            emit();
            warnUnhandledRejection(uid, reason);
            return true;
        case 'warn-with-error-code':
            if (!emit()) {
                warnUnhandledRejection(uid, reason);
                p.exitCode = 1;
            }
            return true;
        default:
            return emit() || [unhandledRejectionError(reason)];
    }
};
globalThis.__lumen_fire_handled = function (promise) {
    const p = globalThis.process;
    const uid = nodeRejections.ids.get(promise);
    if (uid === undefined || !p || typeof p.emit !== 'function') return;
    nodeRejections.ids.delete(promise);
    const warning = new Error(`Promise rejection was handled asynchronously (rejection id: ${uid})`);
    warning.name = 'PromiseRejectionHandledWarning';
    warning.id = uid;
    if (!p.emit('rejectionHandled', promise)) p.emitWarning(warning);
};
globalThis.__lumen_fire_rejection = function (promise, reason) {
    const p = globalThis.process;
    if (p && typeof p.emit === 'function' && typeof p.emitWarning === 'function') {
        if (!processHas('unhandledRejection') && callOnunhandledrejection(promise, reason)) return true;
        return nodeUnhandledRejection(promise, reason);
    }
    if (processHas('unhandledRejection')) {
        try {
            globalThis.process.emit('unhandledRejection', reason, promise);
            return true;
        } catch {
            return false;
        }
    }
    return callOnunhandledrejection(promise, reason);
};
{
    const fire = globalThis.__lumen_fire_error;
    const report = console.__reportUncaught;
    delete console.__reportUncaught;
    globalThis.reportError = function reportError(e) {
        if (arguments.length === 0)
            throw new TypeError('reportError requires at least 1 argument');
        if (!fire(e)) report(e);
    };
}
