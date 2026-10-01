x = "global"
def f():
    return x
def g():
    x = "local"
    return x
print(f(), g(), x)
def h():
    try:
        print(y)
        y = 1
    except UnboundLocalError:
        print("unbound")
h()
class C:
    a = 1
    def m(self):
        return a_global
a_global = "ag"
print(C().m(), C.a)
def comp():
    z = 10
    return [z + i for i in range(3)], {i: z for i in range(2)}
print(comp())
i = 100
print([i for i in range(3)], i)
def shadow(len):
    return len
print(shadow(5))
print(len([1, 2]))
del x
try:
    x
except NameError:
    print("deleted")
def mk():
    fns = []
    for k in range(3):
        fns.append(lambda: k)
    return [f() for f in fns]
print(mk())
print(__name__)
v = 1
def outer():
    v = 2
    def inner():
        return v
    v = 3
    return inner()
print(outer(), v)
print(type(globals()).__name__, "v" in globals())
def lc():
    a = 1
    b = 2
    return sorted(locals().items())
print(lc())
