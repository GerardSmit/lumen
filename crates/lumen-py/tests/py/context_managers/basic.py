class CM:
    def __init__(self, name, suppress=False):
        self.name = name
        self.suppress = suppress
        print("init", name)

    def __enter__(self):
        print("enter", self.name)
        return self

    def __exit__(self, exc_type, exc, tb):
        print("exit", self.name, exc_type.__name__ if exc_type else None, exc)
        return self.suppress


with CM("a") as cm:
    print("body", cm.name)
print("after a")

with CM("b"):
    print("no target")

cm = CM("c")
with cm as same:
    print(same is cm)

with CM("d", suppress=True):
    raise ValueError("swallowed")
print("continued after d")

try:
    with CM("e"):
        raise KeyError("propagates")
except KeyError as e:
    print("caught", e)


class Value:
    def __enter__(self):
        return (1, 2)

    def __exit__(self, *args):
        print("exit args", len(args), args[0], args[1], args[2])
        return False


with Value() as (x, y):
    print(x, y)

with Value() as pair:
    print(pair)


def f():
    with CM("ret"):
        return "returned"
    return "unreached"


print(f())

for i in range(3):
    with CM("loop%d" % i):
        if i == 1:
            continue
        if i == 2:
            break
        print("loop body", i)


class Selective:
    def __enter__(self):
        return self

    def __exit__(self, et, ev, tb):
        return et is not None and issubclass(et, ZeroDivisionError)


with Selective():
    1 / 0
print("zero suppressed")
try:
    with Selective():
        [][0]
except IndexError:
    print("index not suppressed")


class Counter:
    n = 0

    def __enter__(self):
        Counter.n += 1
        return Counter.n

    def __exit__(self, *a):
        Counter.n -= 10


with Counter() as c1, Counter() as c2:
    print(c1, c2)
print(Counter.n)

with CM("attr") as o:
    o.extra = 5
print(o.extra)

lst = [0]
with CM("sub"):
    lst[0] = 9
print(lst)

try:
    with 5:
        pass
except (TypeError, AttributeError) as e:
    print(type(e).__name__)
