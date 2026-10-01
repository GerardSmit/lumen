import functools
from functools import partial, reduce, lru_cache, wraps, cmp_to_key, total_ordering, cache

print("--- partial")
def power(base, exp, scale=1):
    return base ** exp * scale

square = partial(power, exp=2)
cube = partial(power, exp=3, scale=10)
print(square(7), cube(2), partial(power, 2)(10), partial(power, 2, 3, scale=5)())
print(square.func is power, square.args, square.keywords, partial(print, "p:")("x"))
print(list(map(partial(int, base=2), ["101", "1111"])), partial(sorted, reverse=True)([3, 1, 2]))

print("--- reduce")
print(reduce(lambda a, b: a + b, range(1, 11)), reduce(lambda a, b: a * b, range(1, 8), 1))
print(reduce(lambda a, b: a + b, [], 0), reduce(max, [3, 9, 2]), reduce(lambda acc, s: acc + s, "abc", ">"))
print(reduce(lambda acc, kv: {**acc, kv[0]: kv[1]}, [("a", 1), ("b", 2)], {}))
try:
    reduce(lambda a, b: a, [])
except TypeError:
    print("TypeError empty reduce")

print("--- lru_cache")
calls = []

@lru_cache(maxsize=None)
def fib(n):
    calls.append(n)
    return n if n < 2 else fib(n - 1) + fib(n - 2)

print(fib(40), len(calls))
info = fib.cache_info()
print(info.hits, info.misses, info.maxsize, info.currsize)
fib.cache_clear()
print(fib.cache_info().currsize, fib(5), fib.__name__)

@lru_cache(maxsize=2)
def sq(x):
    calls.append(("sq", x))
    return x * x

del calls[:]
print([sq(i) for i in (1, 2, 1, 3, 1)], calls)
print(sq.cache_info())

@cache
def fact(n):
    return 1 if n <= 1 else n * fact(n - 1)

print(fact(25), fact(5))

print("--- wraps")
def logged(fn):
    @wraps(fn)
    def inner(*args, **kwargs):
        """inner doc"""
        res = fn(*args, **kwargs)
        print("call", fn.__name__, args, sorted(kwargs.items()), "->", res)
        return res
    return inner

@logged
def add(a, b=0):
    """Add numbers."""
    return a + b

print(add(1, b=2), add.__name__, add.__doc__, add.__wrapped__(5, 5))

def unwrapped(fn):
    def inner(*a):
        return fn(*a)
    return inner

@unwrapped
def named(x):
    """doc"""
    return x

print(named.__name__, named.__doc__)

print("--- cmp_to_key")
def compare_len_then_alpha(a, b):
    if len(a) != len(b):
        return len(a) - len(b)
    return (a > b) - (a < b)

words = ["pear", "fig", "apple", "kiwi", "date", "banana", "plum"]
print(sorted(words, key=cmp_to_key(compare_len_then_alpha)))
print(sorted([3, 1, 2], key=cmp_to_key(lambda a, b: b - a)), max(words, key=cmp_to_key(compare_len_then_alpha)))
print(sorted([(1, "b"), (1, "a"), (0, "z")], key=cmp_to_key(lambda a, b: (a[0] > b[0]) - (a[0] < b[0]))))

print("--- total_ordering")
@total_ordering
class Version:
    def __init__(self, *parts):
        self.parts = parts

    def __eq__(self, other):
        return self.parts == other.parts

    def __lt__(self, other):
        return self.parts < other.parts

    def __repr__(self):
        return "V" + ".".join(map(str, self.parts))

v1, v2, v3 = Version(1, 2), Version(1, 10), Version(1, 2)
print(v1 < v2, v1 <= v3, v2 > v1, v2 >= v3, v1 != v2, v1 == v3)
print(sorted([v2, v1, Version(0, 9), Version(2)]), max(v1, v2), min(v1, v2))

print("--- misc")
print(functools.reduce(operator_add := (lambda a, b: a + b), [[1], [2], [3]]))
class Lazy:
    @functools.cached_property
    def value(self):
        print("computing")
        return 42

lz = Lazy()
print(lz.value, lz.value)
print(callable(functools.partial(len)), functools.partial(len, [1, 2])())
