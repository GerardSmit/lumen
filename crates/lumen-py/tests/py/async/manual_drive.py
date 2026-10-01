async def add(a, b):
    return a + b

async def compute():
    x = await add(1, 2)
    y = await add(x, 10)
    return x * y

c = add(1, 2)
print(type(c).__name__)
try:
    c.send(None)
except StopIteration as e:
    print("result", e.value)

c2 = compute()
try:
    c2.send(None)
except StopIteration as e:
    print("result", e.value)

class Suspend:
    def __init__(self, tag):
        self.tag = tag
    def __await__(self):
        got = yield self.tag
        return "resumed-with-" + str(got)

async def worker():
    a = await Suspend("first")
    print("a =", a)
    b = await Suspend("second")
    print("b =", b)
    return (a, b)

w = worker()
print(w.send(None))
print(w.send(1))
try:
    w.send(2)
except StopIteration as e:
    print("done", e.value)

async def boom():
    await Suspend("pre")
    raise ValueError("async boom")

b = boom()
b.send(None)
try:
    b.send(None)
except ValueError as e:
    print("ValueError", e)

async def catcher():
    try:
        await Suspend("x")
    except KeyError as e:
        return "caught " + str(e)

k = catcher()
k.send(None)
try:
    k.throw(KeyError("kk"))
except StopIteration as e:
    print(e.value)

cl = worker()
cl.send(None)
cl.close()
print("closed")
print(add.__name__, compute.__name__)
r = add(5, 5)
r.close()
