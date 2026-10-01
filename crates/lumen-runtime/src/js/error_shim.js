globalThis.onerror = null;
globalThis.onunhandledrejection = null;
// Node's process-level hooks come first: a registered 'uncaughtException' /
// 'unhandledRejection' listener owns the error, exactly as it does in Node.
const processHas = (event) => {
    const p = globalThis.process;
    return !!(p && typeof p.listenerCount === 'function' && p.listenerCount(event) > 0);
};
globalThis.__lumen_fire_error = function (error, origin = 'uncaughtException') {
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
globalThis.__lumen_fire_rejection = function (promise, reason) {
    if (processHas('unhandledRejection')) {
        try {
            globalThis.process.emit('unhandledRejection', reason, promise);
            return true;
        } catch {
            return false;
        }
    }
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
