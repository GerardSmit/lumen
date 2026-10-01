class Bag:
    items = []
    count = 0

    def add(self, x):
        self.items.append(x)


a, b = Bag(), Bag()
a.add(1)
b.add(2)
print(a.items, b.items, Bag.items, a.items is b.items)

a.count += 1
print(a.count, b.count, Bag.count)
print(sorted(a.__dict__.items()), sorted(b.__dict__.items()))

Bag.count = 10
print(a.count, b.count, Bag.count)

a.items += [3]
print(Bag.items, "items" in a.__dict__)
a.items = a.items + [4]
print(Bag.items, a.items, "items" in a.__dict__)


class Safe:
    def __init__(self):
        self.items = []


s1, s2 = Safe(), Safe()
s1.items.append(1)
print(s1.items, s2.items)


class Shadow:
    x = "class"

    def get(self):
        return self.x


sh = Shadow()
print(sh.x, sh.get())
sh.x = "instance"
print(sh.x, Shadow.x, sh.get())
del sh.x
print(sh.x)
try:
    del sh.x
except AttributeError:
    print("AttributeError on del")

Shadow.y = "added later"
print(sh.y)
Shadow.get = lambda self: "patched"
print(sh.get())


class Slotted:
    __slots__ = ("a", "b")

    def __init__(self, a, b=None):
        self.a = a
        self.b = b


sl = Slotted(1)
print(sl.a, sl.b)
sl.b = 2
print(sl.b)
try:
    sl.c = 3
except AttributeError:
    print("no c")
print(hasattr(sl, "__dict__"))
print(sorted(Slotted.__slots__))


class SlotUnset:
    __slots__ = ("v",)


su = SlotUnset()
try:
    su.v
except AttributeError:
    print("unset slot")
su.v = 1
del su.v
print(hasattr(su, "v"))


class SlotWithDict:
    __slots__ = ("a", "__dict__")


sd = SlotWithDict()
sd.a = 1
sd.z = 2
print(sd.a, sd.z, sorted(sd.__dict__.items()))


class Base:
    tag = "base"

    @classmethod
    def retag(cls, t):
        cls.tag = t


class Derived(Base):
    pass


Derived.retag("derived")
print(Base.tag, Derived.tag)
Base.retag("new base")
print(Base.tag, Derived.tag)


class Registry:
    _all = []

    def __init__(self, n):
        self.n = n
        Registry._all.append(n)
        type(self).last = n


Registry(1)
Registry(2)
print(Registry._all, Registry.last)

o = Registry(3)
print(sorted(o.__dict__.items()))
print(sorted(k for k, v in Registry.__dict__.items() if not k.startswith("__")))
print(vars(o) is o.__dict__)

class Plain:
    pass


pl = Plain()
pl.__dict__["z"] = 1
pl.a = 2
print(sorted(pl.__dict__.items()), pl.z)
print(Plain.__name__, type(pl).__name__, pl.__class__ is Plain)
print(getattr(pl, "missing", "default"), hasattr(pl, "a"))
setattr(pl, "dyn", 5)
print(pl.dyn)
