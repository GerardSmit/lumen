def counter():
    n = 0
    def inc():
        nonlocal n
        n += 1
        return n
    return inc
c1, c2 = counter(), counter()
print(c1(), c1(), c1(), c2())

def adders():
    return [lambda x, i=i: x + i for i in range(3)]
print([a(10) for a in adders()])
late = [lambda: i for i in range(3)]
print([l() for l in late])

g = 1
def readg():
    return g
def setg():
    global g
    g = 99
print(readg()); setg(); print(readg(), g)

def outer():
    x = "outer"
    def mid():
        def inner():
            return x
        return inner()
    return mid()
print(outer())

def shadow():
    g = "local"
    return g
print(shadow(), g)

def mk(k):
    def mul(x):
        return x * k
    return mul
dbl = mk(2)
print(dbl(4), mk(3)(4))
print([c.cell_contents for c in dbl.__closure__])
print(dbl.__closure__ is not None, readg.__closure__)

def unbound():
    try:
        print(zz)
    except UnboundLocalError:
        print("UnboundLocalError")
    zz = 1
unbound()
def nested_nl():
    a = 1
    def f():
        nonlocal a
        a += 1
        def g():
            nonlocal a
            a *= 10
        g()
    f()
    return a
print(nested_nl())
def gen_fib():
    memo = {}
    def fib(n):
        if n < 2:
            return n
        if n not in memo:
            memo[n] = fib(n - 1) + fib(n - 2)
        return memo[n]
    return fib
print(gen_fib()(60))
x = 5
def listcomp_scope():
    x = 1
    return [x for _ in range(2)]
print(listcomp_scope(), x)
