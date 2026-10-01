def shout(fn):
    def wrapper(*a, **k):
        return fn(*a, **k).upper()
    return wrapper

def exclaim(fn):
    def wrapper(*a, **k):
        return fn(*a, **k) + "!"
    return wrapper

@shout
def hi(name):
    return "hi " + name
print(hi("bob"), hi.__name__)

@shout
@exclaim
def a(n):
    return n
print(a("x"))

@exclaim
@shout
def b(n):
    return n
print(b("x"))

order = []
def tag(t):
    order.append("make " + t)
    def deco(fn):
        order.append("apply " + t)
        def w(*a):
            order.append("call " + t)
            return fn(*a)
        return w
    return deco

@tag("one")
@tag("two")
def f(x):
    order.append("body")
    return x
print(order)
f(1)
print(order[4:])

def count_calls(fn):
    def w(*a, **k):
        w.calls += 1
        return fn(*a, **k)
    w.calls = 0
    return w

@count_calls
def add(x, y):
    return x + y
add(1, 2); add(3, 4); add(y=1, x=1)
print(add.calls, add(1, 1))

def identity(fn):
    return fn
@identity
def same():
    return 1
print(same.__name__, same())

def replace_with_value(fn):
    return 42
@replace_with_value
def gone():
    pass
print(gone)

d = lambda fn: (lambda *a: fn(*a) * 2)
@d
def three():
    return 3
print(three())
