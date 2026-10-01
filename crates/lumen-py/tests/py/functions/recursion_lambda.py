def fact(n):
    return 1 if n <= 1 else n * fact(n - 1)
print(fact(0), fact(5), fact(25))

def fib(n):
    return n if n < 2 else fib(n - 1) + fib(n - 2)
print([fib(i) for i in range(15)])

def ack(m, n):
    if m == 0:
        return n + 1
    if n == 0:
        return ack(m - 1, 1)
    return ack(m - 1, ack(m, n - 1))
print(ack(2, 3))

def even(n):
    return True if n == 0 else odd(n - 1)
def odd(n):
    return False if n == 0 else even(n - 1)
print(even(10), odd(7), even(7))

def depth(n):
    return 0 if n == 0 else 1 + depth(n - 1)
print(depth(500))

def inf(n):
    return inf(n + 1)
try:
    inf(0)
except RecursionError:
    print("RecursionError")

sq = lambda x: x * x
print(sq(7), (lambda a, b=2, *c, **d: (a, b, c, d))(1, 3, 4, z=5))
print(sq.__name__, (lambda: 0).__name__)
print(sorted([3, 1, 2], key=lambda v: -v))
print(list(map(lambda a, b: a + b, [1, 2], [10, 20])))
print(list(filter(lambda v: v % 2, range(8))))
compose = lambda f, g: lambda x: f(g(x))
print(compose(sq, lambda x: x + 1)(3))
fnlist = {"add": lambda a, b: a + b, "sub": lambda a, b: a - b}
print([fnlist[k](5, 3) for k in sorted(fnlist)])
def hanoi(n, a, b, c):
    return 0 if n == 0 else hanoi(n - 1, a, c, b) + 1 + hanoi(n - 1, c, b, a)
print(hanoi(10, "a", "b", "c"))
def flatten(x):
    return [y for i in x for y in flatten(i)] if isinstance(x, list) else [x]
print(flatten([1, [2, [3, [4]], 5]]))
def perms(s):
    if len(s) <= 1:
        return [s]
    return [s[i] + p for i in range(len(s)) for p in perms(s[:i] + s[i+1:])]
print(perms("abc"))
