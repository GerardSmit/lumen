def f(*args, **kwargs):
    return args, sorted(kwargs.items())


def g(a, b, c=3, *, d=4):
    return (a, b, c, d)


print(f())
print(f(1, 2))
print(f(x=1))
print(f(*[1, 2], *(3,), *"ab"))
print(f(**{"a": 1}, **{"b": 2}))
print(f(1, *[2, 3], k=4, **{"z": 5}))
print(f(*range(3), *[], **{}))
print(g(*[1, 2]))
print(g(*[1, 2, 5], d=9))
print(g(1, *[2], **{"c": 7}))
print(g(**{"a": 1, "b": 2, "d": 0}))
print(g(*(1,), b=2))
try:
    g(1, 2, **{"a": 5})
except TypeError as e:
    print(type(e).__name__)
try:
    g(1)
except TypeError as e:
    print(type(e).__name__)
try:
    g(1, 2, 3, 4)
except TypeError as e:
    print(type(e).__name__)
try:
    f(**{"a": 1}, **{"a": 2})
except TypeError as e:
    print(type(e).__name__)
try:
    f(*5)
except TypeError as e:
    print(type(e).__name__)
try:
    f(**[1])
except TypeError as e:
    print(type(e).__name__)

a = {"x": 1, "y": 2}
b = {"y": 20, "z": 30}
print(sorted({**a, **b}.items()))
print(sorted({**b, **a}.items()))
print(sorted({**a, "x": 100, **b, "w": 0}.items()))
print(sorted({"q": 1, **a}.items()))
print({**{}}, {**a} == a, {**a} is a)
la = [1, 2]
lb = [3, 4]
print([*la, *lb], [*la, 0, *lb], [*la, *la])
print([*"ab", *range(2)], [*{1: 2}], [*()])
print((*la, *lb), (*la,), {*la, *lb} == {1, 2, 3, 4})
print(sorted({*la, 9}))
print(max(*la), max(*la, *lb), sum([*la, *lb]))
print(print(*la, sep="-"))
print(*la, *lb, sep=",")


def h(*a, **k):
    a = list(a)
    a.append("m")
    return a, k


args = [1]
print(h(*args), args)
kw = {"k": 1}
r = h(**kw)
r[1]["new"] = 2
print(kw, r)
print(list(map(lambda x, y: x + y, *[[1, 2], [10, 20]])))
print("{} {}".format(*["a", "b"]), "{x}".format(**{"x": 5}))
print(dict(a, **{"y": 7}) == {"x": 1, "y": 7})
print(dict(**a, z=0) == {"x": 1, "y": 2, "z": 0})
