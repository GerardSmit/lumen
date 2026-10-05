import gc
import weakref

gc.collect()
events = []


class Obj:
    pass


a = Obj()
a.me = a
wr = weakref.ref(a, lambda r: events.append("callback"))
print("alive:", wr() is not None)
del a
print("cyclic referent still alive:", wr() is not None, events)
gc.collect()
print("cleared and called:", wr() is None, events)


class WithDel:
    def __init__(self):
        self.me = self
        self.ref = weakref.ref(self, lambda r: events.append("wr callback"))

    def __del__(self):
        events.append("__del__ sees live ref: " + str(self.ref() is not None))


events.clear()
w = WithDel()
del w
gc.collect()
print(events)

events.clear()


def ref_in_garbage():
    o = Obj()
    o.me = o
    holder = Obj()
    holder.me = holder
    holder.ref = weakref.ref(o, lambda r: events.append("never called"))
    o.holder = holder
    return weakref.ref(o)


r = ref_in_garbage()
gc.collect()
print("weakref in the same garbage gets no callback:", r() is None, events)

events.clear()


def proxies():
    o = Obj()
    o.me = o
    o.x = 5
    p = weakref.proxy(o, lambda p: events.append("proxy callback"))
    return o, p


o, p = proxies()
print(p.x)
o_ref = weakref.ref(o)
del o
gc.collect()
print("proxy dead:", end=" ")
try:
    p.x
except ReferenceError as e:
    print(type(e).__name__, events)

events.clear()
cache = weakref.WeakValueDictionary()


def fill():
    for i in range(3):
        n = Obj()
        n.me = n
        cache[i] = n


fill()
print("weak values before:", len(cache))
gc.collect()
print("weak values after:", len(cache))

keys = weakref.WeakKeyDictionary()


def fill_keys():
    for i in range(3):
        k = Obj()
        k.me = k
        keys[k] = i


fill_keys()
print("weak keys before:", len(keys))
gc.collect()
print("weak keys after:", len(keys))

s = weakref.WeakSet()


def fill_set():
    for i in range(3):
        k = Obj()
        k.me = k
        s.add(k)


fill_set()
print("weak set before:", len(s))
gc.collect()
print("weak set after:", len(s))

events.clear()


def finalize_in_cycle():
    o = Obj()
    o.me = o
    weakref.finalize(o, events.append, "finalized")


finalize_in_cycle()
print("finalize pending:", events)
gc.collect()
print("finalize ran:", events)

events.clear()


def callback_creates_garbage():
    o = Obj()
    o.me = o

    def cb(r):
        x = Obj()
        x.me = x
        events.append("cb ran")

    return weakref.ref(o, cb)


r = callback_creates_garbage()
gc.collect()
gc.collect()
print(events, r() is None)
