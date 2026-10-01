class EventEmitter:
    def __init__(self):
        self._handlers = {}
        self.errors = []

    def on(self, event, fn=None, *, priority=0):
        def register(f):
            self._handlers.setdefault(event, []).append((priority, len(self._handlers.get(event, [])), f, False))
            self._handlers[event].sort(key=lambda t: (-t[0], t[1]))
            return f
        return register(fn) if fn is not None else register

    def once(self, event, fn):
        self._handlers.setdefault(event, []).append((0, 10**6 + len(self._handlers.get(event, [])), fn, True))
        self._handlers[event].sort(key=lambda t: (-t[0], t[1]))
        return fn

    def off(self, event, fn=None):
        if fn is None:
            return len(self._handlers.pop(event, []))
        before = len(self._handlers.get(event, []))
        self._handlers[event] = [h for h in self._handlers.get(event, []) if h[2] is not fn]
        return before - len(self._handlers[event])

    def emit(self, event, *args, **kwargs):
        handlers = list(self._handlers.get(event, []))
        results = []
        for h in handlers:
            _, _, fn, once = h
            if once:
                self._handlers[event].remove(h)
            try:
                results.append(fn(*args, **kwargs))
            except Exception as ex:
                self.errors.append((event, type(ex).__name__, str(ex)))
                if event != "error":
                    self.emit("error", ex)
        return results

    def count(self, event):
        return len(self._handlers.get(event, []))

    def events(self):
        return sorted(e for e, hs in self._handlers.items() if hs)


log = []
em = EventEmitter()


@em.on("greet")
def hello(name):
    log.append(f"hello {name}")
    return "hello"


@em.on("greet", priority=10)
def first(name):
    log.append(f"first {name}")
    return "first"


def late(name, punct="!"):
    log.append(f"late {name}{punct}")
    return len(name)


em.on("greet", late, priority=-5)
em.once("greet", lambda name, **kw: log.append(f"once {name} {sorted(kw)}"))
print(em.count("greet"), em.events())
print(em.emit("greet", "ann", punct="?"))
print(em.emit("greet", "bob"))
print(log)
log.clear()
print(em.off("greet", late), em.off("greet", late), em.count("greet"))
em.emit("greet", "cy")
print(log)


@em.on("error")
def on_error(ex):
    log.append(f"handled {type(ex).__name__}")


@em.on("divide")
def divide(a, b):
    return a // b


@em.on("divide", priority=1)
def explode(a, b):
    raise RuntimeError(f"boom {a}/{b}")


log.clear()
print(em.emit("divide", 7, 2))
print(em.emit("divide", 7, 0))
print(log)
print(em.errors)
print(em.emit("nothing"))
print(em.off("divide"), em.off("divide"), em.events())


class Counter(EventEmitter):
    def __init__(self):
        super().__init__()
        self._n = 0

    @property
    def n(self):
        return self._n

    @n.setter
    def n(self, v):
        old, self._n = self._n, v
        self.emit("change", old, v)


c = Counter()
history = []
c.on("change", lambda old, new: history.append((old, new)))
c.on("change", lambda old, new: history.append("big") if new - old > 5 else None)
c.n = 3
c.n = 10
c.n += 1
print(c.n, history)

chain = []
bus = EventEmitter()


def make_relay(depth):
    def relay(x):
        chain.append((depth, x))
        if depth < 4:
            bus.emit("relay", x * 2)
        return depth
    return relay


bus.once("relay", make_relay(1))
bus.on("relay", lambda x: chain.append(("static", x)))
bus.emit("relay", 1)
bus.emit("relay", 100)
print(chain)
print(bus.count("relay"))
