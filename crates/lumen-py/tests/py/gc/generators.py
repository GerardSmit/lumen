import gc
import warnings
import weakref

gc.collect()
log = []


def gen():
    try:
        yield 1
        yield 2
    finally:
        log.append("finally")


g = gen()
next(g)
del g
print("closed on refcount death:", log)

log.clear()
g = gen()
del g
print("unstarted generator has no finally:", log)

g = gen()
next(g)
g = None
print("rebinding:", log)

log.clear()


def with_cycle():
    me = None
    try:
        me = yield
        yield
    finally:
        log.append("cycle finally")


g = with_cycle()
next(g)
g.send(g)
r = weakref.ref(g)
del g
print("cyclic generator pending:", log, r() is not None)
gc.collect()
print("cyclic generator closed:", log, r() is None)

log.clear()


def swallow():
    try:
        yield 1
    except GeneratorExit:
        log.append("GeneratorExit")
        raise


g = swallow()
next(g)
del g
print(log)

log.clear()


def yield_from_inner():
    try:
        yield "inner"
    finally:
        log.append("inner finally")


def yield_from_outer():
    try:
        yield from yield_from_inner()
    finally:
        log.append("outer finally")


g = yield_from_outer()
next(g)
del g
print("yield from:", log)

log.clear()
seen = []


def raising_finally():
    try:
        yield 1
    finally:
        raise ValueError("in finally")


import sys

hook = sys.unraisablehook
sys.unraisablehook = lambda args: seen.append((args.exc_type.__name__, str(args.exc_value)))
g = raising_finally()
next(g)
del g
print("unraisable from close:", seen)
sys.unraisablehook = hook


async def coro():
    return 1


with warnings.catch_warnings(record=True) as caught:
    warnings.simplefilter("always")
    coro()
    gc.collect()
print("never awaited:", [(w.category.__name__, str(w.message)) for w in caught])

with warnings.catch_warnings(record=True) as caught:
    warnings.simplefilter("always")
    c = coro()
    try:
        c.send(None)
    except StopIteration as e:
        print("coroutine result:", e.value)
    del c
    gc.collect()
print("awaited coroutine does not warn:", caught)


class Holder:
    def __init__(self):
        self.gen = self.run()
        next(self.gen)

    def run(self):
        try:
            yield
        finally:
            log.append("holder generator finally")


log.clear()
h = Holder()
r = weakref.ref(h)
del h
gc.collect()
print("generator owned by a cycle:", log, r() is None)
