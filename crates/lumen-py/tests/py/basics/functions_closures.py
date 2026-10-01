def add(a, b=2, *args, c=3, **kw):
    return (a, b, args, c, sorted(kw.items()))
print(add(1))
print(add(1, 5, 6, 7, c=9, z=1, y=2))
print(add(*[1, 2, 3], **{"c": 4, "k": 5}))
def counter():
    n = 0
    def inc():
        nonlocal n
        n += 1
        return n
    return inc
c1, c2 = counter(), counter()
print(c1(), c1(), c2())
fs = [lambda i=i: i * 2 for i in range(4)]
print([f() for f in fs])
gs = [lambda: i for i in range(4)]
print([g() for g in gs])
G = 1
def setg():
    global G
    G += 10
setg()
print(G)
def kwonly(*, a, b=2):
    return a + b
print(kwonly(a=1))
try:
    kwonly(1)
except TypeError as e:
    print("TypeError")
try:
    add()
except TypeError:
    print("TypeError2")
def posonly(a, b, /, c):
    return a, b, c
print(posonly(1, 2, c=3))
try:
    posonly(a=1, b=2, c=3)
except TypeError:
    print("posonly err")
def fact(n):
    return 1 if n <= 1 else n * fact(n - 1)
print(fact(10), fact(20))
def deco(fn):
    def w(*a, **k):
        return "<" + str(fn(*a, **k)) + ">"
    return w
@deco
def hello(n):
    return n * 2
print(hello(4))
print(hello.__name__, add.__name__)
def defaults(x, l=[]):
    l.append(x)
    return l
print(defaults(1), defaults(2))
print((lambda *a, **k: (a, k))(1, 2, x=3))
def gen():
    yield 1
    yield 2
    return 3
print(list(gen()))
print(callable(gen), callable(5))
def outer():
    x = 1
    def mid():
        def inner():
            return x
        return inner
    return mid()()
print(outer())
