class Yield:
    def __await__(self):
        yield


def run(coro):
    while True:
        try:
            coro.send(None)
        except StopIteration as e:
            return e.value


async def fail(msg):
    await Yield()
    raise ValueError(msg)


async def mid():
    return await fail("deep")


async def top():
    try:
        await mid()
    except ValueError as e:
        return "caught " + str(e)


print(run(top()))

try:
    run(mid())
except ValueError as e:
    print("propagated", e)


async def with_finally(log):
    try:
        log.append("start")
        await Yield()
        log.append("after yield")
        return "result"
    finally:
        log.append("finally")


log = []
print(run(with_finally(log)), log)


async def finally_on_error(log):
    try:
        await fail("x")
    finally:
        log.append("cleanup")


log = []
try:
    run(finally_on_error(log))
except ValueError:
    print("error after cleanup", log)

log = []
c = with_finally(log)
c.send(None)
c.close()
print(log)

log = []
c = with_finally(log)
c.close()
print("closed unstarted", log)


async def swallow_close():
    try:
        await Yield()
    except GeneratorExit:
        print("got GeneratorExit")
        raise


c = swallow_close()
c.send(None)
c.close()

c = swallow_close()
c.send(None)
try:
    c.throw(KeyError("thrown in"))
except KeyError as e:
    print("throw propagated", e)


async def recover():
    try:
        await Yield()
    except KeyError:
        await Yield()
        return "recovered"


c = recover()
c.send(None)
c.throw(KeyError("k"))
try:
    c.send(None)
except StopIteration as e:
    print(e.value)


async def raise_in_exc():
    try:
        await fail("one")
    except ValueError as e:
        raise RuntimeError("two") from e


try:
    run(raise_in_exc())
except RuntimeError as e:
    print(e, repr(e.__cause__))


async def not_started():
    return 1


c = not_started()
c.close()
print("closed fresh coroutine")

try:
    c.send(None)
except RuntimeError:
    print("RuntimeError closed")


async def stop_inside():
    raise StopIteration("bad")


try:
    run(stop_inside())
except RuntimeError as e:
    print("RuntimeError", e.__cause__.args)


async def ret_none():
    pass


print(run(ret_none()))


async def collect(coros):
    out = []
    for co in coros:
        try:
            out.append(await co)
        except Exception as e:
            out.append(type(e).__name__)
    return out


print(run(collect([fail("a"), not_started(), ret_none()])))

try:
    run(fail("final"))
except ValueError as e:
    print(e.args)
