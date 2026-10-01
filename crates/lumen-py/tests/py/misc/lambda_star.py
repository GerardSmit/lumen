fs = []
for i in range(3):
    fs.append(lambda: i)
print([f() for f in fs])
fs = [lambda i=i: i for i in range(3)]
print([f() for f in fs])
add = lambda a, b=10, *args, k=1, **kw: (a, b, args, k, kw)
print(add(1), add(1, 2, 3, 4, k=5, z=6))
print((lambda: "no args")(), (lambda *a: len(a))(1, 2, 3), (lambda **k: sorted(k))(b=1, a=2))
print((lambda x: lambda y: x + y)(3)(4))
print(sorted(["bb", "a", "ccc"], key=lambda s: -len(s)))
compose = lambda f, g: lambda x: f(g(x))
print(compose(lambda x: x + 1, lambda x: x * 2)(5))

def f(a, b, *args, c=3, **kwargs):
    return a, b, args, c, kwargs
print(f(1, 2), f(1, 2, 3, 4, c=5, d=6))
args = (1, 2, 3)
kw = {"c": 9, "e": 0}
print(f(*args, **kw), f(*[1, 2], *[3]), f(1, **{"b": 2}))
print(*[1, 2, 3], sep="-")
print(*"ab", *[1], end="!\n")
print([*range(3), *"ab"], {*[1, 2], 3} == {1, 2, 3}, (*[1], *(2,)), {**{"a": 1}, **{"b": 2}, "a": 3})
a, *b = "hello"
print(a, b)
def g(*, only):
    return only
print(g(only=1))
def h(a, /, b):
    return a + b
print(h(1, b=2), h(1, 2))
try:
    h(a=1, b=2)
except TypeError:
    print("TypeError positional-only")
try:
    g(1)
except TypeError:
    print("TypeError keyword-only")
def defaults(x, acc=[]):
    acc.append(x)
    return acc
print(defaults(1), defaults(2), defaults(3, []))
def kwonly_order(**kw):
    return list(kw)
print(kwonly_order(z=1, a=2, m=3))
def ret_multi():
    return 1, "two", [3]
print(ret_multi(), type(ret_multi()).__name__)
print(f.__name__, (lambda: 0).__name__, f.__defaults__, f.__kwdefaults__, g.__kwdefaults__)
print(max(*[1, 5, 3]), print.__name__, sum((*range(3), 10)))
