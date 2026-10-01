import sys

print(__name__)
print(__name__ == "__main__")
print(len(sys.argv), type(sys.argv).__name__, isinstance(sys.argv[0], str))
counter = 0
registry = []


def bump():
    global counter
    counter += 1
    registry.append(counter)
    return counter


bump()
bump()
print(counter, registry)


def read_only():
    return counter + len(registry)


print(read_only())


def shadow():
    counter = 100
    return counter


print(shadow(), counter)


def uses_global_before():
    global late
    late = "set by function"


uses_global_before()
print(late)
print("late" in globals(), "nonexistent" in globals(), globals()["counter"])
x = 1
y = 2
del x
print("x" in globals(), "y" in globals())
try:
    print(x)
except NameError as e:
    print(type(e).__name__)
try:
    del x
except NameError as e:
    print(type(e).__name__)
x = "back"
print(x)
lst = [1, 2, 3, 4, 5]
del lst[0]
del lst[-1]
del lst[1:2]
print(lst)
dd = {"a": 1, "b": 2}
del dd["a"]
print(dd)


class Holder:
    pass


h = Holder()
h.attr = 1
del h.attr
print(hasattr(h, "attr"))
a = b = c = 7
print(a, b, c)
a = b = []
a.append(1)
print(b)
i = j = 0
i += 1
print(i, j)
p, q = 1, 2
p, q = q, p + q
print(p, q)
r = s = t = u = "same"
print(r, s, t, u)
m = {}
m["k"] = n = 5
print(m, n)
lst2 = [0, 0, 0]
idx = 0
idx, lst2[idx] = 1, 9
print(idx, lst2)
order = []


def mk(name, val):
    order.append(name)
    return val


mk("t1", 1), mk("t2", 2)
(mk("l1", 0), mk("l2", 1))
vals = [None, None]
vals[mk("i0", 0)], vals[mk("i1", 1)] = mk("r0", "a"), mk("r1", "b")
print(order, vals)
if __name__ == "__main__":
    print("main guard ran")
else:
    print("imported")


def main():
    print("main() called")
    return 0


if __name__ == "__main__":
    rc = main()
    print("rc", rc)
print(type(sys.modules).__name__, "sys" in sys.modules)
print(sys.version_info[0], sys.version_info >= (3, 8), sys.maxsize == 2**63 - 1)
print(__doc__, type(__builtins__).__name__ in ("module", "dict"))
for k in ("counter", "registry", "bump"):
    print(k, k in dir())
