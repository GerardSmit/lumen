class G:
    real = 1
    def __getattr__(self, name):
        return "missing:" + name
g = G()
print(g.real, g.foo, g.bar)
g.foo = 5
print(g.foo)
print(getattr(g, "zzz"), hasattr(g, "anything"))

class Log:
    def __init__(self):
        object.__setattr__(self, "calls", [])
    def __getattribute__(self, name):
        if name != "calls":
            object.__getattribute__(self, "calls").append(("get", name))
        return object.__getattribute__(self, name)
    def __setattr__(self, name, v):
        object.__getattribute__(self, "calls").append(("set", name, v))
        object.__setattr__(self, name, v)
    def __delattr__(self, name):
        object.__getattribute__(self, "calls").append(("del", name))
        object.__delattr__(self, name)
l = Log()
l.a = 1
l.a
del l.a
print(l.calls)
try:
    l.a
except AttributeError:
    print("AttributeError")

class Upper:
    def __setattr__(self, k, v):
        super().__setattr__(k.upper(), v)
u = Upper()
u.name = 1
u.Other = 2
print(sorted(u.__dict__))

class Slots:
    __slots__ = ("a", "b")
    def __init__(self):
        self.a = 1
s = Slots()
s.b = 2
print(s.a, s.b)
try:
    s.c = 3
except AttributeError:
    print("no c")
print(hasattr(s, "__dict__"))

class Desc:
    def __get__(self, obj, owner):
        return ("get", obj is None, owner.__name__)
    def __set__(self, obj, v):
        print("set", v)
    def __set_name__(self, owner, name):
        print("named", owner.__name__, name)
class H:
    d = Desc()
h = H()
print(h.d, H.d)
h.d = 9

class Ro:
    def __get__(self, obj, owner):
        return 42
class J:
    r = Ro()
j = J()
j.r = 7
print(j.r, j.__dict__)
class Dict:
    pass
d = Dict()
d.x = 1
setattr(d, "y", 2)
print(d.__dict__, getattr(d, "z", "dflt"), vars(d) is d.__dict__)
delattr(d, "x")
print(sorted(d.__dict__), hasattr(d, "x"))
Dict.cls_attr = "c"
print(d.cls_attr, "cls_attr" in d.__dict__)
