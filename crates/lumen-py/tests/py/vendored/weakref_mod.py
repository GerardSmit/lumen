import weakref


class Node:
    def __init__(self, name):
        self.name = name

    def hello(self):
        return "hi " + self.name


n = Node("a")
r = weakref.ref(n)
print(r() is n, weakref.getweakrefcount(n), r() is weakref.ref(n)())
dead = []
r2 = weakref.ref(n, lambda ref: dead.append("cb"))
print(r2 is r, len(weakref.getweakrefs(n)))
p = weakref.proxy(n)
print(p.name, p.hello())
del n
print(r(), dead)
try:
    p.name
except ReferenceError:
    print("ReferenceError")

d = weakref.WeakValueDictionary()
a, b = Node("x"), Node("y")
d["a"] = a
d["b"] = b
print(sorted(d.keys()), d["a"].name)
del a
print(sorted(d.keys()))

k = weakref.WeakKeyDictionary()
key = Node("k")
k[key] = 1
print(len(k), k[key])
del key
print(len(k))

s = weakref.WeakSet()
x = Node("s")
s.add(x)
print(len(s), x in s)
del x
print(len(s))

o = Node("o")
fin = weakref.finalize(o, dead.append, "finalized")
print(fin.alive)
del o
print(fin.alive, dead)


class M:
    def f(self):
        return 1


m = M()
wm = weakref.WeakMethod(m.f)
print(wm()())
del m
print(wm())
try:
    weakref.ref(1)
except TypeError:
    print("TypeError")
