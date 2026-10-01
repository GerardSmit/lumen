// node:diagnostics_channel — Node's channel registry (lib/diagnostics_channel.js): channels are
// interned by name behind weak references, flip between an inert and an active prototype as
// subscribers come and go, and a TracingChannel layers the start/end/asyncStart/asyncEnd/error
// protocol on top (with bindStore/runStores propagating AsyncLocalStorage contexts).

const { ERR_INVALID_ARG_TYPE } = __errors;
const { validateFunction } = __validators;

const triggerUncaughtException = (err) => process.nextTick(() => { throw err; });

class WeakReference {
  #weak;
  #strong = null;
  #refCount = 0;
  constructor(object) {
    this.#weak = new WeakRef(object);
  }
  incRef() {
    this.#refCount++;
    if (this.#refCount === 1) {
      const derefed = this.#weak.deref();
      if (derefed !== undefined) this.#strong = derefed;
    }
    return this.#refCount;
  }
  decRef() {
    this.#refCount--;
    if (this.#refCount === 0) this.#strong = null;
    return this.#refCount;
  }
  get() {
    return this.#weak.deref();
  }
}

// A channel is only deleted once GC has collected it: the count can rise again until then.
class WeakRefMap extends Map {
  #finalizers = new FinalizationRegistry((key) => {
    this.delete(key);
  });
  set(key, value) {
    this.#finalizers.register(value, key);
    return super.set(key, new WeakReference(value));
  }
  get(key) {
    return super.get(key)?.get();
  }
  incRef(key) {
    return super.get(key)?.incRef();
  }
  decRef(key) {
    return super.get(key)?.decRef();
  }
}

function markActive(channel) {
  Object.setPrototypeOf(channel, ActiveChannel.prototype);
  channel._subscribers = [];
  channel._stores = new Map();
}

function maybeMarkInactive(channel) {
  if (!channel._subscribers.length && !channel._stores.size) {
    Object.setPrototypeOf(channel, Channel.prototype);
    channel._subscribers = undefined;
    channel._stores = undefined;
  }
}

function defaultTransform(data) {
  return data;
}

function wrapStoreRun(store, data, next, transform = defaultTransform) {
  return () => {
    let context;
    try {
      context = transform(data);
    } catch (err) {
      triggerUncaughtException(err);
      return next();
    }
    return store.run(context, next);
  };
}

class ActiveChannel {
  subscribe(subscription) {
    validateFunction(subscription, "subscription");
    this._subscribers.push(subscription);
    channels.incRef(this.name);
  }

  unsubscribe(subscription) {
    const index = this._subscribers.indexOf(subscription);
    if (index === -1) return false;
    this._subscribers.splice(index, 1);
    channels.decRef(this.name);
    maybeMarkInactive(this);
    return true;
  }

  bindStore(store, transform) {
    const replacing = this._stores.has(store);
    if (!replacing) channels.incRef(this.name);
    this._stores.set(store, transform);
  }

  unbindStore(store) {
    if (!this._stores.has(store)) return false;
    this._stores.delete(store);
    channels.decRef(this.name);
    maybeMarkInactive(this);
    return true;
  }

  get hasSubscribers() {
    return true;
  }

  publish(data) {
    for (let i = 0; i < (this._subscribers?.length || 0); i++) {
      try {
        const onMessage = this._subscribers[i];
        onMessage(data, this.name);
      } catch (err) {
        triggerUncaughtException(err);
      }
    }
  }

  runStores(data, fn, thisArg, ...args) {
    let run = () => {
      this.publish(data);
      return Reflect.apply(fn, thisArg, args);
    };
    for (const entry of this._stores.entries()) {
      run = wrapStoreRun(entry[0], data, run, entry[1]);
    }
    return run();
  }
}

class Channel {
  constructor(name) {
    this._subscribers = undefined;
    this._stores = undefined;
    this.name = name;
    channels.set(name, this);
  }

  static [Symbol.hasInstance](instance) {
    const prototype = Object.getPrototypeOf(instance);
    return prototype === Channel.prototype || prototype === ActiveChannel.prototype;
  }

  subscribe(subscription) {
    markActive(this);
    this.subscribe(subscription);
  }

  unsubscribe() {
    return false;
  }

  bindStore(store, transform) {
    markActive(this);
    this.bindStore(store, transform);
  }

  unbindStore() {
    return false;
  }

  get hasSubscribers() {
    return false;
  }

  publish() {}

  runStores(data, fn, thisArg, ...args) {
    return Reflect.apply(fn, thisArg, args);
  }
}

const channels = new WeakRefMap();

