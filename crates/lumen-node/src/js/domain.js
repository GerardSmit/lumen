// node:domain — the legacy error-routing shim. Domains are deprecated in Node but the module still
// ships; this is a minimal real implementation: run/bind/intercept execute callbacks with the
// domain active and funnel synchronous throws to the domain's 'error' handler. lumen has no
// async-context propagation, so a domain only spans the synchronous extent of run()/bind() calls.

const EventEmitter = __builtins.get("events");

// The domain stack: the top of stack is the currently-active domain.
const stack = [];
// error -> the domain stack when it was thrown (see `_mark`).
const thrownIn = new WeakMap();
const domainModule = {};

class Domain extends EventEmitter {
  constructor() {
    super();
    this.members = [];
  }

  enter() {
    stack.push(this);
    domainModule.active = this;
    process.domain = this;
  }

  exit() {
    const idx = stack.lastIndexOf(this);
    if (idx !== -1) stack.splice(idx, 1);
    const top = stack[stack.length - 1];
    domainModule.active = top;
    process.domain = top ?? null;
  }

  // As in Node, a throw inside run()/bind() is not caught here: it unwinds like any exception
  // (a surrounding try/catch sees it), and once it reaches the top as uncaught, the domain it
  // was thrown in gets it as 'error' (`__lumen_fire_error` → `domainModule._handleUncaught`).
  run(fn, ...args) {
    this.enter();
    try {
      return Reflect.apply(fn, this, args);
    } catch (err) {
      this._mark(err);
      throw err;
    } finally {
      this.exit();
    }
  }

  bind(fn) {
    const self = this;
    return function bound(...args) {
      self.enter();
      try {
        return Reflect.apply(fn, this, args);
      } catch (err) {
        self._mark(err);
        throw err;
      } finally {
        self.exit();
      }
    };
  }

  // Wrap a Node-style callback: an error first-arg is routed to the domain, otherwise the callback
  // runs bound to the domain.
  intercept(fn) {
    const self = this;
    return function intercepted(err, ...args) {
      if (err) return self._handle(err);
      self.enter();
      try {
        return Reflect.apply(fn, this, args);
      } catch (e) {
        self._handle(e);
      } finally {
        self.exit();
      }
    };
  }

  add(emitter) {
    if (emitter.domain === this) return;
    if (emitter.domain) emitter.domain.remove(emitter);
    emitter.domain = this;
    this.members.push(emitter);
  }

  remove(emitter) {
    emitter.domain = null;
    const idx = this.members.indexOf(emitter);
    if (idx !== -1) this.members.splice(idx, 1);
  }

  // Tag an error with the innermost domain it was thrown in, and remember the domains that were
  // active then (outermost first): a throw from that domain's 'error' handler goes to the next
  // one out, as Node's domain stack does.
  _mark(err) {
    try {
      if (err !== null && typeof err === "object" && !err.domainThrown) {
        err.domain = this;
        err.domainThrown = true;
        thrownIn.set(err, stack.slice());
      }
    } catch {
      /* frozen or exotic error: leave it */
    }
  }

  _handle(err) {
    try {
      err.domain = this;
      err.domainThrown = true;
    } catch {
      /* err may be a primitive; ignore */
    }
    if (this.listenerCount("error") === 0) {
      // No handler: re-throw so the error is not silently swallowed.
      throw err;
    }
    this.emit("error", err);
  }
}

function create() {
  return new Domain();
}

domainModule.Domain = Domain;
domainModule.create = create;
domainModule.createDomain = create;
domainModule.active = null;
domainModule._stack = stack;
// An uncaught error thrown inside a domain goes to that domain's 'error' listeners; true when
// one took it.
domainModule._handleUncaught = (err) => {
  const domain = err !== null && typeof err === "object" && err.domainThrown ? err.domain : null;
  if (!(domain instanceof Domain)) return false;
  const chain = thrownIn.get(err) ?? [domain];
  thrownIn.delete(err);
  while (chain.length !== 0) {
    const current = chain.pop();
    if (current.listenerCount("error") === 0) return false;
    try {
      current.emit("error", err);
      return true;
    } catch (thrown) {
      // The handler threw: the next domain out gets that error.
      err = thrown;
      try {
        if (err !== null && typeof err === "object") {
          err.domain = chain[chain.length - 1];
          err.domainThrown = true;
        }
      } catch {
        /* leave it */
      }
    }
  }
  throw err;
};
// The runtime's uncaught-error path (lumen-runtime `__lumen_fire_error`) asks this first.
Object.defineProperty(globalThis, "__lumen_domain_uncaught", {
  value: domainModule._handleUncaught,
  configurable: true,
  writable: true,
  enumerable: false,
});

__builtins.set("domain", domainModule);
