class cached:
    def __init__(self, fn):
        self.fn = fn
        self.name = fn.__name__
    def __get__(self, obj, owner=None):
        if obj is None:
            return self
        v = self.fn(obj)
        obj.__dict__[self.name] = v
        return v

class T:
    n = 0
    @cached
    def val(self):
        T.n += 1
        return 100
t = T()
print(t.val, t.val, T.n, "val" in t.__dict__)
print(type(T.val).__name__)

class P:
    def __init__(self):
        self._x = 1
    @property
    def x(self):
        return self._x
    @x.setter
    def x(self, v):
        self._x = v * 2
p = P()
p.x = 5
print(p.x)

def trace(label):
    def deco(fn):
        def w(*a, **k):
            print("enter", label, fn.__name__)
            try:
                return fn(*a, **k)
            finally:
                print("exit", label)
        return w
    return deco

@trace("outer")
@trace("inner")
def work(x):
    return x + 1
print(work(1))

def double_result(fn):
    return lambda *a, **k: fn(*a, **k) * 2
class Calc:
    @double_result
    def twice(self, x):
        return x
    @classmethod
    @double_result
    def ctwice(cls, x):
        return x
print(Calc().twice(4), Calc.ctwice(5))

def apply(*decos):
    def deco(fn):
        for d in reversed(decos):
            fn = d(fn)
        return fn
    return deco
inc = lambda f: lambda x: f(x) + 1
mul = lambda f: lambda x: f(x) * 3
@apply(inc, mul)
def base(x):
    return x
print(base(2))
@inc
@mul
def base2(x):
    return x
print(base2(2))