function channel(name) {
  const existing = channels.get(name);
  if (existing) return existing;
  if (typeof name !== "string" && typeof name !== "symbol") {
    throw new ERR_INVALID_ARG_TYPE("channel", ["string", "symbol"], name);
  }
  return new Channel(name);
}

function subscribe(name, subscription) {
  return channel(name).subscribe(subscription);
}

function unsubscribe(name, subscription) {
  return channel(name).unsubscribe(subscription);
}

function hasSubscribers(name) {
  const existing = channels.get(name);
  if (!existing) return false;
  return existing.hasSubscribers;
}

const traceEvents = ["start", "end", "asyncStart", "asyncEnd", "error"];

function assertChannel(value, name) {
  if (!(value instanceof Channel)) {
    throw new ERR_INVALID_ARG_TYPE(name, ["Channel"], value);
  }
}

class TracingChannel {
  constructor(nameOrChannels) {
    if (typeof nameOrChannels === "string") {
      this.start = channel(`tracing:${nameOrChannels}:start`);
      this.end = channel(`tracing:${nameOrChannels}:end`);
      this.asyncStart = channel(`tracing:${nameOrChannels}:asyncStart`);
      this.asyncEnd = channel(`tracing:${nameOrChannels}:asyncEnd`);
      this.error = channel(`tracing:${nameOrChannels}:error`);
    } else if (typeof nameOrChannels === "object") {
      const { start, end, asyncStart, asyncEnd, error } = nameOrChannels;
      assertChannel(start, "nameOrChannels.start");
      assertChannel(end, "nameOrChannels.end");
      assertChannel(asyncStart, "nameOrChannels.asyncStart");
      assertChannel(asyncEnd, "nameOrChannels.asyncEnd");
      assertChannel(error, "nameOrChannels.error");
      this.start = start;
      this.end = end;
      this.asyncStart = asyncStart;
      this.asyncEnd = asyncEnd;
      this.error = error;
    } else {
      throw new ERR_INVALID_ARG_TYPE("nameOrChannels", ["string", "object", "Channel"], nameOrChannels);
    }
  }

  subscribe(handlers) {
    for (const name of traceEvents) {
      if (!handlers[name]) continue;
      this[name]?.subscribe(handlers[name]);
    }
  }

  unsubscribe(handlers) {
    let done = true;
    for (const name of traceEvents) {
      if (!handlers[name]) continue;
      if (!this[name]?.unsubscribe(handlers[name])) done = false;
    }
    return done;
  }

  traceSync(fn, context = {}, thisArg, ...args) {
    const { start, end, error } = this;
    return start.runStores(context, () => {
      try {
        const result = Reflect.apply(fn, thisArg, args);
        context.result = result;
        return result;
      } catch (err) {
        context.error = err;
        error.publish(context);
        throw err;
      } finally {
        end.publish(context);
      }
    });
  }

  tracePromise(fn, context = {}, thisArg, ...args) {
    const { start, end, asyncStart, asyncEnd, error } = this;

    function reject(err) {
      context.error = err;
      error.publish(context);
      asyncStart.publish(context);
      asyncEnd.publish(context);
      return Promise.reject(err);
    }

    function resolve(result) {
      context.result = result;
      asyncStart.publish(context);
      asyncEnd.publish(context);
      return result;
    }

    return start.runStores(context, () => {
      try {
        let promise = Reflect.apply(fn, thisArg, args);
        if (!(promise instanceof Promise)) {
          promise = Promise.resolve(promise);
        }
        return promise.then(resolve, reject);
      } catch (err) {
        context.error = err;
        error.publish(context);
        throw err;
      } finally {
        end.publish(context);
      }
    });
  }

  traceCallback(fn, position = -1, context = {}, thisArg, ...args) {
    const { start, end, asyncStart, asyncEnd, error } = this;

    function wrappedCallback(err, res) {
      if (err) {
        context.error = err;
        error.publish(context);
      } else {
        context.result = res;
      }
      asyncStart.runStores(context, () => {
        try {
          return Reflect.apply(callback, this, arguments);
        } finally {
          asyncEnd.publish(context);
        }
      });
    }

    const callback = args.at(position);
    validateFunction(callback, "callback");
    args.splice(position, 1, wrappedCallback);

    return start.runStores(context, () => {
      try {
        return Reflect.apply(fn, thisArg, args);
      } catch (err) {
        context.error = err;
        error.publish(context);
        throw err;
      } finally {
        end.publish(context);
      }
    });
  }
}

function tracingChannel(nameOrChannels) {
  return new TracingChannel(nameOrChannels);
}

__builtins.set("diagnostics_channel", {
  channel,
  hasSubscribers,
  subscribe,
  tracingChannel,
  unsubscribe,
  Channel,
});
