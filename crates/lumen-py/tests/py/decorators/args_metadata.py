def repeat(n):
    def deco(fn):
        def w(*a, **k):
            r = None
            for _ in range(n):
                r = fn(*a, **k)
            return r
        return w
    return deco

hits = []
@repeat(3)
def ping(x):
    hits.append(x)
    return len(hits)
print(ping("a"), hits)

def wraps_manual(fn):
    def w(*a, **k):
        return fn(*a, **k)
    w.__name__ = fn.__name__
    w.__doc__ = fn.__doc__
    w.__wrapped__ = fn
    return w

@wraps_manual
def documented(x):
    """my doc"""
    return x + 1
print(documented.__name__, documented.__doc__, documented(1))
print(documented.__wrapped__.__name__)

def plain(fn):
    def w(*a):
        return fn(*a)
    return w
@plain
def lost():
    """gone"""
print(lost.__name__, lost.__doc__)

def optional_args(fn=None, *, prefix=">"):
    def deco(f):
        return lambda *a: prefix + str(f(*a))
    if fn is None:
        return deco
    return deco(fn)

@optional_args
def p1(x): return x
@optional_args(prefix="#")
def p2(x): return x
print(p1(1), p2(2))

def validate(*types):
    def deco(fn):
        def w(*a):
            for v, t in zip(a, types):
                if not isinstance(v, t):
                    raise TypeError("bad " + t.__name__)
            return fn(*a)
        return w
    return deco
@validate(int, str)
def pair(a, b):
    return (a, b)
print(pair(1, "x"))
try:
    pair("x", 1)
except TypeError as e:
    print(e)

class Memo:
    def __init__(self, fn):
        self.fn = fn
        self.cache = {}
    def __call__(self, n):
        if n not in self.cache:
            self.cache[n] = self.fn(n)
        return self.cache[n]
@Memo
def sq(n):
    print("computing", n)
    return n * n
print(sq(4), sq(4), sq(5), sorted(sq.cache))

registry = {}
def register(name):
    def deco(fn):
        registry[name] = fn
        return fn
    return deco
@register("a")
def fa(): return "A"
@register("b")
def fb(): return "B"
print(sorted(registry), registry["b"](), fa())
