import _helpers
import _helpers.sibling
from _helpers import sibling, util
from _helpers.sub import deep

print(_helpers.VERSION, _helpers.PACKAGE_FLAG)
print(sibling.make(3))
print(deep.deep_value(4))
print(deep.info())
print(util.f(1), util.double(5))
print(sibling.util is util)
print(sibling.deep is deep)
print(_helpers.sibling is sibling)
print(_helpers.sub.deep is deep)
print(deep.__name__, deep.__package__)
print(sibling.__name__, _helpers.__name__)
print(sorted(n for n in ("util", "sibling", "sub") if hasattr(_helpers, n)))
