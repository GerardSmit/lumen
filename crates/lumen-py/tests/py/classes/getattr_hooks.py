class Fallback:
    real = "real"

    def __init__(self):
        self.inst = "inst"

    def __getattr__(self, name):
        print("__getattr__", name)
        if name.startswith("dyn_"):
            return name[4:]
        raise AttributeError(name)


f = Fallback()
print(f.real)
print(f.inst)
print(f.dyn_thing)
try:
    f.nothing
except AttributeError as e:
    print("AttributeError", e)
print(getattr(f, "nothing", "dflt"))
print(hasattr(f, "dyn_x"), hasattr(f, "other"))


class Spy:
    def __init__(self):
        object.__setattr__(self, "log", [])
        object.__setattr__(self, "x", 1)

    def __getattribute__(self, name):
        if name != "log":
            object.__getattribute__(self, "log").append(("get", name))
        return object.__getattribute__(self, name)

    def __setattr__(self, name, value):
        object.__getattribute__(self, "log").append(("set", name, value))
        object.__setattr__(self, name, value)

    def __delattr__(self, name):
        object.__getattribute__(self, "log").append(("del", name))
        object.__delattr__(self, name)


s = Spy()
s.x
s.y = 5
s.y
del s.y
try:
    s.missing
except AttributeError:
    pass
print(s.log)


class Both:
    def __getattribute__(self, name):
        try:
            return object.__getattribute__(self, name)
        except AttributeError:
            return "gattribute-miss:" + name

    def __getattr__(self, name):
        return "never reached"

    present = 1


b = Both()
print(b.present, b.absent)


class Raising:
    def __getattribute__(self, name):
        if name == "boom":
            raise AttributeError("explicit")
        return object.__getattribute__(self, name)

    def __getattr__(self, name):
        return "getattr saw " + name


r = Raising()
print(r.boom, r.other)


class Frozen:
    def __init__(self, **kw):
        for k, v in kw.items():
            object.__setattr__(self, k, v)

    def __setattr__(self, name, value):
        raise AttributeError("frozen: cannot set " + name)

    def __delattr__(self, name):
        raise AttributeError("frozen: cannot delete " + name)


fr = Frozen(a=1)
print(fr.a)
for op in (lambda: setattr(fr, "a", 2), lambda: delattr(fr, "a")):
    try:
        op()
    except AttributeError as e:
        print(e)
print(fr.a)


class Proxy:
    def __init__(self, target):
        object.__setattr__(self, "_target", target)

    def __getattr__(self, name):
        return getattr(self._target, name)

    def __setattr__(self, name, value):
        setattr(self._target, name, value)


class Target:
    def __init__(self):
        self.v = 1

    def hello(self):
        return "hello from target"


t = Target()
p = Proxy(t)
print(p.v, p.hello())
p.v = 99
print(t.v, p.v)


class Dict:
    def __init__(self):
        self.__dict__["data"] = {}

    def __getattr__(self, name):
        try:
            return self.data[name]
        except KeyError:
            raise AttributeError(name) from None

    def __setattr__(self, name, value):
        self.data[name] = value


d = Dict()
d.a = 1
d.b = 2
print(d.a, d.b, sorted(d.data.items()))
print(hasattr(d, "c"))


class Counter:
    n = 0

    def __getattribute__(self, name):
        type(self).n += 1
        return object.__getattribute__(self, name)

    def m(self):
        return 1


c = Counter()
c.m()
c.m()
print(Counter.n)
