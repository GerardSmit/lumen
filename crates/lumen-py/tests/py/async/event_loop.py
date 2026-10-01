class Yield:
    def __await__(self):
        yield ("yield",)


class Sleep:
    def __init__(self, ticks):
        self.ticks = ticks

    def __await__(self):
        yield ("sleep", self.ticks)


class Task:
    def __init__(self, name, coro):
        self.name = name
        self.coro = coro
        self.result = None
        self.error = None
        self.done = False
        self.wake = 0


class Loop:
    def __init__(self):
        self.now = 0
        self.ready = []
        self.sleeping = []
        self.tasks = []

    def spawn(self, name, coro):
        t = Task(name, coro)
        self.tasks.append(t)
        self.ready.append(t)
        return t

    def step(self, task):
        try:
            req = task.coro.send(None)
        except StopIteration as e:
            task.result = e.value
            task.done = True
            return
        except Exception as e:
            task.error = e
            task.done = True
            return
        if req[0] == "sleep":
            task.wake = self.now + req[1]
            self.sleeping.append(task)
        else:
            self.ready.append(task)

    def run(self):
        while self.ready or self.sleeping:
            if not self.ready:
                self.now = max(self.now, min(t.wake for t in self.sleeping))
            woke = [t for t in self.sleeping if t.wake <= self.now]
            self.sleeping = [t for t in self.sleeping if t.wake > self.now]
            woke.sort(key=lambda t: (t.wake, t.name))
            self.ready.extend(woke)
            batch, self.ready = self.ready, []
            for t in batch:
                self.step(t)
            self.now += 1


events = []


async def worker(name, n, delay):
    for i in range(n):
        events.append((name, i))
        await Sleep(delay)
    return name + "-done"


async def yielder(name, n):
    for i in range(n):
        events.append((name, i))
        await Yield()
    return n


async def boom():
    await Yield()
    raise ValueError("task failed")


async def waiter(loop, others):
    while not all(t.done for t in others):
        await Yield()
    return [t.result for t in others]


loop = Loop()
a = loop.spawn("a", worker("a", 3, 2))
b = loop.spawn("b", worker("b", 2, 3))
c = loop.spawn("c", yielder("c", 3))
d = loop.spawn("d", boom())
g = loop.spawn("gather", waiter(loop, [a, b, c]))
loop.run()
print(events)
print(a.result, b.result, c.result)
print(g.result)
print(type(d.error).__name__, d.error)
print(loop.now)
print([t.done for t in loop.tasks])


async def fib(n):
    if n < 2:
        return n
    await Yield()
    return await fib(n - 1) + await fib(n - 2)


loop2 = Loop()
ts = [loop2.spawn("f%d" % i, fib(i)) for i in range(8)]
loop2.run()
print([t.result for t in ts])
