import _helper_mod
import _helper_mod as hm
from _helper_mod import VALUE, greet, Box as B
from _helper_mod import bump, counter

print(_helper_mod.VALUE, hm.VALUE, VALUE, hm is _helper_mod)
print(greet("world"), _helper_mod.greet("x"))
print(B(3), _helper_mod.Box(4))
print(_helper_mod.__name__, __name__, hm.__doc__)
print(_helper_mod._private, _helper_mod.names)
print(bump(), bump(), _helper_mod.counter, counter)
_helper_mod.VALUE = 7
print(_helper_mod.VALUE, VALUE, hm.VALUE)
_helper_mod.extra = "added"
print(hm.extra, hasattr(hm, "extra"), hasattr(hm, "nope"))
print(type(hm).__name__, callable(hm.greet), "greet" in dir(hm), "VALUE" in vars(hm))
print(getattr(hm, "VALUE"), getattr(hm, "zzz", "dflt"))
try:
    import _no_such_module
except ImportError as e:
    print("ImportError", type(e).__name__)
try:
    from _helper_mod import missing_name
except ImportError as e:
    print("ImportError from")
import math as m, sys as s
print(m.floor(2.5), s.maxsize > 0)
from math import floor, ceil as c
print(floor(1.5), c(1.5))
