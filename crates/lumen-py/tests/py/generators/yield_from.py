def inner():
    x = yield 1
    print("inner got", x)
    y = yield 2
    print("inner got", y)
    return "inner-result"

def outer():
    r = yield from inner()
    print("outer r =", r)
    z = yield from [10, 20]
    print("z =", z)
    return "outer-result"

g = outer()
print(next(g))
print(g.send("a"))
print(g.send("b"))
print(g.send(None))
try:
    g.send(None)
except StopIteration as e:
    print(e.value)

def flatten(items):
    for it in items:
        if isinstance(it, list):
            yield from flatten(it)
        else:
            yield it

print(list(flatten([1, [2, [3, [4, 5]], 6], [[7]], 8])))

def counter(n):
    for i in range(n):
        yield i
    return n * 10

def chain():
    a = yield from counter(3)
    b = yield from counter(2)
    return a + b

c = chain()
out = []
try:
    while True:
        out.append(next(c))
except StopIteration as e:
    print(out, e.value)

def inner_t():
    yield "a"
    yield "b"

def delegating():
    try:
        yield from inner_t()
    except ValueError as e:
        print("outer caught", e)
        yield "after"

d = delegating()
print(next(d))
print(d.throw(ValueError("x")))
