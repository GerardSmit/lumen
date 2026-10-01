async def simple():
    return 42


async def add(a, b):
    return a + b


async def chain():
    x = await add(1, 2)
    y = await add(x, 10)
    return x, y


def run(coro):
    try:
        coro.send(None)
    except StopIteration as e:
        return e.value
    raise RuntimeError("coroutine suspended")


c = simple()
print(type(c).__name__)
try:
    c.send(None)
except StopIteration as e:
    print("StopIteration", e.value, e.args)

print(run(simple()))
print(run(add(2, 3)))
print(run(chain()))


class Suspend:
    def __init__(self, tag):
        self.tag = tag

    def __await__(self):
        got = yield self.tag
        return ("resumed", got)


async def uses_awaitable():
    a = await Suspend("first")
    b = await Suspend("second")
    return a, b


co = uses_awaitable()
print(co.send(None))
print(co.send("A"))
try:
    co.send("B")
except StopIteration as e:
    print(e.value)


class Immediate:
    def __await__(self):
        return 99
        yield


async def imm():
    return await Immediate()


print(run(imm()))


async def sleeper(n):
    seen = []
    for i in range(n):
        seen.append(await Suspend(i))
    return seen


s = sleeper(3)
print(s.send(None))
print(s.send("p"))
print(s.send("q"))
try:
    s.send("r")
except StopIteration as e:
    print("stop", e.value)


async def two():
    await Suspend("x")
    return "done"


t = two()
print(t.send(None))
try:
    t.send(None)
except StopIteration as e:
    print(e.value)
try:
    t.send(None)
except RuntimeError as e:
    print(type(e).__name__)


async def reuse_after():
    return 1


r = reuse_after()
run(r)
try:
    r.send(None)
except RuntimeError:
    print("cannot reuse awaited coroutine")

fresh = simple()
try:
    fresh.send(1)
except TypeError:
    print("non-None send to fresh coroutine")
fresh.close()


async def nested_susp():
    v = await inner_susp()
    return v * 2


async def inner_susp():
    got = await Suspend("inner")
    return len(got[1])


n = nested_susp()
print(n.send(None))
try:
    n.send("abcd")
except StopIteration as e:
    print(e.value)


def plain_gen():
    x = yield 1
    return x


async def await_gen_awaitable():
    class W:
        def __await__(self):
            return plain_gen()

    return await W()


w = await_gen_awaitable()
print(w.send(None))
try:
    w.send("sent")
except StopIteration as e:
    print(e.value)
