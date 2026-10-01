import asyncio

log = []

async def worker(name, n):
    for i in range(n):
        log.append(f"{name}{i}")
        await asyncio.sleep(0)
    return name * n

async def main():
    r = await asyncio.gather(worker("a", 2), worker("b", 3), worker("c", 1))
    print(r)
    print(log)
    t = asyncio.create_task(worker("t", 2))
    await asyncio.sleep(0)
    print(await t)
    try:
        await asyncio.gather(boom(), worker("x", 1))
    except ValueError as e:
        print("ValueError", e)
    res = await asyncio.gather(boom(), worker("y", 1), return_exceptions=True)
    print([type(x).__name__ for x in res])
    q = asyncio.Queue()
    await q.put(1); await q.put(2)
    print(await q.get(), q.qsize())
    return "main-done"

async def boom():
    await asyncio.sleep(0)
    raise ValueError("boom")

print(asyncio.run(main()))
