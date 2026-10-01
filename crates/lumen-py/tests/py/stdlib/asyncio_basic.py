import asyncio

log = []


async def worker(name, steps):
    for i in range(steps):
        log.append("%s:%d" % (name, i))
        await asyncio.sleep(0)
    return name.upper()


async def add(a, b):
    await asyncio.sleep(0)
    return a + b


async def fail(msg):
    await asyncio.sleep(0)
    raise ValueError(msg)


async def nested(n):
    if n == 0:
        return 0
    return n + await nested(n - 1)


async def agen(n):
    for i in range(n):
        await asyncio.sleep(0)
        yield i * i


class Counter:
    def __init__(self):
        self.n = 0

    async def __aenter__(self):
        self.n += 1
        return self

    async def __aexit__(self, *exc):
        self.n += 10
        return False


async def main():
    print(await add(1, 2), await nested(10))

    results = await asyncio.gather(worker("a", 3), worker("b", 2), worker("c", 1))
    print(results, log)
    del log[:]

    results = await asyncio.gather(add(1, 2), add(3, 4), add(5, 6))
    print(results, sum(results))
    print(await asyncio.gather())

    r = await asyncio.gather(add(1, 1), fail("boom"), return_exceptions=True)
    print(r[0], type(r[1]).__name__, r[1].args)
    try:
        await asyncio.gather(add(1, 1), fail("first"))
    except ValueError as e:
        print("gather raised", e)

    t1 = asyncio.create_task(worker("t1", 2))
    t2 = asyncio.create_task(worker("t2", 2))
    print(t1.done(), log)
    await asyncio.sleep(0)
    await asyncio.sleep(0)
    await asyncio.sleep(0)
    print(t1.done(), t2.done(), log)
    print(await t1, await t2, t1.result(), t1.done())
    del log[:]

    t3 = asyncio.create_task(fail("task failure"))
    try:
        await t3
    except ValueError as e:
        print("task raised", e, t3.done(), type(t3.exception()).__name__)

    async def forever():
        try:
            while True:
                await asyncio.sleep(0)
        except asyncio.CancelledError:
            log.append("cancelled")
            raise

    t4 = asyncio.create_task(forever())
    await asyncio.sleep(0)
    t4.cancel()
    try:
        await t4
    except asyncio.CancelledError:
        print("t4 cancelled", t4.cancelled(), log)
    del log[:]

    async with Counter() as c:
        print("inside", c.n)
    print("after", c.n)

    print([x async for x in agen(5)])
    print(sum([x async for x in agen(4) if x % 2 == 0]))

    q = asyncio.Queue()

    async def producer():
        for i in range(4):
            await q.put(i)
            await asyncio.sleep(0)
        await q.put(None)

    async def consumer():
        got = []
        while True:
            item = await q.get()
            if item is None:
                return got
            got.append(item * 10)

    _, got = await asyncio.gather(producer(), consumer())
    print(got)

    lock = asyncio.Lock()
    order = []

    async def locked(n):
        async with lock:
            order.append("in%d" % n)
            await asyncio.sleep(0)
            order.append("out%d" % n)

    await asyncio.gather(locked(1), locked(2), locked(3))
    print(order)

    ev = asyncio.Event()

    async def waiter():
        await ev.wait()
        return "released"

    w = asyncio.create_task(waiter())
    await asyncio.sleep(0)
    print(w.done(), ev.is_set())
    ev.set()
    print(await w)

    try:
        await asyncio.wait_for(asyncio.sleep(10), timeout=0.01)
    except asyncio.TimeoutError:
        print("timeout")
    print(await asyncio.wait_for(add(2, 3), timeout=5))
    return "main done"


print(asyncio.run(main()))
print(asyncio.run(add(20, 22)))
