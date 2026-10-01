x = "global-x"
y = "global-y"

def outer():
    x = "enclosing-x"
    def inner():
        x = "local-x"
        print(x, y)
    inner()
    print(x)
    def inner2():
        print(x)
    inner2()

outer()
print(x)

len = lambda s: "shadowed"
print(len("abc"))
del len
print(len("abc"))

def reads_global():
    return x + "!"
print(reads_global())

def modifies():
    global x
    x = "modified"
modifies()
print(x)

def make():
    count = 0
    def inc():
        nonlocal count
        count += 1
        return count
    def get():
        return count
    return inc, get

inc, get = make()
inc(); inc()
print(get(), inc(), get())

def deep():
    a = 1
    def l1():
        b = 2
        def l2():
            nonlocal a
            c = 3
            a += b + c
            return a
        return l2()
    return l1(), a
print(deep())

def new_global():
    global created
    created = 42
new_global()
print(created)

def unbound():
    try:
        print(zz)
    except UnboundLocalError as e:
        print("UnboundLocalError")
    zz = 1
    return zz
print(unbound())

def unbound2():
    v += 1
try:
    unbound2()
except UnboundLocalError:
    print("UnboundLocalError augassign")

def p(a, b=x):
    return a, b
x = "changed"
print(p(1))
