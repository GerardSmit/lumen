import _pkg
import _pkg.sub
import _pkg.inner.deep
from _pkg import inner
from _pkg.inner import inner_triple, INNER
from _pkg.inner.deep import deep_calc

print("---")
print(_pkg.__name__, _pkg.sub.__name__, inner.__name__, _pkg.inner.deep.__name__)
print(_pkg.sub.double(4), _pkg.double(5), _pkg.TAG, _pkg.PKG_CONST)
print(inner_triple(3), INNER, deep_calc(2), _pkg.inner.deep.DEEP)
print(_pkg.inner.deep.sub is _pkg.sub, inner.triple is _pkg.sub.triple)
print(_pkg.sub.__package__, _pkg.inner.__package__, _pkg.inner.deep.__package__, _pkg.__package__)
import _pkg.inner.deep as dp
print(dp is _pkg.inner.deep, dp.dbl(8))
print(hasattr(_pkg, "inner"), hasattr(_pkg, "nonexistent"))
from _pkg import sub as s2
print(s2 is _pkg.sub)
print(type(_pkg).__name__, type(_pkg.inner).__name__)
