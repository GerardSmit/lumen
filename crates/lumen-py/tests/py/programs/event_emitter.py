class Listener:
    __slots__ = ("fn", "once", "priority", "seq")

    def __init__(self, fn, once, priority, seq):
        self.fn = fn
        self.once = once
        self.priority = priority
        self.seq = seq


class EventEmitter:
    def __init__(self):
        self._events = {}
        self._seq = 0
        self.errors = []

    def _add(self, event, fn, once, priority):
        self._seq += 1
        lst = self._events.setdefault(event, [])
        lst.append(Listener(fn, once, priority, self._seq))
        lst.sort(key=lambda l: (-l.priority, l.seq))
        return self

    def on(self, event, fn=None, priority=0):
        if fn is None:
            def deco(f):
                self._add(event, f, False, priority)
                return f
            return deco
        return self._add(event, fn, False, priority)

    def once(self, event, fn, priority=0):
        return self._add(event, fn, True, priority)

    def off(self, event, fn=None):
        lst = self._events.get(event)
        if not lst:
            return self
        if fn is None:
            del self._events[event]
        else:
            for i, l in enumerate(lst):
                if l.fn == fn:
                    del lst[i]
                    break
            if not lst:
                del self._events[event]
        return self

    def listener_count(self, event):
        return len(self._events.get(event, ()))

    def event_names(self):
        return sorted(self._events)

    def emit(self, event, *args, **kwargs):
        lst = list(self._events.get(event, ()))
        if not lst:
            if event == "error" and args:
                raise args[0]
            return 0
        called = 0
        for l in lst:
            if l.once:
                self._remove_exact(event, l)
            try:
                l.fn(*args, **kwargs)
            except Exception as exc:
                self.errors.append((event, type(exc).__name__, str(exc)))
                if event != "error":
                    self.emit("error", exc)
            called += 1
        return called

    def _remove_exact(self, event, listener):
        lst = self._events.get(event)
        if lst and listener in lst:
            lst.remove(listener)
            if not lst:
                del self._events[event]


log = []


def make_logger(tag):
    def handler(*args, **kwargs):
        log.append("%s:%s%s" % (tag, args, sorted(kwargs.items()) if kwargs else ""))
    handler.__name__ = "logger_" + tag
    return handler


def demo_basic():
    em = EventEmitter()
    a, b, c = make_logger("a"), make_logger("b"), make_logger("c")
    em.on("x", a).on("x", b).once("x", c)
    print(em.listener_count("x"), em.event_names())
    print(em.emit("x", 1, key="v"))
    print(em.emit("x", 2))
    print(log)
    del log[:]
    em.off("x", a)
    print(em.emit("x", 3), log)
    em.off("x")
    print(em.emit("x"), em.event_names())


def demo_priority():
    em = EventEmitter()
    order = []
    em.on("p", lambda: order.append("low"), priority=-5)
    em.on("p", lambda: order.append("default1"))
    em.on("p", lambda: order.append("high"), priority=10)
    em.on("p", lambda: order.append("default2"))
    em.once("p", lambda: order.append("once-high"), priority=10)
    em.emit("p")
    em.emit("p")
    print(order)


def demo_errors():
    em = EventEmitter()
    seen = []
    em.on("error", lambda exc: seen.append("handled " + type(exc).__name__))

    def bad(n):
        if n > 1:
            raise ValueError("too big: %d" % n)
        seen.append("ok %d" % n)

    em.on("job", bad)
    em.on("job", lambda n: seen.append("after %d" % n))
    em.emit("job", 1)
    em.emit("job", 2)
    print(seen)
    print(em.errors)
    em2 = EventEmitter()
    try:
        em2.emit("error", RuntimeError("unhandled"))
    except RuntimeError as e:
        print("raised", e)


def demo_decorator_and_reentrancy():
    em = EventEmitter()
    results = []

    @em.on("greet")
    def hello(name):
        results.append("hello " + name)

    @em.on("greet", priority=1)
    def first(name):
        results.append("first " + name)
        em.off("greet", hello)

    em.emit("greet", "ann")
    em.emit("greet", "bob")
    print(results, hello.__name__)

    counter = {"n": 0}

    def recurse():
        counter["n"] += 1
        if counter["n"] < 4:
            em.emit("rec")

    em.on("rec", recurse)
    em.emit("rec")
    print(counter)

    chain = EventEmitter().on("a", lambda: None).once("b", lambda: None).on("c", lambda: None)
    print(chain.event_names(), chain.emit("b"), chain.event_names())


demo_basic()
demo_priority()
demo_errors()
demo_decorator_and_reentrancy()
