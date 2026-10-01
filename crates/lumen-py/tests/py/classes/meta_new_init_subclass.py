class Meta(type):
    created = []
    def __new__(mcs, name, bases, ns):
        Meta.created.append(name)
        ns["added"] = name.lower()
        return super().__new__(mcs, name, bases, ns)
    def __call__(cls, *a, **k):
        obj = super().__call__(*a, **k)
        obj.tagged = True
        return obj
    def __repr__(cls):
        return "<Meta " + cls.__name__ + ">"
class K(metaclass=Meta):
    def __init__(self, v):
        self.v = v
k = K(3)
print(K, k.v, k.tagged, K.added, Meta.created, type(K).__name__)
class K2(K): pass
print(Meta.created, type(K2) is Meta, K2.added)

class Plugin:
    subs = []
    def __init_subclass__(cls, tag="none", **kw):
        super().__init_subclass__(**kw)
        cls.tag = tag
        Plugin.subs.append(cls.__name__)
class P1(Plugin, tag="one"): pass
class P2(Plugin): pass
print(Plugin.subs, P1.tag, P2.tag)

class Single:
    _inst = None
    def __new__(cls, *a):
        if cls._inst is None:
            cls._inst = super().__new__(cls)
        return cls._inst
    def __init__(self, v):
        self.v = v
s1 = Single(1)
s2 = Single(2)
print(s1 is s2, s1.v)

class Pos(int):
    def __new__(cls, v):
        return super().__new__(cls, abs(v))
print(Pos(-5), Pos(3) + 1, type(Pos(-1)).__name__)
class Other:
    def __new__(cls):
        return 5
    def __init__(self):
        print("not called")
print(Other())

class Generic:
    def __class_getitem__(cls, item):
        return (cls.__name__, item)
print(Generic[int], Generic["a"])
print(Generic[1, 2])

C = type("C", (), {"x": 1, "f": lambda self: self.x + 1})
print(C().f(), C.__name__, type(C).__name__)
D = type("D", (C,), {"x": 10})
print(D().f(), D.__mro__[1].__name__)
class WithNs(type):
    @classmethod
    def __prepare__(mcs, name, bases):
        return {"pre": 1}
class Q(metaclass=WithNs):
    pass
print(Q.pre)
class Ab(type):
    def __instancecheck__(cls, inst):
        return inst == 42
class Is(metaclass=Ab): pass
print(isinstance(42, Is), isinstance(1, Is))
