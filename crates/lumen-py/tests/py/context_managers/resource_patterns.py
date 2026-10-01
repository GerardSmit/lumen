class Pool:
    def __init__(self):
        self.free = ["r1", "r2", "r3"]
        self.used = []
    def acquire(self):
        r = self.free.pop(0)
        self.used.append(r)
        return r
    def release(self, r):
        self.used.remove(r)
        self.free.append(r)
class Lease:
    def __init__(self, pool):
        self.pool = pool
    def __enter__(self):
        self.r = self.pool.acquire()
        return self.r
    def __exit__(self, *a):
        self.pool.release(self.r)
p = Pool()
with Lease(p) as r:
    print(r, p.used, p.free)
print(p.used, p.free)
try:
    with Lease(p) as r1, Lease(p) as r2:
        print(r1, r2, p.used)
        raise KeyError
except KeyError:
    print(p.used, p.free)

class Txn:
    def __init__(self, store):
        self.store = store
    def __enter__(self):
        self.snap = dict(self.store)
        return self.store
    def __exit__(self, et, ev, tb):
        if et:
            self.store.clear()
            self.store.update(self.snap)
            print("rolled back", et.__name__)
        else:
            print("committed")
        return et is ValueError
db = {"a": 1}
with Txn(db) as s:
    s["b"] = 2
print(db)
with Txn(db) as s:
    s["c"] = 3
    raise ValueError
print(db)
try:
    with Txn(db) as s:
        s["d"] = 4
        raise KeyError
except KeyError:
    print(db)

class Indent:
    level = 0
    def __enter__(self):
        Indent.level += 1
    def __exit__(self, *a):
        Indent.level -= 1
def say(s):
    print("  " * Indent.level + s)
say("top")
with Indent():
    say("one")
    with Indent():
        say("two")
    say("one again")
say("top again")

class Attr:
    def __init__(self, obj, name, val):
        self.o, self.n, self.v = obj, name, val
    def __enter__(self):
        self.old = getattr(self.o, self.n)
        setattr(self.o, self.n, self.v)
    def __exit__(self, *a):
        setattr(self.o, self.n, self.old)
class Cfg:
    debug = False
c = Cfg()
with Attr(c, "debug", True):
    print(c.debug)
print(c.debug)
class Timer:
    def __enter__(self):
        self.t = 0
        return self
    def __exit__(self, *a):
        self.done = True
with Timer() as t:
    t.t += 5
print(t.t, t.done)
class Self:
    def __enter__(self):
        return self
    def __exit__(self, *a):
        print("closed")
with Self() as s1, s1 as s2:
    print(s1 is s2)
