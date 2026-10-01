class Suppress:
    def __init__(self, *types):
        self.types = types
    def __enter__(self):
        return self
    def __exit__(self, et, ev, tb):
        return et is not None and issubclass(et, self.types)

with Suppress(KeyError):
    {}["x"]
print("suppressed key")
with Suppress(KeyError, IndexError):
    [][0]
print("suppressed index")
try:
    with Suppress(KeyError):
        1 / 0
except ZeroDivisionError:
    print("not suppressed")

class ExitRaises:
    def __enter__(self): return self
    def __exit__(self, *a):
        raise TypeError("from exit")
try:
    with ExitRaises():
        pass
except TypeError as e:
    print("clean exit raise:", e, e.__context__)
try:
    with ExitRaises():
        raise ValueError("body")
except TypeError as e:
    print(e, type(e.__context__).__name__, e.__context__)

class Replace:
    def __enter__(self): return self
    def __exit__(self, et, ev, tb):
        raise KeyError("new") from ev
try:
    with Replace():
        raise ValueError("old")
except KeyError as e:
    print(repr(e.__cause__))

class Counter:
    depth = 0
    def __enter__(self):
        Counter.depth += 1
        return Counter.depth
    def __exit__(self, *a):
        Counter.depth -= 1
        return False
with Counter() as a:
    with Counter() as b:
        with Counter() as c:
            print(a, b, c, Counter.depth)
print(Counter.depth)

class Truthy:
    def __enter__(self): return self
    def __exit__(self, *a): return [1]
with Truthy():
    raise ValueError
print("truthy swallowed")
class Falsy:
    def __enter__(self): return self
    def __exit__(self, *a): return 0
try:
    with Falsy():
        raise ValueError
except ValueError:
    print("falsy kept")
class NoneRet:
    def __enter__(self): return self
    def __exit__(self, *a): pass
try:
    with NoneRet():
        raise ValueError("n")
except ValueError as e:
    print("none kept", e)

class Finder:
    def __enter__(self): return self
    def __exit__(self, et, ev, tb):
        print("args", et is ValueError, isinstance(ev, ValueError), tb is not None)
try:
    with Finder():
        raise ValueError
except ValueError:
    pass
log = []
class R:
    def __init__(self, n): self.n = n
    def __enter__(self): log.append("+" + self.n); return self
    def __exit__(self, *a): log.append("-" + self.n)
try:
    with R("a"), R("b"), R("c"):
        log.append("body")
        raise EOFError
except EOFError:
    pass
print(log)
