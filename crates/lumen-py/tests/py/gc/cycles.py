import gc
import weakref

gc.collect()


class Node:
    pass


def make_cycle():
    a = Node()
    b = Node()
    a.other = b
    b.other = a
    return weakref.ref(a), weakref.ref(b)


ra, rb = make_cycle()
print("alive before collect:", ra() is not None, rb() is not None)
found = gc.collect()
print("collected something:", found >= 2)
print("alive after collect:", ra() is not None, rb() is not None)

lst = []
lst.append(lst)
del lst
print("list cycle collected:", gc.collect() >= 1)


class Box(list):
    pass


def self_referencing_box():
    b = Box()
    b.append(b)
    return weakref.ref(b)


r = self_referencing_box()
print("subclass alive:", r() is not None)
gc.collect()
print("subclass dead:", r() is None)


def dict_cycle():
    d = {}
    d["me"] = d
    return d


d = dict_cycle()
del d
print("dict cycle collected:", gc.collect() >= 1)


def closure_cycle():
    def inner():
        return inner

    return weakref.ref(inner)


r = closure_cycle()
print("closure alive:", r() is not None)
gc.collect()
print("closure dead:", r() is None)


class Holder:
    def __init__(self):
        self.me = self
        self.items = [self, {"k": self}]


def holder_cycle():
    return weakref.ref(Holder())


r = holder_cycle()
gc.collect()
print("holder dead:", r() is None)

t = []
tup = (t,)
t.append(tup)
del t, tup
print("tuple cycle collected:", gc.collect() >= 2)


def chain():
    nodes = [Node() for _ in range(5)]
    for i, n in enumerate(nodes):
        n.next = nodes[(i + 1) % 5]
    return [weakref.ref(n) for n in nodes]


refs = chain()
print("ring alive:", all(r() is not None for r in refs))
gc.collect()
print("ring dead:", all(r() is None for r in refs))

keep = Node()
keep.me = keep
rk = weakref.ref(keep)
gc.collect()
print("referenced cycle survives:", rk() is not None)
del keep
gc.collect()
print("then dies:", rk() is None)


class Outer:
    def __init__(self):
        self.inner = Inner(self)


class Inner:
    def __init__(self, outer):
        self.outer = outer


def parent_child():
    o = Outer()
    return weakref.ref(o), weakref.ref(o.inner)


ro, ri = parent_child()
gc.collect()
print("parent child dead:", ro() is None and ri() is None)

print("generation arg:", gc.collect(0) >= 0, gc.collect(1) >= 0, gc.collect(2) >= 0)
try:
    gc.collect(3)
except ValueError as e:
    print("ValueError:", e)
try:
    gc.collect(-1)
except ValueError as e:
    print("ValueError:", e)
