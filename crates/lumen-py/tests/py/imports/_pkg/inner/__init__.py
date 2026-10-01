print("init _pkg.inner", __name__)
from ..sub import triple
from . import deep
INNER = "inner-value"
def inner_triple(x):
    return triple(x)
