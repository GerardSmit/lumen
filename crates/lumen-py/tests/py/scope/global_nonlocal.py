counter = 0

def bump(n=1):
    global counter
    counter += n
    return counter

print(bump(), bump(5), counter)

def outer():
    state = []
    total = 0
    def add(v):
        nonlocal total
        total += v
        state.append(total)
    for v in (1, 2, 3):
        add(v)
    return state, total
print(outer())

def gen_global():
    global G1, G2
    G1 = "one"
    G2 = "two"
gen_global()
print(G1, G2)

def nested_global():
    def inner():
        global counter
        counter = -1
    inner()
nested_global()
print(counter)

def shadow():
    counter = "local"
    def inner():
        return counter
    return inner()
print(shadow(), counter)

def acc():
    n = 0
    def a():
        nonlocal n
        n += 1
        def b():
            nonlocal n
            n *= 10
            return n
        return b()
    return a(), n
print(acc())

def global_in_loop():
    global idx
    for idx in range(3):
        pass
global_in_loop()
print(idx)

def fn_attr():
    fn_attr.calls = getattr(fn_attr, "calls", 0) + 1
    return fn_attr.calls
fn_attr(); fn_attr()
print(fn_attr.calls)

def make_counter():
    c = 0
    def counter():
        nonlocal c
        c += 1
        return c
    return counter
c1, c2 = make_counter(), make_counter()
print(c1(), c1(), c2(), c1())

def g_del():
    global tmp
    tmp = 5
    del tmp
g_del()
print("tmp" in globals())
def default_closure():
    v = 10
    def f(a=v):
        return a
    v = 20
    return f()
print(default_closure())
