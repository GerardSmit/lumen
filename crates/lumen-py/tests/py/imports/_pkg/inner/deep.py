print("init _pkg.inner.deep", __name__)
from .. import sub
from ..sub import double as dbl
def deep_calc(x):
    return dbl(x) + sub.triple(x)
DEEP = "deep-value"
