def run(coro):
    try:
        while True:
            coro.send(None)
    except StopIteration as e:
        return e.value

class Ctx:
    def __init__(self, name, swallow=False):
        self.name = name
        self.swallow = swallow
    async def __aenter__(self):
        print("enter", self.name)
        return self.name.upper()
    async def __aexit__(self, et, ev, tb):
        print("exit", self.name, et.__name__ if et else None)
        return self.swallow

class ACount:
    def __init__(self, n):
        self.n = n
        self.i = 0
    def __aiter__(self):
        return self
    async def __anext__(self):
        if self.i >= self.n:
            raise StopAsyncIteration
        self.i += 1
        return self.i * 10

async def main():
    async with Ctx("a") as v:
        print("inside", v)
    async with Ctx("b") as x, Ctx("c") as y:
        print("both", x, y)
    async with Ctx("swallow", True):
        raise ValueError("hidden")
    print("after swallow")
    try:
        async with Ctx("prop"):
            raise KeyError("k")
    except KeyError:
        print("propagated KeyError")
    out = []
    async for n in ACount(3):
        out.append(n)
    print(out)
    print([n async for n in ACount(4)])
    print([n async for n in ACount(5) if n % 20 == 0])
    total = 0
    async for n in ACount(2):
        total += n
    else:
        print("else ran")
    return total

print(run(main()))

async def agen():
    for i in range(3):
        yield i
    return

async def use_agen():
    r = []
    async for v in agen():
        r.append(v * v)
    return r
print(run(use_agen()))
