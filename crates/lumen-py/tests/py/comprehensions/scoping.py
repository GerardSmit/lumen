x = "global"
r = [x for x in range(3)]
print(x, r)

def f():
    y = "outer"
    vals = [y + str(i) for i in range(3)]
    return vals

print(f())

def g():
    i = "keep"
    _ = [i for i in range(5)]
    return i

print(g())

class C:
    a = 10
    items = [1, 2, 3]
    doubled = [i * 2 for i in items]
    pairs = [(i, j) for i in items for j in range(4) if i < j]
    squares = {i: i * i for i in range(3)}

print(C.doubled, C.pairs, C.squares)

class D:
    n = 3
    try:
        bad = [n for _ in range(2)]
    except NameError:
        bad = "NameError"
    ok = [k for k in range(n)]

print(D.bad, D.ok)

fns = [lambda: i for i in range(3)]
print([fn() for fn in fns])
fns2 = [lambda i=i: i for i in range(3)]
print([fn() for fn in fns2])

def counter():
    n = 0
    def inc():
        nonlocal n
        n += 1
        return n
    return [inc() for _ in range(4)]

print(counter())

gen = (v for v in range(3))
v = "mod"
print(list(gen), v)

outer = [1, 2]
nested = [[o * i for i in range(2)] for o in outer]
print(nested)
print([z for z in range(3)] + [z for z in range(2)])
try:
    print(z)
except NameError:
    print("z not defined")
