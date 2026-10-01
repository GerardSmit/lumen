class CM:
    def __init__(self, name, suppress=False, fail_enter=False):
        self.name = name
        self.suppress = suppress
        self.fail_enter = fail_enter
    def __enter__(self):
        print("enter", self.name)
        if self.fail_enter:
            raise RuntimeError("enter failed")
        return self.name.upper()
    def __exit__(self, et, ev, tb):
        print("exit", self.name, et.__name__ if et else None, ev, tb is None)
        return self.suppress

with CM("a") as x:
    print("body", x)
with CM("b"):
    print("no as")
with CM("c") as c1, CM("d") as c2:
    print("two", c1, c2)
with CM("e"), CM("f"):
    with CM("g"):
        print("nested")

try:
    with CM("h"):
        raise ValueError("boom")
except ValueError as e:
    print("propagated", e)
with CM("i", suppress=True):
    raise ValueError("hidden")
print("after suppressed")

try:
    with CM("j", fail_enter=True):
        print("never")
except RuntimeError as e:
    print(e)
try:
    with CM("k"), CM("l", fail_enter=True):
        print("never")
except RuntimeError as e:
    print("k exited first?", e)

def f():
    with CM("ret"):
        return "returned"
print(f())
for i in range(3):
    with CM("loop%d" % i):
        if i == 0:
            continue
        if i == 1:
            break
print("i", i)
with CM("m") as (a):
    pass
with (CM("n") as p, CM("o") as q):
    print(p, q)
class Plain: pass
try:
    with Plain():
        pass
except (TypeError, AttributeError):
    print("not a context manager")
cm = CM("saved")
with cm as s1:
    pass
with cm as s2:
    print(s1 == s2)
with CM("t") as v: pass
print(v)
