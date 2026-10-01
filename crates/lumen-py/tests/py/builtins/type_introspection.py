class Base:
    cls_attr = 1

    def __init__(self):
        self.a = 1
        self.b = 2

    def method(self):
        return self.a

    @staticmethod
    def sm():
        return "sm"

    @classmethod
    def cm(cls):
        return cls.__name__


class Child(Base):
    extra = 5


def func():
    pass


b = Base()
c = Child()
print(type(1).__name__, type("s").__name__, type(1.5).__name__, type(None).__name__, type(True).__name__)
print(type([]).__name__, type(()).__name__, type({}).__name__, type({1}).__name__, type(b"").__name__)
print(type(func).__name__, type(len).__name__, type(lambda: 0).__name__, type(Base).__name__, type(type).__name__)
print(type(b) is Base, type(c) is Base, type(c) is Child, type(b) == Base)
print(type(1) is int, type(True) is int, type(True) is bool)
print(isinstance(1, int), isinstance(True, int), isinstance(1, bool), isinstance(1.0, int))
print(isinstance(1, (str, int)), isinstance("a", (int, float)), isinstance(1, ()), isinstance(c, (Base, int)))
print(isinstance(c, Base), isinstance(b, Child), isinstance(c, object), isinstance(Base, type), isinstance(None, object))
print(isinstance(1, (str, (float, (int,)))))
print(issubclass(Child, Base), issubclass(Base, Child), issubclass(Base, Base), issubclass(bool, int))
print(issubclass(Child, (int, Base)), issubclass(int, object), issubclass(type, object))
try:
    isinstance(1, "int")
except TypeError as e:
    print(type(e).__name__)
try:
    issubclass(1, int)
except TypeError as e:
    print(type(e).__name__)
print(callable(len), callable(Base), callable(b), callable(func), callable(1), callable(Base.method), callable(None))
print(callable(b.method), callable(Base.sm), callable(lambda: 0), callable("s"))

print(getattr(b, "a"), getattr(b, "zz", "dflt"), getattr(b, "zz", None), getattr(c, "cls_attr"))
try:
    getattr(b, "zz")
except AttributeError as e:
    print(type(e).__name__)
setattr(b, "c", 3)
print(b.c, hasattr(b, "c"), hasattr(b, "nope"), hasattr(Base, "method"), hasattr(b, "method"))
delattr(b, "c")
print(hasattr(b, "c"))
try:
    delattr(b, "c")
except AttributeError as e:
    print(type(e).__name__)
setattr(Base, "late", 42)
print(b.late, c.late, Child.late)
del Base.late
print(hasattr(b, "late"))
print(sorted(n for n in dir(Base) if not n.startswith("__")))
print(sorted(n for n in dir(c) if not n.startswith("__")))
print(sorted(n for n in dir(Child) if not n.startswith("_")))
print(vars(b), sorted(vars(c).items()))
print(sorted(k for k in vars(Child) if not k.startswith("__")))
print(b.__dict__ is vars(b), c.__dict__ == {"a": 1, "b": 2})
b.__dict__["viadict"] = 9
print(b.viadict)
print(Base.__name__, Child.__name__, Base.__qualname__, func.__name__, func.__qualname__)
print(Base.method.__name__, Base.method.__qualname__, Child.cm(), Base.cm(), Base.sm())
print(b.__class__ is Base, c.__class__.__name__, c.__class__.__class__.__name__, int.__class__.__name__)
print(Child.__bases__ == (Base,), Base.__bases__ == (object,), Child.__mro__ == (Child, Base, object))
print([k.__name__ for k in Child.__mro__])
print((1).__class__.__name__, "".__class__.__name__, None.__class__.__name__)
print(Base.__module__, func.__module__, Base.__doc__, func.__doc__)


def outer():
    def inner():
        pass

    class Local:
        pass

    return inner, Local


i, L = outer()
print(i.__name__, i.__qualname__, L.__name__, L.__qualname__)
K = type("K", (Base,), {"z": 7})
print(K.__name__, K().z, K().a, issubclass(K, Base), type(K()).__name__)
print(type("X", (), {}).__name__)
