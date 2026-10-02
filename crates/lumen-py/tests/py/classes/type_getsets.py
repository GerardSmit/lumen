import functools
import struct
import pickle

for n in ("__name__", "__qualname__", "__doc__", "__module__", "__bases__", "__abstractmethods__", "__dict__"):
    d = type.__dict__[n]
    print(n, type(d).__name__)

class A:
    "doc"

print(type.__dict__["__name__"].__get__(A))
type.__dict__["__name__"].__set__(A, "B")
print(A.__name__, A.__qualname__)
try:
    del A.__name__
except TypeError as e:
    print(e)
try:
    A.__name__ = 5
except TypeError as e:
    print(e)
try:
    int.__name__ = "x"
except TypeError as e:
    print(e)
try:
    A.__abstractmethods__
except AttributeError as e:
    print(e)
A.__abstractmethods__ = frozenset()
print(A.__abstractmethods__)
del A.__abstractmethods__


class S:
    @staticmethod
    def f(): "static doc"
    @classmethod
    def g(cls): "class doc"

print(S.__dict__["f"].__doc__, S.__dict__["g"].__doc__)

@functools.singledispatch
def h(x): return "obj"

class K:
    @functools.singledispatchmethod
    def m(self, x): return "base"
    @m.register
    @classmethod
    def _(cls, x: int): return "int"
    @m.register
    @staticmethod
    def _s(x: str): return "str"

print(K().m(1), K().m("a"), K().m(1.5))

print(struct.unpack("<f", struct.pack("<f", 2.9802322387695313e-08))[0])


class I(int):
    pass

class L(list):
    pass

for proto in range(6):
    i = pickle.loads(pickle.dumps(I(5), proto))
    l = pickle.loads(pickle.dumps(L([1, 2]), proto))
    print(proto, type(i).__name__, i, type(l).__name__, l)
