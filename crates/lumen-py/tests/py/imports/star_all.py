from _pkg import *
print("---")
print(double(21), TAG, PKG_CONST, sub.triple(2))
print("not_exported" in globals(), "triple" in globals())
try:
    print(not_exported)
except NameError:
    print("NameError not_exported")
from _helper_mod import *
print(VALUE, greet("star"), Box(1), names)
print("_private" in globals(), "bump" in globals())
import _pkg
print(sorted(_pkg.__all__))
print(_pkg.not_exported)
