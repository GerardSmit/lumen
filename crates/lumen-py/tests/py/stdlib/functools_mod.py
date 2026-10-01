from functools import reduce, partial, lru_cache, wraps, cmp_to_key, total_ordering

print(reduce(lambda a, b: a + b, [1, 2, 3, 4]), reduce(lambda a, b: a * b, range(1, 6), 10), reduce(max, [3, 9, 2]))
add = lambda a, b, c=0: a + b + c
p = partial(add, 1)
print(p(2), p(2, c=5), partial(add, 1, 2, c=3)())
print(p.func is add, p.args, p.keywords)

calls = []
@lru_cache(maxsize=None)
def fib(n):
    calls.append(n)
    return n if n < 2 else fib(n - 1) + fib(n - 2)
print(fib(30), len(calls))
print(fib.cache_info().hits, fib.cache_info().misses)
fib.cache_clear()
print(fib.cache_info().currsize)

def logged(fn):
    @wraps(fn)
    def inner(*a, **k):
        print("calling", fn.__name__)
        return fn(*a, **k)
    return inner

@logged
def hello(name):
    """Greets."""
    return "hi " + name
print(hello("bob"), hello.__name__, hello.__doc__)

print(sorted([3, 1, 2], key=cmp_to_key(lambda a, b: b - a)))

@total_ordering
class V:
    def __init__(self, n): self.n = n
    def __eq__(self, o): return self.n == o.n
    def __lt__(self, o): return self.n < o.n
print(V(1) < V(2), V(1) <= V(1), V(3) > V(2), V(1) >= V(2))
