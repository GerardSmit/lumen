def add_hello(cls):
    cls.hello = lambda self: "hello from " + type(self).__name__
    return cls

@add_hello
class A:
    pass
print(A().hello(), A.__name__)

def with_attr(**attrs):
    def deco(cls):
        for k, v in attrs.items():
            setattr(cls, k, v)
        return cls
    return deco

@with_attr(x=1, y=2)
class B:
    pass
print(B.x, B.y, B().x)

registry = []
def register(cls):
    registry.append(cls.__name__)
    return cls

@register
class One: pass
@register
class Two: pass
print(registry)

def singleton(cls):
    inst = {}
    def get(*a, **k):
        if cls not in inst:
            inst[cls] = cls(*a, **k)
        return inst[cls]
    return get

@singleton
class Conf:
    def __init__(self, v=0):
        self.v = v
c1 = Conf(5)
c2 = Conf(9)
print(c1 is c2, c2.v)

def deco_methods(cls):
    for name in ("a", "b"):
        orig = getattr(cls, name)
        def make(o):
            return lambda self: "<" + o(self) + ">"
        setattr(cls, name, make(orig))
    return cls
@deco_methods
class M:
    def a(self): return "A"
    def b(self): return "B"
    def c(self): return "C"
m = M()
print(m.a(), m.b(), m.c())

def logged(fn):
    def w(self, *a):
        return "log:" + str(fn(self, *a))
    return w
class N:
    @logged
    def go(self, x):
        return x * 2
    @staticmethod
    @logged
    def st(self, x):
        return x
    @classmethod
    def cm(cls):
        return cls.__name__
print(N().go(4), N.st(None, 1), N.cm())
def freeze(cls):
    cls.__setattr__ = lambda self, k, v: (_ for _ in ()).throw(AttributeError("frozen"))
    return cls
@freeze
class F:
    pass
try:
    F().x = 1
except AttributeError as e:
    print("AttributeError", e)
