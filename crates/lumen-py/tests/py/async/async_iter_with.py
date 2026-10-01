class Yield:
    def __await__(self):
        yield


def run(coro):
    steps = 0
    while True:
        try:
            coro.send(None)
        except StopIteration as e:
            return e.value, steps
        steps += 1


class Counter:
    def __init__(self, n):
        self.n = n
        self.i = 0

    def __aiter__(self):
        return self

    async def __anext__(self):
        await Yield()
        if self.i >= self.n:
            raise StopAsyncIteration
        self.i += 1
        return self.i


async def consume():
    out = []
    async for x in Counter(4):
        out.append(x)
    else:
        out.append("else")
    return out


print(run(consume()))


class Res:
    def __init__(self, name, swallow=False):
        self.name = name
        self.swallow = swallow

    async def __aenter__(self):
        print("aenter", self.name)
        await Yield()
        return self.name.upper()

    async def __aexit__(self, et, ev, tb):
        print("aexit", self.name, et.__name__ if et else None)
        await Yield()
        return self.swallow


async def use():
    async with Res("a") as a, Res("b") as b:
        print("body", a, b)
    async with Res("c", swallow=True):
        raise ValueError("suppressed")
    print("after c")
    try:
        async with Res("d"):
            raise KeyError("k")
    except KeyError as e:
        print("caught", e)
    return "ok"


print(run(use()))


async def agen(n):
    try:
        for i in range(n):
            await Yield()
            yield i * i
    finally:
        print("agen finally")


async def use_agen():
    total = []
    async for v in agen(4):
        total.append(v)
    return total


print(run(use_agen()))


async def plain_agen(n):
    for i in range(n):
        await Yield()
        yield i * i


async def early_exit():
    async for v in plain_agen(10):
        if v >= 4:
            break
    return v


print(run(early_exit()))


async def comp():
    a = [x async for x in agen(3)]
    b = [x async for x in agen(5) if x % 2 == 0]
    return a, b


print(run(comp()))


async def manual():
    g = agen(3)
    first = await g.__anext__()
    second = await g.asend(None)
    await g.aclose()
    return first, second


print(run(manual()))


async def with_send():
    async def echo():
        got = yield "ready"
        while True:
            got = yield "echo:" + str(got)

    e = echo()
    r = [await e.__anext__()]
    r.append(await e.asend(1))
    r.append(await e.asend(2))
    return r


print(run(with_send()))


async def stop_check():
    g = agen(1)
    await g.__anext__()
    try:
        await g.__anext__()
    except StopAsyncIteration:
        return "stopped"


print(run(stop_check()))
cc = consume()
print(type(plain_agen(1)).__name__, type(cc).__name__)
cc.close()
