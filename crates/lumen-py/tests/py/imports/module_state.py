import sys

print("_helpers.util" in sys.modules)
import _helpers.util as u1

print("_helpers.util" in sys.modules, "_helpers" in sys.modules)
import _helpers.util as u2
from _helpers import util as u3

print(u1 is u2, u2 is u3, sys.modules["_helpers.util"] is u1)
print(u1.__name__)
print(u1.counter)
u1.bump()
u1.bump()
print(u2.counter, u3.counter)
u3.counter = 100
print(u1.counter, u1.bump())

from _helpers.util import counter as snapshot, bump

print(snapshot)
bump()
print(snapshot, u1.counter)

import _helpers.sub.deep
print(sorted(n for n in sys.modules if n.startswith("_helpers")))
print(_helpers.sub.deep.__name__, _helpers.sub.__name__)

try:
    import no_such_module_xyz
except ModuleNotFoundError as e:
    print(type(e).__name__, e.name)
    print(isinstance(e, ImportError))

try:
    import _helpers.nothing_here
except ImportError as e:
    print(type(e).__name__, e.name)

try:
    from _helpers.util import missing_name
except ImportError as e:
    print(type(e).__name__)

try:
    from no_such_pkg import thing
except ImportError as e:
    print(type(e).__name__, e.name)

try:
    import _helpers.util.nested
except ImportError as e:
    print(type(e).__name__)

print("no_such_module_xyz" in sys.modules)

print(sys.modules["sys"] is sys)

m = sys.modules["_helpers.util"]
m.injected = "added from outside"
from _helpers.util import injected
print(injected)

import math
import math as m2
print(math is m2, math.__name__)

for i in range(3):
    import _helpers.util as again
print(again is u1, again.counter)

def lazy():
    import _helpers.sub as s
    return s.LEVEL

print(lazy(), lazy())
print(hasattr(sys.modules[__name__], "u1"), __name__)
