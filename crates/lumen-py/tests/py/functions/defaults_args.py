def f(a, b=2, c=3):
    return (a, b, c)
print(f(1), f(1, 5), f(1, c=9), f(c=1, a=0, b=7))

def acc(x, lst=[]):
    lst.append(x)
    return lst
print(acc(1), acc(2), acc(3, []), acc(4))
print(acc.__defaults__)

def g(a, *args, k=1, **kw):
    return a, args, k, sorted(kw.items())
print(g(1))
print(g(1, 2, 3))
print(g(1, 2, k=5, z=1, y=2))
print(g(*[1, 2], **{"k": 3, "q": 4}))

def kwonly(*, a, b=2):
    return a + b
print(kwonly(a=1), kwonly(b=5, a=1))
try:
    kwonly(1)
except TypeError:
    print("TypeError positional")
try:
    kwonly()
except TypeError:
    print("TypeError missing")

def posonly(a, b, /, c, *, d=0):
    return (a, b, c, d)
print(posonly(1, 2, 3), posonly(1, 2, c=3, d=4), posonly(1, 2, 3, d=1))
try:
    posonly(a=1, b=2, c=3)
except TypeError:
    print("TypeError posonly kw")

def pk(a, /, **kw):
    return a, kw
print(pk(1, a=2))

n = 10
def late(x=n):
    return x
n = 20
print(late())
def h(a, b=1, *r, c, d=4, **k):
    return [a, b, r, c, d, k]
print(h(1, c=3))
print(h(1, 2, 3, 4, c=5, e=6))
print(f(*(1, 2), **{"c": 8}))
try:
    f()
except TypeError:
    print("missing arg")
try:
    f(1, 2, 3, 4)
except TypeError:
    print("too many")
try:
    f(1, a=2)
except TypeError:
    print("multiple values")
def nonedef(x=None):
    if x is None:
        x = []
    x.append(1)
    return x
print(nonedef(), nonedef())
