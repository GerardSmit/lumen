a = 1
b = 2
del a
try:
    print(a)
except NameError as e:
    print("NameError", e)
print("a" in globals(), "b" in globals())

lst = [0, 1, 2, 3, 4, 5]
del lst[1]
print(lst)
del lst[1:3]
print(lst)
del lst[::2]
print(lst)
d = {"x": 1, "y": 2, "z": 3}
del d["y"]
print(d)
try:
    del d["nope"]
except KeyError as e:
    print("KeyError", e)

class O:
    pass
o = O()
o.v = 1
del o.v
print(hasattr(o, "v"))
try:
    del o.v
except AttributeError:
    print("AttributeError")

def f():
    q = 1
    del q
    try:
        return q
    except UnboundLocalError:
        return "unbound"
print(f())

def mk():
    fs = []
    for i in range(3):
        fs.append(lambda: i)
    return [g() for g in fs]
print(mk())

def mk2():
    fs = []
    for i in range(3):
        def g(i=i):
            return i
        fs.append(g)
    return [g() for g in fs]
print(mk2())

def adders():
    return [(lambda x, n=n: x + n) for n in range(3)]
print([f(10) for f in adders()])

def cell():
    v = 1
    def get():
        return v
    v = 2
    return get()
print(cell())

def swap_vals():
    x, y = 1, 2
    x, y = y, x
    return x, y
print(swap_vals())
x = y = z = []
x.append(1)
print(y, z, x is z)
a, (b, c), *d = 1, (2, 3), 4, 5
print(a, b, c, d)
for i in range(3):
    pass
print(i)
