class _GenCM:
    def __init__(self, func, args, kwargs):
        self.gen = func(*args, **kwargs)

    def __enter__(self):
        try:
            return next(self.gen)
        except StopIteration:
            raise RuntimeError("generator didn't yield") from None

    def __exit__(self, typ, value, tb):
        if typ is None:
            try:
                next(self.gen)
            except StopIteration:
                return False
            raise RuntimeError("generator didn't stop")
        if value is None:
            value = typ()
        try:
            self.gen.throw(value)
        except StopIteration as exc:
            return exc is not value
        except BaseException as exc:
            if exc is value:
                return False
            raise
        raise RuntimeError("generator didn't stop after throw")


def my_contextmanager(func):
    def helper(*args, **kwargs):
        return _GenCM(func, args, kwargs)
    return helper


@my_contextmanager
def tag(name):
    print("<%s>" % name)
    yield name
    print("</%s>" % name)


@my_contextmanager
def guard(name, swallow=()):
    print("open", name)
    try:
        yield name.upper()
    except swallow as e:
        print("swallowed", type(e).__name__, e)
    finally:
        print("close", name)


@my_contextmanager
def transaction(store):
    snapshot = dict(store)
    try:
        yield store
    except Exception:
        store.clear()
        store.update(snapshot)
        print("rolled back")
        raise


with tag("html") as t:
    print("inside", t)

with tag("a"), tag("b"):
    print("nested")

with guard("g1") as g:
    print("got", g)

with guard("g2", swallow=(ValueError,)):
    raise ValueError("oops")
print("after g2")

try:
    with guard("g3", swallow=(ValueError,)):
        raise KeyError("passes")
except KeyError as e:
    print("outer", e)

db = {"a": 1}
try:
    with transaction(db) as d:
        d["a"] = 2
        d["b"] = 3
        raise RuntimeError("fail")
except RuntimeError as e:
    print(e, db)

with transaction(db) as d:
    d["c"] = 4
print(db)


def early():
    with tag("early"):
        return "value"


print(early())


@my_contextmanager
def no_yield():
    return
    yield


try:
    with no_yield():
        pass
except RuntimeError as e:
    print(e)


@my_contextmanager
def two_yields():
    yield 1
    yield 2


try:
    with two_yields():
        pass
except RuntimeError as e:
    print(e)

for i in range(2):
    with tag("loop%d" % i):
        if i == 0:
            continue
        print("body")
