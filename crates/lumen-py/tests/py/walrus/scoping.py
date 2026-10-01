def f():
    if (v := 1):
        pass
    return v
print(f())

def g():
    total = 0
    for i in range(5):
        if (sq := i * i) % 2 == 0:
            total += sq
    return total, sq
print(g())

def h():
    n = 0
    def inner():
        nonlocal n
        if (n := n + 1) < 3:
            return inner()
        return n
    return inner()
print(h())

def k():
    global GW
    if (GW := 99):
        pass
k()
print(GW)

def comp():
    r = [(last := x) for x in range(3)]
    return last
print(comp())

class C:
    if (a := 5):
        pass
    b = a + 1
print(C.a, C.b)

def short():
    ok = (p := 0) or (q := 2)
    return ok, p, q
print(short())

def unbound_by_skip():
    if False and (never := 1):
        pass
    try:
        return never
    except UnboundLocalError:
        return "unbound"
print(unbound_by_skip())

def chain(vals):
    out = []
    while vals and (v := vals.pop(0)) is not None:
        out.append(v)
    return out
print(chain([1, 2, None, 3]))
print((a := (b := 2) + 1), a, b)
print([y := 1, y + 1])
