class A:
    cls_attr = 1
    def __init__(self):
        self.x = 1
        self.y = 2
    def method(self):
        return "m"

class B(A):
    pass

a, b = A(), B()
print(isinstance(a, A), isinstance(b, A), isinstance(a, B), isinstance(1, (str, int)), isinstance(True, int))
print(issubclass(B, A), issubclass(A, B), issubclass(bool, int), issubclass(B, (str, A)), issubclass(A, object))
print(isinstance(None, type(None)), isinstance(1.5, (int, float)), isinstance("s", object), isinstance(A, type))
print(getattr(a, "x"), getattr(a, "nope", "dflt"), getattr(a, "cls_attr"), getattr(a, "method")())
print(hasattr(a, "x"), hasattr(a, "z"), hasattr(A, "method"), hasattr(1, "real"))
setattr(a, "z", 99)
print(a.z, hasattr(a, "z"))
delattr(a, "z")
print(hasattr(a, "z"))
print(vars(a), sorted(vars(a).items()), vars(b))
print(sorted(k for k in vars(A) if not k.startswith("__")))
try:
    getattr(a, "missing")
except AttributeError as e:
    print("AttributeError", e)
print(type(1), type("s"), type(None), type([]), type(a), type(A), type(type))
print(type(1) is int, type(a) is A, type(b) == A, type(b).__name__, type(b).__mro__ == (B, A, object))
print(type(3.0).__name__, type(lambda: 0).__name__, type(len).__name__, type(print).__name__, type(A.method).__name__)
D = type("D", (A,), {"extra": 5, "hello": lambda self: "hi"})
d = D()
print(D.__name__, d.extra, d.hello(), d.x, isinstance(d, A), D.__bases__ == (A,))
print(callable(len), callable(A), callable(a), callable(1), callable(lambda: 0), callable(a.method))
print(B.__mro__ == (B, A, object), B.__bases__ == (A,), A.__name__, B.__base__ == A)
print(a.__class__ is A, a.__class__.__name__, a.__dict__ == {"x": 1, "y": 2})
print(type(type(a)).__name__, int.__name__, object.__name__)
print(id(a) == id(a), id(a) != id(b), a is a, a is not b)
print(dir(a)[-2:] if False else "x" in dir(a), "method" in dir(A), "__init__" in dir(a))
print(str(type(1)), repr(type("")), str(int))
print(isinstance([], list) and not isinstance((), list), type([]) == list)
print(len("abc"), len([1, 2]), len({}), len(range(5)), len(b"xy"), len((1,)))
