// node:domain — Node's legacy error-routing module over lumen's async context. Node tracks the
// active domain with async_hooks; here the stack of entered domains lives in the engine's async
// context frame itself, so every callback deferred from inside a domain (timers, nextTick,
// immediates, promise reactions, emitters bound by `domain`) runs with that stack restored.

const EventEmitter = __builtins.get("events");
const { ERR_DOMAIN_CALLBACK_NOT_AVAILABLE, ERR_UNHANDLED_ERROR } = __errors;

if (process.hasUncaughtExceptionCaptureCallback()) {
  throw new ERR_DOMAIN_CALLBACK_NOT_AVAILABLE();
}

EventEmitter.usingDomains = true;
EventEmitter[Symbol.for("lumen.domainRequireStack")] = new Error("require(`domain`) at this point").stack;

const kStack = Symbol("domainStack");
const kNoDomains = Object.freeze([]);
const exports_ = {};
// `process.domain` is null until a domain has been exited, then undefined (as Node leaves it).
let emptyValue = null;

function currentStack() {
  const context = __asyncContextGet();
  return (context !== undefined && context.get(kStack)) || kNoDomains;
}
function setStack(stack) {
  const next = new Map(__asyncContextGet());
  if (stack.length === 0) next.delete(kStack);
  else next.set(kStack, Object.freeze(stack));
  __asyncContextSet(next.size === 0 ? undefined : next);
}
function activeDomain() {
  const stack = currentStack();
  return stack.length === 0 ? emptyValue : stack[stack.length - 1];
}

Object.defineProperty(exports_, "_stack", {
  get() { return [...currentStack()]; },
  set(value) { setStack([...value]); },
  enumerable: true, configurable: true,
});
Object.defineProperty(exports_, "active", {
  get: activeDomain,
  set(value) { emptyValue = value; },
  enumerable: true, configurable: true,
});
Object.defineProperty(process, "domain", {
  get: activeDomain,
  set(value) { emptyValue = value; },
  enumerable: true, configurable: true,
});

const eventInit = EventEmitter.init;
EventEmitter.init = function init(opts) {
  Object.defineProperty(this, "domain", {
    __proto__: null, configurable: true, enumerable: false, value: null, writable: true,
  });
  const active = activeDomain();
  if (active && !(this instanceof Domain)) {
    this.domain = active;
  }
  return Reflect.apply(eventInit, this, [opts]);
};

const eventEmit = EventEmitter.prototype.emit;
EventEmitter.prototype.emit = function emit(...args) {
  const domain = this.domain;
  const type = args[0];
  const shouldEmitError = type === "error" && this.listenerCount(type) > 0;
  if (shouldEmitError || domain === null || domain === undefined || this === process) {
    return Reflect.apply(eventEmit, this, args);
  }
  if (type === "error") {
    const er = args.length > 1 && args[1] ? args[1] : new ERR_UNHANDLED_ERROR();
    if (typeof er === "object") {
      er.domainEmitter = this;
      Object.defineProperty(er, "domain", {
        __proto__: null, configurable: true, enumerable: false, value: domain, writable: true,
      });
      er.domainThrown = false;
    }
    domain.emit("error", er);
    return false;
  }
  domain.enter();
  const ret = Reflect.apply(eventEmit, this, args);
  domain.exit();
  return ret;
};

function updateExceptionCapture() {}

function domainUncaught(err) {
  const thrown = __takeThrownContext(err);
  if (thrown !== undefined) __asyncContextSet(thrown.context);
  const stack = currentStack();
  if (stack.length === 0) return false;
  const domain = stack[stack.length - 1];
  try {
    return Boolean(domain._errorHandler(err));
  } catch (thrown) {
    process.stderr.write(`Uncaught ${thrown?.stack ?? String(thrown)}\n`);
    process.exitCode = 7;
    process.reallyExit(7);
  }
}

const previousUncaught = globalThis.__lumen_domain_uncaught;
Reflect.defineProperty(globalThis, "__lumen_domain_uncaught", {
  value: (err) => {
    if (domainUncaught(err)) return true;
    return previousUncaught(err);
  },
  configurable: true, writable: true, enumerable: false,
});

function domainUncaughtExceptionClear() {
  setStack([]);
  emptyValue = null;
}

class Domain extends EventEmitter {
  constructor() {
    super();
    this.members = [];
  }

  enter() {
    setStack([...currentStack(), this]);
  }

  exit() {
    const stack = currentStack();
    const index = stack.lastIndexOf(this);
    if (index === -1) return;
    setStack(stack.slice(0, index));
    emptyValue = undefined;
  }

  add(ee) {
    if (ee.domain === this) return;
    if (ee.domain) ee.domain.remove(ee);
    if (this.domain && ee instanceof Domain) {
      for (let d = this.domain; d; d = d.domain) {
        if (ee === d) return;
      }
    }
    Object.defineProperty(ee, "domain", {
      __proto__: null, configurable: true, enumerable: false, value: this, writable: true,
    });
    this.members.push(ee);
  }

  remove(ee) {
    ee.domain = null;
    const index = this.members.indexOf(ee);
    if (index !== -1) this.members.splice(index, 1);
  }

  run(fn, ...args) {
    this.enter();
    const ret = Reflect.apply(fn, this, args);
    this.exit();
    return ret;
  }

  bind(cb) {
    const self = this;
    function runBound(...args) {
      self.enter();
      const ret = Reflect.apply(cb, this, args);
      self.exit();
      return ret;
    }
    Object.defineProperty(runBound, "domain", {
      __proto__: null, configurable: true, enumerable: false, value: this, writable: true,
    });
    return runBound;
  }

  intercept(cb) {
    const self = this;
    function runIntercepted(...fnargs) {
      if (fnargs[0] && fnargs[0] instanceof Error) {
        const er = fnargs[0];
        er.domainBound = cb;
        er.domainThrown = false;
        Object.defineProperty(er, "domain", {
          __proto__: null, configurable: true, enumerable: false, value: self, writable: true,
        });
        self.emit("error", er);
        return undefined;
      }
      self.enter();
      const ret = Reflect.apply(cb, this, fnargs.slice(1));
      self.exit();
      return ret;
    }
    return runIntercepted;
  }

  _errorHandler(er) {
    let caught = false;
    if ((typeof er === "object" && er !== null) || typeof er === "function") {
      Object.defineProperty(er, "domain", {
        __proto__: null, configurable: true, enumerable: false, value: this, writable: true,
      });
      er.domainThrown = true;
    }
    // The handler must not run inside the domain it handles, nor re-enter itself.
    while (activeDomain() === this) this.exit();
    if (currentStack().length === 0) {
      if (this.listenerCount("error") > 0) {
        try {
          caught = this.emit("error", er);
        } finally {
          updateExceptionCapture();
        }
      }
    } else {
      try {
        caught = this.emit("error", er);
      } catch (er2) {
        if (this === activeDomain()) this.exit();
        if (currentStack().length) {
          caught = process._fatalException(er2);
        } else {
          caught = false;
        }
        return caught;
      }
    }
    domainUncaughtExceptionClear();
    return caught;
  }

}

exports_.Domain = Domain;
exports_.create = exports_.createDomain = function create() {
  return new Domain();
};

__builtins.set("domain", exports_);
