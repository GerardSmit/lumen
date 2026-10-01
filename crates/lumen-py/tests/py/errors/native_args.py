# Argument errors raised by natively bound functions and types must match CPython 3.12 exactly.
import math
import itertools
from collections import deque


def show(f):
    try:
        r = f()
        print("ok", repr(r))
    except Exception as e:
        print(type(e).__name__, e)


# positional-only, one argument
show(lambda: math.sqrt())
show(lambda: math.sqrt(1, 2))
show(lambda: math.sqrt(x=4))
show(lambda: math.sqrt("x"))
show(lambda: math.sqrt(None))
show(lambda: math.factorial(1.5))
show(lambda: math.factorial(-1))
show(lambda: math.isqrt("3"))

# two positional-only
show(lambda: math.copysign(1.0))
show(lambda: math.copysign(1.0, 2.0, 3.0))
show(lambda: math.atan2(1, y=2))
show(lambda: math.pow("a", 2))

# keyword-capable and keyword-only parameters
show(lambda: math.isclose(1.0))
show(lambda: math.isclose(1.0, 2.0, 3.0))
show(lambda: math.isclose(1.0, 2.0, bogus=1))
show(lambda: math.isclose(1.0, a=2.0))
show(lambda: math.isclose(b=1.0, a=1.0, rel_tol=-1))
show(lambda: math.isclose(1.0, 2.0, rel_tol="x"))
show(lambda: math.prod([1, 2], 3))
show(lambda: math.prod([1, 2], start=3))
show(lambda: math.prod())
show(lambda: math.nextafter(1.0, 2.0, 3))
show(lambda: math.nextafter(1.0, 2.0, steps=-1))
show(lambda: math.nextafter(1.0, 2.0, steps="a"))
show(lambda: math.log())
show(lambda: math.log(1, 2, 3))

# *args
show(lambda: math.gcd(4, "a"))
show(lambda: math.hypot(3, "4"))
show(lambda: math.gcd(a=1))
show(lambda: math.hypot(x=1))

# signatures
for f in (math.sqrt, math.isclose, math.prod, math.nextafter, math.gcd, math.hypot, math.log, math.copysign):
    print(f.__name__, f.__text_signature__)

# methods of a native type
d = deque([1, 2, 3])
show(lambda: d.append())
show(lambda: d.append(1, 2))
show(lambda: d.append(x=1))
show(lambda: d.rotate("a"))
show(lambda: d.rotate(1, 2))
show(lambda: d.insert(0))
show(lambda: d.insert("a", 1))
show(lambda: d.index())
show(lambda: d.index(1, "a"))
show(lambda: d.pop(0))
show(lambda: deque.append(1, 2))
show(lambda: deque.append())
show(lambda: deque([1], 2, 3))
show(lambda: deque([1], maxlen="a"))
show(lambda: deque([1], maxlen=-1))
show(lambda: deque(bogus=1))
show(lambda: d[10])
show(lambda: d["a"])
for name in ("append", "rotate", "insert", "index", "__reversed__", "__len__", "__getitem__", "__init__"):
    print(name, getattr(deque, name).__text_signature__)
print(deque.__text_signature__)

# constructors of native types
show(lambda: itertools.count("a"))
show(lambda: itertools.repeat())
show(lambda: itertools.repeat(1, 2, 3))
show(lambda: itertools.islice())
show(lambda: itertools.islice([1], "a"))
show(lambda: itertools.combinations("ab"))
show(lambda: itertools.combinations("ab", -1))
show(lambda: itertools.permutations("ab", "x"))
show(lambda: itertools.accumulate())
show(lambda: itertools.accumulate([1], None, 0))
show(lambda: itertools.zip_longest("a", bogus=1))
show(lambda: itertools.product("a", repeat=-1))
show(lambda: itertools.product("a", bogus=1))
show(lambda: itertools.chain.from_iterable())
show(lambda: itertools.batched("abc", 0))
show(lambda: itertools.batched("abc"))
show(lambda: itertools.starmap(pow))
show(lambda: itertools.compress("a"))
show(lambda: itertools.tee([1], -1))
show(lambda: itertools.groupby())
for t in (itertools.count, itertools.repeat, itertools.islice, itertools.accumulate, itertools.chain, itertools.combinations):
    print(t.__name__, t.__text_signature__)
