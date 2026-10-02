import gc

gc.collect()
log = []


class Plain:
    def __init__(self, name):
        self.name = name

    def __del__(self):
        log.append("del " + self.name)


p = Plain("plain")
del p
print("refcount death is immediate:", log)

p = Plain("rebound")
p = None
print("rebinding finalizes:", log)

Plain("temporary")
print("discarded temporary:", log)


def local_scope():
    x = Plain("local")
    log.append("in function")


local_scope()
print("after function:", log)

log.clear()


class Cyclic:
    def __init__(self, name):
        self.name = name
        self.me = self

    def __del__(self):
        log.append("del " + self.name)


c = Cyclic("cyclic")
del c
print("cycle not finalized yet:", log)
gc.collect()
print("cycle finalized by gc:", log)
gc.collect()
print("only once:", log)

log.clear()


class Pair:
    def __init__(self, name):
        self.name = name
        self.peer = None

    def __del__(self):
        log.append("del " + self.name + " peer alive: " + str(self.peer is not None))


a = Pair("a")
b = Pair("b")
a.peer = b
b.peer = a
del a, b
gc.collect()
print(sorted(log))

log.clear()
saved = []


class Phoenix:
    def __init__(self):
        self.me = self
        self.count = 0

    def __del__(self):
        log.append("del")
        saved.append(self)


ph = Phoenix()
del ph
gc.collect()
print("resurrected:", len(saved), log, gc.is_finalized(saved[0]))
saved.clear()
gc.collect()
print("second death does not call __del__ again:", log)

log.clear()


class Raises:
    def __del__(self):
        raise ValueError("from __del__")


import sys

seen = []
old_hook = sys.unraisablehook
sys.unraisablehook = lambda args: seen.append((args.exc_type.__name__, str(args.exc_value)))
r = Raises()
del r
print("unraisable:", seen)
sys.unraisablehook = old_hook


class Base:
    def __del__(self):
        log.append("base")


class Derived(Base):
    pass


Derived()
print("inherited __del__:", log)

log.clear()


class Late:
    pass


obj = Late()
Late.__del__ = lambda self: log.append("late")
del obj
print("__del__ added to the class later:", log)

log.clear()


def finalizer_creates_garbage():
    class Maker:
        def __del__(self):
            x = Cyclic("made")
            del x

    Maker()


finalizer_creates_garbage()
gc.collect()
print("garbage made in a finalizer:", log)
