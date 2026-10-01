class Yield:
    def __await__(self):
        yield

def run_all(coros):
    coros = list(coros)
    results = [None] * len(coros)
    pending = list(range(len(coros)))
    rounds = 0
    while pending:
        rounds += 1
        still = []
        for i in pending:
            try:
                coros[i].send(None)
                still.append(i)
            except StopIteration as e:
                results[i] = e.value
        pending = still
    return results, rounds

log = []

async def job(name, steps):
    for s in range(steps):
        log.append(f"{name}:{s}")
        await Yield()
    return name * steps

res, rounds = run_all([job("a", 2), job("b", 4), job("c", 1)])
print(res, rounds)
print(log)

async def nested(depth):
    if depth == 0:
        await Yield()
        return 0
    return 1 + await nested(depth - 1)

print(run_all([nested(5)]))

class Future:
    def __init__(self):
        self.done = False
        self.value = None
    def set(self, v):
        self.done = True
        self.value = v
    def __await__(self):
        while not self.done:
            yield
        return self.value

fut = Future()
async def waiter():
    v = await fut
    return "got " + str(v)
async def setter():
    await Yield()
    await Yield()
    fut.set(42)
    return "set"
print(run_all([waiter(), setter()]))

async def failing():
    await Yield()
    raise RuntimeError("fail")
try:
    run_all([failing(), job("z", 1)])
except RuntimeError as e:
    print("RuntimeError", e)

import sys
async def noop():
    pass
co = noop()
print(co.__class__.__name__, hasattr(co, "send"), hasattr(co, "__await__"))
co.close()
