from _helpers.util import f as g
from _helpers.util import double as dbl, CONSTANT as K
import _helpers.sub.deep as d
import _helpers.util as u
import _helpers as h

print(g(10), dbl(4), K)
print(d.deep_value(1))
print(u.f is g)
print(h.util is u)
print(h.sub.deep is d)

from _helpers.util import *

print(f(1), double(2), CONSTANT, Box(3))
print("not_exported" in dir(), "_private" in dir(), "bump" in dir())

ns = {}
exec("from _helpers.util import *", ns)
print(sorted(k for k in ns if not k.startswith("__")))

from _helpers import util as util2, VERSION as ver

print(util2 is u, ver)
print(g.__name__, dbl.__name__)

try:
    print(f_missing)
except NameError:
    print("f_missing undefined")

import _helpers.util, _helpers.sub
print(_helpers.util.f(0), _helpers.sub.LEVEL)
